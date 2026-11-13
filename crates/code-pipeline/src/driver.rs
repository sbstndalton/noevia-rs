//! `server/code-pipeline.cjs` `createCodePipeline` → `start` / `drive`, and the part of jobs.cjs
//! `run` the pipeline relies on: one Code task from its job and claimed workspace through plan →
//! implement → verify → review → audit → accept (and merge), at most `max_loops` change rounds,
//! with a deadline on every stage and the job's cancel reaching whatever runs.
//!
//! # Shape
//!
//! Blocking, like code-harness's executor: [`Pipeline::start`] creates the job and claims the
//! workspace on the caller's thread, then runs the stages on a std thread of their own (the
//! returned [`Started::thread`]). Every effect is a trait the host implements ([`Deps`]):
//! [`Jobs`] (the job store; `append_authority` is the lifecycle-authority append), [`Workspaces`],
//! [`Harness`] (claim, executor run, release, and the run handle's facts), [`Roles`] (the model
//! pin and its [`Session`]), [`Verify`], [`Approvals`] and [`Host`] (flags, engine key, log,
//! clock). Values cross the traits as JS values ([`JValue`]) in Node's shapes and key order, so a
//! host passes them on unchanged; the trait docs name the shapes.
//!
//! A [`Signal`] stands for an `AbortSignal`: the job's own (the caller's, cancelled by the host)
//! and, per stage, one that fires on the job's cancel OR the stage's deadline ([`Deadline`]).
//! Every trait method that can wait takes the signal it must stop on (the Executor: its
//! [`JobCtx::signal`]).
//!
//! jobs.cjs `run` is mirrored here: `job.started` is written by [`Pipeline::start`] before it
//! returns; then `job.completed {result}` (or `job.cancelled {result}`), and when the work threw or
//! that row could not be written, `job.failed {error, result}` (or `job.cancelled`); when that
//! fails too, start's catch gives the live claim back (and fails a job still queued). All through
//! [`Jobs::append`], so the journal is the one Node writes. A panic in any host trait is caught: the
//! live claim is released and the job fails with "failed" (the Executor's precedent); a flag reader
//! or the engine key that panics answers false / none, as Node's `flag()` and `credentials()` do
//! for a throw. The Planner's plan and the review are [`crate::sessions`]' in-session calls over
//! the pinned session; the audit write-up is a session call too.
//!
//! Lifecycle authority: `task.stage` / `task.revision` go only through
//! [`Jobs::append_authority`], which nothing outside this module can reach. A harness or the
//! verifier gets a [`JobCtx`] (`event`, `checkpoint`, `artifact`: Node's two-argument closures),
//! whose `event` refuses both kinds with jobs.cjs's 403.
//!
//! The behaviour is pinned by `tests/fixtures/code-pipeline-driver.v1.json`, recorded from Node's
//! createCodePipeline with the real jobs.cjs, createPlannerPlan and createPlannerReview over
//! scripted stand-ins by `tools/gen-driver-fixtures.cjs`; `tests/driver.rs` replays every
//! scenario and compares each dependency call (name, arguments, order), the start result, and the
//! journal and job the appends make.
//!
//! # Differences from Node
//!
//! - Node's `abandonOn` lets the pin's promise run on after its signal fires; a blocking call
//!   cannot be left behind, so [`Roles::pin_model`] must return once its signal fires (its answer
//!   is then ignored, as in Node).
//! - [`Harness::release_run`] cannot fail (Node's would replace the task's own error); a panic in
//!   it is caught.
//! - Threads, where Node has one: a deadline counts only when it fired before the stage's work
//!   returned (Node cannot observe a later one), and a cancel is checked again right before
//!   `mergeVerified` (tightening: in Node nothing can run between the check after the card and
//!   the merge). It is not checked between a merge that went through and the `merged` stage: the
//!   base has moved by then, so the stage is recorded and the late cancel lands as
//!   `job.cancelled {result}`, as jobs.cjs records a cancel that comes once the work returned.
//! - Inputs Node takes untyped are typed: the task's fields, a run handle's head must be a string
//!   (any other value is "no change"), a merge base branch that is not a string is "" (not
//!   offered); a job that `buildAuditReport` refuses stops the pipeline with that refusal's text
//!   (a TypeError there is "TypeError"), a plan whose steps or a listed file name Node's
//!   `String()` refuses stops it with a fixed "TypeError: …" text. Untyped answers Node reads
//!   property by property keep V8's TypeError texts: a `null`/`undefined` snapshot, its `files`,
//!   a change, its `files` or one of them, a verifier or audit answer ("Cannot read properties of
//!   null (reading 'status')"), and `files` that are not an array but have a length
//!   ("snapshot.files.join is not a function", "change.files.map is not a function").
//! - The Planner and the reviewer are always this crate's; Node's "planner or review missing"
//!   check is the role engine's absence alone (`roles: None`).
//! - The network list's 32-entry cap cannot be reached through jobs.cjs (its 64 `task.stage`
//!   rows end a task after 21 rounds); it is kept and unit-tested.
//!
//! Strings are js_json's WTF-16 form (a lone surrogate held as its mapped character) throughout,
//! and every value handed to a trait is a [`JValue`]: a host serialises them with
//! `js_json::wtf16::stringify` only, never a plain UTF-8 JSON writer. A lone surrogate in the
//! task reaches role-context, whose port declines it (`impl_refused`), as Node's does.

use crate::audit::{build_audit_report, is_full_sha, read_audit, AuditInput, AuditReportError};
use crate::brief::{plan_hash, render_executor_brief};
use crate::js::{join_part, obj, s, tpl, trim, with};
use crate::pipeline::{
    accept_card, accept_result, blocked, blocked_result, capability_text, is_accepted, merge_offer,
    merge_refused, merge_turned_off_result, merged_result, merged_stage_reason, next_round,
    plan_card, review_feedback, should_merge, stage_deadline_message, tests_failed_feedback,
    Accept, Deadlines, DEADLINES, EXECUTOR_INSTRUCTIONS, EXPECTED_ARTIFACTS, MAX_LOOPS,
    PIPELINE_META, PLAN_ACTION,
};
use crate::sessions::{
    generate_in_session, project, review_in_session, verdict_value, Refused, DOSSIER_ROLES,
    SHARED_FRAME,
};
use crate::{audit, planner, review};
use js_json::{wtf16, JValue};
use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// sha256 of the `server/jobs.cjs` whose `run` this mirrors.
pub const JOBS_SHA256: &str = "3a04aea9e594428d62cb5dc7f29eb937d322c6b24d83c9b2d9b08211366e5457";
/// sha256 of the `server/role-engine.cjs` whose `SHARED_FRAME` the leak guard is given.
pub const ROLE_ENGINE_SHA256: &str =
    "6227930c6b303787dc158f8cce12579f4e924d7968b15f974eff080f1b48d61f";
/// sha256 of the `server/plan-constrained-decoding.cjs` whose `PLAN_ARTIFACT_SCHEMA` is sent.
pub const PLAN_CONSTRAINED_DECODING_SHA256: &str =
    "5cc023774e794b0e3dce92821915578e24583058d7eea9a5f8c07f02829284be";

// ── Signals ─────────────────────────────────────────────────────────────────────────────────

type Hook = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct SignalInner {
    aborted: Mutex<bool>,
    woken: Condvar,
    /// The next hook id, and the hooks still waiting.
    hooks: Mutex<(u64, Vec<(u64, Hook)>)>,
}

/// An `AbortSignal`: aborted once, for good; hooks run once when it is.
#[derive(Clone, Default)]
pub struct Signal(Arc<SignalInner>);

