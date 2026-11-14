//! Track R11 (S9b): `/api/model-manager/*`, core `routes/models.cjs`'s proxy to the model management
//! service, answered by the front (crates/model-manager-proxy, recorded from core) behind
//! `NOEVIA_RUST_MODEL_MANAGER`, which is off by default: unset, every request under the prefix goes
//! to core exactly as before (exact `GET /api/model-manager/models` stays model_manager_scan.rs
//! either way). With `rust`, lib.rs hands every other method and path under the prefix to the models
//! routes and they are answered here; core, told the same switch, refuses them itself.
//!
//! Kept on core under the switch: **`POST /api/model-manager/benchmark/sweep`** (whatever spelling
//! of the path decodes to it). Core holds the throughput sweep's maintenance gate in its memory
//! (benchmark-sweep-guard.cjs) and takes it back after a restart (`adopt`), so the sweep's start is
//! relayed to core after the session gate and, for an administrator on a deployment with a model
//! manager, after the crate's strict payload check (`strict_payload`: `aliases` an array of
//! strings, at most 64 models; its 400 answered here), with the parsed body written again and its
//! length corrected: core and the model manager are sent the value that was checked (core alone
//! would read an unparseable body as `{}` and forward its raw bytes, and would let `[7]` or `[true]`
//! through uncounted for the model manager to read as text). The crate's sweep host steps
//! (`sweep_hold`, `sweep_release`, `sweep_watch`) are never reached through the front.
//!
//! The flow is the crate's [`model_manager_proxy::handle`], run on the blocking pool with each host
//! step run to its end on the runtime (as model_writes_routes.rs does), after the session gate. Its
//! host:
//! - the reads fence: core's `/internal/auto-roles/invalidate` (its `invalidateModelReads`);
//! - the folder-scan cache: core's trusted bridge `/internal/model-manager-scan/{snapshot,publish,clear}`
//!   (the `clear` op is sbstndalton/noevia#2550's; pair the switch with a core that has it);
//! - the system model: models.roles.json beside models.ini and the deprecated name rule
//!   (model_kind.rs, as delete_model.rs asks); the sidecar names: core's facts (`sidecarNames`, as
//!   delete_model.rs asks); the reranker of a sidecar: delete_model.rs's `service_of`;
//! - the served catalogue: the front's installed list (`installed_cached`), which is what core's
//!   `servedCatalogue()` is (its shared `modelsInstalled()`, `null` when unreadable);
//! - the budget checks: core's load refusal over the maintenance bridge, model_maint preset.rs's
//!   `presetRefusal`, the sizing budget of model_state_routes.rs;
//! - a preset write's hold: model_maint `preset_hold` / `reload_held` / `Hold::release`;
//! - the delete clean-up: the router's unload, auto_roles_routes.rs's role clearing and
//!   `forget_model`;
//! - benchmark evidence: model_maint's `record_evidence`, not awaited;
//! - the model manager: reqwest, no redirects, no proxy, the crate's timeout, answers past 8 MiB as
//!   `undefined`, the body as JSON or else its text.
//!
//! Named differences (tests/model_manager_proxy.rs where testable):
//! - **The reranker-settings snapshot.** Core runs an administrator's write inside a
//!   rerank-settings.cjs `withSettings` snapshot (read from this front first; a failed read is its
//!   503 before anything else). The front reads the same effective settings from its store
//!   (reranker_routes.rs `effective_env`) and answers the reranker question from them; core's
//!   sidecar names (its facts) are read once per request, from core's own snapshot of that moment.
//! - **Embedding protection.** Core's `embeddingProtectionUnknown` reads its embedding service's
//!   freshness, which no bridge gives; the front treats protection as unknown exactly when core's
//!   facts give no sidecar names (core's own names fail closed while its status is stale).
//! - **Fail closed** where core's step cannot fail: no sidecar names is core's 503 throw; a load
//!   refusal core's bridge cannot give refuses (409, the budget could not be checked); a scan cache
//!   the bridge cannot read is no cache (a delete then scans fresh); a publish is conditional on the
//!   snapshot it was read with (core's `set` is not).
//! - **Ids that are not strings** (a scanned `modelId` of 7) are not unloaded, cleared or forgotten;
//!   a role clean-up that fails is no roles cleared (as delete_model.rs), not a 500.
//! - **A scan-cache clear core does not confirm** (`{cleared: true}`) fails closed: the forward
//!   after it is not sent and the answer is the 502 of no answer (core's clear cannot fail). A clear
//!   after a delete's clean-up has nothing left to refuse.
//! - A preset refusal that is not a 409 (preset.rs's undecided micro batch, no budget) is answered
//!   as core answers a throw of its status.
//! - The crate's own named differences (lib.rs: control characters, the judged payload forwarded,
//!   Node's method list, a section named `rename`) apply here too.
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::{wtf16, JValue};
use model_manager_proxy::{
    Caps, Config, Fetch, HoldError, Host, Outcome, PresetHoldError, Reply, Thrown, BODY_LIMIT,
    PREFIX,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// The switch.
pub const SWITCH: &str = "NOEVIA_RUST_MODEL_MANAGER";
/// The sweep start core keeps.
pub const SWEEP: &str = "benchmark/sweep";
/// fetchJson's answer cap (http.cjs `DEFAULT_MAX_RESPONSE_BYTES`).
const ANSWER_CAP: usize = 8 * 1024 * 1024;
const LOAD_UNCHECKED: &str = "The load could not be checked against the inference memory budget.";

#[derive(Clone, Default)]
pub struct Settings {
    pub on: bool,
    /// `MODEL_LOADER_URL` as set.
    pub loader_url: Option<String>,
    pub loader_token: Option<String>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("on", &self.on)
            .field("loader_url", &self.loader_url.is_some())
            .field("loader_token", &self.loader_token.is_some())
            .finish()
    }
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Settings {
        Settings {
            on: get(SWITCH).as_deref() == Some("rust"),
            loader_url: get("MODEL_LOADER_URL"),
            loader_token: get("MODEL_LOADER_TOKEN").filter(|t| !t.is_empty()),
        }
    }

    /// Whether the models routes take this request (lib.rs's dispatch and model_routes.rs ask):
    /// any method under the prefix but exact `GET /api/model-manager/models`.
    pub fn answers(&self, method: &Method, path: &str) -> bool {
        self.on
            && path.starts_with(PREFIX)
            && !(method == Method::GET && path == crate::model_manager_scan::PATH)
    }
}

