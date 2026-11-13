//! `POST /api/models/load` and `POST /api/models/unload` (core `routes/models.cjs`, #545, #548, #697, #2115): load or
//! unload one model on the llama.cpp router, through the inference gate and the inference budget, never around them.
//!
//! Load, in Core's order: the body (400), management disabled (404), the system-model and sidecar refusals (409),
//! a preset whose file is gone (409, #545), speculative decoding asked for (400), then the work. The work is: the
//! budget decision (Core's own, `loadRefusal`, over the fence of the memory watchdog; a refusal is a 409 whose text
//! the route does not pass on, so it answers 502 `load failed: 409`), a lease from the gate (the one-model-at-a-time
//! queue; a refusal of the gate is passed on as it is), the router's listing (a sleeping model is left to wake on
//! its next request), the router's load under that lease, and the wait for the model to become ready. The router's
//! own fence ([`crate::model_maint::router::Router`]) stands under every native load, so a model the watchdog
//! unloaded stays refused even when Core does not answer. The lease is given back when the work ends; a client that
//! leaves while the load waits in the queue starts no switch (the handler is not spawned, so its future is dropped).
//!
//! Unload: the router's unload (the gate refuses a model that is answering a request: 409 `model_in_use`, with
//! `canForce` when this process has the gate's admin token), or with `force: true` the forced unload that ends what
//! the model is answering (admin token needed).
//!
//! Differences from Core, on purpose: a body of JSON `null` is `name required` (Core threw a TypeError); a name that is
//! not a string is `name required`. Same as the delete and prune routes, an engine other than llama.cpp is "model
//! management is disabled" here, which includes Core's `mtp` path for those engines (the llama.cpp answer, 400, is
//! native).
//! While a job of this process holds the engine (a reload, a calibration) both verbs answer Core's 503 `Internal
//! error` (the thrown maintenance refusal, as its error handler shows it).
use crate::delete_model::{answer, refusal_of, refuse, Answer};
use crate::model_maint::router::{self, Router};
use crate::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub const LOAD: &str = "/api/models/load";
pub const UNLOAD: &str = "/api/models/unload";

const SYSTEM_MODEL_LOAD_REASON: &str =
    "System routing model runs in its own service — it is not loaded or unloaded here";
const SIDECAR_MODEL_LOAD_REASON: &str = "A running sidecar (embedding or reranking) manages this model — it is not loaded or unloaded here";
const MISSING_MODEL_FILE_REASON: &str =
    "This preset points at a model file that is not in the models folder — it cannot be loaded";
const LOAD_UNCHECKED: &str = "The load could not be checked against the inference memory budget.";
const START_WAIT: Duration = Duration::from_secs(120);
const READY_WAIT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, PartialEq)]
pub enum Verb {
    Load,
    Unload,
}

/// Core's thrown-error answer (`errorResponse`): the text of an internal failure is never shown.
fn internal(status: u16) -> Answer {
    refuse(status, "Internal error")
}

fn failed(verb: Verb, status: u16) -> Answer {
    let what = if verb == Verb::Load { "load" } else { "unload" };
    refuse(502, &format!("{what} failed: {status}"))
}

/// A refusal of the gate (or the engine) as the route answers it, with `extra` pairs after Node's own.
fn refused(status: u16, body: &Option<Value>, extra: &[(&str, Value)]) -> Option<Answer> {
    let (status, code, error, retryable) = refusal_of(status, body)?;
    let mut pairs = vec![
        ("error", json!(error)),
        ("code", json!(code)),
        ("retryable", json!(retryable)),
    ];
    pairs.extend(extra.iter().map(|(k, v)| (*k, v.clone())));
    Some(answer(status, &pairs))
}

/// `alreadyRunning`: the router says it already runs the model.
fn already_running(status: u16, body: &Option<Value>) -> bool {
    status == 400
        && body.as_ref().is_some_and(|b| {
            b.to_string()
                .to_ascii_lowercase()
                .contains("already running")
        })
}

