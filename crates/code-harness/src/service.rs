//! core code-service.cjs (`createCodeService`) in Rust: Code mode as a project feature. Start a
//! task in a repository the operator registered, answer its approval cards, cancel it, and say
//! what the panel needs (`facts`). One store per tenant directory, opened (and recovered) once.
//!
//! Approvals live in memory on purpose, as in Node: a decision that outlives the request it belongs
//! to is not a decision, and a restart re-asks.
//!
//! The Planner preparation ([`PLANNER_PREPARATION`]) is offered, as in Node, only when the host
//! gave a pipeline ([`Options::pipeline`]) whose `enabled()` says so (features.codeHarness and
//! features.codePipeline) and Code mode runs in the sandbox; otherwise the list is exactly the
//! three entries and `planner` is an unknown preparation. A Planner start runs crate
//! code-pipeline's driver over [`crate::pipeline_host`]'s adapters, its cancel reaches the
//! pipeline's signal (or, while its workspace is still being claimed and the run has no
//! controller yet, writes `job.cancelled` at once as jobs.cjs `cancel` does, and the run never
//! starts), and its cards are this service's. The run's watcher is started before the pipeline,
//! so a start that cannot have one is refused before anything begins; a host trait that panics
//! inside the start fails the job it created (`Code pipeline could not start.`). When the host cannot build the pipeline
//! for a start ([`Setup::create`] answers `None`, or the lifecycle authority was already claimed
//! for the tenant's directory) the start is refused with Node's [`NOT_SET_UP`] (409).
//!
//! Not here: the Planner review of a direct task (`features.plannerReview`): a direct task is never
//! reviewed, so a deployment with that flag on keeps Code mode with Node.

use crate::executor::{Approvals, Deps, Egress, Engine, Executor, Fail, Jobs, Lease, StartRequest};
use crate::pipeline_host::{to_x, Live, Phase, PipeHarness, PipeJobs, PipeWorkspaces, StartState};
pub use crate::pipeline_host::{Parts, Setup, NOT_SET_UP, PLANNER_PREPARATION};
use crate::session::Ended;
use code_acp::Transport;
use code_pipeline::driver::{self, Pipeline, Signal};
use code_workspace::git::js_trim;
use code_workspace::{Ids, Options as WsOptions, Workspaces};
use js_json::{js_len, to_js_string, wtf16, JValue};
use server_projects::task_journals::{code_view, Job};
use server_projects::task_store::LifecycleAuthority;
use server_projects::task_store::{NewJob, Options as StoreOptions, Refusal, Store};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// `APPROVAL_TIMEOUT_MS`: a card nobody answers is refused after this.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_millis(5 * 60 * 1000);
/// `EXPIRED_MEMORY_MS`: how long a timed-out card is remembered, to tell a late answer why.
const EXPIRED_MEMORY_MS: f64 = 60_000.0;
const RETAIN_MS: f64 = 14.0 * 86_400_000.0;
const MAX_JOBS: usize = 50;
/// code-pipeline.cjs's failure for a run that could not start.
const COULD_NOT_START: &str = "Code pipeline could not start.";

/// `GRANTABLE`: what a task may be granted at all (code-actions.cjs `ACTIONS`).
pub const GRANTABLE: [&str; 7] = [
    "read_repository",
    "edit_file",
    "execute_command",
    "install_dependency",
    "network",
    "delete",
    "git_push",
];
/// `DEFAULT_CAPABILITIES`.
pub const DEFAULT_CAPABILITIES: [&str; 3] = ["read_repository", "edit_file", "execute_command"];
const OFFLINE: [&str; 2] = ["network", "install_dependency"];

/// `PROMPT_PREPARATION`: `(id, label, available, reason)`.
pub const PROMPT_PREPARATION: [(&str, &str, bool, &str); 3] = [
    ("direct", "Direct", true, "Your request goes to the model as you wrote it."),
    ("local", "Local architect", false, "Not offered yet: as an architect the 4B returned 0 of 18 usable execution prompts (list fields came back as strings). Direct stays the default until that changes."),
    ("frontier", "Frontier architect", false, "Not built: it needs a provider, official authentication and the outbound allowlist enforced in code (spec §2)."),
];

/// A repository the operator registered (`CODE_REPOS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub id: String,
    pub path: String,
}

