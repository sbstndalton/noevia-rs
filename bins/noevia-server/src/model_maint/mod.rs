//! Model maintenance moves to the Rust front (sbstndalton/noevia#2210, models round B; follows the
//! model reads of #2187 and #2189). The authorities of the maintenance work on the llama.cpp router:
//!
//! - [`gate`]: the inference gate's maintenance hold, the one exclusion every client of the router
//!   (Node's chat, Diary, this front) respects. A hold has a deadline and is given back on failure.
//! - [`lease`]: with no gate, the maintenance lease authority: one exclusive, fenced, expiring lease
//!   shared by this front's reload and calibration and Node's chat, tune, delete and prune.
//! - [`node_lease`]: the hold of Node's local chat admission that backs a lease this front holds.
//! - [`router`]: the router's HTTP API as the maintenance work uses it.
//! - [`ini`]: the models.ini store (read, parse, commit through the model-loader or the web writer).
//! - [`evidence_recheck`]: one image-input request under normal inference admission, with carried identity.
//! - [`reload`]: `POST /api/models/presets/reload` and the reload-guard state file
//!   (`native-preset-reload-<hash>.json`).
//!
//! The state files live in `UI_DATA_DIR`, named by the first 16 hex digits of the sha256 of the
//! model manager's base URL exactly as Core names them, so a Core and a front that swap owners
//! (rollback) read each other's files.
//!
//! Spawned work: every job runs on its own `tokio::spawn`ed task, never on the request's future, so
//! a client that goes away strands neither the work nor its hold (a [`gate::Hold`] is released by the
//! task, has a deadline, and is given back from `Drop` if the task dies).

pub mod autotune_context;
pub mod calibration;
pub mod calibration_facts;
pub mod calibration_worker;
pub mod evidence;
pub mod evidence_live;
pub mod evidence_recheck;
pub mod gate;
pub mod ini;
pub mod model_state;
// Port contract row J: the one maintenance lease authority (replaces core model-maintenance-lease.cjs).
pub mod lease;
pub mod node_lease;
pub mod reload;
pub mod router;
pub mod v8_json;
// #697 the memory watchdog (Core inference-budget-watch.cjs; `rust-budget-watch`).
pub mod budget_watch;

use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use serde_json::Value;
use server_auth::Role;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use gate::Hold;

pub const RELOAD: &str = "/api/models/presets/reload";
pub const PRUNE: &str = crate::prune_missing::PATH;
pub const DELETE: &str = crate::delete_model::PATH;
pub const LOAD: &str = crate::load_model::LOAD;
pub const UNLOAD: &str = crate::load_model::UNLOAD;
pub const EVIDENCE: &str = "/api/models/evidence";
pub const EVIDENCE_IMPORT: &str = "/api/models/evidence/import";

/// The maintenance routes this front answers.
pub const ROUTES: &[&str] = &[
    RELOAD,
    PRUNE,
    DELETE,
    LOAD,
    UNLOAD,
    EVIDENCE,
    EVIDENCE_IMPORT,
    evidence_recheck::PATH,
];

const MISSING_TOKEN_WARNING: &str = "[inference-gate] INFERENCE_GATE_ADMIN_TOKEN is not set: the memory watchdog cannot unload a model that is answering a request, and \"unload anyway\" on the Models page is unavailable. Set it to the admin token of the gate.";

/// The methods the front answers on each (routes.toml); every other method stays Node's.
pub fn owned(method: &Method, path: &str) -> bool {
    match path {
        RELOAD | PRUNE | DELETE | LOAD | UNLOAD | EVIDENCE_IMPORT | evidence_recheck::PATH => {
            method == Method::POST
        }
        EVIDENCE => method == Method::GET,
        _ => false,
    }
}

