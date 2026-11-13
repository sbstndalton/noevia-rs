//! What the runner is given (core `createFullAutotuner`'s injected dependencies), one method per
//! Node dependency so a recorded Node trace replays call for call. Production implements it over
//! the front's router client, models.ini store, maintenance gate and calibrator; tests replay.

use js_json::JValue;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A Node `Error` with the flags the runner reads (`cancelled`, `fatal`, `publicMessage`, ...).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fail {
    pub message: String,
    /// `{ cancelled: true }` (`cancelledError()`).
    pub cancelled: bool,
    /// `e.name === 'AbortError'`.
    pub abort: bool,
    pub fatal: bool,
    /// `publicMessage`: a sentence written for people (it passes every boundary unchanged).
    pub public_message: Option<String>,
    /// From the engine boundary: the engine's own words replaced by a fixed sentence (#1049).
    pub engine_failure: bool,
    /// The restore after a failed phase did not happen or could not be confirmed.
    pub unsafe_: bool,
    pub foreign_timeout: bool,
    pub idle_timeout: bool,
    /// `e.coverageLost`: the maintenance lease ended under the call.
    pub coverage_lost: bool,
    /// `e.code` (`autoconfig_impl`).
    pub code: Option<String>,
    /// `e.status`.
    pub status: Option<u16>,
}

impl Fail {
    /// `Error(message)`.
    pub fn new(message: impl Into<String>) -> Self {
        Fail {
            message: message.into(),
            ..Fail::default()
        }
    }
    /// `Object.assign(Error(message), { fatal: true })`.
    pub fn fatal(message: impl Into<String>) -> Self {
        Fail {
            fatal: true,
            ..Fail::new(message)
        }
    }
    /// `publicFail(message)`.
    pub fn public(message: impl Into<String>) -> Self {
        let message = message.into();
        Fail {
            public_message: Some(message.clone()),
            ..Fail::new(message)
        }
    }
    /// `cancelledError()`.
    pub fn cancelled() -> Self {
        Fail {
            cancelled: true,
            ..Fail::new("Cancelled")
        }
    }
    pub fn with_fatal(mut self) -> Self {
        self.fatal = true;
        self
    }
    /// `Settings changed outside auto-tune.` (fatal), the revision guard every write checks.
    pub fn outside_edit() -> Self {
        Fail::fatal("Settings changed outside auto-tune.")
    }
}

/// An abort signal the runner raises and a dependency watches (`AbortController`). A dependency
/// handed one must give up promptly once it is raised ([`Abort::wait`] resolves then), as a
/// `fetch` rejects at once on `controller.abort()`.
#[derive(Debug, Clone, Default)]
pub struct Abort(Arc<AbortInner>);

#[derive(Debug, Default)]
struct AbortInner {
    flag: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Abort {
    pub fn abort(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }
    pub fn aborted(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }
    /// Resolves once the signal is raised (at once if it already is).
    pub async fn wait(&self) {
        loop {
            let notified = self.0.notify.notified();
            if self.aborted() {
                return;
            }
            notified.await;
        }
    }
    /// The same signal (for identity checks).
    pub fn same(&self, other: &Abort) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// One router call (`request(path, { method, body, signal }, timeoutMs)`).
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub path: String,
    /// `GET` unless given.
    pub method: &'static str,
    /// The JSON body (the runner always sends JSON text of this value).
    pub body: Option<JValue>,
    pub timeout_ms: u64,
}

impl Call {
    pub fn get(path: impl Into<String>, timeout_ms: u64) -> Self {
        Call {
            path: path.into(),
            method: "GET",
            body: None,
            timeout_ms,
        }
    }
    pub fn post(path: impl Into<String>, body: JValue, timeout_ms: u64) -> Self {
        Call {
            path: path.into(),
            method: "POST",
            body: Some(body),
            timeout_ms,
        }
    }
}

/// The per-family facts the runner reads (sampling-recommendation.cjs `quirksOf`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Quirks {
    pub harmony: bool,
    pub reasoning_effort: Option<String>,
    pub quality_budget: Option<u32>,
}

/// What [`Deps::spawn`] is asked to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spawn {
    Run,
    Timer,
}

/// The hold `maintenance.holdWhenIdle` gives: chat stays paused until it is released.
pub trait Release: Send + Sync {
    /// `release.setReason(reason)`: what the paused chat shows.
    fn set_reason(&self, reason: &str);
    /// `release()`.
    fn release(&self);
}