/// `parseRepos`: `name|/path,other|/path2`; an entry whose path is not absolute, does not resolve,
/// or has no `.git` is dropped, and a repeated name keeps its first path.
pub fn parse_repos(raw: &str) -> Vec<Repo> {
    let mut out: Vec<Repo> = Vec::new();
    for entry in raw.split(',').map(js_trim).filter(|e| !e.is_empty()) {
        let mut parts = entry.split('|');
        let name = js_trim(parts.next().unwrap_or(""));
        let location = js_trim(parts.next().unwrap_or(""));
        if name.is_empty() || location.is_empty() || !location.starts_with('/') {
            continue;
        }
        let Ok(resolved) = std::fs::canonicalize(location) else {
            continue;
        };
        let Some(resolved) = resolved.to_str().map(str::to_string) else {
            continue;
        };
        if !Path::new(&resolved).join(".git").exists() {
            continue;
        }
        if !out.iter().any(|r| r.id == name) {
            out.push(Repo {
                id: name.to_string(),
                path: resolved,
            });
        }
    }
    out
}

/// `parseUser`: `CODE_HARNESS_USER=1000:1000`, up to seven digits each.
pub fn parse_user(raw: Option<&str>) -> Option<Ids> {
    let raw = js_trim(raw.unwrap_or(""));
    let (uid, gid) = raw.split_once(':')?;
    let digits = |s: &str| (1..=7).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(uid) || !digits(gid) {
        return None;
    }
    Some(Ids {
        uid: uid.parse().ok()?,
        gid: gid.parse().ok()?,
    })
}

/// One coding harness this deployment runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Harness {
    pub id: String,
    pub label: String,
    pub version: Option<String>,
}

/// `defaultHarnesses`: `CODE_HARNESS_NAME` (default `opencode`) and `CODE_HARNESS_VERSION`.
pub fn default_harnesses(name: Option<&str>, version: Option<&str>) -> Vec<Harness> {
    let named = js_trim(name.filter(|n| !n.is_empty()).unwrap_or("opencode"));
    let id = if named.is_empty() { "opencode" } else { named };
    let label = match id {
        "opencode" => "OpenCode",
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        "pi" => "pi",
        "qwen-code" => "Qwen Code",
        "deepseek" => "DeepSeek Harness",
        other => other,
    };
    vec![Harness {
        id: id.to_string(),
        label: label.to_string(),
        version: version.filter(|v| !v.is_empty()).map(str::to_string),
    }]
}

/// The model manager's maintenance gate (`{ enter(), paused() }`).
pub trait Inference: Send + Sync {
    fn enter(&self) -> Result<Lease, Fail>;
    fn paused(&self) -> bool;
}

/// `createCodeService`'s dependencies.
pub struct Options {
    pub repos: Vec<Repo>,
    pub harnesses: Vec<Harness>,
    /// `sandboxKind === 'sandbox'` (CODE_HARNESS_ENDPOINT set).
    pub sandboxed: bool,
    pub transport: Option<Transport>,
    pub tree_root: Option<String>,
    pub harness_user: Option<Ids>,
    pub egress: Option<Arc<dyn Egress>>,
    pub engine: Arc<dyn Fn() -> Engine + Send + Sync>,
    pub inference: Option<Arc<dyn Inference>>,
    /// features.executorGuard.
    pub guard: Arc<dyn Fn() -> bool + Send + Sync>,
    pub log: Arc<dyn Fn(JValue) + Send + Sync>,
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
    pub timeout: Duration,
    pub base_path: Option<String>,
    pub base_lang: Option<String>,
    /// The job and approval ids (`crypto.randomUUID`).
    pub new_id: Arc<dyn Fn() -> String + Send + Sync>,
    /// This process, for the workspaces' recovery.
    pub epoch: String,
    /// core index.cjs's `pipeline` (`{ enabled(), create(...) }`); `None`: no Planner.
    pub pipeline: Option<Arc<dyn Setup>>,
}

/// The project a request is for: its id and its record (for `sharedContext`).
pub struct Project<'a> {
    pub id: &'a str,
    pub record: &'a JValue,
}

/// What the host resolved for one start: the engine and the gate (else the service's own), and the
/// project's shared context for Code.
#[derive(Default)]
pub struct Resolved {
    pub engine: Option<Engine>,
    pub inference: Option<Arc<dyn Inference>>,
    pub context: String,
    /// Whatever else the host learned for this start (the front: core's engine facts), for its
    /// [`Setup::create`].
    pub facts: Option<JValue>,
}

struct Slot {
    decision: Mutex<Option<String>>,
    settled: Condvar,
}

