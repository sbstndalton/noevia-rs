//! Code mode's task actions in the front (Rust port track R7), behind `NOEVIA_CODE_EXECUTOR=rust`,
//! which is off by default: unset, these requests go to Node exactly as before.
//!
//! `POST /api/projects/{id}/code` (start, 202), `POST .../code/{task}/approve` and
//! `POST .../code/{task}/cancel`, answer for answer with core routes/code.cjs, over
//! crates/code-harness's service (code-service.cjs), executor (code-harness.cjs), job store
//! (server-projects task_store, jobs.cjs) and ACP client (code-acp.cjs); the harness itself stays
//! the sandbox's program. Core, told the same switch (rust-code-executor.cjs), refuses these
//! requests, recovers no code journal (the front's live tasks would read as interrupted), answers
//! the task facts bridge with the flag only, and says on `/internal/code/engine` what one start
//! needs from it: the engine and model, whether the Code role is on an inference machine, whether a
//! tune is active, and the project's shared context.
//!
//! The order is core's: the flag (404), the administrator (403), the project (404), the method (405),
//! the body (400/413), then the service's own answers.
//!
//! Named differences (each refused, never run differently):
//! - the Code role on an inference machine (core's relay, #1853) is refused with 409;
//! - the Planner preparation is not offered and no Planner review runs (`codePipeline`,
//!   `plannerReview` stay Node's): the switch stays off where either flag is on;
//! - a Cowork session's task start from chat stays Node's, which refuses it while the front owns Code.
//!
//! The lease: a task holds the inference admission authority's maintenance-only lease (the one
//! Node's latch takes as `modelManager.enterInference()`, as Node's own Code tasks hold) for its run.
use crate::code_egress::Issuer;
use crate::task_routes::{self, Facts};
use crate::{identity::Rejection, reply, App};
use axum::{
    body::Body,
    http::{Response, StatusCode},
    response::IntoResponse,
};
use code_harness::executor::{Egress, Engine, Fail, Lease};
use code_harness::service::{
    default_harnesses, parse_repos, parse_user, Inference, Options, Project, Resolved, Service,
    APPROVAL_TIMEOUT,
};
use engine_admission::{Admission, Ask};
use js_json::{to_js_string, wtf16, JValue};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The switch: `rust` hands Code tasks to the front.
pub const SWITCH: &str = "NOEVIA_CODE_EXECUTOR";
const ENGINE_PATH: &str = "/internal/code/engine";
const ENGINE_DEADLINE: Duration = Duration::from_secs(5);
const UNAVAILABLE: &str = "Code tasks are unavailable right now. Try again shortly.";
const ON_A_MACHINE: &str = "The Code role runs on an inference machine, which this server's native Code executor cannot reach yet. Move the Code role back to this machine, or ask an administrator.";
/// core `readJson`'s limit.
const BODY_LIMIT: usize = 1024 * 1024;

/// The deployment's Code configuration, as core index.cjs and code-acp.cjs read it.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub executor: bool,
    pub repos: String,
    pub harness_name: Option<String>,
    pub harness_version: Option<String>,
    pub endpoint: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub tree_root: Option<String>,
    pub harness_user: Option<String>,
    pub path: Option<String>,
    pub lang: Option<String>,
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Settings {
        let set = |k: &str| get(k).filter(|v| !v.is_empty());
        Settings {
            executor: get(SWITCH).as_deref() == Some("rust"),
            repos: get("CODE_REPOS").unwrap_or_default(),
            harness_name: get("CODE_HARNESS_NAME"),
            harness_version: get("CODE_HARNESS_VERSION"),
            endpoint: set("CODE_HARNESS_ENDPOINT"),
            command: set("CODE_HARNESS_COMMAND"),
            args: get("CODE_HARNESS_ARGS")
                .unwrap_or_default()
                .split(' ')
                .filter(|a| !a.is_empty())
                .map(str::to_string)
                .collect(),
            tree_root: set("CODE_WORKSPACE_ROOT"),
            harness_user: get("CODE_HARNESS_USER"),
            path: get("PATH"),
            lang: get("LANG"),
        }
    }

    /// The sandbox wins when both are set (code-acp.cjs `createAcpTransport`).
    fn transport(&self) -> Option<code_acp::Transport> {
        if let Some(e) = &self.endpoint {
            return Some(code_acp::Transport::Socket(e.clone()));
        }
        self.command.as_ref().map(|c| code_acp::Transport::Spawn {
            command: c.clone(),
            args: self.args.clone(),
        })
    }
}

