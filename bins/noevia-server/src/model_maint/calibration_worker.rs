//! The native calibration job (core `llamacpp-calibration.cjs` `stepOnce`, `run`,
//! `compareAndLaunch`, `compareAndAbort`): the engine-facing step, the run under a maintenance
//! hold, its cancellation controller and its evidence record. UNWIRED: the public routes still
//! reach Node's worker through the private bridges until the routes switch as one unit.
//!
//! The job is the same object Node keeps in `native-calibration-<hash>.json` (see
//! [`super::calibration`]), with the same keys in the same order, so either owner reads the other's
//! jobs and history. The original profile text is kept in the file only while the job runs (for
//! [`Maint::recover_calibration`]) and never leaves this module.

use super::calibration::{
    bound_load, grouped, ladder, over_budget_reason, verify_long, Applied, Feed, Kind, LongOutcome,
    LongPrompt, Progress, Recovery, State, StateFile, Steps, Stop, NOT_RESTORED,
};
use super::{Hold, HoldRefusal, Maint};
use js_json::{wtf16, JValue};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const PAD: &str = "This is synthetic padding for a context allocation validation.\n";
/// What a run that lost its exclusion records (the lease ended or Node stopped confirming it).
pub const LEASE_LOST: &str = "Calibration stopped because its maintenance hold ended; other clients may have used the engine.";
/// Added to a run's worst case before asking Node for a lease that long.
const RUN_MARGIN: Duration = Duration::from_secs(15 * 60);
/// The refusal of a run that could outlast the longest lease (no gate hold).
/// The request timeouts a run uses besides [`Limits`] (Node's fixed values), shared with
/// [`Worker::worst_case`]: `POST /models/load`, `POST /tokenize`, `GET /props`.
const LOAD_REQUEST: Duration = Duration::from_secs(120);
const TOKENIZE: Duration = Duration::from_secs(30);
const PROPS: Duration = Duration::from_secs(8);
/// The guarded reload after a profile write (reload.rs `RELOAD_LIMIT`) and the raw one after a
/// restore: both 120 s.
const RELOAD: Duration = super::calibration::RELOAD_TIMEOUT;
pub const OVER_LIMIT: &str = "This calibration could take longer than the 24-hour maintenance limit. Choose a shorter prompt time limit.";
/// Added to a recovered job's error while the state file still says it is running: the save of
/// the interrupted state failed and is being retried (never written to the file).
pub const UNSAVED: &str =
    " The interrupted state could not be saved yet; noevia keeps trying to save it.";
/// The longest wait between those retries (they start at [`Limits::save_retry`] and double).
pub const FINALIZATION_FAILED: &str = "Calibration finalization failed.";
const FAILURE_SAVE_GRACE: Duration = Duration::from_millis(100);
const SAVE_RETRY_MAX: Duration = Duration::from_secs(60);
/// The reply a load gets when Node cannot decide it (never sent to the engine, never shown).
const LOAD_UNCHECKED: &str = "The load could not be checked against the inference memory budget.";
/// Node's load decision, asked before every load.
const LOAD_CHECK: Duration = super::node_lease::LOAD_CHECK;
const ANOTHER_CLIENT: &str = "Another client loaded a model during calibration. Stop Diary background jobs and other clients, then retry.";

/// `limits`: the step timeouts (Node's defaults; tests shorten them).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub load: Duration,
    pub smoke: Duration,
    pub long: Duration,
    pub unload: Duration,
    pub poll: Duration,
    pub memory_poll: Duration,
    pub recover: Duration,
    pub backend_poll: Duration,
    pub memory_settle: Duration,
    /// How often a run checks that its hold still excludes other clients.
    pub exclusion_poll: Duration,
    /// The first wait before saving a recovered job again after its save failed.
    pub save_retry: Duration,
    /// Maximum time final persistence and admission cleanup can delay terminal publication.
    pub finalize: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            load: Duration::from_millis(600_000),
            smoke: Duration::from_millis(180_000),
            long: Duration::from_millis(7_200_000),
            unload: Duration::from_millis(60_000),
            poll: Duration::from_millis(500),
            memory_poll: Duration::from_millis(1000),
            recover: Duration::from_millis(180_000),
            backend_poll: Duration::from_millis(2000),
            memory_settle: Duration::from_millis(1000),
            exclusion_poll: Duration::from_millis(1000),
            save_retry: Duration::from_millis(1000),
            finalize: Duration::from_secs(30),
        }
    }
}

/// `readMemAvailableGib()`: `/proc/meminfo` MemAvailable in GiB (the host's, inside a container).
pub fn read_mem_available_gib() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: f64 = line
        .trim_start_matches("MemAvailable:")
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse()
        .ok()?;
    Some(kb * 1024.0 / (1024.0 * 1024.0 * 1024.0))
}

/// An `AbortController`: `abort()` once, `aborted()` read anywhere, `wait()` until aborted.
#[derive(Debug, Default)]
pub struct Abort {
    flag: AtomicBool,
    notify: Notify,
}

impl Abort {
    pub fn abort(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
    pub fn aborted(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
    pub async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.aborted() {
                return;
            }
            notified.await;
        }
    }
}

/// A thrown error inside a step or run: cancelled, fatal (the run stops with the text) or any other
/// throw (a step treats it as the engine having failed; a run records its text).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Thrown {
    Cancelled,
    Fatal(String),
    Other(String),
}

impl From<Thrown> for Stop {
    fn from(t: Thrown) -> Stop {
        match t {
            Thrown::Cancelled => Stop::Cancelled,
            Thrown::Fatal(m) | Thrown::Other(m) => Stop::Fatal(m),
        }
    }
}

/// Why a launch did not start a job.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// Launching, or a job is running.
    Running,
    /// The maintenance hold was refused: `{ status, publicMessage, code }` as the route reads it.
    Hold(JValue),
    /// The state could not be saved, or the engine is not native llama.cpp.
    Unavailable,
    /// No gate hold, and the run's worst case is over the longest lease Node grants.
    Limit,
}

/// What a launch plans (the route's `plan`, already validated).
#[derive(Debug, Clone)]
pub struct Plan {
    pub prompt_budget_seconds: f64,
    pub native: f64,
    /// Preset options in insertion order (strings; anything else is refused by the profile write).
    pub base: Vec<(String, JValue)>,
}

pub(super) struct Inner {
    pub(super) state: State,
    pub(super) cancel_requested: bool,
    pub(super) inflight: Option<Arc<Abort>>,
    pub(super) launching: bool,
    /// A live run, including its final persistence and admission release.
    pub(super) active: bool,
    /// Node's `objectId(state.job)`: changes whenever the job object is replaced (launch, reload).
    pub(super) job_object: u64,
    /// Node's `objectId(inflight)`: 0 when no step controller is live.
    pub(super) controller: u64,
    pub(super) next_object: u64,
    /// The running job's hold stopped excluding other clients: the run stops as on a cancel.
    pub(super) lease_lost: bool,
    /// The boot recovery is running (it holds `launching` too): the job in memory is the file's,
    /// with no run behind it, so nothing may cancel or launch over it yet.
    pub(super) recovering: bool,
    /// The models live when the run was asked for (at least one): its first unload is allowed one
    /// unload request for each, and no longer ([`Worker::worst_case`]).
    pub(super) first_unload: u32,
    /// The recovered job in memory is not in the state file yet (saving it failed): memory is
    /// right, the file is behind, and the save is retried until it lands or a launch replaces it.
    pub(super) unsaved: bool,
}

/// The native worker: one per process, owning its state file, controller and job.
pub struct Worker {
    pub(super) maint: Arc<Maint>,
    file: StateFile,
    inner: Arc<Mutex<Inner>>,
    writes: Arc<Mutex<()>>,
    #[cfg(test)]
    panic_on_run: AtomicBool,
    memory: Arc<dyn Fn() -> Option<f64> + Send + Sync>,
    floor_gib: f64,
    limits: Limits,
    /// The per-process salt of the facts tokens (Node's `launchSalt` / `cancelSalt`).
    pub(super) salt: String,
}

/// The job keys the bridge and the public routes may show (`cancellationFacts`).
pub const PUBLIC_KEYS: &[&str] = &[
    "id",
    "model",
    "promptBudgetSeconds",
    "native",
    "base",
    "status",
    "phase",
    "startedAt",
    "steps",
    "memoryFloorGib",
    "memoryGuard",
    "originalRevision",
    "lastRevision",
    "result",
    "error",
    "restored",
    "finishedAt",
];

pub(super) fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

pub(super) fn random_hex(bytes: usize) -> String {
    use rand_core::RngCore;
    let mut b = vec![0u8; bytes];
    rand_core::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `crypto.randomUUID()`.
fn uuid() -> String {
    let h = random_hex(16);
    let variant = match h.as_bytes().get(16).map(|b| b % 4) {
        Some(0) => "8",
        Some(1) => "9",
        Some(2) => "a",
        _ => "b",
    };
    format!(
        "{}-{}-4{}-{}{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        variant,
        &h[17..20],
        &h[20..32]
    )
}

/// `crypto.randomInt(1000, 9999)`.
fn random_int() -> u32 {
    use rand_core::RngCore;
    1000 + rand_core::OsRng.next_u32() % 8999
}

fn status_of(m: &Value) -> Option<&str> {
    m.get("status")
        .and_then(|s| s.get("value"))
        .and_then(Value::as_str)
}

pub(super) fn id_of(m: &Value) -> Option<&str> {
    m.get("id").and_then(Value::as_str)
}

/// `Number(v)` of a JValue (strings as ECMAScript reads them).
pub(super) fn number(v: &JValue) -> f64 {
    match v {
        JValue::Num(n) => *n,
        JValue::Null => 0.0,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Str(s) => {
            llamacpp_autoconfig::string_to_number(&s.encode_utf16().collect::<Vec<_>>())
                .unwrap_or(f64::NAN)
        }
        _ => f64::NAN,
    }
}

/// A job's `error` text, or none.
fn job_error(job: &[(String, JValue)]) -> String {
    job.iter()
        .find(|(k, _)| k == "error")
        .and_then(|(_, v)| v.as_str())
        .unwrap_or("")
        .to_owned()
}

/// The job's error without [`UNSAVED`] (what the file gets).
fn strip_unsaved(job: &mut Vec<(String, JValue)>) {
    let error = job_error(job);
    if let Some(kept) = error.strip_suffix(UNSAVED) {
        wtf16::set(job, "error", JValue::Str(kept.to_owned()));
    }
}

pub(super) fn public(job: &JValue) -> JValue {
    match job {
        JValue::Obj(_) => JValue::Obj(
            PUBLIC_KEYS
                .iter()
                .filter_map(|k| {
                    let v = job.get(k);
                    (!matches!(v, JValue::Undefined)).then(|| ((*k).to_owned(), v.clone()))
                })
                .collect(),
        ),
        _ => JValue::Null,
    }
}

/// The steps the search asks for, run against the engine.
struct Engine<'a> {
    worker: &'a Arc<Worker>,
    hold: &'a Hold,
}

impl Steps for Engine<'_> {
    async fn step(&mut self, ctx: f64, kind: Kind) -> Result<bool, Stop> {
        self.worker.step(self.hold, ctx, kind).await
    }
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("floor_gib", &self.floor_gib)
            .finish_non_exhaustive()
    }
}

fn finish_snapshot(state: &mut State, failed: bool) {
    if let Some(job) = state.job_mut() {
        if failed {
            wtf16::set(job, "status", JValue::Str("failed".into()));
            wtf16::set(job, "phase", JValue::Str("Failed".into()));
            wtf16::set(job, "error", JValue::Str(FINALIZATION_FAILED.into()));
        }
        wtf16::set(job, "finishedAt", JValue::Num(now_ms()));
        job.retain(|(key, _)| key != "originalText");
    }
}

pub(super) fn visible_job(i: &Inner) -> JValue {
    let mut job = public(i.state.job());
    if i.active {
        if let JValue::Obj(fields) = &mut job {
            if i.state.job().get("status").as_str() != Some("running") {
                wtf16::set(fields, "phase", JValue::Str("Finalizing".into()));
            }
            wtf16::set(fields, "status", JValue::Str("running".into()));
            fields.retain(|(key, _)| key != "finishedAt");
        }
    }
    job
}

/// Revokes a blocking writer's commit on timeout, cancellation or unwind.
struct WritePermit(Arc<AtomicBool>);
impl Drop for WritePermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Hold's drop releases admission on unwind; this reconciles the public job without leaking
/// panic payloads or leaving an immortal active flag. Also captured by never-polled tasks.
struct RunGuard {
    worker: Arc<Worker>,
    object: u64,
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        let retry = self.worker.with(|i| {
            if !i.active || i.job_object != self.object {
                return false;
            }
            finish_snapshot(&mut i.state, true);
            i.active = false;
            i.cancel_requested = false;
            i.lease_lost = false;
            i.inflight = None;
            i.controller = 0;
            i.unsaved = true;
            true
        });
        if retry && tokio::runtime::Handle::try_current().is_ok() {
            self.worker.spawn_save_retry();
        }
    }
}

impl Worker {
    /// `createCalibrator(...)` + `load()`: the worker of `maint`, with the job and history its state
    /// file holds.
    pub fn new(
        maint: Arc<Maint>,
        memory: Arc<dyn Fn() -> Option<f64> + Send + Sync>,
        floor_gib: f64,
        limits: Limits,
    ) -> Arc<Worker> {
        let file = maint.calibration_state();
        let state = file.load();
        Arc::new(Worker {
            maint,
            file,
            writes: Arc::new(Mutex::new(())),
            #[cfg(test)]
            panic_on_run: AtomicBool::new(false),
            inner: Arc::new(Mutex::new(Inner {
                state,
                cancel_requested: false,
                inflight: None,
                launching: false,
                active: false,
                job_object: 1,
                controller: 0,
                next_object: 2,
                lease_lost: false,
                recovering: false,
                first_unload: 1,
                unsaved: false,
            })),
            salt: random_hex(32),
            memory,
            floor_gib,
            limits,
        })
    }