pub async fn run(app: &Arc<App>, verb: Verb, raw: &str) -> Answer {
    let Ok(body) = serde_json::from_str::<Value>(raw) else {
        return refuse(400, "invalid JSON");
    };
    let Some(name) = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
    else {
        return refuse(400, "name required");
    };
    let maint = Arc::clone(&app.config.maint);
    let (true, Some(router)) = (
        app.config.sizing.kind() == server_stats::Kind::LlamaCpp,
        maint.router.as_ref(),
    ) else {
        return refuse(404, "model management is disabled");
    };
    // #548: Laya and the in-use embedding/reranker models are run by their own services.
    let roles = crate::model_kind::roles_beside(app.config.sizing.presets_path());
    if crate::model_kind::is_system_model(&roles, name) {
        return refuse(409, SYSTEM_MODEL_LOAD_REASON);
    }
    // The sidecar names are Core's; when it cannot say, nothing is touched.
    match crate::model_state_routes::sidecar_names(app).await {
        Some(sidecars) if !sidecars.contains(name.trim()) => {}
        _ => return refuse(409, SIDECAR_MODEL_LOAD_REASON),
    }
    if verb == Verb::Load {
        // #545: a preset whose file is gone can only fail to load; an unreadable list skips the check.
        if let Ok(list) = crate::model_state_routes::installed_cached(app).await {
            if list
                .iter()
                .find(|m| m.get("name").as_str() == Some(name))
                .is_some_and(|m| m.get("missingFile").as_bool() == Some(true))
            {
                return refuse(409, MISSING_MODEL_FILE_REASON);
            }
        }
        if body.get("mtp").is_some() {
            return refuse(
                400,
                "Use the native preset editor to configure speculative decoding.",
            );
        }
    }
    // `maintenance.enter()`: a job of this process holds the engine.
    if maint.local_held() {
        return internal(503);
    }
    if verb == Verb::Unload {
        return unload(
            app,
            router,
            name,
            body.get("force") == Some(&Value::Bool(true)),
        )
        .await;
    }
    load(app, router, name).await
}

async fn unload(app: &Arc<App>, router: &Router, name: &str, force: bool) -> Answer {
    let gate = app.config.maint.gate.as_ref();
    let has_admin = gate.is_some_and(|g| g.has_admin());
    let model = json!({ "model": name });
    if force {
        // The forced unload ends a request in progress; it needs the gate's admin token.
        let Some((header, value)) = gate.and_then(|g| g.admin_header()) else {
            return refused(
                403,
                &Some(json!({"error": "Unloading a model in use is not set up on this server.", "code": "admin_required"})),
                &[],
            )
            .unwrap_or_else(|| failed(Verb::Unload, 403));
        };
        let forced = json!({ "model": name, "force": true });
        return match router
            .request_with(
                reqwest::Method::POST,
                "/models/unload",
                Some(&forced),
                None,
                &[(header, value)],
                Duration::from_secs(60),
            )
            .await
        {
            Ok(r) if r.ok => answer(200, &[("ok", json!(true)), ("forced", json!(true))]),
            Ok(r) => refused(r.status, &r.body, &[])
                .unwrap_or_else(|| refuse(502, &format!("unload failed: {}", r.status))),
            Err(_) => internal(500),
        };
    }
    match router
        .request(
            reqwest::Method::POST,
            "/models/unload",
            Some(&model),
            None,
            START_WAIT,
        )
        .await
    {
        Ok(r) if r.ok => answer(200, &[("ok", json!(true))]),
        Ok(r) => {
            let can_force = json!(has_admin);
            let extra: Vec<(&str, Value)> = match refusal_of(r.status, &r.body) {
                Some((_, code, _, _)) if code == "model_in_use" => vec![("canForce", can_force)],
                _ => Vec::new(),
            };
            refused(r.status, &r.body, &extra).unwrap_or_else(|| failed(Verb::Unload, r.status))
        }
        Err(_) => internal(500),
    }
}

