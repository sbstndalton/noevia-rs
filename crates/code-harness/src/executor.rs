//! core code-harness.cjs `start` / `claimRun` / `runExecutor` / `releaseRun` and jobs.cjs `run`:
//! one Code task, from its job and its claimed workspace through the agent's one turn to the
//! release, in Rust. The task's job journal, its worktree, the egress grant, the approval cards
//! and the clock come from the host ([`Deps`]); the pin files are code-harness-config's, the agent
//! is reached with code-acp, and the rules are the [`Session`]'s.
//!
//! The inference lease (code-service.cjs `leased`) is the host's [`Deps::inference`]: taken
//! before the run starts and held until it ends.
//!
//! Not here (yet): the Planner review gate (`features.plannerReview`), the pipeline
//! (`features.codePipeline`) and a Code role on another inference machine (the relay). A host
//! that has any of them on keeps such tasks with Node.

use crate::meta;
use crate::session::{Ended, Host, Refusal, Session, Task};
use code_acp::{Cancel, Handlers, Transport};
use code_workspace::{Claim, Workspaces};
use js_json::{wtf16, JValue};
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

/// code-service.cjs `INFERENCE_PAUSED`: what a task refused by the maintenance gate says.
pub const INFERENCE_PAUSED: &str =
    "Coding tasks wait while noevia tunes or reconfigures models. Start this task when that finishes.";

/// A refused or failed step: Node's `Error` (`status`, `message`, `publicMessage`, `result`).
#[derive(Debug, Clone, PartialEq)]
pub struct Fail {
    pub status: Option<u16>,
    pub message: String,
    pub public_message: Option<String>,
    /// What the run had made of itself when it failed (`error.result`), or `Undefined`.
    pub result: JValue,
}

impl Fail {
    pub fn new(status: Option<u16>, message: impl Into<String>) -> Fail {
        Fail {
            status,
            message: message.into(),
            public_message: None,
            result: JValue::Undefined,
        }
    }
}

/// The task's job store (jobs.cjs, over server-projects' task_store in the front).
pub trait Jobs: Send + Sync {
    /// `create({ kind: 'code', projectId, capabilities })`: the new job's id.
    fn create(&self, project_id: Option<&str>, capabilities: &[String]) -> Result<String, Fail>;
    fn append(&self, id: &str, kind: &str, data: JValue) -> Result<(), Fail>;
    fn status(&self, id: &str) -> Option<String>;
}

/// The egress controller (the front's code_egress): a grant for named domains, its revoke with
/// the network report, and the report's acknowledgement.
pub trait Egress: Send + Sync {
    fn endpoint(&self) -> Option<String>;
    /// `{ token, … }`.
    fn grant(&self, task_id: &str, domains: &[String]) -> Result<JValue, Fail>;
    /// `{ confirmed, report }`.
    fn revoke(&self, grant: &JValue) -> Result<JValue, Fail>;
    /// `{ confirmed }`.
    fn forget(&self, grant: &JValue, snapshot_id: &str) -> Result<JValue, Fail>;
}

/// Where a person answers (code-service.cjs `askApproval`): `approve`, `approve_all`, `deny`,
/// `timeout`, or `aborted` once `ended` ends.
pub trait Approvals: Send + Sync {
    fn ask(&self, request: &JValue, ended: &Arc<Ended>) -> String;
}

/// The deployment's model endpoint for coding tasks (core index.cjs `engine`).
#[derive(Clone, Default, PartialEq)]
pub struct Engine {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub context_tokens: Option<f64>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model", &self.model)
            .field("context_tokens", &self.context_tokens)
            .finish()
    }
}

/// A lease the host holds for a run; dropping it gives it back.
pub type Lease = Box<dyn Send>;

