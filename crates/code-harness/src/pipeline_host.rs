//! The host side of crate code-pipeline's driver (core code-service.cjs's `pipeline.create` and
//! what index.cjs hands it): the driver's traits over this crate's pieces, for the Planner
//! preparation (`features.codePipeline`, the sandbox, and a host that can build the pipeline).
//!
//! - [`PipeJobs`]: jobs.cjs over the tenant's task store and the lifecycle authority claimed once
//!   when the tenant's store opens; the start's inference lease (code-service.cjs `leased`) is
//!   taken where Node's `jobs.run` takes it, just before `job.started`, and held until the
//!   pipeline's thread ends (the service's watcher drops it); a lease the authority ends stops
//!   the task as a cancel would (`code.lease_lost`). [`Phase`] is jobs.cjs's controller: a cancel
//!   before `job.started` writes `job.cancelled` itself and the start is then refused (409).
//! - [`PipeWorkspaces`]: code-workspace's pipeline reads (Node's default limits).
//! - [`PipeHarness`]: the [`Executor`]'s claim (a resume reclaims), one turn with the brief
//!   (`promptPreparation: 'planner'`), and the release, once per handle.
//! - [`PipeVerify`]: code-verify's adapter, with the workspace's `verify_checkout`.
//! - [`EngineRoles`]: role-engine's pin and session, driven on a [`role_engine::Blocking`].
//! - [`PipeHost`]: the flags, engine key, log and clock.
//!
//! The approval cards are the service's own (`approve_plan`, `review_change`): answered through
//! the same decide route, listed in the same facts, refused as `aborted` on a cancel and as
//! `timeout` after [`crate::service::APPROVAL_TIMEOUT`].
//!
//! Named differences from Node:
//! - [`job_value`]: the derived job the driver reads carries `parentId` and `pendingApproval` as
//!   `null` (the Rust fold does not keep them; the pipeline never reads them).
//! - A per-turn `machine` (a Planner pinned on an inference machine) is refused as Node's
//!   `model_mismatch` without a relay. With one (council p6, [`PipeHarness::on_machine`]), the
//!   run's relay lease was opened when the start resolved, so the turn's pinned key is compared
//!   with that lease's key (Node opens the lease here with `expect: machine` and compares it with
//!   the target at that moment; the relay re-checks the target on every request either way).

use crate::executor::{ClaimRequest, Engine, Executor, Fail as XFail, Gate, Lease, RunHandle};
use crate::executor::{StartRequest, INFERENCE_PAUSED};
use code_acp::Cancel;
use code_pipeline::driver::{self, Connect, Fail, JobCtx, Pin, Signal};
use js_json::{wtf16, JValue};
use server_projects::task_journals::Job;
use server_projects::task_store::{LifecycleAuthority, NewJob, Refusal, Store};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// sha256 of the `server/code-service.cjs` (core 0c294a3, whose Planner branch core main keeps) and
/// `server/code-pipeline.cjs` the service fixture was recorded from.
pub const CODE_SERVICE_SHA256: &str =
    "bdb2292fde574cc9a04b8d1b468155e863578096f956a388fa287f63fa6f8b0c";
pub const CODE_PIPELINE_SHA256: &str =
    "18d7e6ce2f3e779345d0789af10c5aa0bd056501f8ac55659ec6c3c3a0b339b5";

/// code-service.cjs `PLANNER_PREPARATION`.
pub const PLANNER_PREPARATION: (&str, &str, bool, &str) = (
    "planner",
    "Planner",
    true,
    "The Planner plans the task, the coding agent carries it out, the operator\u{2019}s tests verify it and the Planner reviews it, at most twice more, before you accept.",
);
/// code-harness.cjs: pinned on a machine, the role is back here.
pub const MOVED_BACK: &str = "The Code role moved back to this machine during this task, and one task runs on one model. Start the task again.";
/// code-machine.cjs `open` with `expect` not the target's key.
pub const MOVED_ELSEWHERE: &str = "The Code role moved to another machine or model during this task, and one task runs on one model. Start the task again.";

/// code-harness.cjs before an Executor turn: `lease` is the run's relay key (`None`: no relay),
/// `pinned` the turn's `machine` option (`Undefined`: not given, no check; `null`: pinned here).
pub fn machine_refusal(lease: Option<&str>, pinned: &JValue) -> Option<&'static str> {
    match (lease, pinned) {
        (_, JValue::Undefined) => None,
        (Some(key), JValue::Str(p)) if p == key => None,
        (Some(_), _) => Some(MOVED_ELSEWHERE),
        (None, p) if p.truthy() => Some(MOVED_BACK),
        (None, _) => None,
    }
}