/// Why a maintenance hold could not be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldError {
    /// Work is running (or another job holds the engine): 409.
    Busy,
    /// The inference gate refused for another reason (its HTTP status and text).
    Gate { status: u16, message: String },
    /// A gate fronts the engine, this process has no admin token for its hold, and the caller
    /// cannot run without it.
    NeedsGate,
    /// No gate, and Node's own maintenance gate gave no answer: nothing was changed.
    NodeUnavailable,
}

/// Why a run's hold ([`Maint::acquire_for_run`]) could not be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldRefusal {
    Busy,
    /// The gate's own refusal: its status, code and text.
    Gate {
        status: u16,
        code: String,
        message: String,
    },
    NeedsGate,
    NodeUnavailable,
    /// No gate hold, and the run could outlast the longest lease Node grants.
    OverLimit,
    /// Node granted a lease shorter than the run could take (and it was given back).
    ShortLease,
}

/// The local job flag, owned by the acquisition until [`Self::transfer`] hands it to a Hold.
struct Reservation {
    flag: Arc<AtomicBool>,
    armed: bool,
}

impl Reservation {
    fn take(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Reservation {
            flag: Arc::clone(flag),
            armed: true,
        })
    }
    fn transfer(mut self) {
        self.armed = false;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(false, Ordering::Release);
        }
    }
}

/// An authority grant and the Node latch asked for it, before the Hold exists.
struct PendingLease {
    authority: lease::Authority,
    grant: Option<lease::Grant>,
    node: Arc<node_lease::NodeLeases>,
    ticket: Option<String>,
}

impl PendingLease {
    /// The Hold takes over both.
    fn complete(mut self) -> Option<lease::Grant> {
        self.ticket = None;
        self.grant.take()
    }
}

impl Drop for PendingLease {
    fn drop(&mut self) {
        if let Some(g) = self.grant.take() {
            self.authority.release(&g.id, Some(g.generation));
        }
        if let Some(ticket) = self.ticket.take() {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                let node = Arc::clone(&self.node);
                rt.spawn(async move {
                    let _ = node.release_ticket(&ticket).await;
                });
            }
        }
    }
}

/// What the maintenance routes need from the environment, as Core's index.cjs reads it.
pub struct Maint {
    pub kind: server_stats::Kind,
    pub router: Option<router::Router>,
    pub gate: Option<Arc<gate::Gate>>,
    pub ini: Option<ini::Store>,
    pub guard: reload::Guard,
    /// The sizing reads' mounts: where a container path (`/models/...`) is on disk.
    pub(crate) sizing: Arc<crate::model_sizing::Sizing>,
    /// The evidence store (`evidence-<hash>/`), this front being its only writer.
    pub evidence: Option<Arc<evidence::Store>>,
    /// The inbox Node's evidence producers leave records in.
    pub inbox_dir: Option<PathBuf>,
    /// model -> (when, live identity), the 30 s identity cache.
    pub(crate) identities: std::sync::Mutex<Vec<(String, Instant, Option<evidence_live::Live>)>>,
    /// The no-model-selected default (model_state.rs).
    pub(crate) last_loaded: model_state::LastLoaded,
    /// The model-card source: `https://huggingface.co`, or the QA source.
    pub(crate) card_base: String,
    pub(crate) source_client: reqwest::Client,
    /// Node's own maintenance gate: the arbiter when the inference gate gives no hold.
    pub(crate) node: Arc<node_lease::NodeLeases>,
    /// The maintenance lease authority (row J): the one table of who holds the engine.
    pub authority: lease::Authority,
    /// The inbox entries that failed or could not be removed, by file name.
    pub(crate) drain: std::sync::Mutex<evidence_live::DrainState>,
    /// A job of this process is running: one at a time.
    local: Arc<AtomicBool>,
    /// `UI_DATA_DIR` and the 16-digit suffix of the state files.
    pub data_dir: Option<PathBuf>,
    pub suffix: String,
    pub(crate) recheck: evidence_recheck::Recheck,
}

