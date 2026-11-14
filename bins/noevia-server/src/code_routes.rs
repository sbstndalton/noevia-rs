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
//! The Planner preparation (core code-service.cjs `PLANNER_PREPARATION`, the Code pipeline #705)
//! runs here too, through crates/code-pipeline's driver over code-harness's `pipeline_host`, with
//! the role engine (crates/role-engine) on [`FrontRoles`] and the verifier (crates/code-verify,
//! `CODE_VERIFY_ENDPOINT`). It is offered exactly when Node offers it: features.codeHarness and
//! features.codePipeline on (read from the front's own settings, as features.cjs reads them, with
//! codePipeline and codeMerge available exactly when features.cjs's `codeSandboxReason` says so:
//! `CODE_HARNESS_ENDPOINT` set and not blank) and Code mode in the sandbox. Its start needs the role engine's endpoint
//! from core's engine facts (`role`, an additive fact): a core without it answers the Planner start
//! with Node's 409 "The Planner pipeline is not set up on this server.". Flags off, the front
//! offers and runs exactly what it did before.
//!
//! The Planner review of a direct task (core code-harness.cjs `reviewGate`, #519) runs here too
//! (code-harness's `review_gate`): features.plannerReview is read once per direct start (an
//! unreadable setting is off), and with it on the finished, released change goes to the reviewer
//! (code-review.cjs `createPlannerReview` over the same role engine as the Planner: a review alone
//! pins the task's model for that task) and then on a `review_change` card, answered through the
//! decide route below. A core that gives no `role` fact gets Node's "No reviewer model is
//! configured on this server." on that card; one whose reviewer cannot be built here (the HTTP
//! client failing) gets review_gate's `Unreachable` ("The reviewer could not be reached."). The
//! switch-flip condition "codePipeline and plannerReview off" therefore goes away: the front runs
//! both, so `NOEVIA_CODE_EXECUTOR=rust` no longer has to wait for either flag to be off.
//!
//! features.plannerReview is read as features.cjs `selected` reads it: the env value, else the
//! typed `feature:plannerReview` row, else (missing or not `'true'`/`'false'`) the legacy
//! `feature:astraReview` row, else the default (off).
//!
//! Named differences (each refused, never run differently):
//! - the Code role on an inference machine (core's relay, #1853) is refused with 409, for the
//!   Planner and the review too (so no role call ever goes to a machine); with
//!   `NOEVIA_RUST_CODE_RELAY=1` (council p4, code_machine.rs) a direct task runs through the
//!   front's relay instead. A relay refusal (no relay port, the role moved, another model named,
//!   the machine unreachable or serving another model) then answers the start with 409 and Node's
//!   text, where Node starts the task and fails it with the same text; the relay's log events name
//!   the project rather than the task (the task id is made after the engine is resolved);
//! - the inference budget check (core `modelManager.loadRefusal`) is core's, asked over its bridge:
//!   when core cannot answer, the Planner's pin is refused as over budget ([`BUDGET_UNCHECKED`])
//!   rather than pinned unchecked;
//! - the role engine's endpoint and the engine key a Planner task uses are a snapshot taken from
//!   core's engine facts at its start, for the life of that task; Node's `codeRoleEndpoint` and
//!   `engineKey` re-read them at each use, so a Code role or key changed mid-task reaches Node's
//!   next role call but not the front's.
//!
//! The lease: a task holds the inference admission authority's maintenance-only lease (the one
//! Node's latch takes as `modelManager.enterInference()`, as Node's own Code tasks hold) for its run.
//! A lease the authority ends (Node restarted, stopped confirming it, the hard cap) stops the run
//! (logged `code.lease_lost`): Node's own lease cannot end under a task, the front's can.
//!
//! A Cowork session's task start from chat (`POST /api/chat` with `mode: 'cowork'`, routes/chat.cjs)
//! is the front's too when it owns Code and `NOEVIA_RUST_COWORK=1` ([`cowork`], called by
//! chat_turn.rs after chat_edge.rs's refusals): the same service start with Node's Cowork body and
//! answers. Without it core refuses every such start (409) once the executor switch is on
//! (noevia#2671); the second switch keeps a release of this front from changing that unflipped.
//!
//! Code actions are answered only when core says the front owns them (`owner: 'front'` on the
//! facts bridge); a core that still runs Code tasks itself gets 503, never a second writer.
use crate::code_egress::Issuer;
use crate::task_routes::{self, Facts};
use crate::{identity::Rejection, reply, App};
use axum::{
    body::Body,
    http::{Response, StatusCode},
    response::IntoResponse,
};
use code_harness::executor::{Egress, Engine, Fail, Lease};
use code_harness::pipeline_host::{EngineRoles, Flag, PipeHost, PipeVerify};
use code_harness::review_gate::{PlannerReview, Reviewer, Unconfigured, Unreachable};
use code_harness::service::{
    default_harnesses, parse_repos, parse_user, Inference, Options, Parts, Project, Resolved,
    Service, Setup, APPROVAL_TIMEOUT,
};
use code_pipeline::driver;
use engine_admission::{Admission, Ask};
use js_json::{to_js_string, wtf16, JValue};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The switch: `rust` hands Code tasks to the front.
pub const SWITCH: &str = "NOEVIA_CODE_EXECUTOR";
/// Council p1: `NOEVIA_RUST_COWORK=1` (with [`SWITCH`] on) lets the front answer a Cowork start from
/// chat ([`cowork`]); unset (or any other value), core answers it as before.
pub const COWORK_SWITCH: &str = "NOEVIA_RUST_COWORK";
const ENGINE_PATH: &str = "/internal/code/engine";
const ENGINE_DEADLINE: Duration = Duration::from_secs(5);
const UNAVAILABLE: &str = "Code tasks are unavailable right now. Try again shortly.";
/// The Planner's pin when core cannot say whether the model fits the inference budget.
pub const BUDGET_UNCHECKED: &str =
    "The inference memory budget could not be checked, so no model was pinned for this task.";
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
    /// `CODE_VERIFY_ENDPOINT`: the pipeline's verifier.
    pub verify_endpoint: Option<String>,
    /// Track R11: where a start's engine facts come from (`NOEVIA_RUST_ENGINE_FACTS`, code_facts.rs).
    pub facts: crate::code_facts::Settings,
    /// [`COWORK_SWITCH`] is `1`.
    pub cowork: bool,
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
            verify_endpoint: set("CODE_VERIFY_ENDPOINT"),
            facts: crate::code_facts::Settings::from_lookup(get),
            cowork: get(COWORK_SWITCH).as_deref() == Some("1"),
        }
    }

    /// features.cjs `codeSandboxReason(env) === null`: `CODE_HARNESS_ENDPOINT` not blank after JS's
    /// `trim` (Unicode white space and U+FEFF, without U+0085).
    pub fn sandbox_available(&self) -> bool {
        self.endpoint.as_deref().is_some_and(|e| {
            !e.trim_matches(|c: char| c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}'))
                .is_empty()
        })
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
    reply::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Code request failed",
        true,
    )
}