/// code-service.cjs's refusal when the pipeline cannot be built for this start.
pub const NOT_SET_UP: &str = "The Planner pipeline is not set up on this server.";

fn s(text: &str) -> JValue {
    JValue::Str(text.to_string())
}

pub(crate) fn from_x(e: XFail) -> Fail {
    Fail {
        message: e.message,
        public_message: e.public_message,
        status: e.status,
        result: e.result,
    }
}

pub(crate) fn to_x(e: Fail) -> XFail {
    XFail {
        status: e.status,
        message: e.message,
        public_message: e.public_message,
        result: e.result,
    }
}

fn refused(r: Refusal) -> Fail {
    Fail {
        status: r.status,
        ..Fail::new(r.message)
    }
}

/// A [`Signal`] reaching a code-acp [`Cancel`] (or anything else) while `f` runs.
fn linked<T>(signal: &Signal, hook: impl FnOnce() + Send + 'static, f: impl FnOnce() -> T) -> T {
    let id = signal.on_abort(Box::new(hook));
    let out = f();
    if let Some(id) = id {
        signal.off(id);
    }
    out
}

/// jobs.cjs `get`'s derived job as a JS value.
pub fn job_value(job: &Job) -> JValue {
    let mut v = vec![
        ("id", job.id.clone()),
        ("kind", job.kind.clone()),
        ("projectId", job.project_id.clone()),
        ("parentId", JValue::Null),
        ("capabilities", job.capabilities.clone()),
        ("status", job.status.clone()),
        ("stage", job.stage.clone()),
        ("steps", JValue::Arr(job.steps.clone())),
        ("artifacts", JValue::Arr(job.artifacts.clone())),
        ("plan", job.plan.clone()),
        ("assistantOutput", job.assistant_output.clone()),
        ("checkpoint", job.checkpoint.clone()),
        ("pendingApproval", JValue::Null),
        ("uncertain", JValue::Arr(job.uncertain.clone())),
        ("result", job.result.clone()),
        ("error", job.error.clone()),
        ("createdAt", job.created_at.clone()),
        ("updatedAt", job.updated_at.clone()),
    ];
    match &job.authoritative {
        Some(a) => v.extend([
            ("lifecycle", a.lifecycle.clone()),
            ("revision", a.revision.clone()),
            ("stages", JValue::Arr(a.stages.clone())),
        ]),
        None => v.push(("lifecycle", JValue::Null)),
    }
    if let Some(r) = &job.review {
        v.push(("review", r.clone()));
    }
    JValue::obj(v)
}

/// The tasks the pipeline runs in this process, by id: what a cancel reaches.
pub type Live = Arc<Mutex<HashMap<String, Arc<StartState>>>>;

/// Where a pipeline task is in jobs.cjs `run`: whether it has a controller a cancel aborts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// Created, its workspace being claimed: no controller yet, so jobs.cjs `cancel` writes
    /// `job.cancelled` at once and the later `job.started` is refused (the work never runs).
    #[default]
    Queued,
    /// `job.started` written: a cancel aborts the signal and the run records the rest.
    Running,
    /// Cancelled while queued.
    Cancelled,
}

/// One start's state the job store holds: the task's signal, its lease and the gate it comes from.
pub struct StartState {
    pub signal: Signal,
    /// Held while `job.started` is written and while a queued task's cancel is, so the two never
    /// cross (Node's single thread).
    pub phase: Mutex<Phase>,
    /// The job, once created.
    pub task: Mutex<Option<String>>,
    pub gate: Option<Gate>,
    pub lease: Mutex<Option<Lease>>,
    pub log: Arc<dyn Fn(JValue) + Send + Sync>,
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
}

/// jobs.cjs over the tenant's store, for one start.
pub struct PipeJobs {
    pub store: Arc<Store>,
    pub authority: Arc<LifecycleAuthority>,
    pub live: Live,
    pub start: Arc<StartState>,
}

