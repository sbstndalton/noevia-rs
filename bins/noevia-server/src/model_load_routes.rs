//! Row A (2026-11-05): `POST /api/models/load` and `POST /api/models/unload`, moved from core
//! routes/models.cjs and llamacpp-manager.cjs (`load`, `unload`, `unloadAnyway`). Node refuses both POSTs
//! (core rust-model-load.cjs, 503 `RUST_MODEL_LOAD_OWNED`); every other method stays Node's.
//!
//! The steps are Node's, in Node's order (the recorded oracle, tests/fixtures/model-load-node.v1.json):
//! the session gate, administrators only, the body (`invalid JSON` / `name required`), no engine
//! (`model management is disabled`), Laya and the in-use sidecar models (409), a preset whose model
//! file is gone (409, load only), `mtp` (400, load only), then the engine.
//!
//! **Where the rules still live.** The one-model rule and what can refuse a load are Node's memory while
//! chat lives there: the maintenance gate, the inference memory budget, the watchdog quarantine, a pending
//! reload, and the inference gate's queue. The load takes its place there through the inference-admission
//! lease ([`EngineEntry::load`], `Ask::Engine`); the unload enters the maintenance gate only
//! ([`EngineEntry::mutate`], `Ask::Embedding`: `enterInference` and nothing else). Everything after that is
//! here: the sleeping check, the router's `POST /models/load` with the gate's lease header, the
//! already-running case, the gate's refusals, the readiness wait, the plain and the forced unload.
//! Row H moves the lease authority to the front; [`EngineEntry`] is the narrow seam it switches.
//!
//! **Cancellation.** A caller who leaves while the load waits in the queue starts no switch: the request's
//! future is dropped, which drops the lease ask (Node's `callerGone` aborts its wait) or the held
//! [`engine_admission::Lease`], whose `Drop` gives it back. A lease lost while the load runs stops it.
//!
//! **Reads.** As Node's handler did around itself, the shared model reads Node keeps (the installed list and
//! the folder scan) are dropped before and after every request, over `/internal/models/invalidate`. Best
//! effort: a missed drop leaves a read at most its few seconds old.
//!
//! Deviations (recorded in the row A checkpoint): a truthy `name` that is not a string is `name required`;
//! a refusal status outside Core's lease statuses (400, 409, 429, 503) arrives as 503; Node unreachable for
//! the sidecar names is 503 `Internal error` (Node never failed that synchronous check).
use crate::model_maint::router::{self, Reply};
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use engine_admission::{Ask, Lease, Refused};
use model_stream::Cancel;
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{json, Value};
use server_auth::Role;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const LOAD: &str = "/api/models/load";
pub const UNLOAD: &str = "/api/models/unload";
pub const ROUTES: &[&str] = &[LOAD, UNLOAD];

const INVALIDATE: &str = "/internal/models/invalidate";
const INVALIDATE_DEADLINE: Duration = Duration::from_secs(5);
const FACTS_DEADLINE: Duration = Duration::from_secs(15);
/// llamacpp-manager.cjs: `post()`'s 120 s, the readiness deadline, and its 200 ms step.
const LOAD_TIMEOUT: Duration = Duration::from_secs(120);
const READY_WITHIN: Duration = Duration::from_secs(120);
const READY_STEP: Duration = Duration::from_millis(200);
/// `forcedUnload(model, 60000)`; a plain `post('/models/unload')` keeps the 120 s default.
const FORCED_TIMEOUT: Duration = Duration::from_secs(60);
const BODY_LIMIT: usize = 1024 * 1024;

const SYSTEM_MODEL_LOAD_REASON: &str =
    "System routing model runs in its own service — it is not loaded or unloaded here";
const SIDECAR_MODEL_LOAD_REASON: &str =
    "A running sidecar (embedding or reranking) manages this model — it is not loaded or unloaded here";
const MISSING_MODEL_FILE_REASON: &str =
    "This preset points at a model file that is not in the models folder — it cannot be loaded";
const MTP_REFUSAL: &str = "Use the native preset editor to configure speculative decoding.";
const ADMIN_REQUIRED_UNLOAD: &str = "Unloading a model in use is not set up on this server.";
const LEASE_LOST: &str = "The model is busy. Try again shortly.";