/// core `readJson`: at most 1 MiB, an empty body is `{}`.
async fn read_json(body: Body) -> Result<JValue, Box<Response<Body>>> {
    let read = crate::account_routes::read_body(body, BODY_LIMIT)
        .await
        .map_err(|()| Box::new(reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true)))?;
    if read.over {
        return Err(Box::new(reply::error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request exceeds size limit",
            true,
        )));
    }
    if read.bytes.is_empty() {
        return Ok(JValue::Obj(Vec::new()));
    }
    wtf16::parse(&String::from_utf8_lossy(&read.bytes))
        .map_err(|_| Box::new(reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true)))
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
pub(crate) struct FrontEgress {
    issuer: Arc<Issuer>,
    rt: tokio::runtime::Handle,
}

/// The egress controller over the front's issuer, for Browser tasks too (browser_routes.rs): one
/// proxy per deployment, each task scoped by its own grant.
pub(crate) fn front_egress(app: &App) -> Option<Arc<FrontEgress>> {
    let rt = tokio::runtime::Handle::current();
    app.code_egress
        .clone()
        .map(|issuer| Arc::new(FrontEgress { issuer, rt }))
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
        let Ok(done) = self
            .rt
            .block_on(self.issuer.revoke(&handle, Some(10_000.0)))
        else {
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
    watch: Option<tokio::task::JoinHandle<()>>,
}

impl code_harness::executor::Holding for Held {
    /// The authority ends a lease that no longer covers its holder (`Lease::lost`).
    fn on_lost(&mut self, hook: Box<dyn FnOnce() + Send>) {
        let Some(lost) = self.lease.as_ref().map(|l| l.lost().clone()) else {
            return;
        };
        self.watch = Some(self.rt.spawn(async move {
            lost.cancelled().await;
            // Stopping the run takes locks and waits briefly: not on a runtime worker.
            let _ = tokio::task::spawn_blocking(hook).await;
        }));
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(w) = self.watch.take() {
            w.abort();
        }
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
            watch: None,
        }))
    }
    fn paused(&self) -> bool {
        self.paused
    }
}

fn log_code(entry: &JValue) {
    println!("[code] {}", wtf16::stringify(entry).unwrap_or_default());
}

/// features.cjs `enabled(name)` for a restart-free flag: the env value, else the admin setting,
/// else the default; an unreadable setting is off.
pub fn feature(app: &App, feature: server_features::Feature) -> bool {
    let env = server_features::Feature::ALL
        .iter()
        .position(|f| *f == feature)
        .and_then(|i| app.config.feature_context_env.get(i).copied().flatten());
    let stored = if env.is_some() {
        None
    } else {
        // features.cjs `selected`: the typed row, then the declared legacy row
        // (`feature:astraReview` for plannerReview) when the new one is missing or not typed.
        app.writes.writer().ok().and_then(|w| {
            w.read(|r| server_account::feature_context::startup::selected(r, feature))
                .ok()
                .flatten()
                .map(|on| if on { "true" } else { "false" })
        })
    };
    server_features::evaluate_with_context(feature, env, stored, availability(app, feature), None)
        .enabled
}