/// One Code action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Start(String),
    /// project, task
    Decide(String, String),
    /// project, task
    Cancel(String, String),
    /// core's 405, after the flag, the administrator and the project.
    NotAllowed(String),
}

impl Action {
    fn project(&self) -> &str {
        match self {
            Action::Start(p)
            | Action::Decide(p, _)
            | Action::Cancel(p, _)
            | Action::NotAllowed(p) => p,
        }
    }
}

/// core's `PATTERN`, `^\/api\/projects\/([^/]+)\/code(?:\/([0-9a-f-]{36}))?(?:\/(approve|cancel))?$`,
/// less the reads the front already answers (task_routes.rs): `None` is not a Code action.
pub fn parse(method: &str, path: &str) -> Option<Action> {
    let rest = path.strip_prefix("/api/projects/")?;
    let (project, tail) = rest.split_once('/')?;
    if project.is_empty() {
        return None;
    }
    let mut tail = tail.strip_prefix("code")?;
    let mut task = None;
    if let Some(after) = tail.strip_prefix('/') {
        let id = after.get(..36).filter(|id| {
            id.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
        });
        if let Some(id) = id {
            task = Some(id.to_string());
            tail = after.get(36..).unwrap_or("");
        }
    }
    let approve = match tail {
        "" => None,
        "/approve" => Some(true),
        "/cancel" => Some(false),
        _ => return None,
    };
    let p = project.to_string();
    let post = method == "POST";
    Some(match (task, approve) {
        // `!taskId`: a POST starts a task (`.../code/approve` too, as core's pattern matches it).
        (None, _) if post => Action::Start(p),
        (None, None) | (Some(_), None) if method == "GET" => return None,
        (Some(t), Some(true)) if post => Action::Decide(p, t),
        (Some(t), Some(false)) if post => Action::Cancel(p, t),
        _ => Action::NotAllowed(p),
    })
}

/// routes/code.cjs's answer for a failure: `status || 500`, the public words, else the message
/// of an error that has a status, else "Code request failed".
fn refused(e: &Fail) -> Response<Body> {
    let message = match (&e.public_message, e.status) {
        (Some(m), _) if !m.is_empty() => m.clone(),
        (_, Some(_)) => e.message.clone(),
        (_, None) => "Code request failed".to_string(),
    };
    let status = e
        .status
        .and_then(|s| StatusCode::from_u16(s).ok())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    answer(status, &JValue::obj([("error", JValue::Str(message))]))
}

fn answer(status: StatusCode, body: &JValue) -> Response<Body> {
    reply::json_text(
        status,
        wtf16::stringify(body).unwrap_or_else(|| "null".into()),
        true,
    )
}

fn failed() -> Response<Body> {
    reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Code request failed", true)
}

/// core `readJson`: at most 1 MiB, an empty body is `{}`.
async fn read_json(body: Body) -> Result<JValue, Response<Body>> {
    let read = crate::account_routes::read_body(body, BODY_LIMIT)
        .await
        .map_err(|()| reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true))?;
    if read.over {
        return Err(reply::error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request exceeds size limit",
            true,
        ));
    }
    if read.bytes.is_empty() {
        return Ok(JValue::Obj(Vec::new()));
    }
    wtf16::parse(&String::from_utf8_lossy(&read.bytes))
        .map_err(|_| reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true))
}

/// `String(x || '')`.
fn text_of(v: &JValue) -> String {
    if v.truthy() {
        to_js_string(v).unwrap_or_default()
    } else {
        String::new()
    }
}

/// The egress controller, over the front's own issuer (code_egress.rs), as core's
/// code-egress-client.cjs answers it: a revoke that is not confirmed and final is the uncertain report.
struct FrontEgress {
    issuer: Arc<Issuer>,
    rt: tokio::runtime::Handle,
}

/// code-egress-client.cjs `EgressUnavailableError`.
fn egress_down() -> Fail {
    Fail::new(Some(503), "native egress is unavailable")
}