/// routes/models.cjs `GATE_CODES`.
const GATE_CODES: &[&str] = &[
    "inference_queue_full",
    "inference_queue_timeout",
    "engine_unavailable",
    "engine_switch_stuck",
    "lease_unknown",
    "model_in_use",
    "engine_busy",
    "gate_busy",
    "model_not_found",
    "admin_required",
    "maintenance_held",
];

/// Whether this front answers `method path` (routes.toml: POST only).
pub fn owned(method: &Method, path: &str) -> bool {
    method == Method::POST && ROUTES.contains(&path)
}

// ── The seam to the engine's admission (row H switches its implementation) ─────────────────────

/// What a load or an unload holds in the engine's admission while it runs.
pub(crate) trait EngineEntry: Send + Sync {
    /// A place for a load of `model`: the maintenance gate, the load's budget checks and the gate's
    /// queue (core `acquireLease`), held until the returned lease is released or dropped.
    fn load<'a>(
        &'a self,
        model: &'a str,
        cancel: &'a Cancel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Lease, Refused>> + Send + 'a>>;
    /// The maintenance gate only (core `mutate`'s `maintenance.enter()`): no load, no queue.
    fn mutate<'a>(
        &'a self,
        model: &'a str,
        cancel: &'a Cancel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Lease, Refused>> + Send + 'a>>;
}

// ── Answers ─────────────────────────────────────────────────────────────────────────────────────

fn answer(status: u16, body: Value) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        body.to_string(),
        true,
    )
}

fn error(status: u16, message: &str) -> Response<Body> {
    answer(status, json!({ "error": message }))
}

/// index.cjs `errorResponse` for what Node's route threw (a transport failure, the maintenance gate).
fn internal(status: u16) -> Response<Body> {
    error(status, "Internal error")
}

fn cut(s: &str) -> String {
    s.chars().take(300).collect()
}

/// A refusal the client sees: routes/models.cjs `refusalOf`.
#[derive(Debug, Clone, PartialEq)]
struct Refusal {
    status: u16,
    code: String,
    error: String,
    retryable: bool,
}

impl Refusal {
    fn body(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("error".into(), Value::String(self.error.clone()));
        m.insert("code".into(), Value::String(self.code.clone()));
        m.insert("retryable".into(), Value::Bool(self.retryable));
        m
    }
}

/// routes/models.cjs `refusalOf(r)`, with `fallback` the words when the gate gave none.
fn refusal_of(reply: &Reply, fallback: &str) -> Option<Refusal> {
    if !(400..=599).contains(&reply.status) {
        return None;
    }
    let b = reply.body.as_ref()?;
    let e = b.get("error");
    if let Some(Value::Object(err)) = e {
        let code = err.get("code").and_then(Value::as_str);
        if err.get("type").and_then(Value::as_str) == Some("gate_error") {
            if let Some(code) = code.filter(|c| GATE_CODES.contains(c)) {
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty())
                    .unwrap_or(fallback);
                return Some(Refusal {
                    status: reply.status,
                    code: code.to_owned(),
                    error: cut(message),
                    retryable: err.get("retryable") == Some(&Value::Bool(true)),
                });
            }
        }
    }
    let code = b.get("code").and_then(Value::as_str)?;
    let text = e.and_then(Value::as_str)?;
    GATE_CODES.contains(&code).then(|| Refusal {
        status: reply.status,
        code: code.to_owned(),
        error: cut(text),
        retryable: b.get("retryable") == Some(&Value::Bool(true)),
    })
}

/// `${verb} failed: ${status}` or the refusal, for an engine answer that was not ok.
fn engine_failure(verb: &str, reply: &Reply, fallback: &str) -> Response<Body> {
    match refusal_of(reply, fallback) {
        Some(r) => answer(r.status, Value::Object(r.body())),
        None => error(502, &format!("{verb} failed: {}", reply.status)),
    }
}

/// What a refused load ask answers (Node: `failure(error)` in `load`, then the route's `refusalOf`).
fn load_refused(r: &Refused) -> Response<Body> {
    match r.code.as_deref() {
        Some(code) if GATE_CODES.contains(&code) => answer(
            r.status,
            json!({ "error": cut(&r.message), "code": code, "retryable": r.retryable }),
        ),
        // The maintenance gate threw before the load's own try (index.cjs errorResponse), and an
        // unreachable admission: refuse, never load.
        None if r.status == 503 => internal(503),
        _ => error(502, &format!("load failed: {}", r.status)),
    }
}

// ── The request ─────────────────────────────────────────────────────────────────────────────────