/// features.cjs's runtime availability for the flags Code mode reads: the Code-sandbox ones
/// (codePipeline, codeMerge) exactly when `codeSandboxReason` is null, i.e. `CODE_HARNESS_ENDPOINT`
/// is set and not blank (JS `trim`); every other flag Code mode reads (codeHarness, plannerReview,
/// executorGuard, constrainedPlanDecoding) has no runtime availability and needs none.
fn availability(app: &App, feature: server_features::Feature) -> server_features::Availability {
    use server_features::{Availability, AvailabilityKind};
    match server_features::spec(feature).availability {
        AvailabilityKind::CodeSandbox if app.config.code.sandbox_available() => {
            Availability::Available
        }
        AvailabilityKind::CodeSandbox => Availability::Unavailable,
        _ => Availability::Unknown,
    }
}

/// The flags the pipeline reads during a run (core index.cjs `merge`, `constrain`, the guard).
fn pipeline_flag(app: &App, flag: Flag) -> bool {
    use server_features::Feature;
    feature(
        app,
        match flag {
            Flag::Merge => Feature::CodeMerge,
            Flag::Constrain => Feature::ConstrainedPlanDecoding,
            Flag::Guard => Feature::ExecutorGuard,
        },
    )
}

/// core index.cjs's `pipeline: { enabled, create }` for the front.
struct FrontPipeline {
    app: std::sync::Weak<App>,
    rt: tokio::runtime::Handle,
    verify: Arc<code_verify::Adapter>,
}

/// core's `role` engine fact (index.cjs `codeRoleEndpoint`): `None` when core did not give one.
pub fn role_endpoint(facts: &JValue) -> Option<role_engine::Endpoint> {
    let role = facts.get("role");
    if !role.is_object() {
        return None;
    }
    let text = |v: &JValue| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    Some(role_engine::Endpoint {
        base_url: text(role.get("baseUrl")),
        api_key: text(role.get("apiKey")),
        model: text(role.get("model")),
        provider: JValue::obj([(
            "capabilities",
            JValue::obj([(
                "jsonSchemaParam",
                JValue::Bool(role.get("jsonSchema").as_bool() == Some(true)),
            )]),
        )]),
        external: role.get("external").as_bool() == Some(true),
        machine: None,
    })
}

impl FrontPipeline {
    /// This start's Code role engine (core's `role` fact) and its key, or `None` without one.
    fn roles(&self, resolved: &Resolved) -> Option<(EngineRoles<FrontRoles>, Option<String>)> {
        let endpoint = role_endpoint(resolved.facts.as_ref()?)?;
        let client = model_stream::client().ok()?;
        let engine_key = endpoint.api_key.clone();
        let engine = role_engine::RoleEngine::new(FrontRoles {
            endpoint,
            app: self.app.clone(),
            client,
        });
        let roles = EngineRoles {
            engine: Arc::new(engine),
            blocking: Arc::new(role_engine::Blocking::with_handle(self.rt.clone())),
        };
        Some((roles, engine_key))
    }
}

impl Setup for FrontPipeline {
    fn enabled(&self) -> bool {
        use server_features::Feature;
        self.app.upgrade().is_some_and(|app| {
            feature(&app, Feature::CodeHarness) && feature(&app, Feature::CodePipeline)
        })
    }

    fn review_enabled(&self) -> bool {
        self.app
            .upgrade()
            .is_some_and(|app| feature(&app, server_features::Feature::PlannerReview))
    }

    fn reviewer(&self, resolved: &Resolved) -> Arc<dyn Reviewer> {
        // No `role` fact from core: Node's "no reviewer configured". A role fact but no reviewer
        // (the HTTP client could not be built): review_gate's `Unreachable`.
        if resolved.facts.as_ref().and_then(role_endpoint).is_none() {
            return Arc::new(Unconfigured);
        }
        match self.roles(resolved) {
            Some((roles, _)) => Arc::new(PlannerReview::new(
                Arc::new(roles),
                Arc::new(|e| log_code(&e)),
            )),
            None => Arc::new(Unreachable),
        }
    }

    fn create(&self, parts: Parts, resolved: &Resolved) -> Option<driver::Deps> {
        let (roles, engine_key) = self.roles(resolved)?;
        let app = self.app.clone();
        let flag = move |f: Flag| app.upgrade().is_some_and(|a| pipeline_flag(&a, f));
        Some(driver::Deps::new(
            parts.jobs,
            parts.workspaces,
            parts.harness,
            Some(Arc::new(roles)),
            Some(Arc::new(PipeVerify {
                adapter: self.verify.clone(),
                workspaces: parts.tree,
            })),
            parts.approvals,
            Arc::new(PipeHost {
                flag: Arc::new(flag),
                engine_key,
                log: Arc::new(|e| log_code(&e)),
                now: Arc::new(server_projects::task_store::date_now),
            }),
        ))
    }
}

/// The role engine's dependencies in the front (core index.cjs `createRoleEngine({...})`): this
/// start's endpoint, the front's HTTP client (no redirect, no proxy), the sidecars' models as
/// the keep-list (core's facts), the roles file beside models.ini, core's budget check, no
/// admission (production passes none) and no machines (refused before any start).
pub struct FrontRoles {
    endpoint: role_engine::Endpoint,
    app: std::sync::Weak<App>,
    client: reqwest::Client,
}

/// A response body, chunk by chunk.
pub struct FrontBody(Option<reqwest::Response>);

fn transport(detail: impl ToString) -> role_engine::TransportError {
    role_engine::TransportError {
        detail: detail.to_string(),
    }
}