struct Waiting {
    id: String,
    task_id: String,
    request: JValue,
    slot: Arc<Slot>,
}

/// `pending` and `expired`.
struct Registry {
    pending: Mutex<Vec<Waiting>>,
    expired: Mutex<HashMap<String, f64>>,
    now: Arc<dyn Fn() -> f64 + Send + Sync>,
    new_id: Arc<dyn Fn() -> String + Send + Sync>,
    timeout: Duration,
}

impl Registry {
    /// `finish(decision)` of one card: the first answer settles it, later ones change nothing
    /// (false).
    fn finish(&self, id: &str, decision: &str) -> bool {
        let taken = {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            pending
                .iter()
                .position(|w| w.id == id)
                .map(|i| pending.remove(i))
        };
        let Some(waiting) = taken else {
            return false;
        };
        if decision == "timeout" {
            let now = (self.now)();
            let mut expired = self.expired.lock().unwrap_or_else(PoisonError::into_inner);
            expired.insert(id.to_string(), now + EXPIRED_MEMORY_MS);
            if expired.len() > 200 {
                expired.retain(|_, until| *until > now);
            }
        }
        let mut slot = waiting
            .slot
            .decision
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let settled = slot.is_none();
        if settled {
            *slot = Some(decision.to_string());
        }
        waiting.slot.settled.notify_all();
        settled
    }

    fn abort_task(&self, task_id: &str) {
        let ids: Vec<String> = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|w| w.task_id == task_id)
            .map(|w| w.id.clone())
            .collect();
        for id in ids {
            let _ = self.finish(&id, "aborted");
        }
    }
}

/// `askApproval`: a card with a random id, refused as `timeout` on this process's own clock and as
/// `aborted` once the task ends.
struct Asker(Arc<Registry>);

impl Approvals for Asker {
    fn ask(&self, request: &JValue, ended: &Arc<Ended>) -> String {
        let ended2 = ended.clone();
        self.wait(request, &|abort| ended.on_end(abort), &move || {
            ended2.is_ended()
        })
    }
}

/// The pipeline's cards: refused as `aborted` once the task's signal fires.
impl driver::Approvals for Asker {
    fn ask(&self, request: &JValue, signal: &Signal) -> Result<JValue, driver::Fail> {
        let hook = std::sync::Mutex::new(None);
        let answer = self.wait(
            request,
            &|abort| {
                *hook.lock().unwrap_or_else(PoisonError::into_inner) = signal.on_abort(abort);
            },
            &|| signal.is_aborted(),
        );
        if let Some(id) = hook.into_inner().unwrap_or_else(PoisonError::into_inner) {
            signal.off(id);
        }
        Ok(JValue::Str(answer))
    }
}

type Abort = Box<dyn Fn() + Send + Sync>;