/// Drops Node's shared model reads, now and when dropped (the end of the request, however it ends).
struct Fence(Arc<App>);

impl Fence {
    fn now(app: &Arc<App>) {
        let app = Arc::clone(app);
        tokio::spawn(async move {
            let _ = app
                .config
                .research
                .node_call(INVALIDATE, &json!({}), INVALIDATE_DEADLINE)
                .await;
        });
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        if tokio::runtime::Handle::try_current().is_ok() {
            Fence::now(&self.0);
        }
    }
}

pub async fn serve(app: &Arc<App>, conn: crate::serve::Conn, req: Request<Body>) -> Response<Body> {
    // Node matches the exact pathname; a trailing slash is not this route there.
    let path = req.uri().path().to_owned();
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
    // routes/models.cjs: every non-GET model write drops the shared reads first, and again once settled.
    Fence::now(app);
    let _fence = Fence(Arc::clone(app));
    if id.user.role != Role::Admin {
        return error(403, "administrator required");
    }
    let raw = match crate::model_maint::body_text(body, BODY_LIMIT, Duration::from_secs(10)).await {
        Ok(t) => t,
        Err(413) => return error(413, "Request exceeds size limit"),
        Err(_) => return error(400, "invalid JSON"),
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return error(400, "invalid JSON");
    };
    // `body.name` on JSON null throws (500); on any other non-object it is undefined.
    if parsed.is_null() {
        return internal(500);
    }
    let Some(name) = parsed
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
    else {
        return error(400, "name required");
    };
    let maint = Arc::clone(&app.config.maint);
    if maint.kind == server_stats::Kind::None {
        return error(404, "model management is disabled");
    }
    let Some(router) = maint.router.clone() else {
        return error(404, "model management is disabled");
    };
    let presets = app
        .config
        .sizing
        .presets_path()
        .map(std::path::Path::to_path_buf);
    let roles =
        tokio::task::spawn_blocking(move || crate::model_kind::roles_beside(presets.as_deref()))
            .await
            .unwrap_or_default();
    if crate::model_kind::is_system_model(&roles, &name) {
        return error(409, SYSTEM_MODEL_LOAD_REASON);
    }
    match sidecar(app, &name).await {
        Some(true) => return error(409, SIDECAR_MODEL_LOAD_REASON),
        Some(false) => {}
        None => return internal(503),
    }
    let entry = ArcEntry(Arc::clone(&maint.recheck.admission));
    if path == LOAD {
        if missing_file(app, &name).await {
            return error(409, MISSING_MODEL_FILE_REASON);
        }
        if parsed.get("mtp").is_some() {
            return error(400, MTP_REFUSAL);
        }
        return load(&entry, &router, &name).await;
    }
    if parsed.get("force") == Some(&Value::Bool(true)) {
        let Some(admin) = maint.gate.as_ref().and_then(|g| g.admin_header()) else {
            return answer(
                403,
                json!({ "error": ADMIN_REQUIRED_UNLOAD, "code": "admin_required", "retryable": false }),
            );
        };
        return unload(&entry, &router, &name, Some(admin), false).await;
    }
    let can_force = maint.gate.as_ref().is_some_and(|g| g.has_admin());
    unload(&entry, &router, &name, None, can_force).await
}

/// Today's implementation: Node's admission lease (core inference-admission-lease.cjs), the client the
/// front already shares with the image recheck.
pub(crate) struct ArcEntry(pub Arc<engine_admission::Admission>);

impl EngineEntry for ArcEntry {
    fn load<'a>(
        &'a self,
        model: &'a str,
        cancel: &'a Cancel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Lease, Refused>> + Send + 'a>>
    {
        Box::pin(self.0.enter(Ask::Engine { model }, cancel))
    }
    fn mutate<'a>(
        &'a self,
        model: &'a str,
        cancel: &'a Cancel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Lease, Refused>> + Send + 'a>>
    {
        // `{model, embedding: true}` is exactly `enterInference()` in Core (paired: rust-embedding-background-v1).
        Box::pin(self.0.enter(Ask::Embedding { model }, cancel))
    }
}