impl PipeJobs {
    /// code-service.cjs `leased`: the lease before `job.started`; a refusal fails the job.
    fn enter(&self, id: &str) -> Result<(), Fail> {
        let Some(gate) = &self.start.gate else {
            return Ok(());
        };
        match gate() {
            Ok(mut lease) => {
                let (signal, log, now, task) = (
                    self.start.signal.clone(),
                    self.start.log.clone(),
                    self.start.now.clone(),
                    id.to_string(),
                );
                lease.on_lost(Box::new(move || {
                    if !signal.is_aborted() {
                        log(JValue::obj([
                            ("at", JValue::Num(now())),
                            ("taskId", s(&task)),
                            ("event", s("code.lease_lost")),
                        ]));
                        signal.abort();
                    }
                }));
                *self
                    .start
                    .lease
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(lease);
                Ok(())
            }
            Err(e) => {
                let refusal = if e.status == Some(503) {
                    XFail::new(Some(503), INFERENCE_PAUSED)
                } else {
                    e
                };
                let text = refusal
                    .public_message
                    .clone()
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| refusal.message.clone());
                let _ = self.store.append(
                    id,
                    "job.failed",
                    JValue::obj([("error", s(&wtf16::slice(&text, 500)))]),
                );
                Err(from_x(refusal))
            }
        }
    }
}