/// Everything the executor is given.
pub struct Deps {
    pub jobs: Arc<dyn Jobs>,
    pub workspaces: Arc<Workspaces>,
    pub egress: Option<Arc<dyn Egress>>,
    pub approvals: Arc<dyn Approvals>,
    pub engine: Box<dyn Fn() -> Engine + Send + Sync>,
    pub transport: Transport,
    pub log: Box<dyn Fn(JValue) + Send + Sync>,
    pub now: Box<dyn Fn() -> f64 + Send + Sync>,
    /// code-service.cjs `inference.enter()`: the maintenance gate's lease, held for the whole run.
    /// `None`: no gate. A refusal fails the job (a 503 says [`INFERENCE_PAUSED`]) and gives the
    /// claim back.
    pub inference: Option<Box<dyn Fn() -> Result<Lease, Fail> + Send + Sync>>,
    /// features.executorGuard, read once per task.
    pub guard: Box<dyn Fn() -> bool + Send + Sync>,
    /// Who the harness runs as (CODE_HARNESS_USER): the pin files are handed over to it.
    pub owner: Option<code_harness_config::Owner>,
    /// The agent's `PATH` and `LANG` (the server's own).
    pub base_path: Option<String>,
    pub base_lang: Option<String>,
}

/// `start(...)`'s argument.
#[derive(Debug, Clone, Default)]
pub struct StartRequest {
    pub project_id: Option<String>,
    pub repo_path: String,
    pub prompt: String,
    pub capabilities: Vec<String>,
    pub domains: Vec<String>,
    pub harness: String,
    pub model: Option<String>,
    pub sandbox_kind: String,
    pub prompt_preparation: String,
    pub context: String,
    pub tenant_id: Option<String>,
}

/// One Code task executor per workspace.
pub struct Executor {
    deps: Arc<Deps>,
    /// jobs.cjs `controllers`: the runs this process has live.
    live: Mutex<HashMap<String, Arc<Cancel>>>,
}

fn s(text: &str) -> JValue {
    JValue::Str(text.to_string())
}

fn or_null(v: Option<&str>) -> JValue {
    v.map_or(JValue::Null, s)
}

/// `String(x).slice(0, n)` in UTF-16 units.
fn clip(text: &str, n: usize) -> String {
    wtf16::slice(text, n)
}