/// The calls the auto-tune's private calibrator makes back into the runner (core
/// `contextFactory(hooks)`).
pub trait Hooks: Send + Sync {
    /// `contextCurrent()`: the run that made this calibrator is still the live one.
    fn context_current(&self) -> bool;
    /// `applyUnlocked(body)` guarded by the job's revision (`guardedApply`).
    fn apply_unlocked(&self, body: JValue) -> impl Future<Output = Result<JValue, Fail>> + Send;
    /// `foreignHeld(rows)`: live rows of another client.
    fn foreign_held(&self, rows: JValue) -> impl Future<Output = Result<Vec<JValue>, Fail>> + Send;
    /// `foreignWait(legacy)`: wait for another client, then the calibrator repeats its step.
    fn foreign_wait(&self, legacy: Fail) -> impl Future<Output = Result<(), Fail>> + Send;
    /// `onWrite(revision)`.
    fn on_write(&self, revision: &str);
    /// `onUpdate(c)`: the calibrator's job, mirrored into the context phase's steps.
    fn on_update(&self, c: &JValue);
}

/// The auto-tune's private calibrator (core llamacpp-calibration.cjs, built per stage).
pub trait Context: Send + Sync {
    /// `start(model, { confirmPause: true, promptBudgetSeconds })`: `{ ok, status, body }`.
    fn start(
        &self,
        model: &str,
        prompt_budget_seconds: f64,
    ) -> impl Future<Output = Result<JValue, Fail>> + Send;
    /// `await completion()`.
    fn completion(&self) -> impl Future<Output = Result<(), Fail>> + Send;
    /// `status(model).body.job`.
    fn status_job(&self, model: &str) -> JValue;
    /// `compareAndAbort(cancellationFacts().token)`.
    fn abort(&self);
    /// `probe(model, opts)` (one fill-and-recall step).
    fn probe(&self, model: &str, opts: JValue)
        -> impl Future<Output = Result<JValue, Fail>> + Send;
    /// `cancelProbe()`.
    fn cancel_probe(&self);
}