impl Asker {
    /// One card: `on_end` is given the hook that refuses it as `aborted`.
    fn wait(&self, request: &JValue, on_end: &dyn Fn(Abort), ended: &dyn Fn() -> bool) -> String {
        let registry = &self.0;
        let id = (registry.new_id)();
        let slot = Arc::new(Slot {
            decision: Mutex::new(None),
            settled: Condvar::new(),
        });
        registry
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Waiting {
                id: id.clone(),
                task_id: request.get("taskId").as_str().unwrap_or("").to_string(),
                request: request.clone(),
                slot: slot.clone(),
            });
        {
            let registry = registry.clone();
            let id = id.clone();
            on_end(Box::new(move || {
                let _ = registry.finish(&id, "aborted");
            }));
        }
        if ended() {
            let _ = registry.finish(&id, "aborted");
        }
        let deadline = Instant::now() + registry.timeout;
        let mut decision = slot.decision.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(d) = decision.as_ref() {
                return d.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(decision);
                let _ = registry.finish(&id, "timeout");
                return slot
                    .decision
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
                    .unwrap_or_else(|| "timeout".into());
            }
            decision = slot
                .settled
                .wait_timeout(decision, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// jobs.cjs over server-projects' task store.
pub struct StoreJobs(pub Arc<Store>);

fn refused(r: Refusal) -> Fail {
    Fail::new(r.status, r.message)
}

impl Jobs for StoreJobs {
    fn create(&self, project_id: Option<&str>, capabilities: &[String]) -> Result<String, Fail> {
        self.0
            .create(NewJob {
                kind: "code",
                project_id,
                parent_id: None,
                capabilities,
            })
            .map_err(refused)
    }
    fn append(&self, id: &str, kind: &str, data: JValue) -> Result<(), Fail> {
        self.0.append(id, kind, data).map(|_| ()).map_err(refused)
    }
    fn status(&self, id: &str) -> Option<String> {
        self.0
            .get(id)
            .ok()
            .flatten()
            .and_then(|j| j.status.as_str().map(str::to_string))
    }
}

struct Tenant {
    store: Arc<Store>,
    executor: Arc<Executor>,
    workspaces: Arc<Workspaces>,
    /// The lifecycle authority, claimed once when the store opens (with a pipeline only).
    authority: Option<Arc<LifecycleAuthority>>,
    /// The pipeline's live tasks.
    pipelines: Live,
    /// One start at a time, from the one-task check to the job (Node's are one synchronous step).
    starting: Mutex<()>,
}

/// One Code service per process.
pub struct Service {
    options: Options,
    registry: Arc<Registry>,
    stores: Mutex<HashMap<String, Arc<Tenant>>>,
}

fn fail(status: u16, message: &str) -> Fail {
    Fail {
        status: Some(status),
        message: message.to_string(),
        public_message: Some(message.to_string()),
        result: JValue::Undefined,
    }
}

fn s(text: &str) -> JValue {
    JValue::Str(text.to_string())
}

fn strings(list: &[&str]) -> JValue {
    JValue::Arr(list.iter().map(|x| s(x)).collect())
}

/// `String(x || '')`.
fn text_of(v: &JValue) -> String {
    if v.truthy() {
        to_js_string(v).unwrap_or_default()
    } else {
        String::new()
    }
}

/// `/^[a-z0-9.-]+\.[a-z]{2,}$/`.
fn domain_like(d: &str) -> bool {
    let class = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-';
    let Some(dot) = d.rfind('.') else {
        return false;
    };
    let (head, tld) = (&d[..dot], &d[dot + 1..]);
    !head.is_empty()
        && head.chars().all(class)
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_lowercase())
}

/// shared-context.cjs `read(project).code`.
pub fn shares_code(project: &JValue) -> bool {
    project.get("sharedContext").is_object()
        && project.get("sharedContext").get("code").as_bool() == Some(true)
}

impl Service {
    pub fn new(options: Options) -> Arc<Service> {
        let registry = Arc::new(Registry {
            pending: Mutex::new(Vec::new()),
            expired: Mutex::new(HashMap::new()),
            now: options.now.clone(),
            new_id: options.new_id.clone(),
            timeout: options.timeout,
        });
        Arc::new(Service {
            options,
            registry,
            stores: Mutex::new(HashMap::new()),
        })
    }

    /// `storeFor`: the tenant's store, recovered from a previous process the first time.
    fn tenant(&self, dir: &str) -> Result<Arc<Tenant>, Fail> {
        let mut stores = self.stores.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(t) = stores.get(dir) {
            return Ok(t.clone());
        }
        let o = &self.options;
        let now = o.now.clone();
        let new_id = o.new_id.clone();
        let store = Arc::new(Store::with(
            Path::new(dir),
            StoreOptions {
                kinds: Some(vec!["code".into()]),
                durable: false,
                retain_ms: RETAIN_MS,
                max_jobs: MAX_JOBS,
            },
            Box::new(move || now()),
            Box::new(move || new_id()),
        ));
        store.recover(&|_| false).map_err(refused)?;
        let authority = o
            .pipeline
            .as_ref()
            .and_then(|_| store.claim_lifecycle_authority().ok().map(Arc::new));
        let now = o.now.clone();
        let workspaces = Arc::new(Workspaces::with(
            WsOptions {
                dir: dir.to_string(),
                tree_root: o.tree_root.clone(),
                owner: o.harness_user,
                noevia: None,
                mode: None,
                epoch: o.epoch.clone(),
                change_max_buffer: None,
            },
            Box::new(move || now()),
            code_workspace::git::Git::default(),
            Box::new(code_workspace::chown_tree),
        ));
        workspaces
            .recover()
            .map_err(|e| Fail::new(e.status, e.message))?;
        let (engine, log, now, guard, inference) = (
            o.engine.clone(),
            o.log.clone(),
            o.now.clone(),
            o.guard.clone(),
            o.inference.clone(),
        );
        let executor = Executor::new(Deps {
            jobs: Arc::new(StoreJobs(store.clone())),
            workspaces: workspaces.clone(),
            egress: o.egress.clone(),
            approvals: Arc::new(Asker(self.registry.clone())),
            engine: Box::new(move || engine()),
            transport: o.transport.clone(),
            log: Box::new(move |e| log(e)),
            now: Box::new(move || now()),
            inference: inference.map(|gate| {
                Box::new(move || gate.enter()) as Box<dyn Fn() -> Result<Lease, Fail> + Send + Sync>
            }),
            guard: Box::new(move || guard()),
            owner: o.harness_user.map(|i| code_harness_config::Owner {
                uid: i.uid,
                gid: i.gid,
            }),
            base_path: o.base_path.clone(),
            base_lang: o.base_lang.clone(),
        });
        let tenant = Arc::new(Tenant {
            store,
            executor,
            workspaces,
            authority,
            pipelines: Arc::new(Mutex::new(HashMap::new())),
            starting: Mutex::new(()),
        });
        stores.insert(dir.to_string(), tenant.clone());
        Ok(tenant)
    }