impl std::fmt::Debug for Maint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Maint")
            .field("kind", &self.kind)
            .field("router", &self.router)
            .field("gate", &self.gate)
            .field("ini", &self.ini)
            .finish_non_exhaustive()
    }
}

/// `now + max`, or the furthest representable deadline (a hold with no limit of its own).
fn deadline_after(max: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(max)
        .or_else(|| now.checked_add(Duration::from_secs(u64::from(u32::MAX))))
        .unwrap_or(now)
}

fn first(get: &impl Fn(&str) -> Option<String>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| get(k).filter(|v| !v.is_empty()))
}

impl Maint {
    pub fn from_lookup(
        kind: server_stats::Kind,
        engine: &server_stats::Engine,
        sizing: Arc<crate::model_sizing::Sizing>,
        get: &impl Fn(&str) -> Option<String>,
    ) -> Result<Maint, String> {
        let raw_base = first(
            get,
            &[
                "MODEL_MANAGER_BASE_URL",
                "LEMONADE_BASE_URL",
                "INFERENCE_BASE_URL",
            ],
        )
        .unwrap_or_else(|| "http://host.docker.internal:11434".into());
        // index.cjs: `sha256(MODEL_MANAGER_BASE).hex.slice(0, 16)`, over the value as configured.
        let suffix: String = Sha256::digest(raw_base.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            .chars()
            .take(16)
            .collect();
        let data_dir = get("UI_DATA_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from);
        let key = first(
            get,
            &[
                "MODEL_MANAGER_API_KEY",
                "INFERENCE_API_KEY",
                "LEMONADE_API_KEY",
            ],
        );
        let llama = kind == server_stats::Kind::LlamaCpp;
        let router = if llama {
            Some(
                router::Router::new(engine.base(), key.as_deref())?.with_budget_fence(
                    data_dir
                        .as_ref()
                        .map(|d| d.join(format!("budget-watch-{suffix}"))),
                    first(get, &["LLAMACPP_PRESET_PATH"]).map(PathBuf::from),
                    budget_watch::NodeBridge::new(
                        first(get, &["NOEVIA_LEGACY_UPSTREAM"]).as_deref(),
                        get("NOEVIA_PROJECT_STORE_TOKEN").as_deref(),
                    )?,
                ),
            )
        } else {
            None
        };
        let gate = if llama {
            Some(Arc::new(gate::Gate::new(
                engine.base(),
                get("INFERENCE_GATE_ADMIN_TOKEN").as_deref(),
            )?))
        } else {
            None
        };
        // models-ini-writer.cjs: `MODELS_INI_WRITER` is `web` unless set; `model-loader` needs the
        // model management service's address.
        let writer = match get("MODELS_INI_WRITER")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "" | "web" => None,
            "model-loader" => {
                let url = first(get, &["MODEL_LOADER_URL"])
                    .ok_or("MODELS_INI_WRITER=model-loader requires MODEL_LOADER_URL")?;
                Some(ini::LoaderWriter::new(
                    &url,
                    get("MODEL_LOADER_TOKEN").as_deref(),
                )?)
            }
            other => return Err(format!("unsupported MODELS_INI_WRITER: {other}")),
        };
        let ini = first(get, &["LLAMACPP_PRESET_PATH"])
            .filter(|_| llama)
            .map(|p| ini::Store::new(PathBuf::from(p), writer));
        let state = |name: &str| {
            data_dir
                .as_ref()
                .map(|d| d.join(format!("{name}-{suffix}.json")))
        };
        let evidence = data_dir
            .as_ref()
            .filter(|_| llama)
            .map(|d| Arc::new(evidence::Store::new(d.join(format!("evidence-{suffix}")))));
        let inbox_dir = data_dir
            .as_ref()
            .filter(|_| llama)
            .map(|d| evidence_live::inbox_path(d, &suffix));
        // NOEVIA_QA_MODEL_CARD_SOURCE: the model-card source for synthetic runs (tests, the contract
        // replay), so they never reach the Internet. Production leaves it unset.
        let card_base = first(get, &["NOEVIA_QA_MODEL_CARD_SOURCE"])
            .map(|u| u.trim_end_matches('/').to_owned())
            .unwrap_or_else(|| "https://huggingface.co".into());
        let source_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| format!("model card client: {e}"))?;
        let node = Arc::new(node_lease::NodeLeases::new(
            first(get, &["NOEVIA_LEGACY_UPSTREAM"]).as_deref(),
            get("NOEVIA_PROJECT_STORE_TOKEN").as_deref(),
        )?);
        Ok(Maint {
            kind,
            router,
            gate,
            ini,
            guard: reload::Guard::with(
                state("native-preset-reload"),
                &get("NOEVIA_START_ID").unwrap_or_default(),
                reload::Timing::default(),
            ),
            sizing,
            evidence,
            inbox_dir,
            identities: std::sync::Mutex::new(Vec::new()),
            last_loaded: model_state::LastLoaded::default(),
            card_base,
            source_client,
            node,
            authority: lease::Authority::new(),
            drain: std::sync::Mutex::new(evidence_live::DrainState::default()),
            local: Arc::new(AtomicBool::new(false)),
            data_dir,
            suffix,
            recheck: evidence_recheck::Recheck::new(get)?,
        })
    }