async fn load(app: &Arc<App>, router: &Router, name: &str) -> Answer {
    let maint = Arc::clone(&app.config.maint);
    // #697: the memory budget first, before anything is switched or queued (Core's own decision; with no answer from
    // it nothing loads unchecked).
    match maint.node.load_refusal(name).await {
        crate::model_maint::node_lease::LoadDecision::Proceed => {}
        crate::model_maint::node_lease::LoadDecision::Refused(_) => return failed(Verb::Load, 409),
        crate::model_maint::node_lease::LoadDecision::Unavailable => {
            return refuse(503, LOAD_UNCHECKED)
        }
    }
    // noevia#2115: the gate's queue; leaving it (this future dropped) starts no switch.
    let lease = match maint.gate.as_ref() {
        Some(gate) => match gate.lease(name).await {
            Ok(lease) => lease,
            Err(f) => {
                let mut pairs = vec![("error", json!(f.message)), ("code", json!(f.code))];
                if f.retryable {
                    pairs.push(("retryable", json!(true)));
                }
                let body: serde_json::Map<String, Value> =
                    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
                return refused(f.status, &Some(Value::Object(body)), &[])
                    .unwrap_or_else(|| failed(Verb::Load, f.status));
            }
        },
        None => None,
    };
    let answer = loaded(router, name, lease.as_ref()).await;
    if let Some(lease) = lease {
        lease.release().await;
    }
    answer
}

async fn loaded(
    router: &Router,
    name: &str,
    lease: Option<&crate::model_maint::gate::Lease>,
) -> Answer {
    let held: Vec<_> = lease.map(|l| l.header()).into_iter().collect();
    let before = match router.models(None).await {
        Ok(r) => r,
        Err(_) => return internal(500),
    };
    // #1758: never ask the router to load a sleeping model; the next request wakes it.
    let row_status = |rows: &Vec<Value>| {
        rows.iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some(name))
            .and_then(|m| m.get("status"))
            .cloned()
    };
    if before.ok {
        if let Some(rows) = router::rows(&before.body) {
            if row_status(rows)
                .and_then(|s| s.get("value").and_then(Value::as_str).map(str::to_owned))
                .as_deref()
                == Some("sleeping")
            {
                return answer(200, &[("ok", json!(true))]);
            }
        }
    }
    let started = match router
        .request_with(
            reqwest::Method::POST,
            "/models/load",
            Some(&json!({ "model": name })),
            None,
            &held,
            START_WAIT,
        )
        .await
    {
        Ok(r) => r,
        Err(_) => return internal(500),
    };
    // Woke or slept between the listing and the load: the router already runs it.
    if !started.ok && !already_running(started.status, &started.body) {
        return refused(started.status, &started.body, &[])
            .unwrap_or_else(|| failed(Verb::Load, started.status));
    }
    // Native load acknowledges launch; readiness is observed here.
    let deadline = tokio::time::Instant::now() + READY_WAIT;
    while tokio::time::Instant::now() < deadline {
        let listing = match router.models(None).await {
            Ok(l) => l,
            Err(_) => return internal(500),
        };
        if !listing.ok {
            return failed(Verb::Load, listing.status);
        }
        let entry = router::rows(&listing.body).and_then(|rows| {
            rows.iter()
                .find(|m| m.get("id").and_then(Value::as_str) == Some(name))
        });
        match entry {
            Some(e) => {
                let status = e.get("status");
                let value = status.and_then(|s| s.get("value")).and_then(Value::as_str);
                if matches!(value, Some("loaded" | "sleeping")) {
                    return answer(200, &[("ok", json!(true))]);
                }
                let failed_flag = status
                    .and_then(|s| s.get("failed"))
                    .is_some_and(|f| !matches!(f, Value::Null | Value::Bool(false)));
                if failed_flag || value == Some("unloaded") {
                    return failed(Verb::Load, 502);
                }
            }
            None => return failed(Verb::Load, 502),
        }
        tokio::time::sleep(POLL).await;
    }
    failed(Verb::Load, 504)
}