impl role_engine::Body for FrontBody {
    async fn read(&mut self) -> Result<Option<Vec<u8>>, role_engine::TransportError> {
        match self.0.as_mut() {
            None => Ok(None),
            Some(r) => r
                .chunk()
                .await
                .map(|c| c.map(|b| b.to_vec()))
                .map_err(transport),
        }
    }
    fn cancel(&mut self) {
        self.0 = None;
    }
}

impl role_engine::Deps for FrontRoles {
    type Body = FrontBody;

    fn engine(&self) -> Option<role_engine::Endpoint> {
        Some(self.endpoint.clone())
    }

    async fn fetch(
        &self,
        req: role_engine::Request,
        cancel: role_engine::Cancel,
    ) -> Result<role_engine::Response<FrontBody>, role_engine::TransportError> {
        let method = if req.method == "GET" {
            reqwest::Method::GET
        } else {
            reqwest::Method::POST
        };
        let mut b = self.client.request(method, &req.url);
        for (k, v) in &req.headers {
            b = b.header(k.as_str(), v.as_str());
        }
        if let Some(body) = req.body {
            b = b.body(body);
        }
        if let Some(ms) = req.timeout_ms {
            b = b.timeout(Duration::from_millis(ms));
        }
        let sent = tokio::select! {
            r = b.send() => r.map_err(transport)?,
            () = cancel.cancelled() => return Err(transport("aborted")),
        };
        Ok(role_engine::Response {
            status: sent.status().as_u16(),
            body: FrontBody(Some(sent)),
        })
    }

    async fn keep(&self) -> Option<Vec<String>> {
        let app = self.app.upgrade()?;
        crate::model_state_routes::sidecar_names(&app)
            .await
            .map(|names| names.into_iter().collect())
    }

    fn system_roles(&self) -> std::collections::HashMap<String, String> {
        self.app
            .upgrade()
            .map(|a| crate::model_kind::roles_beside(a.config.sizing.presets_path()))
            .unwrap_or_default()
    }

    async fn budget_refusal(&self, model: &str) -> Option<role_engine::BudgetRefusal> {
        use crate::model_maint::node_lease::LoadDecision;
        let unchecked = || role_engine::BudgetRefusal {
            error: Some(BUDGET_UNCHECKED.into()),
        };
        let Some(app) = self.app.upgrade() else {
            return Some(unchecked());
        };
        match app.config.maint.node.load_refusal(model).await {
            LoadDecision::Proceed => None,
            LoadDecision::Refused(r) => Some(role_engine::BudgetRefusal {
                error: r.get("error").and_then(|e| e.as_str()).map(str::to_string),
            }),
            LoadDecision::Unavailable => Some(unchecked()),
        }
    }

    fn log(&self, entry: JValue) {
        log_code(&entry);
    }

    fn now(&self) -> f64 {
        server_projects::task_store::date_now()
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
                log: Arc::new(|entry| log_code(&entry)),
                now: Arc::new(server_projects::task_store::date_now),
                timeout: APPROVAL_TIMEOUT,
                base_path: s.path.clone(),
                base_lang: s.lang.clone(),
                new_id: Arc::new(server_projects::source_jobs::new_id),
                epoch: server_projects::source_jobs::new_id(),
                pipeline: Some(Arc::new(FrontPipeline {
                    app: Arc::downgrade(app),
                    rt: rt.clone(),
                    verify: Arc::new(code_verify::Adapter::new(Some(Box::new(
                        code_verify::Verify::new(code_verify::Options {
                            log: Box::new(|e| log_code(&e.to_json())),
                            ..code_verify::Options::new(s.verify_endpoint.clone())
                        }),
                    )
                        as Box<dyn code_verify::Verifier>))),
                })),
            })
        })
        .clone()
}

/// What core says for one start: its answer on `/internal/code/engine`, or `None`. A project core
/// does not know (or a bridge that is not mounted, which answers the same 404) is no answer: the
/// front has just found it.
async fn core_facts(app: &App, user: &str, project: &str) -> Option<JValue> {
    let body = json!({ "user": user, "projectId": project });
    let (status, bytes) = app
        .config
        .research
        .node_call(ENGINE_PATH, &body, ENGINE_DEADLINE)
        .await?;
    (status == 200)
        .then(|| wtf16::parse(&String::from_utf8_lossy(&bytes)).ok())
        .flatten()
        .filter(JValue::is_object)
}

/// The engine facts for one start, called by the service where Node reads the engine: after the
/// body's checks and the one-task check. Core's (the default), or the front's own behind
/// `NOEVIA_RUST_ENGINE_FACTS` (code_facts.rs); either way a missing answer is the "unavailable" 503.
async fn resolve(
    app: Arc<App>,
    user: String,
    project: String,
    record: JValue,
    model: Option<String>,
) -> Result<Resolved, Fail> {
    use crate::code_facts::{self, Mode};
    let down = || fail(503, UNAVAILABLE);
    let facts = match app.config.code.facts.mode {
        Mode::Core => core_facts(&app, &user, &project).await.ok_or_else(down)?,
        Mode::Shadow => {
            let core = core_facts(&app, &user, &project).await.ok_or_else(down)?;
            code_facts::shadow(&app, &user, &record, &core).await;
            core
        }
        Mode::Rust => {
            code_facts::native(&app, &user, &record, || core_facts(&app, &user, &project))
                .await
                .map_err(|_| down())?
        }
    };
    if facts.get("machine").as_bool() == Some(true) {
        return on_a_machine(&app, &user, record, model.as_deref(), facts).await;
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
        facts: Some(facts),
    })
}

