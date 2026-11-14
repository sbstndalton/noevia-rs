//! The native auto-tune runner in this front (sbstndalton/noevia#2516, R6), behind
//! `AUTOTUNE_RUNNER_IMPL=rust` (off by default; core rust-autotune-runner.cjs reads the same
//! variable and then builds no auto-tuner of its own).
//!
//! With the switch on, the actor (`crates/autotune-runner`) owns the job and
//! `native-autotune-<hash>.json`, and answers what the auto-tune routes used to ask Node's live
//! actor through `/internal/autotune-*` bridges: [`call`] gives the same status and JSON body the
//! Node bridge gave, so each route's own policy is unchanged. `POST /api/models/autotune` (the
//! start) is answered here too. With the switch off, [`call`] is the Node bridge as before and the
//! start is forwarded to Node.

use crate::model_maint::autotune_deps::{front_owns_runner, FrontDeps, ReadFacts};
use crate::model_maint::Maint;
use autotune_runner::{Config as RunnerConfig, Runner, StartBody};
use js_json::{wtf16, JValue};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The reason a recovery's hold shows (Node: `requireTrialRecovery`'s message).
const RECOVERY_REASON: &str =
    "Chat is paused while noevia restores the settings an interrupted tune left behind.";
/// The longest a recovery may hold the engine (unload, restore, re-read).
const RECOVERY_LEASE: Duration = Duration::from_secs(15 * 60);
/// Between attempts of a recovery that did not succeed (inference-maintenance.cjs retries).
const RECOVERY_RETRY: Duration = Duration::from_secs(30);

/// The native actor and what its boot needs.
pub struct Native {
    pub runner: Runner<FrontDeps>,
    maint: Arc<Maint>,
    recoveries: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>>,
}

impl std::fmt::Debug for Native {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Native").finish_non_exhaustive()
    }
}

/// The actor, when the switch is on and the front maintains a llama.cpp router with a models.ini
/// (`None` otherwise, or when the state file holds JSON that is not an object: Node then builds no
/// auto-tuner either).
pub fn build(
    maint: &Arc<Maint>,
    research: &Arc<crate::research_routes::Research>,
    get: &impl Fn(&str) -> Option<String>,
) -> Option<Arc<Native>> {
    if !front_owns_runner(get)
        || maint.kind != server_stats::Kind::LlamaCpp
        || maint.ini.is_none()
        || maint.router.is_none()
    {
        return None;
    }
    let dir = maint.data_dir.clone()?;
    let state_file = dir.join(format!("native-autotune-{}.json", maint.suffix));
    let text = std::fs::read_to_string(&state_file).ok();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let research = Arc::clone(research);
    let read_facts: ReadFacts = Arc::new(move || {
        let research = Arc::clone(&research);
        Box::pin(async move {
            let budget = crate::model_state_routes::sizing_budget_via(&research)
                .await
                .unwrap_or(0.0);
            let sidecars = crate::model_state_routes::sidecar_names_via(&research).await;
            (budget, sidecars)
        })
    });
    let deps = FrontDeps::new(Arc::clone(maint), state_file, read_facts, tx, get);
    let runner = Runner::new(deps, RunnerConfig::default(), text.as_deref()).ok()?;
    Some(Arc::new(Native {
        runner,
        maint: Arc::clone(maint),
        recoveries: Mutex::new(Some(rx)),
    }))
}