impl Signal {
    pub fn new() -> Signal {
        Signal::default()
    }
    pub fn is_aborted(&self) -> bool {
        *self
            .0
            .aborted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
    pub fn abort(&self) {
        {
            let mut a = self
                .0
                .aborted
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if *a {
                return;
            }
            *a = true;
        }
        self.0.woken.notify_all();
        let hooks = std::mem::take(
            &mut self
                .0
                .hooks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .1,
        );
        for (_, hook) in hooks {
            hook();
        }
    }
    /// Runs `hook` once when this aborts (at once if it has). The id takes it off again
    /// ([`Signal::off`]); `None` when it already ran.
    pub fn on_abort(&self, hook: Hook) -> Option<u64> {
        let mut hooks = self.0.hooks.lock().unwrap_or_else(PoisonError::into_inner);
        if self.is_aborted() {
            drop(hooks);
            hook();
            None
        } else {
            let id = hooks.0;
            hooks.0 = id.wrapping_add(1);
            hooks.1.push((id, hook));
            Some(id)
        }
    }
    /// `removeEventListener`: the hook `id` will not run.
    pub fn off(&self, id: u64) {
        let mut hooks = self.0.hooks.lock().unwrap_or_else(PoisonError::into_inner);
        hooks.1.retain(|(h, _)| *h != id);
    }
    /// Waits until this aborts or `timeout` passes; true when it aborted. A timeout past what
    /// [`Instant`] can hold waits for the abort alone.
    pub fn wait(&self, timeout: Duration) -> bool {
        let end = Instant::now().checked_add(timeout);
        let mut a = self
            .0
            .aborted
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while !*a {
            a = match end {
                Some(end) => {
                    let left = end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return false;
                    }
                    match self.0.woken.wait_timeout(a, left) {
                        Ok((g, _)) => g,
                        Err(p) => p.into_inner().0,
                    }
                }
                None => self.0.woken.wait(a).unwrap_or_else(PoisonError::into_inner),
            };
        }
        true
    }
}

/// `setTimeout`'s delay: one past `2^31 - 1` ms, or under 1, is 1 ms (Node's rule).
fn timer_ms(ms: u64) -> u64 {
    if (1..=0x7FFF_FFFF).contains(&ms) {
        ms
    } else {
        1
    }
}

/// Whether the stage's work has returned, and whether the timer fired first.
#[derive(Default)]
struct Fire {
    finished: bool,
    fired: bool,
}

/// One deadline joined to a parent signal (`AbortSignal.any([parent, timer])`): [`Deadline::signal`]
/// fires when the parent does or when `ms` pass (Node's `setTimeout` clamp: over `2^31 - 1` or
/// under 1 is 1 ms). [`Deadline::finish`] marks the work returned: a timer that had not fired by
/// then never counts, as in Node, where nothing runs between an awaited answer and the check.
/// If no timer thread can be started the deadline counts as passed at once (the stage is
/// blocked, never run without a deadline). Dropping it takes its hook off the parent and stops
/// (and joins) the timer thread.
pub struct Deadline {
    pub signal: Signal,
    parent: Signal,
    hook: Option<u64>,
    fire: Arc<Mutex<Fire>>,
    done: Signal,
    thread: Option<JoinHandle<()>>,
}

impl Deadline {
    pub fn start(parent: &Signal, ms: u64) -> Deadline {
        let (signal, done) = (Signal::new(), Signal::new());
        let fire = Arc::new(Mutex::new(Fire::default()));
        let s2 = signal.clone();
        let hook = parent.on_abort(Box::new(move || s2.abort()));
        let (s3, f, d) = (signal.clone(), Arc::clone(&fire), done.clone());
        let ms = timer_ms(ms);
        let thread = std::thread::Builder::new()
            .name("code-pipeline-deadline".into())
            .spawn(move || {
                if !d.wait(Duration::from_millis(ms)) {
                    let mut st = f.lock().unwrap_or_else(PoisonError::into_inner);
                    if !st.finished {
                        st.fired = true;
                        drop(st);
                        s3.abort();
                    }
                }
            })
            .ok();
        if thread.is_none() {
            fire.lock().unwrap_or_else(PoisonError::into_inner).fired = true;
            signal.abort();
        }
        Deadline {
            signal,
            parent: parent.clone(),
            hook,
            fire,
            done,
            thread,
        }
    }
    /// The work returned: from now on the timer cannot fire. True when it already had.
    pub fn finish(&self) -> bool {
        let mut st = self.fire.lock().unwrap_or_else(PoisonError::into_inner);
        st.finished = true;
        st.fired
    }
    /// Whether the timer fired (before [`Deadline::finish`], once that is called).
    pub fn timed_out(&self) -> bool {
        self.fire
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fired
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.finish();
        if let Some(id) = self.hook.take() {
            self.parent.off(id);
        }
        self.done.abort();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ── Dependencies ────────────────────────────────────────────────────────────────────────────

/// A dependency's throw: Node's `Error` (`message`, `publicMessage`, `status`, `result`).
#[derive(Debug, Clone, PartialEq)]
pub struct Fail {
    pub message: String,
    pub public_message: Option<String>,
    pub status: Option<u16>,
    /// `error.result` (`Undefined`: none).
    pub result: JValue,
}

impl Fail {
    pub fn new(message: impl Into<String>) -> Fail {
        Fail {
            message: message.into(),
            public_message: None,
            status: None,
            result: JValue::Undefined,
        }
    }
    /// `String(error?.message || error)`.
    fn text(&self) -> String {
        if self.message.is_empty() {
            "Error".into()
        } else {
            self.message.clone()
        }
    }
    /// `error?.publicMessage || error?.message || error`.
    fn public(&self) -> String {
        match &self.public_message {
            Some(p) if !p.is_empty() => p.clone(),
            _ => self.text(),
        }
    }
    /// jobs.cjs `run`'s `error?.publicMessage || error?.message || 'failed'`.
    fn run_text(&self) -> String {
        match &self.public_message {
            Some(p) if !p.is_empty() => p.clone(),
            _ if self.message.is_empty() => "failed".into(),
            _ => self.message.clone(),
        }
    }
}

/// A host call that may panic, as a throw: the Executor's precedent (code-harness `run`), a panic
/// is the error "failed".
fn guarded<T>(f: impl FnOnce() -> Result<T, Fail>) -> Result<T, Fail> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(Fail::new("failed")))
}

/// Node's `flag(read)`: a reader that throws (here: panics) answers false.
fn flag(read: impl FnOnce() -> bool) -> bool {
    catch_unwind(AssertUnwindSafe(read)).unwrap_or(false)
}

/// `releaseRun(handle)`, which must not take the pipeline down with it.
fn release(d: &Deps, handle: RunHandle) {
    let _ = catch_unwind(AssertUnwindSafe(|| d.harness.release_run(handle)));
}

/// The job store (jobs.cjs over the front's task store).
pub trait Jobs: Send + Sync {
    /// `create({ kind: 'code', projectId, capabilities })` (`req` is that object): the job id.
    fn create(&self, req: &JValue) -> Result<String, Fail>;
    /// `append(id, type, data)`, without the lifecycle authority. The host MUST refuse
    /// `task.stage` and `task.revision` here (jobs.cjs's 403 "Task lifecycle events are
    /// server-only"): only [`Jobs::append_authority`] may write them. The pipeline never asks it
    /// to, and [`JobCtx::event`] refuses them before they reach it.
    fn append(&self, id: &str, kind: &str, data: JValue) -> Result<(), Fail>;
    /// `append(id, type, data, LIFECYCLE)`: `task.stage` / `task.revision`, which only the pipeline
    /// may write; the store validates and rebuilds them (and may refuse, e.g. the completeness gate).
    /// Never reachable from a [`JobCtx`].
    fn append_authority(&self, id: &str, kind: &str, data: JValue) -> Result<(), Fail>;
    /// `get(id)`: the derived job, or `null`.
    fn get(&self, id: &str) -> Result<JValue, Fail>;
}

/// jobs.cjs `AUTHORITY_TYPES`: the kinds [`JobCtx::event`] refuses.
pub const AUTHORITY_TYPES: [&str; 2] = ["task.stage", "task.revision"];

/// The job context a stage hands on (jobs.cjs `run`'s `ctx`, as `{ ...ctx, signal }`): the job id,
/// the signal the holder must stop on, and the two-argument appends. It holds no way to the
/// lifecycle authority: `event` refuses `task.stage` / `task.revision` as jobs.cjs's `append`
/// does without the token.
pub struct JobCtx<'a> {
    id: &'a str,
    jobs: &'a dyn Jobs,
    signal: &'a Signal,
}