impl Egress for FrontEgress {
    fn endpoint(&self) -> Option<String> {
        self.issuer
            .status()
            .get("endpoint")
            .and_then(|e| e.as_str())
            .map(str::to_string)
    }
    fn available(&self) -> bool {
        self.issuer.status().get("available") == Some(&serde_json::Value::Bool(true))
    }
    fn grant(&self, task_id: &str, domains: &[String]) -> Result<JValue, Fail> {
        let handle = self
            .rt
            .block_on(self.issuer.grant(task_id, domains))
            .map_err(|_| egress_down())?;
        Ok(JValue::obj([
            ("handle", JValue::Str(handle.id)),
            ("taskId", JValue::Str(handle.task_id)),
            (
                "domains",
                JValue::Arr(handle.domains.into_iter().map(JValue::Str).collect()),
            ),
            ("token", JValue::Str(handle.token)),
        ]))
    }
    fn revoke(&self, grant: &JValue) -> Result<JValue, Fail> {
        let uncertain = || {
            JValue::obj([
                ("confirmed", JValue::Bool(false)),
                (
                    "report",
                    JValue::obj([
                        ("available", JValue::Bool(false)),
                        ("state", JValue::Str("uncertain".into())),
                        ("final", JValue::Bool(false)),
                        ("partial", JValue::Bool(true)),
                    ]),
                ),
            ])
        };
        let handle = grant.get("handle").as_str().unwrap_or("").to_string();
        let Ok(done) = self.rt.block_on(self.issuer.revoke(&handle, Some(10_000.0))) else {
            return Ok(uncertain());
        };
        let done = wtf16::parse(&done.to_string()).unwrap_or(JValue::Null);
        let ok = done.get("confirmed").as_bool() == Some(true)
            && done.get("report").is_object()
            && done.get("report").get("final").as_bool() == Some(true);
        Ok(if ok {
            JValue::obj([
                ("confirmed", JValue::Bool(true)),
                ("report", done.get("report").clone()),
            ])
        } else {
            uncertain()
        })
    }
    fn forget(&self, grant: &JValue, snapshot_id: &str) -> Result<JValue, Fail> {
        let handle = grant.get("handle").as_str().unwrap_or("").to_string();
        let confirmed = self
            .rt
            .block_on(self.issuer.forget(&handle, snapshot_id))
            .unwrap_or(false);
        Ok(JValue::obj([("confirmed", JValue::Bool(confirmed))]))
    }
}

/// The maintenance-only lease of one start, and whether a tune is active.
struct FrontGate {
    paused: bool,
    model: String,
    admission: Arc<Admission>,
    rt: tokio::runtime::Handle,
}

struct Held {
    lease: Option<engine_admission::Lease>,
    rt: tokio::runtime::Handle,
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            self.rt.spawn(lease.release());
        }
    }
}

impl Inference for FrontGate {
    fn enter(&self) -> Result<Lease, Fail> {
        let cancel = model_stream::Cancel::new();
        let lease = self
            .rt
            .block_on(
                self.admission
                    .enter(Ask::Embedding { model: &self.model }, &cancel),
            )
            .map_err(|r| Fail {
                status: Some(r.status),
                message: r.message.clone(),
                public_message: Some(r.message),
                result: JValue::Undefined,
            })?;
        Ok(Box::new(Held {
            lease: Some(lease),
            rt: self.rt.clone(),
        }))
    }
    fn paused(&self) -> bool {
        self.paused
    }
}

/// The process's Code service, made on first use.
pub fn service(app: &Arc<App>) -> Arc<Service> {
    app.code_service
        .get_or_init(|| {
            let s = &app.config.code;
            let rt = tokio::runtime::Handle::current();
            let egress = app.code_egress.clone().map(|issuer| {
                Arc::new(FrontEgress {
                    issuer,
                    rt: rt.clone(),
                }) as Arc<dyn Egress>
            });
            Service::new(Options {
                repos: parse_repos(&s.repos),
                harnesses: default_harnesses(
                    s.harness_name.as_deref(),
                    s.harness_version.as_deref(),
                ),
                sandboxed: s.endpoint.is_some(),
                transport: s.transport(),
                tree_root: s.tree_root.clone(),
                harness_user: parse_user(s.harness_user.as_deref()),
                egress,
                // Every start resolves its own engine (core's bridge); none is ever run without it.
                engine: Arc::new(Engine::default),
                inference: None,
                guard: Arc::new(|| false),
                log: Arc::new(|entry| {
                    println!("[code] {}", wtf16::stringify(&entry).unwrap_or_default());
                }),
                now: Arc::new(server_projects::task_store::date_now),
                timeout: APPROVAL_TIMEOUT,
                base_path: s.path.clone(),
                base_lang: s.lang.clone(),
                new_id: Arc::new(server_projects::source_jobs::new_id),
                epoch: server_projects::source_jobs::new_id(),
            })
        })
        .clone()
}