impl driver::Jobs for PipeJobs {
    fn create(&self, req: &JValue) -> Result<String, Fail> {
        let capabilities: Vec<String> = match req.get("capabilities") {
            JValue::Arr(a) => a
                .iter()
                .filter_map(|c| c.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        let id = self
            .store
            .create(NewJob {
                kind: "code",
                project_id: req.get("projectId").as_str(),
                parent_id: None,
                capabilities: &capabilities,
            })
            .map_err(refused)?;
        // Live from its creation, so a cancel never misses it.
        *self
            .start
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(id.clone());
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone(), self.start.clone());
        Ok(id)
    }
    fn append(&self, id: &str, kind: &str, data: JValue) -> Result<(), Fail> {
        if kind == "job.started" {
            self.enter(id)?;
            let mut phase = self
                .start
                .phase
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if *phase == Phase::Cancelled {
                // jobs.cjs `append` on the cancelled job: its 409, so the work never runs.
                return Err(Fail {
                    status: Some(409),
                    ..Fail::new("Job already finished")
                });
            }
            self.store.append(id, kind, data).map_err(refused)?;
            *phase = Phase::Running;
            return Ok(());
        }
        self.store
            .append(id, kind, data)
            .map(|_| ())
            .map_err(refused)
    }
    fn append_authority(&self, id: &str, kind: &str, data: JValue) -> Result<(), Fail> {
        self.store
            .append_authority(id, kind, data, &self.authority)
            .map(|_| ())
            .map_err(refused)
    }
    fn get(&self, id: &str) -> Result<JValue, Fail> {
        match self.store.get(id).map_err(refused)? {
            Some(job) => Ok(job_value(&job)),
            None => Ok(JValue::Null),
        }
    }
}

fn ws_fail(e: code_workspace::Error) -> Fail {
    Fail {
        status: e.status,
        ..Fail::new(e.message)
    }
}

/// code-workspace.cjs's pipeline reads, with Node's default limits.
pub struct PipeWorkspaces(pub Arc<code_workspace::Workspaces>);

impl driver::Workspaces for PipeWorkspaces {
    fn project_snapshot(&self, task_id: &str) -> Result<JValue, Fail> {
        self.0
            .project_snapshot(task_id, 400, 8 * 1024)
            .map_err(ws_fail)
    }
    fn change(&self, task_id: &str) -> Result<JValue, Fail> {
        self.0.change(task_id, 200).map_err(ws_fail)
    }
    fn branch_tip(&self, task_id: &str) -> Result<JValue, Fail> {
        Ok(self.0.branch_tip(task_id).map_or(JValue::Null, JValue::Str))
    }
    fn merge_preflight(&self, task_id: &str, head_sha: &str) -> Result<JValue, Fail> {
        Ok(self.0.merge_preflight(task_id, head_sha))
    }
    fn merge_verified(&self, task_id: &str, head_sha: &str) -> Result<JValue, Fail> {
        Ok(self.0.merge_verified(task_id, head_sha))
    }
}

fn strings(v: &JValue) -> Vec<String> {
    match v {
        JValue::Arr(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// code-harness.cjs `claimRun` / `runExecutor` / `releaseRun` over the tenant's executor, for one
/// start (its engine is the one the host resolved for it).
pub struct PipeHarness {
    pub executor: Arc<Executor>,
    pub engine: Option<Engine>,
    /// The relay's target key of the run's lease (`None`: this machine's engine).
    pub machine: Option<String>,
    handles: Mutex<HashMap<u64, RunHandle>>,
    next: AtomicU64,
}

impl PipeHarness {
    pub fn new(executor: Arc<Executor>, engine: Option<Engine>) -> PipeHarness {
        PipeHarness {
            executor,
            engine,
            machine: None,
            handles: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }
    /// The run's engine is a relay lease on this target key (a start on an inference machine).
    pub fn on_machine(mut self, key: Option<String>) -> PipeHarness {
        self.machine = key;
        self
    }

    /// code-harness.cjs before a turn: `relay.open({ expect: machine })` on a machine, else a
    /// pinned machine with the role back here is refused. `opts.machine` absent: no check.
    pub fn machine_refusal(&self, pinned: &JValue) -> Option<&'static str> {
        machine_refusal(self.machine.as_deref(), pinned)
    }

    fn run(&self, h: driver::RunHandle) -> Option<RunHandle> {
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&h.0)
            .cloned()
    }
}

impl driver::Harness for PipeHarness {
    fn claim_run(&self, req: &JValue) -> Result<driver::RunHandle, Fail> {
        let task_id = req.get("taskId").as_str().unwrap_or("");
        let (capabilities, domains) = (
            strings(req.get("capabilities")),
            strings(req.get("domains")),
        );
        let resume = req.get("resume") == &JValue::Bool(true);
        let claim = ClaimRequest {
            repo_path: if resume {
                None
            } else {
                Some(req.get("repoPath").as_str().unwrap_or(""))
            },
            harness: req.get("harness").as_str().unwrap_or("opencode"),
            capabilities: &capabilities,
            domains: &domains,
        };
        let run = self.executor.claim_run(task_id, &claim).map_err(from_x)?;
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(n, run);
        Ok(driver::RunHandle(n))
    }
    fn handle(&self, handle: driver::RunHandle) -> JValue {
        self.run(handle)
            .map_or(JValue::Undefined, |r| self.executor.run_facts(&r))
    }
    fn run_executor(
        &self,
        job: &JobCtx,
        handle: driver::RunHandle,
        opts: &JValue,
        _connect: &Connect,
    ) -> Result<(), Fail> {
        let Some(run) = self.run(handle) else {
            return Err(Fail::new("No such run"));
        };
        if let Some(text) = self.machine_refusal(opts.get("machine")) {
            self.executor.release_run(&run);
            return Err(Fail::new(text));
        }
        let text = |k: &str| opts.get(k).as_str().unwrap_or("").to_string();
        let req = StartRequest {
            prompt: text("prompt"),
            capabilities: strings(opts.get("capabilities")),
            domains: strings(opts.get("domains")),
            harness: text("harness"),
            model: opts.get("model").as_str().map(str::to_string),
            sandbox_kind: text("sandboxKind"),
            prompt_preparation: text("promptPreparation"),
            context: text("context"),
            engine: self.engine.clone(),
            ..StartRequest::default()
        };
        let cancel = Cancel::new();
        let c = cancel.clone();
        let guarded = opts.get("guarded") == &JValue::Bool(true);
        linked(
            job.signal(),
            move || c.cancel(),
            || {
                self.executor
                    .run_turn(&run, &req, &text("label"), guarded, &cancel)
            },
        )
        .map(|_| ())
        .map_err(from_x)
    }
    fn release_run(&self, handle: driver::RunHandle) {
        if let Some(run) = self.run(handle) {
            self.executor.release_run(&run);
        }
    }
}

/// code-pipeline-verify.cjs over code-verify, with the workspace naming the commit to verify.
pub struct PipeVerify {
    pub adapter: Arc<code_verify::Adapter>,
    pub workspaces: Arc<code_workspace::Workspaces>,
}

impl driver::Verify for PipeVerify {
    fn available(&self) -> bool {
        self.adapter.available()
    }
    fn run(
        &self,
        req: &JValue,
        emit: &mut dyn FnMut(&str, JValue) -> Result<(), Fail>,
        signal: &Signal,
    ) -> Result<JValue, Fail> {
        let ws = self.workspaces.clone();
        let checkout = move |task: &str, sha: &str| match ws.verify_checkout(task, sha) {
            Ok(v) => Ok(v.get("source").as_str().unwrap_or("").to_string()),
            Err(e) => Err(code_verify::CheckoutError {
                code: e.code.map(str::to_string),
                message: Some(e.message),
            }),
        };
        let cancel = Cancel::new();
        let request = code_verify::Request {
            task_id: req.get("taskId").as_str().unwrap_or(""),
            repo: req.get("repo").as_str().unwrap_or(""),
            head_sha: req.get("headSha").as_str().unwrap_or(""),
            revision: req.get("revision").as_num().unwrap_or(f64::NAN),
            verify_checkout: &checkout,
            cancel: Some(&cancel),
        };
        // Node's emit throws into the verifier; the first failure is kept and thrown after.
        let mut thrown: Option<Fail> = None;
        let c = cancel.clone();
        let outcome = linked(
            signal,
            move || c.cancel(),
            || {
                self.adapter
                    .run(&request, &mut |e: &code_verify::Event<'_>| {
                        if thrown.is_none() {
                            thrown = emit(e.kind(), e.data()).err();
                        }
                    })
            },
        );
        match thrown {
            Some(f) => Err(f),
            None => Ok(outcome.to_json()),
        }
    }
}

/// The pipeline's flags (features.cjs), read when the driver asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    /// features.codeMerge.
    Merge,
    /// features.constrainedPlanDecoding (the audit write-up's and the Planner's).
    Constrain,
    /// features.executorGuard.
    Guard,
}

/// What the service hands the host's pipeline for one start (Node's `create({ jobs, workspaces,
/// harness, askApproval })`), each over the tenant's own store, worktrees, executor and cards.
pub struct Parts {
    pub jobs: Arc<dyn driver::Jobs>,
    pub workspaces: Arc<dyn driver::Workspaces>,
    pub harness: Arc<dyn driver::Harness>,
    pub approvals: Arc<dyn driver::Approvals>,
    /// The tenant's worktrees, for [`PipeVerify`].
    pub tree: Arc<code_workspace::Workspaces>,
}

/// core index.cjs's `pipeline: { enabled, create }`.
pub trait Setup: Send + Sync {
    /// features.codeHarness and features.codePipeline (a panic is false).
    fn enabled(&self) -> bool;
    /// `create(...)` for one start, with what the host resolved for it: the driver's dependencies,
    /// or `None` when this start cannot have a pipeline (refused as [`NOT_SET_UP`]).
    fn create(&self, parts: Parts, resolved: &crate::service::Resolved) -> Option<driver::Deps>;
    /// features.plannerReview (index.cjs's `codeReview.enabled()`), read once per direct start (a
    /// panic is off). Off unless the host says so.
    fn review_enabled(&self) -> bool {
        false
    }
    /// The reviewer of one direct start with the review on: `createPlannerReview` over this
    /// start's Code role engine ([`crate::review_gate::PlannerReview`] on [`EngineRoles`]).
    fn reviewer(
        &self,
        _resolved: &crate::service::Resolved,
    ) -> Arc<dyn crate::review_gate::Reviewer> {
        Arc::new(crate::review_gate::Unconfigured)
    }
}

/// Flags, engine key, log and clock.
pub struct PipeHost {
    pub flag: Arc<dyn Fn(Flag) -> bool + Send + Sync>,
    pub engine_key: Option<String>,
    pub log: Arc<dyn Fn(JValue) + Send + Sync>,
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl driver::Host for PipeHost {
    fn merge(&self) -> bool {
        (self.flag)(Flag::Merge)
    }
    fn constrain(&self) -> bool {
        (self.flag)(Flag::Constrain)
    }
    fn guard(&self) -> bool {
        (self.flag)(Flag::Guard)
    }
    fn plan_constrain(&self) -> bool {
        (self.flag)(Flag::Constrain)
    }
    fn engine_key(&self) -> Option<String> {
        self.engine_key.clone()
    }
    fn log(&self, entry: JValue) {
        (self.log)(entry);
    }
    fn now(&self) -> f64 {
        (self.now)()
    }
}

/// role-engine.cjs `pinModel` over [`role_engine::RoleEngine`], driven on one [`role_engine::Blocking`].
pub struct EngineRoles<D: role_engine::Deps + 'static> {
    pub engine: Arc<role_engine::RoleEngine<D>>,
    pub blocking: Arc<role_engine::Blocking>,
}

/// One pinned session's `call`.
struct EngineSession<D: role_engine::Deps + 'static> {
    session: role_engine::Session<D>,
    blocking: Arc<role_engine::Blocking>,
}