    /// A maintenance hold for work of up to `max`. The gate's when it grants one. Otherwise (no admin
    /// token, or nothing at the engine address knows the hold) a lease on Node's own maintenance gate:
    /// the normal production case, exclusive with Node's chat, tune, calibration, sweep, idle-unload,
    /// delete and prune. No lease, no work.
    ///
    /// `need_gate`: a gate fronts the engine but this process has no admin token for its hold, and the
    /// work would end requests other clients have running through that gate: refuse.
    pub async fn acquire(
        &self,
        reason: &str,
        max: Duration,
        need_gate: bool,
    ) -> Result<Hold, HoldError> {
        // Review P2: the reservation is owned from the moment it is taken, so a dropped acquisition
        // (or any error) clears it; only a completed Hold takes it over.
        let Some(reserved) = Reservation::take(&self.local) else {
            return Err(HoldError::Busy);
        };
        let hold = self.acquire_locally_held(reason, max, need_gate).await?;
        reserved.transfer();
        Ok(hold)
    }

    async fn acquire_locally_held(
        &self,
        reason: &str,
        max: Duration,
        need_gate: bool,
    ) -> Result<Hold, HoldError> {
        self.acquire_inner(reason, max, need_gate, None, false)
            .await
            .map_err(|r| match r {
                HoldRefusal::Busy => HoldError::Busy,
                HoldRefusal::Gate {
                    status, message, ..
                } => HoldError::Gate { status, message },
                HoldRefusal::NeedsGate => HoldError::NeedsGate,
                HoldRefusal::NodeUnavailable | HoldRefusal::OverLimit | HoldRefusal::ShortLease => {
                    HoldError::NodeUnavailable
                }
            })
    }

    /// A hold for a run that may last `lease_for` (calibration): the gate's hold with no limit of its
    /// own, as today; otherwise a lease on Node's gate asking for that hard cap. A run longer than
    /// [`node_lease::MAX_LEASE`] is refused before any lease is taken, and a lease granted shorter than
    /// asked (an older Node) is given back: such a run would outlive its exclusion.
    pub async fn acquire_for_run(
        &self,
        reason: &str,
        lease_for: Duration,
    ) -> Result<Hold, HoldRefusal> {
        let Some(reserved) = Reservation::take(&self.local) else {
            return Err(HoldRefusal::Busy);
        };
        let hold = self
            .acquire_inner(reason, Duration::MAX, false, Some(lease_for), false)
            .await?;
        reserved.transfer();
        Ok(hold)
    }