impl<'a> JobCtx<'a> {
    fn new(id: &'a str, jobs: &'a dyn Jobs, signal: &'a Signal) -> JobCtx<'a> {
        JobCtx { id, jobs, signal }
    }
    pub fn id(&self) -> &str {
        self.id
    }
    /// The job's cancel, or within a stage the stage's signal (the cancel or its deadline).
    pub fn signal(&self) -> &Signal {
        self.signal
    }
    /// `ctx.event(type, data)`.
    pub fn event(&self, kind: &str, data: JValue) -> Result<(), Fail> {
        if AUTHORITY_TYPES.contains(&kind) {
            return Err(Fail {
                status: Some(403),
                ..Fail::new("Task lifecycle events are server-only")
            });
        }
        self.jobs.append(self.id, kind, data)
    }
    /// `ctx.checkpoint(data)`.
    pub fn checkpoint(&self, data: JValue) -> Result<(), Fail> {
        self.jobs.append(self.id, "checkpoint.created", data)
    }
    /// `ctx.artifact(data)`.
    pub fn artifact(&self, data: JValue) -> Result<(), Fail> {
        self.jobs.append(self.id, "artifact.created", data)
    }
}

/// code-workspace.cjs for one task.
pub trait Workspaces: Send + Sync {
    /// `projectSnapshot(taskId)`: `{ files: [path], truncated, readme: { path, text } | null }`.
    fn project_snapshot(&self, task_id: &str) -> Result<JValue, Fail>;
    /// `change(taskId)`: `{ files: [{ path, … }], … }` (the reviewer gets it whole).
    fn change(&self, task_id: &str) -> Result<JValue, Fail>;
    /// `branchTip(taskId)`: the task branch's tip sha (or `null`).
    fn branch_tip(&self, task_id: &str) -> Result<JValue, Fail>;
    /// `mergePreflight(taskId, { headSha })`: `{ ok, baseBranch }` or `{ ok: false, code, reason }`.
    fn merge_preflight(&self, task_id: &str, head_sha: &str) -> Result<JValue, Fail>;
    /// `mergeVerified(taskId, { headSha })`: `{ ok, baseBranch, recordFailed? }` or
    /// `{ ok: false, code, reason }`.
    fn merge_verified(&self, task_id: &str, head_sha: &str) -> Result<JValue, Fail>;
}

/// A run handle the harness hands out (`claimRun`'s), by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunHandle(pub u64);

/// The task's transport to the coding agent (Node's `connect`), passed through untouched.
pub type Connect = Arc<dyn Any + Send + Sync>;

/// code-harness.cjs `claimRun` / `runExecutor` / `releaseRun`.
pub trait Harness: Send + Sync {
    /// `claimRun(req)`: `{ taskId, repoPath, capabilities, domains, harness }`, or on a later round
    /// `{ taskId, capabilities, domains, harness, resume: true }`.
    fn claim_run(&self, req: &JValue) -> Result<RunHandle, Fail>;
    /// The handle's facts as they stand now: `{ workspace: { branch, path, baseSha }, grant:
    /// { token } | null, network, cleanupConfirmed, released: { status, headSha, error } }`
    /// (`runExecutor` and the release update them).
    fn handle(&self, handle: RunHandle) -> JValue;
    /// `runExecutor({ ...ctx, signal }, handle, opts)`: `job` carries the implement stage's
    /// signal (the job's cancel or the stage deadline), which the run must stop on; `opts` is
    /// Node's without `connect`.
    fn run_executor(
        &self,
        job: &JobCtx,
        handle: RunHandle,
        opts: &JValue,
        connect: &Connect,
    ) -> Result<(), Fail>;
    /// `releaseRun(handle)`: idempotent.
    fn release_run(&self, handle: RunHandle);
}

/// A pinned role-engine session (role-engine.cjs).
pub trait Session: Send + Sync {
    /// `call({ role, state, instructions, schema, schemaName, constrain, maxTokens?, maxBytes?,
    /// signal })` (`req` without the signal): `{ ok, text, corrected, constraint }` or
    /// `{ ok: false, code, reason }`.
    fn call(&self, req: &JValue, signal: &Signal) -> Result<JValue, Fail>;
}

/// `pinModel`'s answer.
pub enum Pin {
    /// `{ ok: true, session: { model, machine } }` (`machine` `Undefined`: not given).
    Pinned {
        model: JValue,
        machine: JValue,
        session: Arc<dyn Session>,
    },
    /// `{ ok: false, reason }`.
    Refused { reason: JValue },
}

/// The role engine.
pub trait Roles: Send + Sync {
    /// `pinModel({ taskId, model, thinking: false, roles })`; must return once `signal` fires.
    fn pin_model(&self, req: &JValue, signal: &Signal) -> Result<Pin, Fail>;
}

/// The verifier (code-pipeline-verify.cjs's adapter).
pub trait Verify: Send + Sync {
    fn available(&self) -> bool;
    /// `run({ taskId, repo, headSha, revision })` (`req`), reporting through `emit(type, data)`:
    /// `{ status: 'passed'|'failed'|'error'|'unavailable', report, error }`.
    fn run(
        &self,
        req: &JValue,
        emit: &mut dyn FnMut(&str, JValue) -> Result<(), Fail>,
        signal: &Signal,
    ) -> Result<JValue, Fail>;
}

/// code-service.cjs `askApproval(request, { signal })`: the person's answer.
pub trait Approvals: Send + Sync {
    fn ask(&self, request: &JValue, signal: &Signal) -> Result<JValue, Fail>;
}

/// Flags, engine key, log and clock. A flag reader that would throw answers false.
pub trait Host: Send + Sync {
    /// features.codeMerge (read at start and again after the card is answered).
    fn merge(&self) -> bool;
    /// The audit write-up's constrained decoding.
    fn constrain(&self) -> bool;
    /// features.executorGuard (false: no guard).
    fn guard(&self) -> bool;
    /// The Planner's features.constrainedPlanDecoding.
    fn plan_constrain(&self) -> bool;
    /// `engineKey()`: the engine credential the leak guard redacts.
    fn engine_key(&self) -> Option<String>;
    fn log(&self, entry: JValue);
    fn now(&self) -> f64;
}

/// Everything the pipeline is given (`createCodePipeline`'s deps).
pub struct Deps {
    pub jobs: Arc<dyn Jobs>,
    pub workspaces: Arc<dyn Workspaces>,
    pub harness: Arc<dyn Harness>,
    /// `None`: no role engine, so no Planner ("not set up on this server").
    pub roles: Option<Arc<dyn Roles>>,
    pub verify: Option<Arc<dyn Verify>>,
    pub approvals: Arc<dyn Approvals>,
    pub host: Arc<dyn Host>,
    pub audit_write_up: bool,
    pub deadlines: Deadlines,
    pub max_loops: u32,
    /// The plan's and the review's own deadlines (their modules' `DEFAULT_DEADLINE_MS`).
    pub plan_deadline_ms: u64,
    pub review_deadline_ms: u64,
}

impl Deps {
    /// Node's defaults for everything but the dependencies.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        jobs: Arc<dyn Jobs>,
        workspaces: Arc<dyn Workspaces>,
        harness: Arc<dyn Harness>,
        roles: Option<Arc<dyn Roles>>,
        verify: Option<Arc<dyn Verify>>,
        approvals: Arc<dyn Approvals>,
        host: Arc<dyn Host>,
    ) -> Deps {
        Deps {
            jobs,
            workspaces,
            harness,
            roles,
            verify,
            approvals,
            host,
            audit_write_up: true,
            deadlines: DEADLINES,
            max_loops: MAX_LOOPS,
            plan_deadline_ms: planner::DEFAULT_DEADLINE_MS,
            review_deadline_ms: review::DEFAULT_DEADLINE_MS,
        }
    }
}