/// What core says for one start.
async fn resolve(app: &Arc<App>, user: &str, project: &str) -> Result<Resolved, Response<Body>> {
    let body = json!({ "user": user, "projectId": project });
    let Some((status, bytes)) = app
        .config
        .research
        .node_call(ENGINE_PATH, &body, ENGINE_DEADLINE)
        .await
    else {
        return Err(reply::error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE, true));
    };
    if status == 404 {
        return Err(reply::error(StatusCode::NOT_FOUND, "project not found", true));
    }
    let parsed = (status == 200)
        .then(|| wtf16::parse(&String::from_utf8_lossy(&bytes)).ok())
        .flatten()
        .filter(JValue::is_object);
    let Some(facts) = parsed else {
        return Err(reply::error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE, true));
    };
    if facts.get("machine").as_bool() == Some(true) {
        return Err(reply::error(StatusCode::CONFLICT, ON_A_MACHINE, true));
    }
    let e = facts.get("engine");
    let text = |v: &JValue| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let engine = Engine {
        base_url: text(e.get("baseUrl")),
        api_key: text(e.get("apiKey")),
        model: text(e.get("model")),
        context_tokens: e.get("contextTokens").as_num(),
    };
    let gate = FrontGate {
        paused: facts.get("paused").as_bool() == Some(true),
        model: engine.model.clone().unwrap_or_else(|| "code".into()),
        admission: app.config.research.admission(),
        rt: tokio::runtime::Handle::current(),
    };
    Ok(Resolved {
        engine: Some(engine),
        inference: Some(Arc::new(gate)),
        context: facts.get("context").as_str().unwrap_or("").to_string(),
    })
}

/// One Code action, after the session gate.
pub async fn serve(
    app: &Arc<App>,
    authn: server_auth::Identity,
    data_dir: PathBuf,
    action: Action,
    body: Body,
) -> Response<Body> {
    let admin = authn.user.role == server_auth::Role::Admin;
    let user = authn.user.id.as_str().to_owned();
    match task_routes::code_flag(app, &user, admin).await {
        None => return task_routes::unreachable(),
        Some(Facts::Off) => {
            return reply::error(
                StatusCode::NOT_FOUND,
                "Code mode is not enabled on this server.",
                true,
            )
        }
        Some(Facts::Member) => {
            return reply::error(StatusCode::FORBIDDEN, "Administrator required", true)
        }
        Some(_) => {}
    }
    let Some(project_id) = server_projects::js::decode_uri_component(action.project()) else {
        return failed();
    };
    let record = {
        let (data_dir, user, id) = (data_dir.clone(), user.clone(), project_id.clone());
        let switch = app.config.rust_projects;
        tokio::task::spawn_blocking(move || task_routes::project_record(&data_dir, &user, switch, &id))
            .await
    };
    let record = match record {
        Ok(Ok(Some(r))) => r,
        Ok(Ok(None)) => return reply::error(StatusCode::NOT_FOUND, "project not found", true),
        Ok(Err(())) | Err(_) => return failed(),
    };
    let Ok(dir) = server_projects::store::user_dir(&data_dir, &user) else {
        return failed();
    };
    let Some(dir) = dir.to_str().map(str::to_string) else {
        return failed();
    };
    let service = service(app);
    match action {
        Action::NotAllowed(_) => {
            reply::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", true)
        }
        Action::Start(_) => {
            let body = match read_json(body).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let resolved = match resolve(app, &user, &project_id).await {
                Ok(r) => r,
                Err(r) => return r,
            };
            let work = tokio::task::spawn_blocking(move || {
                let project = Project {
                    id: &project_id,
                    record: &record,
                };
                service.start(&dir, Some(&user), &project, resolved, &body)
            })
            .await;
            match work {
                Ok(Ok(v)) => answer(StatusCode::ACCEPTED, &v),
                Ok(Err(e)) => refused(&e),
                Err(_) => Rejection::Unavailable.into_response(),
            }
        }
        Action::Decide(_, task) => {
            let body = match read_json(body).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let (decision, approval) = (
                text_of(body.get("decision")),
                text_of(body.get("approvalId")),
            );
            let work = tokio::task::spawn_blocking(move || {
                service.decide(&dir, &project_id, &task, &decision, &approval)
            })
            .await;
            match work {
                Ok(Ok(v)) => answer(StatusCode::OK, &v),
                Ok(Err(e)) => refused(&e),
                Err(_) => Rejection::Unavailable.into_response(),
            }
        }
        Action::Cancel(_, task) => {
            let work =
                tokio::task::spawn_blocking(move || service.cancel(&dir, &project_id, &task)).await;
            match work {
                Ok(Ok(v)) => answer(StatusCode::OK, &v),
                Ok(Err(e)) => refused(&e),
                Err(_) => Rejection::Unavailable.into_response(),
            }
        }
    }
}