/// The Code role is on an inference machine (core said `machine: true`). Without the relay
/// (`NOEVIA_RUST_CODE_RELAY` unset) this is the 409 it always was. With it, the harness is given the
/// relay (code_machine.rs) for this run: its URL, a token that lives as long as the run (held by
/// the start's gate, closed when the gate is dropped at the run's end) and the machine's model; no
/// local inference lease and no tune pause (core: "a task whose Code role is on a machine does not
/// use this machine's engine"). A relay refusal is the start's answer with its code and text
/// (Node's `publicMessage`), as core's executor throws it.
///
/// The Planner preparation and the review stay refused on a machine (the role engine's machine pin
/// is not the front's yet): core gives no `role` fact on a machine, so a pipeline start answers
/// Node's 409 "The Planner pipeline is not set up on this server." and a review its "No reviewer
/// model is configured on this server." card.
async fn on_a_machine(
    app: &Arc<App>,
    user: &str,
    record: JValue,
    model: Option<&str>,
    facts: JValue,
) -> Result<Resolved, Fail> {
    let Some(state) = app.code_relay.get().and_then(Clone::clone) else {
        return Err(fail(409, ON_A_MACHINE));
    };
    let task = record.get("id").as_str().unwrap_or("").to_string();
    let opened = state.open(user, &task, model).await.map_err(|r| Fail {
        status: Some(409),
        message: r.message.clone(),
        public_message: Some(r.message),
        result: JValue::Undefined,
    })?;
    let engine = Engine {
        base_url: Some(opened.base_url.clone()),
        api_key: Some(opened.token.clone()),
        model: Some(opened.model.clone()),
        context_tokens: opened.context_tokens,
    };
    let gate = MachineGate {
        lease: std::sync::Mutex::new(Some(opened)),
    };
    Ok(Resolved {
        engine: Some(engine),
        inference: Some(Arc::new(gate)),
        context: facts.get("context").as_str().unwrap_or("").to_string(),
        facts: Some(facts),
    })
}

/// A machine task's gate: no local inference lease and never paused (core's `enter: () => () =>
/// {}`, `paused: () => false` on a machine). It carries the run's relay lease: [`enter`] hands it
/// to the executor, which holds it for the run and drops it at the end (closing it, as core's
/// `finally` closes `relayLease`); a start that never reaches its run closes it with the gate.
///
/// [`enter`]: code_harness::service::Inference::enter
struct MachineGate {
    lease: std::sync::Mutex<Option<code_engine_relay::Opened>>,
}