/// `POST benchmark/sweep`, however its path is spelled: core's.
pub fn is_sweep(method: &Method, path: &str) -> bool {
    method == Method::POST
        && path
            .strip_prefix(PREFIX)
            .and_then(model_manager_proxy::manager_path)
            .is_some_and(|p| p == SWEEP)
}

/// `url.search` as WHATWG `URL` gives it (`""` for no query and for a bare `?`).
fn whatwg_search(uri: &axum::http::Uri) -> String {
    let target = uri
        .path_and_query()
        .map_or_else(|| uri.path(), |pq| pq.as_str());
    let Ok(base) = url::Url::parse("http://h") else {
        return String::new();
    };
    match url::Url::options().base_url(Some(&base)).parse(target) {
        Ok(u) => match u.query() {
            Some(q) if !q.is_empty() => format!("?{q}"),
            _ => String::new(),
        },
        Err(_) => String::new(),
    }
}

fn caps(app: &App) -> Caps {
    let llama = app.config.maint.kind == server_stats::Kind::LlamaCpp;
    Caps {
        load_refusal: llama,
        preset_refusal: llama,
        sizing_budget: llama,
        preset_writer: llama,
        reload_presets: false,
        record_evidence: llama,
        unload: llama && app.config.maint.router.is_some(),
        // The sweep is core's (see the module documentation).
        sweep_guard: false,
    }
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let sweep = is_sweep(req.method(), &path);
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
    let role = if id.user.role == server_auth::Role::Admin {
        "admin"
    } else {
        "member"
    };
    if sweep {
        return relay_sweep(app, conn, parts, body, role == "admin").await;
    }
    let method = parts.method.as_str().to_string();
    // readBody's limit, one byte over to tell "too large" apart; only for a method that carries one.
    let bytes = if model_manager_proxy::carries_body(&method) {
        axum::body::to_bytes(body, BODY_LIMIT + 1).await.ok()
    } else {
        Some(axum::body::Bytes::new())
    };
    let within: Option<Vec<u8>> = bytes
        .as_deref()
        .filter(|b| b.len() <= BODY_LIMIT)
        .map(<[u8]>::to_vec);
    let search = whatwg_search(&parts.uri);
    let settings = &app.config.model_manager;
    let cfg = Config {
        loader_url: settings.loader_url.clone(),
        loader_token: settings.loader_token.clone(),
        caps: caps(app),
    };
    let (app2, user, rt) = (
        Arc::clone(app),
        id.user.id.clone(),
        tokio::runtime::Handle::current(),
    );
    let (role, p) = (role.to_string(), path.clone());
    let work = tokio::task::spawn_blocking(move || {
        let mut host = FrontHost {
            app: app2,
            rt,
            user,
            roles: None,
            reranker_env: None,
            sidecars: None,
            scan_token: None,
            hold: None,
            clear_failed: false,
        };
        let request = model_manager_proxy::Request {
            method: &method,
            path: &p,
            search: &search,
            role: &role,
            body: within.as_deref(),
        };
        let out = model_manager_proxy::handle(&request, &cfg, &mut host);
        // Every path of the flow releases the hold; this is the backstop.
        host.preset_release();
        out
    })
    .await;
    match work {
        Ok(Outcome::Answer(a)) => reply::json_text(
            StatusCode::from_u16(a.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            wtf16::stringify(&a.body).unwrap_or_default(),
            true,
        ),
        Ok(Outcome::Core | Outcome::Pass) => {
            let req = Request::from_parts(parts, Body::from(bytes.unwrap_or_default()));
            app.proxy.forward(conn.peer, req).await
        }
        // A panic in the flow (its hold released as the host drops): core's answer to a throw.
        Err(_) => reply::json_text(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"Internal error"}"#.into(),
            true,
        ),
    }
}