/// `start(task)`'s argument.
#[derive(Clone, Default)]
pub struct Task {
    pub project_id: Option<String>,
    pub repo_id: String,
    pub repo_path: String,
    pub prompt: String,
    pub capabilities: Vec<String>,
    pub domains: Vec<String>,
    /// `None`: `opencode`.
    pub harness: Option<String>,
    /// `None`: no transport (refused, 500).
    pub connect: Option<Connect>,
    pub model: Option<String>,
    /// `None`: `spawn`.
    pub sandbox_kind: Option<String>,
    pub context: String,
    pub tenant_id: Option<String>,
    pub approve_plan: bool,
}

/// What `start` returns once the workspace is claimed; the stages run on `thread`.
pub struct Started {
    pub task_id: String,
    /// The workspace's `branch` and `path`, as the handle has them.
    pub branch: JValue,
    pub workspace: JValue,
    /// `None` when `job.started` could not be written or no thread could be spawned: the claim is
    /// then given back and the job failed (when still queued, for the first).
    pub thread: Option<JoinHandle<()>>,
}

/// The pipeline (`createCodePipeline(deps)`).
pub struct Pipeline {
    deps: Arc<Deps>,
}

fn opt(v: Option<&str>) -> JValue {
    v.map_or(JValue::Null, s)
}

fn strs(v: &[String]) -> JValue {
    JValue::Arr(v.iter().map(|x| s(x)).collect())
}

impl Pipeline {
    pub fn new(deps: Deps) -> Pipeline {
        Pipeline {
            deps: Arc::new(deps),
        }
    }

    /// `start(task)`: creates the job and claims the workspace, then runs the stages on their own
    /// thread under `signal` (the job's cancel, which the host keeps).
    pub fn start(&self, task: Task, signal: Signal) -> Result<Started, Fail> {
        let d = &self.deps;
        if trim(&task.prompt).is_empty() {
            return Err(Fail {
                status: Some(400),
                ..Fail::new("A task needs a prompt")
            });
        }
        let Some(connect) = task.connect.clone() else {
            return Err(Fail {
                status: Some(500),
                ..Fail::new("No harness transport")
            });
        };
        let mut unique: Vec<String> = Vec::new();
        for c in &task.capabilities {
            if !unique.contains(c) {
                unique.push(c.clone());
            }
        }
        let task_id = d.jobs.create(&obj(vec![
            ("kind", s("code")),
            ("projectId", opt(task.project_id.as_deref())),
            ("capabilities", strs(&unique)),
        ]))?;
        let harness_id = harness_id(&task);
        let claim = obj(vec![
            ("taskId", s(&task_id)),
            ("repoPath", s(&task.repo_path)),
            ("capabilities", strs(&task.capabilities)),
            ("domains", strs(&task.domains)),
            ("harness", s(&harness_id)),
        ]);
        let handle = match d.harness.claim_run(&claim) {
            Ok(h) => h,
            Err(e) => {
                let error = wtf16::slice(&e.text(), 500);
                let _ = d
                    .jobs
                    .append(&task_id, "job.failed", obj(vec![("error", s(&error))]));
                return Err(e);
            }
        };
        let settings = Settings {
            merge: flag(|| d.host.merge()),
            guarded: flag(|| d.host.guard()),
        };
        let facts = catch_unwind(AssertUnwindSafe(|| d.harness.handle(handle)))
            .unwrap_or(JValue::Undefined);
        let (branch, workspace) = (
            facts.get("workspace").get("branch").clone(),
            facts.get("workspace").get("path").clone(),
        );
        let out = |thread| {
            Ok(Started {
                task_id: task_id.clone(),
                branch: branch.clone(),
                workspace: workspace.clone(),
                thread,
            })
        };
        // jobs.cjs `run`: `job.started` is written before `start` returns, so a cancel right
        // after it finds the job running. If it cannot be, the work never runs (start's catch).
        let started = guarded(|| {
            d.jobs
                .append(&task_id, "job.started", obj(Vec::<(&str, JValue)>::new()))
        });
        if started.is_err() {
            not_started(d, &task_id, handle);
            return out(None);
        }
        let deps = Arc::clone(&self.deps);
        let id = task_id.clone();
        let thread = std::thread::Builder::new()
            .name("code-pipeline".into())
            .spawn(move || {
                run_job(&deps, &id, &signal, handle, &task, &connect, settings);
            });
        match thread {
            Ok(t) => out(Some(t)),
            Err(_) => {
                release(d, handle);
                let _ = guarded(|| {
                    d.jobs.append(
                        &task_id,
                        "job.failed",
                        obj(vec![("error", s("Code pipeline could not start."))]),
                    )
                });
                out(None)
            }
        }
    }
}

/// `task.harness || 'opencode'`.
fn harness_id(task: &Task) -> String {
    match task.harness.as_deref() {
        Some(h) if !h.is_empty() => h.into(),
        _ => "opencode".into(),
    }
}

/// start's `.catch` on `jobs.run`: the run could not record its start or its end, so give the
/// claim back, and fail the job if it is still queued.
fn not_started(d: &Deps, id: &str, handle: RunHandle) {
    release(d, handle);
    let queued =
        guarded(|| d.jobs.get(id)).is_ok_and(|j| j.get("status").as_str() == Some("queued"));
    if queued {
        let _ = guarded(|| {
            d.jobs.append(
                id,
                "job.failed",
                obj(vec![("error", s("Code pipeline could not start."))]),
            )
        });
    }
}

/// Decided once, at start, for the life of the task.
#[derive(Clone, Copy)]
struct Settings {
    merge: bool,
    guarded: bool,
}

/// jobs.cjs `run(id, work)` around `drive`, after `job.started`: the try writes the terminal row
/// for what the work answered; the catch writes `job.failed` (or `job.cancelled`) for a work that
/// threw or a terminal row that could not be written; if that fails too, start's catch.
/// A panic in a host trait is the work throwing "failed" (the live claim is given back first).
fn run_job(
    d: &Deps,
    id: &str,
    signal: &Signal,
    handle: RunHandle,
    task: &Task,
    connect: &Connect,
    settings: Settings,
) {
    let ctx = JobCtx::new(id, d.jobs.as_ref(), signal);
    let mut run = Run::new(d, &ctx, task, connect, settings, handle);
    let out = match catch_unwind(AssertUnwindSafe(|| run.drive())) {
        Ok(out) => out,
        Err(_) => {
            release(d, run.handle);
            Err(Fail::new("failed"))
        }
    };
    let live = run.handle;
    let aborted = signal.is_aborted();
    let caught = match out {
        Ok(result) => {
            let kind = if aborted {
                "job.cancelled"
            } else {
                "job.completed"
            };
            guarded(|| d.jobs.append(id, kind, obj(vec![("result", result)]))).err()
        }
        Err(e) => Some(e),
    };
    let Some(e) = caught else { return };
    let written = if aborted {
        guarded(|| {
            d.jobs
                .append(id, "job.cancelled", obj(Vec::<(&str, JValue)>::new()))
        })
    } else {
        let result = if e.result.is_nullish() {
            JValue::Null
        } else {
            e.result.clone()
        };
        let error = wtf16::slice(&e.run_text(), 500);
        guarded(|| {
            d.jobs.append(
                id,
                "job.failed",
                obj(vec![("error", s(&error)), ("result", result)]),
            )
        })
    };
    if written.is_err() {
        not_started(d, id, live);
    }
}

// ── drive ───────────────────────────────────────────────────────────────────────────────────

/// How a stage stops the pipeline.
enum Stop {
    /// `Blocked(reason)`, with the result it carries (`Undefined`: none).
    Blocked(String, JValue),
    Cancelled,
    /// Any other throw.
    Raw(Fail),
}

fn block(reason: impl Into<String>) -> Stop {
    Stop::Blocked(reason.into(), JValue::Undefined)
}

fn raw(f: Fail) -> Stop {
    Stop::Raw(f)
}

fn type_error(what: &str) -> Stop {
    Stop::Raw(Fail::new(format!("TypeError: {what}")))
}