    /// Delta review P1: a fresh hold for a recovery after a lost one, taken while the caller keeps this
    /// process's local admission closed itself (its admission fence): no new reservation, and the hold
    /// never clears local admission when it ends. The lost hold is never reused.
    pub(crate) async fn acquire_recovery(
        &self,
        reason: &str,
        lease_for: Duration,
    ) -> Result<Hold, HoldRefusal> {
        let mut hold = self
            .acquire_inner(reason, Duration::MAX, false, Some(lease_for), true)
            .await?;
        drop(hold.defer_local_release());
        Ok(hold)
    }

    async fn acquire_inner(
        &self,
        reason: &str,
        max: Duration,
        need_gate: bool,
        lease_for: Option<Duration>,
        recovery: bool,
    ) -> Result<Hold, HoldRefusal> {
        let Some(gate) = self.gate.as_ref() else {
            return Err(HoldRefusal::NeedsGate);
        };
        match gate.hold(reason, Duration::ZERO).await {
            Ok(Some(grant)) => Ok(Hold::gated(
                Arc::clone(&self.local),
                Arc::clone(gate),
                grant,
                reason,
                deadline_after(max),
            )),
            Ok(None) => {
                if need_gate && !gate.has_admin() && gate.held().await.is_some() {
                    return Err(HoldRefusal::NeedsGate);
                }
                if lease_for.is_some_and(|d| d > node_lease::MAX_LEASE) {
                    return Err(HoldRefusal::OverLimit);
                }
                // The authority first: a lease Node's gate or another job of this front holds is a 409
                // before Node is asked anything.
                if recovery {
                    let ticket = self.authority.require_recovery();
                    if !self.node.recovery_barrier(true, &ticket).await {
                        return Err(HoldRefusal::NodeUnavailable);
                    }
                }
                let grant = if recovery {
                    self.authority.try_recovery(lease_for, reason)
                } else {
                    self.authority
                        .try_acquire(lease::Origin::Front, lease_for, reason)
                };
                let Ok(own) = grant else {
                    return Err(HoldRefusal::Busy);
                };
                // Review P2: the authority grant and the latch it asks Node for are owned from here; a
                // drop before the Hold exists gives both back (the latch by its ticket, in case Node
                // granted it but the answer was lost).
                let mut pending = PendingLease {
                    authority: self.authority.clone(),
                    grant: Some(own),
                    node: Arc::clone(&self.node),
                    ticket: None,
                };
                let ticket = pending.grant.as_ref().map(|g| g.id.clone());
                pending.ticket.clone_from(&ticket);
                match self
                    .node
                    .acquire_kind(lease_for, ticket.as_deref(), recovery)
                    .await
                {
                    Ok(grant) if lease_for.is_some_and(|d| grant.hard < d) => {
                        drop(pending);
                        self.node.release(&grant.id).await;
                        Err(HoldRefusal::ShortLease)
                    }
                    Ok(grant) => {
                        let own = pending.complete();
                        Ok(Hold::leased(
                            Arc::clone(&self.local),
                            Arc::clone(&self.node),
                            grant,
                            own.map(|g| (self.authority.clone(), g)),
                            reason,
                            deadline_after(max),
                        ))
                    }
                    // Refused: Node holds no latch for it, but the guard's ticket release is harmless.
                    Err(node_lease::LeaseFault::Busy) => Err(HoldRefusal::Busy),
                    Err(node_lease::LeaseFault::Unavailable) => Err(HoldRefusal::NodeUnavailable),
                }
            }
            Err(f) if f.busy() => Err(HoldRefusal::Busy),
            Err(f) => Err(HoldRefusal::Gate {
                status: f.status(),
                code: f.code(),
                message: f.message(),
            }),
        }
    }

