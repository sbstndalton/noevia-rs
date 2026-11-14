//! Track R11 (S12): `POST /api/models/pull` answered by the front (crates/model-pull's pull flow,
//! recorded from core's routes/models.cjs and llamacpp-manager.cjs), behind the model writes switch
//! `NOEVIA_RUST_MODEL_WRITES=rust` (model_writes_routes.rs `Settings::answers`). Unset, the POST
//! goes to core exactly as before; core, told the same switch, must refuse its own (a direct
//! request on its port cannot pull beside the front).
//!
//! The flow, in core's order: the write prelude (core drops its shared model reads:
//! `/internal/auto-roles/invalidate`, as model_writes_routes.rs; a member gets 403 `administrator
//! required`), the folder-scan clear (core's trusted scan bridge
//! `/internal/model-manager-scan/clear`, as model_manager_routes.rs), the body (`readBody`, 1 MiB,
//! 413 over it; `JSON.parse`, 400 `invalid JSON`), 400 `checkpoint required`, 404 `model
//! management is disabled` (no manager), then llama.cpp's pull: core's `mutate` (its maintenance
//! gate's `enter()`) is the front's inference admission authority's maintenance-only lease (the
//! embedding ask: in core exactly `modelManager.enterInference()`, which is what `mutate` takes),
//! held for the checkpoint check, the tracker's record and the router call and given back on every
//! path; the checkpoint test; core's download tracker told over its bridge
//! (`/internal/models/downloads/requested`); the router's `POST /models` `{ model }` (the router
//! client of model_maint: its key as `Authorization: Bearer`, no redirects, 120 s); the tracker
//! told of a refusal (`/internal/models/downloads/rejected`); the answer. A pull carries no
//! `X-Noevia-Maintenance` header: core's `mutate` is not a maintenance hold, so its router request
//! carries none either.
//!
//! The front never runs with the Lemonade manager (config.rs refuses it at startup), so its pull
//! request has no path here: a manager request that is not the router's `POST /models` is refused
//! as a throw (500 `Internal error`).
//!
//! # Named differences (fail closed)
//!
//! - `scan-clear-unconfirmed`: core's in-memory scan clear cannot fail. A clear core does not
//!   confirm (`{"cleared":true}`) is answered at once, 502 `{"error":"The model management service
//!   is not responding."}` (the proxy's answer to the same failure), before the body is read and
//!   before any manager call.
//! - `download-record-failed`: core's tracker cannot fail to record. A `requested` call core does
//!   not confirm (200 `{"ok":true}`; a 404, core without a tracker, included), and a checkpoint the
//!   bridge does not take (not a string of 1 to 512 characters: core's test also passes an array
//!   that reads as `owner/repo`), is answered 503
//!   `{"error":"The download could not be recorded."}`: the lease is given back and the router is
//!   not asked. When core may have recorded the `requested` call all the same (no answer within
//!   the bridge's deadline, a 200 without `{"ok":true}`, a 5xx), a best-effort `rejected` for the
//!   same model goes first, still under the lease, so core's tracker does not keep a `requested`
//!   row (later shown as `unknown`) for a pull nobody asked the router for; its outcome is not
//!   read. A 4xx is core's bridge refusing before it records anything (a body it does not take,
//!   no tracker, not the front), so nothing is sent then: a `rejected` row would overwrite the
//!   model's earlier row. A `rejected` call that fails is logged only (the router already refused).
//! - `admission-refused`: core's `enter()` refuses only when its own gate is closed (503 with its
//!   reason, answered `Internal error`). The front's authority may also refuse when core does not
//!   answer it; that is the same 503 `Internal error`.
//! - `body-unsupported`: a body core's `JSON.parse` reads but the port cannot hold is passed to core
//!   unchanged (and so refused by it under the switch: 503), as model_writes_routes.rs does.
//!
//! A member's request is answered 403 (after the prelude's invalidations, as core) without its
//! body being read: core's prelude refuses before `readBody`, and so a member cannot make the
//! front hold a 1 MiB body.
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse;
use engine_admission::Ask;
use js_json::{wtf16, JValue};
use model_pull::artifact::{self, Artifacts, FetchError, Fetched, Fetcher};
use model_pull::manager::{self, Reply, Thrown};
use model_pull::{Answered, Call, Outcome};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PATH: &str = crate::model_pull_routes::PULL;
/// The name the maintenance-only lease is asked with (core's latch requires one; an embedding ask
/// admits no model).
const LEASE_NAME: &str = "model-pull";
pub const DOWNLOADS_REQUESTED: &str = "/internal/models/downloads/requested";
pub const DOWNLOADS_REJECTED: &str = "/internal/models/downloads/rejected";
const NOT_RESPONDING: &str = "The model management service is not responding.";
const NOT_RECORDED: &str = "The download could not be recorded.";

/// The download tracker bridge's deadline (ms): 5 s, as every trusted core bridge call here.
static TRACKER_TIMEOUT_MS: AtomicU64 = AtomicU64::new(5_000);