/// Everything `createFullAutotuner` is given, plus the module-level calls it makes (`require`d
/// helpers and the Rust decisions it reaches through the WASM module).
pub trait Deps: Send + Sync + 'static {
    type Release: Release + 'static;
    type Context: Context + 'static;

    // ── the engine (behind `engineBoundary`) ──
    /// `rawModels()`: `{ ok, status?, body: { data: [...] } }`.
    fn raw_models(&self) -> impl Future<Output = Result<JValue, Fail>> + Send;
    /// `request(path, opts, timeoutMs)`: `{ ok, status?, body }`; `abort` is the call's signal:
    /// once it is raised the call must end promptly with an error (Node's fetch rejects), so the
    /// memory guard and a cancel never wait for the request's own timeout.
    fn request(
        &self,
        call: Call,
        abort: Option<Abort>,
    ) -> impl Future<Output = Result<JValue, Fail>> + Send;

    // ── models.ini ──
    /// `presets.get(model)`: `{ model, revision, exists, options, defaults, fields }`.
    fn presets_get(&self, model: &str) -> JValue;
    /// `presets.snapshot()`: `(text, revision)`.
    fn presets_snapshot(&self) -> (String, String);
    /// `presets.commit(candidate, options)`.
    fn presets_commit(
        &self,
        candidate: JValue,
        options: JValue,
    ) -> impl Future<Output = Result<(), Fail>> + Send;
    /// The manager's `applyUnlocked(body, writeOptions)`: `{ ok, status, body }`.
    fn apply_unlocked(
        &self,
        body: JValue,
        write_options: JValue,
    ) -> impl Future<Output = Result<JValue, Fail>> + Send;
    /// The sha256 revision models.ini has after a write of `text` (`revisionOf`).
    fn revision_of(&self, text: &str) -> String;

    // ── model facts ──
    fn identity_for(
        &self,
        model: &str,
    ) -> impl Future<Output = Result<Option<JValue>, Fail>> + Send;
    /// `planFacts(model)`: `{ meta, modelBytes, mmprojBytes }` or null.
    fn plan_facts(&self, model: &str) -> impl Future<Output = Result<JValue, Fail>> + Send;
    /// `samplingFor(model)` (null when no recommendation, or none injected).
    fn sampling_for(&self, model: &str) -> impl Future<Output = Result<JValue, Fail>> + Send;
    /// `fileMissing(row)`.
    fn file_missing(&self, row: &JValue) -> bool;
    /// `isSystemModel(id, path)`.
    fn is_system_model(&self, id: &str, path: Option<&str>) -> bool;
    /// `isSidecarModel(id)`.
    fn is_sidecar_model(&self, id: &str) -> bool;
    /// `quirksOf(model)`.
    fn quirks(&self, model: &str) -> Quirks;

    // ── memory and sizing ──
    /// `readMemory()`: MemAvailable in GiB.
    fn read_memory(&self) -> Option<f64>;
    /// `budgetGib()` (throws `autoconfig_impl` when the budget cannot be read).
    fn budget_gib(&self) -> Result<f64, Fail>;
    /// llamacpp-calibration.cjs `ladder(native)` (throws `autoconfig_impl`).
    fn ladder(&self, native: u64) -> Result<Vec<u64>, Fail>;
    /// inference-budget.cjs `cacheRamLimits()`: `(capMib, hardMaxMib)` (the operator's variables).
    fn cache_ram_limits(&self) -> (f64, f64);
    /// `allowBelowQ5Kv()` (the operator's override variable).
    fn allow_below_q5(&self) -> bool;

    // ── maintenance ──
    /// `maintenance.holdWhenIdle(reason, { timeoutMs, signal })`. Once `abort` is raised the wait
    /// must end promptly with `Fail::cancelled()`; at the deadline, with `idle_timeout` set and the
    /// message `Timed out waiting for chat to become idle.`.
    fn hold_when_idle(
        &self,
        reason: &str,
        timeout_ms: u64,
        abort: Abort,
    ) -> impl Future<Output = Result<Self::Release, Fail>> + Send;
    /// `maintenance.assertCovered?.()`.
    fn assert_covered(&self) -> Result<(), Fail>;
    /// `maintenance.coverageSignal?.()?.aborted === true`.
    fn coverage_lost(&self) -> bool;
    /// `typeof maintenance.requireRecovery === 'function'`.
    fn recoverable(&self) -> bool;
    /// `maintenance.requireRecovery(key, repair, message)`: the gate keeps chat closed and later
    /// calls back [`crate::Runner::recover_trial`] for `key`.
    fn require_recovery(&self, key: &str, message: &str);
    /// `maintenance.recoverNow?.()`.
    fn recover_now(&self) -> impl Future<Output = ()> + Send;
    /// `invalidateBaseline('autotune')`: false when the reload guard did not acknowledge.
    fn invalidate_baseline(&self, tag: &str) -> impl Future<Output = bool> + Send;
    /// The injected `reloadRouter()`; `None` when the runner was given none (it then sends
    /// `/models?reload=1` itself).
    fn reload_router(&self) -> Option<impl Future<Output = Result<JValue, Fail>> + Send>;

    // ── calibrator, evidence, advice ──
    /// `contextFactory(hooks)`: a private calibrator for one stage. `hooks` are the runner's
    /// callbacks; `fill_only` marks fillCheck's calibrator (it is given only `applyUnlocked`,
    /// `onWrite` and a no-op `onUpdate`).
    fn context(&self, hooks: crate::runner::StageHooks<Self>, fill_only: bool) -> Self::Context
    where
        Self: Sized;
    /// `onResult({ model, result })`.
    fn on_result(
        &self,
        model: &str,
        result: &JValue,
    ) -> impl Future<Output = Result<(), Fail>> + Send;
    /// `loadAdvisor.judge({ cause, evidence })` (null: no advice).
    fn judge(&self, request: JValue) -> impl Future<Output = Result<JValue, Fail>> + Send;

    // ── the Rust decisions (dav-parse.wasm in Node) ──
    /// `planner.plan(request)` (`Err` where the module refuses or traps).
    fn plan(&self, request: &JValue) -> Result<JValue, Fail>;
    /// `contention(request)` (tune-contention).
    fn contention(&self, request: &JValue) -> Result<JValue, Fail>;
    /// `servingChecks.templateCaps(template)`.
    fn template_caps(&self, template: &str) -> Result<JValue, Fail>;
    /// `servingChecks.verdict(status, bodyText)`.
    fn serving_verdict(&self, status: f64, body_text: &str) -> Result<JValue, Fail>;
    /// `autotuneScan({ version: 1, ...request })`.
    fn scan(&self, request: JValue) -> Result<JValue, Fail>;
    /// `longProfiles.pairsOf(rows, presets)`.
    fn long_pairs(&self, rows: &JValue) -> JValue;
    /// `longProfiles.sectionFor(text, row, presets)`: `{ ok, id?, text?, reason? }`.
    fn long_section(&self, text: &str, row: &JValue) -> JValue;

    // ── time, ids, the state file ──
    fn now(&self) -> f64;
    /// The runner's `sleep(ms)` (between models, unload polls, foreign-client polls).
    fn sleep(&self, ms: u64) -> impl Future<Output = ()> + Send;
    /// A timer that is not the runner's `sleep`: chat's memory guard (`setInterval(guard, 500)`).
    /// It never advances the runner's clock and is not part of a trace.
    fn tick(&self, ms: u64) -> impl Future<Output = ()> + Send;
    /// Runs `task` in the background: the run a start or resume launches ([`Spawn::Run`]) or a
    /// deferred router re-read ([`Spawn::Timer`], Node's `scheduleReload` timer).
    fn spawn(&self, kind: Spawn, task: impl Future<Output = ()> + Send + 'static);
    fn random_uuid(&self) -> String;
    /// The atomic write of the state file (`save()`: tmp file + rename, mode 0600).
    fn save(&self, text: &str) -> Result<(), Fail>;
    /// Logged only (`console.error`), never stored.
    fn log(&self, _message: &str) {}
}