/// The relay lease as the run holds it.
struct RelayHeld(#[allow(dead_code)] code_engine_relay::Opened);

impl code_harness::executor::Holding for RelayHeld {}

impl code_harness::service::Inference for MachineGate {
    fn enter(&self) -> Result<Lease, Fail> {
        let taken = self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Ok(match taken {
            Some(opened) => Box::new(RelayHeld(opened)),
            None => Box::new(()),
        })
    }
    fn paused(&self) -> bool {
        false
    }
}

fn fail(status: u16, message: &str) -> Fail {
    Fail {
        status: Some(status),
        message: message.to_string(),
        public_message: Some(message.to_string()),
        result: JValue::Undefined,
    }
}

/// code-service.cjs `start` for one project the caller owns: the service's answer, or `None` when
/// the blocking task did not finish (a panic).
async fn start(
    app: &Arc<App>,
    service: Arc<Service>,
    dir: String,
    user: String,
    project_id: String,
    record: JValue,
    body: JValue,
) -> Option<Result<JValue, Fail>> {
    let (app, rt) = (Arc::clone(app), tokio::runtime::Handle::current());
    tokio::task::spawn_blocking(move || {
        let project = Project {
            id: &project_id,
            record: &record,
        };
        // The task's model as the executor reads it (`String(body.model)` when truthy): a machine
        // task naming another model is refused by the relay (code-harness.cjs `relay.open`).
        let model = match body.get("model") {
            v if v.truthy() => to_js_string(v).ok(),
            _ => None,
        };
        let resolve = || {
            rt.block_on(resolve(
                Arc::clone(&app),
                user.clone(),
                project_id.clone(),
                record.clone(),
                model.clone(),
            ))
        };
        service.start(&dir, Some(&user), &project, &resolve, &body)
    })
    .await
    .ok()
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
        Some(Facts::Front) => {}
        // Code mode is on but core still runs Code tasks itself (it does not have the switch, or
        // predates it): the front must not open the tenant's journals beside it.
        Some(Facts::Admin(_)) => {
            return reply::error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE, true)
        }
    }
    let Some(project_id) = server_projects::js::decode_uri_component(action.project()) else {
        return failed();
    };
    let record = {
        let (data_dir, user, id) = (data_dir.clone(), user.clone(), project_id.clone());
        let switch = app.config.rust_projects;
        tokio::task::spawn_blocking(move || {
            task_routes::project_record(&data_dir, &user, switch, &id)
        })
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
                Err(r) => return *r,
            };
            match start(app, service, dir, user, project_id, record, body).await {
                Some(Ok(v)) => answer(StatusCode::ACCEPTED, &v),
                Some(Err(e)) => refused(&e),
                None => Rejection::Unavailable.into_response(),
            }
        }
        Action::Decide(_, task) => {
            let body = match read_json(body).await {
                Ok(b) => b,
                Err(r) => return *r,
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

/// routes/chat.cjs's answer for a Cowork start that failed: `status || 500` and
/// `{error: publicMessage || (status ? message : 'Cowork task could not start'), mode: 'cowork'}`.
fn cowork_refused(e: &Fail) -> Response<Body> {
    let message = match (&e.public_message, e.status) {
        (Some(m), _) if !m.is_empty() => m.clone(),
        (_, Some(_)) => e.message.clone(),
        (_, None) => COWORK_FAILED.to_string(),
    };
    let status = e
        .status
        .and_then(|s| StatusCode::from_u16(s).ok())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    cowork_answer(status, JValue::obj([("error", JValue::Str(message))]))
}

/// `json(res, status, { ...body, mode: 'cowork' })` in routes/chat.cjs's key order: the error, then
/// the mode; a start's `{mode, task}`.
fn cowork_answer(status: StatusCode, body: JValue) -> Response<Body> {
    let mut pairs = match body {
        JValue::Obj(pairs) => pairs,
        _ => Vec::new(),
    };
    pairs.push(("mode".into(), JValue::Str("cowork".into())));
    answer(status, &JValue::Obj(pairs))
}

/// routes/chat.cjs's fallback words for a Cowork start that failed without a status.
pub const COWORK_FAILED: &str = "Cowork task could not start";

/// A Cowork session's task start from chat (routes/chat.cjs `startCoworkTask`, #236), answered
/// by the front's own Code service when the front owns Code tasks: index.cjs's
/// `getProject(String(body.projectId))` (no URI decoding, unlike the Code route), then
/// `codeService.start(workspace, project, { repository: body.repository, prompt: body.message })`
/// (the default capabilities, the first harness, the direct preparation, no domains, no model),
/// answered 202 `{mode: 'cowork', task}`.
///
/// `bytes` is the chat body chat_edge.rs already let through as [`chat_edge::Verdict::Cowork`]
/// (an object, an administrator, `projectId` a non-empty string). `None` means "not the front's":
/// either switch ([`SWITCH`], [`COWORK_SWITCH`]) is unset, the data directory is not configured, core does not say the front owns
/// Code (`owner: 'front'`), the facts bridge did not answer or the body does not parse; the caller
/// then relays the start to core's `/internal/chat/cowork` exactly as before, and core answers it
/// (a core that hands Code to the front refuses it there, which is what this replaces).
///
/// [`chat_edge::Verdict::Cowork`]: crate::chat_edge::Verdict::Cowork
pub async fn cowork(
    app: &Arc<App>,
    authn: &server_auth::Identity,
    bytes: &[u8],
) -> Option<Response<Body>> {
    if !app.config.code.executor || !app.config.code.cowork {
        return None;
    }
    let data_dir = app.config.data_dir.clone()?;
    let admin = authn.user.role == server_auth::Role::Admin;
    let user = authn.user.id.as_str().to_owned();
    if !matches!(
        task_routes::code_flag(app, &user, admin).await,
        Some(Facts::Front)
    ) {
        return None;
    }
    let body = wtf16::parse(&String::from_utf8_lossy(bytes)).ok()?;
    let project_id = body.get("projectId").as_str()?.to_string();
    let failed = || {
        cowork_answer(
            StatusCode::INTERNAL_SERVER_ERROR,
            JValue::obj([("error", JValue::Str(COWORK_FAILED.into()))]),
        )
    };
    let record = {
        let (data_dir, user, id) = (data_dir.clone(), user.clone(), project_id.clone());
        let switch = app.config.rust_projects;
        tokio::task::spawn_blocking(move || {
            task_routes::project_record(&data_dir, &user, switch, &id)
        })
        .await
    };
    let record = match record {
        Ok(Ok(Some(r))) => r,
        Ok(Ok(None)) => {
            return Some(cowork_answer(
                StatusCode::NOT_FOUND,
                JValue::obj([("error", JValue::Str("project not found".into()))]),
            ))
        }
        Ok(Err(())) | Err(_) => return Some(failed()),
    };
    let Some(dir) = server_projects::store::user_dir(&data_dir, &user)
        .ok()
        .and_then(|d| d.to_str().map(str::to_string))
    else {
        return Some(failed());
    };
    let start_body = cowork_body(&body);
    Some(
        match start(app, service(app), dir, user, project_id, record, start_body).await {
            Some(Ok(task)) => answer(
                StatusCode::ACCEPTED,
                &JValue::obj([("mode", JValue::Str("cowork".into())), ("task", task)]),
            ),
            Some(Err(e)) => cowork_refused(&e),
            None => failed(),
        },
    )
}

/// index.cjs's `{ repository: body.repository, prompt: body.message }`: both as sent (absent stays
/// absent), every other field of the chat body dropped.
pub fn cowork_body(body: &JValue) -> JValue {
    JValue::obj([
        ("repository", body.get("repository").clone()),
        ("prompt", body.get("message").clone()),
    ])
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const T: &str = "0123abcd-0123-4abc-8def-0123456789ab";

    #[test]
    fn the_actions_are_cores_pattern_less_the_reads() {
        let p = |m: &str, path: &str| parse(m, path);
        assert_eq!(
            p("POST", "/api/projects/qa/code"),
            Some(Action::Start("qa".into()))
        );
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
            assert_eq!(
                p(m, &path),
                Some(Action::NotAllowed("qa".into())),
                "{m} {path}"
            );
        }
        for (m, path) in [
            ("GET", "/api/projects/qa/code".to_string()),
            ("GET", format!("/api/projects/qa/code/{T}")),
            ("POST", "/api/projects//code".to_string()),
            ("POST", "/api/projects/qa/codes".to_string()),
            ("POST", format!("/api/projects/qa/code/{T}/merge")),
            (
                "POST",
                "/api/projects/qa/code/0123ABCD-0123-4abc-8def-0123456789ab/cancel".to_string(),
            ),
            ("POST", format!("/api/projects/qa/code/{T}x")),
            ("POST", "/api/projects/qa/browser".to_string()),
            ("POST", "/api/projects/a/b/code".to_string()),
        ] {
            assert_eq!(p(m, &path), None, "{m} {path}");
        }
    }

    #[test]
    fn the_role_fact_is_the_role_engines_endpoint_or_nothing() {
        let facts =
            |role: &str| wtf16::parse(&format!(r#"{{"engine":null,"role":{role}}}"#)).unwrap();
        for missing in ["null", "7", r#""qa""#, "[]"] {
            assert!(role_endpoint(&facts(missing)).is_none(), "{missing}");
        }
        assert!(role_endpoint(&wtf16::parse(r#"{"engine":null}"#).unwrap()).is_none());
        let e = role_endpoint(&facts(
            r#"{"baseUrl":"http://qa-provider.invalid:8080/v1","apiKey":"qa-key","model":"qa-model","external":false,"jsonSchema":true}"#,
        ))
        .unwrap();
        assert_eq!(
            e.base_url.as_deref(),
            Some("http://qa-provider.invalid:8080/v1")
        );
        assert_eq!(
            (e.api_key.as_deref(), e.model.as_deref()),
            (Some("qa-key"), Some("qa-model"))
        );
        assert!(!e.external && e.machine.is_none());
        assert!(role_engine::supports_json_schema(&e.provider));
        let bare = role_endpoint(&facts(
            r#"{"baseUrl":"","apiKey":null,"external":"yes","jsonSchema":1}"#,
        ))
        .unwrap();
        assert_eq!(
            (bare.base_url, bare.api_key, bare.model),
            (None, None, None)
        );
        assert!(!bare.external && !role_engine::supports_json_schema(&bare.provider));
        let external = role_endpoint(&facts(r#"{"external":true}"#)).unwrap();
        assert!(external.external);
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
            assert!(
                !Settings::from_lookup(&env(&[(SWITCH, v)])).executor,
                "{v:?}"
            );
        }
        let on = Settings::from_lookup(&env(&[
            (SWITCH, "rust"),
            ("CODE_HARNESS_ENDPOINT", "code-sandbox:7000"),
            ("CODE_HARNESS_COMMAND", "opencode"),
            ("CODE_HARNESS_ARGS", " acp  --qa "),
            ("CODE_VERIFY_ENDPOINT", "unix:/qa/verify.sock"),
        ]));
        assert!(on.executor);
        assert_eq!(on.verify_endpoint.as_deref(), Some("unix:/qa/verify.sock"));
        assert!(off.verify_endpoint.is_none());
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

    /// The front as it boots: `env` as the process environment, `settings` in the settings table.
    fn front(env: &[(&str, &str)], settings: &[(&str, &str)]) -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().unwrap();
        let db =
            server_store::rusqlite::Connection::open(server_store::db_path(dir.path())).unwrap();
        db.execute_batch(include_str!(
            "../../../crates/server-store/tests/fixtures/node-schema.sql"
        ))
        .unwrap();
        for (k, v) in settings {
            db.execute("INSERT INTO settings(key, value) VALUES(?1, ?2)", [k, v])
                .unwrap();
        }
        drop(db);
        let data = dir.path().to_string_lossy().into_owned();
        let config = crate::config::Config::from_lookup(|k| match k {
            "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
            "UI_DATA_DIR" => Some(data.clone()),
            _ => env
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string()),
        })
        .unwrap();
        (dir, App::new(config))
    }

    /// What the front offers and reads for one deployment: (codePipeline, FrontPipeline::enabled,
    /// the Planner in the service's preparation list, the merge flag the pipeline reads).
    fn offered(env: &[(&str, &str)], settings: &[(&str, &str)]) -> (bool, bool, bool, bool) {
        use server_features::Feature;
        let (dir, app) = front(env, settings);
        let setup = FrontPipeline {
            app: Arc::downgrade(&app),
            rt: tokio::runtime::Handle::current(),
            verify: Arc::new(code_verify::Adapter::new(None)),
        };
        let tenant = dir.path().join("qa-user");
        std::fs::create_dir_all(&tenant).unwrap();
        let facts = service(&app).facts(tenant.to_str().unwrap()).unwrap();
        let list = facts.get("promptPreparation");
        assert!(
            matches!(list, JValue::Arr(l) if l.len() >= 3),
            "the preparation list"
        );
        let planner = matches!(list, JValue::Arr(l) if l
            .iter()
            .any(|p| p.get("id").as_str() == Some(PLANNER_PREPARATION_ID)));
        (
            feature(&app, Feature::CodePipeline),
            setup.enabled(),
            planner,
            pipeline_flag(&app, Flag::Merge),
        )
    }

    const PLANNER_PREPARATION_ID: &str = code_harness::service::PLANNER_PREPARATION.0;
    const SANDBOX: (&str, &str) = ("CODE_HARNESS_ENDPOINT", "code-sandbox:7000");
    const ALL_ON: [(&str, &str); 3] = [
        ("feature:codeHarness", "true"),
        ("feature:codePipeline", "true"),
        ("feature:codeMerge", "true"),
    ];

    #[tokio::test(flavor = "multi_thread")]
    async fn the_planner_is_offered_with_its_flags_on_in_the_sandbox() {
        assert_eq!(offered(&[SANDBOX], &ALL_ON), (true, true, true, true));
        // The env value wins over the setting, as features.cjs reads it.
        assert_eq!(
            offered(
                &[SANDBOX, ("NOEVIA_FEATURE_CODE_PIPELINE", "true")],
                &[("feature:codeHarness", "true")]
            ),
            (true, true, true, false)
        );
        // The other flags the pipeline reads have no runtime availability.
        let (_dir, app) = front(
            &[],
            &[
                ("feature:executorGuard", "true"),
                ("feature:constrainedPlanDecoding", "true"),
                ("feature:codeHarness", "true"),
            ],
        );
        assert!(pipeline_flag(&app, Flag::Guard));
        assert!(pipeline_flag(&app, Flag::Constrain));
        assert!(feature(&app, server_features::Feature::CodeHarness));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_planner_without_the_sandbox() {
        assert_eq!(offered(&[], &ALL_ON), (false, false, false, false));
        // codeSandboxReason trims: a blank endpoint is no sandbox.
        assert_eq!(
            offered(&[("CODE_HARNESS_ENDPOINT", " \u{feff}\t")], &ALL_ON),
            (false, false, false, false)
        );
        // A command transport (spawn) is not the sandbox either.
        assert_eq!(
            offered(&[("CODE_HARNESS_COMMAND", "opencode")], &ALL_ON),
            (false, false, false, false)
        );
    }

    /// features.cjs `selected` (features.cjs:146-150): the env value, else the typed new row, else
    /// the legacy `feature:astraReview` row when the new one is missing or not typed, else off.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_review_flag_falls_back_to_its_legacy_row() {
        use server_features::Feature;
        const NEW: &str = "feature:plannerReview";
        const OLD: &str = "feature:astraReview";
        const ENV: &str = "NOEVIA_FEATURE_PLANNER_REVIEW";
        let review = |env: &[(&str, &str)], settings: &[(&str, &str)]| {
            let (_dir, app) = front(env, settings);
            feature(&app, Feature::PlannerReview)
        };
        assert!(!review(&[], &[]), "the default is off");
        assert!(review(&[], &[(OLD, "true")]), "only the legacy row");
        assert!(!review(&[], &[(OLD, "false")]));
        assert!(review(&[], &[(NEW, "true")]));
        assert!(
            !review(&[], &[(NEW, "false"), (OLD, "true")]),
            "the new row wins"
        );
        assert!(review(&[], &[(NEW, "true"), (OLD, "false")]));
        // An untyped new row is no value: the legacy row decides.
        assert!(review(&[], &[(NEW, "yes"), (OLD, "true")]));
        assert!(!review(&[], &[(NEW, "TRUE"), (OLD, "false")]));
        assert!(
            !review(&[], &[(NEW, "1"), (OLD, "on")]),
            "an untyped legacy row is no value either"
        );
        // The env value wins over both rows.
        assert!(!review(&[(ENV, "false")], &[(OLD, "true")]));
        assert!(review(&[(ENV, "on")], &[(NEW, "false"), (OLD, "false")]));
        // Only plannerReview has a legacy name: no other flag reads it.
        let (_dir, app) = front(&[], &[(OLD, "true"), ("feature:codeHarness", "true")]);
        assert!(feature(&app, Feature::CodeHarness));
        assert!(feature(&app, Feature::PlannerReview));
        assert!(!feature(&app, Feature::CodePipeline));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_planner_with_its_flags_off() {
        assert_eq!(offered(&[SANDBOX], &[]), (false, false, false, false));
        assert_eq!(
            offered(
                &[SANDBOX],
                &[
                    ("feature:codeHarness", "false"),
                    ("feature:codePipeline", "true")
                ]
            ),
            (true, false, false, false)
        );
        assert_eq!(
            offered(
                &[SANDBOX, ("NOEVIA_FEATURE_CODE_PIPELINE", "false")],
                &ALL_ON
            ),
            (false, false, false, true)
        );
    }
}