/// Test-only seam: the download tracker bridge's deadline for this process, so an end-to-end test
/// can show a `requested` call that outlives it without waiting 5 s. Nothing in the binary calls
/// this.
#[doc(hidden)]
pub fn set_tracker_timeout_for_tests(timeout: Duration) {
    TRACKER_TIMEOUT_MS.store(
        u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        Ordering::SeqCst,
    );
}

/// What core's download tracker bridge made of one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Told {
    /// 200 `{"ok":true}`: recorded.
    Recorded,
    /// A 4xx: the bridge refuses before it records anything.
    Refused,
    /// No answer within the deadline, or an answer that confirms nothing (core may have recorded).
    Unsure,
}

fn log(entry: &JValue) {
    println!("[models] {}", wtf16::stringify(entry).unwrap_or_default());
}

/// The pull's host: each step run to its end on the runtime (inside `spawn_blocking`).
struct FrontPull {
    app: Arc<App>,
    rt: tokio::runtime::Handle,
    lease: Option<engine_admission::Lease>,
}

impl Drop for FrontPull {
    fn drop(&mut self) {
        // The backstop: every path of the flow leaves the gate itself.
        if let Some(lease) = self.lease.take() {
            self.rt.spawn(lease.release());
        }
    }
}

impl FrontPull {
    /// One call of a trusted core bridge: its status and body, `None` without an answer.
    fn node_raw(
        &self,
        path: &str,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> Option<(u16, Vec<u8>)> {
        let app = Arc::clone(&self.app);
        let (path, body) = (path.to_string(), body.clone());
        self.rt
            .block_on(async move { app.config.research.node_call(&path, &body, timeout).await })
    }

    /// One call of a trusted core bridge: the parsed answer of a 200, else `None`.
    fn node(&self, path: &str, body: &serde_json::Value) -> Option<JValue> {
        let (status, bytes) = self.node_raw(path, body, Duration::from_secs(5))?;
        if status != 200 {
            return None;
        }
        wtf16::parse(std::str::from_utf8(&bytes).ok()?).ok()
    }

    /// The model the tracker bridge takes for `checkpoint`: `{"model": "<checkpoint>"}`, a
    /// non-empty string of at most 512 UTF-16 units (core's `model.length > 512` refusal);
    /// anything else (an array core's test reads as a string, a lone surrogate) is never sent,
    /// which fails the pull closed.
    fn tracked_model(checkpoint: &JValue) -> Option<&str> {
        checkpoint.as_str().filter(|m| {
            !m.is_empty()
                && wtf16::len(m) <= 512
                && !m.chars().any(|c| wtf16::lone_unit(c).is_some())
        })
    }

    fn tell(&self, path: &str, model: &str) -> Told {
        let timeout = Duration::from_millis(TRACKER_TIMEOUT_MS.load(Ordering::SeqCst));
        let Some((status, bytes)) =
            self.node_raw(path, &serde_json::json!({ "model": model }), timeout)
        else {
            return Told::Unsure;
        };
        match status {
            200 => {
                let ok = std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|t| wtf16::parse(t).ok())
                    .is_some_and(|v| v.get("ok") == &JValue::Bool(true));
                if ok {
                    Told::Recorded
                } else {
                    Told::Unsure
                }
            }
            400..=499 => Told::Refused,
            _ => Told::Unsure,
        }
    }
}

impl Fetcher for FrontPull {
    fn fetch(&mut self, _: &artifact::Fetch) -> Result<Fetched, FetchError> {
        // A pull fetches nothing public.
        Err(FetchError::Failed("fetch failed".into()))
    }
    fn now_ms(&self) -> i64 {
        identity::now_ms()
    }
}

impl model_pull::Host for FrontPull {
    fn invalidate_model_reads(&mut self) {
        let _ = self.node("/internal/auto-roles/invalidate", &serde_json::json!({}));
    }

    fn clear_model_scan(&mut self) -> Result<(), Answered> {
        let app = Arc::clone(&self.app);
        let cleared = self.rt.block_on(async move {
            crate::model_manager_scan::cache(&app, "clear", &serde_json::json!({})).await
        });
        if cleared.is_some_and(|v| v.get("cleared") == &JValue::Bool(true)) {
            return Ok(());
        }
        log(&JValue::obj([(
            "event",
            JValue::from("pull.scan_clear_failed"),
        )]));
        Err(Answered {
            status: 502,
            message: NOT_RESPONDING.into(),
        })
    }