    pub(super) fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        f(&mut guard)
    }

    /// Serialized, generation-fenced writes. A canceled blocking task cannot later commit a
    /// superseded snapshot; admission never waits indefinitely for its kernel I/O to return.
    async fn write_state(
        &self,
        state: State,
        object: u64,
        until: Option<Instant>,
    ) -> std::io::Result<()> {
        if self.file.path.is_none() {
            return Err(std::io::Error::other("calibration state unavailable"));
        }
        let file = self.file.clone();
        let inner = Arc::clone(&self.inner);
        let writes = Arc::clone(&self.writes);
        let permit = Arc::new(AtomicBool::new(true));
        let _permit_guard = WritePermit(Arc::clone(&permit));
        let allowed = Arc::clone(&permit);
        let task = tokio::task::spawn_blocking(move || {
            let _writer = writes.lock().unwrap_or_else(|p| p.into_inner());
            file.save_if(&state, || {
                allowed.load(Ordering::Acquire)
                    && until.is_none_or(|end| Instant::now() < end)
                    && inner.lock().unwrap_or_else(|p| p.into_inner()).job_object == object
            })
        });
        let result = match until {
            Some(end) => match tokio::time::timeout_at(end.into(), task).await {
                Ok(result) => result,
                Err(_) => {
                    permit.store(false, Ordering::Release);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "state write ended",
                    ));
                }
            },
            None => task.await,
        };
        result.map_err(|_| std::io::Error::other("state write task"))?
    }

    async fn save(&self) -> std::io::Result<()> {
        let (state, object) = self.with(|i| (i.state.clone(), i.job_object));
        self.write_state(state, object, Some(Instant::now() + self.limits.finalize))
            .await
    }

    fn job_get(&self, key: &str) -> JValue {
        self.with(|i| i.state.job().get(key).clone())
    }

    fn job_set(&self, key: &str, value: JValue) {
        self.with(|i| {
            if let Some(job) = i.state.job_mut() {
                wtf16::set(job, key, value);
            }
        });
    }

    fn with_step<T>(
        &self,
        index: usize,
        f: impl FnOnce(&mut Vec<(String, JValue)>) -> T,
    ) -> Option<T> {
        self.with(|i| {
            let job = i.state.job_mut()?;
            let (_, JValue::Arr(steps)) = job.iter_mut().find(|(k, _)| k == "steps")? else {
                return None;
            };
            match steps.get_mut(index)? {
                JValue::Obj(fields) => Some(f(fields)),
                _ => None,
            }
        })
    }

    fn cancelled(&self) -> bool {
        self.with(|i| i.cancel_requested)
    }

    /// The public job (`cancellationFacts().job`): known keys only, never the original text.
    pub fn job(&self) -> JValue {
        self.with(|i| visible_job(i))
    }

    /// `state.history[model]`, or an empty list.
    pub fn history(&self, model: &str) -> JValue {
        self.with(|i| match i.state.get("history").get(model) {
            v if v.truthy() => v.clone(),
            _ => JValue::Arr(Vec::new()),
        })
    }

    /// Whether a launch is starting.
    pub fn starting(&self) -> bool {
        self.with(|i| i.launching)
    }

    /// The boot `recover()`, started before the routes serve: the recovery takes the launch slot at
    /// once, so no launch can start (and be replaced by the file's older state) while it runs.
    pub fn spawn_recover(
        self: &Arc<Self>,
    ) -> Option<tokio::task::JoinHandle<Option<Option<bool>>>> {
        if !self.begin_recovery() {
            return None;
        }
        let worker = Arc::clone(self);
        Some(tokio::spawn(async move { worker.recover_held().await }))
    }

    /// `recover()`: a job a restart left running becomes interrupted (its profile restored if
    /// models.ini is still what it last wrote). `None` when a launch holds the slot.
    pub async fn recover(self: &Arc<Self>) -> Option<Option<bool>> {
        if !self.begin_recovery() {
            return None;
        }
        self.recover_held().await
    }

    fn begin_recovery(&self) -> bool {
        self.with(|i| {
            if i.launching || i.active {
                return false;
            }
            i.launching = true;
            i.recovering = true;
            true
        })
    }

    /// Memory takes the recovered state only when there was a job to recover, so a file with nothing
    /// to recover never replaces what memory holds. If the interrupted state could not be saved,
    /// memory still has it (with [`UNSAVED`] on its error) and the save is retried: the job is not
    /// running, so launches are allowed, each saving it first ([`Worker::take_hold`]).
    async fn recover_held(self: &Arc<Self>) -> Option<Option<bool>> {
        let outcome = self.maint.recover_calibration().await;
        let restored = outcome.restored();
        let unsaved = matches!(outcome, Recovery::Unsaved { .. });
        if let Recovery::Saved { state, .. } | Recovery::Unsaved { state, .. } = outcome {
            let mut state = state;
            if unsaved {
                if let Some(job) = state.job_mut() {
                    let error = job_error(job);
                    wtf16::set(job, "error", JValue::Str(format!("{error}{UNSAVED}")));
                }
            }
            self.with(|i| {
                i.state = state;
                i.job_object = i.next_object;
                i.next_object += 1;
                i.unsaved = unsaved;
            });
        }
        self.with(|i| {
            i.launching = false;
            i.recovering = false;
        });
        if unsaved {
            self.spawn_save_retry();
        }
        restored
    }

    /// Whether memory holds a recovered job the state file does not have yet.
    pub fn unsaved(&self) -> bool {
        self.with(|i| i.unsaved)
    }

    /// Writes the recovered job the file is missing (without [`UNSAVED`]), then drops the note from
    /// memory. The caller holds the launch slot, so no launch's save can land in between.
    async fn save_recovered(&self) -> std::io::Result<()> {
        let (object, mut state) = self.with(|i| (i.job_object, i.state.clone()));
        if let Some(job) = state.job_mut() {
            strip_unsaved(job);
        }
        self.write_state(state, object, Some(Instant::now() + self.limits.finalize))
            .await?;
        self.with(|i| {
            if i.job_object == object {
                if let Some(job) = i.state.job_mut() {
                    strip_unsaved(job);
                }
                i.unsaved = false;
            }
        });
        Ok(())
    }

    /// The retries of a failed recovery save: waits from [`Limits::save_retry`], doubling up to
    /// [`SAVE_RETRY_MAX`], until the save lands or a launch's own save has replaced the job. A round
    /// whose launch slot is taken waits for the next (that launch saves first itself).
    fn spawn_save_retry(self: &Arc<Self>) {
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            let mut wait = worker.limits.save_retry;
            loop {
                tokio::time::sleep(wait).await;
                let took = worker.with(|i| {
                    if !i.unsaved {
                        return None;
                    }
                    let free = !i.launching;
                    if free {
                        i.launching = true;
                    }
                    Some(free)
                });
                match took {
                    None => return,
                    Some(false) => {}
                    Some(true) => {
                        let saved = worker.save_recovered().await;
                        worker.with(|i| i.launching = false);
                        if saved.is_ok() {
                            return;
                        }
                    }
                }
                wait = (wait * 2).min(SAVE_RETRY_MAX);
            }
        });
    }

    /// Whether the boot recovery is still running.
    pub fn recovering(&self) -> bool {
        self.with(|i| i.recovering)
    }

    /// `compareAndAbort(token)`: the running job is cancelled; its public snapshot, taken before the
    /// controller is aborted. `None` when no job is running.
    pub fn cancel(&self) -> Option<JValue> {
        self.with(|i| {
            if i.recovering
                || (!i.active && i.state.job().get("status").as_str() != Some("running"))
            {
                return None;
            }
            let job = visible_job(i);
            i.cancel_requested = true;
            if let Some(a) = &i.inflight {
                a.abort();
            }
            Some(job)
        })
    }

    // ── Engine requests ──────────────────────────────────────────────────────

    async fn race<T>(
        abort: Option<&Abort>,
        fut: impl std::future::Future<Output = T>,
    ) -> Result<T, Thrown> {
        match abort {
            None => Ok(fut.await),
            Some(a) => {
                if a.aborted() {
                    return Err(Thrown::Other("This operation was aborted".into()));
                }
                tokio::select! {
                    v = fut => Ok(v),
                    () = a.wait() => Err(Thrown::Other("This operation was aborted".into())),
                }
            }
        }
    }

    async fn request(
        &self,
        hold: &Hold,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
        abort: Option<&Abort>,
    ) -> Result<super::router::Reply, Thrown> {
        let router = self
            .maint
            .router
            .as_ref()
            .ok_or_else(|| Thrown::Other("fetch failed".into()))?;
        // llamacpp-manager.cjs `request`: every load passes the load decision first (the budget,
        // the watchdog's quarantine, the size knobs), answered as Node's own 409 when refused.
        if method == reqwest::Method::POST && path == "/models/load" {
            if let Some(model) = body.and_then(|b| b.get("model")).and_then(Value::as_str) {
                if let Some(refused) = Self::race(abort, self.load_refusal(model)).await? {
                    return Ok(refused);
                }
            }
        }
        let token = hold.token();
        Self::race(
            abort,
            router.request(method, path, body, token.as_deref(), timeout),
        )
        .await?
        .map_err(|_| Thrown::Other("fetch failed".into()))
    }

    /// Node's decision on loading `model` now (core model-load-refusal.cjs, over the models.ini the
    /// run just wrote and the current budget): `None` lets the load go; otherwise the reply the load
    /// gets instead, never sent to the engine. Refused, it is Node's 409 with Node's own body; with
    /// no decision (Node unreachable, any other answer) it is refused all the same, as a 503, so the
    /// step fails as a refused load does and nothing loads unchecked.
    async fn load_refusal(&self, model: &str) -> Option<super::router::Reply> {
        use super::node_lease::LoadDecision;
        match self.maint.node.load_refusal(model).await {
            LoadDecision::Proceed => None,
            LoadDecision::Refused(body) => Some(super::router::Reply {
                ok: false,
                status: 409,
                body: Some(body),
            }),
            LoadDecision::Unavailable => Some(super::router::Reply {
                ok: false,
                status: 503,
                body: Some(json!({ "error": LOAD_UNCHECKED })),
            }),
        }
    }

    /// `listing(signal)`: the router's rows, or the backend error.
    async fn listing(&self, hold: &Hold, abort: Option<&Abort>) -> Result<Vec<Value>, Thrown> {
        let down = || Thrown::Other("The model server is not responding.".into());
        let r = self
            .request(
                hold,
                reqwest::Method::GET,
                "/models",
                None,
                super::router::DEFAULT_TIMEOUT,
                abort,
            )
            .await
            .map_err(|e| match e {
                Thrown::Other(m) if m.contains("aborted") => Thrown::Other(m),
                _ => down(),
            })?;
        if !r.ok {
            return Err(down());
        }
        super::router::rows(&r.body).cloned().ok_or_else(down)
    }

    async fn post_unload(&self, hold: &Hold, model: &str) {
        let _ = self
            .request(
                hold,
                reqwest::Method::POST,
                "/models/unload",
                Some(&json!({ "model": model })),
                self.limits.unload,
                None,
            )
            .await;
    }

    /// `unloadAll()`: every live model asked to unload, then polled until none is live. It gets
    /// [`Worker::unload_allowance`] for `models` and no more: a listing that turns up more live
    /// models than were counted ends as Node's own time-out does.
    async fn unload_all(&self, hold: &Hold, models: u32) -> Result<(), Thrown> {
        let late = || Thrown::Other("Models did not unload in time.".into());
        tokio::time::timeout(self.unload_allowance(models), self.unload_all_inner(hold))
            .await
            .unwrap_or_else(|_| Err(late()))
    }

    /// The longest [`Worker::unload_all`] may take with `models` live: the listing, one unload
    /// request for each, then polling to the unload limit.
    fn unload_allowance(&self, models: u32) -> Duration {
        let l = &self.limits;
        super::router::DEFAULT_TIMEOUT + l.unload * models.max(1) + self.polled(l.unload)
    }

    async fn unload_all_inner(&self, hold: &Hold) -> Result<(), Thrown> {
        let models = self.listing(hold, None).await?;
        for m in &models {
            if matches!(status_of(m), Some("loaded" | "loading" | "sleeping")) {
                if let Some(id) = id_of(m) {
                    self.post_unload(hold, id).await;
                }
            }
        }
        let deadline = tokio::time::Instant::now() + self.limits.unload;
        while tokio::time::Instant::now() < deadline {
            let rows = self.listing(hold, None).await?;
            if !rows.iter().any(|m| status_of(m) != Some("unloaded")) {
                return Ok(());
            }
            tokio::time::sleep(self.limits.poll).await;
        }
        Err(Thrown::Other("Models did not unload in time.".into()))
    }

    /// `unload(model)`: asked, then polled until it reads unloaded (or the time is up).
    async fn unload(&self, hold: &Hold, model: &str) {
        self.post_unload(hold, model).await;
        let deadline = tokio::time::Instant::now() + self.limits.unload;
        while tokio::time::Instant::now() < deadline {
            if let Ok(rows) = self.listing(hold, None).await {
                if rows
                    .iter()
                    .find(|m| id_of(m) == Some(model))
                    .and_then(status_of)
                    == Some("unloaded")
                {
                    return;
                }
            }
            tokio::time::sleep(self.limits.poll).await;
        }
    }

    async fn wait_for_backend(&self, hold: &Hold) -> bool {
        let deadline = tokio::time::Instant::now() + self.limits.recover;
        while tokio::time::Instant::now() < deadline {
            if self.listing(hold, None).await.is_ok() {
                return true;
            }
            tokio::time::sleep(self.limits.backend_poll).await;
        }
        false
    }

    async fn load_and_wait(&self, hold: &Hold, model: &str) -> bool {
        let Ok(started) = self
            .request(
                hold,
                reqwest::Method::POST,
                "/models/load",
                Some(&json!({ "model": model })),
                LOAD_REQUEST,
                None,
            )
            .await
        else {
            return false;
        };
        if !started.ok {
            return false;
        }
        let deadline = tokio::time::Instant::now() + self.limits.load;
        let mut polls = 0;
        while tokio::time::Instant::now() <= deadline && polls < 2400 {
            let Ok(rows) = self.listing(hold, None).await else {
                return false;
            };
            let row = rows.iter().find(|m| id_of(m) == Some(model));
            match row {
                Some(r) if status_of(r) == Some("loaded") => return true,
                None => return false,
                Some(r)
                    if r.get("status")
                        .and_then(|s| s.get("failed"))
                        .is_some_and(truthy) =>
                {
                    return false
                }
                Some(r) if status_of(r) == Some("unloaded") => return false,
                _ => {}
            }
            tokio::time::sleep(self.limits.poll).await;
            polls += 1;
        }
        false
    }

    // ── Memory ───────────────────────────────────────────────────────────────

    /// One sample into step `index`: false (and the controller aborted) under the floor.
    fn sample(&self, index: usize, abort: &Abort) -> bool {
        let Some(value) = (self.memory)() else {
            return true;
        };
        let rounded = js_round(value * 100.0) / 100.0;
        self.with_step(index, |r| {
            let old = r
                .iter()
                .find(|(k, _)| k == "minAvailableGib")
                .and_then(|(_, v)| v.as_num())
                .unwrap_or(f64::INFINITY);
            wtf16::set(r, "minAvailableGib", JValue::Num(old.min(rounded)));
        });
        if value < self.floor_gib {
            self.with_step(index, |r| {
                wtf16::set(r, "memoryFloorHit", JValue::Bool(true))
            });
            abort.abort();
            return false;
        }
        true
    }

    fn floor_hit(&self, index: usize) -> bool {
        self.with_step(index, |r| {
            r.iter()
                .any(|(k, v)| k == "memoryFloorHit" && *v == JValue::Bool(true))
        })
        .unwrap_or(false)
    }

    // ── One step ─────────────────────────────────────────────────────────────

    /// `finish(status, reason, cause)`: the step record closed and saved; whether it passed.
    async fn finish(
        &self,
        index: usize,
        started: f64,
        status: &str,
        reason: Option<&str>,
        cause: Option<&str>,
    ) -> bool {
        self.with_step(index, |r| {
            wtf16::set(r, "status", JValue::Str(status.into()));
            if let Some(reason) = reason {
                wtf16::set(r, "reason", JValue::Str(reason.into()));
            }
            if let Some(cause) = cause {
                wtf16::set(r, "cause", JValue::Str(cause.into()));
            }
            wtf16::set(
                r,
                "seconds",
                JValue::Num(js_round((now_ms() - started) / 1000.0)),
            );
        });
        let _ = self.save().await;
        status == "passed"
    }

    fn oom(&self) -> String {
        format!("Available memory fell below {} GiB.", plain(self.floor_gib))
    }

    /// `stepOnce(job, ctx, kind)`.
    async fn step(self: &Arc<Self>, hold: &Hold, ctx: f64, kind: Kind) -> Result<bool, Stop> {
        if self.cancelled() {
            return Err(Stop::Cancelled);
        }
        let started = now_ms();
        let kind_name = if kind == Kind::Long { "long" } else { "load" };
        let index = self.with(|i| {
            let job = i.state.job_mut()?;
            let record = JValue::obj([
                ("ctx", JValue::Num(ctx)),
                ("kind", JValue::Str(kind_name.into())),
                ("status", JValue::Str("running".into())),
                ("startedAt", JValue::Num(started)),
            ]);
            let steps = match job.iter_mut().find(|(k, _)| k == "steps") {
                Some((_, JValue::Arr(s))) => s,
                _ => {
                    wtf16::set(job, "steps", JValue::Arr(Vec::new()));
                    match job.iter_mut().find(|(k, _)| k == "steps") {
                        Some((_, JValue::Arr(s))) => s,
                        _ => return None,
                    }
                }
            };
            steps.push(record);
            let index = steps.len() - 1;
            let phase = if kind == Kind::Long {
                format!("Long-prompt test at {} tokens", grouped(ctx))
            } else {
                format!("Loading at {} tokens", grouped(ctx))
            };
            wtf16::set(job, "phase", JValue::Str(phase));
            Some(index)
        });
        let Some(index) = index else {
            return Err(Stop::Fatal("Calibration failed.".into()));
        };
        let _ = self.save().await;
        // Memory freed by the previous unload can take a moment to return.
        let mut waited = 0;
        while (self.memory)().is_some_and(|v| v < self.floor_gib) {
            if waited >= 30 {
                let floor = plain(self.floor_gib);
                self.with_step(index, |r| {
                    wtf16::set(r, "status", JValue::Str("failed".into()));
                    wtf16::set(
                        r,
                        "reason",
                        JValue::Str(format!(
                            "Available memory stayed below {floor} GiB with the model unloaded."
                        )),
                    );
                });
                let _ = self.save().await;
                return Err(Stop::Fatal(format!("Available memory stayed below {floor} GiB even with the model unloaded. Free memory on this machine, then retry.")));
            }
            tokio::time::sleep(self.limits.memory_settle).await;
            waited += 1;
        }
        let abort = Arc::new(Abort::default());
        self.with(|i| {
            i.inflight = Some(Arc::clone(&abort));
            i.controller = i.next_object;
            i.next_object += 1;
        });
        let watcher = if (self.memory)().is_none() {
            self.with_step(index, |r| {
                wtf16::set(r, "memoryGuard", JValue::Str("unavailable".into()))
            });
            None
        } else {
            self.sample(index, &abort);
            let (w, a, every) = (
                Arc::clone(self),
                Arc::clone(&abort),
                self.limits.memory_poll,
            );
            Some(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    w.sample(index, &a);
                }
            }))
        };
        let attempt = self
            .attempt(hold, ctx, kind, index, &abort, watcher.is_some())
            .await;
        let outcome = match attempt {
            Ok((status, reason, cause)) => Ok(self
                .finish(index, started, status, reason.as_deref(), cause)
                .await),
            Err(e) => {
                if e == Thrown::Cancelled || self.cancelled() {
                    self.finish(index, started, "skipped", Some("Cancelled."), None)
                        .await;
                    Err(Stop::Cancelled)
                } else if let Thrown::Fatal(m) = e {
                    self.finish(index, started, "failed", Some(&m), None).await;
                    Err(Stop::Fatal(m))
                } else if self.floor_hit(index) {
                    let oom = self.oom();
                    Ok(self
                        .finish(index, started, "failed", Some(&oom), Some("oom"))
                        .await)
                } else if !self.wait_for_backend(hold).await {
                    self.finish(
                        index,
                        started,
                        "failed",
                        Some("The model server stopped responding."),
                        None,
                    )
                    .await;
                    Err(Stop::Fatal(
                        "The model server did not come back after a failed step.".into(),
                    ))
                } else {
                    // #1046: a crash; its text never reads as a time out.
                    Ok(self
                        .finish(
                            index,
                            started,
                            "failed",
                            Some("The model server failed during this step."),
                            Some("oom"),
                        )
                        .await)
                }
            }
        };
        if let Some(w) = watcher {
            w.abort();
        }
        self.with(|i| {
            i.inflight = None;
            i.controller = 0;
        });
        let model = self.job_get("model").as_str().unwrap_or("").to_owned();
        self.unload(hold, &model).await;
        outcome
    }

    fn base_options(&self, extra: (&str, String)) -> Vec<(String, Option<String>)> {
        let mut out: Vec<(String, Option<String>)> = match self.job_get("base") {
            JValue::Obj(fields) => fields
                .into_iter()
                .map(|(k, v)| {
                    let s = v.as_str().map(str::to_owned);
                    (k, s)
                })
                .collect(),
            _ => Vec::new(),
        };
        match out.iter_mut().find(|(k, _)| k == extra.0) {
            Some(slot) => slot.1 = Some(extra.1),
            None => out.push((extra.0.to_owned(), Some(extra.1))),
        }
        out
    }

    fn base_revision(&self) -> String {
        let last = self.job_get("lastRevision");
        let rev = if last.truthy() {
            last
        } else {
            self.job_get("originalRevision")
        };
        rev.as_str().unwrap_or("").to_owned()
    }

    async fn current_revision(&self) -> Option<String> {
        let ini = self.maint.ini.clone()?;
        tokio::task::spawn_blocking(move || ini.snapshot().ok().map(|s| s.revision))
            .await
            .ok()
            .flatten()
    }

    /// The step's body: `Ok((status, reason, cause))` for `finish`, or the throw.
    async fn attempt(
        &self,
        hold: &Hold,
        ctx: f64,
        kind: Kind,
        index: usize,
        abort: &Abort,
        guarded: bool,
    ) -> Result<(&'static str, Option<String>, Option<&'static str>), Thrown> {
        let model = self.job_get("model").as_str().unwrap_or("").to_owned();
        let check = |w: &Self| !guarded || w.sample(index, abort);
        self.unload_all(hold, 1).await?;
        let options = self.base_options(("ctx-size", plain(ctx)));
        match self
            .maint
            .apply_unlocked(hold, &model, &self.base_revision(), &options, true)
            .await
        {
            Applied::Ok => {}
            Applied::Refused(e) => {
                return Err(Thrown::Fatal(
                    e.unwrap_or_else(|| "Could not write the test profile.".into()),
                ))
            }
            Applied::Threw(e) => return Err(Thrown::Other(e)),
        }
        let revision = self
            .current_revision()
            .await
            .ok_or_else(|| Thrown::Other("Could not read models.ini.".into()))?;
        self.job_set("lastRevision", JValue::Str(revision));
        // #871: a restart during the load/test below must find the revision.
        let _ = self.save().await;
        let started = self
            .request(
                hold,
                reqwest::Method::POST,
                "/models/load",
                Some(&json!({ "model": model })),
                LOAD_REQUEST,
                Some(abort),
            )
            .await?;
        if !started.ok {
            return Ok((
                "failed",
                Some("The engine refused to load the model at this size.".into()),
                Some("load"),
            ));
        }
        let deadline = tokio::time::Instant::now() + self.limits.load;
        loop {
            if self.cancelled() {
                return Err(Thrown::Cancelled);
            }
            if !check(self) || self.floor_hit(index) {
                return Ok(("failed", Some(self.oom()), Some("oom")));
            }
            if tokio::time::Instant::now() > deadline {
                return Ok((
                    "failed",
                    Some("Loading did not finish in time.".into()),
                    Some("timeout"),
                ));
            }
            let rows = match self.listing(hold, Some(abort)).await {
                Ok(r) => r,
                Err(_) if self.floor_hit(index) => Vec::new(),
                Err(e) => return Err(e),
            };
            if rows.iter().any(|m| {
                id_of(m) != Some(model.as_str())
                    && matches!(status_of(m), Some("loaded" | "loading"))
            }) {
                return Err(Thrown::Fatal(ANOTHER_CLIENT.into()));
            }
            let row = rows.iter().find(|m| id_of(m) == Some(model.as_str()));
            match row {
                Some(r) if status_of(r) == Some("loaded") => {
                    if !check(self) {
                        return Ok(("failed", Some(self.oom()), Some("oom")));
                    }
                    break;
                }
                None => {
                    return Ok((
                        "failed",
                        Some("The model failed to load at this size.".into()),
                        Some("load"),
                    ))
                }
                Some(r)
                    if r.get("status")
                        .and_then(|s| s.get("failed"))
                        .is_some_and(truthy)
                        || status_of(r) == Some("unloaded") =>
                {
                    return Ok((
                        "failed",
                        Some("The model failed to load at this size.".into()),
                        Some("load"),
                    ))
                }
                _ => {}
            }
            tokio::time::sleep(self.limits.poll).await;
        }
        if kind == Kind::Load {
            let smoke = self
                .request(
                    hold,
                    reqwest::Method::POST,
                    "/v1/chat/completions",
                    Some(&json!({
                        "model": model, "temperature": 0, "stream": false,
                        "chat_template_kwargs": { "enable_thinking": false },
                        "messages": [{ "role": "user", "content": "Reply with the single word OK." }],
                        "max_tokens": 16
                    })),
                    self.limits.smoke,
                    Some(abort),
                )
                .await?;
            if self.floor_hit(index) {
                return Ok(("failed", Some(self.oom()), Some("oom")));
            }
            let answered = smoke.ok
                && smoke
                    .body
                    .as_ref()
                    .and_then(|b| b.get("choices"))
                    .is_some_and(Value::is_array);
            return Ok(if answered {
                ("passed", None, None)
            } else {
                (
                    "failed",
                    Some("The loaded model did not answer a short request.".into()),
                    Some("load"),
                )
            });
        }
        // Near-capacity recall test against the per-slot window, streamed so progress can be watched.
        let parallel = match self.job_get("base").get("parallel") {
            v if v.truthy() => v.clone(),
            _ => self
                .preset_option(&model, "parallel")
                .await
                .map_or(JValue::Undefined, JValue::Str),
        };
        let slots = {
            let n = number(&parallel);
            if n.is_nan() || n == 0.0 {
                1.0
            } else {
                n.max(1.0)
            }
        };
        let target = ((ctx / slots) * 0.9).floor() - 256.0;
        let marker = format!("CAL-{}-{}", random_int(), random_int());
        let per_line = self.tokens_per_pad_line(hold, &model, abort).await?;
        let repeats = (target / per_line).floor().max(1.0);
        let budget_s = self
            .job_get("promptBudgetSeconds")
            .as_num()
            .unwrap_or(120.0);
        let content = format!(
            "Remember the start marker: {marker}.\n{}\nReturn only the start marker.",
            PAD.repeat(repeats as usize)
        );
        let result = self
            .stream_long(hold, &model, &content, abort, index, budget_s * 1000.0)
            .await?;
        if self.floor_hit(index) {
            return Ok(("failed", Some(self.oom()), Some("oom")));
        }
        match result {
            Long::Done(LongOutcome::OverBudget { predicted, eta_ms }) => Ok((
                "failed",
                Some(over_budget_reason(predicted, eta_ms, budget_s)),
                Some("time"),
            )),
            Long::Done(LongOutcome::EngineError { .. }) => Ok((
                "failed",
                Some("The engine reported an error during the long prompt.".into()),
                Some("oom"),
            )),
            Long::Failed(error) => Ok(("failed", Some(error), Some("oom"))),
            Long::Done(LongOutcome::Passed {
                text,
                prompt_tokens,
                prompt_per_second,
                prompt_seconds,
            }) => {
                self.with_step(index, |r| {
                    wtf16::set(
                        r,
                        "promptTokens",
                        prompt_tokens.map_or(JValue::Null, JValue::Num),
                    );
                    if let Some(p) = prompt_per_second.filter(|p| *p != 0.0 && !p.is_nan()) {
                        wtf16::set(r, "promptPerSecond", JValue::Num(js_round(p)));
                    }
                    if prompt_seconds != 0.0 && !prompt_seconds.is_nan() {
                        wtf16::set(r, "promptSeconds", JValue::Num(prompt_seconds));
                    }
                });
                if !text.contains(&marker) {
                    return Ok((
                        "failed",
                        Some(
                            "The model did not recall the marker from the start of the prompt."
                                .into(),
                        ),
                        Some("recall"),
                    ));
                }
                if prompt_tokens.is_some_and(|t| t != 0.0 && !t.is_nan() && t < target * 0.8) {
                    return Ok((
                        "failed",
                        Some("The engine accepted fewer prompt tokens than requested.".into()),
                        Some("recall"),
                    ));
                }
                Ok(("passed", None, None))
            }
        }
    }

    async fn preset_option(&self, model: &str, key: &'static str) -> Option<String> {
        let ini = self.maint.ini.clone()?;
        let model = model.to_owned();
        tokio::task::spawn_blocking(move || {
            let p = ini.read().ok()?;
            let (_, _, options) = p.span(&model)?;
            options
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        })
        .await
        .ok()
        .flatten()
    }

    async fn tokens_per_pad_line(
        &self,
        hold: &Hold,
        model: &str,
        abort: &Abort,
    ) -> Result<f64, Thrown> {
        let r = self
            .request(
                hold,
                reqwest::Method::POST,
                "/tokenize",
                Some(&json!({ "model": model, "content": PAD.repeat(50) })),
                TOKENIZE,
                Some(abort),
            )
            .await;
        if let Err(Thrown::Other(m)) = &r {
            if abort.aborted() {
                return Err(Thrown::Other(m.clone()));
            }
        }
        if let Ok(r) = r {
            let n = r
                .body
                .as_ref()
                .and_then(|b| b.get("tokens"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if r.ok && n > 0 {
                return Ok(n as f64 / 50.0);
            }
        }
        Ok(16.0)
    }

    /// `streamLong(...)`: the streamed long prompt, read with [`LongPrompt`]. Node stops a stream
    /// once it has run 1.5 times the budget, but only when the next event arrives; here the stream
    /// also has that long by the wall clock, so an engine that goes quiet ends the test the same way
    /// (over budget: a failed long test) and the run stays within [`Worker::step_cost`].
    async fn stream_long(
        &self,
        hold: &Hold,
        model: &str,
        content: &str,
        abort: &Abort,
        index: usize,
        budget_ms: f64,
    ) -> Result<Long, Thrown> {
        let started = now_ms();
        let allowance = Duration::from_secs_f64((budget_ms * 1.5 / 1000.0).max(0.0));
        let read = self.stream_long_inner(hold, model, content, abort, index, budget_ms, started);
        match tokio::time::timeout(allowance, read).await {
            Ok(read) => read,
            Err(_) => {
                self.show_progress(index, &Progress::default());
                Ok(Long::Done(LongOutcome::OverBudget {
                    predicted: false,
                    eta_ms: now_ms() - started,
                }))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn stream_long_inner(
        &self,
        hold: &Hold,
        model: &str,
        content: &str,
        abort: &Abort,
        index: usize,
        budget_ms: f64,
        started: f64,
    ) -> Result<Long, Thrown> {
        let Some(router) = self.maint.router.as_ref() else {
            return Ok(Long::Failed("The long prompt could not be sent.".into()));
        };
        let body = json!({
            "model": model, "stream": true, "return_progress": true, "temperature": 0,
            "cache_prompt": false, "max_tokens": 64,
            "chat_template_kwargs": { "enable_thinking": false },
            "messages": [{ "role": "user", "content": content }]
        });
        let token = hold.token();
        let sent = Self::race(
            Some(abort),
            router.stream(
                "/v1/chat/completions",
                &body,
                token.as_deref(),
                self.limits.long,
            ),
        )
        .await?;
        let mut res = match sent {
            Ok(r) => r,
            Err(_) => return Ok(Long::Failed("The long prompt could not be sent.".into())),
        };
        if !res.status().is_success() {
            return Ok(Long::Failed(format!(
                "The engine rejected the long prompt (HTTP {}).",
                res.status().as_u16()
            )));
        }
        let mut reader = LongPrompt::new(budget_ms, started);
        let mut progress = Progress::default();
        loop {
            let chunk = Self::race(Some(abort), res.chunk()).await?;
            let chunk = match chunk {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(_) => return Err(Thrown::Other("The long prompt stream failed.".into())),
            };
            let fed = reader.feed(&chunk, &mut now_ms, &mut progress);
            self.show_progress(index, &progress);
            match fed {
                Feed::Continue { save } => {
                    if save {
                        let _ = self.save().await;
                    }
                }
                // Dropping the response closes the request, as Node's abort does.
                Feed::Stop(outcome) => {
                    progress.eta_seconds = None;
                    self.show_progress(index, &progress);
                    return Ok(Long::Done(outcome));
                }
            }
        }
        let outcome = reader.finish(now_ms(), &mut progress);
        self.show_progress(index, &progress);
        Ok(Long::Done(outcome))
    }

    fn show_progress(&self, index: usize, p: &Progress) {
        self.with_step(index, |r| {
            if let Some(v) = p.progress {
                wtf16::set(r, "progress", JValue::Num(v));
            }
            match p.eta_seconds {
                Some(v) => wtf16::set(r, "etaSeconds", JValue::Num(v)),
                None => r.retain(|(k, _)| k != "etaSeconds"),
            }
        });
    }

    // ── The run ──────────────────────────────────────────────────────────────

    /// `compareAndLaunch(...)` past the route's checks: the hold, the job saved, the run spawned.
    /// The public job, or why nothing started.
    pub async fn launch(self: &Arc<Self>, model: &str, plan: Plan) -> Result<JValue, Refusal> {
        let busy = self.with(|i| {
            let busy =
                i.launching || i.active || i.state.job().get("status").as_str() == Some("running");
            if !busy {
                i.launching = true;
            }
            busy
        });
        if busy {
            return Err(Refusal::Running);
        }
        let launched = match self.take_hold(model, &plan).await {
            Ok(hold) => self.start(hold, model, plan).await,
            Err(r) => Err(r),
        };
        self.with(|i| i.launching = false);
        launched
    }

    /// `maintenance.hold(...)` for a run of `model`.
    /// A recovered job the state file does not have yet is saved first: a launch that cannot save
    /// it is refused before any hold (its own first save would fail the same way).
    pub(super) async fn take_hold(&self, model: &str, plan: &Plan) -> Result<Hold, Refusal> {
        if self.maint.router.is_none() || self.maint.ini.is_none() {
            return Err(Refusal::Unavailable);
        }
        if self.unsaved() && self.save_recovered().await.is_err() {
            return Err(Refusal::Unavailable);
        }
        let reason = format!("Chat is paused while noevia calibrates {model}. It will be available again when calibration finishes or is cancelled.");
        let live = self.live_models().await;
        // The caller holds the launch slot: no other run reads this until its own launch.
        self.with(|i| i.first_unload = live);
        match self
            .maint
            .acquire_for_run(&reason, self.worst_case(plan, live))
            .await
        {
            Ok(h) => Ok(h),
            Err(HoldRefusal::OverLimit) => Err(Refusal::Limit),
            Err(e) => Err(Refusal::Hold(hold_facts(&e))),
        }
    }

    /// The models live now (loaded, loading or sleeping), at least one: what the run's first unload
    /// is allowed to ask to unload. An unreadable listing counts one; the run's own listing then
    /// fails the same way, or finds what it may unload in that time.
    async fn live_models(&self) -> u32 {
        let Some(router) = self.maint.router.as_ref() else {
            return 1;
        };
        let Ok(reply) = router
            .get("/models", None, super::router::DEFAULT_TIMEOUT)
            .await
        else {
            return 1;
        };
        let live = super::router::rows(&reply.body).map_or(0, |rows| {
            rows.iter()
                .filter(|m| matches!(status_of(m), Some("loaded" | "loading" | "sleeping")))
                .count()
        });
        u32::try_from(live).unwrap_or(u32::MAX).max(1)
    }

    /// A poll loop to `limit`: it can overrun by one sleep and one listing.
    fn polled(&self, limit: Duration) -> Duration {
        limit + self.limits.poll + super::router::DEFAULT_TIMEOUT
    }

    /// The longest one step can take, from the limits and request timeouts it uses: the memory
    /// floor wait (30 settles), unloading everything (the listing, one unload request, then polling
    /// to the unload limit), the profile write's reload, Node's load decision, the load request and
    /// its polling to the load limit, then the smoke request (a load check) or the tokenize request and the long prompt (a
    /// long test, stopped at 1.5 times `budget_s`), the wait for the backend after a failed step, and
    /// the end-of-step unload (its request, then polling). Each is enforced: every request has its
    /// timeout, every poll its limit, and the long prompt its 1.5x wall-clock stop
    /// ([`Worker::stream_long`]).
    fn step_cost(&self, kind: Kind, budget_s: f64) -> Duration {
        let l = &self.limits;
        let listing = super::router::DEFAULT_TIMEOUT;
        let settle = l.memory_settle * 30;
        let unload_all = self.unload_allowance(1);
        let load = LOAD_CHECK + LOAD_REQUEST + self.polled(l.load);
        let body = match kind {
            Kind::Load => l.smoke,
            Kind::Long => TOKENIZE + Duration::from_secs_f64(budget_s.max(0.0) * 1.5),
        };
        let backend = l.recover + l.backend_poll + listing;
        let unload = l.unload + self.polled(l.unload);
        settle + unload_all + RELOAD + load + body + backend + unload
    }

    /// The longest `plan` can run, the hard cap its lease asks for: every load check (doublings to
    /// the top of its ladder, then three bisections) and at most seven long-prompt tests, each at
    /// [`Worker::step_cost`]; the first unload (one request for each of the `live` models counted at
    /// launch) and the one after the search; the calibrated profile's write, load decision, load and
    /// `/props` read; a restore's reload; and a 15-minute margin.
    pub fn worst_case(&self, plan: &Plan, live: u32) -> Duration {
        let grid = ladder(plan.native);
        let (Some(start), Some(&top)) = (super::calibration::start_of(&grid), grid.last()) else {
            return RUN_MARGIN;
        };
        let (loads, longs) = Self::step_counts(&grid, start, top);
        let l = &self.limits;
        let budget = plan.prompt_budget_seconds;
        let steps =
            self.step_cost(Kind::Load, budget) * loads + self.step_cost(Kind::Long, budget) * longs;
        let first_unload = self.unload_allowance(live);
        let last_unload = self.unload_allowance(1);
        let finish = RELOAD + LOAD_CHECK + LOAD_REQUEST + self.polled(l.load) + PROPS;
        steps + first_unload + last_unload + finish + RELOAD + RUN_MARGIN
    }

    /// Load checks (the doublings from `start` to `top`, then three bisections) and long tests (at
    /// most seven, one per ladder size from `start`).
    fn step_counts(grid: &[f64], start: f64, top: f64) -> (u32, u32) {
        let mut loads: u32 = 1;
        let mut ctx = start;
        while ctx < top {
            ctx = grid
                .iter()
                .copied()
                .find(|&c| c >= ctx * 2.0)
                .unwrap_or(top);
            loads += 1;
        }
        loads += 3;
        let longs = u32::try_from(grid.iter().filter(|&&c| c >= start).count().min(7)).unwrap_or(7);
        (loads, longs)
    }

    fn lease_lost(&self) -> bool {
        self.with(|i| i.lease_lost)
    }

    /// The hold stopped excluding other clients: stop as a cancel does, and remember why.
    fn lose_exclusion(&self) {
        self.with(|i| {
            i.lease_lost = true;
            i.cancel_requested = true;
            if let Some(a) = &i.inflight {
                a.abort();
            }
        });
    }

    /// The job created and saved under `hold`, and its run spawned; the public job.
    pub(super) async fn start(
        self: &Arc<Self>,
        hold: Hold,
        model: &str,
        plan: Plan,
    ) -> Result<JValue, Refusal> {
        let guard = if (self.memory)().is_none() {
            "unavailable"
        } else {
            "active"
        };
        let job = JValue::obj([
            ("id", JValue::Str(uuid())),
            ("model", JValue::Str(model.to_owned())),
            (
                "promptBudgetSeconds",
                JValue::Num(plan.prompt_budget_seconds),
            ),
            ("native", JValue::Num(plan.native)),
            ("base", JValue::Obj(plan.base)),
            ("status", JValue::Str("running".into())),
            ("phase", JValue::Str("Preparing".into())),
            ("startedAt", JValue::Num(now_ms())),
            ("steps", JValue::Arr(Vec::new())),
            ("memoryFloorGib", JValue::Num(self.floor_gib)),
            ("memoryGuard", JValue::Str(guard.into())),
        ]);
        let previous = self.with(|i| {
            let p = (i.state.job().clone(), i.cancel_requested, i.job_object);
            i.cancel_requested = false;
            i.state.set_job(job);
            i.job_object = i.next_object;
            i.next_object += 1;
            p
        });
        let (previous, previous_cancel, previous_object) = (previous.0, previous.1, previous.2);
        if self.save().await.is_err() {
            self.with(|i| {
                i.state.set_job(previous);
                i.cancel_requested = previous_cancel;
                i.job_object = previous_object;
            });
            hold.release().await;
            return Err(Refusal::Unavailable);
        }
        // The file now has this job: nothing recovered is left to save.
        self.with(|i| {
            i.unsaved = false;
            i.active = true;
        });
        let worker = Arc::clone(self);
        let object = self.with(|i| i.job_object);
        // Capture the armed guard before spawning, so even a never-polled task is reconciled.
        let guard = RunGuard {
            worker: Arc::clone(&worker),
            object,
        };
        tokio::spawn(async move {
            let _guard = guard;
            worker.run(hold).await;
        });
        Ok(self.job())
    }

    /// `run(job, release)`.
    async fn run(self: Arc<Self>, hold: Hold) {
        #[cfg(test)]
        if self.panic_on_run.swap(false, Ordering::AcqRel) {
            panic!("synthetic run panic");
        }
        self.with(|i| i.lease_lost = false);
        let outcome = {
            // The run never goes on without its exclusion: a lost hold stops it as a cancel would.
            let watch = async {
                loop {
                    tokio::time::sleep(self.limits.exclusion_poll).await;
                    if !hold.excluded() {
                        self.lose_exclusion();
                        return;
                    }
                }
            };
            let inner = self.run_inner(&hold);
            tokio::pin!(watch, inner);
            let mut watching = true;
            loop {
                tokio::select! {
                    o = &mut inner => break o,
                    () = &mut watch, if watching => watching = false,
                }
            }
        };
        // A step can see the loss first (its reload refused under the ended lease) and fail before
        // the watch notices: however the run ended, a hold that no longer excludes is a lost one.
        if !hold.excluded() {
            self.with(|i| i.lease_lost = true);
        }
        let outcome = match outcome {
            Ok(_) if self.lease_lost() => Err(Thrown::Cancelled),
            o => o,
        };
        let model = self.job_get("model").as_str().unwrap_or("").to_owned();
        let budget = self.job_get("promptBudgetSeconds");
        if let Err(e) = outcome {
            let lost = self.lease_lost();
            let e = if lost {
                Thrown::Fatal(LEASE_LOST.into())
            } else {
                e
            };
            let cancelled = e == Thrown::Cancelled;
            self.job_set(
                "status",
                JValue::Str(
                    if lost {
                        "interrupted"
                    } else if cancelled {
                        "cancelled"
                    } else {
                        "failed"
                    }
                    .into(),
                ),
            );
            self.job_set(
                "phase",
                JValue::Str(
                    if lost {
                        "Interrupted"
                    } else if cancelled {
                        "Cancelled"
                    } else {
                        "Failed"
                    }
                    .into(),
                ),
            );
            if let Thrown::Fatal(m) | Thrown::Other(m) = &e {
                let m = if m.is_empty() {
                    "Calibration failed."
                } else {
                    m.as_str()
                };
                self.job_set("error", JValue::Str(m.to_owned()));
            }
            // Put the original profile back; never overwrite a file someone else changed since.
            let original = self.job_get("originalText");
            let last = self.job_get("lastRevision");
            if !original.is_nullish() && last.truthy() {
                let restored = match (original.as_str(), last.as_str()) {
                    (Some(text), Some(rev))
                        if self.current_revision().await.as_deref() == Some(rev) =>
                    {
                        self.restore(&hold, rev, text).await
                    }
                    _ => false,
                };
                self.job_set("restored", JValue::Bool(restored));
                if !restored {
                    let error = match self.job_get("error") {
                        JValue::Str(s) if !s.is_empty() => s,
                        _ if cancelled => "Calibration cancelled.".into(),
                        _ => "Calibration failed.".into(),
                    };
                    self.job_set("error", JValue::Str(format!("{error}{NOT_RESTORED}")));
                }
            }
            // Evidence identity must reflect the restored profile, so record only after restoring.
            if matches!(e, Thrown::Fatal(_)) && !lost {
                let error = self.job_get("error").as_str().unwrap_or("").to_owned();
                let limitation = wtf16::slice(&error, 200);
                let record = JValue::obj([
                    ("category", JValue::Str("context_capacity".into())),
                    ("result", JValue::Str("failed".into())),
                    ("value", JValue::Null),
                    (
                        "suite",
                        JValue::obj([
                            ("name", JValue::Str("native-calibration".into())),
                            ("version", JValue::Num(1.0)),
                        ]),
                    ),
                    ("source", JValue::Str("calibration".into())),
                    ("limitations", JValue::Arr(vec![JValue::Str(limitation)])),
                ]);
                let _ = self.maint.record_evidence(&model, &record).await;
            }
        } else if let Some(entry) = outcome.ok().flatten() {
            // After the lease check: a run whose hold ended after it passed was restored above and
            // must not leave a verified size in the history that the profile no longer has.
            self.with(|i| i.state.push_history(&model, entry.clone()));
            let ctx = entry.get("verifiedCtx").clone();
            let record = JValue::obj([
                ("category", JValue::Str("context_capacity".into())),
                ("result", JValue::Str("passed".into())),
                (
                    "value",
                    JValue::obj([
                        ("ctx", ctx),
                        ("appliedCtx", entry.get("appliedCtx").clone()),
                        ("slots", entry.get("slots").clone()),
                    ]),
                ),
                (
                    "suite",
                    JValue::obj([
                        ("name", JValue::Str("native-calibration".into())),
                        ("version", JValue::Num(1.0)),
                    ]),
                ),
                ("source", JValue::Str("calibration".into())),
                (
                    "limitations",
                    JValue::Arr(vec![JValue::Str(format!(
                        "prompt budget {} s",
                        plain(number(&budget))
                    ))]),
                ),
            ]);
            let _ = self.maint.record_evidence(&model, &record).await;
        }
        self.finalize(hold).await;
    }

    async fn finalize(self: &Arc<Self>, mut hold: Hold) {
        let until = Instant::now() + self.limits.finalize;
        let (mut state, object) = self.with(|i| {
            // Cancellation already handled by run's outcome must not turn cancelled into failed.
            if matches!(
                i.state.job().get("status").as_str(),
                Some("cancelled" | "interrupted")
            ) {
                i.cancel_requested = false;
            }
            (i.state.clone(), i.job_object)
        });
        finish_snapshot(&mut state, false);
        let mut saved = false;
        let mut wait = self.limits.save_retry.max(Duration::from_millis(1));
        loop {
            let interrupted = self.cancelled() || hold.lost() || !hold.excluded();
            if Instant::now() >= until
                || (interrupted && state.job().get("status").as_str() == Some("passed"))
            {
                break;
            }
            if self
                .write_state(state.clone(), object, Some(until))
                .await
                .is_ok()
            {
                saved = true;
                break;
            }
            tokio::time::sleep(wait.min(until.saturating_duration_since(Instant::now()))).await;
            wait = (wait * 2).min(Duration::from_secs(1));
        }
        let released = tokio::time::timeout_at(until.into(), hold.release_node_lease())
            .await
            .is_ok()
            && hold.node_release_confirmed();
        let failed = !saved
            || !released
            || self.cancelled()
            || (hold.lost() && state.job().get("status").as_str() == Some("passed"));
        if failed {
            finish_snapshot(&mut state, true);
            saved = self
                .write_state(
                    state.clone(),
                    object,
                    Some(Instant::now() + FAILURE_SAVE_GRACE),
                )
                .await
                .is_ok();
        }
        self.with(|i| {
            drop(hold);
            i.state = state;
            i.cancel_requested = false;
            i.lease_lost = false;
            i.active = false;
            i.unsaved = !saved;
        });
        if !saved {
            self.spawn_save_retry();
        }
    }

    /// Put `text` back if models.ini is still at `revision`, then reload the router.
    ///
    /// After a lost hold this runs without exclusion, on purpose: the profile under test must not
    /// outlive the run, and the file write is still compare-and-swap on the revision, so nothing
    /// another writer saved is overwritten. Only the raw reload is unexcluded: once Node has ended
    /// the lease it can admit a chat, which that reload may interrupt. Leaving a half-tested profile
    /// live would be worse, so the trade-off is kept.
    async fn restore(&self, hold: &Hold, revision: &str, text: &str) -> bool {
        let Some(ini) = self.maint.ini.clone() else {
            return false;
        };
        let c = super::ini::Candidate {
            base_revision: revision.to_owned(),
            text: text.to_owned(),
            before: String::new(),
        };
        if ini.commit_reconciled(&c, true).await.is_err() {
            return false;
        }
        let _ = self
            .request(
                hold,
                reqwest::Method::GET,
                "/models?reload=1",
                None,
                RELOAD,
                None,
            )
            .await;
        // The raw reload Node sends (no reload-guard record or forget, as in Node's run()).
        true
    }

    /// The search and the final write; the history entry on a pass.
    async fn run_inner(self: &Arc<Self>, hold: &Hold) -> Result<Option<JValue>, Thrown> {
        let live = self.with(|i| i.first_unload);
        self.unload_all(hold, live).await?;
        let ini = self
            .maint
            .ini
            .clone()
            .ok_or_else(|| Thrown::Other("Calibration failed.".into()))?;
        let snapshot = tokio::task::spawn_blocking(move || ini.snapshot())
            .await
            .map_err(|_| Thrown::Other("Calibration failed.".into()))?
            .map_err(|t| Thrown::Other(t.message))?;
        self.job_set("originalText", JValue::Str(snapshot.text));
        self.job_set("originalRevision", JValue::Str(snapshot.revision));
        let _ = self.save().await;
        let native = self.job_get("native").as_num().unwrap_or(0.0);
        let budget = self
            .job_get("promptBudgetSeconds")
            .as_num()
            .unwrap_or(120.0);
        let grid = ladder(native);
        let mut engine = Engine { worker: self, hold };
        let load = bound_load(&grid, &mut engine).await.map_err(from_stop)?;
        self.job_set("result", JValue::obj([("loadCtx", JValue::Num(load))]));
        let chosen = verify_long(&grid, load, budget, &mut engine)
            .await
            .map_err(from_stop)?;
        self.result_set("verifiedCtx", chosen);
        self.unload_all(hold, 1).await?;
        let model = self.job_get("model").as_str().unwrap_or("").to_owned();
        if self.lease_lost() {
            return Err(Thrown::Cancelled);
        }
        let options = self.base_options(("ctx-size", plain(chosen)));
        match self
            .maint
            .apply_unlocked(hold, &model, &self.base_revision(), &options, true)
            .await
        {
            Applied::Ok => {}
            Applied::Refused(e) => {
                return Err(Thrown::Fatal(e.unwrap_or_else(|| {
                    "Could not save the calibrated profile.".into()
                })))
            }
            Applied::Threw(e) => return Err(Thrown::Other(e)),
        }
        let revision = self
            .current_revision()
            .await
            .ok_or_else(|| Thrown::Other("Could not read models.ini.".into()))?;
        self.job_set("lastRevision", JValue::Str(revision));
        let _ = self.save().await;
        self.result_set("appliedCtx", chosen);
        // Leave the model loaded with its calibrated profile, ready for the next chat.
        self.job_set(
            "phase",
            JValue::Str("Loading the calibrated profile".into()),
        );
        let _ = self.save().await;
        if self.lease_lost() {
            return Err(Thrown::Cancelled);
        }
        let loaded = self.load_and_wait(hold, &model).await;
        self.with(|i| {
            if let Some(JValue::Obj(r)) = i
                .state
                .job_mut()
                .and_then(|j| j.iter_mut().find(|(k, _)| k == "result").map(|(_, v)| v))
            {
                wtf16::set(r, "loaded", JValue::Bool(loaded));
            }
        });
        if !loaded {
            return Err(Thrown::Fatal("The calibrated profile did not load.".into()));
        }
        self.job_set("status", JValue::Str("passed".into()));
        self.job_set("phase", JValue::Str("Done".into()));
        let props = self
            .request(hold, reqwest::Method::GET, "/props", None, PROPS, None)
            .await
            .ok();
        let build = props
            .and_then(|p| p.body)
            .and_then(|b| b.get("build_info").cloned())
            .filter(truthy)
            .map_or(JValue::Null, |b| {
                wtf16::parse(&b.to_string()).unwrap_or(JValue::Null)
            });
        let parallel = self
            .preset_option(&model, "parallel")
            .await
            .map_or(JValue::Undefined, JValue::Str);
        let slots = {
            let n = number(&parallel);
            if n.is_nan() || n == 0.0 {
                1.0
            } else {
                n.max(1.0)
            }
        };
        let mut entry = vec![
            ("at".to_owned(), JValue::Num(now_ms())),
            ("promptBudgetSeconds".to_owned(), JValue::Num(budget)),
        ];
        if let JValue::Obj(result) = self.job_get("result") {
            entry.extend(result);
        }
        entry.push(("build".into(), build));
        entry.push(("slots".into(), JValue::Num(slots)));
        // The history entry is pushed by run(), only once the lease check has passed.
        Ok(Some(JValue::Obj(entry)))
    }

    fn result_set(&self, key: &str, value: f64) {
        self.with(|i| {
            if let Some(job) = i.state.job_mut() {
                let mut result = match job
                    .iter()
                    .find(|(k, _)| k == "result")
                    .map(|(_, v)| v.clone())
                {
                    Some(JValue::Obj(r)) => r,
                    _ => Vec::new(),
                };
                wtf16::set(&mut result, key, JValue::Num(value));
                wtf16::set(job, "result", JValue::Obj(result));
            }
        });
    }
}

enum Long {
    Done(LongOutcome),
    Failed(String),
}

fn from_stop(s: Stop) -> Thrown {
    match s {
        Stop::Cancelled => Thrown::Cancelled,
        Stop::Fatal(m) => Thrown::Fatal(m),
    }
}

fn plain(n: f64) -> String {
    super::calibration::plain(n)
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// The hold refusal as the launch bridge reported it (`{ status, publicMessage, code }`).
fn hold_facts(e: &HoldRefusal) -> JValue {
    // Node: a busy refusal is `busy()` (409, no message, no code); any other gate refusal is the
    // GateError itself. Anything else had no integer status there (null).
    let (status, message, code) = match e {
        HoldRefusal::Busy => (JValue::Num(409.0), JValue::Null, JValue::Null),
        HoldRefusal::Gate {
            status,
            code,
            message,
        } => (
            JValue::Num(f64::from(*status)),
            JValue::Str(wtf16::slice(message, 512)),
            if (1..=80).contains(&code.len())
                && code
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-'))
            {
                JValue::Str(code.clone())
            } else {
                JValue::Null
            },
        ),
        HoldRefusal::ShortLease => (
            JValue::Num(503.0),
            JValue::Str(
                "the maintenance lease is shorter than this calibration could take.".into(),
            ),
            JValue::Null,
        ),
        HoldRefusal::NeedsGate | HoldRefusal::NodeUnavailable | HoldRefusal::OverLimit => {
            (JValue::Null, JValue::Null, JValue::Null)
        }
    };
    JValue::obj([
        ("status", status),
        ("publicMessage", message),
        ("code", code),
    ])
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn uuid_has_the_v4_shape() {
        let u = uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(["8", "9", "a", "b"].contains(&&u[19..20]));
    }

    #[test]
    fn public_job_never_carries_the_original_text() {
        let job = wtf16::parse(
            r#"{"id":"j","model":"m","status":"running","originalText":"[m]\n","steps":[]}"#,
        )
        .unwrap();
        let text = wtf16::stringify(&public(&job)).unwrap();
        assert!(!text.contains("originalText"));
        assert!(text.contains("\"steps\":[]"));
    }

    #[tokio::test]
    async fn abort_wakes_a_waiter_and_stays_aborted() {
        let a = Arc::new(Abort::default());
        let b = Arc::clone(&a);
        let waiter = tokio::spawn(async move { b.wait().await });
        tokio::task::yield_now().await;
        a.abort();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(a.aborted());
        tokio::time::timeout(Duration::from_secs(5), a.wait())
            .await
            .unwrap();
    }

    /// A synthetic llama.cpp router and inference gate: loads pass up to 16K, long prompts up to
    /// 8K (an error event above), `stall` makes the long prompt hang until the client goes away.
    async fn synthetic_engine(
        ini: std::path::PathBuf,
        stall: bool,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        synthetic_engine_with(
            ini,
            Synth {
                stall,
                ..Synth::default()
            },
        )
        .await
    }

    /// What the synthetic engine does besides passing loads to 16K and long prompts to 8K.
    #[derive(Default, Clone)]
    struct Synth {
        /// The long prompt answers only after 60 s (until the client goes away).
        stall: bool,
        /// The first `GET /props` sets it and answers late (a lease that ends after the pass).
        props: Option<Arc<AtomicBool>>,
        /// The long prompt answers 200 at once, then sends nothing, ever.
        silent: bool,
        /// Other models live besides the one under test (`extra-<n>`, each until it is unloaded).
        live: Option<Arc<std::sync::atomic::AtomicUsize>>,
        /// Node's load decision (this engine also stands in for Node's loopback listener): given the
        /// 1-based number of the ask and its body, the status and body it answers. None: no refusal.
        node: Option<NodeAnswer>,
    }

    type NodeAnswer = Arc<dyn Fn(usize, &Value) -> (u16, Value) + Send + Sync>;

    /// Node's load decision refusing as `overBudget()` does, with `code` (synthetic bodies in
    /// Node's shape: core llamacpp-manager.cjs).
    fn node_refusal(code: &str) -> Value {
        let error = match code {
            "inference_budget_unloaded" => "synthetic was unloaded by the memory safety net because measured inference memory ran more than the allowed margin over the 2 GiB budget. It stays unloaded until its settings or the budget change: lower its context or prompt cache, or raise the budget, in Settings → Models & routing.",
            "invalid_size" => "synthetic was not loaded: ctx-size must be a whole number (0 for the native context)",
            _ => "synthetic needs about 9.5 GiB to load (weights 4.5, context 4.0 at 8,192 tokens, prompt cache 0.5, runtime 0.5), above the 2 GiB inference memory budget. Lower its context or prompt cache, use a smaller quantization, or raise the budget in Settings → Models & routing.",
        };
        json!({"error": error, "code": code, "budgetGib": 2})
    }

    /// A body that never sends anything and never ends.
    struct Silent;
    impl http_body::Body for Silent {
        type Data = bytes::Bytes;
        type Error = std::convert::Infallible;
        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<bytes::Bytes>, Self::Error>>> {
            std::task::Poll::Pending
        }
    }

    async fn synthetic_engine_with(
        ini: std::path::PathBuf,
        synth: Synth,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        use axum::body::Body;
        use axum::http::{Request, Response};
        use http_body_util::BodyExt;
        let Synth {
            stall,
            props,
            silent,
            live,
            node,
        } = synth;
        let asks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let status = Arc::new(Mutex::new("unloaded".to_owned()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let ctx_of = move || -> f64 {
            std::fs::read_to_string(&ini)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.strip_prefix("ctx-size = "))
                .next_back()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0)
        };
        let app = axum::Router::new().fallback(move |req: Request<Body>| {
            let (status, log, ctx_of, props, live, node, asks) = (
                Arc::clone(&status),
                Arc::clone(&log),
                ctx_of.clone(),
                props.clone(),
                live.clone(),
                node.clone(),
                Arc::clone(&asks),
            );
            async move {
                let (parts, body) = req.into_parts();
                let path = parts.uri.path().to_owned();
                let body = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                log.lock().unwrap().push(format!("{} {}", parts.method, parts.uri));
                let json = |code: u16, v: Value| {
                    Response::builder().status(code).header("content-type", "application/json").body(Body::from(v.to_string())).unwrap()
                };
                match (parts.method.as_str(), path.as_str()) {
                    ("POST", "/internal/model-maintenance/load-refusal") => {
                        let n = asks.fetch_add(1, Ordering::AcqRel) + 1;
                        let (code, answer) = node.as_ref().map_or((200, json!({"refusal": null})), |f| f(n, &body));
                        json(code, answer)
                    }
                    ("POST", "/gate/v1/maintenance") => json(201, json!({"hold": "0123456789abcdef0123456789abcdef", "idle_ttl_ms": 60000})),
                    ("GET", "/gate/v1/status") => json(200, json!({"maintenance": {"held": false}})),
                    (_, p) if p.starts_with("/gate/") => json(200, json!({})),
                    ("GET", "/models") => {
                        let v = status.lock().unwrap().clone();
                        let failed = v == "failed";
                        let value = if failed { "unloaded" } else { v.as_str() };
                        let mut rows = vec![json!({"id": "synthetic", "status": {"value": value, "failed": failed}})];
                        let extra = live.as_ref().map_or(0, |l| l.load(Ordering::Acquire));
                        rows.extend((0..extra).map(|n| json!({"id": format!("extra-{n}"), "status": {"value": "loaded"}})));
                        json(200, json!({"data": rows}))
                    }
                    ("POST", "/models/unload") => {
                        match (body["model"].as_str(), &live) {
                            (Some(m), Some(l)) if m.starts_with("extra-") => {
                                let n = l.load(Ordering::Acquire);
                                l.store(n.saturating_sub(1), Ordering::Release);
                            }
                            _ => *status.lock().unwrap() = "unloaded".into(),
                        }
                        json(200, json!({"success": true}))
                    }
                    ("POST", "/models/load") => {
                        *status.lock().unwrap() = if ctx_of() <= 16384.0 { "loaded" } else { "failed" }.into();
                        json(200, json!({"success": true}))
                    }
                    ("POST", "/tokenize") => json(200, json!({"tokens": vec![1; 650]})),
                    ("GET", "/props") => {
                        if let Some(p) = &props {
                            if !p.swap(true, Ordering::AcqRel) {
                                tokio::time::sleep(Duration::from_millis(300)).await;
                            }
                        }
                        json(200, json!({"build_info": "b-synthetic"}))
                    }
                    ("POST", "/v1/chat/completions") if body["stream"] == Value::Bool(true) => {
                        let content = body["messages"][0]["content"].as_str().unwrap_or("").to_owned();
                        let marker: String = content
                            .strip_prefix("Remember the start marker: ")
                            .unwrap_or("")
                            .chars()
                            .take_while(|c| *c != '.')
                            .collect();
                        if stall {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
                        if silent {
                            return Response::builder().status(200).header("content-type", "text/event-stream").body(Body::new(Silent)).unwrap();
                        }
                        let events = if ctx_of() <= 8192.0 {
                            format!("data: {}\n\ndata: [DONE]\n\n", json!({"choices": [{"delta": {"content": marker}}]}))
                        } else {
                            format!("data: {}\n\n", json!({"error": {"code": 500, "message": "synthetic engine words"}}))
                        };
                        Response::builder().status(200).header("content-type", "text/event-stream").body(Body::from(events)).unwrap()
                    }
                    ("POST", "/v1/chat/completions") => json(200, json!({"choices": [{"message": {"content": "OK"}}]})),
                    _ => json(404, json!({"error": "not found"})),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (port, seen)
    }

    fn worker(dir: &std::path::Path, port: u16) -> Arc<Worker> {
        worker_with(dir, port, None)
    }

    /// `lease`: no gate admin token, and Node's lease route at this port instead (it answers the
    /// load decision too). With the gate, Node's load decision is the synthetic engine's.
    fn worker_with(dir: &std::path::Path, port: u16, lease: Option<u16>) -> Arc<Worker> {
        worker_node(dir, port, lease, lease.unwrap_or(port))
    }

    /// As [`worker_with`], with Node (its load decision) at `node`.
    fn worker_node(dir: &std::path::Path, port: u16, lease: Option<u16>, node: u16) -> Arc<Worker> {
        let mut env = vec![
            ("MODEL_MANAGER_KIND".to_owned(), "llamacpp".to_owned()),
            (
                "MODEL_MANAGER_BASE_URL".to_owned(),
                format!("http://127.0.0.1:{port}"),
            ),
            (
                "LLAMACPP_PRESET_PATH".to_owned(),
                dir.join("models.ini").display().to_string(),
            ),
            // `/models/synthetic.gguf` is `dir/synthetic.gguf` (evidence needs it to fingerprint).
            ("LLAMACPP_MODELS_PATH".to_owned(), dir.display().to_string()),
            ("UI_DATA_DIR".to_owned(), dir.display().to_string()),
        ];
        if lease.is_none() {
            env.push((
                "INFERENCE_GATE_ADMIN_TOKEN".to_owned(),
                "synthetic-admin".to_owned(),
            ));
        }
        env.push((
            "NOEVIA_LEGACY_UPSTREAM".to_owned(),
            format!("http://127.0.0.1:{node}"),
        ));
        env.push((
            "NOEVIA_PROJECT_STORE_TOKEN".to_owned(),
            "synthetic-store-token".to_owned(),
        ));
        let get = |k: &str| env.iter().find(|(e, _)| e == k).map(|(_, v)| v.clone());
        let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
        let sizing = Arc::new(crate::model_sizing::Sizing::from_lookup(
            engine.kind(),
            Arc::clone(&engine),
            &get,
        ));
        let maint = Arc::new(Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap());
        let limits = Limits {
            exclusion_poll: Duration::from_millis(20),
            poll: Duration::from_millis(10),
            unload: Duration::from_secs(5),
            recover: Duration::from_millis(500),
            backend_poll: Duration::from_millis(50),
            memory_poll: Duration::from_millis(50),
            memory_settle: Duration::from_millis(10),
            ..Limits::default()
        };
        Worker::new(maint, Arc::new(|| Some(64.0)), 2.0, limits)
    }

    async fn settled(w: &Worker) -> JValue {
        for _ in 0..600 {
            let job = w.job();
            // finishedAt is published only after persistence and local admission release.
            if job.get("status").as_str() != Some("running")
                && !matches!(job.get("finishedAt"), JValue::Undefined)
            {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the synthetic run did not finish: {:?}", w.job());
    }

    async fn released(log: &Mutex<Vec<String>>, prefix: &str) {
        for _ in 0..600 {
            if log.lock().unwrap().iter().any(|r| r.starts_with(prefix)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("remote release was not observed: {prefix}");
    }

    const INI: &str =
        "[*]\nn-gpu-layers = 99\n\n[synthetic]\nmodel = /models/synthetic.gguf\nctx-size = 4096\n";

    // REVIEW_REGRESSIONS_BEGIN
    async fn review_wait_outcome(w: &Worker) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while w.with(|i| i.state.job().get("status").as_str() == Some("running")) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn review_blocked_save() -> (tempfile::TempDir, Arc<Worker>, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let props = Arc::new(AtomicBool::new(false));
        let (port, _) = synthetic_engine_with(
            ini,
            Synth {
                props: Some(Arc::clone(&props)),
                ..Synth::default()
            },
        )
        .await;
        let w = worker(d.path(), port);
        let w = with_limits(
            &w,
            Limits {
                finalize: Duration::from_millis(300),
                save_retry: Duration::from_millis(5),
                ..w.limits
            },
        );
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !props.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let path = w.file.path.clone().unwrap();
        let backup = path.with_extension("review-backup");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        (d, w, backup)
    }

    fn review_failed(w: &Worker, job: &JValue) {
        assert_eq!(job.get("status").as_str(), Some("failed"), "{job:?}");
        assert_eq!(
            job.get("error").as_str(),
            Some("Calibration finalization failed.")
        );
        assert!(!matches!(job.get("finishedAt"), JValue::Undefined));
        assert!(!w.with(|i| i.active));
        assert!(!w.maint.local_held());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_persistent_final_save_failure_releases_admission() {
        let (_d, w, backup) = review_blocked_save().await;
        let job = tokio::time::timeout(Duration::from_secs(2), settled(&w))
            .await
            .expect("bounded finalization");
        review_failed(&w, &job);
        let path = w.file.path.as_ref().unwrap();
        std::fs::remove_dir(path).unwrap();
        std::fs::rename(backup, path).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while w.file.load().job().get("status").as_str() != Some("failed") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_cancel_during_finalization_is_terminal() {
        let (_d, w, _backup) = review_blocked_save().await;
        review_wait_outcome(&w).await;
        assert_eq!(w.job().get("status").as_str(), Some("running"));
        assert!(w.cancel().is_some(), "finalization is cancellable");
        let job = tokio::time::timeout(Duration::from_secs(2), settled(&w))
            .await
            .unwrap();
        review_failed(&w, &job);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_lease_loss_during_finalization_is_terminal() {
        let (_d, w, _backup) = review_blocked_save().await;
        review_wait_outcome(&w).await;
        w.lose_exclusion();
        let job = tokio::time::timeout(Duration::from_secs(2), settled(&w))
            .await
            .unwrap();
        review_failed(&w, &job);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_panic_reconciles_and_allows_restart() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, _) = synthetic_engine(ini, false).await;
        let w = worker(d.path(), port);
        let w = with_limits(
            &w,
            Limits {
                save_retry: Duration::from_millis(5),
                ..w.limits
            },
        );
        w.panic_on_run.store(true, Ordering::Release);
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        let job = tokio::time::timeout(Duration::from_secs(2), settled(&w))
            .await
            .expect("panic reconciled");
        review_failed(&w, &job);
        tokio::time::timeout(Duration::from_secs(3), async {
            while w.unsaved() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(w.file.load().job().get("status").as_str(), Some("failed"));
        w.launch("synthetic", plan(32768.0, 120.0))
            .await
            .expect("restart after panic");
        let _ = w.cancel();
        settled(&w).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_unavailable_delete_does_not_retain_admission() {
        use axum::{
            body::Body,
            http::{Request, Response},
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new().fallback(|r: Request<Body>| async move {
            let (code, body) = if r.method() == "DELETE" {
                (503, json!({"error":"private synthetic transport detail"}))
            } else if r.uri().path().ends_with("/lease") {
                (201, json!({"lease":"fedcba9876543210fedcba9876543210","ttlMs":180000,"renewMs":20,"hardMs":86400000}))
            } else { (200, json!({"refusal":null})) };
            Response::builder().status(code).body(Body::from(body.to_string())).unwrap()
        });
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (engine, _) = synthetic_engine(ini, false).await;
        let w = worker_with(d.path(), engine, Some(port));
        let w = with_limits(
            &w,
            Limits {
                finalize: Duration::from_millis(300),
                ..w.limits
            },
        );
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        let job = tokio::time::timeout(Duration::from_secs(2), settled(&w))
            .await
            .expect("bounded DELETE");
        review_failed(&w, &job);
        assert_eq!(w.file.load().job().get("status").as_str(), Some("failed"));
    }
    // REVIEW_REGRESSIONS_END

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_synthetic_run_searches_writes_the_verified_size_and_records_history() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, seen) = synthetic_engine(ini.clone(), false).await;
        let w = worker(d.path(), port);
        let plan = Plan {
            prompt_budget_seconds: 120.0,
            native: 32768.0,
            base: vec![("parallel".into(), JValue::Str("1".into()))],
        };
        let job = w.launch("synthetic", plan.clone()).await.unwrap();
        assert_eq!(job.get("status").as_str(), Some("running"));
        assert_eq!(w.launch("synthetic", plan).await, Err(Refusal::Running));
        let job = settled(&w).await;
        assert_eq!(job.get("status").as_str(), Some("passed"), "{job:?}");
        assert_eq!(job.get("result").get("loadCtx").as_num(), Some(16384.0));
        assert_eq!(job.get("result").get("verifiedCtx").as_num(), Some(8192.0));
        assert_eq!(job.get("result").get("loaded"), &JValue::Bool(true));
        let text = std::fs::read_to_string(&ini).unwrap();
        assert!(
            text.contains("ctx-size = 8192\n") && text.contains("cache-ram = 1024\n"),
            "{text}"
        );
        let saved = std::fs::read_to_string(
            d.path()
                .join(format!("native-calibration-{}.json", w.maint.suffix)),
        )
        .unwrap();
        assert!(!saved.contains("originalText"));
        assert!(!saved.contains("synthetic engine words"));
        let JValue::Arr(history) = w.history("synthetic") else {
            panic!("history")
        };
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].get("build").as_str(), Some("b-synthetic"));
        assert_eq!(history[0].get("slots").as_num(), Some(1.0));
        assert_eq!(history[0].get("verifiedCtx").as_num(), Some(8192.0));
        let JValue::Arr(steps) = job.get("steps") else {
            panic!("steps")
        };
        assert!(steps
            .iter()
            .all(|s| matches!(s.get("status").as_str(), Some("passed" | "failed"))));
        released(&seen, "DELETE /gate/v1/maintenance/").await;
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .any(|r| r == "DELETE /gate/v1/maintenance/0123456789abcdef0123456789abcdef"));
        // A new worker reads the same file (restart/rollback): the job and history are there.
        let again = worker(d.path(), port);
        assert_eq!(again.job().get("id"), job.get("id"));
        assert_eq!(again.recover().await, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_long_prompt_stops_the_run_and_restores_the_original_profile() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, _) = synthetic_engine(ini.clone(), true).await;
        let w = worker(d.path(), port);
        let plan = Plan {
            prompt_budget_seconds: 120.0,
            native: 16384.0,
            base: Vec::new(),
        };
        w.launch("synthetic", plan).await.unwrap();
        for _ in 0..600 {
            if w.job()
                .get("phase")
                .as_str()
                .is_some_and(|p| p.starts_with("Long-prompt"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let snapshot = w.cancel().unwrap();
        assert_eq!(snapshot.get("status").as_str(), Some("running"));
        let job = settled(&w).await;
        assert_eq!(job.get("status").as_str(), Some("cancelled"), "{job:?}");
        assert_eq!(job.get("restored"), &JValue::Bool(true));
        assert_eq!(std::fs::read_to_string(&ini).unwrap(), INI);
        assert!(w.cancel().is_none());
    }

    /// A running native job's state file, as the worker leaves it mid-step (its long prompt hangs).
    async fn native_running_state(d: &std::path::Path) -> (String, String) {
        let ini = d.join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, _) = synthetic_engine(ini.clone(), true).await;
        let w = worker(d, port);
        let plan = Plan {
            prompt_budget_seconds: 120.0,
            native: 16384.0,
            base: Vec::new(),
        };
        w.launch("synthetic", plan).await.unwrap();
        for _ in 0..600 {
            if w.job()
                .get("phase")
                .as_str()
                .is_some_and(|p| p.starts_with("Long-prompt"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let file = d.join(format!("native-calibration-{}.json", w.maint.suffix));
        let state = std::fs::read_to_string(&file).unwrap();
        let profile = std::fs::read_to_string(&ini).unwrap();
        w.cancel().unwrap();
        settled(&w).await;
        (state, profile)
    }

    fn keys(v: &JValue) -> Vec<String> {
        match v {
            JValue::Obj(f) => f.iter().map(|(k, _)| k.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// Rollback safety (tests/fixtures/calibration-recover-node.json): old Core's boot `recover()` on a
    /// state file this worker wrote mid-run. The fixture's input is native output (its shape is checked
    /// against a fresh run here), and native recovery gives Node's answers on every case.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn old_core_recovers_a_native_running_job_as_native_recovery_does() {
        let d = tempfile::tempdir().unwrap();
        let (state, profile) = native_running_state(d.path()).await;
        if let Ok(out) = std::env::var("NOEVIA_DUMP_NATIVE_CALIBRATION_STATE") {
            std::fs::write(
                &out,
                json!({"state": state, "profile": profile}).to_string(),
            )
            .unwrap();
        }
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/calibration-recover-node.json"
        ))
        .unwrap();
        // The fixture's native input has this run's shape: the same keys in the same order.
        let fresh = wtf16::parse(&state).unwrap();
        let recorded = wtf16::parse(fixture["native"]["state"].as_str().unwrap()).unwrap();
        assert_eq!(keys(&fresh), keys(&recorded));
        assert_eq!(keys(fresh.get("job")), keys(recorded.get("job")));
        assert_eq!(fresh.get("job").get("status").as_str(), Some("running"));
        assert!(fresh.get("job").get("originalText").as_str().is_some());
        assert!(fresh.get("job").get("lastRevision").as_str().is_some());
        let cases = fixture["cases"].as_array().unwrap();
        assert!(cases.len() >= 3);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("models.ini"), case["ini"].as_str().unwrap()).unwrap();
            let w = worker(dir.path(), 9);
            let file = dir
                .path()
                .join(format!("native-calibration-{}.json", w.maint.suffix));
            std::fs::write(&file, case["state"].as_str().unwrap()).unwrap();
            w.recover().await;
            let mut got = wtf16::parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
            let mut want = wtf16::parse(case["after"]["state"].as_str().unwrap()).unwrap();
            for v in [&mut got, &mut want] {
                if let JValue::Obj(f) = v {
                    if let Some((_, JValue::Obj(job))) = f.iter_mut().find(|(k, _)| k == "job") {
                        job.retain(|(k, _)| k != "finishedAt");
                    }
                }
            }
            assert_eq!(got, want, "{name}");
            assert_eq!(
                std::fs::read_to_string(dir.path().join("models.ini")).unwrap(),
                case["after"]["ini"].as_str().unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn hold_refusals_have_the_shape_node_launch_reported() {
        let busy = hold_facts(&HoldRefusal::Busy);
        assert_eq!(
            wtf16::stringify(&busy).unwrap(),
            r#"{"status":409,"publicMessage":null,"code":null}"#
        );
        let gate = hold_facts(&HoldRefusal::Gate {
            status: 503,
            code: "engine_unavailable".into(),
            message: "The local model engine is not answering. Try again shortly.".into(),
        });
        assert_eq!(
            wtf16::stringify(&gate).unwrap(),
            r#"{"status":503,"publicMessage":"The local model engine is not answering. Try again shortly.","code":"engine_unavailable"}"#
        );
        let odd = hold_facts(&HoldRefusal::Gate {
            status: 500,
            code: "bad code!".into(),
            message: "x".into(),
        });
        assert_eq!(odd.get("code"), &JValue::Null);
        assert_eq!(
            hold_facts(&HoldRefusal::NodeUnavailable).get("status"),
            &JValue::Null
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn launch_and_abort_apply_only_to_the_snapshot_their_token_names() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, _) = synthetic_engine(ini.clone(), true).await;
        let w = worker(d.path(), port);
        let plan = json!({"promptBudgetSeconds": 120, "native": 16384, "base": {}});
        let plan = wtf16::parse(&plan.to_string()).unwrap();
        let bad = wtf16::parse(r#"{"promptBudgetSeconds":14,"native":16384,"base":{}}"#).unwrap();
        let token = w.state_facts().get("token").as_str().unwrap().to_owned();
        assert!(w
            .compare_and_launch(
                "synthetic",
                &token,
                &"0".repeat(64),
                &"0".repeat(64),
                &bad,
                16.0
            )
            .await
            .is_err());
        let stale = w
            .compare_and_launch("synthetic", &"1".repeat(64), "", "", &plan, 16.0)
            .await
            .unwrap();
        assert_eq!(stale.get("reason").as_str(), Some("stale"));
        let idle = w.cancellation_facts();
        assert_eq!(
            w.compare_and_abort(idle.get("token").as_str().unwrap())
                .get("applied"),
            &JValue::Bool(false)
        );
        w.launch(
            "synthetic",
            Plan {
                prompt_budget_seconds: 120.0,
                native: 16384.0,
                base: Vec::new(),
            },
        )
        .await
        .unwrap();
        let first = w.cancellation_facts();
        assert_eq!(
            w.compare_and_abort(&"2".repeat(64)).get("applied"),
            &JValue::Bool(false)
        );
        // A launch token taken before the job is stale now.
        let late = w
            .compare_and_launch("synthetic", &token, "", "", &plan, 16.0)
            .await
            .unwrap();
        assert_eq!(late.get("reason").as_str(), Some("stale"));
        let mut facts = w.cancellation_facts();
        for _ in 0..200 {
            let applied = w.compare_and_abort(facts.get("token").as_str().unwrap());
            if applied.get("applied") == &JValue::Bool(true) {
                assert_eq!(applied.get("generation"), first.get("generation"));
                assert_eq!(applied.get("job").get("status").as_str(), Some("running"));
                break;
            }
            facts = w.cancellation_facts();
        }
        assert_eq!(settled(&w).await.get("status").as_str(), Some("cancelled"));
        assert_eq!(std::fs::read_to_string(&ini).unwrap(), INI);
    }

    /// Node's lease route (model-maintenance-lease.cjs): grants `hard` (None: the cap asked for, as
    /// the new Core does), renews until `ended` is set, then answers 404. Logs requests and bodies.
    async fn lease_stub(
        hard: Option<u64>,
        ended: Arc<AtomicBool>,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        use axum::body::Body;
        use axum::http::{Request, Response};
        use http_body_util::BodyExt;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let held = Arc::new(AtomicBool::new(false));
        let app = axum::Router::new().fallback(move |req: Request<Body>| {
            let (log, ended, held) = (Arc::clone(&log), Arc::clone(&ended), Arc::clone(&held));
            async move {
                let (parts, body) = req.into_parts();
                let body = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                let text = String::from_utf8_lossy(&body).into_owned();
                log.lock().unwrap().push(format!("{} {} {text}", parts.method, parts.uri.path()));
                let asked = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v["maxMs"].as_u64());
                let (code, out) = match (parts.method.as_str(), parts.uri.path()) {
                    ("POST", "/internal/model-maintenance/lease") if held.swap(true, Ordering::AcqRel) => (
                        409, json!({"error": "Busy"}),
                    ),
                    ("POST", "/internal/model-maintenance/lease") => (
                        201,
                        json!({"lease": "fedcba9876543210fedcba9876543210", "ttlMs": 180000, "renewMs": 20,
                               "hardMs": hard.or(asked).unwrap_or(3_600_000)}),
                    ),
                    ("POST", p) if p.ends_with("/renew") => {
                        if ended.load(Ordering::Acquire) {
                            held.store(false, Ordering::Release);
                            (404, json!({"error": "Not found"}))
                        } else {
                            (200, json!({}))
                        }
                    }
                    ("DELETE", _) => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        held.store(false, Ordering::Release);
                        (204, json!({}))
                    },
                    ("POST", "/internal/model-maintenance/load-refusal") => (200, json!({"refusal": null})),
                    _ => (404, json!({"error": "Not found"})),
                };
                Response::builder()
                    .status(code)
                    .header("content-type", "application/json")
                    .body(Body::from(if code == 204 { String::new() } else { out.to_string() }))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (port, seen)
    }

    fn plan(native: f64, budget: f64) -> Plan {
        Plan {
            prompt_budget_seconds: budget,
            native,
            base: Vec::new(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn without_a_gate_hold_the_lease_asks_for_the_run_worst_case_and_refuses_over_24_hours() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (port, _) = synthetic_engine_with(
            ini.clone(),
            Synth {
                live: Some(Arc::clone(&live)),
                ..Synth::default()
            },
        )
        .await;
        let (lease, seen) = lease_stub(None, Arc::new(AtomicBool::new(false))).await;
        let w = worker_with(d.path(), port, Some(lease));
        // Node's limits; at most 1800 s a test, a 128K model fits the 24-hour lease.
        let w = with_limits(&w, Limits::default());
        let huge = plan(131_072.0, 1800.0);
        assert!(w.worst_case(&huge, 1) < super::super::node_lease::MAX_LEASE);
        // With 1000 other models live, the first unload alone could take over 16 hours: refused
        // before any lease is asked for.
        live.store(1000, Ordering::Release);
        assert!(w.worst_case(&huge, 1000) > super::super::node_lease::MAX_LEASE);
        assert_eq!(w.launch("synthetic", huge).await, Err(Refusal::Limit));
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(w.job(), JValue::Null);
        live.store(0, Ordering::Release);
        let w = worker_with(d.path(), port, Some(lease));
        // Inside it: one lease asking for exactly the worst case, and the run passes.
        let p = plan(32_768.0, 120.0);
        let worst = u64::try_from(w.worst_case(&p, 1).as_millis()).unwrap();
        assert!(worst > 3_600_000 && worst < 24 * 3_600_000, "{worst}");
        w.launch("synthetic", p).await.unwrap();
        let job = settled(&w).await;
        assert_eq!(job.get("status").as_str(), Some("passed"), "{job:?}");
        released(&seen, "DELETE /internal/model-maintenance/lease/").await;
        let log = seen.lock().unwrap().clone();
        assert_eq!(
            log[0],
            format!("POST /internal/model-maintenance/lease {{\"maxMs\":{worst}}}")
        );
        assert!(log
            .iter()
            .any(|l| l.starts_with("DELETE /internal/model-maintenance/lease/")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_finished_job_restarts_without_busy_while_core_lease_release_is_delayed() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, _) = synthetic_engine(ini, false).await;
        let (lease, _) = lease_stub(None, Arc::new(AtomicBool::new(false))).await;
        let w = worker_with(d.path(), port, Some(lease));
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        let job = settled(&w).await;
        let saved = w.file.load();
        assert_eq!(saved.job().get("finishedAt"), job.get("finishedAt"));
        assert!(matches!(saved.job().get("originalText"), JValue::Undefined));
        // No event wait or private local-held observation: public completion must suffice.
        let next = w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        assert_ne!(next.get("id"), job.get("id"));
        let _ = w.cancel();
        settled(&w).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lease_granted_shorter_than_asked_is_given_back_and_nothing_runs() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, _) = synthetic_engine(ini.clone(), false).await;
        // An older Core ignores maxMs and grants its 60-minute default.
        let (lease, seen) = lease_stub(Some(3_600_000), Arc::new(AtomicBool::new(false))).await;
        let w = worker_with(d.path(), port, Some(lease));
        let p = plan(32_768.0, 1800.0);
        assert!(w.worst_case(&p, 1) > Duration::from_secs(3600));
        let Err(Refusal::Hold(facts)) = w.launch("synthetic", p).await else {
            panic!("refused")
        };
        assert_eq!(facts.get("status").as_num(), Some(503.0));
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("DELETE ")));
        assert_eq!(w.job(), JValue::Null);
        assert_eq!(std::fs::read_to_string(&ini).unwrap(), INI);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lease_lost_mid_run_stops_it_restores_the_profile_and_marks_it_interrupted() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, _) = synthetic_engine(ini.clone(), true).await;
        let ended = Arc::new(AtomicBool::new(false));
        let (lease, _) = lease_stub(None, Arc::clone(&ended)).await;
        let w = worker_with(d.path(), port, Some(lease));
        w.launch("synthetic", plan(16_384.0, 120.0)).await.unwrap();
        for _ in 0..600 {
            if w.job()
                .get("phase")
                .as_str()
                .is_some_and(|p| p.starts_with("Long-prompt"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        ended.store(true, Ordering::Release);
        let job = settled(&w).await;
        assert_eq!(job.get("status").as_str(), Some("interrupted"), "{job:?}");
        assert_eq!(job.get("phase").as_str(), Some("Interrupted"));
        assert_eq!(job.get("error").as_str(), Some(LEASE_LOST));
        assert_eq!(job.get("restored"), &JValue::Bool(true));
        assert_eq!(std::fs::read_to_string(&ini).unwrap(), INI);
        // The next run starts clean.
        assert!(!w.lease_lost());
    }

    fn with_limits(w: &Worker, limits: Limits) -> Arc<Worker> {
        Worker::new(Arc::clone(&w.maint), Arc::new(|| Some(64.0)), 2.0, limits)
    }

    /// P3-1: the lease's cap counts every limit a step and the run use, as often as they use it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_worst_case_counts_each_limit_as_often_as_the_run_uses_it() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("models.ini"), INI).unwrap();
        let w = worker(d.path(), 9);
        let base = Limits::default();
        let w = with_limits(&w, base);
        let p = plan(131_072.0, 120.0);
        let grid = ladder(p.native);
        let start = super::super::calibration::start_of(&grid).unwrap();
        let (loads, longs) = Worker::step_counts(&grid, start, *grid.last().unwrap());
        let steps = loads + longs;
        let at = w.worst_case(&p, 4);
        let second = Duration::from_secs(1);
        type Bump = fn(&mut Limits);
        let cases: [(&str, Bump, u32); 10] = [
            // Every step polls its load, and the calibrated profile is loaded once more.
            ("load", |l| l.load += Duration::from_secs(1), steps + 1),
            ("smoke", |l| l.smoke += Duration::from_secs(1), loads),
            // Per step: the unload-all request and poll, and the end-of-step request and poll; the
            // first unload asks each of the 4 live models and polls; the last asks one and polls.
            (
                "unload",
                |l| l.unload += Duration::from_secs(1),
                4 * steps + (4 + 1) + 2,
            ),
            // Each poll loop may overrun by one sleep: three per step, three in the run.
            ("poll", |l| l.poll += Duration::from_secs(1), 3 * steps + 3),
            ("recover", |l| l.recover += Duration::from_secs(1), steps),
            (
                "backend_poll",
                |l| l.backend_poll += Duration::from_secs(1),
                steps,
            ),
            (
                "memory_settle",
                |l| l.memory_settle += Duration::from_secs(1),
                30 * steps,
            ),
            // The stream's own deadline (1.5x the budget) ends it before `long`; the memory sampler
            // and the exclusion watch add no time.
            ("long", |l| l.long += Duration::from_secs(1), 0),
            (
                "memory_poll",
                |l| l.memory_poll += Duration::from_secs(1),
                0,
            ),
            (
                "exclusion_poll",
                |l| l.exclusion_poll += Duration::from_secs(1),
                0,
            ),
        ];
        for (name, bump, uses) in cases {
            let mut limits = base;
            bump(&mut limits);
            let bumped = with_limits(&w, limits).worst_case(&p, 4);
            assert_eq!(bumped - at, second * uses, "{name}");
        }
        // Each long test stops its prompt at 1.5 times the budget.
        let longer = w.worst_case(&plan(131_072.0, 122.0), 4);
        assert_eq!(longer - at, Duration::from_secs(3) * longs);
        // Each live model counted at launch is one more unload request in the first unload.
        let more = w.worst_case(&p, 6);
        assert_eq!(more - at, base.unload * 2);
        assert_eq!(w.worst_case(&p, 0), w.worst_case(&p, 1));
        // The fixed request timeouts and margin, with every step limit at zero.
        // Finalization keeps its separate bounded cleanup budget.
        let zero = Limits {
            load: Duration::ZERO,
            smoke: Duration::ZERO,
            long: Duration::ZERO,
            unload: Duration::ZERO,
            poll: Duration::ZERO,
            memory_poll: Duration::ZERO,
            recover: Duration::ZERO,
            backend_poll: Duration::ZERO,
            memory_settle: Duration::ZERO,
            exclusion_poll: Duration::ZERO,
            save_retry: Duration::ZERO,
            finalize: base.finalize,
        };
        let listing = super::super::router::DEFAULT_TIMEOUT;
        let step_fixed = listing * 3 + RELOAD + LOAD_CHECK + LOAD_REQUEST + listing * 2;
        let expected = step_fixed * steps
            + TOKENIZE * longs
            + Duration::from_secs(180) * longs
            + listing * 2
            + listing * 2
            + RELOAD
            + LOAD_CHECK
            + LOAD_REQUEST
            + listing
            + PROPS
            + RELOAD
            + RUN_MARGIN;
        assert_eq!(with_limits(&w, zero).worst_case(&p, 4), expected);
        // Node's defaults: a 128K model at a 2-minute budget fits the 24-hour lease.
        assert!(at < super::super::node_lease::MAX_LEASE, "{at:?}");
    }

    /// P3-1: a long prompt the engine accepts and then never answers ends at its 1.5x deadline as
    /// a failed long test (Node's over-budget stop), not at `limits.long`; the run then fails as
    /// one in which no size passed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_long_prompt_ends_at_its_deadline_as_a_failed_long_test() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, _) = synthetic_engine_with(
            ini.clone(),
            Synth {
                silent: true,
                ..Synth::default()
            },
        )
        .await;
        let w = worker(d.path(), port);
        let began = std::time::Instant::now();
        w.launch("synthetic", plan(16_384.0, 1.0)).await.unwrap();
        let job = settled(&w).await;
        assert!(
            began.elapsed() < Duration::from_secs(25),
            "{:?}",
            began.elapsed()
        );
        assert_eq!(job.get("status").as_str(), Some("failed"), "{job:?}");
        assert!(job
            .get("error")
            .as_str()
            .is_some_and(|e| e.starts_with("No size passed the long-prompt test within 1 s")));
        let JValue::Arr(steps) = job.get("steps") else {
            panic!("{job:?}")
        };
        let longs: Vec<&JValue> = steps
            .iter()
            .filter(|s| s.get("kind").as_str() == Some("long"))
            .collect();
        assert!(!longs.is_empty(), "{job:?}");
        for step in longs {
            assert_eq!(step.get("status").as_str(), Some("failed"), "{step:?}");
            assert_eq!(step.get("cause").as_str(), Some("time"), "{step:?}");
            let reason = step.get("reason").as_str().unwrap_or("");
            assert!(
                reason.starts_with("Filling this context took ")
                    && reason.ends_with(" s, over the 1 s limit."),
                "{reason}"
            );
            assert!(matches!(step.get("etaSeconds"), JValue::Undefined));
        }
        // The profile went back, and nothing still runs.
        assert_eq!(std::fs::read_to_string(&ini).unwrap(), INI);
    }

    /// P3-1: the lease counts one unload request for every model live at launch, more than four
    /// included, and the run's first unload unloads them all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_lease_counts_every_model_live_at_launch() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let live = Arc::new(std::sync::atomic::AtomicUsize::new(6));
        let (port, _) = synthetic_engine_with(
            ini.clone(),
            Synth {
                live: Some(Arc::clone(&live)),
                ..Synth::default()
            },
        )
        .await;
        let (lease, seen) = lease_stub(None, Arc::new(AtomicBool::new(false))).await;
        let w = worker_with(d.path(), port, Some(lease));
        let p = plan(16_384.0, 120.0);
        let six = u64::try_from(w.worst_case(&p, 6).as_millis()).unwrap();
        assert!(w.worst_case(&p, 6) > w.worst_case(&p, 4));
        w.launch("synthetic", p).await.unwrap();
        let job = settled(&w).await;
        assert_eq!(job.get("status").as_str(), Some("passed"), "{job:?}");
        assert_eq!(
            seen.lock().unwrap()[0],
            format!("POST /internal/model-maintenance/lease {{\"maxMs\":{six}}}")
        );
        assert_eq!(live.load(Ordering::Acquire), 0);
    }

    fn engine_loads(seen: &Mutex<Vec<String>>) -> usize {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|l| *l == "POST /models/load")
            .count()
    }

    fn load_asks(seen: &Mutex<Vec<String>>) -> usize {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|l| *l == "POST /internal/model-maintenance/load-refusal")
            .count()
    }

    /// Every step of `job` failed as a refused load does in Node (`stepOnce`).
    fn every_step_refused(job: &JValue) {
        let JValue::Arr(steps) = job.get("steps") else {
            panic!("{job:?}")
        };
        assert!(!steps.is_empty(), "{job:?}");
        for step in steps {
            assert_eq!(step.get("status").as_str(), Some("failed"), "{step:?}");
            assert_eq!(
                step.get("reason").as_str(),
                Some("The engine refused to load the model at this size."),
                "{step:?}"
            );
            assert_eq!(step.get("cause").as_str(), Some("load"), "{step:?}");
        }
    }

    /// P2: what Node's load decision refuses (over the budget, quarantined by the watchdog, a size
    /// knob that is not a whole number) never reaches the engine: each step fails as Node's refused
    /// load does, the run fails, and the original profile goes back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loads_nodes_decision_refuses_never_reach_the_engine() {
        for code in [
            "inference_budget",
            "inference_budget_unloaded",
            "invalid_size",
        ] {
            let d = tempfile::tempdir().unwrap();
            let ini = d.path().join("models.ini");
            std::fs::write(&ini, INI).unwrap();
            let bodies = Arc::new(Mutex::new(Vec::new()));
            let asked = Arc::clone(&bodies);
            let refusal = node_refusal(code);
            let (port, seen) = synthetic_engine_with(
                ini.clone(),
                Synth {
                    node: Some(Arc::new(move |_, body| {
                        asked.lock().unwrap().push(body.clone());
                        (200, json!({ "refusal": refusal.clone() }))
                    })),
                    ..Synth::default()
                },
            )
            .await;
            let w = worker(d.path(), port);
            w.launch("synthetic", plan(32_768.0, 120.0)).await.unwrap();
            let job = settled(&w).await;
            assert_eq!(
                job.get("status").as_str(),
                Some("failed"),
                "{code}: {job:?}"
            );
            every_step_refused(&job);
            assert_eq!(engine_loads(&seen), 0, "{code}");
            assert!(load_asks(&seen) >= 1, "{code}");
            assert!(bodies
                .lock()
                .unwrap()
                .iter()
                .all(|b| *b == json!({"model": "synthetic"})));
            assert_eq!(std::fs::read_to_string(&ini).unwrap(), INI, "{code}");
        }
    }

    /// P2: the final load of the calibrated profile asks too; refused, the run fails as Node's does
    /// when that load fails ("The calibrated profile did not load."), and the engine never gets it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_final_load_fails_the_run_without_reaching_the_engine() {
        // The control run: every load is asked for, each just before it is sent.
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, seen) = synthetic_engine(ini.clone(), false).await;
        let w = worker(d.path(), port);
        w.launch("synthetic", plan(32_768.0, 120.0)).await.unwrap();
        assert_eq!(settled(&w).await.get("status").as_str(), Some("passed"));
        let loads = engine_loads(&seen);
        assert!(loads > 1);
        assert_eq!(load_asks(&seen), loads);
        // The same run with its last load (the calibrated profile's) refused.
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, seen) = synthetic_engine_with(
            ini.clone(),
            Synth {
                node: Some(Arc::new(move |n, _| {
                    let refusal = if n == loads {
                        node_refusal("inference_budget")
                    } else {
                        Value::Null
                    };
                    (200, json!({ "refusal": refusal }))
                })),
                ..Synth::default()
            },
        )
        .await;
        let w = worker(d.path(), port);
        w.launch("synthetic", plan(32_768.0, 120.0)).await.unwrap();
        let job = settled(&w).await;
        assert_eq!(job.get("status").as_str(), Some("failed"), "{job:?}");
        assert_eq!(
            job.get("error").as_str(),
            Some("The calibrated profile did not load.")
        );
        assert_eq!(job.get("phase").as_str(), Some("Failed"));
        assert_eq!(engine_loads(&seen), loads - 1);
        assert_eq!(load_asks(&seen), loads);
        assert!(matches!(w.history("synthetic"), JValue::Arr(ref h) if h.is_empty()));
        assert_eq!(std::fs::read_to_string(&ini).unwrap(), INI);
    }

    /// P2: with no decision from Node (an error answer, or Node not listening), every load is
    /// refused, never sent unchecked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn without_nodes_decision_nothing_loads() {
        let gone = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        for outage in ["503", "a string", "no refusal key", "unreachable"] {
            let d = tempfile::tempdir().unwrap();
            let ini = d.path().join("models.ini");
            std::fs::write(&ini, INI).unwrap();
            let (port, seen) = synthetic_engine_with(
                ini.clone(),
                Synth {
                    node: Some(Arc::new(move |_, _| match outage {
                        "503" => (503, json!({"error": "synthetic"})),
                        "a string" => (200, json!("refusal")),
                        _ => (200, json!({})),
                    })),
                    ..Synth::default()
                },
            )
            .await;
            let node = if outage == "unreachable" { gone } else { port };
            let w = worker_node(d.path(), port, None, node);
            w.launch("synthetic", plan(32_768.0, 120.0)).await.unwrap();
            let job = settled(&w).await;
            assert_eq!(
                job.get("status").as_str(),
                Some("failed"),
                "{outage}: {job:?}"
            );
            every_step_refused(&job);
            assert_eq!(engine_loads(&seen), 0, "{outage}");
            assert_eq!(std::fs::read_to_string(&ini).unwrap(), INI, "{outage}");
        }
    }

    async fn evidence_count(w: &Worker) -> usize {
        let store = Arc::clone(w.maint.evidence.as_ref().unwrap());
        tokio::task::spawn_blocking(move || store.list().len())
            .await
            .unwrap()
    }

    /// P3-2 control: a run that keeps its lease records its history entry and its evidence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_run_that_keeps_its_lease_records_history_and_evidence() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        std::fs::write(d.path().join("synthetic.gguf"), b"GGUF synthetic").unwrap();
        let (port, _) = synthetic_engine(ini.clone(), false).await;
        let (lease, _) = lease_stub(None, Arc::new(AtomicBool::new(false))).await;
        let w = worker_with(d.path(), port, Some(lease));
        w.launch("synthetic", plan(16_384.0, 120.0)).await.unwrap();
        let job = settled(&w).await;
        assert_eq!(job.get("status").as_str(), Some("passed"), "{job:?}");
        let JValue::Arr(history) = w.history("synthetic") else {
            panic!("history")
        };
        assert_eq!(history.len(), 1);
        assert_eq!(evidence_count(&w).await, 1);
    }

    /// P3-2: a lease that ends after the run passed (during its `/props` read) rolls the run back:
    /// the profile is restored, and neither a history entry nor evidence is left behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lease_lost_after_the_pass_leaves_no_history_and_no_evidence() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        std::fs::write(d.path().join("synthetic.gguf"), b"GGUF synthetic").unwrap();
        let ended = Arc::new(AtomicBool::new(false));
        let (port, _) = synthetic_engine_with(
            ini.clone(),
            Synth {
                props: Some(Arc::clone(&ended)),
                ..Synth::default()
            },
        )
        .await;
        let (lease, _) = lease_stub(None, Arc::clone(&ended)).await;
        let w = worker_with(d.path(), port, Some(lease));
        w.launch("synthetic", plan(16_384.0, 120.0)).await.unwrap();
        let job = settled(&w).await;
        assert!(
            ended.load(Ordering::Acquire),
            "the run reached its /props read"
        );
        assert_eq!(job.get("status").as_str(), Some("interrupted"), "{job:?}");
        assert_eq!(job.get("error").as_str(), Some(LEASE_LOST));
        assert_eq!(job.get("restored"), &JValue::Bool(true));
        assert!(
            !matches!(w.history("synthetic"), JValue::Arr(h) if !h.is_empty()),
            "{:?}",
            w.history("synthetic")
        );
        assert_eq!(evidence_count(&w).await, 0);
        assert_eq!(std::fs::read_to_string(&ini).unwrap(), INI);
    }

    /// P3-4: with nothing to recover, the file is not read back over what memory holds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovery_with_nothing_to_recover_keeps_the_state_in_memory() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("models.ini"), INI).unwrap();
        let (port, _) = synthetic_engine(d.path().join("models.ini"), false).await;
        let w = worker(d.path(), port);
        assert_eq!(w.job(), JValue::Null);
        // Written after the worker read the file (as a launch's first save would be).
        std::fs::write(
            w.maint.calibration_state().path.clone().unwrap(),
            r#"{"job":{"id":"other","model":"synthetic","status":"passed","steps":[]},"history":{}}"#,
        )
        .unwrap();
        assert_eq!(w.recover().await, None);
        assert_eq!(w.job(), JValue::Null);
        assert!(!w.starting() && !w.recovering());
    }

    /// P3-4: while the boot recovery runs, no launch starts and no abort applies.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nothing_launches_or_cancels_while_recovery_runs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("models.ini"), INI).unwrap();
        let (port, seen) = synthetic_engine(d.path().join("models.ini"), false).await;
        let (state, _) = native_running_state(d.path()).await;
        let w = worker(d.path(), port);
        // As the boot finds it: the file's running job, with no run behind it.
        std::fs::write(w.maint.calibration_state().path.clone().unwrap(), &state).unwrap();
        let w = with_limits(&w, w.limits);
        assert!(w.begin_recovery());
        assert!(w.recovering() && w.starting());
        assert_eq!(w.state_facts().get("starting"), &JValue::Bool(true));
        assert_eq!(
            w.launch("synthetic", plan(16_384.0, 120.0)).await,
            Err(Refusal::Running)
        );
        let token = w
            .cancellation_facts()
            .get("token")
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            w.compare_and_abort(&token).get("applied"),
            &JValue::Bool(false)
        );
        assert!(w.cancel().is_none());
        assert!(!seen
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("POST /gate/")));
        assert!(w.recover_held().await.is_some());
        assert!(!w.recovering() && !w.starting());
        // Recovery restores and reloads the preset as Node's recover() does; it loads nothing.
        assert_eq!(engine_loads(&seen), 0);
        assert_eq!(load_asks(&seen), 0);
        assert_eq!(w.job().get("status").as_str(), Some("interrupted"));
    }
}