/// V8's TypeError message for `<v>.<prop>` on `null` / `undefined`, else the property.
fn read<'v>(v: &'v JValue, prop: &str) -> Result<&'v JValue, Stop> {
    let what = match v {
        JValue::Null => "null",
        JValue::Undefined => "undefined",
        _ => return Ok(v.get(prop)),
    };
    Err(raw(Fail::new(format!(
        "Cannot read properties of {what} (reading '{prop}')"
    ))))
}

/// `v.length` (after [`read`] has ruled out `null` / `undefined`): a string's UTF-16 length, an
/// array's, an object's own `length`, otherwise `undefined`.
fn length(v: &JValue) -> JValue {
    match v {
        JValue::Str(t) => JValue::Num(crate::js::units(t).len() as f64),
        JValue::Arr(a) => JValue::Num(a.len() as f64),
        JValue::Obj(_) => v.get("length").clone(),
        _ => JValue::Undefined,
    }
}

/// V8's `<expr> is not a function`.
fn not_a_function(expr: &str) -> Stop {
    raw(Fail::new(format!("{expr} is not a function")))
}

/// Sets `path` inside `v` (objects only) to `value`.
fn set_in(v: &mut JValue, path: &[&str], value: JValue) {
    let JValue::Obj(items) = v else { return };
    match path {
        [last] => wtf16::set(items, last, value),
        [first, rest @ ..] => {
            if let Some((_, inner)) = items.iter_mut().find(|(k, _)| k == first) {
                set_in(inner, rest, value);
            }
        }
        [] => {}
    }
}

/// `${v}` (a value `String()` refuses as "").
fn text(v: &JValue) -> String {
    tpl(v).unwrap_or_default()
}

/// `{ revision: n, ...network }`.
fn network_entry(n: u32, net: &JValue) -> JValue {
    let items = match net {
        JValue::Obj(items) => items.iter().map(|(k, v)| (k.as_str(), v.clone())).collect(),
        _ => Vec::new(),
    };
    with(&obj(vec![("revision", JValue::from(n))]), items)
}

struct Run<'a> {
    d: &'a Deps,
    ctx: &'a JobCtx<'a>,
    task: &'a Task,
    connect: &'a Connect,
    settings: Settings,
    handle: RunHandle,
    lifecycle: String,
    revision: u32,
    head: Option<String>,
    loops: u32,
    tokens: Vec<JValue>,
    network: Vec<JValue>,
}