    fn send(&mut self, request: &manager::Request) -> Result<Reply, Thrown> {
        let failed = || Thrown::internal("fetch failed");
        let router = self.app.config.maint.router.as_ref().ok_or_else(failed)?;
        // Only the router's own pull: anything else (a Lemonade request) is not sent. The crate
        // names the URL from the base it was handed (manager_config), so the test is against that
        // base; the request itself goes to the router client's own base, which is core's
        // (engine.rs: `.replace(/\/+$/, '').replace(/\/v1$/, '')`, once).
        let wanted = format!("{}/models", manager_config(&self.app).base());
        if request.target != manager::Target::Manager
            || request.method != "POST"
            || request.url != wanted
        {
            return Err(failed());
        }
        let router = router.clone();
        let (body, timeout) = (
            request.body.clone(),
            Duration::from_millis(u64::from(request.timeout_ms)),
        );
        // No maintenance hold is held (module documentation): no X-Noevia-Maintenance.
        let raw = self
            .rt
            .block_on(async move {
                router
                    .send_text(reqwest::Method::POST, "/models", body, None, &[], timeout)
                    .await
            })
            .map_err(|_| failed())?;
        // fetchJson's `redirect: 'error'`: a redirect throws.
        if (300..400).contains(&raw.status) {
            return Err(failed());
        }
        let Some(bytes) = raw.bytes else {
            return Ok(Reply {
                ok: false,
                status: raw.status,
                body: JValue::Undefined,
            });
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let body = wtf16::parse(&text).unwrap_or(JValue::Str(text));
        Ok(Reply {
            ok: raw.ok,
            status: raw.status,
            body,
        })
    }

    fn enter(&mut self) -> Result<(), Thrown> {
        let admission = self.app.config.research.admission();
        let cancel = model_stream::Cancel::new();
        let lease = self
            .rt
            .block_on(async move {
                admission
                    .enter(Ask::Embedding { model: LEASE_NAME }, &cancel)
                    .await
            })
            .map_err(|r| Thrown {
                status: Some(f64::from(r.status)),
                message: r.message,
            })?;
        self.lease = Some(lease);
        Ok(())
    }

    fn leave(&mut self) {
        if let Some(lease) = self.lease.take() {
            self.rt.block_on(lease.release());
        }
    }

    fn downloads_requested(&mut self, checkpoint: &JValue) -> Result<(), Answered> {
        let told = match Self::tracked_model(checkpoint) {
            Some(model) => {
                let told = self.tell(DOWNLOADS_REQUESTED, model);
                // Core may hold a `requested` row for a pull the router is never asked for: the
                // same model is marked rejected (idempotent on core), its outcome not read.
                if told == Told::Unsure {
                    let _ = self.tell(DOWNLOADS_REJECTED, model);
                }
                told
            }
            None => Told::Refused,
        };
        if told == Told::Recorded {
            return Ok(());
        }
        log(&JValue::obj([
            ("event", JValue::from("pull.requested_unrecorded")),
            ("rejectedSent", JValue::Bool(told == Told::Unsure)),
        ]));
        Err(Answered {
            status: 503,
            message: NOT_RECORDED.into(),
        })
    }

    fn downloads_rejected(&mut self, checkpoint: &JValue) {
        let told = Self::tracked_model(checkpoint).map(|m| self.tell(DOWNLOADS_REJECTED, m));
        if told != Some(Told::Recorded) {
            log(&JValue::obj([(
                "event",
                JValue::from("pull.rejected_unrecorded"),
            )]));
        }
    }
}

fn answer(status: u16, body: &JValue) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        wtf16::stringify(body).unwrap_or_default(),
        true,
    )
}

fn manager_config(app: &App) -> manager::Config {
    let kind = match app.config.maint.kind {
        server_stats::Kind::None => manager::Kind::None,
        server_stats::Kind::Lemonade => manager::Kind::Lemonade,
        server_stats::Kind::LlamaCpp => manager::Kind::Llamacpp,
    };
    manager::Config {
        kind,
        // The router client adds its own key; the crate's request names only the URL and body.
        base_url: app
            .config
            .maint
            .router
            .as_ref()
            .map(|r| r.base().to_string())
            .unwrap_or_default(),
        api_key: None,
    }
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
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
    let admin = id.user.role == server_auth::Role::Admin;
    // readBody's limit, one byte over to tell "too large" apart. A member's body is never read:
    // core's prelude answers 403 before `readBody` (the flow below gives the same 403 with its
    // invalidations, and reads no body for it).
    let bytes = if admin {
        axum::body::to_bytes(body, model_pull::BODY_LIMIT + 1)
            .await
            .ok()
    } else {
        drop(body);
        None
    };
    let config = manager_config(app);
    let (app2, rt) = (Arc::clone(app), tokio::runtime::Handle::current());
    let body = bytes.as_deref().map(<[u8]>::to_vec);
    let work = tokio::task::spawn_blocking(move || {
        let mut host = FrontPull {
            app: app2,
            rt,
            lease: None,
        };
        let call = Call {
            method: "POST",
            path: PATH,
            query: "",
            admin,
            body: body
                .as_deref()
                .filter(|b| b.len() <= model_pull::BODY_LIMIT),
        };
        // A pull has no artifact check: this state is never read.
        model_pull::handle(&config, &Mutex::new(Artifacts::new()), call, &mut host)
    })
    .await;
    match work {
        Ok(Outcome::Answer { status, body }) => answer(status, &body),
        Ok(Outcome::Forward) => {
            let req = Request::from_parts(parts, Body::from(bytes.unwrap_or_default()));
            app.proxy.forward(conn.peer, req).await
        }
        // Not reached (the path and method are this route's); a panic in the flow (its lease given
        // back as the host drops): core's answer to a throw.
        Ok(Outcome::Pass) | Err(_) => answer(
            500,
            &JValue::obj([("error", JValue::from("Internal error"))]),
        ),
    }
}