fn role_cancel(signal: &Signal) -> (role_engine::Cancel, Option<u64>) {
    let cancel = role_engine::Cancel::new();
    let c = cancel.clone();
    (cancel, signal.on_abort(Box::new(move || c.cancel())))
}

impl<D: role_engine::Deps + 'static> driver::Roles for EngineRoles<D> {
    fn pin_model(&self, req: &JValue, signal: &Signal) -> Result<Pin, Fail> {
        let (cancel, hook) = role_cancel(signal);
        let pin = role_engine::PinRequest {
            task_id: req.get("taskId").as_str().unwrap_or("").to_string(),
            model: req.get("model").as_str().map(str::to_string),
            thinking: req.get("thinking") == &JValue::Bool(true),
            roles: Some(strings(req.get("roles"))),
            cancel: Some(cancel),
        };
        let pinned = self.blocking.block_on(self.engine.pin_model(pin));
        if let Some(id) = hook {
            signal.off(id);
        }
        Ok(match pinned {
            Ok(session) => Pin::Pinned {
                model: s(&session.model),
                machine: session.machine.as_deref().map_or(JValue::Null, s),
                session: Arc::new(EngineSession {
                    session,
                    blocking: self.blocking.clone(),
                }),
            },
            Err(f) => Pin::Refused {
                reason: s(&f.reason),
            },
        })
    }
}