/// The sweep start, relayed to core after the session gate. For an administrator on a deployment
/// with a model manager (where core reads the body), the body must pass the crate's strict payload
/// check (`aliases` an array of strings, at most 64 models) and core is sent the parsed value
/// written again, so core and the model manager are sent the value that was checked. Anything else is
/// relayed as it came: core refuses it before it reads the body.
async fn relay_sweep(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    mut parts: axum::http::request::Parts,
    body: Body,
    admin: bool,
) -> Response<Body> {
    let configured = app
        .config
        .model_manager
        .loader_url
        .as_deref()
        .is_some_and(|u| !u.is_empty());
    if !admin || !configured {
        return app
            .proxy
            .forward(conn.peer, Request::from_parts(parts, body))
            .await;
    }
    let answer = |status: u16, body: &JValue| {
        reply::json_text(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            wtf16::stringify(body).unwrap_or_default(),
            true,
        )
    };
    let error = |m: &str| JValue::obj([("error", JValue::from(m))]);
    let bytes = match axum::body::to_bytes(body, BODY_LIMIT + 1).await {
        Ok(b) if b.len() <= BODY_LIMIT => b,
        _ => return answer(413, &error("Request exceeds size limit")),
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let parsed = match model_manager_proxy::strict_payload(Some(&text), Some("aliases")) {
        Ok(v) => v,
        Err(message) => return answer(400, &error(&message)),
    };
    let models = match parsed.get("aliases") {
        JValue::Arr(items) => items
            .iter()
            .filter(|a| a.as_str().is_some_and(|s| !s.is_empty()))
            .count(),
        _ => 0,
    };
    if models > model_manager_proxy::MAX_ALIASES {
        return answer(400, &error("Choose at most 64 models for one sweep."));
    }
    let judged = wtf16::stringify(&parsed).unwrap_or_else(|| "{}".to_string());
    parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
    if let Ok(len) = axum::http::HeaderValue::from_str(&judged.len().to_string()) {
        parts
            .headers
            .insert(axum::http::header::CONTENT_LENGTH, len);
    }
    app.proxy
        .forward(conn.peer, Request::from_parts(parts, Body::from(judged)))
        .await
}

fn of_serde(v: &serde_json::Value) -> JValue {
    wtf16::parse(&v.to_string()).unwrap_or(JValue::Undefined)
}

fn thrown_of(status: u16, body: &str) -> Thrown {
    let parsed = wtf16::parse(body).unwrap_or(JValue::Undefined);
    Thrown {
        status: JValue::from(u32::from(status)),
        message: parsed.get("error").as_str().unwrap_or("").to_string(),
    }
}

/// The proxy's host: each step one async call run to its end on the runtime.
struct FrontHost {
    app: Arc<App>,
    rt: tokio::runtime::Handle,
    user: String,
    roles: Option<HashMap<String, String>>,
    /// The reranker settings' effective environment this request runs on (core `withSettings`).
    reranker_env: Option<HashMap<String, String>>,
    /// Core's sidecar names, read once per request (`None` inside: core did not say).
    sidecars: Option<Option<HashSet<String>>>,
    /// The scan snapshot's token a publish is conditional on.
    scan_token: Option<String>,
    hold: Option<crate::model_maint::Hold>,
    /// A scan-cache clear core did not confirm: the forward that follows it is not sent.
    clear_failed: bool,
}

impl FrontHost {
    fn sidecars(&mut self) -> Option<&HashSet<String>> {
        if self.sidecars.is_none() {
            let app = Arc::clone(&self.app);
            let names = self
                .rt
                .block_on(async move { crate::model_state_routes::sidecar_names(&app).await });
            self.sidecars = Some(names);
        }
        self.sidecars.as_ref().and_then(Option::as_ref)
    }

    async fn call(&self, request: &Fetch) -> Option<Reply> {
        let url = reqwest::Url::parse(&request.url).ok()?;
        // Node fetch refuses userinfo; never silently turn a URL into Basic credentials.
        if !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).ok()?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_millis(request.timeout_ms))
            .build()
            .ok()?;
        let mut req = client.request(method, url);
        for (k, v) in &request.headers {
            let mut value = reqwest::header::HeaderValue::from_str(v).ok()?;
            if k.eq_ignore_ascii_case("x-model-loader-token") {
                value.set_sensitive(true);
            }
            req = req.header(k.as_str(), value);
        }
        if let Some(b) = &request.body {
            req = req.body(b.clone());
        }
        let mut res = req.send().await.ok()?;
        // fetch with `redirect: 'error'` throws on a redirect.
        if res.status().is_redirection() {
            return None;
        }
        let status = res.status().as_u16();
        let ok = res.status().is_success();
        let mut buf = Vec::new();
        while let Some(chunk) = res.chunk().await.ok()? {
            buf.extend_from_slice(&chunk);
            if buf.len() > ANSWER_CAP {
                // http.cjs: `{ ok: false, status, error }`, no body.
                return Some(Reply {
                    ok: false,
                    status,
                    body: JValue::Undefined,
                });
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        let body = wtf16::parse(&text).unwrap_or(JValue::Str(text));
        Some(Reply { ok, status, body })
    }
}

impl Host for FrontHost {
    fn reranker_settings(&mut self) -> Result<(), Thrown> {
        // On the blocking pool already (the flow runs there).
        match crate::reranker_routes::effective_env(&self.app) {
            Ok(env) => {
                self.reranker_env = Some(env);
                Ok(())
            }
            Err(_) => Err(Thrown::new(503, "Reranker settings are unavailable.")),
        }
    }
    fn invalidate_model_reads(&mut self) {
        let app = Arc::clone(&self.app);
        self.rt.block_on(async move {
            let _ = app
                .config
                .research
                .node_call(
                    "/internal/auto-roles/invalidate",
                    &serde_json::json!({}),
                    Duration::from_secs(2),
                )
                .await;
        });
    }
    fn embedding_protection_unknown(&mut self) -> bool {
        self.sidecars().is_none()
    }
    fn is_system_model(&mut self, key: &str) -> bool {
        let presets = self
            .app
            .config
            .sizing
            .presets_path()
            .map(|p| p.to_path_buf());
        let roles = self
            .roles
            .get_or_insert_with(|| crate::model_kind::roles_beside(presets.as_deref()));
        crate::model_kind::is_system_model(roles, key)
    }
    fn is_sidecar_model(&mut self, name: &str) -> Result<bool, Thrown> {
        match self.sidecars() {
            Some(names) => Ok(names.contains(name)),
            None => Err(Thrown::new(
                503,
                "Could not confirm reranker model protection",
            )),
        }
    }
    fn is_reranker_model(&mut self, name: &str) -> Result<bool, Thrown> {
        // rerankModelNames: the deployment's reranker, and the saved settings' one in this
        // request's snapshot.
        if crate::delete_model::service_of(&self.app, name) == "reranker" {
            return Ok(true);
        }
        let Some(env) = self.reranker_env.as_ref() else {
            return Ok(false);
        };
        let on = env
            .get("NOEVIA_FEATURE_RAG_RERANK")
            .map(|v| v.trim().to_ascii_lowercase())
            .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "on"));
        let model = env.get("RERANK_MODEL").map_or("", |m| m.trim());
        Ok(on && !model.is_empty() && model == name)
    }
    fn served_catalogue(&mut self) -> Result<JValue, Thrown> {
        let app = Arc::clone(&self.app);
        let list = self
            .rt
            .block_on(async move { crate::model_state_routes::installed_cached(&app).await });
        Ok(list.map_or(JValue::Null, JValue::Arr))
    }
    fn load_refusal(&mut self, alias: &str) -> JValue {
        use crate::model_maint::node_lease::LoadDecision;
        let maint = Arc::clone(&self.app.config.maint);
        let alias = alias.to_string();
        match self
            .rt
            .block_on(async move { maint.node.load_refusal(&alias).await })
        {
            LoadDecision::Proceed => JValue::Null,
            LoadDecision::Refused(body) => of_serde(&body),
            LoadDecision::Unavailable => JValue::obj([("error", JValue::from(LOAD_UNCHECKED))]),
        }
    }
    fn sweep_hold(&mut self, _reason: &str) -> Result<(), HoldError> {
        // Not reachable: the sweep start is relayed to core before the flow runs.
        Err(HoldError {
            status: JValue::Undefined,
            code: JValue::Undefined,
        })
    }
    fn sweep_release(&mut self) {}
    fn sweep_watch(&mut self, _models: usize) {}
    fn scan_cache_get(&mut self) -> JValue {
        let app = Arc::clone(&self.app);
        let snapshot = self.rt.block_on(async move {
            crate::model_manager_scan::cache(&app, "snapshot", &serde_json::json!({})).await
        });
        let Some(snapshot) = snapshot else {
            return JValue::Undefined;
        };
        self.scan_token = crate::model_manager_scan::opaque(&snapshot, "token").map(str::to_string);
        match snapshot.get("hit") {
            hit @ JValue::Obj(_) => hit.get("body").clone(),
            _ => JValue::Undefined,
        }
    }
    fn scan_cache_set(&mut self, body: &JValue) {
        let (Some(token), Some(json)) = (self.scan_token.clone(), wtf16::stringify(body)) else {
            return;
        };
        let app = Arc::clone(&self.app);
        self.rt.block_on(async move {
            let _ = crate::model_manager_scan::cache(
                &app,
                "publish",
                &serde_json::json!({"token": token, "bodyJson": json}),
            )
            .await;
        });
    }
    fn scan_cache_clear(&mut self) {
        self.scan_token = None;
        let app = Arc::clone(&self.app);
        let cleared = self.rt.block_on(async move {
            crate::model_manager_scan::cache(&app, "clear", &serde_json::json!({})).await
        });
        if cleared.is_none_or(|v| v.get("cleared") != &JValue::Bool(true)) {
            self.clear_failed = true;
        }
    }
    fn preset_refusal(&mut self, name: &str, options: &JValue) -> Result<JValue, Thrown> {
        let section: Vec<(String, String)> = match options {
            JValue::Obj(items) => items
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect(),
            _ => Vec::new(),
        };
        let app = Arc::clone(&self.app);
        let name = name.to_string();
        let answer = self.rt.block_on(async move {
            let maint = Arc::clone(&app.config.maint);
            maint
                .preset_refusal(None, &name, &section, || async {
                    crate::model_state_routes::sizing_budget(&app).await
                })
                .await
        });
        match answer {
            None => Ok(JValue::Null),
            Some(a) if a.status == 409 => Ok(wtf16::parse(&a.body).unwrap_or(JValue::Undefined)),
            Some(a) => Err(thrown_of(a.status, &a.body)),
        }
    }
    fn sizing_budget_gib(&mut self) -> JValue {
        let app = Arc::clone(&self.app);
        self.rt
            .block_on(async move { crate::model_state_routes::sizing_budget(&app).await })
            .map_or(JValue::Null, JValue::Num)
    }
    fn fetch(&mut self, request: &Fetch) -> Option<Reply> {
        // Fail closed: a write whose scan clear core did not confirm is not forwarded (a later
        // delete would be judged by the stale scan); the flow answers its 502.
        if self.clear_failed {
            return None;
        }
        self.rt.block_on(self.call(request))
    }
    fn preset_hold(&mut self) -> Result<(), PresetHoldError> {
        let maint = Arc::clone(&self.app.config.maint);
        match self.rt.block_on(async move { maint.preset_hold().await }) {
            Ok(h) => {
                self.hold = Some(h);
                Ok(())
            }
            Err(o) if o.status == 409 => Err(PresetHoldError::Refused),
            Err(o) => Err(PresetHoldError::Threw(thrown_of(o.status, &o.body))),
        }
    }
    fn preset_reload(&mut self) -> Result<(), Thrown> {
        let Some(hold) = self.hold.as_ref() else {
            return Ok(());
        };
        let maint = Arc::clone(&self.app.config.maint);
        let outcome = self.rt.block_on(maint.reload_held(hold));
        if crate::model_maint::reload::lease_lost(&outcome) {
            return Err(thrown_of(outcome.status, &outcome.body));
        }
        Ok(())
    }
    fn preset_release(&mut self) {
        if let Some(hold) = self.hold.take() {
            self.rt.block_on(hold.release());
        }
    }
    fn reload_presets(&mut self) {
        // Core's reloadPresets without a held writer belongs to a manager that is not llama.cpp,
        // which has no preset file (caps: never asked).
    }
    fn now_ms(&mut self) -> f64 {
        identity::now_ms() as f64
    }
    fn record_evidence(&mut self, model: &JValue, record: &JValue) {
        let Some(model) = model.as_str().map(str::to_string) else {
            return;
        };
        let (maint, record) = (Arc::clone(&self.app.config.maint), record.clone());
        // Not awaited, a failure ignored (core's `.catch(() => undefined)`).
        self.rt.spawn(async move {
            let _ = maint.record_evidence(&model, &record).await;
        });
    }
    fn unload(&mut self, id: &JValue) -> bool {
        let Some(name) = id.as_str().map(str::to_string) else {
            return false;
        };
        let maint = Arc::clone(&self.app.config.maint);
        self.rt.block_on(async move {
            match maint.router.as_ref() {
                Some(router) => router.unload(&name, None).await.is_ok_and(|r| r.ok),
                None => false,
            }
        })
    }
    fn clear_role_references(&mut self, id: &JValue) -> Result<JValue, Thrown> {
        let (Some(name), Some(dir)) = (id.as_str(), self.app.config.data_dir.clone()) else {
            return Ok(JValue::Arr(Vec::new()));
        };
        let (app, user, name) = (Arc::clone(&self.app), self.user.clone(), name.to_string());
        let cleared = self.rt.block_on(async move {
            crate::auto_roles_routes::clear_role_references(&app, &dir, &user, &name)
                .await
                .unwrap_or_default()
        });
        Ok(JValue::Arr(cleared.into_iter().map(JValue::Str).collect()))
    }
    fn forget_model_state(&mut self, id: &JValue) -> Result<(), Thrown> {
        let Some(name) = id.as_str().map(str::to_string) else {
            return Ok(());
        };
        let app = Arc::clone(&self.app);
        self.rt.block_on(async move {
            crate::auto_roles_routes::forget_model(&app, &name).await;
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_covers_the_prefix_but_the_scan_read() {
        let settings = |v: Option<&str>| {
            let v = v.map(str::to_string);
            Settings::from_lookup(&move |k: &str| (k == SWITCH).then(|| v.clone()).flatten())
        };
        for off in [None, Some(""), Some("on"), Some("Rust")] {
            assert!(!settings(off).answers(&Method::POST, "/api/model-manager/x"));
        }
        let on = settings(Some("rust"));
        assert!(on.answers(&Method::POST, "/api/model-manager/benchmark/start"));
        assert!(on.answers(&Method::DELETE, "/api/model-manager/sections/qa"));
        assert!(on.answers(&Method::HEAD, crate::model_manager_scan::PATH));
        assert!(on.answers(&Method::POST, crate::model_manager_scan::PATH));
        assert!(!on.answers(&Method::GET, crate::model_manager_scan::PATH));
        assert!(!on.answers(&Method::GET, "/api/model-manager"));
        assert!(!on.answers(&Method::GET, "/api/model-managerx/a"));
    }

    #[test]
    fn the_sweep_start_is_cores_in_every_spelling() {
        assert!(is_sweep(
            &Method::POST,
            "/api/model-manager/benchmark/sweep"
        ));
        assert!(is_sweep(
            &Method::POST,
            "/api/model-manager/benchmark/sw%65ep"
        ));
        assert!(!is_sweep(
            &Method::GET,
            "/api/model-manager/benchmark/sweep"
        ));
        assert!(!is_sweep(
            &Method::POST,
            "/api/model-manager/benchmark/sweep/x"
        ));
        assert!(!is_sweep(
            &Method::POST,
            "/api/model-manager/benchmark%2Fsweep"
        ));
    }

    #[test]
    fn whatwg_search_is_url_search() {
        let s = |t: &str| whatwg_search(&t.parse().unwrap_or_default());
        assert_eq!(s("/a"), "");
        assert_eq!(s("/a?"), "");
        assert_eq!(s("/a?x=1&b=%20"), "?x=1&b=%20");
    }

    #[test]
    fn the_loader_token_stays_out_of_debug() {
        let s = Settings::from_lookup(&|k: &str| match k {
            "MODEL_LOADER_TOKEN" => Some("qa-secret-token".into()),
            "MODEL_LOADER_URL" => Some("http://qa-loader.invalid".into()),
            _ => None,
        });
        assert!(!format!("{s:?}").contains("qa-secret"));
    }
}