    /// Node's boot warning, when a gate fronts the engine and this process has no admin token for it.
    /// `inference-gate-client.cjs` `adminTokenWarning` is the same text (a key name, never a value).
    pub async fn missing_token_warning(&self) -> Option<&'static str> {
        let gate = self.gate.as_ref()?;
        if gate.has_admin() {
            return None;
        }
        gate.held().await?;
        Some(MISSING_TOKEN_WARNING)
    }

    /// Whether a job of this process holds the engine (the local flag).
    pub fn local_held(&self) -> bool {
        self.local.load(Ordering::Acquire)
    }
}

/// The body of a request, capped (http.cjs `readBody`): `Err(413)` over the limit.
async fn body_text(body: Body, limit: usize, within: Duration) -> Result<String, u16> {
    let read = async {
        let mut body = body;
        let mut out: Vec<u8> = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| 400u16)?;
            if let Ok(data) = frame.into_data() {
                if out.len() + data.len() > limit {
                    return Err(413u16);
                }
                out.extend_from_slice(&data);
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    };
    tokio::time::timeout(within, read)
        .await
        .map_err(|_| 408u16)?
}

fn text(status: u16, body: String) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        body,
        true,
    )
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if !owned(req.method(), &path) {
        return app.proxy.forward(conn.peer, req).await;
    }
    if path == evidence_recheck::PATH {
        return evidence_recheck::serve(app, req).await;
    }
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let maint = Arc::clone(&app.config.maint);
    if parts.method == Method::GET {
        return evidence_get(&maint, &parts.uri).await;
    }
    // routes/models.cjs: every write under /api/models/ is administrator-only, before anything else.
    if id.user.role != Role::Admin {
        return reply::error(StatusCode::FORBIDDEN, "administrator required", true);
    }
    match path.as_str() {
        RELOAD => {
            // `!modelManager.reloadPresets` precedes the body read.
            if maint.router.is_none() {
                return reply::error(
                    StatusCode::NOT_FOUND,
                    "This engine does not use a preset file",
                    true,
                );
            }
            let raw = match body_text(body, 1024 * 1024, Duration::from_secs(10)).await {
                Ok(t) => t,
                Err(413) => {
                    return reply::error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "Request exceeds size limit",
                        true,
                    )
                }
                Err(_) => return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true),
            };
            let parsed: Value = if raw.is_empty() {
                Value::Object(Default::default())
            } else {
                match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true),
                }
            };
            let unload = parsed.get("unload") == Some(&Value::Bool(true));
            // The work outlives the request: a client that leaves does not strand the hold.
            let job = tokio::spawn(async move { maint.reload_presets(unload).await });
            match job.await {
                Ok(o) => text(o.status, o.body),
                Err(_) => reply::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not reload model profiles.",
                    true,
                ),
            }
        }
        PRUNE => {
            // `!modelManager.enabled` precedes the work; the route reads no body.
            if app.config.sizing.kind() != server_stats::Kind::LlamaCpp {
                return reply::error(StatusCode::NOT_FOUND, "model management is disabled", true);
            }
            // The work outlives the request: a client that leaves does not strand a half-done prune.
            let (app, user) = (Arc::clone(app), id.user.id.clone());
            let job = tokio::spawn(async move { crate::prune_missing::run(&app, &user).await });
            match job.await {
                Ok(a) => text(a.status, a.body),
                Err(_) => reply::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not remove missing presets.",
                    true,
                ),
            }
        }
        DELETE => {
            // The body is read before anything else, as Core's route does.
            let raw = match body_text(body, 1024 * 1024, Duration::from_secs(10)).await {
                Ok(t) => t,
                Err(413) => {
                    return reply::error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "Request exceeds size limit",
                        true,
                    )
                }
                Err(_) => return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true),
            };
            // The work outlives the request: a client that leaves does not strand a half-done delete.
            let (app, user) = (Arc::clone(app), id.user.id.clone());
            let job =
                tokio::spawn(async move { crate::delete_model::run(&app, &user, &raw).await });
            match job.await {
                Ok(a) => text(a.status, a.body),
                Err(_) => reply::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not delete the model.",
                    true,
                ),
            }
        }
        LOAD | UNLOAD => {
            // The body is read before anything else, as Core's route does.
            let raw = match body_text(body, 1024 * 1024, Duration::from_secs(10)).await {
                Ok(t) => t,
                Err(413) => {
                    return reply::error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "Request exceeds size limit",
                        true,
                    )
                }
                Err(_) => return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true),
            };
            if path == LOAD {
                // Not spawned: a client that leaves while the load waits in the gate's queue drops this future,
                // which leaves the queue and starts no switch (its lease, if granted, is given back on drop).
                let a = crate::load_model::run(app, crate::load_model::Verb::Load, &raw).await;
                return text(a.status, a.body);
            }
            let app = Arc::clone(app);
            let job = tokio::spawn(async move {
                crate::load_model::run(&app, crate::load_model::Verb::Unload, &raw).await
            });
            match job.await {
                Ok(a) => text(a.status, a.body),
                Err(_) => reply::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not unload the model.",
                    true,
                ),
            }
        }
        EVIDENCE_IMPORT => evidence_import(&maint, body).await,
        _ => reply::error(StatusCode::NOT_FOUND, "Not found", true),
    }
}