    fn network(&self) -> bool {
        self.options.egress.as_ref().is_some_and(|e| e.available())
    }

    /// `pipelineOn()`: a pipeline whose `enabled()` is true, and the sandbox.
    fn pipeline_on(&self) -> bool {
        self.options.sandboxed
            && self.options.pipeline.as_ref().is_some_and(|p| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.enabled()))
                    .unwrap_or(false)
            })
    }

    /// `preparations()`.
    fn offered(&self) -> Vec<(&'static str, &'static str, bool, &'static str)> {
        let mut list = PROMPT_PREPARATION.to_vec();
        if self.pipeline_on() {
            list.push(PLANNER_PREPARATION);
        }
        list
    }

    fn preparations(&self) -> JValue {
        JValue::Arr(
            self.offered()
                .iter()
                .map(|(id, label, available, reason)| {
                    JValue::obj([
                        ("id", s(id)),
                        ("label", s(label)),
                        ("available", JValue::Bool(*available)),
                        ("reason", s(reason)),
                    ])
                })
                .collect(),
        )
    }

    /// `repositories()`.
    pub fn repositories(&self) -> JValue {
        JValue::Arr(
            self.options
                .repos
                .iter()
                .map(|r| JValue::obj([("id", s(&r.id))]))
                .collect(),
        )
    }

    /// `facts(workspace)`: what the task reads need besides the journals. Opening the store
    /// recovers the tenant's tasks first; only this tenant's waiting cards are named.
    pub fn facts(&self, dir: &str) -> Result<JValue, Fail> {
        let tenant = self.tenant(dir)?;
        let mut approvals: Vec<(String, JValue)> = Vec::new();
        let waiting: Vec<(String, String, JValue)> = self
            .registry
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|w| (w.id.clone(), w.task_id.clone(), w.request.clone()))
            .collect();
        for (id, task, request) in waiting {
            if approvals.iter().any(|(k, _)| *k == task) {
                continue;
            }
            let own = matches!(tenant.store.get(&task), Ok(Some(job)) if job.kind.as_str() == Some("code"));
            if own {
                let mut card = vec![("id".to_string(), s(&id))];
                if let JValue::Obj(items) = request {
                    for (k, v) in items {
                        wtf16::set(&mut card, &k, v);
                    }
                }
                approvals.push((task, JValue::Obj(card)));
            }
        }
        let harnesses = JValue::Arr(
            self.options
                .harnesses
                .iter()
                .map(|h| {
                    JValue::obj([
                        ("id", s(&h.id)),
                        ("label", s(&h.label)),
                        ("version", h.version.as_deref().map_or(JValue::Null, s)),
                    ])
                })
                .collect(),
        );
        let state = match &self.options.egress {
            None => "unconfigured",
            Some(e) if e.available() => "ready",
            Some(_) => "unavailable",
        };
        Ok(JValue::obj([
            ("repositories", self.repositories()),
            ("capabilities", strings(&GRANTABLE)),
            ("defaultCapabilities", strings(&DEFAULT_CAPABILITIES)),
            ("harnesses", harnesses),
            ("promptPreparation", self.preparations()),
            ("sandboxed", JValue::Bool(self.options.sandboxed)),
            ("network", JValue::Bool(self.network())),
            ("networkState", s(state)),
            ("approvals", JValue::Obj(approvals)),
        ]))
    }

    /// `owned`: a task of another kind or project is not found, never forbidden.
    fn owned(&self, tenant: &Tenant, project_id: &str, task_id: &str) -> Result<Job, Fail> {
        match tenant.store.get(task_id).map_err(refused)? {
            Some(job)
                if job.kind.as_str() == Some("code")
                    && job.project_id.as_str() == Some(project_id) =>
            {
                Ok(job)
            }
            _ => Err(fail(404, "Task not found")),
        }
    }

    /// `start(workspace, project, body)`: 202's body.
    pub fn start(
        &self,
        dir: &str,
        user_id: Option<&str>,
        project: &Project<'_>,
        resolve: &dyn Fn() -> Result<Resolved, Fail>,
        body: &JValue,
    ) -> Result<JValue, Fail> {
        // `body.repository` of a JSON null is Node's TypeError: the route's 500.
        if matches!(body, JValue::Null) {
            return Err(Fail::new(None, "Cannot read properties of null"));
        }
        let wanted = text_of(body.get("repository"));
        let Some(repo) = self.options.repos.iter().find(|r| r.id == wanted) else {
            return Err(fail(
                400,
                "Choose a repository the operator registered on this server.",
            ));
        };
        let prompt = js_trim(&text_of(body.get("prompt"))).to_string();
        if prompt.is_empty() {
            return Err(fail(400, "Describe the task."));
        }
        if js_len(&prompt) > 8000 {
            return Err(fail(400, "That task description is too long."));
        }
        let asked: Vec<JValue> = match body.get("capabilities") {
            JValue::Arr(a) => a.clone(),
            _ => DEFAULT_CAPABILITIES.iter().map(|c| s(c)).collect(),
        };
        let egress = self.options.egress.is_some();
        let capabilities: Vec<String> = GRANTABLE
            .iter()
            .filter(|c| asked.contains(&s(c)) && (egress || !OFFLINE.contains(c)))
            .map(|c| c.to_string())
            .collect();
        let first = self
            .options
            .harnesses
            .first()
            .map(|h| h.id.clone())
            .unwrap_or_default();
        let harness = match body.get("harness") {
            v if v.truthy() => text_of(v),
            _ => first,
        };
        if !self.options.harnesses.iter().any(|h| h.id == harness) {
            return Err(fail(
                400,
                "That coding harness is not configured on this server.",
            ));
        }
        let preparation = match body.get("promptPreparation") {
            v if v.truthy() => text_of(v),
            _ => "direct".to_string(),
        };
        let offered = self.offered();
        let Some((prep_id, _, available, reason)) =
            offered.iter().find(|(id, ..)| *id == preparation)
        else {
            return Err(fail(400, "Unknown prompt preparation."));
        };
        if !available {
            return Err(fail(409, reason));
        }
        let domains: Vec<String> = match body.get("domains") {
            JValue::Arr(a) => a
                .iter()
                .map(|d| js_trim(&text_of(d)).to_lowercase())
                .filter(|d| domain_like(d))
                .take(20)
                .collect(),
            _ => Vec::new(),
        };
        let tenant = self.tenant(dir)?;
        let _one = tenant
            .starting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.active(&tenant, project.id) {
            return Err(fail(409, "This project already has a task running."));
        }
        // What the host learns for this start (the engine, the gate, the shared context), where
        // Node first reads them: after the checks above.
        let resolved = resolve()?;
        let gate = resolved
            .inference
            .clone()
            .or_else(|| self.options.inference.clone());
        if gate.as_ref().is_some_and(|g| g.paused()) {
            return Err(fail(503, crate::executor::INFERENCE_PAUSED));
        }
        let model = match body.get("model") {
            v if v.truthy() => Some(text_of(v)),
            _ => None,
        };
        let started = if *prep_id == PLANNER_PREPARATION.0 {
            self.start_pipeline(
                &tenant,
                resolved,
                gate,
                driver::Task {
                    project_id: Some(project.id.to_string()),
                    repo_id: repo.id.clone(),
                    repo_path: repo.path.clone(),
                    prompt,
                    capabilities: capabilities.clone(),
                    domains: domains.clone(),
                    harness: Some(harness.clone()),
                    connect: Some(Arc::new(())),
                    model,
                    sandbox_kind: Some("sandbox".into()),
                    context: String::new(),
                    tenant_id: user_id.map(str::to_string),
                    approve_plan: *body.get("approvePlan") == JValue::Bool(true),
                },
            )?
        } else {
            tenant.executor.start(StartRequest {
                project_id: Some(project.id.to_string()),
                repo_path: repo.path.clone(),
                prompt,
                capabilities: capabilities.clone(),
                domains: domains.clone(),
                harness: harness.clone(),
                model,
                sandbox_kind: if self.options.sandboxed {
                    "sandbox".into()
                } else {
                    "spawn".into()
                },
                prompt_preparation: prep_id.to_string(),
                context: resolved.context,
                tenant_id: user_id.map(str::to_string),
                engine: resolved.engine,
                inference: resolved
                    .inference
                    .map(|g| Arc::new(move || g.enter()) as crate::executor::Gate),
            })?
        };
        let mut out = match started {
            JValue::Obj(items) => items,
            _ => Vec::new(),
        };
        let list = |v: &[String]| JValue::Arr(v.iter().map(|x| s(x)).collect());
        for (k, v) in [
            ("repository", s(&repo.id)),
            ("capabilities", list(&capabilities)),
            ("domains", list(&domains)),
            ("harness", s(&harness)),
            ("promptPreparation", s(prep_id)),
            ("sharedContext", JValue::Bool(shares_code(project.record))),
        ] {
            wtf16::set(&mut out, k, v);
        }
        Ok(JValue::Obj(out))
    }

    /// The `PLANNER_PREPARATION` branch: `pipelineRun.start(...)`'s `{ taskId, branch, workspace }`.
    /// The pipeline is built for this start (its engine is this start's); its lease is entered
    /// just before `job.started` and held until its thread ends.
    fn start_pipeline(
        &self,
        tenant: &Arc<Tenant>,
        resolved: Resolved,
        gate: Option<Arc<dyn Inference>>,
        mut task: driver::Task,
    ) -> Result<JValue, Fail> {
        let not_set_up = || fail(409, NOT_SET_UP);
        let (Some(setup), Some(authority)) = (&self.options.pipeline, &tenant.authority) else {
            return Err(not_set_up());
        };
        task.context = resolved.context.clone();
        let start = Arc::new(StartState {
            signal: Signal::new(),
            phase: Mutex::new(Phase::Queued),
            task: Mutex::new(None),
            gate: gate.map(|g| Arc::new(move || g.enter()) as crate::executor::Gate),
            lease: Mutex::new(None),
            log: self.options.log.clone(),
            now: self.options.now.clone(),
        });
        let parts = Parts {
            jobs: Arc::new(PipeJobs {
                store: tenant.store.clone(),
                authority: authority.clone(),
                live: tenant.pipelines.clone(),
                start: start.clone(),
            }),
            workspaces: Arc::new(PipeWorkspaces(tenant.workspaces.clone())),
            harness: Arc::new(PipeHarness::new(
                tenant.executor.clone(),
                resolved.engine.clone(),
            )),
            approvals: Arc::new(Asker(self.registry.clone())),
            tree: tenant.workspaces.clone(),
        };
        let deps = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            setup.create(parts, &resolved)
        }))
        .ok()
        .flatten()
        .ok_or_else(not_set_up)?;
        let live = tenant.pipelines.clone();
        let store = tenant.store.clone();
        let state = start.clone();
        // The task stops being live, and its lease is given back, once its thread has ended (or
        // at once when there is none).
        let finish = move || {
            let id = state
                .task
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(id) = id {
                live.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
            }
            state
                .lease
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
        };
        // The watcher is up before the pipeline begins: when it cannot be, nothing has started
        // (no job, no claim) and the start is refused, so a run never goes on without the lease
        // being held until it ends. It gets the run's thread, or nothing (no thread, a refused
        // or panicked start), and then finishes.
        let (hand, thread_of) = std::sync::mpsc::channel::<std::thread::JoinHandle<()>>();
        let watcher = std::thread::Builder::new()
            .name("code pipeline watcher".into())
            .spawn(move || {
                if let Ok(thread) = thread_of.recv() {
                    let _ = thread.join();
                }
                finish();
            });
        if watcher.is_err() {
            return Err(Fail::new(None, COULD_NOT_START));
        }
        let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Pipeline::new(deps).start(task, start.signal.clone())
        }));
        let started = match started {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(to_x(e)),
            // A host trait panicked inside the start (jobs.create, the claim): the job it created,
            // still queued, fails as jobs.run's catch fails it; the watcher (its sender dropped)
            // takes the task out of `live`.
            Err(_) => {
                let id = start
                    .task
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                if let Some(id) = id {
                    if matches!(store.get(&id), Ok(Some(job)) if job.status.as_str() == Some("queued"))
                    {
                        let _ = store.append(
                            &id,
                            "job.failed",
                            JValue::obj([("error", s(COULD_NOT_START))]),
                        );
                    }
                }
                return Err(Fail::new(None, COULD_NOT_START));
            }
        };
        let out = JValue::obj([
            ("taskId", s(&started.task_id)),
            ("branch", started.branch.clone()),
            ("workspace", started.workspace.clone()),
        ]);
        if let Some(thread) = started.thread {
            // The watcher is waiting on this: it cannot have gone away.
            let _ = hand.send(thread);
        }
        Ok(out)
    }

    /// `jobs.list({ projectId, kind: 'code', active: true }).length`: an unreadable journal counts,
    /// as jobs.cjs lists it whatever the filter.
    fn active(&self, tenant: &Tenant, project_id: &str) -> bool {
        tenant
            .store
            .ids()
            .iter()
            .any(|id| match tenant.store.get(id) {
                Ok(Some(job)) => {
                    job.project_id.as_str() == Some(project_id)
                        && job.kind.as_str() == Some("code")
                        && !matches!(
                            job.status.as_str(),
                            Some("completed" | "failed" | "cancelled" | "interrupted")
                        )
                }
                Ok(None) => false,
                Err(_) => true,
            })
    }

    /// `decide(workspace, project, taskId, decision, approvalId)`.
    pub fn decide(
        &self,
        dir: &str,
        project_id: &str,
        task_id: &str,
        decision: &str,
        approval_id: &str,
    ) -> Result<JValue, Fail> {
        let tenant = self.tenant(dir)?;
        self.owned(&tenant, project_id, task_id)?;
        if !matches!(decision, "approve" | "approve_all" | "deny") {
            return Err(fail(400, "Unknown decision"));
        }
        if approval_id.is_empty() {
            return Err(fail(400, "Which approval is this answer for?"));
        }
        let waiting = self
            .registry
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|w| w.id == approval_id && w.task_id == task_id);
        // Settled by this answer, or by a timeout or cancel that came first: then it is 409 too.
        if !waiting || !self.registry.finish(approval_id, decision) {
            let now = (self.options.now)();
            let was_expired = self
                .registry
                .expired
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(approval_id)
                .is_some_and(|until| *until > now);
            return Err(fail(
                409,
                if was_expired {
                    "This approval expired before anyone answered it."
                } else {
                    "That approval is no longer waiting."
                },
            ));
        }
        Ok(JValue::obj([("ok", JValue::Bool(true))]))
    }

    /// `cancel(workspace, project, taskId)`: every card still waiting is refused, the run is
    /// stopped (or an idle task recorded cancelled), and the task's view answers. A live run's view
    /// is the one before the cancel took effect, as Node's (whose effects all come later).
    pub fn cancel(&self, dir: &str, project_id: &str, task_id: &str) -> Result<JValue, Fail> {
        let tenant = self.tenant(dir)?;
        self.owned(&tenant, project_id, task_id)?;
        let view_now = || -> Result<JValue, Fail> {
            Ok(match tenant.store.get(task_id).map_err(refused)? {
                Some(job) => code_view(&job, &JValue::Null),
                None => JValue::Null,
            })
        };
        let mut before: Option<Result<JValue, Fail>> = None;
        // A pipeline task: its signal (jobs.cjs `cancel` of a live run); the run records the rest.
        let pipeline = tenant
            .pipelines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(task_id)
            .cloned();
        if let Some(start) = pipeline {
            let mut phase = start.phase.lock().unwrap_or_else(PoisonError::into_inner);
            if *phase == Phase::Queued {
                // No controller yet (the workspace is being claimed): jobs.cjs `cancel` writes
                // `job.cancelled` at once, and the run's `job.started` is then refused.
                self.registry.abort_task(task_id);
                match tenant
                    .store
                    .append(task_id, "job.cancelled", JValue::Obj(Vec::new()))
                {
                    Err(e) if !(e.status == Some(409) && e.message == "Job already finished") => {
                        return Err(refused(e));
                    }
                    _ => {}
                }
                *phase = Phase::Cancelled;
                drop(phase);
                return view_now();
            }
            drop(phase);
            before = Some(view_now());
            self.registry.abort_task(task_id);
            start.signal.abort();
            return before.unwrap_or_else(view_now);
        }
        tenant.executor.cancel_with(task_id, &mut || {
            before = Some(view_now());
            self.registry.abort_task(task_id);
        })?;
        self.registry.abort_task(task_id);
        before.unwrap_or_else(view_now)
    }
}
