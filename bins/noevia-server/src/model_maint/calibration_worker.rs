//! The native calibration job (core `llamacpp-calibration.cjs` `stepOnce`, `run`,
//! `compareAndLaunch`, `compareAndAbort`): the engine-facing step, the run under a maintenance
//! hold, its cancellation controller and its evidence record. UNWIRED: the public routes still
//! reach Node's worker through the private bridges until the routes switch as one unit.
//!
//! The job is the same object Node keeps in `native-calibration-<hash>.json` (see
//! [`super::calibration`]), with the same keys in the same order, so either owner reads the other's
//! jobs and history. Original profile text is private: it remains while a run or an unsettled
//! restoration needs restart recovery and is never included in public job views.

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
    /// Normal finalization deadline. A failed run's durable tombstone is mandatory: persistent
    /// control-storage errors retain local admission beyond this limit until recovery.
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
    /// Sealed under the worker mutex before durable terminal intent or marker-free saves.
    pub(super) decided: bool,
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

#[cfg(test)]
type TerminalStall = (
    bool,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

#[cfg(test)]
#[derive(Default)]
struct SaveCounts {
    outstanding: std::sync::atomic::AtomicUsize,
    snapshots: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    dispatched: std::sync::atomic::AtomicUsize,
}
#[cfg(test)]
struct SaveCount(std::sync::Arc<SaveCounts>);
#[cfg(test)]
impl SaveCount {
    fn new(counts: std::sync::Arc<SaveCounts>) -> Self {
        let n = counts.outstanding.fetch_add(1, Ordering::AcqRel) + 1;
        counts.snapshots.fetch_add(1, Ordering::AcqRel);
        counts.peak.fetch_max(n, Ordering::AcqRel);
        counts.dispatched.fetch_add(1, Ordering::AcqRel);
        Self(counts)
    }
}
#[cfg(test)]
impl Drop for SaveCount {
    fn drop(&mut self) {
        self.0.outstanding.fetch_sub(1, Ordering::AcqRel);
        self.0.snapshots.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Owned by the Worker even when an observing async future is cancelled. Exactly one
/// blocking write can exist; a retry drains this handle before dispatching any replacement.
struct PendingWrite {
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    permit: Arc<AtomicBool>,
}

/// The native worker: one per process, owning its state file, controller and job.
pub struct Worker {
    pub(super) maint: Arc<Maint>,
    file: StateFile,
    inner: Arc<Mutex<Inner>>,
    writes: Arc<Mutex<()>>,
    pending_write: tokio::sync::Mutex<Option<PendingWrite>>,
    // An owned restoration can outlive the cleanup deadline. Do not admit another run until
    // its write has a definitive result; cancellation cannot revoke a remote CAS already sent.
    restoring: Arc<AtomicBool>,
    local_fence: Mutex<Option<super::gate::AdmissionFence>>,
    fence_wake: Mutex<Arc<Notify>>,
    fence_interrupted: AtomicBool,
    #[cfg(test)]
    save_counts: Arc<SaveCounts>,
    #[cfg(test)]
    terminal_stall: Mutex<Option<TerminalStall>>,
    #[cfg(test)]
    terminal_error: AtomicBool,
    #[cfg(test)]
    run_abort: Mutex<Option<tokio::task::AbortHandle>>,
    #[cfg(test)]
    panic_on_run: AtomicBool,
    #[cfg(test)]
    panic_on_restore: Arc<AtomicBool>,
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

fn state_status_passed(job: &[(String, JValue)]) -> bool {
    job.iter()
        .any(|(k, v)| k == "status" && v.as_str() == Some("passed"))
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
    if i.active || i.recovering || i.state.job().get("pendingTombstone").truthy() {
        if let JValue::Obj(fields) = &mut job {
            if i.recovering
                || i.state.job().get("status").as_str() != Some("running")
                || i.state.job().get("pendingTombstone").truthy()
            {
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

/// Reconciles unwind without leaking panic payloads. The Worker retains its admission fence
/// until the sealed outcome is durable. Also captured by never-polled tasks.
struct RunGuard {
    worker: Arc<Worker>,
    object: u64,
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        let live = self
            .worker
            .with(|i| i.active && i.job_object == self.object);
        if !live {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let worker = Arc::clone(&self.worker);
            let object = self.object;
            runtime.spawn(async move {
                // A sealed decision is retried, never changed to failure. Shutdown retains
                // the local fence and durable recovery intent for the next process.
                let state = worker.with(|i| {
                    if !i.active || i.job_object != object {
                        return None;
                    }
                    if !i.decided {
                        finish_snapshot(&mut i.state, true);
                        if let Some(job) = i.state.job_mut() {
                            wtf16::set(job, "pendingTombstone", JValue::Bool(true));
                            wtf16::set(job, "terminalDecision", JValue::Str("fenced".into()));
                            if !worker.restoring.load(Ordering::Acquire) {
                                job.retain(|(k, _)| k != "pendingRestore");
                            }
                        }
                        i.decided = true;
                    }
                    Some(i.state.clone())
                });
                if let Some(state) = state {
                    worker.complete_decision(state, object).await;
                }
            });
        }
    }
}

/// Cancellation of a recovery future gives its launch slot back for an explicit retry on
/// this Worker. The local fence deliberately survives unwind, including a never-polled task.
struct RecoveryGuard {
    worker: Arc<Worker>,
    object: u64,
}
impl Drop for RecoveryGuard {
    fn drop(&mut self) {
        self.worker.with(|i| {
            if i.recovering && i.job_object == self.object {
                i.launching = false;
                i.recovering = false;
            }
        });
    }
}

/// Dropping a panicked/cancelled restoration schedules the same idempotent settlement again.
/// The reservation remains held until that resolver reaches a definitive success or conflict.
struct RestoreResolver {
    ini: super::ini::Store,
    candidate: super::ini::Candidate,
    pending: Arc<AtomicBool>,
    retry: Duration,
    #[cfg(test)]
    panic_once: Arc<AtomicBool>,
}
struct RestoreGuard {
    resolver: Arc<RestoreResolver>,
    armed: bool,
}
impl Drop for RestoreGuard {
    fn drop(&mut self) {
        if self.armed && tokio::runtime::Handle::try_current().is_ok() {
            // Schedule first; never silently clear a reservation on unwind.
            self.resolver.spawn();
        }
    }
}
impl RestoreResolver {
    fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<bool> {
        let resolver = Arc::clone(self);
        tokio::spawn(async move {
            let mut guard = RestoreGuard {
                resolver: Arc::clone(&resolver),
                armed: true,
            };
            #[cfg(test)]
            #[allow(clippy::panic)]
            if resolver.panic_once.swap(false, Ordering::AcqRel) {
                panic!("synthetic restore panic");
            }
            let restored = resolver
                .ini
                .settle_restore(&resolver.candidate, resolver.retry)
                .await;
            resolver.pending.store(false, Ordering::Release);
            guard.armed = false;
            restored
        })
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
            pending_write: tokio::sync::Mutex::new(None),
            restoring: Arc::new(AtomicBool::new(false)),
            local_fence: Mutex::new(None),
            fence_wake: Mutex::new(Arc::new(Notify::new())),
            fence_interrupted: AtomicBool::new(false),
            #[cfg(test)]
            save_counts: Arc::new(SaveCounts::default()),
            #[cfg(test)]
            terminal_stall: Mutex::new(None),
            #[cfg(test)]
            terminal_error: AtomicBool::new(false),
            #[cfg(test)]
            run_abort: Mutex::new(None),
            #[cfg(test)]
            panic_on_run: AtomicBool::new(false),
            #[cfg(test)]
            panic_on_restore: Arc::new(AtomicBool::new(false)),
            inner: Arc::new(Mutex::new(Inner {
                state,
                cancel_requested: false,
                inflight: None,
                launching: false,
                active: false,
                decided: false,
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

    /// Serial writes retain their blocking owner across cancellation/timeout. The async
    /// slot bounds dispatched work; the blocking mutex covers checks, rename and fsync.
    async fn drain_writes(&self) {
        let mut slot = self.pending_write.lock().await;
        if let Some(pending) = slot.as_mut() {
            pending.permit.store(false, Ordering::Release);
            let _ = (&mut pending.task).await;
            *slot = None;
        }
    }

    async fn write_state(
        &self,
        mut state: State,
        object: u64,
        until: Option<Instant>,
    ) -> std::io::Result<()> {
        let timed_out = || std::io::Error::new(std::io::ErrorKind::TimedOut, "state write ended");
        let mut slot = match until {
            Some(end) => tokio::time::timeout_at(end.into(), self.pending_write.lock())
                .await
                .map_err(|_| timed_out())?,
            None => self.pending_write.lock().await,
        };
        if let Some(pending) = slot.as_mut() {
            pending.permit.store(false, Ordering::Release);
            match until {
                Some(end) => {
                    let _ = tokio::time::timeout_at(end.into(), &mut pending.task)
                        .await
                        .map_err(|_| timed_out())?;
                }
                None => {
                    let _ = (&mut pending.task).await;
                }
            }
            *slot = None;
        }
        if until.is_some_and(|end| Instant::now() >= end) {
            return Err(timed_out());
        }
        let evidence = self.maint.evidence.clone();
        if self.file.path.is_none() {
            return Err(std::io::Error::other("calibration state unavailable"));
        }
        let file = self.file.clone();
        let inner = Arc::clone(&self.inner);
        let writes = Arc::clone(&self.writes);
        let permit = Arc::new(AtomicBool::new(true));
        let _permit_guard = WritePermit(Arc::clone(&permit));
        let allowed = Arc::clone(&permit);
        #[cfg(test)]
        let terminal = !state.job().get("finishedAt").is_nullish();
        #[cfg(test)]
        let marker_free = !state.job().get("pendingTombstone").truthy();
        #[cfg(test)]
        let stall = {
            let mut hook = self
                .terminal_stall
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if terminal
                && hook
                    .as_ref()
                    .is_some_and(|(free, _, _)| *free == marker_free)
            {
                hook.take()
            } else {
                None
            }
        };
        #[cfg(test)]
        if terminal && marker_free && self.terminal_error.load(Ordering::Acquire) {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        #[cfg(test)]
        let counted = SaveCount::new(Arc::clone(&self.save_counts));
        let task = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let _counted = counted;
            #[cfg(test)]
            if let Some((_, entered, release)) = stall {
                let _ = entered.send(());
                let _ = release.recv_timeout(Duration::from_secs(10));
            }
            let _writer = writes.lock().unwrap_or_else(|p| p.into_inner());
            // Reject queued/revoked generations before hydration, serialization or disk I/O.
            if !allowed.load(Ordering::Acquire)
                || until.is_some_and(|end| Instant::now() >= end)
                || inner.lock().unwrap_or_else(|p| p.into_inner()).job_object != object
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "state write ended",
                ));
            }
            // Unknown validity hides consumers, but must never destructively erase history.
            // Hydration and filtering are inside the bounded, generation-fenced write task.
            if let Some(store) = evidence {
                store
                    .hydrate()
                    .map_err(|e| std::io::Error::from(e.kind()))?;
                let history = store.filter_history(state.get("history").clone());
                wtf16::set(&mut state.0, "history", history);
            }
            file.save_if(&state, || {
                allowed.load(Ordering::Acquire)
                    && until.is_none_or(|end| Instant::now() < end)
                    && inner.lock().unwrap_or_else(|p| p.into_inner()).job_object == object
            })
        });
        *slot = Some(PendingWrite {
            task,
            permit: Arc::clone(&permit),
        });
        let pending = slot
            .as_mut()
            .ok_or_else(|| std::io::Error::other("state write task"))?;
        let result = match until {
            Some(end) => match tokio::time::timeout_at(end.into(), &mut pending.task).await {
                Ok(result) => result,
                Err(_) => {
                    permit.store(false, Ordering::Release);
                    return Err(timed_out());
                }
            },
            None => (&mut pending.task).await,
        };
        *slot = None;
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
        let history = self.filtered_history(self.with(|i| i.state.get("history").clone()));
        match history.get(model) {
            v if v.truthy() => v.clone(),
            _ => JValue::Arr(Vec::new()),
        }
    }

    pub(super) fn filtered_history(&self, history: JValue) -> JValue {
        self.maint.evidence.as_ref().map_or_else(
            || history.clone(),
            |store| store.filter_history(history.clone()),
        )
    }

    fn tag_run_record(&self, mut record: JValue) -> JValue {
        if let JValue::Obj(fields) = &mut record {
            let id = self.job_get("id");
            let model = self.job_get("model");
            let generation = self
                .maint
                .evidence
                .as_ref()
                .and_then(|store| {
                    store.generation_for(model.as_str().unwrap_or(""), id.as_str().unwrap_or(""))
                })
                .unwrap_or(0);
            wtf16::set(fields, "runId", id);
            wtf16::set(fields, "runGeneration", JValue::Str(generation.to_string()));
        }
        record
    }

    fn release_local_fence(&self) {
        let fence = self
            .local_fence
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(fence) = fence {
            fence.release();
        }
    }

    pub(super) fn interrupt_fence_wait(&self) {
        if !self.fence_interrupted.swap(true, Ordering::AcqRel) {
            self.fence_wake
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .notify_one();
        }
    }

    /// Storage publication is fail-closed: a failed run cannot release calibration admission
    /// until its small tombstone is durable. This does not take the evidence append mutex.
    async fn tombstone_job(&self, job: &JValue) {
        let Some(store) = self.maint.evidence.clone() else {
            return;
        };
        let model = job.get("model").as_str().unwrap_or("").to_owned();
        let id = job.get("id").as_str().unwrap_or("").to_owned();
        self.with(|i| {
            if i.state.job().get("id").as_str() == Some(id.as_str()) {
                if let Some(job) = i.state.job_mut() {
                    if !i.decided {
                        wtf16::set(job, "phase", JValue::Str("Finalizing".into()));
                    }
                    wtf16::set(job, "pendingTombstone", JValue::Bool(true));
                }
            }
        });
        let wake = Arc::clone(&self.fence_wake.lock().unwrap_or_else(|p| p.into_inner()));
        let mut wait = Duration::from_secs(1);
        loop {
            let (store, model, id) = (Arc::clone(&store), model.clone(), id.clone());
            let result = tokio::task::spawn_blocking(move || store.tombstone(&model, &id)).await;
            match result {
                Ok(Ok(())) => return,
                Ok(Err(e)) => eprintln!(
                    "[models] calibration fence persistence failed: {:?}",
                    e.kind()
                ),
                Err(_) => eprintln!("[models] calibration fence persistence failed: Other"),
            }
            // Cancellation wakes a single wait; it never releases the retained local fence.
            tokio::select! { () = tokio::time::sleep(wait) => {}, () = wake.notified() => {} }
            wait = (wait * 2).min(Duration::from_secs(60));
        }
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
        let guard = RecoveryGuard {
            worker: Arc::clone(&worker),
            object: worker.with(|i| i.job_object),
        };
        Some(tokio::spawn(async move {
            let _guard = guard;
            worker.recover_held().await
        }))
    }

    /// `recover()`: a job a restart left running becomes interrupted (its profile restored if
    /// models.ini is still what it last wrote). `None` when a launch holds the slot.
    pub async fn recover(self: &Arc<Self>) -> Option<Option<bool>> {
        if !self.begin_recovery() {
            return None;
        }
        let _guard = RecoveryGuard {
            worker: Arc::clone(self),
            object: self.with(|i| i.job_object),
        };
        self.recover_held().await
    }

    fn begin_recovery(&self) -> bool {
        self.with(|i| {
            if i.launching || i.active {
                return false;
            }
            // Reserve synchronously when the cached job already identifies recovery work,
            // before the spawned recovery can yield to another maintenance request.
            if i.state.job().get("status").as_str() == Some("running")
                || i.state.job().get("pendingRestore").truthy()
                || i.state.job().get("pendingTombstone").truthy()
            {
                let mut fence = self.local_fence.lock().unwrap_or_else(|p| p.into_inner());
                if fence.is_none() {
                    *fence = super::gate::AdmissionFence::reserve(Arc::clone(&self.maint.local));
                }
            }
            *self.fence_wake.lock().unwrap_or_else(|p| p.into_inner()) = Arc::new(Notify::new());
            self.fence_interrupted.store(false, Ordering::Release);
            i.job_object = i.next_object;
            i.next_object += 1;
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
        self.drain_writes().await;
        let file = self.file.clone();
        let loaded = tokio::task::spawn_blocking(move || file.load()).await.ok();
        let cached = self.with(|i| i.state.job().clone());
        if let Some(state) = loaded.filter(|state| {
            // An aborted recovery's last save may already have landed. Adopt that same job's
            // durable terminal record rather than retaining the stale pending marker in memory.
            (state.job().get("id") == cached.get("id")
                && !state.job().get("finishedAt").is_nullish()
                && (cached.get("pendingTombstone").truthy()
                    || cached.get("pendingRestore").truthy()))
                || state.job().get("status").as_str() == Some("running")
                || state.job().get("pendingRestore").truthy()
                || state.job().get("pendingTombstone").truthy()
        }) {
            loop {
                if self
                    .local_fence
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_some()
                {
                    break;
                }
                if let Some(fence) =
                    super::gate::AdmissionFence::reserve(Arc::clone(&self.maint.local))
                {
                    *self.local_fence.lock().unwrap_or_else(|p| p.into_inner()) = Some(fence);
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            self.with(|i| i.state = state);
        }
        let decided = self.with(|i| {
            i.decided = matches!(
                i.state.job().get("terminalDecision").as_str(),
                Some("passed" | "fenced")
            );
            if i.state.job().get("terminalDecision").as_str() == Some("passed") {
                Some((i.state.clone(), i.job_object))
            } else {
                None
            }
        });
        if let Some((state, object)) = decided {
            let restored = match state.job().get("restored") {
                JValue::Bool(v) => Some(*v),
                _ => None,
            };
            self.complete_decision(state, object).await;
            return Some(restored);
        }
        let wake = Arc::clone(&self.fence_wake.lock().unwrap_or_else(|p| p.into_inner()));
        let object = self.with(|i| i.job_object);
        let outcome = match self.maint.prepare_calibration_recovery(Some(wake)).await {
            None => Recovery::Nothing,
            Some(prepared) => {
                let saved = if prepared.decided {
                    self.save_decision(&prepared.state, object).await;
                    true
                } else {
                    self.write_state(
                        prepared.state.clone(),
                        object,
                        Some(Instant::now() + self.limits.finalize),
                    )
                    .await
                    .is_ok()
                };
                if saved {
                    Recovery::Saved {
                        restored: prepared.restored,
                        state: prepared.state,
                    }
                } else {
                    Recovery::Unsaved {
                        restored: prepared.restored,
                        state: prepared.state,
                    }
                }
            }
        };
        // Even a bounded legacy save may have an unresolved blocking owner after timeout.
        // Resolve/discard it before publishing or releasing the recovery's admission fence.
        self.drain_writes().await;
        let restored = outcome.restored();
        let unsaved = matches!(outcome, Recovery::Unsaved { .. });
        let replacement =
            if let Recovery::Saved { state, .. } | Recovery::Unsaved { state, .. } = outcome {
                let mut state = state;
                if unsaved {
                    if let Some(job) = state.job_mut() {
                        let error = job_error(job);
                        wtf16::set(job, "error", JValue::Str(format!("{error}{UNSAVED}")));
                    }
                }
                Some(state)
            } else {
                None
            };
        self.with(|i| {
            if let Some(state) = replacement {
                i.state = state;
                i.job_object = i.next_object;
                i.next_object += 1;
                i.unsaved = unsaved;
            }
            self.release_local_fence();
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
    async fn save_recovered(&self, release_launch: bool) -> std::io::Result<()> {
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
                if release_launch {
                    i.launching = false;
                }
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
                        let saved = worker.save_recovered(true).await;
                        if saved.is_err() {
                            worker.with(|i| i.launching = false);
                        }
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
            if i.recovering || i.decided {
                self.interrupt_fence_wait();
                return None;
            }
            if !i.active && i.state.job().get("status").as_str() != Some("running") {
                return None;
            }
            let job = visible_job(i);
            if i.state.job().get("phase").as_str() == Some("Finalizing") {
                self.interrupt_fence_wait();
            }
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
        if self.restoring.load(Ordering::Acquire)
            || self.with(|i| i.state.job().get("pendingTombstone").truthy())
            || self.maint.router.is_none()
            || self.maint.ini.is_none()
        {
            return Err(Refusal::Unavailable);
        }
        if self.unsaved() && self.save_recovered(false).await.is_err() {
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
            if i.decided {
                return;
            }
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
        mut hold: Hold,
        model: &str,
        plan: Plan,
    ) -> Result<JValue, Refusal> {
        let guard = if (self.memory)().is_none() {
            "unavailable"
        } else {
            "active"
        };
        *self.fence_wake.lock().unwrap_or_else(|p| p.into_inner()) = Arc::new(Notify::new());
        self.fence_interrupted.store(false, Ordering::Release);
        let id = uuid();
        if let Some(store) = self.maint.evidence.clone() {
            let (model, id) = (model.to_owned(), id.clone());
            match tokio::task::spawn_blocking(move || store.admit_run(&model, &id)).await {
                Ok(Ok(_)) => {}
                _ => {
                    hold.release().await;
                    return Err(Refusal::Unavailable);
                }
            }
        }
        let job = JValue::obj([
            ("id", JValue::Str(id)),
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
            i.decided = false;
            i.state.set_job(job);
            i.job_object = i.next_object;
            i.next_object += 1;
            p
        });
        let (previous, previous_cancel, previous_object) = (previous.0, previous.1, previous.2);
        if self.save().await.is_err() {
            let job = self.with(|i| i.state.job().clone());
            self.tombstone_job(&job).await;
            self.with(|i| {
                i.state.set_job(previous);
                i.cancel_requested = previous_cancel;
                i.job_object = previous_object;
            });
            hold.release().await;
            return Err(Refusal::Unavailable);
        }
        // Transfer local admission before spawning: even an unpolled task cannot drop it
        // before its durable failure fence. The run guard resolves that fence on cancellation.
        *self.local_fence.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(hold.defer_local_release());
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
        let task = tokio::spawn(async move {
            let _guard = guard;
            worker.run(hold).await;
        });
        #[cfg(test)]
        {
            *self.run_abort.lock().unwrap_or_else(|p| p.into_inner()) = Some(task.abort_handle());
        }
        #[cfg(not(test))]
        drop(task);
        Ok(self.job())
    }

    /// `run(job, release)`.
    async fn run(self: Arc<Self>, hold: Hold) {
        #[cfg(test)]
        #[allow(clippy::panic)]
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
        self.job_set("pendingTombstone", JValue::Bool(true));
        // One deadline covers every post-outcome await, including blocking-store work.
        let until = Instant::now() + self.limits.finalize;
        let object = self.with(|i| i.job_object);
        let permit = Arc::new(AtomicBool::new(true));
        let inner = Arc::clone(&self.inner);
        let live = Arc::clone(&permit);
        let allowed: super::ini::WriteAllowed = Arc::new(move || {
            live.load(Ordering::Acquire)
                && Instant::now() < until
                && inner
                    .lock()
                    .is_ok_and(|i| i.active && i.job_object == object)
        });
        let revoke = WritePermit(permit);
        let timed_out =
            tokio::time::timeout_at(until.into(), self.post_outcome(&hold, outcome, allowed))
                .await
                .is_err();
        drop(revoke);
        self.finalize(hold, until, timed_out).await;
    }

    async fn post_outcome(
        self: &Arc<Self>,
        hold: &Hold,
        outcome: Result<Option<JValue>, Thrown>,
        allowed: super::ini::WriteAllowed,
    ) {
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
                        self.restore(hold, rev, text, Arc::clone(&allowed)).await
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
                let record = self.tag_run_record(record);
                let _ = self
                    .maint
                    .record_evidence_if(&model, &record, Arc::clone(&allowed))
                    .await;
            }
        } else if let Some(entry) = outcome.ok().flatten() {
            // After the lease check: a run whose hold ended after it passed was restored above and
            // must not leave a verified size in the history that the profile no longer has.
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
            let record = self.tag_run_record(record);
            let _ = self
                .maint
                .record_evidence_if(&model, &record, Arc::clone(&allowed))
                .await;
            if allowed() {
                let entry = self.tag_run_record(entry);
                self.with(|i| i.state.push_history(&model, entry));
            }
        }
    }

    async fn finalize(self: &Arc<Self>, mut hold: Hold, until: Instant, post_failed: bool) {
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
        if !self.restoring.load(Ordering::Acquire) {
            if let Some(job) = state.job_mut() {
                job.retain(|(k, _)| k != "pendingRestore");
            }
        }
        finish_snapshot(&mut state, post_failed);
        if state.job().get("status").as_str() != Some("passed") {
            self.tombstone_job(state.job()).await;
        }
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
        // Cancellation and this choice serialize on the same mutex. Once sealed, neither
        // cancellation nor unwind can change the outcome, even during an uncertain save.
        let decided = self.with(|i| {
            let failed = post_failed
                || !saved
                || !released
                || i.cancel_requested
                || (hold.lost() && state.job().get("status").as_str() == Some("passed"));
            if failed {
                finish_snapshot(&mut state, true);
            }
            if let Some(job) = state.job_mut() {
                wtf16::set(job, "pendingTombstone", JValue::Bool(true));
                let decision = if state_status_passed(job) {
                    "passed"
                } else {
                    "fenced"
                };
                wtf16::set(job, "terminalDecision", JValue::Str(decision.into()));
            }
            i.decided = true;
            i.state = state.clone();
            state
        });
        self.complete_decision(decided, object).await;
        drop(hold);
    }

    /// Timeouts observe the SAME pending operation, never queue a replacement. Only a
    /// completed failure gets the shared interruptible 1s..60s retry backoff.
    async fn save_decision(&self, state: &State, object: u64) {
        let wake = Arc::clone(&self.fence_wake.lock().unwrap_or_else(|p| p.into_inner()));
        let mut wait = Duration::from_secs(1);
        loop {
            let attempt = self.write_state(state.clone(), object, None);
            tokio::pin!(attempt);
            let result = loop {
                tokio::select! {
                    result = tokio::time::timeout(self.limits.finalize, &mut attempt) => match result {
                        Ok(result) => break result,
                        Err(_) => eprintln!("[models] calibration terminal persistence failed: TimedOut"),
                    },
                    () = wake.notified() => {},
                }
            };
            match result {
                Ok(()) => return,
                Err(e) => eprintln!(
                    "[models] calibration terminal persistence failed: {:?}",
                    e.kind()
                ),
            }
            tokio::select! { () = tokio::time::sleep(wait) => {}, () = wake.notified() => {} }
            wait = (wait * 2).min(SAVE_RETRY_MAX);
        }
    }

    async fn complete_decision(self: &Arc<Self>, mut state: State, object: u64) {
        // Persist the decision WITH its recovery marker before any marker-free terminal record
        // can exist. This also makes a timed-out/lost-response save idempotent on restart.
        self.save_decision(&state, object).await;
        if state.job().get("terminalDecision").as_str() != Some("passed") {
            self.tombstone_job(state.job()).await;
        }
        if let Some(job) = state.job_mut() {
            job.retain(|(k, _)| k != "pendingTombstone" && k != "terminalDecision");
        }
        self.save_decision(&state, object).await;
        self.with(|i| {
            if i.job_object != object {
                return;
            }
            i.state = state;
            i.cancel_requested = false;
            i.lease_lost = false;
            i.active = false;
            i.unsaved = false;
            if i.recovering {
                i.recovering = false;
                i.launching = false;
            }
            self.release_local_fence();
        });
    }

    /// Put `text` back if models.ini is still at `revision`, then reload the router.
    ///
    /// After a lost hold this runs without exclusion, on purpose: the profile under test must not
    /// outlive the run, and the file write is still compare-and-swap on the revision, so nothing
    /// another writer saved is overwritten. Only the raw reload is unexcluded: once Node has ended
    /// the lease it can admit a chat, which that reload may interrupt. Leaving a half-tested profile
    /// live would be worse, so the trade-off is kept.
    async fn restore(
        &self,
        hold: &Hold,
        revision: &str,
        text: &str,
        allowed: super::ini::WriteAllowed,
    ) -> bool {
        let Some(ini) = self.maint.ini.clone() else {
            return false;
        };
        let c = super::ini::Candidate {
            base_revision: revision.to_owned(),
            text: text.to_owned(),
            before: String::new(),
        };
        if !allowed() {
            return false;
        }
        // Persist the exact idempotent CAS before dispatch. A terminal failed job can still have
        // this private marker when its resolver outlives finalization; boot recovery must settle it.
        self.job_set(
            "pendingRestore",
            JValue::obj([
                ("baseRevision", JValue::Str(c.base_revision.clone())),
                ("text", JValue::Str(c.text.clone())),
            ]),
        );
        let (mut state, object) = self.with(|i| (i.state.clone(), i.job_object));
        if let Some(job) = state.job_mut() {
            wtf16::set(job, "status", JValue::Str("running".into()));
            job.retain(|(k, _)| k != "finishedAt");
        }
        if self
            .write_state(state, object, Some(Instant::now() + self.limits.finalize))
            .await
            .is_err()
        {
            return false;
        }
        self.restoring.store(true, Ordering::Release);
        let resolver = Arc::new(RestoreResolver {
            ini,
            candidate: c,
            pending: Arc::clone(&self.restoring),
            retry: self.limits.save_retry,
            #[cfg(test)]
            panic_once: Arc::clone(&self.panic_on_restore),
        });
        let restored = resolver.spawn().await.unwrap_or(false);
        if restored && allowed() {
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
        }
        restored
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
        // Bounded at 30 s: a run's state saves on the way here can wait out a slow fsync.
        for _ in 0..3000 {
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

    async fn review_repair_decided_save(w: &Worker, backup: &std::path::Path) {
        delta_wait(|| w.with(|i| i.decided)).await;
        assert_eq!(w.job().get("status").as_str(), Some("running"));
        assert_eq!(w.job().get("phase").as_str(), Some("Finalizing"));
        assert!(w.job().get("finishedAt").is_nullish());
        assert!(w.maint.local_held());
        assert!(w.with(|i| i.active));
        let path = w.file.path.as_ref().unwrap();
        std::fs::remove_dir(path).unwrap();
        std::fs::rename(backup, path).unwrap();
        w.interrupt_fence_wait();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_persistent_final_save_failure_retains_admission_until_durable() {
        let (_d, w, backup) = review_blocked_save().await;
        review_repair_decided_save(&w, &backup).await;
        let job = tokio::time::timeout(Duration::from_secs(3), settled(&w))
            .await
            .unwrap();
        review_failed(&w, &job);
        assert_eq!(w.file.load().job().get("status").as_str(), Some("failed"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_cancel_during_finalization_is_terminal() {
        let (_d, w, backup) = review_blocked_save().await;
        review_wait_outcome(&w).await;
        assert_eq!(w.job().get("status").as_str(), Some("running"));
        assert!(
            w.cancel().is_some(),
            "pre-decision finalization is cancellable"
        );
        review_repair_decided_save(&w, &backup).await;
        let job = tokio::time::timeout(Duration::from_secs(3), settled(&w))
            .await
            .unwrap();
        review_failed(&w, &job);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_lease_loss_during_finalization_is_terminal() {
        let (_d, w, backup) = review_blocked_save().await;
        review_wait_outcome(&w).await;
        w.lose_exclusion();
        review_repair_decided_save(&w, &backup).await;
        let job = tokio::time::timeout(Duration::from_secs(3), settled(&w))
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
        tokio::time::timeout(Duration::from_secs(3), async {
            while w.unsaved() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("failed record retry persisted");
        assert_eq!(w.file.load().job().get("status").as_str(), Some("failed"));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_post_outcome_evidence_is_bounded_and_cannot_write_into_a_new_run() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        std::fs::write(d.path().join("synthetic.gguf"), b"GGUF synthetic").unwrap();
        let (port, _) = synthetic_engine(ini, false).await;
        let w = worker(d.path(), port);
        let w = with_limits(
            &w,
            Limits {
                finalize: Duration::from_millis(300),
                ..w.limits
            },
        );
        let store = Arc::clone(w.maint.evidence.as_ref().unwrap());
        let (entered, started) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let blocked = std::thread::spawn(move || store.stall_writes(entered, resume));
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        review_wait_outcome(&w).await;
        // Evidence is waiting on the actual store mutex after the successful search.
        let result = tokio::time::timeout(Duration::from_secs(2), settled(&w)).await;
        if let Ok(job) = &result {
            review_failed(&w, job);
            assert!(!w.with(|i| i.active));
            let h = w
                .maint
                .acquire_for_run("synthetic admission after timeout", Duration::from_secs(1))
                .await;
            assert!(h.is_ok(), "local maintenance admission released");
            drop(h);
            w.launch("synthetic", plan(32768.0, 120.0))
                .await
                .expect("new run after timeout");
            let _ = w.cancel();
        }
        release.send(()).unwrap();
        blocked.join().unwrap();
        result.expect("post-outcome evidence deadline must publish failure");
        settled(&w).await;
        assert_eq!(
            evidence_count(&w).await,
            0,
            "expired evidence must never append later"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovery_save_publishes_unsaved_and_launch_release_together() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("models.ini"), INI).unwrap();
        let (port, _) = synthetic_engine(d.path().join("models.ini"), true).await;
        let w = worker(d.path(), port);
        w.with(|i| {
            i.state = State::empty();
            i.state
                .set_job(wtf16::parse(r#"{"id":"recovered","status":"interrupted"}"#).unwrap());
            i.unsaved = true;
            i.launching = true;
        });
        // The background retry publishes the recovered record before it returns to its caller.
        // A reader observing unsaved=false must also observe the released launch reservation.
        w.save_recovered(true).await.unwrap();
        assert_eq!(w.with(|i| (i.unsaved, i.launching)), (false, false));
        w.launch("synthetic", plan(32768.0, 120.0))
            .await
            .expect("immediate restart after recovery publication");
        let _ = w.cancel();
        settled(&w).await;
    }
    // SETTLEMENT_REGRESSIONS_BEGIN
    async fn restore_rig(
        mode: &'static str,
    ) -> (
        tempfile::TempDir,
        Arc<Worker>,
        Arc<AtomicBool>,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
    ) {
        use axum::{
            body::Body,
            http::{Request, Response},
        };
        use http_body_util::BodyExt;
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (engine, seen) = synthetic_engine(ini.clone(), true).await;
        let available = Arc::new(AtomicBool::new(mode != "down"));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (ready, attempts, file) = (Arc::clone(&available), Arc::clone(&calls), ini.clone());
        let app = axum::Router::new().fallback(move |r: Request<Body>| {
            let (ready, attempts, file) = (Arc::clone(&ready), Arc::clone(&attempts), file.clone());
            async move {
                let bytes = r.into_body().collect().await.unwrap().to_bytes();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                let current = std::fs::read_to_string(&file).unwrap();
                let text = body["text"].as_str().unwrap();
                let (code, reply) = if body["baseRevision"].as_str()
                    != Some(super::super::ini::sha(&current).as_str())
                {
                    (409, json!({"detail":"changed"}))
                } else if text == INI {
                    let attempt = attempts.fetch_add(1, Ordering::AcqRel);
                    match mode {
                        "lost" if attempt == 0 => {
                            std::fs::write(&file, text).unwrap();
                            (503, json!({"error":"synthetic lost reply"}))
                        }
                        "moved" if attempt == 0 => {
                            std::fs::write(&file, format!("{INI}# later edit\n")).unwrap();
                            (503, json!({"error":"synthetic lost reply"}))
                        }
                        _ if !ready.load(Ordering::Acquire) => {
                            (503, json!({"error":"synthetic unavailable"}))
                        }
                        _ => {
                            std::fs::write(&file, text).unwrap();
                            (200, json!({"ok":true}))
                        }
                    }
                } else {
                    std::fs::write(&file, text).unwrap();
                    (200, json!({"ok":true}))
                };
                Response::builder()
                    .status(code)
                    .body(Body::from(reply.to_string()))
                    .unwrap()
            }
        });
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut w = worker(d.path(), engine);
        let maint = Arc::get_mut(&mut Arc::get_mut(&mut w).unwrap().maint).unwrap();
        maint.ini = Some(super::super::ini::Store::new(
            ini,
            Some(
                super::super::ini::LoaderWriter::new(&format!("http://127.0.0.1:{port}"), None)
                    .unwrap(),
            ),
        ));
        // `finalize` also bounds every launch, restore-intent and recovery state save, each of
        // which refuses fail-closed when it lapses (an unsaved launch is Unavailable). A loaded
        // shared runner's fsync can take seconds, so these tests must not race it: they wait on
        // published state, and only the unreachable-restore case waits out this deadline.
        let w = with_limits(
            &w,
            Limits {
                finalize: Duration::from_secs(10),
                save_retry: Duration::from_millis(20),
                ..w.limits
            },
        );
        (d, w, available, calls, seen)
    }
    async fn wait_restoration(w: &Worker) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while w.restoring.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("owned restoration settles and readmits");
    }
    async fn begin_restore_run(w: &Arc<Worker>, seen: &Mutex<Vec<String>>) {
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        released(seen, "POST /v1/chat/completions").await;
        w.cancel().unwrap();
    }
    async fn assert_restore_readmits(w: &Arc<Worker>) {
        wait_restoration(w).await;
        settled(w).await;
        w.launch("synthetic", plan(32768.0, 120.0))
            .await
            .expect("settled CAS readmits a successor");
        let _ = w.cancel();
        settled(w).await;
        wait_restoration(w).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lost_restore_response_settles_by_readback_and_readmits() {
        let (d, w, _, calls, seen) = restore_rig("lost").await;
        begin_restore_run(&w, &seen).await;
        settled(&w).await;
        wait_restoration(&w).await;
        assert_eq!(
            std::fs::read_to_string(d.path().join("models.ini")).unwrap(),
            INI
        );
        assert_eq!(
            calls.load(Ordering::Acquire),
            1,
            "readback settles without another write"
        );
        assert_restore_readmits(&w).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_moved_restore_cas_settles_without_overwriting_the_new_revision() {
        let (d, w, _, calls, seen) = restore_rig("moved").await;
        begin_restore_run(&w, &seen).await;
        settled(&w).await;
        wait_restoration(&w).await;
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(
            std::fs::read_to_string(d.path().join("models.ini")).unwrap(),
            format!("{INI}# later edit\n")
        );
        assert_restore_readmits(&w).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unreachable_restore_refuses_calibration_but_keeps_inference_admitted() {
        let (_d, w, available, calls, seen) = restore_rig("down").await;
        begin_restore_run(&w, &seen).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            while calls.load(Ordering::Acquire) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        settled(&w).await;
        assert!(w.restoring.load(Ordering::Acquire));
        assert_eq!(
            w.launch("synthetic", plan(32768.0, 120.0)).await,
            Err(Refusal::Unavailable)
        );
        assert!(!w.maint.local_held());
        let reply = w
            .maint
            .router
            .as_ref()
            .unwrap()
            .request(
                reqwest::Method::POST,
                "/v1/chat/completions",
                Some(&json!({"messages":[],"stream":false})),
                None,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(
            reply.status, 200,
            "ordinary inference remains admitted while only calibration waits"
        );
        available.store(true, Ordering::Release);
        assert_restore_readmits(&w).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicked_restore_is_rescheduled_until_settled() {
        let (d, w, _, _, seen) = restore_rig("normal").await;
        w.panic_on_restore.store(true, Ordering::Release);
        begin_restore_run(&w, &seen).await;
        settled(&w).await;
        wait_restoration(&w).await;
        assert_eq!(
            std::fs::read_to_string(d.path().join("models.ini")).unwrap(),
            INI
        );
        assert_restore_readmits(&w).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boot_recovery_retries_the_same_restore_until_it_settles() {
        let (d, w, available, calls, _) = restore_rig("down").await;
        let modified = INI.replace("4096", "8192");
        std::fs::write(d.path().join("models.ini"), &modified).unwrap();
        let mut state = State::empty();
        state.set_job(JValue::obj([
            ("id", JValue::Str("boot-restore".into())),
            ("model", JValue::Str("synthetic".into())),
            ("status", JValue::Str("failed".into())),
            ("finishedAt", JValue::Num(100.0)),
            (
                "pendingRestore",
                JValue::obj([
                    (
                        "baseRevision",
                        JValue::Str(super::super::ini::sha(&modified)),
                    ),
                    ("text", JValue::Str(INI.into())),
                ]),
            ),
        ]));
        w.file.save(&state).unwrap();
        let worker = Arc::clone(&w);
        let recovery = tokio::spawn(async move { worker.recover().await });
        tokio::time::timeout(Duration::from_secs(15), async {
            while calls.load(Ordering::Acquire) < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("boot recovery retains and retries the unsettled CAS");
        available.store(true, Ordering::Release);
        assert_eq!(
            // Recovery's own CAS retry backs off 1 s, 2 s, ...: allow several rounds.
            tokio::time::timeout(Duration::from_secs(15), recovery)
                .await
                .unwrap()
                .unwrap(),
            Some(Some(true))
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("models.ini")).unwrap(),
            INI
        );
        assert_eq!(w.job().get("status").as_str(), Some("failed"));
        assert_eq!(w.job().get("finishedAt").as_num(), Some(100.0));
        assert!(w.file.load().job().get("pendingRestore").is_nullish());
        assert_restore_readmits(&w).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_append_stalled_after_its_permit_is_never_visible_after_expiry() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        std::fs::write(d.path().join("synthetic.gguf"), b"GGUF synthetic").unwrap();
        let (port, _) = synthetic_engine(ini, false).await;
        let w = worker(d.path(), port);
        let w = with_limits(
            &w,
            Limits {
                finalize: Duration::from_millis(150),
                ..w.limits
            },
        );
        let store = Arc::clone(w.maint.evidence.as_ref().unwrap());
        let (entered, started) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        store.stall_next_append(entered, resume);
        let old = w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(3)))
            .await
            .unwrap()
            .unwrap();
        let record = w.tag_run_record(JValue::obj([("model", JValue::Str("synthetic".into()))]));
        // Cover old persisted history as well as the deferred evidence append.
        w.with(|i| i.state.push_history("synthetic", record.clone()));
        let job = tokio::time::timeout(Duration::from_secs(2), settled(&w))
            .await
            .unwrap();
        review_failed(&w, &job);
        let withdrawn_before_next = !store.visible("synthetic", &record);
        let reopened =
            super::super::evidence::Store::new(store.file().parent().unwrap().to_owned());
        let durable_before_next =
            std::fs::read(store.file().parent().unwrap().join("run-state.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some_and(|ledger| {
                    ledger["models"]["synthetic"]["id"].as_str() == old.get("id").as_str()
                        && ledger["models"]["synthetic"]["tombstoned"] == true
                });
        let next = w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        assert_ne!(next.get("id"), old.get("id"));
        let _ = w.cancel();
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !std::fs::read_to_string(store.file())
                .unwrap_or_default()
                .contains(old.get("id").as_str().unwrap())
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("old physical append really completed after successor admission");
        assert!(
            store.list().is_empty(),
            "late physical evidence is invisible"
        );
        assert!(
            withdrawn_before_next,
            "tombstone was effective before successor admission"
        );
        assert!(
            durable_before_next,
            "tombstone was durable before successor admission"
        );
        assert_eq!(
            w.state_facts().get("history").get("synthetic"),
            &JValue::Arr(vec![])
        );
        assert_eq!(store.count(), 0);
        assert!(reopened.list().is_empty());
        let body = w.maint.evidence_body("synthetic").await.unwrap();
        assert!(!body.contains(old.get("id").as_str().unwrap()));
        assert_eq!(w.history("synthetic"), JValue::Arr(vec![]));
        assert_eq!(store.compact().unwrap(), 0);
        assert!(
            !std::fs::read_to_string(store.file())
                .unwrap()
                .contains(old.get("id").as_str().unwrap()),
            "compaction physically drops the expired record"
        );
        settled(&w).await;
        // A reader opened before the successor must see valid new-generation records too;
        // suppression cannot be implemented by making every consumer permanently empty.
        store
            .append(
                &JValue::obj([
                    ("model", JValue::Str("synthetic".into())),
                    ("category", JValue::Str("vision".into())),
                    ("result", JValue::Str("passed".into())),
                ]),
                1,
            )
            .unwrap();
        assert_eq!(store.count(), 1);
        assert_eq!(reopened.count(), 1);
        assert!(reopened
            .list()
            .iter()
            .all(|r| r.get("runId") != old.get("id")));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persistent_tombstone_errors_keep_admission_through_cancel_and_task_abort() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, seen) = synthetic_engine(ini, true).await;
        let w = worker(d.path(), port);
        let store = Arc::clone(w.maint.evidence.as_ref().unwrap());
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        released(&seen, "POST /v1/chat/completions").await;
        store.inject_run_write_error(true);
        w.cancel().unwrap();
        tokio::time::timeout(Duration::from_secs(4), async {
            while store.run_write_failures().len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let failures = store.run_write_failures();
        assert!(
            failures[1].duration_since(failures[0]) >= Duration::from_secs(1),
            "backoff never spins"
        );
        let job = w.job();
        assert_eq!(job.get("status").as_str(), Some("running"));
        assert_eq!(job.get("phase").as_str(), Some("Finalizing"));
        assert!(job.get("finishedAt").is_nullish());
        assert!(w.maint.local_held());
        assert!(w
            .maint
            .acquire_for_run("synthetic fence check", Duration::from_secs(1))
            .await
            .is_err());
        // Cancellation interrupts the wait without releasing the retained local admission.
        let attempts = store.run_write_failures().len();
        w.cancel().unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while store.run_write_failures().len() == attempts {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cancellation interrupts the retry wait");
        assert!(w.maint.local_held());
        // Task cancellation/shutdown drops Hold, but the worker still owns the local fence.
        w.run_abort.lock().unwrap().as_ref().unwrap().abort();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !w.run_abort.lock().unwrap().as_ref().unwrap().is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(w.maint.local_held());
        assert_eq!(w.job().get("phase").as_str(), Some("Finalizing"));
        assert!(w.job().get("finishedAt").is_nullish());
        store.inject_run_write_error(false);
        let job = tokio::time::timeout(Duration::from_secs(4), settled(&w))
            .await
            .unwrap();
        review_failed(&w, &job);
        tokio::time::timeout(Duration::from_secs(3), async {
            while w.unsaved() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(w.file.load().job().get("status").as_str(), Some("failed"));
        assert!(!w.maint.local_held());
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        let _ = w.cancel();
        settled(&w).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_recovers_a_pending_tombstone_before_releasing_admission() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, _) = synthetic_engine(ini, false).await;
        let w = worker(d.path(), port);
        let store = Arc::clone(w.maint.evidence.as_ref().unwrap());
        store.admit_run("synthetic", "unfinished-fence").unwrap();
        let mut state = State::empty();
        state.set_job(JValue::obj([
            ("id", JValue::Str("unfinished-fence".into())),
            ("model", JValue::Str("synthetic".into())),
            ("status", JValue::Str("passed".into())),
            ("finishedAt", JValue::Num(100.0)),
            ("pendingTombstone", JValue::Bool(true)),
        ]));
        w.file.save(&state).unwrap();
        let restarted = with_limits(&w, w.limits);
        store.inject_run_write_error(true);
        let recovery = restarted.spawn_recover().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while store.run_write_failures().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("recovery retries the pending fence first");
        assert!(restarted.maint.local_held());
        assert_eq!(restarted.job().get("status").as_str(), Some("running"));
        assert_eq!(restarted.job().get("phase").as_str(), Some("Finalizing"));
        assert!(restarted.job().get("finishedAt").is_nullish());
        store.inject_run_write_error(false);
        tokio::time::timeout(Duration::from_secs(3), recovery)
            .await
            .unwrap()
            .unwrap();
        assert!(!restarted.maint.local_held());
        assert_eq!(restarted.job().get("status").as_str(), Some("failed"));
        assert!(!restarted.job().get("finishedAt").is_nullish());
        assert!(restarted
            .file
            .load()
            .job()
            .get("pendingTombstone")
            .is_nullish());
        restarted
            .launch("synthetic", plan(32768.0, 120.0))
            .await
            .unwrap();
        let _ = restarted.cancel();
        settled(&restarted).await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn an_unpolled_run_keeps_its_local_fence_until_the_tombstone_is_durable() {
        let d = tempfile::tempdir().unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, INI).unwrap();
        let (port, _) = synthetic_engine(ini, false).await;
        let w = worker(d.path(), port);
        let store = Arc::clone(w.maint.evidence.as_ref().unwrap());
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        // On this single-threaded runtime there is no await between spawn and abort, so the
        // run future has never been polled. Its local fence must already have been transferred.
        store.inject_run_write_error(true);
        w.run_abort.lock().unwrap().as_ref().unwrap().abort();
        tokio::time::timeout(Duration::from_secs(2), async {
            while store.run_write_failures().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            w.maint.local_held(),
            "unpolled task drop cannot release an unpersisted fence"
        );
        assert_eq!(w.job().get("phase").as_str(), Some("Finalizing"));
        assert!(w.job().get("finishedAt").is_nullish());
        store.inject_run_write_error(false);
        let job = tokio::time::timeout(Duration::from_secs(3), settled(&w))
            .await
            .unwrap();
        review_failed(&w, &job);
    }

    // SETTLEMENT_DELTA_REGRESSIONS_BEGIN
    async fn delta_wait(mut ready: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(4), async {
            while !ready() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("synthetic transition completed");
    }
    fn delta_pending(w: &Worker, id: &str) {
        let store = w.maint.evidence.as_ref().unwrap();
        store.admit_run("synthetic", "earlier").unwrap();
        assert_eq!(store.admit_run("synthetic", id).unwrap(), 2);
        let mut state = State::empty();
        state.set_job(JValue::obj([
            ("id", JValue::Str(id.into())),
            ("model", JValue::Str("synthetic".into())),
            ("status", JValue::Str("failed".into())),
            ("finishedAt", JValue::Num(100.0)),
            ("pendingTombstone", JValue::Bool(true)),
        ]));
        w.file.save(&state).unwrap();
    }
    fn delta_stall(
        w: &Worker,
        marker_free: bool,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, ready) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        *w.terminal_stall.lock().unwrap() = Some((marker_free, entered, wait));
        (ready, release)
    }
    async fn delta_restart(
        source: &Worker,
        port: u16,
        deny_ledger: bool,
    ) -> (tempfile::TempDir, Arc<Worker>) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("models.ini"), INI).unwrap();
        let restarted = worker(d.path(), port);
        let old = source.maint.evidence.as_ref().unwrap().file();
        let new = restarted.maint.evidence.as_ref().unwrap().file();
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::copy(
            old.parent().unwrap().join("run-state.json"),
            new.parent().unwrap().join("run-state.json"),
        )
        .unwrap();
        std::fs::copy(
            source.file.path.as_ref().unwrap(),
            restarted.file.path.as_ref().unwrap(),
        )
        .unwrap();
        drop(restarted);
        if deny_ledger {
            use std::os::unix::fs::PermissionsExt;
            let path = new.parent().unwrap().join("run-state.json");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
            assert_eq!(
                std::fs::read(path).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        let reopened = worker(d.path(), port);
        (d, reopened)
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_at_terminal_save_preserves_the_durable_decision() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("models.ini"), INI).unwrap();
        let (port, _) = synthetic_engine(d.path().join("models.ini"), false).await;
        let w = worker(d.path(), port);
        let (ready, release) = delta_stall(&w, true);
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        delta_wait(|| ready.try_recv().is_ok()).await;
        let token = w
            .cancellation_facts()
            .get("token")
            .as_str()
            .unwrap()
            .to_owned();
        let cancelled = w.cancel().is_some();
        let applied = w.compare_and_abort(&token).get("applied").truthy();
        w.maint
            .evidence
            .as_ref()
            .unwrap()
            .inject_run_write_error(true);
        w.run_abort.lock().unwrap().as_ref().unwrap().abort();
        release.send(()).unwrap();
        delta_wait(|| w.run_abort.lock().unwrap().as_ref().unwrap().is_finished()).await;
        // An abort after the decision cannot convert it into an unpersisted failure fence.
        let (_copy, restarted) = delta_restart(&w, port, false).await;
        restarted.recover().await;
        assert!(
            !cancelled && !applied,
            "the terminal decision refuses cancellation"
        );
        assert_eq!(restarted.job().get("status").as_str(), Some("passed"));
        assert!(!restarted.maint.local_held());
        w.maint
            .evidence
            .as_ref()
            .unwrap()
            .inject_run_write_error(false);
        delta_wait(|| !w.maint.local_held()).await;
        assert_eq!(
            w.job().get("status").as_str(),
            Some("passed"),
            "abort cleanup preserves the original decision too"
        );

        // Cancellation before the decision stays fenced across a failed control write/restart.
        let w = worker(d.path(), port);
        let (ready, release) = delta_stall(&w, false);
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        delta_wait(|| ready.try_recv().is_ok()).await;
        w.maint
            .evidence
            .as_ref()
            .unwrap()
            .inject_run_write_error(true);
        assert!(w.cancel().is_some());
        release.send(()).unwrap();
        delta_wait(|| {
            !w.maint
                .evidence
                .as_ref()
                .unwrap()
                .run_write_failures()
                .is_empty()
        })
        .await;
        assert!(w.file.load().job().get("pendingTombstone").truthy());
        let (_copy, restarted) = delta_restart(&w, port, false).await;
        let store = restarted.maint.evidence.as_ref().unwrap();
        store.inject_run_write_error(true);
        let recovery = restarted.spawn_recover().unwrap();
        delta_wait(|| !store.run_write_failures().is_empty()).await;
        assert_eq!(restarted.job().get("phase").as_str(), Some("Finalizing"));
        assert!(restarted.job().get("finishedAt").is_nullish());
        assert!(restarted.maint.local_held());
        store.inject_run_write_error(false);
        tokio::time::timeout(Duration::from_secs(4), recovery)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restarted.job().get("status").as_str(), Some("failed"));
        assert!(!restarted.maint.local_held());
        w.maint
            .evidence
            .as_ref()
            .unwrap()
            .inject_run_write_error(false);
        delta_wait(|| !w.maint.local_held()).await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_decided_save_failure_recovers_the_same_passed_outcome() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("models.ini"), INI).unwrap();
        let (port, _) = synthetic_engine(d.path().join("models.ini"), false).await;
        let w = worker(d.path(), port);
        w.terminal_error.store(true, Ordering::Release);
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        delta_wait(|| w.file.load().job().get("terminalDecision").as_str() == Some("passed")).await;
        assert_eq!(w.job().get("phase").as_str(), Some("Finalizing"));
        assert!(w.job().get("finishedAt").is_nullish());
        assert!(w.maint.local_held());
        assert!(w.cancel().is_none());
        assert!(w.launch("synthetic", plan(32768.0, 120.0)).await.is_err());
        let (_copy, restarted) = delta_restart(&w, port, true).await;
        restarted.terminal_error.store(true, Ordering::Release);
        let recovery = restarted.spawn_recover().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(restarted.maint.local_held());
        assert_eq!(restarted.job().get("status").as_str(), Some("running"));
        restarted.terminal_error.store(false, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(
            restarted.maint.local_held(),
            "a passed decision waits for ledger hydration without erasing history"
        );
        assert_eq!(restarted.job().get("phase").as_str(), Some("Finalizing"));
        {
            use std::os::unix::fs::PermissionsExt;
            let path = restarted
                .maint
                .evidence
                .as_ref()
                .unwrap()
                .file()
                .parent()
                .unwrap()
                .join("run-state.json");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        restarted.interrupt_fence_wait();
        tokio::time::timeout(Duration::from_secs(4), recovery)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restarted.job().get("status").as_str(), Some("passed"));
        assert_eq!(
            restarted.file.load().job().get("status").as_str(),
            Some("passed")
        );
        assert!(restarted
            .file
            .load()
            .job()
            .get("pendingTombstone")
            .is_nullish());
        assert!(!restarted.maint.local_held());
        assert!(
            matches!(restarted.history("synthetic"), JValue::Arr(h) if !h.is_empty()),
            "valid passed history survives the read denial"
        );
        w.terminal_error.store(false, Ordering::Release);
        w.interrupt_fence_wait();
        delta_wait(|| !w.maint.local_held()).await;
        assert_eq!(w.job().get("status").as_str(), Some("passed"));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_transient_ledger_read_denial_rehydrates_without_recreating_the_worker() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("models.ini"), INI).unwrap();
        let (port, _) = synthetic_engine(d.path().join("models.ini"), false).await;
        let w = worker(d.path(), port);
        delta_pending(&w, "denied-read");
        let ledger = w
            .maint
            .evidence
            .as_ref()
            .unwrap()
            .file()
            .parent()
            .unwrap()
            .join("run-state.json");
        drop(w);
        std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o0)).unwrap();
        assert_eq!(
            std::fs::read(&ledger).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let w = worker(d.path(), port);
        let recovery = w.spawn_recover().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(w.maint.local_held());
        assert!(w.launch("synthetic", plan(32768.0, 120.0)).await.is_err());
        std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o600)).unwrap();
        w.interrupt_fence_wait();
        tokio::time::timeout(Duration::from_secs(4), recovery)
            .await
            .expect("repaired read hydrates the same registry")
            .unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
        assert_eq!(saved["models"]["synthetic"]["generation"], 2);
        assert_eq!(saved["models"]["synthetic"]["tombstoned"], true);
        assert!(!w.maint.local_held());
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        w.cancel();
        settled(&w).await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_aborted_recovery_can_retry_on_the_same_worker_without_releasing_early() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("models.ini"), INI).unwrap();
        let (port, _) = synthetic_engine(d.path().join("models.ini"), false).await;
        let w = worker(d.path(), port);
        delta_pending(&w, "aborted-recovery");
        let w = with_limits(&w, w.limits);
        let store = w.maint.evidence.as_ref().unwrap();
        store.inject_run_write_error(true);
        let recovery = w.spawn_recover().unwrap();
        delta_wait(|| !store.run_write_failures().is_empty()).await;
        let attempts = store.run_write_failures().len();
        assert!(w.cancel().is_none());
        delta_wait(|| store.run_write_failures().len() > attempts).await;
        recovery.abort();
        assert!(recovery.await.unwrap_err().is_cancelled());
        assert!(w.maint.local_held());
        assert!(w.launch("synthetic", plan(32768.0, 120.0)).await.is_err());
        store.inject_run_write_error(false);
        assert!(
            tokio::time::timeout(Duration::from_secs(4), w.recover())
                .await
                .unwrap()
                .is_some(),
            "aborted recovery releases its launch slot for retry"
        );
        assert!(!w.maint.local_held());
        assert_eq!(w.job().get("status").as_str(), Some("failed"));
        w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        w.cancel();
        settled(&w).await;
    }

    // SERIAL_WRITER_REGRESSIONS_BEGIN
    fn pause_commit(
        w: &Worker,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    ) {
        let (entered, ready) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        *w.file.commit_pause.lock().unwrap() = Some((entered, wait, done));
        (ready, release, finished)
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_aborted_recovery_commit_cannot_overwrite_a_new_run() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("models.ini"), INI).unwrap();
        let (port, seen) = synthetic_engine(d.path().join("models.ini"), true).await;
        let w = worker(d.path(), port);
        delta_pending(&w, "stalled-old-recovery");
        let w = with_limits(&w, w.limits);
        let (ready, release, done) = pause_commit(&w);
        let first = w.spawn_recover().unwrap();
        delta_wait(|| ready.try_recv().is_ok()).await;
        // The old writer has passed its permission check and written/fsynced its temporary file.
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let mut retry = w.spawn_recover().expect("the recovery slot is retryable");
        let overtook = tokio::time::timeout(Duration::from_millis(100), &mut retry)
            .await
            .is_ok();
        let mut release = Some(release);
        if !overtook {
            assert!(w.maint.local_held());
            assert!(w.launch("synthetic", plan(32768.0, 120.0)).await.is_err());
            release.take().unwrap().send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(4), &mut retry)
                .await
                .unwrap()
                .unwrap();
        }
        let next = w.launch("synthetic", plan(32768.0, 120.0)).await.unwrap();
        released(&seen, "POST /v1/chat/completions").await;
        // Freeze normal checkpoints after the successor is durable. The broken recovery writer
        // bypasses this lock; its stale rename must be observed before another checkpoint hides it.
        let writes = Arc::clone(&w.writes);
        let (entered, locked) = std::sync::mpsc::channel();
        let (unlock, wait) = std::sync::mpsc::channel();
        let checkpoint_block = tokio::task::spawn_blocking(move || {
            let _writer = writes.lock().unwrap();
            entered.send(()).unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(10));
        });
        delta_wait(|| locked.try_recv().is_ok()).await;
        if let Some(release) = release {
            release.send(()).unwrap();
        }
        delta_wait(|| done.try_recv().is_ok()).await;
        let latest = w.file.load().job().get("id").clone();
        let (_copy, restarted) = delta_restart(&w, port, false).await;
        unlock.send(()).unwrap();
        checkpoint_block.await.unwrap();
        w.cancel();
        settled(&w).await;
        assert_eq!(
            latest,
            *next.get("id"),
            "an old permitted recovery cannot overwrite the newest job"
        );
        assert!(!overtook, "retry waits for the unresolved blocking writer");
        restarted.recover().await;
        assert_eq!(restarted.job().get("id"), next.get("id"));
        assert_eq!(restarted.job().get("status").as_str(), Some("interrupted"));
    }
    #[tokio::test(flavor = "current_thread")]
    async fn a_slow_decided_save_retains_exactly_one_blocking_operation_and_snapshot() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("models.ini"), INI).unwrap();
        let w = worker(d.path(), 9);
        let w = with_limits(
            &w,
            Limits {
                finalize: Duration::from_millis(10),
                ..w.limits
            },
        );
        w.maint
            .evidence
            .as_ref()
            .unwrap()
            .admit_run("synthetic", "slow-decision")
            .unwrap();
        let mut state = State::empty();
        state.set_job(JValue::obj([
            ("id", JValue::Str("slow-decision".into())),
            ("model", JValue::Str("synthetic".into())),
            ("status", JValue::Str("passed".into())),
            ("finishedAt", JValue::Num(100.0)),
            ("pendingTombstone", JValue::Bool(true)),
            ("terminalDecision", JValue::Str("passed".into())),
        ]));
        w.file.save(&state).unwrap();
        let object = w.with(|i| {
            i.state = state.clone();
            i.active = true;
            i.decided = true;
            i.job_object
        });
        *w.local_fence.lock().unwrap() =
            super::super::gate::AdmissionFence::reserve(Arc::clone(&w.maint.local));
        let (ready, release, done) = pause_commit(&w);
        let worker = Arc::clone(&w);
        let task = tokio::spawn(async move { worker.complete_decision(state, object).await });
        delta_wait(|| ready.try_recv().is_ok()).await;
        tokio::time::pause();
        for _ in 0..32 {
            tokio::time::advance(Duration::from_secs(120)).await;
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
        }
        let outstanding = w.save_counts.outstanding.load(Ordering::Acquire);
        let snapshots = w.save_counts.snapshots.load(Ordering::Acquire);
        let peak = w.save_counts.peak.load(Ordering::Acquire);
        let dispatched = w.save_counts.dispatched.load(Ordering::Acquire);
        let held = w.maint.local_held();
        let status = w.job();
        tokio::time::resume();
        release.send(()).unwrap();
        delta_wait(|| done.try_recv().is_ok()).await;
        if outstanding != 1 || snapshots != 1 || peak != 1 || dispatched != 1 {
            task.abort();
            let _ = task.await;
            delta_wait(|| w.save_counts.outstanding.load(Ordering::Acquire) == 0).await;
        } else {
            tokio::time::timeout(Duration::from_secs(4), task)
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            outstanding, 1,
            "only one blocking operation survives repeated deadline observations"
        );
        assert_eq!(snapshots, 1, "only one blocking snapshot is retained");
        assert_eq!(peak, 1);
        assert_eq!(dispatched, 1);
        assert!(held);
        assert_eq!(status.get("phase").as_str(), Some("Finalizing"));
        assert!(status.get("finishedAt").is_nullish());
        assert_eq!(w.job().get("status").as_str(), Some("passed"));
        assert!(!w.maint.local_held());
        assert_eq!(w.file.load().job().get("status").as_str(), Some("passed"));
        assert_eq!(
            w.save_counts.dispatched.load(Ordering::Acquire),
            2,
            "one intent commit and one final commit"
        );
        assert_eq!(w.save_counts.snapshots.load(Ordering::Acquire), 0);
    }
    // SERIAL_WRITER_REGRESSIONS_END

    // SETTLEMENT_DELTA_REGRESSIONS_END

    // SETTLEMENT_REGRESSIONS_END
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