/// `isSidecarModel(name)`: Node's sidecar names (the embedding service's models, the reranker in use and
/// the saved one). `None`: Node did not answer.
async fn sidecar(app: &Arc<App>, name: &str) -> Option<bool> {
    let facts = crate::model_state_routes::node_facts(
        app,
        &json!({ "want": "installed", "ids": [], "scan": false }),
        FACTS_DEADLINE,
    )
    .await?;
    match facts.get("sidecarNames") {
        js_json::JValue::Arr(names) => Some(names.iter().any(|n| n.as_str() == Some(name.trim()))),
        _ => None,
    }
}

/// #545: a fresh installed list names `name` with its file gone. A list that cannot be read lets the load
/// go on, as Node's `catch { installed = null }` did.
async fn missing_file(app: &Arc<App>, name: &str) -> bool {
    let sizing = Arc::clone(&app.config.sizing);
    if sizing.kind() != server_stats::Kind::LlamaCpp {
        return false;
    }
    let listing = sizing.listing().await;
    match crate::model_state_routes::installed_list(app, &sizing, &listing).await {
        Ok(rows) => rows.iter().any(|m| {
            m.get("name").as_str() == Some(name) && m.get("missingFile").as_bool() == Some(true)
        }),
        Err(_) => false,
    }
}

fn header_pairs(lease: &Lease) -> Vec<(HeaderName, HeaderValue)> {
    lease
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            Some((
                HeaderName::from_bytes(k.as_bytes()).ok()?,
                HeaderValue::from_str(v).ok()?,
            ))
        })
        .collect()
}

fn rows(reply: &Reply) -> Option<&Vec<Value>> {
    router::rows(&reply.body)
}