/// The engine's host name, kept off the proxy (core `engineHost`: `new URL(baseUrl).hostname`,
/// '' when it does not parse).
pub fn engine_host(base: Option<&str>) -> String {
    base.and_then(|b| url::Url::parse(b).ok())
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

/// The task's live handle (`claimRun`'s): its workspace record and network grant.
struct Handle {
    task_id: String,
    workspace: JValue,
    grant: Option<JValue>,
    released: Mutex<Option<JValue>>,
    done: Mutex<bool>,
    cleanup_confirmed: Mutex<bool>,
    network: Mutex<JValue>,
}

struct TaskHost {
    deps: Arc<Deps>,
    task_id: String,
    halt: Arc<Cancel>,
}

impl Host for TaskHost {
    fn event(&self, kind: &str, data: JValue) -> Result<(), String> {
        self.deps
            .jobs
            .append(&self.task_id, kind, data)
            .map_err(|f| f.message)
    }
    fn ask_approval(&self, request: &JValue, ended: &Arc<Ended>) -> String {
        self.deps.approvals.ask(request, ended)
    }
    fn contains(&self, path: &JValue) -> Option<bool> {
        match path.as_str() {
            Some(p) => self
                .deps
                .workspaces
                .contains(&self.task_id, p)
                .ok()
                .flatten(),
            None => self
                .deps
                .workspaces
                .contains(&self.task_id, "")
                .ok()
                .flatten()
                .map(|_| false),
        }
    }
    fn root(&self) -> Option<String> {
        self.deps
            .workspaces
            .get(&self.task_id)
            .ok()
            .and_then(|r| r.get("path").as_str().map(str::to_string))
    }
    fn log(&self, entry: JValue) {
        (self.deps.log)(entry);
    }
    fn now(&self) -> f64 {
        (self.deps.now)()
    }
    fn halt(&self) {
        self.halt.cancel();
    }
}

/// The session as the ACP client's handlers.
struct AgentHandlers(Arc<Session>);

fn acp_refusal(r: Refusal) -> code_acp::Refusal {
    code_acp::Refusal {
        code: r.code,
        message: r.message,
        data: r.data,
    }
}

impl Handlers for AgentHandlers {
    fn request_permission(&self, params: &JValue) -> Result<JValue, code_acp::Refusal> {
        self.0.request_permission(params).map_err(acp_refusal)
    }
    fn read_text_file(&self, params: &JValue) -> Result<JValue, code_acp::Refusal> {
        self.0.read_text_file(params).map_err(acp_refusal)
    }
    fn write_text_file(&self, params: &JValue) -> Result<JValue, code_acp::Refusal> {
        self.0.write_text_file(params).map_err(acp_refusal)
    }
    fn session_update(&self, update: &JValue) {
        // An update is not worth failing a task over.
        let _ = self.0.session_update(update);
    }
}

impl Executor {
    pub fn new(deps: Deps) -> Arc<Executor> {
        Arc::new(Executor {
            deps: Arc::new(deps),
            live: Mutex::new(HashMap::new()),
        })
    }

    /// Whether this process is running `task_id` (jobs.cjs `controllers.has`).
    pub fn is_live(&self, task_id: &str) -> bool {
        self.live
            .lock()
            .map(|l| l.contains_key(task_id))
            .unwrap_or(false)
    }

    /// Stops a live run (it records its own `job.cancelled`); false when there is none.
    pub fn abort(&self, task_id: &str) -> bool {
        let token = self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(task_id)
            .cloned();
        match token {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    /// jobs.cjs `cancel` for a code job: a live run is stopped (it records its own
    /// `job.cancelled`), an unfinished one that is not running is recorded cancelled here, a
    /// finished one is left as it is. Under the live lock, so a run either sees the cancel
    /// before it ends or has already ended.
    pub fn cancel(&self, task_id: &str) -> Result<(), Fail> {
        let live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(t) = live.get(task_id) {
            t.cancel();
            return Ok(());
        }
        match self.deps.jobs.status(task_id).as_deref() {
            None | Some("completed" | "failed" | "cancelled") => Ok(()),
            Some(_) => self
                .deps
                .jobs
                .append(task_id, "job.cancelled", JValue::Obj(Vec::new())),
        }
    }

    fn log(&self, task_id: &str, entry: Vec<(&str, JValue)>) {
        let mut out = vec![
            ("at".to_string(), JValue::Num((self.deps.now)())),
            ("taskId".to_string(), s(task_id)),
        ];
        for (k, v) in entry {
            wtf16::set(&mut out, k, v);
        }
        (self.deps.log)(JValue::Obj(out));
    }

    /// `claimRun`: the workspace and, when the task has the network and named domains, its grant.
    /// Nothing is left held when it fails.
    fn claim(&self, task_id: &str, req: &StartRequest) -> Result<Arc<Handle>, (Fail, bool)> {
        let pinned: Vec<JValue> = code_harness_config::cwd_pin_paths(Some(&req.harness))
            .into_iter()
            .map(s)
            .collect();
        let workspace = self
            .deps
            .workspaces
            .claim(Claim {
                task_id,
                repo_path: &req.repo_path,
                branch: None,
                capabilities: &req.capabilities,
                domains: &req.domains,
                pinned: &pinned,
            })
            .map_err(|e| (Fail::new(e.status, e.message), false))?;
        let mut handle = Handle {
            task_id: task_id.to_string(),
            workspace,
            grant: None,
            released: Mutex::new(None),
            done: Mutex::new(false),
            cleanup_confirmed: Mutex::new(true),
            network: Mutex::new(JValue::Undefined),
        };
        let wants_network = req
            .capabilities
            .iter()
            .any(|c| c == "network" || c == "install_dependency");
        if let Some(egress) = self
            .deps
            .egress
            .as_ref()
            .filter(|_| wants_network && !req.domains.is_empty())
        {
            match egress.grant(task_id, &req.domains) {
                // A controller that grants nothing leaves the task without a proxy.
                Ok(grant) if grant.truthy() => handle.grant = Some(grant),
                Ok(_) => {}
                Err(e) => {
                    let handle = Arc::new(handle);
                    self.release(&handle, None);
                    return Err((e, true));
                }
            }
        }
        Ok(Arc::new(handle))
    }

    /// `releaseRun`: the grant revoked (its report kept in the journal before it is
    /// acknowledged) and the workspace given back, once per handle.
    fn release(&self, handle: &Handle, uncertain_via_ctx: Option<()>) -> JValue {
        let Ok(mut done) = handle.done.lock() else {
            return JValue::Null;
        };
        if *done {
            return handle
                .released
                .lock()
                .map(|r| r.clone().unwrap_or(JValue::Null))
                .unwrap_or(JValue::Null);
        }
        let task = handle.task_id.as_str();
        let _ = uncertain_via_ctx;
        if let (Some(grant), Some(egress)) = (handle.grant.as_ref(), self.deps.egress.as_ref()) {
            let cleanup = egress.revoke(grant).ok();
            let confirmed = cleanup
                .as_ref()
                .is_some_and(|c| *c.get("confirmed") == JValue::Bool(true));
            let mut report = match cleanup.as_ref().map(|c| c.get("report")) {
                Some(r @ JValue::Obj(_)) => match r {
                    JValue::Obj(items) => items.clone(),
                    _ => Vec::new(),
                },
                _ => vec![
                    ("available".to_string(), JValue::Bool(false)),
                    ("state".to_string(), s("uncertain")),
                    ("partial".to_string(), JValue::Bool(true)),
                    ("final".to_string(), JValue::Bool(false)),
                ],
            };
            wtf16::set(&mut report, "cleanupConfirmed", JValue::Bool(confirmed));
            let network = JValue::Obj(report);
            if let Ok(mut c) = handle.cleanup_confirmed.lock() {
                *c = confirmed;
            }
            if let Ok(mut n) = handle.network.lock() {
                *n = network.clone();
            }
            if !confirmed {
                let data = JValue::obj([
                    ("tool", s("network")),
                    ("reason", s("Native egress cleanup could not be confirmed.")),
                ]);
                let _ = self.deps.jobs.append(task, "tool.uncertain", data);
            }
            // The immutable report goes into the journal before the controller may forget it.
            let artifact = JValue::obj([
                ("name", s("network.cleanup")),
                ("kind", s("native-network-report")),
                ("network", network.clone()),
            ]);
            let retained = match self.deps.jobs.append(task, "artifact.created", artifact) {
                Ok(()) => true,
                Err(_) => {
                    self.log(
                        task,
                        vec![
                            ("event", s("code.cleanup_failed")),
                            ("what", s("network report")),
                        ],
                    );
                    false
                }
            };
            let snapshot = network
                .get("snapshotId")
                .as_str()
                .filter(|x| !x.is_empty())
                .map(str::to_string);
            if retained && confirmed && network.get("final").truthy() {
                if let Some(snapshot) = snapshot {
                    let _ = egress.forget(grant, &snapshot);
                }
            }
        }
        let released = match self.deps.workspaces.release(task, false) {
            Ok(r) => r,
            Err(e) => {
                self.log(
                    task,
                    vec![
                        ("event", s("code.cleanup_failed")),
                        ("what", s("workspace")),
                        ("error", s(&e.message)),
                    ],
                );
                JValue::Null
            }
        };
        if let Ok(mut r) = handle.released.lock() {
            *r = Some(released.clone());
        }
        *done = true;
        released
    }

    /// `start`: the task's job and workspace now, its run on a thread of its own. Answers
    /// `{ taskId, branch, workspace }`.
    pub fn start(self: &Arc<Self>, req: StartRequest) -> Result<JValue, Fail> {
        if code_workspace::git::js_trim(&req.prompt).is_empty() {
            return Err(Fail::new(Some(400), "A task needs a prompt"));
        }
        let mut capabilities: Vec<String> = Vec::new();
        for c in &req.capabilities {
            if !capabilities.contains(c) {
                capabilities.push(c.clone());
            }
        }
        let task_id = self
            .deps
            .jobs
            .create(req.project_id.as_deref(), &capabilities)?;
        let handle = match self.claim(&task_id, &req) {
            Ok(h) => h,
            Err((e, grant_failed)) => {
                let data = JValue::obj([("error", s(&clip(&e.message, 500)))]);
                let appended = self.deps.jobs.append(&task_id, "job.failed", data);
                if !grant_failed {
                    appended?;
                }
                return Err(e);
            }
        };
        let guarded = (self.deps.guard)();
        let started = JValue::obj([
            ("taskId", s(&task_id)),
            ("branch", handle.workspace.get("branch").clone()),
            ("workspace", handle.workspace.get("path").clone()),
        ]);
        // code-service's lease, then jobs.run up to its first await: both before start answers,
        // as in Node, so a cancel right after start finds the run live.
        let lease = match self.deps.inference.as_ref().map(|enter| enter()) {
            Some(Err(e)) => {
                let refusal = if e.status == Some(503) {
                    Fail::new(Some(503), INFERENCE_PAUSED)
                } else {
                    e
                };
                let message = refusal
                    .public_message
                    .clone()
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| refusal.message.clone());
                let _ = self.deps.jobs.append(
                    &task_id,
                    "job.failed",
                    JValue::obj([("error", s(&clip(&message, 500)))]),
                );
                self.abandon(&task_id, &handle);
                return Ok(started);
            }
            Some(Ok(lease)) => Some(lease),
            None => None,
        };
        let cancel = Cancel::new();
        let began = {
            let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
            // job.started must be on file before the run counts as live.
            !live.contains_key(&task_id)
                && self
                    .deps
                    .jobs
                    .append(&task_id, "job.started", JValue::Obj(Vec::new()))
                    .is_ok()
                && live.insert(task_id.clone(), cancel.clone()).is_none()
        };
        if !began {
            drop(lease);
            self.abandon(&task_id, &handle);
            return Ok(started);
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("code task {task_id}"))
            .spawn({
                let task_id = task_id.clone();
                let handle = handle.clone();
                let cancel = cancel.clone();
                move || {
                    let _lease = lease;
                    me.run(&task_id, &handle, &req, guarded, &cancel);
                }
            });
        if spawned.is_err() {
            // The run never started: the grant and the claim come back, the job says why.
            self.live
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&task_id);
            self.release(&handle, None);
            let _ = self.deps.jobs.append(
                &task_id,
                "job.failed",
                JValue::obj([("error", s("Code task could not start."))]),
            );
        }
        Ok(started)
    }

    /// start's `.catch`: a run that never started gives the grant and the claim back, and a job
    /// still queued says why.
    fn abandon(&self, task_id: &str, handle: &Arc<Handle>) {
        self.release(handle, None);
        if self.deps.jobs.status(task_id).as_deref() == Some("queued") {
            let _ = self.deps.jobs.append(
                task_id,
                "job.failed",
                JValue::obj([("error", s("Code task could not start."))]),
            );
        }
    }

    /// jobs.cjs `run(id, work)` after its start, with `runExecutor` as the work.
    fn run(
        &self,
        task_id: &str,
        handle: &Arc<Handle>,
        req: &StartRequest,
        guarded: bool,
        cancel: &Arc<Cancel>,
    ) {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            self.execute(task_id, handle, req, guarded, cancel)
        }))
        .unwrap_or_else(|_| Err(Fail::new(None, "failed")));
        // A run that panicked never reached its release; it happens once per handle either way.
        self.release(handle, None);
        // Under the live lock, as Node's one thread: a cancel either reaches this run before it
        // ends or finds the job finished.
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        let aborted = cancel.is_cancelled();
        let message = |e: &Fail| {
            let m = e
                .public_message
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| e.message.clone());
            if m.is_empty() {
                "failed".to_string()
            } else {
                m
            }
        };
        // The try: the terminal row for what the work answered.
        let caught = match outcome {
            Ok(result) => {
                let kind = if aborted { "job.cancelled" } else { "job.completed" };
                self.deps
                    .jobs
                    .append(task_id, kind, JValue::obj([("result", result)]))
                    .err()
            }
            Err(e) => Some(e),
        };
        // The catch: a failed work, or a terminal row that could not be written.
        if let Some(e) = caught {
            let _ = if aborted {
                self.deps
                    .jobs
                    .append(task_id, "job.cancelled", JValue::Obj(Vec::new()))
            } else {
                let result = if e.result.is_nullish() {
                    JValue::Null
                } else {
                    e.result.clone()
                };
                self.deps.jobs.append(
                    task_id,
                    "job.failed",
                    JValue::obj([("error", s(&clip(&message(&e), 500))), ("result", result)]),
                )
            };
        }
        live.remove(task_id);
    }

    fn checkpoint(&self, task_id: &str, data: JValue) -> Result<(), Fail> {
        self.deps.jobs.append(task_id, "checkpoint.created", data)
    }

    /// `runExecutor(ctx, handle, …)`: one agent turn in the claimed workspace, released after.
    fn execute(
        &self,
        task_id: &str,
        handle: &Arc<Handle>,
        req: &StartRequest,
        guarded: bool,
        cancel: &Arc<Cancel>,
    ) -> Result<JValue, Fail> {
        let workspace = &handle.workspace;
        let title = clip(&req.prompt, 120);
        let branch = workspace.get("branch").clone();
        let base_sha = match workspace.get("baseSha") {
            JValue::Undefined => JValue::Null,
            v => v.clone(),
        };
        // The agent's own cancel: the task's, and the guard's halt (which is not a cancel of the task).
        let agent_cancel = Cancel::new();
        {
            let agent_cancel = agent_cancel.clone();
            cancel.on_cancel(Box::new(move || agent_cancel.cancel()));
        }
        let mut scope: Option<JValue> = None;
        let mut meta_seen = JValue::Null;
        let mut opened: Option<Arc<Session>> = None;
        let outcome = (|| -> Result<JValue, Fail> {
            let endpoint = (self.deps.engine)();
            let chosen = req
                .model
                .clone()
                .filter(|m| !m.is_empty())
                .or_else(|| endpoint.model.clone().filter(|m| !m.is_empty()));
            let path = workspace.get("path").as_str().unwrap_or("").to_string();
            let session = Arc::new(Session::new(
                Arc::new(TaskHost {
                    deps: self.deps.clone(),
                    task_id: task_id.to_string(),
                    halt: agent_cancel.clone(),
                }),
                Task {
                    task_id: task_id.to_string(),
                    capabilities: req.capabilities.clone(),
                    domains: req.domains.clone(),
                    harness: req.harness.clone(),
                    model: or_null(chosen.as_deref()),
                    workspace_path: path.clone(),
                    guarded,
                },
            ));
            opened = Some(session.clone());
            {
                let session = session.clone();
                cancel.on_cancel(Box::new(move || session.end()));
            }
            let finish = |session: &Session| session.end();
            self.checkpoint(
                task_id,
                JValue::obj([
                    ("branch", branch.clone()),
                    ("task", s(&title)),
                    ("baseSha", base_sha.clone()),
                ]),
            )
            .inspect_err(|_| finish(&session))?;
            self.deps.jobs.append(
                task_id,
                "step.started",
                JValue::obj([
                    ("id", s("harness.config")),
                    ("title", s("Pin the harness configuration")),
                ]),
            )?;
            let home = workspace
                .get("home")
                .as_str()
                .filter(|h| !h.is_empty())
                .map(str::to_string);
            let pin = code_harness_config::WriteRequest {
                cwd: Some(Path::new(&path)),
                home: home.as_deref().map(Path::new),
                owner: self.deps.owner,
                options: code_harness_config::PinOptions {
                    harness: Some(&req.harness),
                    model: chosen.as_deref(),
                    engine: endpoint.base_url.as_deref(),
                    api_key: endpoint.api_key.as_deref(),
                    context_tokens: endpoint.context_tokens.filter(|t| *t != 0.0 && !t.is_nan()),
                    output_tokens: None,
                },
            };
            let pinned = match code_harness_config::write_harness_config(
                &pin,
                &mut code_harness_config::Chown,
            ) {
                Ok(record) => record.to_json(),
                Err(e) => {
                    let message = e.to_string();
                    let _ = self.deps.jobs.append(
                        task_id,
                        "step.completed",
                        JValue::obj([
                            ("id", s("harness.config")),
                            ("failed", JValue::Bool(true)),
                            ("error", s(&clip(&message, 300))),
                        ]),
                    );
                    finish(&session);
                    return Err(Fail::new(e.status(), message));
                }
            };
            let mut completed = vec![("id".to_string(), s("harness.config"))];
            if let JValue::Obj(items) = pinned {
                for (k, v) in items {
                    wtf16::set(&mut completed, &k, v);
                }
            }
            self.deps
                .jobs
                .append(task_id, "step.completed", JValue::Obj(completed))?;
            let proxy = match (&handle.grant, &self.deps.egress) {
                (Some(grant), Some(egress)) => Some(code_acp::Proxy {
                    url: format!(
                        "http://task:{}@{}",
                        js_json::to_js_string(grant.get("token")).unwrap_or_default(),
                        egress
                            .endpoint()
                            .filter(|e| !e.is_empty())
                            .unwrap_or_else(|| "egress".into())
                    ),
                    no_proxy: Some(engine_host(endpoint.base_url.as_deref())),
                }),
                _ => None,
            };
            let mut options = code_acp::Options::new(self.deps.transport.clone(), &path);
            options.home = home.clone();
            options.permission = Some(JValue::obj([
                ("edit", s("ask")),
                ("bash", s("ask")),
                ("webfetch", s("ask")),
            ]));
            options.harness = Some(req.harness.clone());
            options.proxy = proxy;
            options.model = chosen.clone();
            options.base_path = self.deps.base_path.clone();
            options.base_lang = self.deps.base_lang.clone();
            let connected = code_acp::connect(
                options,
                Arc::new(AgentHandlers(session.clone())),
                agent_cancel.clone(),
            );
            let agent = match connected {
                Ok(a) => a,
                Err(e) => {
                    finish(&session);
                    return Err(Fail {
                        status: e.status,
                        message: e.message,
                        public_message: e.public_message,
                        result: JValue::Undefined,
                    });
                }
            };
            if !cancel.is_cancelled() {
                self.deps.jobs.append(
                    task_id,
                    "progress",
                    JValue::obj([("stage", s("running"))]),
                )?;
            }
            let text = if req.context.is_empty() {
                req.prompt.clone()
            } else {
                format!("{}\n\nTask:\n{}", req.context, req.prompt)
            };
            let prompted = agent.prompt(&text);
            let _ = session.flush_output();
            let outcome = match prompted {
                Ok(o) => o,
                Err(e) => {
                    finish(&session);
                    if let Some(blocked) = session.blocked() {
                        return Err(Fail {
                            status: None,
                            message: blocked.message,
                            public_message: Some(blocked.public_message),
                            result: blocked.result,
                        });
                    }
                    return Err(Fail {
                        status: e.status,
                        message: e.message,
                        public_message: e.public_message,
                        result: JValue::Undefined,
                    });
                }
            };
            if let Some(blocked) = session.blocked() {
                finish(&session);
                return Err(Fail {
                    status: None,
                    message: blocked.message,
                    public_message: Some(blocked.public_message),
                    result: blocked.result,
                });
            }
            let meta = session.meta(agent.agent(), &meta::read_usage(outcome.get("_meta")));
            meta_seen = meta.clone();
            let identity = meta::coding_identity(&JValue::obj([
                (
                    "harness",
                    if meta.get("harness").truthy() {
                        meta.get("harness").clone()
                    } else {
                        s(&req.harness)
                    },
                ),
                ("harnessVersion", meta.get("harnessVersion").clone()),
                ("model", or_null(chosen.as_deref())),
                ("protocolVersion", meta.get("protocolVersion").clone()),
                (
                    "capabilities",
                    JValue::Arr(req.capabilities.iter().map(|c| s(c)).collect()),
                ),
                ("promptPreparation", s(&req.prompt_preparation)),
                ("sandbox", s(&req.sandbox_kind)),
            ]))
            .map_err(|_| Fail::new(None, "The run's identity could not be read."))?;
            scope = Some(identity.clone());
            let head = self.deps.workspaces.head_sha(task_id).ok().flatten();
            self.checkpoint(
                task_id,
                JValue::obj([
                    ("branch", branch.clone()),
                    ("task", s(&title)),
                    ("identityHash", identity.get("identityHash").clone()),
                    ("identity", identity.get("identity").clone()),
                    ("meta", meta.clone()),
                    ("baseSha", base_sha.clone()),
                    ("headSha", or_null(head.as_deref())),
                ]),
            )?;
            let stop = match outcome.get("stopReason") {
                v if v.truthy() => v.clone(),
                _ => s("end_turn"),
            };
            let mut result = vec![
                ("stopReason".to_string(), stop),
                ("branch".to_string(), branch.clone()),
                (
                    "identityHash".to_string(),
                    identity.get("identityHash").clone(),
                ),
                ("meta".to_string(), meta),
            ];
            if let JValue::Obj(items) = session.summary() {
                for (k, v) in items {
                    wtf16::set(&mut result, &k, v);
                }
            }
            finish(&session);
            Ok(JValue::Obj(result))
        })();
        // Whatever happened, no approval the task raised is left waiting, the task stops being
        // able to reach anything, and its workspace is given back; then the checkpoint is
        // corrected with the head the source repository holds.
        if let Some(session) = &opened {
            session.end();
        }
        let released = self.release(handle, Some(()));
        let mut outcome = outcome;
        if let (Ok(JValue::Obj(items)), Some(_)) = (&mut outcome, handle.grant.as_ref()) {
            let network = handle
                .network
                .lock()
                .map(|n| n.clone())
                .unwrap_or(JValue::Undefined);
            wtf16::set(items, "network", network);
        }
        if let Some(identity) = &scope {
            let corrected = JValue::obj([
                ("branch", branch.clone()),
                ("task", s(&title)),
                ("identityHash", identity.get("identityHash").clone()),
                ("identity", identity.get("identity").clone()),
                ("meta", meta_seen.clone()),
                ("baseSha", base_sha.clone()),
                (
                    "headSha",
                    match released.get("headSha") {
                        JValue::Undefined => JValue::Null,
                        v => v.clone(),
                    },
                ),
            ]);
            if let Err(e) = self.checkpoint(task_id, corrected) {
                self.log(
                    task_id,
                    vec![
                        ("event", s("code.cleanup_failed")),
                        ("what", s("corrected checkpoint")),
                        ("error", s(&e.message)),
                    ],
                );
            }
        }
        let confirmed = handle.cleanup_confirmed.lock().map(|c| *c).unwrap_or(false);
        // Only a run that succeeded is turned into this failure: a failed run keeps its own error
        // (in Node that error is already unwinding past the line).
        match outcome {
            Ok(result) if handle.grant.is_some() && !confirmed => Err(Fail {
                status: None,
                message: "Native network cleanup could not be confirmed.".into(),
                public_message: None,
                result,
            }),
            other => other,
        }
    }
}