/// A review alone: `pinModel({ taskId })` (the pipeline's roles), with no signal, as Node's.
impl<D: role_engine::Deps + 'static> crate::review_gate::ReviewRoles for EngineRoles<D> {
    fn pin(&self, task_id: &str) -> Result<Arc<dyn driver::Session>, code_pipeline::RoleFail> {
        let pin = role_engine::PinRequest {
            task_id: task_id.to_string(),
            model: None,
            thinking: false,
            roles: None,
            cancel: None,
        };
        match self.blocking.block_on(self.engine.pin_model(pin)) {
            Ok(session) => Ok(Arc::new(EngineSession {
                session,
                blocking: self.blocking.clone(),
            })),
            Err(f) => Err(code_pipeline::RoleFail {
                code: s(&f.code),
                reason: s(&f.reason),
            }),
        }
    }
}

impl<D: role_engine::Deps + 'static> driver::Session for EngineSession<D> {
    fn call(&self, req: &JValue, signal: &Signal) -> Result<JValue, Fail> {
        let (cancel, hook) = role_cancel(signal);
        let num = |k: &str| req.get(k).as_num();
        let call = role_engine::CallRequest {
            role: req.get("role").as_str().unwrap_or("").to_string(),
            state: req.get("state").clone(),
            instructions: req.get("instructions").as_str().unwrap_or("").to_string(),
            schema: req.get("schema").clone(),
            constrain: req.get("constrain") == &JValue::Bool(true),
            schema_name: req.get("schemaName").as_str().map(str::to_string),
            max_tokens: num("maxTokens"),
            max_bytes: num("maxBytes"),
            cancel: Some(cancel),
        };
        let answer = self.blocking.block_on(self.session.call(call));
        if let Some(id) = hook {
            signal.off(id);
        }
        Ok(match answer {
            Ok(a) => a.to_json(),
            Err(f) => f.to_json(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_executor_turn_s_machine_is_code_harness_s_expect() {
        let key = "qa-m1|http://10.9.0.5:8080|qa-coder";
        let on = |p: &JValue| machine_refusal(Some(key), p);
        let here = |p: &JValue| machine_refusal(None, p);
        // Not given: no check, on a machine or here.
        assert_eq!(on(&JValue::Undefined), None);
        assert_eq!(here(&JValue::Undefined), None);
        // Pinned on the lease's machine: runs.
        assert_eq!(on(&s(key)), None);
        // Pinned elsewhere, or here (`null`) while the lease is on a machine: relay.open's refusal.
        assert_eq!(on(&s("qa-m2|http://10.9.0.6:8080|qa-coder")), Some(MOVED_ELSEWHERE));
        assert_eq!(on(&JValue::Null), Some(MOVED_ELSEWHERE));
        // No relay: a pinned machine is "moved back here"; `null` runs.
        assert_eq!(here(&s(key)), Some(MOVED_BACK));
        assert_eq!(here(&JValue::Null), None);
    }
}