fn state_of<'r>(rows: &'r [Value], model: &str) -> Option<&'r Value> {
    rows.iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(model))
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `alreadyRunning(result)`: a 400 whose body text says the model is already running (#1758).
fn already_running(reply: &Reply) -> bool {
    reply.status == 400
        && reply
            .body
            .as_ref()
            .map(|b| b.to_string().to_lowercase())
            .is_some_and(|t| t.contains("already running"))
}

/// llamacpp-manager.cjs `load`, then the route's answer for it.
pub(crate) async fn load(
    entry: &dyn EngineEntry,
    router: &router::Router,
    model: &str,
) -> Response<Body> {
    let cancel = Cancel::new();
    let lease = match entry.load(model, &cancel).await {
        Ok(l) => l,
        Err(r) => return load_refused(&r),
    };
    let lost = lease.lost().clone();
    let outcome = tokio::select! {
        biased;
        () = lost.cancelled() => error(503, LEASE_LOST),
        out = load_steps(router, model, &lease) => out,
    };
    lease.release().await;
    outcome
}

async fn load_steps(router: &router::Router, model: &str, lease: &Lease) -> Response<Body> {
    // #1758: never ask the router to load a sleeping model; the next request wakes it.
    let Ok(before) = router.models(None).await else {
        return internal(500);
    };
    if before.ok
        && rows(&before)
            .and_then(|r| state_of(r, model))
            .and_then(|m| m.get("status"))
            .and_then(|s| s.get("value"))
            .and_then(Value::as_str)
            == Some("sleeping")
    {
        return answer(200, json!({ "ok": true }));
    }
    let Ok(started) = router
        .request_with(
            reqwest::Method::POST,
            "/models/load",
            Some(&json!({ "model": model })),
            &header_pairs(lease),
            LOAD_TIMEOUT,
        )
        .await
    else {
        return internal(500);
    };
    if !started.ok && !already_running(&started) {
        // A gate refusal goes through `failure()` first: its words default to the gate client's.
        return engine_failure("load", &started, "The inference gate refused the request.");
    }
    // Native load acknowledges launch; readiness must be observed separately.
    let deadline = Instant::now() + READY_WITHIN;
    while Instant::now() < deadline {
        let Ok(listing) = router.models(None).await else {
            return internal(500);
        };
        if !listing.ok {
            return engine_failure("load", &listing, "The model engine refused the request.");
        }
        let entry = rows(&listing).and_then(|r| state_of(r, model));
        let status = entry.and_then(|m| m.get("status"));
        let value = status.and_then(|s| s.get("value")).and_then(Value::as_str);
        if matches!(value, Some("loaded" | "sleeping")) {
            return answer(200, json!({ "ok": true }));
        }
        if entry.is_none()
            || truthy(status.and_then(|s| s.get("failed")))
            || value == Some("unloaded")
        {
            return error(502, "load failed: 502");
        }
        tokio::time::sleep(READY_STEP).await;
    }
    error(502, "load failed: 504")
}

/// `mutate(() => post('/models/unload', ...))`, plain or forced, then the route's answer.
pub(crate) async fn unload(
    entry: &dyn EngineEntry,
    router: &router::Router,
    model: &str,
    admin: Option<(HeaderName, HeaderValue)>,
    can_force: bool,
) -> Response<Body> {
    let cancel = Cancel::new();
    let held = match entry.mutate(model, &cancel).await {
        Ok(l) => l,
        // `maintenance.enter()` threw: index.cjs errorResponse, and an unreachable Node refuses too.
        Err(_) => return internal(503),
    };
    let forced = admin.is_some();
    let mut body = json!({ "model": model });
    if forced {
        if let Some(o) = body.as_object_mut() {
            o.insert("force".into(), Value::Bool(true));
        }
    }
    let extra: Vec<(HeaderName, HeaderValue)> = admin.into_iter().collect();
    let lost = held.lost().clone();
    let sent = tokio::select! {
        biased;
        () = lost.cancelled() => None,
        r = router.request_with(
            reqwest::Method::POST,
            "/models/unload",
            Some(&body),
            &extra,
            if forced { FORCED_TIMEOUT } else { LOAD_TIMEOUT },
        ) => Some(r),
    };
    held.release().await;
    let Some(sent) = sent else {
        return error(503, LEASE_LOST);
    };
    let Ok(r) = sent else {
        return internal(500);
    };
    if r.ok {
        return answer(
            200,
            if forced {
                json!({ "ok": true, "forced": true })
            } else {
                json!({ "ok": true })
            },
        );
    }
    match refusal_of(&r, "The model engine refused the request.") {
        Some(refusal) => {
            let mut b = refusal.body();
            if !forced && refusal.code == "model_in_use" {
                b.insert("canForce".into(), Value::Bool(can_force));
            }
            answer(refusal.status, Value::Object(b))
        }
        None => error(502, &format!("unload failed: {}", r.status)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn reply(status: u16, body: Value) -> Reply {
        Reply {
            ok: (200..300).contains(&status),
            status,
            body: Some(body),
        }
    }

    #[test]
    fn refusal_of_is_the_routes_mapping() {
        let gate = reply(
            409,
            json!({"error":{"type":"gate_error","code":"model_in_use","message":"busy","retryable":true}}),
        );
        assert_eq!(
            refusal_of(&gate, "x"),
            Some(Refusal {
                status: 409,
                code: "model_in_use".into(),
                error: "busy".into(),
                retryable: true
            })
        );
        let unknown = reply(
            409,
            json!({"error":{"type":"gate_error","code":"other","message":"m"}}),
        );
        assert_eq!(refusal_of(&unknown, "x"), None);
        let raw = reply(503, json!({"code":"engine_unavailable","error":"raw"}));
        assert!(!refusal_of(&raw, "x").unwrap().retryable);
        assert_eq!(
            refusal_of(&reply(200, json!({"code":"engine_busy","error":"e"})), "x"),
            None
        );
        let empty = reply(
            409,
            json!({"error":{"type":"gate_error","code":"engine_busy"}}),
        );
        assert_eq!(refusal_of(&empty, "fallback").unwrap().error, "fallback");
    }

    #[test]
    fn already_running_reads_the_body_text() {
        assert!(already_running(&reply(
            400,
            json!({"error":{"message":"Model is Already Running"}})
        )));
        assert!(!already_running(&reply(
            409,
            json!({"error":"already running"})
        )));
    }

    #[test]
    fn a_refused_load_answers_as_nodes_failure_then_route() {
        let r = |status, code: Option<&str>| Refused {
            status,
            message: "m".into(),
            code: code.map(str::to_owned),
            retryable: true,
        };
        assert_eq!(
            load_refused(&r(503, Some("inference_queue_full"))).status(),
            503
        );
        assert_eq!(
            load_refused(&r(409, Some("inference_budget"))).status(),
            502
        );
        assert_eq!(load_refused(&r(503, None)).status(), 503);
        assert_eq!(load_refused(&r(400, None)).status(), 502);
    }

    #[test]
    fn owned_is_post_only() {
        assert!(owned(&Method::POST, LOAD));
        assert!(owned(&Method::POST, UNLOAD));
        assert!(!owned(&Method::GET, LOAD));
        assert!(!owned(&Method::POST, "/api/models/delete"));
    }
}