/// A failure of the evidence work, as `errorResponse` answers it.
fn failure(f: evidence_live::Failure) -> Response<Body> {
    reply::error(
        StatusCode::from_u16(f.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        &f.message,
        true,
    )
}

/// `GET /api/models/evidence?model=`: any signed-in user (Node checks no role on a GET).
async fn evidence_get(maint: &Arc<Maint>, uri: &axum::http::Uri) -> Response<Body> {
    if maint.router.is_none() {
        return reply::error(
            StatusCode::NOT_FOUND,
            "Qualification evidence needs the native engine",
            true,
        );
    }
    let model = crate::model_sizing::model_param(uri).unwrap_or_default();
    if !evidence_live::model_ok(&model) {
        return reply::error(StatusCode::BAD_REQUEST, "Choose a model", true);
    }
    let m = Arc::clone(maint);
    match tokio::spawn(async move { m.evidence_body(&model).await }).await {
        Ok(Ok(body)) => text(200, body),
        Ok(Err(f)) => failure(f),
        Err(_) => failure(evidence_live::Failure {
            status: 500,
            message: "Internal error".into(),
        }),
    }
}

/// `POST /api/models/evidence/import`: re-import a model's attributable model-card evidence. An
/// offline or failed lookup is never an error here: the answer is the evidence as it stands.
async fn evidence_import(maint: &Arc<Maint>, body: Body) -> Response<Body> {
    if maint.router.is_none() {
        return reply::error(
            StatusCode::NOT_FOUND,
            "Model evidence import needs the native engine",
            true,
        );
    }
    let parsed = match body_text(body, 1024 * 1024, Duration::from_secs(10)).await {
        Ok(t) if t.is_empty() => Some(Value::Object(Default::default())),
        Ok(t) => serde_json::from_str::<Value>(&t).ok(),
        Err(_) => None,
    };
    let model = parsed
        .as_ref()
        .and_then(|b| b.get("model"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if !evidence_live::model_ok(&model) {
        return reply::error(StatusCode::BAD_REQUEST, "Choose a model", true);
    }
    let checkpoint = parsed
        .as_ref()
        .and_then(|b| b.get("checkpoint"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let m = Arc::clone(maint);
    let work = tokio::spawn(async move {
        // `.catch(() => null)`: the import's own failures are swallowed; the answer follows.
        let _ = m.import_evidence(&model, checkpoint.as_deref()).await;
        m.evidence_body(&model).await
    });
    match work.await {
        Ok(Ok(body)) => text(200, body),
        Ok(Err(f)) => failure(f),
        Err(_) => failure(evidence_live::Failure {
            status: 500,
            message: "Internal error".into(),
        }),
    }
}