impl Native {
    /// Boot: the recovery of a run a restart interrupted (Core's boot `recover()`, which Core skips
    /// when the front owns the runner), and the worker that runs every repair the runner asks the
    /// gate for (`requireRecovery`) under a fresh hold.
    pub fn spawn_boot(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = me.runner.recover().await {
                eprintln!("[autotune] boot recovery failed: {}", e.message);
            }
        });
        let Some(mut rx) = self.recoveries.lock().ok().and_then(|mut r| r.take()) else {
            return;
        };
        let me = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(key) = rx.recv().await {
                me.repair(&key).await;
            }
        });
    }

    /// One owed repair, as calibration's recovery after a lost hold runs (calibration_worker.rs
    /// `recover_after_loss`): under a fresh recovery hold, every model confirmed unloaded (Node's
    /// settle step), `recoverTrial`, then the recovery proven to Node and completed at the authority;
    /// otherwise the recovery stays required and is retried with backoff. Chat stays fenced
    /// throughout (`FrontDeps::require_recovery`).
    async fn repair(&self, key: &str) {
        let mut wait = Duration::from_secs(1);
        loop {
            self.runner.deps().fence_owed(key);
            if let Ok(hold) = self
                .maint
                .acquire_recovery(RECOVERY_REASON, RECOVERY_LEASE)
                .await
            {
                let ticket = self.maint.authority.recovery_ticket().unwrap_or_default();
                let hold = Arc::new(hold);
                self.runner.deps().cover_with(Some(Arc::clone(&hold)));
                let mut done = self.settle(&hold).await;
                if done {
                    // An `apply:<revision>` key is a write's own restore (applyUnlocked's loss
                    // branch); any other is the run's trial recovery.
                    match self.runner.deps().restore_apply(key).await {
                        Some(true) => {}
                        Some(false) => {
                            eprintln!(
                                "[autotune] recovery {key} failed: the restore did not complete"
                            );
                            done = false;
                        }
                        None => {
                            if let Err(e) = self.runner.recover_trial().await {
                                eprintln!("[autotune] recovery {key} failed: {}", e.message);
                                done = false;
                            }
                        }
                    }
                }
                self.runner.deps().cover_with(None);
                if done {
                    done = hold.excluded()
                        && (hold.gated_now()
                            || match hold.recovery_proof() {
                                Some((id, generation)) => {
                                    self.maint
                                        .node
                                        .complete_recovery(&ticket, id, generation)
                                        .await
                                }
                                None => false,
                            })
                        && hold.excluded();
                    if done {
                        done = self.maint.authority.complete_recovery(&ticket);
                    }
                }
                if !done {
                    self.maint.authority.require_recovery();
                    if !hold.gated_now() {
                        let _ = self.maint.node.recovery_barrier(true, &ticket).await;
                    }
                }
                if let Ok(hold) = Arc::try_unwrap(hold) {
                    hold.release().await;
                }
                if done {
                    self.runner.deps().settle_owed(key);
                    return;
                }
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(RECOVERY_RETRY);
        }
    }

    /// The recovery's settle step: every router model asked to unload and confirmed unloaded.
    async fn settle(&self, hold: &crate::model_maint::Hold) -> bool {
        let Some(router) = self.maint.router.as_ref() else {
            return false;
        };
        let token = hold.token();
        let rows = |r: &crate::model_maint::router::Reply| {
            crate::model_maint::router::rows(&r.body)
                .cloned()
                .unwrap_or_default()
        };
        let state = |row: &Value| {
            row.get("status")
                .and_then(|s| s.get("value"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let Ok(first) = router.models(token.as_deref()).await else {
            return false;
        };
        for row in rows(&first) {
            if matches!(
                state(&row).as_deref(),
                Some("loaded" | "loading" | "sleeping")
            ) {
                if let Some(id) = row.get("id").and_then(Value::as_str) {
                    let _ = router.unload(id, token.as_deref()).await;
                }
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while tokio::time::Instant::now() < deadline {
            if let Ok(r) = router.models(token.as_deref()).await {
                if r.ok
                    && rows(&r)
                        .iter()
                        .all(|row| state(row).as_deref() == Some("unloaded"))
                {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        false
    }

    /// `POST /api/models/autotune`: the route's coercion (routes/models.cjs), then `start`.
    pub async fn start(&self, body: &JValue) -> (u16, JValue) {
        // The manager's start wrapper: a budget fault refuses before anything is held.
        let budget = self.runner.deps().inference_budget();
        if let Some(fault) = self
            .maint
            .sizing
            .budget_fault()
            .filter(|_| self.maint.sizing.calibration_budget_gib(budget) <= 0.0)
        {
            return (
                503,
                JValue::obj([
                    (
                        "error",
                        JValue::from(format!("Auto-tune unavailable: {fault}")),
                    ),
                    ("code", JValue::from("autoconfig_impl")),
                ]),
            );
        }
        self.runner.deps().refresh().await;
        let present =
            |k: &str| (!matches!(body.get(k), JValue::Undefined)).then(|| body.get(k).clone());
        let model = {
            let m = body.get("model");
            if m.truthy() {
                js_json::to_js_string(m).unwrap_or_default()
            } else {
                String::new()
            }
        };
        let start = StartBody {
            confirm_pause: present("confirmPause"),
            prompt_budget_seconds: present("promptBudgetSeconds"),
            untuned: body.get("untuned") == &JValue::Bool(true),
            mode: present("mode"),
        };
        match self.runner.start(&model, start).await {
            Ok(answer) => answer,
            Err(e) => {
                eprintln!("[autotune] start failed: {}", e.message);
                (
                    500,
                    JValue::obj([("error", JValue::from("Internal error"))]),
                )
            }
        }
    }

    /// The Node bridge `path`'s answer from the native actor.
    async fn bridge(&self, path: &str, ask: &JValue) -> (u16, JValue) {
        let r = &self.runner;
        let token = || ask.get("token").as_str().unwrap_or("").to_owned();
        let with_available = |facts: JValue| {
            let mut out = JValue::obj([("available", JValue::Bool(true))]);
            if let JValue::Obj(items) = &mut out {
                for (k, v) in autotune_runner::js::entries(&facts).into_iter().flatten() {
                    wtf16::set(items, k, v.clone());
                }
            }
            out
        };
        let applied = |answer: JValue| {
            let status = if answer.get("applied") == &JValue::Bool(false) {
                409
            } else {
                200
            };
            (status, answer)
        };
        let unavailable = || {
            (
                503,
                JValue::obj([("error", JValue::from("Auto-tune state is unavailable."))]),
            )
        };
        match path {
            "/internal/autotune-read/facts" => {
                let keys: Vec<String> = autotune_runner::js::items(ask.get("keys"))
                    .iter()
                    .filter_map(|k| k.as_str().map(str::to_owned))
                    .collect();
                match r.status_facts(&keys) {
                    Ok(f) => (200, with_available(f)),
                    Err(_) => unavailable(),
                }
            }
            "/internal/autotune-cancel/facts" => (200, with_available(r.cancellation_facts())),
            "/internal/autotune-cancel/abort" => applied(r.compare_and_abort(&token())),
            "/internal/autotune-clear/facts" => (200, with_available(r.clear_facts())),
            "/internal/autotune-clear/clear" => match r.compare_and_clear(&token()) {
                Ok(a) => applied(a),
                Err(_) => unavailable(),
            },
            "/internal/autotune-resume/facts" => (200, with_available(r.resume_facts())),
            "/internal/autotune-resume/prepare" => {
                match r
                    .prepare_resume(
                        &token(),
                        ask.get("stage").as_str().unwrap_or(""),
                        ask.get("selectors"),
                    )
                    .await
                {
                    Ok(a) => applied(a),
                    Err(_) => unavailable(),
                }
            }
            "/internal/autotune-resume/commit" => {
                match r.compare_and_resume(&token(), ask.get("plan")) {
                    Ok(a) => applied(a),
                    Err(_) => unavailable(),
                }
            }
            "/internal/autotune-settings/facts" => {
                let mut out = JValue::obj([
                    ("available", JValue::Bool(true)),
                    ("settingsAvailable", JValue::Bool(true)),
                ]);
                if let Some(model) = ask.get("model").as_str() {
                    if let JValue::Obj(items) = &mut out {
                        wtf16::set(items, "facts", r.settings_facts(model));
                    }
                }
                (200, out)
            }
            "/internal/autotune-settings/commit" => {
                let model = ask.get("model").as_str().unwrap_or("");
                match r.commit_settings(
                    model,
                    ask.get("allowQ5Kv") == &JValue::Bool(true),
                    &token(),
                ) {
                    Ok(a) => applied(a),
                    Err(_) => unavailable(),
                }
            }
            "/internal/autotune-untuned/facts" => {
                if ask.get("scan") != &JValue::Bool(true) {
                    return (200, JValue::obj([("available", JValue::Bool(true))]));
                }
                let scan = async {
                    r.deps().refresh().await;
                    r.scan_facts_public().await
                };
                match tokio::time::timeout(Duration::from_millis(4500), scan).await {
                    Ok(Ok(facts)) => (200, with_available(facts)),
                    Ok(Err(e)) => {
                        let failure = match e.public_message.as_deref() {
                            Some("The model server is not responding.") => "router",
                            Some("Model identity could not be read.") => "identity",
                            _ => "unavailable",
                        };
                        (503, JValue::obj([("failure", JValue::from(failure))]))
                    }
                    Err(_) => (503, JValue::obj([("failure", JValue::from("unavailable"))])),
                }
            }
            _ => (404, JValue::obj([("error", JValue::from("Not found"))])),
        }
    }
}

/// What an auto-tune route asks its owner: the native actor when it runs here, else the Node
/// bridge (`research.node_call`), with the same `(status, JSON bytes)`.
pub async fn call(
    app: &crate::App,
    path: &str,
    body: &Value,
    timeout: Duration,
) -> Option<(u16, Vec<u8>)> {
    let Some(native) = app.config.autotune.as_ref() else {
        return app.config.research.node_call(path, body, timeout).await;
    };
    let ask = wtf16::parse(&body.to_string()).unwrap_or(JValue::Null);
    let (status, answer) = native.bridge(path, &ask).await;
    // Node's resume bridge cleared Core's cached folder scan after a commit that applied (#2550).
    if path == "/internal/autotune-resume/commit" && answer.get("applied") == &JValue::Bool(true) {
        crate::model_manager_scan::clear(app).await;
    }
    Some((
        status,
        wtf16::stringify(&answer).unwrap_or_default().into_bytes(),
    ))
}

/// `POST /v1/autotune/{flush,state}` on the project-store listener, for Core when the front owns
/// the runner (core rust-autotune-runner.cjs): `flush` is the runner's `flushReload(model)` (Core's
/// `prepareLoad` asks it before a chat model loads, #1068), `state` whether a run is active (Core's
/// `tuningActive`, for the preset writer and the code tasks). `None`: both answer 503.
pub fn core_routes(
    store: crate::project_store::Store,
    native: Option<Arc<Native>>,
) -> axum::Router {
    use axum::routing::post;
    let mut router = axum::Router::new();
    for op in ["flush", "state"] {
        let (store, native) = (store.clone(), native.clone());
        router = router.route(
            &format!("/v1/autotune/{op}"),
            post(
                move |headers: axum::http::HeaderMap, raw: axum::body::Bytes| {
                    let (store, native) = (store.clone(), native.clone());
                    async move { core_call(store, native, headers, raw, op).await }
                },
            ),
        );
    }
    router.layer(axum::extract::DefaultBodyLimit::max(4096))
}

async fn core_call(
    store: crate::project_store::Store,
    native: Option<Arc<Native>>,
    headers: axum::http::HeaderMap,
    raw: axum::body::Bytes,
    op: &'static str,
) -> axum::response::Response {
    use axum::http::StatusCode;
    let answer = |status: StatusCode, v: JValue| {
        crate::reply::json_text(status, wtf16::stringify(&v).unwrap_or_default(), true)
    };
    if !crate::project_store::authorised(&store, &headers) {
        return answer(
            StatusCode::UNAUTHORIZED,
            JValue::obj([("error", JValue::from("unauthorised"))]),
        );
    }
    let Some(native) = native else {
        // No runner here: nothing can be owed and nothing runs (Node's flush is then a no-op).
        let ok = std::str::from_utf8(&raw)
            .ok()
            .and_then(|t| wtf16::parse(t).ok())
            .is_some_and(|v| v.is_object());
        if !ok {
            return answer(
                StatusCode::BAD_REQUEST,
                JValue::obj([("error", JValue::from("invalid JSON"))]),
            );
        }
        return answer(
            StatusCode::OK,
            if op == "state" {
                JValue::obj([("running", JValue::Bool(false))])
            } else {
                JValue::obj([("flushed", JValue::Bool(true))])
            },
        );
    };
    let ask = std::str::from_utf8(&raw)
        .ok()
        .and_then(|t| wtf16::parse(t).ok());
    let Some(ask) = ask.filter(JValue::is_object) else {
        return answer(
            StatusCode::BAD_REQUEST,
            JValue::obj([("error", JValue::from("invalid JSON"))]),
        );
    };
    if op == "state" {
        let job = native.runner.job_record();
        let running = native.runner.starting() || job.get("status").as_str() == Some("running");
        return answer(
            StatusCode::OK,
            JValue::obj([("running", JValue::Bool(running))]),
        );
    }
    let model = ask.get("model").as_str().filter(|m| !m.is_empty());
    match native.runner.flush_reload_public(model).await {
        Ok(()) => answer(StatusCode::OK, JValue::obj([("flushed", JValue::Bool(true))])),
        Err(e) => answer(
            StatusCode::SERVICE_UNAVAILABLE,
            JValue::obj([(
                "error",
                JValue::from(e.public_message.unwrap_or_else(|| "The model server has not re-read the restored settings yet. Try again shortly.".to_owned())),
            )]),
        ),
    }
}