/// The task reads' facts when the front owns Code (task_routes.rs).
pub async fn facts(app: &Arc<App>, data_dir: &std::path::Path, user: &str) -> Option<JValue> {
    let dir = server_projects::store::user_dir(data_dir, user).ok()?;
    let dir = dir.to_str()?.to_string();
    let service = service(app);
    tokio::task::spawn_blocking(move || service.facts(&dir).ok())
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const T: &str = "0123abcd-0123-4abc-8def-0123456789ab";

    #[test]
    fn the_actions_are_cores_pattern_less_the_reads() {
        let p = |m: &str, path: &str| parse(m, path);
        assert_eq!(p("POST", "/api/projects/qa/code"), Some(Action::Start("qa".into())));
        assert_eq!(
            p("POST", "/api/projects/qa/code/approve"),
            Some(Action::Start("qa".into())),
            "core's pattern: no task id is a start"
        );
        assert_eq!(
            p("POST", &format!("/api/projects/qa/code/{T}/approve")),
            Some(Action::Decide("qa".into(), T.into()))
        );
        assert_eq!(
            p("POST", &format!("/api/projects/qa/code/{T}/cancel")),
            Some(Action::Cancel("qa".into(), T.into()))
        );
        for (m, path) in [
            ("PUT", "/api/projects/qa/code".to_string()),
            ("POST", format!("/api/projects/qa/code/{T}")),
            ("GET", format!("/api/projects/qa/code/{T}/approve")),
            ("DELETE", format!("/api/projects/qa/code/{T}/cancel")),
            ("GET", "/api/projects/qa/code/cancel".to_string()),
        ] {
            assert_eq!(p(m, &path), Some(Action::NotAllowed("qa".into())), "{m} {path}");
        }
        for (m, path) in [
            ("GET", "/api/projects/qa/code".to_string()),
            ("GET", format!("/api/projects/qa/code/{T}")),
            ("POST", "/api/projects//code".to_string()),
            ("POST", "/api/projects/qa/codes".to_string()),
            ("POST", format!("/api/projects/qa/code/{T}/merge")),
            ("POST", "/api/projects/qa/code/0123ABCD-0123-4abc-8def-0123456789ab/cancel".to_string()),
            ("POST", format!("/api/projects/qa/code/{T}x")),
            ("POST", "/api/projects/qa/browser".to_string()),
            ("POST", "/api/projects/a/b/code".to_string()),
        ] {
            assert_eq!(p(m, &path), None, "{m} {path}");
        }
    }

    #[test]
    fn the_settings_are_read_as_core_reads_them() {
        let env = |pairs: &[(&str, &str)]| {
            let owned: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            move |k: &str| owned.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        };
        let off = Settings::from_lookup(&env(&[]));
        assert!(!off.executor);
        assert!(off.transport().is_none());
        for v in ["", "node", "Rust", "1", "true"] {
            assert!(!Settings::from_lookup(&env(&[(SWITCH, v)])).executor, "{v:?}");
        }
        let on = Settings::from_lookup(&env(&[
            (SWITCH, "rust"),
            ("CODE_HARNESS_ENDPOINT", "code-sandbox:7000"),
            ("CODE_HARNESS_COMMAND", "opencode"),
            ("CODE_HARNESS_ARGS", " acp  --qa "),
        ]));
        assert!(on.executor);
        assert_eq!(
            on.transport(),
            Some(code_acp::Transport::Socket("code-sandbox:7000".into())),
            "the sandbox wins"
        );
        let spawn = Settings::from_lookup(&env(&[
            ("CODE_HARNESS_COMMAND", "opencode"),
            ("CODE_HARNESS_ARGS", " acp  --qa "),
        ]));
        assert_eq!(
            spawn.transport(),
            Some(code_acp::Transport::Spawn {
                command: "opencode".into(),
                args: vec!["acp".into(), "--qa".into()],
            })
        );
    }
}
