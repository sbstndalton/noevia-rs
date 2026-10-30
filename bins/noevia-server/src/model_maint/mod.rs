//! Model maintenance moves to the Rust front (sbstndalton/noevia#2210, models round B; follows the
//! model reads of #2187 and #2189). The authorities of the maintenance work on the llama.cpp router:
//!
//! - [`gate`]: the inference gate's maintenance hold, the one exclusion every client of the router
//!   (Node's chat, Diary, this front) respects. A hold has a deadline and is given back on failure.
//! - [`node_lease`]: with no gate, a lease on Node's own maintenance gate, so the reload is still
//!   exclusive with Node's chat and tune.
//! - [`router`]: the router's HTTP API as the maintenance work uses it.
//! - [`ini`]: the models.ini store (read, parse, commit through the model-loader or the web writer).
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

pub mod evidence;
pub mod evidence_live;
pub mod gate;
pub mod ini;
pub mod node_lease;
pub mod reload;
pub mod router;

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
pub const EVIDENCE: &str = "/api/models/evidence";
pub const EVIDENCE_IMPORT: &str = "/api/models/evidence/import";

/// The maintenance routes this front answers.
pub const ROUTES: &[&str] = &[RELOAD, EVIDENCE, EVIDENCE_IMPORT];

const MISSING_TOKEN_WARNING: &str = "[inference-gate] INFERENCE_GATE_ADMIN_TOKEN is not set: the memory watchdog cannot unload a model that is answering a request, and \"unload anyway\" on the Models page is unavailable. Set it to the admin token of the gate.";

/// The methods the front answers on each (routes.toml); every other method stays Node's.
pub fn owned(method: &Method, path: &str) -> bool {
    match path {
        RELOAD | EVIDENCE_IMPORT => method == Method::POST,
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
    /// The model-card source: `https://huggingface.co`, or the QA source.
    pub(crate) card_base: String,
    pub(crate) source_client: reqwest::Client,
    /// Node's own maintenance gate: the arbiter when the inference gate gives no hold.
    node: Arc<node_lease::NodeLeases>,
    /// The inbox entries that failed or could not be removed, by file name.
    pub(crate) drain: std::sync::Mutex<evidence_live::DrainState>,
    /// A job of this process is running: one at a time.
    local: Arc<AtomicBool>,
    /// `UI_DATA_DIR` and the 16-digit suffix of the state files.
    pub data_dir: Option<PathBuf>,
    pub suffix: String,
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
            Some(router::Router::new(engine.base(), key.as_deref())?)
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
            guard: reload::Guard::new(state("native-preset-reload")),
            sizing,
            evidence,
            inbox_dir,
            identities: std::sync::Mutex::new(Vec::new()),
            card_base,
            source_client,
            node,
            drain: std::sync::Mutex::new(evidence_live::DrainState::default()),
            local: Arc::new(AtomicBool::new(false)),
            data_dir,
            suffix,
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
        if self
            .local
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(HoldError::Busy);
        }
        let taken = self.acquire_locally_held(reason, max, need_gate).await;
        if taken.is_err() {
            self.local.store(false, Ordering::Release);
        }
        taken
    }

    async fn acquire_locally_held(
        &self,
        reason: &str,
        max: Duration,
        need_gate: bool,
    ) -> Result<Hold, HoldError> {
        let Some(gate) = self.gate.as_ref() else {
            return Err(HoldError::NeedsGate);
        };
        match gate.hold(reason, Duration::ZERO).await {
            Ok(Some(grant)) => Ok(Hold::gated(
                Arc::clone(&self.local),
                Arc::clone(gate),
                grant,
                reason,
                Instant::now() + max,
            )),
            Ok(None) => {
                if need_gate && !gate.has_admin() && gate.held().await.is_some() {
                    return Err(HoldError::NeedsGate);
                }
                match self.node.acquire().await {
                    Ok(grant) => Ok(Hold::leased(
                        Arc::clone(&self.local),
                        Arc::clone(&self.node),
                        grant,
                        reason,
                        Instant::now() + max,
                    )),
                    Err(node_lease::LeaseFault::Busy) => Err(HoldError::Busy),
                    Err(node_lease::LeaseFault::Unavailable) => Err(HoldError::NodeUnavailable),
                }
            }
            Err(f) if f.busy() => Err(HoldError::Busy),
            Err(f) => Err(HoldError::Gate {
                status: f.status(),
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