impl<'a> Run<'a> {
    fn new(
        d: &'a Deps,
        ctx: &'a JobCtx<'a>,
        task: &'a Task,
        connect: &'a Connect,
        settings: Settings,
        handle: RunHandle,
    ) -> Run<'a> {
        Run {
            d,
            ctx,
            task,
            connect,
            settings,
            handle,
            lifecycle: "planned".into(),
            revision: 0,
            head: None,
            loops: 0,
            tokens: Vec::new(),
            network: Vec::new(),
        }
    }

    fn log(&self, pairs: Vec<(&str, JValue)>) {
        let at = JValue::Num(self.d.host.now());
        let mut entry = vec![("at", at), ("taskId", s(self.ctx.id))];
        entry.extend(pairs);
        self.d.host.log(obj(entry));
    }

    fn stage(&mut self, to: &str, reason: &str, extra: Vec<(&str, JValue)>) -> Result<(), Fail> {
        let mut data = vec![
            ("from", s(&self.lifecycle)),
            ("to", s(to)),
            ("revision", JValue::from(self.revision)),
            ("reason", s(reason)),
        ];
        data.extend(extra);
        self.d
            .jobs
            .append_authority(self.ctx.id, "task.stage", obj(data))?;
        self.lifecycle = to.into();
        self.log(vec![
            ("event", s("code.pipeline_stage")),
            ("to", s(to)),
            ("revision", JValue::from(self.revision)),
        ]);
        Ok(())
    }

    fn live(&self) -> Result<(), Stop> {
        if self.ctx.signal.is_aborted() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    /// `within(name, ms, work)`: the stage's deadline joined to the job's signal.
    fn within<T>(
        &self,
        name: &str,
        ms: u64,
        work: impl FnOnce(&Signal) -> Result<T, Stop>,
    ) -> Result<T, Stop> {
        let deadline = Deadline::start(self.ctx.signal, ms);
        let out = work(&deadline.signal);
        // Whether the deadline fired before the work returned, decided now: one that fires after
        // this cannot block a stage that has finished.
        let timed_out = deadline.finish();
        drop(deadline);
        self.live()?;
        match out {
            Ok(_) if timed_out => Err(block(stage_deadline_message(name, ms))),
            Err(e) if timed_out && !matches!(e, Stop::Blocked(..)) => {
                Err(block(stage_deadline_message(name, ms)))
            }
            other => other,
        }
    }

    fn credentials(&self) -> JValue {
        match catch_unwind(AssertUnwindSafe(|| self.d.host.engine_key())).unwrap_or(None) {
            Some(key) if !key.is_empty() => obj(vec![("engine", s(&key))]),
            _ => JValue::Undefined,
        }
    }

    fn tokens(&self) -> JValue {
        if self.tokens.is_empty() {
            JValue::Undefined
        } else {
            JValue::Arr(self.tokens.clone())
        }
    }

    fn push_network(&mut self, n: u32) {
        let net = self.d.harness.handle(self.handle).get("network").clone();
        if net.truthy() {
            if self.network.len() >= 32 {
                self.network.remove(0);
            }
            self.network.push(network_entry(n, &net));
        }
    }

    /// `drive(ctx, task, state)`: the result, or the job's error (with its `result`).
    fn drive(&mut self) -> Result<JValue, Fail> {
        let out = self.stages();
        let out = match out {
            Ok(r) => Ok(r),
            Err(stop) if self.ctx.signal.is_aborted() || matches!(stop, Stop::Cancelled) => {
                if self.lifecycle != "blocked" && self.lifecycle != "merged" {
                    let _ = self.stage("blocked", "Cancelled", Vec::new());
                }
                Err(match stop {
                    Stop::Blocked(m, _) => Fail {
                        public_message: Some(m.clone()),
                        ..Fail::new(m)
                    },
                    Stop::Raw(f) => f,
                    Stop::Cancelled => Fail {
                        public_message: Some("Cancelled".into()),
                        ..Fail::new("cancelled")
                    },
                })
            }
            Err(stop) => {
                let (message, extra) = match stop {
                    Stop::Blocked(m, r) => (m, r),
                    Stop::Raw(f) => (
                        format!("The pipeline stopped: {}", wtf16::slice(&f.text(), 300)),
                        f.result,
                    ),
                    Stop::Cancelled => ("cancelled".into(), JValue::Undefined),
                };
                if self.lifecycle != "blocked" && self.lifecycle != "merged" {
                    if let Err(e) = self.stage("blocked", &message, Vec::new()) {
                        self.log(vec![
                            ("event", s("code.pipeline_stage_failed")),
                            ("error", s(&wtf16::slice(&e.text(), 200))),
                        ]);
                    }
                }
                self.log(vec![
                    ("event", s("code.pipeline_blocked")),
                    ("revision", JValue::from(self.revision)),
                ]);
                let result = blocked_result(
                    self.revision,
                    self.head.as_deref(),
                    self.loops,
                    &self.network,
                    Some(&extra),
                );
                Err(Fail {
                    public_message: Some(message.clone()),
                    result,
                    ..Fail::new(message)
                })
            }
        };
        self.d.harness.release_run(self.handle);
        out
    }

    #[allow(clippy::too_many_lines)]
    fn stages(&mut self) -> Result<JValue, Stop> {
        let (d, ctx, task) = (self.d, self.ctx, self.task);
        let id = ctx.id;
        let first = d.harness.handle(self.handle).get("workspace").clone();
        let branch = first.get("branch").clone();
        let base_sha = match first.get("baseSha") {
            JValue::Str(b) if is_full_sha(b) => b.clone(),
            _ => return Err(block(blocked::NO_BASE_COMMIT)),
        };
        let harness_id = harness_id(task);
        let tenant = opt(task.tenant_id.as_deref());
        let context = if task.context.is_empty() {
            JValue::Undefined
        } else {
            s(&task.context)
        };
        let capability_list = JValue::Arr(
            task.capabilities
                .iter()
                .map(|c| {
                    obj(vec![
                        ("name", s(c)),
                        (
                            "description",
                            capability_text(c).map_or(JValue::Undefined, s),
                        ),
                    ])
                })
                .collect(),
        );
        let limits = d.deadlines;

        self.stage("planned", "Planning", Vec::new()).map_err(raw)?;
        ctx.checkpoint(obj(vec![
            ("branch", branch.clone()),
            ("task", s(&wtf16::slice(&task.prompt, 120))),
            ("baseSha", s(&base_sha)),
        ]))
        .map_err(raw)?;
        ctx.artifact(obj(vec![
            ("name", s("pipeline")),
            ("kind", s("pipeline")),
            ("version", JValue::from(1u32)),
            ("maxLoops", JValue::from(d.max_loops)),
            ("merge", JValue::Bool(self.settings.merge)),
        ]))
        .map_err(raw)?;

        // ── plan ──
        let Some(roles) = d.roles.as_ref() else {
            return Err(block(blocked::NO_PLANNER));
        };
        let pin_req = obj(vec![
            ("taskId", s(id)),
            ("model", opt(task.model.as_deref())),
            ("thinking", JValue::Bool(false)),
            (
                "roles",
                JValue::Arr(DOSSIER_ROLES.iter().map(|r| s(r)).collect()),
            ),
        ]);
        let pin = self.within("plan", limits.plan, |sig| {
            let r = roles.pin_model(&pin_req, sig);
            if sig.is_aborted() {
                return Err(raw(Fail::new("aborted")));
            }
            r.map_err(raw)
        })?;
        let (session_model, machine, session) = match pin {
            Pin::Pinned {
                model,
                machine,
                session,
            } => (model, machine, session),
            Pin::Refused { reason } => {
                return Err(block(format!(
                    "No model was pinned for this task: {}",
                    text(&reason)
                )))
            }
        };
        let snapshot = d.workspaces.project_snapshot(id).map_err(raw)?;
        let files = read(&snapshot, "files")?;
        read(files, "length")?;
        let mut snippets = Vec::new();
        if length(files).truthy() {
            let JValue::Arr(files) = files else {
                return Err(not_a_function("snapshot.files.join"));
            };
            let mut list = Vec::with_capacity(files.len());
            for f in files {
                list.push(join_part(f).map_err(|_| type_error("a file name"))?);
            }
            let more = if snapshot.get("truncated").truthy() {
                "\n…"
            } else {
                ""
            };
            snippets.push(obj(vec![
                ("source", s("project")),
                ("tenantId", tenant.clone()),
                ("label", s("git ls-files")),
                ("text", s(&format!("{}{more}", list.join("\n")))),
            ]));
        }
        let readme = snapshot.get("readme");
        if readme.truthy() {
            snippets.push(obj(vec![
                ("source", s("project")),
                ("tenantId", tenant.clone()),
                ("label", readme.get("path").clone()),
                ("text", readme.get("text").clone()),
            ]));
        }
        let base = vec![
            ("taskId", s(id)),
            ("tenantId", tenant.clone()),
            ("request", s(&task.prompt)),
            ("credentials", self.credentials()),
        ];
        let state_with = |extra: Vec<(&str, JValue)>| {
            let mut all = base.clone();
            all.extend(extra);
            obj(all)
        };
        ctx.event(
            "step.started",
            obj(vec![
                ("id", s("pipeline.plan")),
                ("title", s("The Planner writes the plan")),
            ]),
        )
        .map_err(raw)?;
        let plan_state = state_with(vec![
            ("revision", JValue::from(0u32)),
            ("projectInstructions", context.clone()),
            ("snippets", JValue::Arr(snippets)),
            ("capabilities", capability_list.clone()),
            (
                "constraints",
                JValue::Arr(vec![
                    s("Change only this repository, on the task branch."),
                    s("Every write, command or network use may need a person’s approval."),
                ]),
            ),
        ]);
        let host = &d.host;
        let log = |e: JValue| host.log(e);
        let planned = self.within("plan", limits.plan, |sig| {
            Ok(generate_in_session(
                session.as_ref(),
                &plan_state,
                sig,
                d.plan_deadline_ms,
                &|| flag(|| host.plan_constrain()),
                &log,
            ))
        })?;
        let plan = match planned {
            Ok(plan) => plan,
            Err(f) => {
                let reason = if f.reason.truthy() {
                    text(&f.reason)
                } else {
                    String::new()
                };
                ctx.event(
                    "step.completed",
                    obj(vec![
                        ("id", s("pipeline.plan")),
                        ("failed", JValue::Bool(true)),
                        ("reason", s(&wtf16::slice(&reason, 300))),
                    ]),
                )
                .map_err(raw)?;
                return Err(block(format!(
                    "The Planner could not plan this task: {}",
                    text(&f.reason)
                )));
            }
        };
        ctx.event("step.completed", obj(vec![("id", s("pipeline.plan"))]))
            .map_err(raw)?;
        let hash = plan_hash(&plan).unwrap_or_default();
        if task.approve_plan {
            let request = plan_card(id, &plan, &hash).map_err(|_| type_error("plan steps"))?;
            ctx.event("approval.requested", request.clone())
                .map_err(raw)?;
            let answer = d.approvals.ask(&request, ctx.signal).map_err(raw)?;
            ctx.event(
                "approval.decided",
                obj(vec![
                    ("decision", answer.clone()),
                    ("action", s(PLAN_ACTION)),
                ]),
            )
            .map_err(raw)?;
            self.live()?;
            if !is_accepted(&answer) {
                return Err(block(blocked::PLAN_NOT_APPROVED));
            }
        }

        // ── implement → verify → review, at most `max_loops` changes ──
        let mut feedback: Vec<String> = Vec::new();
        let mut reason = String::from("Plan ready");
        loop {
            self.stage("implementing", &reason, Vec::new())
                .map_err(raw)?;
            let n = self.revision.saturating_add(1);
            ctx.artifact(obj(vec![
                ("name", s("plan")),
                ("kind", s("plan")),
                ("revision", JValue::from(n)),
                ("planHash", s(&hash)),
                ("plan", plan.clone()),
            ]))
            .map_err(raw)?;
            if n > 1 {
                let facts = d.harness.handle(self.handle);
                if facts.get("cleanupConfirmed") == &JValue::Bool(false) {
                    return Err(block(blocked::CLEANUP_BEFORE_GRANT));
                }
                let req = obj(vec![
                    ("taskId", s(id)),
                    ("capabilities", strs(&task.capabilities)),
                    ("domains", strs(&task.domains)),
                    ("harness", s(&harness_id)),
                    ("resume", JValue::Bool(true)),
                ]);
                self.handle = d.harness.claim_run(&req).map_err(|e| {
                    block(format!(
                        "The task’s branch could not be taken up again: {}",
                        wtf16::slice(&e.text(), 200)
                    ))
                })?;
            }
            let token = d
                .harness
                .handle(self.handle)
                .get("grant")
                .get("token")
                .clone();
            if token.truthy() {
                self.tokens.push(token);
            }
            let fb = if feedback.is_empty() {
                JValue::Undefined
            } else {
                strs(&feedback)
            };
            let executor_state = obj(vec![
                ("taskId", s(id)),
                ("tenantId", tenant.clone()),
                ("revision", JValue::from(n)),
                ("request", s(&task.prompt)),
                ("projectInstructions", context.clone()),
                ("plan", plan.clone()),
                ("capabilities", capability_list.clone()),
                ("feedback", fb),
                ("credentials", self.credentials()),
                ("tokens", self.tokens()),
                (
                    "roleSystemPrompts",
                    obj(vec![
                        ("executor", s(EXECUTOR_INSTRUCTIONS)),
                        ("planner", s(planner::INSTRUCTIONS)),
                        ("reviewer", s(review::INSTRUCTIONS)),
                        ("auditor", s(audit::AUDIT_INSTRUCTIONS)),
                    ]),
                ),
                (
                    "orchestrator",
                    obj(vec![
                        ("metaPrompt", s(PIPELINE_META)),
                        ("systemPrompt", s(SHARED_FRAME)),
                    ]),
                ),
            ]);
            let brief = match project("executor", &executor_state) {
                Ok(p) => render_executor_brief(&p).map_err(|_| Refused::Other("render".into())),
                Err(e) => Err(e),
            };
            let brief = match brief {
                Ok(b) => b,
                Err(e) => {
                    d.harness.release_run(self.handle);
                    return Err(block(if matches!(e, Refused::Leak(_)) {
                        blocked::BRIEF_LEAK
                    } else {
                        blocked::BRIEF_FAILED
                    }));
                }
            };
            let model = if session_model.truthy() {
                session_model.clone()
            } else {
                opt(task.model.as_deref())
            };
            let mut opts = vec![
                ("prompt", s(&brief)),
                ("label", s(&task.prompt)),
                ("model", model),
                ("harness", s(&harness_id)),
                ("capabilities", strs(&task.capabilities)),
                ("domains", strs(&task.domains)),
                (
                    "sandboxKind",
                    s(task.sandbox_kind.as_deref().unwrap_or("spawn")),
                ),
                ("promptPreparation", s("planner")),
                ("context", s("")),
                ("guarded", JValue::Bool(self.settings.guarded)),
            ];
            if machine != JValue::Undefined {
                opts.push(("machine", machine.clone()));
            }
            let opts = obj(opts);
            let handle = self.handle;
            let ran = self.within("implement", limits.implement, |sig| {
                let stage_ctx = JobCtx::new(id, d.jobs.as_ref(), sig);
                d.harness
                    .run_executor(&stage_ctx, handle, &opts, self.connect)
                    .map_err(raw)
            });
            self.push_network(n);
            match ran {
                Ok(()) => {}
                Err(Stop::Raw(f)) => {
                    return Err(block(format!(
                        "The coding agent stopped: {}",
                        wtf16::slice(&f.public(), 300)
                    )))
                }
                Err(e) => return Err(e),
            }
            let facts = d.harness.handle(self.handle);
            if facts.get("cleanupConfirmed") == &JValue::Bool(false) {
                return Err(block(blocked::CLEANUP_AFTER_RUN));
            }
            let released = facts.get("released");
            if !released.truthy() || released.get("status").as_str() != Some("released") {
                let error = released.get("error");
                return Err(block(if error.truthy() {
                    format!(
                        "The workspace was not released cleanly: {}",
                        wtf16::slice(&text(error), 200)
                    )
                } else {
                    "The workspace was not released cleanly.".into()
                }));
            }
            let head = match released.get("headSha").as_str() {
                Some(h) if is_full_sha(h) && h != base_sha => h.to_string(),
                _ => return Err(block(blocked::NO_CHANGE)),
            };
            if self.head.as_deref() == Some(head.as_str()) {
                return Err(block(blocked::NO_FURTHER_CHANGE));
            }
            d.jobs
                .append_authority(
                    id,
                    "task.revision",
                    obj(vec![
                        ("n", JValue::from(n)),
                        ("headSha", s(&head)),
                        ("planHash", s(&hash)),
                    ]),
                )
                .map_err(raw)?;
            self.revision = n;
            self.head = Some(head.clone());
            ctx.artifact(obj(vec![
                ("name", s("revision")),
                ("kind", s("revision")),
                ("revision", JValue::from(n)),
                ("headSha", s(&head)),
                ("baseSha", s(&base_sha)),
            ]))
            .map_err(raw)?;

            // ── verify ──
            self.stage("verifying", "Change committed", Vec::new())
                .map_err(raw)?;
            let verify = match d.verify.as_ref() {
                Some(v) if v.available() => v,
                _ => return Err(block(blocked::NO_VERIFIER)),
            };
            let mut emit = |kind: &str, data: JValue| {
                if kind == "artifact.created" {
                    ctx.artifact(with(
                        &data,
                        vec![
                            ("name", s("test-report")),
                            ("kind", s("test-report")),
                            ("revision", JValue::from(n)),
                        ],
                    ))
                } else {
                    ctx.event(kind, data)
                }
            };
            let req = obj(vec![
                ("taskId", s(id)),
                ("repo", s(&task.repo_id)),
                ("headSha", s(&head)),
                ("revision", JValue::from(n)),
            ]);
            let verified = self.within("verify", limits.verify, |sig| {
                verify.run(&req, &mut emit, sig).map_err(raw)
            })?;
            let status = read(&verified, "status")?.as_str().unwrap_or("");
            let message = verified.get("error").get("message");
            let message = message.truthy().then(|| text(message));
            if status == "unavailable" {
                return Err(block(format!(
                    "Verification is unavailable: {}",
                    message.unwrap_or_else(|| "no verifier".into())
                )));
            }
            if status != "passed" && status != "failed" {
                return Err(block(format!(
                    "Verification could not run: {}",
                    message.unwrap_or_else(|| "unknown error".into())
                )));
            }
            if status == "failed" {
                let report = verified.get("report");
                let empty = obj(Vec::<(&str, JValue)>::new());
                let report = if report.truthy() { report } else { &empty };
                feedback = tests_failed_feedback(n, report);
                self.stage("changes_requested", "Tests failed", Vec::new())
                    .map_err(raw)?;
                reason = self.next_round("Tests failed")?;
                continue;
            }
            let expected = JValue::Arr(EXPECTED_ARTIFACTS.iter().map(|a| s(a)).collect());
            self.stage(
                "reviewing",
                "Tests passed",
                vec![("expectedArtifacts", expected)],
            )
            .map_err(|e| block(wtf16::slice(&e.text(), 300)))?;

            // ── review ──
            let change = d.workspaces.change(id).map_err(|e| {
                block(format!(
                    "The change could not be read for review: {}",
                    wtf16::slice(&e.text(), 200)
                ))
            })?;
            let changed = read(&change, "files")?;
            read(changed, "length")?;
            let count = length(changed);
            ctx.event(
                "review.requested",
                obj(vec![
                    ("baseSha", s(&base_sha)),
                    ("headSha", s(&head)),
                    ("files", count),
                ]),
            )
            .map_err(raw)?;
            let JValue::Arr(changed) = changed else {
                return Err(not_a_function("change.files.map"));
            };
            let mut paths: Vec<JValue> = Vec::with_capacity(changed.len());
            for f in changed {
                paths.push(read(f, "path")?.clone());
            }
            let execution = obj(vec![
                ("headSha", s(&head)),
                ("changedFiles", JValue::Arr(paths.clone())),
                (
                    "testResults",
                    JValue::Arr(vec![obj(vec![
                        ("name", s("tests")),
                        ("passed", JValue::Bool(true)),
                    ])]),
                ),
                (
                    "summary",
                    s(&format!(
                        "Revision {n}; the operator's tests passed at this head."
                    )),
                ),
            ]);
            let review_state = state_with(vec![
                ("revision", JValue::from(n)),
                ("capabilities", capability_list.clone()),
                ("plan", plan.clone()),
                ("execution", execution.clone()),
                ("change", change.clone()),
                ("tokens", self.tokens()),
            ]);
            let judged = self.within("review", limits.review, |sig| {
                Ok(review_in_session(
                    session.as_ref(),
                    &review_state,
                    sig,
                    d.review_deadline_ms,
                    &log,
                ))
            })?;
            let (verdict, corrected) = match judged {
                Ok(v) => v,
                Err(f) => {
                    ctx.event(
                        "review.failed",
                        obj(vec![
                            ("baseSha", s(&base_sha)),
                            ("headSha", s(&head)),
                            ("code", f.code.clone()),
                            ("reason", f.reason.clone()),
                        ]),
                    )
                    .map_err(raw)?;
                    return Err(block(format!(
                        "The Planner’s review did not finish: {}",
                        text(&f.reason)
                    )));
                }
            };
            let current = d.jobs.get(id).map_err(raw)?;
            let bound = current.get("revision");
            let stale = bound.get("n").as_num() != Some(f64::from(n))
                || bound.get("headSha").as_str() != Some(head.as_str())
                || d.workspaces.branch_tip(id).map_err(raw)?.as_str() != Some(head.as_str());
            if stale {
                ctx.event(
                    "review.failed",
                    obj(vec![
                        ("baseSha", s(&base_sha)),
                        ("headSha", s(&head)),
                        ("code", s("stale")),
                        ("reason", s("The verdict was for an earlier revision.")),
                    ]),
                )
                .map_err(raw)?;
                return Err(block(blocked::STALE_VERDICT));
            }
            let fields: Vec<(String, JValue)> = match verdict_value(&verdict) {
                JValue::Obj(items) => items,
                _ => Vec::new(),
            };
            let spread = |front: Vec<(&str, JValue)>| {
                with(
                    &obj(front),
                    fields
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.clone()))
                        .collect(),
                )
            };
            ctx.event(
                "review.completed",
                with(
                    &spread(vec![("baseSha", s(&base_sha)), ("headSha", s(&head))]),
                    vec![("corrected", JValue::Bool(corrected))],
                ),
            )
            .map_err(raw)?;
            ctx.artifact(spread(vec![
                ("name", s("review")),
                ("kind", s("review-verdict")),
                ("revision", JValue::from(n)),
                ("headSha", s(&head)),
            ]))
            .map_err(raw)?;
            if verdict.verdict == "request_changes" {
                feedback = review_feedback(&verdict);
                self.stage(
                    "changes_requested",
                    "The Planner requested changes",
                    Vec::new(),
                )
                .map_err(raw)?;
                reason = self.next_round("Changes requested")?;
                continue;
            }

            // ── audit ──
            let job = d.jobs.get(id).map_err(raw)?;
            let tip = d.workspaces.branch_tip(id).map_err(raw)?;
            let input = AuditInput {
                revision: n,
                head_sha: &head,
                base_sha: Some(&base_sha),
                plan_hash: Some(&hash),
                branch_tip: tip.as_str(),
            };
            let report = build_audit_report(&job, &input).map_err(|e| {
                raw(Fail::new(match e {
                    AuditReportError::NotAJob => AuditReportError::NOT_A_JOB,
                    AuditReportError::Type => "TypeError",
                }))
            })?;
            let (mut write_up, mut write_up_error) = (None, JValue::Undefined);
            if d.audit_write_up {
                let r = self.within("audit", limits.audit, |sig| {
                    let req = obj(vec![
                        ("role", s("auditor")),
                        ("instructions", s(audit::AUDIT_INSTRUCTIONS)),
                        ("schema", audit::audit_schema()),
                        ("schemaName", s("auditor_report")),
                        ("constrain", JValue::Bool(flag(|| host.constrain()))),
                        (
                            "state",
                            state_with(vec![
                                ("revision", JValue::from(n)),
                                ("plan", plan.clone()),
                                ("lifecycleState", s("reviewing")),
                                ("execution", execution.clone()),
                            ]),
                        ),
                    ]);
                    session.call(&req, sig).map_err(raw)
                })?;
                if read(&r, "ok")?.truthy() {
                    match crate::js::tpl(r.get("text"))
                        .ok()
                        .and_then(|t| wtf16::parse(&t).ok())
                        .map(|v| read_audit(&v))
                    {
                        Some(Ok(w)) => write_up = Some(w),
                        _ => write_up_error = s("invalid"),
                    }
                } else {
                    let code = r.get("code");
                    write_up_error = if code.truthy() {
                        code.clone()
                    } else {
                        s("error")
                    };
                }
            }
            let mut audit_artifact = with(
                &obj(vec![("name", s("audit")), ("kind", s("audit-report"))]),
                match report.to_value() {
                    JValue::Obj(items) => items,
                    _ => Vec::new(),
                }
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone()))
                .collect(),
            );
            audit_artifact = with(
                &audit_artifact,
                vec![(
                    "writeUp",
                    write_up.as_ref().map_or(JValue::Null, |w| w.to_value()),
                )],
            );
            if write_up_error.truthy() {
                audit_artifact = with(&audit_artifact, vec![("writeUpError", write_up_error)]);
            }
            ctx.artifact(audit_artifact).map_err(raw)?;

            // ── accept (and merge) ──
            let files: Vec<String> = paths.iter().map(text).collect();
            let mut thrown = None;
            let (into, withheld) = merge_offer(self.settings.merge, &report, || {
                match d.workspaces.merge_preflight(id, &head) {
                    Ok(pre) if pre.get("ok").truthy() => {
                        Ok(pre.get("baseBranch").as_str().unwrap_or("").to_string())
                    }
                    Ok(pre) => Err((text(pre.get("code")), text(pre.get("reason")))),
                    Err(f) => {
                        thrown = Some(f);
                        Err((String::new(), String::new()))
                    }
                }
            });
            if let Some(f) = thrown {
                return Err(raw(f));
            }
            let job_review = d.jobs.get(id).map_err(raw)?.get("review").clone();
            let mut a = Accept {
                task_id: id,
                revision: n,
                branch: branch.as_str().unwrap_or(""),
                base_sha: &base_sha,
                head_sha: &head,
                files: &files,
                job_review: &job_review,
                verdict: &verdict,
                report: &report,
                write_up: write_up.as_ref(),
                plan_hash: &hash,
                into: into.as_deref(),
                withheld: withheld.as_ref(),
                decision: &JValue::Null,
                loops: self.loops,
                network: &self.network,
            };
            // Node's card carries the paths (and the branch) as change() and the handle gave them.
            let mut card = accept_card(&a);
            set_in(
                &mut card,
                &["paths"],
                JValue::Arr(paths.iter().take(50).cloned().collect()),
            );
            set_in(
                &mut card,
                &["arguments", "files"],
                JValue::Arr(paths.clone()),
            );
            set_in(&mut card, &["arguments", "branch"], branch.clone());
            ctx.event("approval.requested", card.clone()).map_err(raw)?;
            let answer = d.approvals.ask(&card, ctx.signal).map_err(raw)?;
            ctx.event(
                "approval.decided",
                obj(vec![
                    ("decision", answer.clone()),
                    ("action", s(crate::review::REVIEW_ACTION)),
                ]),
            )
            .map_err(raw)?;
            self.log(vec![
                ("event", s("code.pipeline_decided")),
                ("decision", answer.clone()),
                ("revision", JValue::from(n)),
                (
                    "merge",
                    JValue::Bool(into.as_deref().is_some_and(|i| !i.is_empty())),
                ),
            ]);
            self.live()?;
            a.decision = &answer;
            let mut result = accept_result(&a);
            set_in(&mut result, &["branch"], branch.clone());
            if !should_merge(&a) {
                return Ok(result);
            }
            if !flag(|| host.merge()) {
                return Ok(merge_turned_off_result(&result));
            }
            // Tighter than Node, where nothing can run between the check after the card and
            // here: a cancel that came since never merges.
            self.live()?;
            let merged = d.workspaces.merge_verified(id, &head).map_err(raw)?;
            if !merged.get("ok").truthy() {
                let (message, result) = merge_refused(
                    &result,
                    &text(merged.get("code")),
                    &text(merged.get("reason")),
                );
                return Err(Stop::Blocked(message, result));
            }
            let mut recorded = merged.get("recordFailed") != &JValue::Bool(true);
            let base_branch = text(merged.get("baseBranch"));
            if let Err(e) = self.stage("merged", &merged_stage_reason(&base_branch), Vec::new()) {
                recorded = false;
                self.log(vec![
                    ("event", s("code.pipeline_record_failed")),
                    ("error", s(&wtf16::slice(&e.text(), 200))),
                ]);
            }
            return Ok(merged_result(&result, &base_branch, recorded));
        }
    }

    /// `loop(why)`: one more change round, or blocked once `max_loops` are spent.
    fn next_round(&mut self, why: &str) -> Result<String, Stop> {
        let round = next_round(why, self.loops, self.d.max_loops);
        self.loops = round.loops;
        round.next.map_err(block)
    }
}
