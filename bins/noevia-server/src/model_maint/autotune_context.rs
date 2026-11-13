//! The auto-tune's PRIVATE calibrator (core `llamacpp-calibration.cjs` `createCalibrator` as
//! `llamacpp-manager.cjs` `contextFactory` builds it per stage): the context search a tune's
//! context stage runs (`start` / `completion` / `status`) and fillCheck's one fill-and-recall step
//! (`probe`). It implements [`autotune_runner::Context`].
//!
//! It differs from the public worker ([`super::calibration_worker::Worker`]) the way Node's private
//! calibrator differs from the public one: no state file (a save only reports the job through
//! [`Hooks::on_update`]), no hold of its own (the tune's hold covers it), profile writes through
//! [`Hooks::apply_unlocked`] (the runner's revision-guarded write) and [`Hooks::on_write`], another
//! client's model left to [`Hooks::foreign_held`] / [`Hooks::foreign_wait`] (the step is then
//! repeated), and `probe`. The search order, the ladder, the streamed long-prompt reading and the
//! messages are the public worker's ([`super::calibration`]: `bound_load`, `verify_long`,
//! `LongPrompt`, `over_budget_reason`); the step body follows Node's current `stepOnce`, which the
//! public worker predates (foreign waits, evidence, `fill`).
//!
//! Every engine, preset, clock and randomness call goes through [`Engine`], one method per Node
//! dependency, so a recorded Node trace replays call for call
//! (`tests/fixtures/private-calibrator`). Nothing in production builds this yet.

use super::calibration::{
    bound_load, grouped, over_budget_reason, plain, verify_long, Feed, Kind, LongOutcome,
    LongPrompt, Progress, Steps, Stop, ENGINE_TEXT_UNITS, NOT_RESTORED,
};
use super::calibration_worker::PUBLIC_KEYS;
use autotune_runner::{js, Abort, Call, Context, Fail, Hooks};
use js_json::{wtf16, JValue};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

const PAD: &str = "This is synthetic padding for a context allocation validation.\n";
const ANOTHER_CLIENT: &str = "Another client loaded a model during calibration. Stop Diary background jobs and other clients, then retry.";
const NOT_RELOADED: &str = " Reload failed or timed out. Previous preset file restored; check router health before retrying.";
/// The contextFactory's answer when a start found its stage stale.
pub const STALE: &str = "The context worker state changed or is unavailable.";
const ERROR_BODY_MS: u64 = 2000;

/// One streamed response (`stream(path, opts)`): its status, then its body read chunk by chunk.
pub trait Body: Send + 'static {
    fn ok(&self) -> bool;
    fn status(&self) -> f64;
    /// `response.body` is present.
    fn has_body(&self) -> bool;
    /// The next chunk (`None`: the body ended). Ends with an error once the step's abort is raised
    /// (the engine boundary owns that, as a fetch body does).
    fn read(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, Fail>> + Send;
}

/// What the private calibrator is given (the manager's `createCalibrator` deps for a private
/// stage), one method per Node dependency.
pub trait Engine: Send + Sync + 'static {
    type Body: Body;
    /// `rawModels(signal)`: `{ ok, status?, body: { data } }`.
    fn raw_models(&self, abort: Option<Abort>)
        -> impl Future<Output = Result<JValue, Fail>> + Send;
    /// `request(path, { method, body, signal }, timeoutMs)` (the covered request): `{ ok, status, body }`.
    fn request(
        &self,
        call: Call,
        abort: Option<Abort>,
    ) -> impl Future<Output = Result<JValue, Fail>> + Send;
    /// `stream(path, { method: 'POST', signal, body })`.
    fn stream(
        &self,
        path: &str,
        body: JValue,
        abort: Abort,
    ) -> impl Future<Output = Result<Self::Body, Fail>> + Send;
    /// `presets.get(model)`.
    fn presets_get(&self, model: &str) -> JValue;
    /// `presets.snapshot()`: `(text, revision)`.
    fn presets_snapshot(&self) -> (String, String);
    /// `presets.commit({ baseRevision, text })`.
    fn presets_commit(&self, candidate: JValue) -> impl Future<Output = Result<(), Fail>> + Send;
    /// `invalidateBaseline(tag)`.
    fn invalidate_baseline(&self, tag: &str) -> impl Future<Output = Result<bool, Fail>> + Send;
    /// `conservativeFor(model)`.
    fn conservative_for(&self, model: &str) -> impl Future<Output = Result<JValue, Fail>> + Send;
    /// `identityFor(model)` (the tune's identity).
    fn identity_for(&self, model: &str) -> impl Future<Output = Result<JValue, Fail>> + Send;
    /// The manager's `maintenance.held()` (the contextFactory's `current()`).
    fn held(&self) -> bool;
    /// `readMemory()`: MemAvailable in GiB.
    fn read_memory(&self) -> Option<f64>;
    fn now(&self) -> f64;
    /// The calibrator's `sleep(ms)`.
    fn sleep(&self, ms: f64) -> impl Future<Output = ()> + Send;
    /// A timer that is not `sleep`: the memory watch's `setInterval` and the error-body deadline.
    fn tick(&self, ms: f64) -> impl Future<Output = ()> + Send;
    /// `crypto.randomInt(min, max)` (the recall marker).
    fn random_int(&self, min: u32, max: u32) -> u32;
    /// `crypto.randomUUID()` (the job id).
    fn random_uuid(&self) -> String;
    /// `ladder(native)` (throws `autoconfig_impl` when the candidates cannot be read).
    fn ladder(&self, native: f64) -> Result<Vec<f64>, Fail> {
        Ok(super::calibration::ladder(native))
    }
}

/// `limits` in ms (Node's defaults; tests shorten them). `long` is not used by the step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub load: f64,
    pub smoke: f64,
    pub unload: f64,
    pub poll: f64,
    pub memory_poll: f64,
    pub recover: f64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            load: 600_000.0,
            smoke: 180_000.0,
            unload: 60_000.0,
            poll: 500.0,
            memory_poll: 1000.0,
            recover: 180_000.0,
        }
    }
}

impl Limits {
    /// `{ ...defaults, ...timeouts }` from the `timeouts` option object.
    pub fn from_timeouts(t: &JValue) -> Limits {
        let d = Limits::default();
        let pick = |k: &str, v: f64| t.get(k).as_num().unwrap_or(v);
        Limits {
            load: pick("load", d.load),
            smoke: pick("smoke", d.smoke),
            unload: pick("unload", d.unload),
            poll: pick("poll", d.poll),
            memory_poll: pick("memoryPoll", d.memory_poll),
            recover: pick("recover", d.recover),
        }
    }
}

/// Which job a step writes to: the calibrator's own (`state.job`, a context search) or a probe's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    State,
    Probe,
}

#[derive(Debug)]
struct Inner {
    /// `state.job` (`Null`: none yet).
    job: JValue,
    /// The probe's own job (never `state.job`, as in Node).
    probe: JValue,
    /// Node's `objectId(state.job)`.
    job_object: u64,
    cancel_requested: bool,
    inflight: Option<Abort>,
    /// Node's `objectId(inflight)`.
    controller: u64,
    next_object: u64,
    launching: bool,
    /// `evidenceOf`: a failed step's engine evidence by (slot, step index).
    evidence: Vec<(Slot, usize, JValue)>,
}

/// `launchToken()`'s inputs: a start's reads stay valid only while these are unchanged.
#[derive(Debug, Clone, PartialEq)]
struct Token {
    job: String,
    job_object: u64,
    cancel_requested: bool,
    controller: u64,
}

/// `listingFacts` / `preparationFacts` that can be compared (`rowToken`, `preparedToken`).
enum Facts {
    /// Stale, the backend was down, or an identity/ladder that is not ready.
    No,
    Row {
        row: JValue,
        key: String,
    },
}

/// `preparationFacts`: stale (no `preparedToken`), not ready (`identityReady: false`), or ready.
enum Prep {
    Stale,
    NotReady,
    Ready(Prepared),
}

struct Prepared {
    row: JValue,
    profile: JValue,
    conservative: JValue,
    revision: String,
    ladder_ready: bool,
    key: String,
}

/// How one step attempt ended (`stepOnce`'s body before its `catch`).
enum Attempt {
    Finish {
        status: &'static str,
        reason: Option<String>,
        cause: Option<&'static str>,
        evidence: Option<JValue>,
    },
    /// Another client had a model live; it was waited for, and the step is repeated.
    Repeat,
}

fn finish(status: &'static str, reason: Option<String>, cause: Option<&'static str>) -> Attempt {
    Attempt::Finish {
        status,
        reason,
        cause,
        evidence: None,
    }
}

fn failed_with(reason: &str, cause: &'static str, evidence: JValue) -> Attempt {
    Attempt::Finish {
        status: "failed",
        reason: Some(reason.to_string()),
        cause: Some(cause),
        evidence: Some(evidence),
    }
}

/// `streamLong(...)`'s answer.
enum Long {
    Passed {
        text: String,
        prompt_tokens: Option<f64>,
        prompt_per_second: Option<f64>,
        prompt_seconds: f64,
    },
    OverBudget {
        predicted: bool,
        eta_ms: f64,
    },
    Failed {
        error: String,
        /// `status` (undefined, null or the HTTP status).
        status: JValue,
        detail: Option<String>,
    },
}

fn cancelled() -> Fail {
    Fail {
        cancelled: true,
        ..Fail::new("cancelled")
    }
}

fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

fn status_of(m: &JValue) -> Option<&str> {
    m.get("status").get("value").as_str()
}

fn id_of(m: &JValue) -> Option<&str> {
    m.get("id").as_str()
}

fn live(m: &JValue, with_sleeping: bool) -> bool {
    match status_of(m) {
        Some("loaded" | "loading") => true,
        Some("sleeping") => with_sleeping,
        _ => false,
    }
}

/// `loadedOthers(models, model)`.
fn loaded_others(rows: &[JValue], model: Option<&str>) -> bool {
    rows.iter().any(|m| id_of(m) != model && live(m, false))
}

/// `String(v)` for a reply's error text (`applied.body?.error || fallback`).
fn error_text(v: &JValue, fallback: &str) -> String {
    if !v.truthy() {
        return fallback.to_string();
    }
    match v {
        JValue::Str(s) => s.clone(),
        other => js_json::to_js_string(other).unwrap_or_else(|_| fallback.to_string()),
    }
}

/// `engineText(body)`: what the engine said, at most 2048 UTF-16 units.
fn engine_text(body: &JValue) -> String {
    let text = match body {
        JValue::Undefined | JValue::Null => return String::new(),
        JValue::Str(s) => s.clone(),
        other => wtf16::stringify(other).unwrap_or_default(),
    };
    wtf16::slice(&text, ENGINE_TEXT_UNITS)
}

/// `identityValid(value)`.
fn identity_valid(v: &JValue) -> bool {
    let Some(items) = js::entries(v) else {
        return false;
    };
    let mut keys: Vec<&str> = items.iter().map(|(k, _)| k.as_str()).collect();
    keys.sort_unstable();
    if keys != ["arch", "artifact", "build", "hardware", "profile", "quant"] {
        return false;
    }
    let null_or_str = |k: &str| matches!(v.get(k), JValue::Null | JValue::Str(_));
    let hex64 =
        |s: &str| s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    ["arch", "quant", "hardware"].iter().all(|k| null_or_str(k))
        && v.get("artifact").as_str().is_some_and(|s| !s.is_empty())
        && v.get("profile").as_str().is_some_and(hex64)
        && matches!(
            v.get("build"),
            JValue::Null | JValue::Str(_) | JValue::Obj(_)
        )
}

/// `Number.isInteger(n)`.
fn is_integer(n: f64) -> bool {
    n.is_finite() && n.fract() == 0.0
}

fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Core<E: Engine, H: Hooks> {
    engine: E,
    hooks: H,
    /// fillCheck's calibrator: no `contextCurrent`, `foreignHeld` or `foreignWait`.
    fill: bool,
    floor_gib: f64,
    limits: Limits,
    /// The models.ini revision when the stage made this calibrator.
    revision: String,
    inner: Mutex<Inner>,
    /// The contextFactory's `controller` (a start's reads) and `departed` (a start answered stale).
    controller: Abort,
    departed: Abort,
    /// The run is over (`completion`).
    done: tokio::sync::watch::Sender<bool>,
}

/// The private calibrator for one stage. Cheap to clone (one shared core).
pub struct PrivateCalibrator<E: Engine, H: Hooks + 'static> {
    core: Arc<Core<E, H>>,
}

impl<E: Engine, H: Hooks + 'static> Clone for PrivateCalibrator<E, H> {
    fn clone(&self) -> Self {
        PrivateCalibrator {
            core: Arc::clone(&self.core),
        }
    }
}

impl<E: Engine, H: Hooks + 'static> PrivateCalibrator<E, H> {
    /// `contextFactory(hooks)`: reads the models.ini revision the stage starts from. `fill_only`
    /// is fillCheck's calibrator; `memory_floor_gib` is `autoconfig.memoryFloorGib || 2`.
    pub fn new(
        engine: E,
        hooks: H,
        fill_only: bool,
        memory_floor_gib: f64,
        limits: Limits,
    ) -> Self {
        let revision = engine.presets_snapshot().1;
        let (done, _) = tokio::sync::watch::channel(true);
        PrivateCalibrator {
            core: Arc::new(Core {
                engine,
                hooks,
                fill: fill_only,
                floor_gib: memory_floor_gib,
                limits,
                revision,
                inner: Mutex::new(Inner {
                    job: JValue::Null,
                    probe: JValue::Null,
                    job_object: 0,
                    cancel_requested: false,
                    inflight: None,
                    controller: 0,
                    next_object: 1,
                    launching: false,
                    evidence: Vec::new(),
                }),
                controller: Abort::default(),
                departed: Abort::default(),
                done,
            }),
        }
    }

    /// `cancellationFacts().job`: the public keys of the job, a JSON copy.
    pub fn job(&self) -> JValue {
        self.core.public_job()
    }
}

impl<E: Engine, H: Hooks + 'static> Context for PrivateCalibrator<E, H> {
    fn start(
        &self,
        model: &str,
        prompt_budget_seconds: f64,
    ) -> impl Future<Output = Result<JValue, Fail>> + Send {
        let core = Arc::clone(&self.core);
        let model = model.to_string();
        async move {
            let inner = tokio::spawn({
                let core = Arc::clone(&core);
                async move { core.start_inner(&model, prompt_budget_seconds).await }
            });
            tokio::select! {
                biased;
                r = inner => r.unwrap_or_else(|_| Ok(stale())),
                () = core.departed.wait() => Ok(stale()),
            }
        }
    }

    fn completion(&self) -> impl Future<Output = Result<(), Fail>> + Send {
        let mut done = self.core.done.subscribe();
        async move {
            let _ = done.wait_for(|d| *d).await;
            Ok(())
        }
    }

    fn status_job(&self, _model: &str) -> JValue {
        self.core.public_job()
    }

    fn abort(&self) {
        let core = &self.core;
        {
            let mut i = lock(&core.inner);
            if !matches!(i.job, JValue::Null) {
                i.cancel_requested = true;
                if let Some(a) = &i.inflight {
                    a.abort();
                }
            }
        }
        core.controller.abort();
        core.departed.abort();
    }

    fn probe(
        &self,
        model: &str,
        opts: JValue,
    ) -> impl Future<Output = Result<JValue, Fail>> + Send {
        let core = Arc::clone(&self.core);
        let model = model.to_string();
        async move { core.probe(&model, &opts).await }
    }

    fn cancel_probe(&self) {
        let mut i = lock(&self.core.inner);
        i.cancel_requested = true;
        if let Some(a) = &i.inflight {
            a.abort();
        }
    }
}

fn stale() -> JValue {
    JValue::obj([
        ("ok", JValue::Bool(false)),
        ("body", JValue::obj([("error", JValue::from(STALE))])),
    ])
}

/// The search's steps through one calibrator; keeps the step's own error (its flags) for `run`.
struct RunSteps<'a, E: Engine, H: Hooks> {
    core: &'a Arc<Core<E, H>>,
    last: Option<Fail>,
}

impl<E: Engine, H: Hooks + 'static> Steps for RunSteps<'_, E, H> {
    async fn step(&mut self, ctx: f64, kind: Kind) -> Result<bool, Stop> {
        match self.core.step(Slot::State, ctx, kind).await {
            Ok(passed) => Ok(passed),
            Err(e) => {
                let stop = if e.cancelled {
                    Stop::Cancelled
                } else {
                    Stop::Fatal(e.message.clone())
                };
                self.last = Some(e);
                Err(stop)
            }
        }
    }
}

impl<E: Engine, H: Hooks + 'static> RunSteps<'_, E, H> {
    fn fail(&mut self, s: Stop) -> Fail {
        self.last.take().unwrap_or_else(|| match s {
            Stop::Cancelled => cancelled(),
            Stop::Fatal(m) => Fail::fatal(m),
        })
    }
}

impl<E: Engine, H: Hooks + 'static> Core<E, H> {
    // ── state ────────────────────────────────────────────────────────────────

    fn inner(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    fn with_job<T>(&self, slot: Slot, f: impl FnOnce(&mut JValue) -> T) -> T {
        let mut i = self.inner();
        match slot {
            Slot::State => f(&mut i.job),
            Slot::Probe => f(&mut i.probe),
        }
    }

    fn job_get(&self, slot: Slot, key: &str) -> JValue {
        self.with_job(slot, |j| j.get(key).clone())
    }

    fn job_set(&self, slot: Slot, key: &str, value: JValue) {
        self.with_job(slot, |j| js::set(j, key, value));
    }

    fn with_step<T>(
        &self,
        slot: Slot,
        index: usize,
        f: impl FnOnce(&mut JValue) -> T,
    ) -> Option<T> {
        self.with_job(slot, |j| {
            js::get_mut(j, "steps")
                .and_then(js::items_mut)
                .and_then(|s| s.get_mut(index))
                .map(f)
        })
    }

    fn model(&self, slot: Slot) -> String {
        self.job_get(slot, "model")
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    /// `state.job?.model`.
    fn own_model(&self) -> Option<String> {
        self.inner().job.get("model").as_str().map(str::to_string)
    }

    fn cancel_requested(&self) -> bool {
        self.inner().cancel_requested
    }

    /// `save()`: no state file; the job (or null) is reported to the stage.
    fn save(&self) {
        let job = self.inner().job.clone();
        self.hooks.on_update(&job);
    }

    fn public_job(&self) -> JValue {
        let i = self.inner();
        let JValue::Obj(items) = &i.job else {
            return JValue::Null;
        };
        let picked: Vec<(String, JValue)> = PUBLIC_KEYS
            .iter()
            .filter_map(|k| {
                items
                    .iter()
                    .find(|(key, _)| key == k)
                    .map(|(key, v)| (key.clone(), v.clone()))
            })
            .collect();
        js::json_copy(&JValue::Obj(picked))
    }

    fn token(&self) -> Token {
        let i = self.inner();
        Token {
            job: wtf16::stringify(&i.job).unwrap_or_default(),
            job_object: i.job_object,
            cancel_requested: i.cancel_requested,
            controller: i.controller,
        }
    }

    /// The contextFactory's `current()`.
    fn current(&self) -> bool {
        !self.fill
            && self.hooks.context_current()
            && self.engine.held()
            && self.engine.presets_snapshot().1 == self.revision
    }

    // ── start ────────────────────────────────────────────────────────────────

    async fn listing(&self, abort: Option<Abort>) -> Result<Vec<JValue>, Fail> {
        let r = self.engine.raw_models(abort).await?;
        match r.get("body").get("data") {
            JValue::Arr(rows) if r.get("ok").truthy() => Ok(rows.clone()),
            _ => Err(Fail::new("The model server is not responding.")),
        }
    }

    /// `listingFacts(model, token, { signal })`.
    async fn listing_facts(&self, model: &str, token: &Token) -> Facts {
        let a = &self.controller;
        if a.aborted() || *token != self.token() {
            return Facts::No;
        }
        let Ok(models) = self.listing(Some(a.clone())).await else {
            return Facts::No;
        };
        if a.aborted() || *token != self.token() {
            return Facts::No;
        }
        let row = js::json_copy(
            models
                .iter()
                .find(|m| id_of(m) == Some(model))
                .unwrap_or(&JValue::Null),
        );
        // `system` follows from the row; the row is the token's content.
        let key = wtf16::stringify(&row).unwrap_or_default();
        Facts::Row { row, key }
    }

    /// `preparationFacts(model, token, rowToken, { signal })`.
    async fn preparation_facts(&self, model: &str, token: &Token, row_key: &str) -> Prep {
        let Facts::Row { row, key } = self.listing_facts(model, token).await else {
            return Prep::Stale;
        };
        if key != row_key {
            return Prep::Stale;
        }
        let profile = js::json_copy(&self.engine.presets_get(model));
        let revision = self.engine.presets_snapshot().1;
        let conservative = js::json_copy(
            &self
                .engine
                .conservative_for(model)
                .await
                .unwrap_or(JValue::Null),
        );
        let identity = js::json_copy(
            &self
                .engine
                .identity_for(model)
                .await
                .unwrap_or(JValue::Null),
        );
        if !identity_valid(&identity) {
            return Prep::NotReady;
        }
        if self.controller.aborted()
            || *token != self.token()
            || revision != self.engine.presets_snapshot().1
        {
            return Prep::Stale;
        }
        let ladder_ready = self.engine.ladder(0.0).is_ok();
        let key = wtf16::stringify(&JValue::Arr(vec![
            JValue::from(key),
            row.clone(),
            profile.clone(),
            conservative.clone(),
            identity,
            JValue::from(revision.as_str()),
            JValue::Bool(ladder_ready),
        ]))
        .unwrap_or_default();
        Prep::Ready(Prepared {
            row,
            profile,
            conservative,
            revision,
            ladder_ready,
            key,
        })
    }

    /// The contextFactory's `start(model, { promptBudgetSeconds })`.
    async fn start_inner(self: &Arc<Self>, model: &str, budget: f64) -> Result<JValue, Fail> {
        if !self.current() || !is_integer(budget) {
            return Ok(stale());
        }
        let token = self.token();
        let row = self.listing_facts(model, &token).await;
        let Facts::Row { key: row_key, .. } = row else {
            // `!current() || aborted || !row.rowToken` (current() is read first in Node).
            let _ = self.current();
            return Ok(stale());
        };
        if !self.current() || self.controller.aborted() {
            return Ok(stale());
        }
        let facts = self.preparation_facts(model, &token, &row_key).await;
        if !self.current() || self.controller.aborted() {
            return Ok(stale());
        }
        let Prep::Ready(facts) = facts else {
            return Ok(stale());
        };
        if facts.profile.get("exists") != &JValue::Bool(true)
            || facts.revision != self.revision
            || !facts.ladder_ready
        {
            return Ok(stale());
        }
        let or_zero = |n: f64| {
            if n.is_nan() || n == 0.0 {
                None
            } else {
                Some(n)
            }
        };
        let native = or_zero(js::number(facts.row.get("meta").get("n_ctx_train")))
            .or_else(|| or_zero(js::number(facts.conservative.get("native"))))
            .unwrap_or(0.0);
        match self
            .compare_and_launch(model, &token, &row_key, &facts.key, budget, native)
            .await?
        {
            Some(job) => Ok(JValue::obj([("ok", JValue::Bool(true)), ("body", job)])),
            None => Ok(stale()),
        }
    }

    /// `compareAndLaunch(...)` with `isCurrent: current`: `Some(job)` when it launched.
    async fn compare_and_launch(
        self: &Arc<Self>,
        model: &str,
        token: &Token,
        row_key: &str,
        prepared_key: &str,
        budget: f64,
        native: f64,
    ) -> Result<Option<JValue>, Fail> {
        let current = || !self.controller.aborted() && self.current();
        if !current()
            || self.inner().launching
            || self.inner().job.get("status").as_str() == Some("running")
            || self.controller.aborted()
            || *token != self.token()
        {
            return Ok(None);
        }
        if !is_integer(budget) || !(15.0..=1800.0).contains(&budget) || !native.is_finite() {
            return Err(Fail::new("invalid prepared launch"));
        }
        self.inner().launching = true;
        struct Launching<'a>(&'a Mutex<Inner>);
        impl Drop for Launching<'_> {
            fn drop(&mut self) {
                lock(self.0).launching = false;
            }
        }
        let _launching = Launching(&self.inner);
        for _ in 0..2 {
            // `before`, then (past the no-op hold) `after`.
            let key = match self.preparation_facts(model, token, row_key).await {
                Prep::NotReady => return Ok(None),
                Prep::Ready(p) if !p.ladder_ready => return Ok(None),
                Prep::Ready(p) => Some(p.key),
                Prep::Stale => None,
            };
            if !current() || key.as_deref() != Some(prepared_key) {
                return Ok(None);
            }
        }
        let id = self.engine.random_uuid();
        let started = self.engine.now();
        let guard = if self.engine.read_memory().is_none() {
            "unavailable"
        } else {
            "active"
        };
        let job = JValue::obj([
            ("id", JValue::from(id)),
            ("model", JValue::from(model)),
            ("promptBudgetSeconds", JValue::Num(budget)),
            ("native", JValue::Num(native)),
            ("base", JValue::Obj(Vec::new())),
            ("status", JValue::from("running")),
            ("phase", JValue::from("Preparing")),
            ("startedAt", JValue::Num(started)),
            ("steps", JValue::Arr(Vec::new())),
            ("memoryFloorGib", JValue::Num(self.floor_gib)),
            ("memoryGuard", JValue::from(guard)),
        ]);
        {
            let mut i = self.inner();
            i.cancel_requested = false;
            i.job = job;
            i.job_object = i.next_object;
            i.next_object += 1;
        }
        self.save();
        self.done.send_replace(false);
        let run = Arc::clone(self);
        tokio::spawn(async move {
            /// `completion` settles even if the run panicked.
            struct Done<'a>(&'a tokio::sync::watch::Sender<bool>);
            impl Drop for Done<'_> {
                fn drop(&mut self) {
                    self.0.send_replace(true);
                }
            }
            let _done = Done(&run.done);
            run.run().await;
        });
        Ok(Some(self.public_job()))
    }

    // ── engine helpers ───────────────────────────────────────────────────────

    async fn request(&self, call: Call, abort: Option<Abort>) -> Result<JValue, Fail> {
        self.engine.request(call, abort).await
    }

    fn foreign(&self) -> bool {
        !self.fill
    }

    /// `othersLive(model)`.
    async fn others_live(&self, model: Option<&str>) -> Result<bool, Fail> {
        match self.listing(None).await {
            Ok(rows) => Ok(loaded_others(&rows, model)),
            Err(e) if e.coverage_lost => Err(e),
            Err(_) => Ok(false),
        }
    }

    /// `unloadAll()`: repeated after a wait for another client.
    async fn unload_all(&self) -> Result<(), Fail> {
        loop {
            match self.unload_all_once().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if !self.foreign() || e.fatal {
                        return Err(e);
                    }
                    let own = self.own_model();
                    if !self.others_live(own.as_deref()).await? {
                        return Err(e);
                    }
                    self.hooks.foreign_wait(e).await?;
                }
            }
        }
    }

    async fn unload_all_once(&self) -> Result<(), Fail> {
        let models = self.listing(None).await?;
        let own = self.own_model();
        let own = own.as_deref();
        if self.foreign() {
            let live_ids: Vec<JValue> = models
                .iter()
                .filter(|m| id_of(m) != own && live(m, true))
                .map(|m| m.get("id").clone())
                .collect();
            let held = if live_ids.is_empty() {
                Vec::new()
            } else {
                self.hooks.foreign_held(JValue::Arr(models.clone())).await?
            };
            if !held.is_empty() {
                return Err(Fail::new("Another client has a model loaded."));
            }
        }
        let mut asked: Vec<String> = Vec::new();
        let mut gone: Vec<String> = Vec::new();
        for m in &models {
            if live(m, true) {
                let id = m.get("id").clone();
                if let Some(s) = id.as_str() {
                    asked.push(s.to_string());
                }
                let _ = self
                    .request(
                        Call::post(
                            "/models/unload",
                            JValue::obj([("model", id)]),
                            self.limits.unload as u64,
                        ),
                        None,
                    )
                    .await;
            }
        }
        let deadline = self.engine.now() + self.limits.unload;
        while self.engine.now() < deadline {
            let rows = self.listing(None).await?;
            if !rows.iter().any(|m| status_of(m) != Some("unloaded")) {
                return Ok(());
            }
            if self.foreign() {
                for m in &rows {
                    if status_of(m) == Some("unloaded") {
                        if let Some(id) = id_of(m) {
                            gone.push(id.to_string());
                        }
                    }
                }
                let again = rows.iter().any(|m| {
                    let id = id_of(m);
                    id != own
                        && live(m, false)
                        && (status_of(m) == Some("loading")
                            || id.is_some_and(|i| gone.iter().any(|g| g == i))
                            || !id.is_some_and(|i| asked.iter().any(|a| a == i)))
                });
                if again {
                    return Err(Fail::new(
                        "Another client loaded a model while models were unloading.",
                    ));
                }
            }
            self.engine.sleep(self.limits.poll).await;
        }
        Err(Fail::new("Models did not unload in time."))
    }

    /// `unload(model)` (its caller ignores the error).
    async fn unload(&self, model: &str) -> Result<(), Fail> {
        let _ = self
            .request(
                Call::post(
                    "/models/unload",
                    JValue::obj([("model", JValue::from(model))]),
                    self.limits.unload as u64,
                ),
                None,
            )
            .await;
        let deadline = self.engine.now() + self.limits.unload;
        while self.engine.now() < deadline {
            match self.listing(None).await {
                Ok(rows) => {
                    if rows
                        .iter()
                        .find(|m| id_of(m) == Some(model))
                        .and_then(status_of)
                        == Some("unloaded")
                    {
                        return Ok(());
                    }
                }
                Err(e) if e.coverage_lost => return Err(e),
                Err(_) => {}
            }
            self.engine.sleep(self.limits.poll).await;
        }
        Ok(())
    }

    /// `waitForBackend()`.
    async fn wait_for_backend(&self) -> Result<bool, Fail> {
        let deadline = self.engine.now() + self.limits.recover;
        while self.engine.now() < deadline {
            match self.listing(None).await {
                Ok(_) => return Ok(true),
                Err(e) if e.coverage_lost => return Err(e),
                Err(_) => {}
            }
            self.engine.sleep(2000.0).await;
        }
        Ok(false)
    }

    /// `loadAndWait(model)`.
    async fn load_and_wait(&self, model: &str) -> bool {
        let Ok(started) = self
            .request(
                Call::post(
                    "/models/load",
                    JValue::obj([("model", JValue::from(model))]),
                    120_000,
                ),
                None,
            )
            .await
        else {
            return false;
        };
        if !started.get("ok").truthy() {
            return false;
        }
        let deadline = self.engine.now() + self.limits.load;
        let mut polls = 0;
        while self.engine.now() <= deadline && polls < 2400 {
            let Ok(rows) = self.listing(None).await else {
                return false;
            };
            let row = rows.iter().find(|m| id_of(m) == Some(model));
            match row {
                Some(r) if status_of(r) == Some("loaded") => return true,
                None => return false,
                Some(r)
                    if r.get("status").get("failed").truthy()
                        || status_of(r) == Some("unloaded") =>
                {
                    return false
                }
                _ => {}
            }
            self.engine.sleep(self.limits.poll).await;
            polls += 1;
        }
        false
    }

    // ── memory ───────────────────────────────────────────────────────────────

    /// One sample into the step record: false (and the step aborted) under the floor.
    fn sample(&self, slot: Slot, index: usize, abort: &Abort) -> bool {
        let Some(value) = self.engine.read_memory() else {
            return true;
        };
        let rounded = js_round(value * 100.0) / 100.0;
        self.with_step(slot, index, |r| {
            let old = r.get("minAvailableGib").as_num().unwrap_or(f64::INFINITY);
            js::set(r, "minAvailableGib", JValue::Num(old.min(rounded)));
        });
        if value < self.floor_gib {
            self.with_step(slot, index, |r| {
                js::set(r, "memoryFloorHit", JValue::Bool(true))
            });
            abort.abort();
            return false;
        }
        true
    }

    fn floor_hit(&self, slot: Slot, index: usize) -> bool {
        self.with_step(slot, index, |r| r.get("memoryFloorHit").truthy())
            .unwrap_or(false)
    }

    fn oom(&self) -> String {
        format!("Available memory fell below {} GiB.", plain(self.floor_gib))
    }

    // ── one step ─────────────────────────────────────────────────────────────

    /// `step(job, ctx, kind)`: one measured step, repeated after a wait for another client.
    async fn step(self: &Arc<Self>, slot: Slot, ctx: f64, kind: Kind) -> Result<bool, Fail> {
        loop {
            let before = self.with_job(slot, |j| js::items(j.get("steps")).len());
            match self.step_once(slot, ctx, kind).await? {
                Some(passed) => return Ok(passed),
                None => {
                    self.with_job(slot, |j| {
                        if let Some(steps) = js::get_mut(j, "steps").and_then(js::items_mut) {
                            steps.truncate(before);
                        }
                    });
                    self.inner()
                        .evidence
                        .retain(|(s, i, _)| *s != slot || *i < before);
                    self.save();
                }
            }
        }
    }

    /// `finish(status, reason, cause, evidence)`.
    fn finish(
        &self,
        slot: Slot,
        index: usize,
        status: &str,
        reason: Option<&str>,
        cause: Option<&str>,
        evidence: Option<JValue>,
    ) -> bool {
        let now = self.engine.now();
        self.with_step(slot, index, |r| {
            js::set(r, "status", JValue::from(status));
            if let Some(reason) = reason.filter(|s| !s.is_empty()) {
                js::set(r, "reason", JValue::from(reason));
            }
            if let Some(cause) = cause {
                js::set(r, "cause", JValue::from(cause));
            }
            let started = js::number(r.get("startedAt"));
            js::set(
                r,
                "seconds",
                JValue::Num(js_round((now - started) / 1000.0)),
            );
        });
        if let Some(evidence) = evidence {
            if self.job_get(slot, "evidenceWanted") == JValue::Bool(true) {
                let mut i = self.inner();
                i.evidence.retain(|(s, n, _)| *s != slot || *n != index);
                i.evidence.push((slot, index, evidence));
            }
        }
        self.save();
        status == "passed"
    }

    /// `stepOnce(job, ctx, kind)`: `Some(passed)`, or `None` to repeat it.
    async fn step_once(
        self: &Arc<Self>,
        slot: Slot,
        ctx: f64,
        kind: Kind,
    ) -> Result<Option<bool>, Fail> {
        if self.cancel_requested() {
            return Err(cancelled());
        }
        let started = self.engine.now();
        let long = kind == Kind::Long;
        let index = self.with_job(slot, |j| {
            let record = JValue::obj([
                ("ctx", JValue::Num(ctx)),
                ("kind", JValue::from(if long { "long" } else { "load" })),
                ("status", JValue::from("running")),
                ("startedAt", JValue::Num(started)),
            ]);
            if !matches!(j.get("steps"), JValue::Arr(_)) {
                js::set(j, "steps", JValue::Arr(Vec::new()));
            }
            let steps = js::get_mut(j, "steps").and_then(js::items_mut);
            let index = steps.map(|s| {
                s.push(record);
                s.len() - 1
            });
            let phase = if long {
                format!("Long-prompt test at {} tokens", grouped(ctx))
            } else {
                format!("Loading at {} tokens", grouped(ctx))
            };
            js::set(j, "phase", JValue::from(phase));
            index
        });
        let Some(index) = index else {
            return Err(Fail::fatal("Calibration failed."));
        };
        self.save();
        // Memory freed by the previous unload can take a moment to return.
        let mut waited = 0;
        // `readMemory() != null && readMemory() < floor`: two reads, as Node makes them.
        while self.engine.read_memory().is_some()
            && self
                .engine
                .read_memory()
                .is_some_and(|v| v < self.floor_gib)
        {
            if waited >= 30 {
                let floor = plain(self.floor_gib);
                self.with_step(slot, index, |r| {
                    js::set(r, "status", JValue::from("failed"));
                    js::set(
                        r,
                        "reason",
                        JValue::from(format!(
                            "Available memory stayed below {floor} GiB with the model unloaded."
                        )),
                    );
                });
                self.save();
                return Err(Fail::fatal(format!("Available memory stayed below {floor} GiB even with the model unloaded. Free memory on this machine, then retry.")));
            }
            self.engine.sleep(1000.0).await;
            waited += 1;
        }
        let abort = Abort::default();
        {
            let mut i = self.inner();
            i.inflight = Some(abort.clone());
            i.controller = i.next_object;
            i.next_object += 1;
        }
        let guarded = self.engine.read_memory().is_some();
        let watcher = if guarded {
            self.sample(slot, index, &abort);
            let (core, a, every) = (Arc::clone(self), abort.clone(), self.limits.memory_poll);
            Some(tokio::spawn(async move {
                loop {
                    core.engine.tick(every).await;
                    core.sample(slot, index, &a);
                }
            }))
        } else {
            self.with_step(slot, index, |r| {
                js::set(r, "memoryGuard", JValue::from("unavailable"))
            });
            None
        };
        let attempt = self.attempt(slot, ctx, kind, index, &abort, guarded).await;
        let outcome = match attempt {
            Ok(Attempt::Repeat) => Ok(None),
            Ok(Attempt::Finish {
                status,
                reason,
                cause,
                evidence,
            }) => Ok(Some(self.finish(
                slot,
                index,
                status,
                reason.as_deref(),
                cause,
                evidence,
            ))),
            Err(e) => {
                if e.cancelled || self.cancel_requested() {
                    self.finish(slot, index, "skipped", Some("Cancelled."), None, None);
                    Err(cancelled())
                } else if e.fatal || e.coverage_lost {
                    self.finish(slot, index, "failed", Some(&e.message), None, None);
                    Err(e)
                } else if self.floor_hit(slot, index) {
                    let oom = self.oom();
                    Ok(Some(self.finish(
                        slot,
                        index,
                        "failed",
                        Some(&oom),
                        Some("oom"),
                        None,
                    )))
                } else {
                    match self.wait_for_backend().await {
                        Err(lost) => Err(lost),
                        Ok(false) => {
                            self.finish(
                                slot,
                                index,
                                "failed",
                                Some("The model server stopped responding."),
                                None,
                                None,
                            );
                            Err(Fail::fatal(
                                "The model server did not come back after a failed step.",
                            ))
                        }
                        // #1046: a crash; its text never reads as a time out.
                        Ok(true) => Ok(Some(self.finish(
                            slot,
                            index,
                            "failed",
                            Some("The model server failed during this step."),
                            Some("oom"),
                            Some(JValue::obj([
                                ("text", JValue::from(e.message.as_str())),
                                ("crash", JValue::Bool(true)),
                            ])),
                        ))),
                    }
                }
            }
        };
        if let Some(w) = watcher {
            w.abort();
        }
        self.inner().inflight = None;
        let model = self.model(slot);
        let _ = self.unload(&model).await;
        outcome
    }

    fn options(&self, slot: Slot, ctx: f64) -> JValue {
        let mut options = js::spread(&[&self.job_get(slot, "base")]);
        js::set(&mut options, "ctx-size", JValue::from(plain(ctx)));
        options
    }

    fn base_revision(&self, slot: Slot) -> JValue {
        let last = self.job_get(slot, "lastRevision");
        if last.truthy() {
            last
        } else {
            self.job_get(slot, "originalRevision")
        }
    }

    /// `repeatIfForeign()`.
    async fn repeat_if_foreign(&self, model: &str) -> Result<bool, Fail> {
        if !self.foreign() || !self.others_live(Some(model)).await? {
            return Ok(false);
        }
        self.hooks
            .foreign_wait(Fail::fatal(
                "Another client loaded a model during calibration.",
            ))
            .await?;
        Ok(true)
    }

    /// `stepOnce`'s `try` body.
    async fn attempt(
        &self,
        slot: Slot,
        ctx: f64,
        kind: Kind,
        index: usize,
        abort: &Abort,
        guarded: bool,
    ) -> Result<Attempt, Fail> {
        let check = |c: &Self| !guarded || c.sample(slot, index, abort);
        let model = self.model(slot);
        let oom = || finish("failed", Some(self.oom()), Some("oom"));
        self.unload_all().await?;
        let body = JValue::obj([
            ("model", JValue::from(model.as_str())),
            ("baseRevision", self.base_revision(slot)),
            ("options", self.options(slot, ctx)),
        ]);
        let applied = self.hooks.apply_unlocked(body).await?;
        if !applied.get("ok").truthy() {
            return Err(Fail::fatal(error_text(
                applied.get("body").get("error"),
                "Could not write the test profile.",
            )));
        }
        let revision = self.engine.presets_get(&model).get("revision").clone();
        self.job_set(slot, "lastRevision", revision.clone());
        self.hooks.on_write(revision.as_str().unwrap_or(""));
        self.save();
        let started = self
            .request(
                Call::post(
                    "/models/load",
                    JValue::obj([("model", JValue::from(model.as_str()))]),
                    120_000,
                ),
                Some(abort.clone()),
            )
            .await?;
        if !started.get("ok").truthy() {
            return Ok(failed_with(
                "The engine refused to load the model at this size.",
                "load",
                JValue::obj([
                    ("status", started.get("status").clone()),
                    ("text", JValue::from(engine_text(started.get("body")))),
                ]),
            ));
        }
        let deadline = self.engine.now() + self.limits.load;
        loop {
            if self.cancel_requested() {
                return Err(cancelled());
            }
            if !check(self) || self.floor_hit(slot, index) {
                return Ok(oom());
            }
            if self.engine.now() > deadline {
                return Ok(finish(
                    "failed",
                    Some("Loading did not finish in time.".into()),
                    Some("timeout"),
                ));
            }
            let rows = match self.listing(Some(abort.clone())).await {
                Ok(r) => r,
                Err(_) if self.floor_hit(slot, index) => Vec::new(),
                Err(e) => return Err(e),
            };
            if loaded_others(&rows, Some(&model)) {
                let error = Fail::fatal(ANOTHER_CLIENT);
                if !self.foreign() {
                    return Err(error);
                }
                self.hooks.foreign_wait(error).await?;
                return Ok(Attempt::Repeat);
            }
            let row = rows.iter().find(|m| id_of(m) == Some(model.as_str()));
            match row {
                Some(r) if status_of(r) == Some("loaded") => {
                    if !check(self) {
                        return Ok(oom());
                    }
                    break;
                }
                Some(r)
                    if !r.get("status").get("failed").truthy()
                        && status_of(r) != Some("unloaded") => {}
                _ => {
                    return Ok(failed_with(
                        "The model failed to load at this size.",
                        "load",
                        JValue::obj([
                            (
                                "exitCode",
                                row.map_or(JValue::Undefined, |r| {
                                    r.get("status").get("exit_code").clone()
                                }),
                            ),
                            ("text", JValue::from("")),
                        ]),
                    ))
                }
            }
            self.engine.sleep(self.limits.poll).await;
        }
        if kind == Kind::Load {
            let smoke = self
                .request(
                    Call::post(
                        "/v1/chat/completions",
                        JValue::obj([
                            ("model", JValue::from(model.as_str())),
                            ("temperature", JValue::Num(0.0)),
                            ("stream", JValue::Bool(false)),
                            (
                                "chat_template_kwargs",
                                JValue::obj([("enable_thinking", JValue::Bool(false))]),
                            ),
                            (
                                "messages",
                                JValue::Arr(vec![JValue::obj([
                                    ("role", JValue::from("user")),
                                    ("content", JValue::from("Reply with the single word OK.")),
                                ])]),
                            ),
                            ("max_tokens", JValue::Num(16.0)),
                        ]),
                        self.limits.smoke as u64,
                    ),
                    Some(abort.clone()),
                )
                .await?;
            if self.floor_hit(slot, index) {
                return Ok(oom());
            }
            let answered = smoke.get("ok").truthy()
                && matches!(smoke.get("body").get("choices"), JValue::Arr(_));
            if !answered && self.repeat_if_foreign(&model).await? {
                return Ok(Attempt::Repeat);
            }
            return Ok(if answered {
                finish("passed", None, None)
            } else {
                finish(
                    "failed",
                    Some("The loaded model did not answer a short request.".into()),
                    Some("load"),
                )
            });
        }
        // Near-capacity recall test against the per-slot window, streamed so progress can be watched.
        let base_parallel = self.job_get(slot, "base").get("parallel").clone();
        let parallel = if base_parallel.truthy() {
            base_parallel
        } else {
            self.engine
                .presets_get(&model)
                .get("options")
                .get("parallel")
                .clone()
        };
        let n = js::number(&parallel);
        let slots = if n.is_nan() || n == 0.0 {
            1.0
        } else {
            n.max(1.0)
        };
        let fill = self.job_get(slot, "fill").as_num();
        let target = match fill {
            Some(f) if is_integer(f) && f > 0.0 => f,
            _ => ((ctx / slots) * 0.9).floor() - 256.0,
        };
        let marker = format!(
            "CAL-{}-{}",
            self.engine.random_int(1000, 9999),
            self.engine.random_int(1000, 9999)
        );
        let per_line = self.tokens_per_pad_line(&model, abort).await;
        let repeats = (target / per_line).floor().max(1.0);
        let budget_s = js::number(&self.job_get(slot, "promptBudgetSeconds"));
        let want_detail = self.job_get(slot, "evidenceWanted") == JValue::Bool(true);
        let content = format!(
            "Remember the start marker: {marker}.\n{}\nReturn only the start marker.",
            PAD.repeat(repeats as usize)
        );
        let result = self
            .stream_long(
                &model,
                &content,
                abort,
                slot,
                index,
                budget_s * 1000.0,
                want_detail,
            )
            .await?;
        if self.floor_hit(slot, index) {
            return Ok(oom());
        }
        if matches!(result, Long::Failed { .. }) && self.repeat_if_foreign(&model).await? {
            return Ok(Attempt::Repeat);
        }
        match result {
            Long::OverBudget { predicted, eta_ms } => Ok(finish(
                "failed",
                Some(over_budget_reason(predicted, eta_ms, budget_s)),
                Some("time"),
            )),
            Long::Failed {
                error,
                status,
                detail,
            } => {
                let reason = if error.is_empty() {
                    "The long prompt failed.".to_string()
                } else {
                    error.clone()
                };
                Ok(failed_with(
                    &reason,
                    "oom",
                    JValue::obj([
                        ("status", status),
                        ("text", JValue::from(detail.unwrap_or(error))),
                    ]),
                ))
            }
            Long::Passed {
                text,
                prompt_tokens,
                prompt_per_second,
                prompt_seconds,
            } => {
                self.with_step(slot, index, |r| {
                    js::set(
                        r,
                        "promptTokens",
                        prompt_tokens.map_or(JValue::Null, JValue::Num),
                    );
                    if let Some(p) = prompt_per_second.filter(|p| *p != 0.0 && !p.is_nan()) {
                        js::set(r, "promptPerSecond", JValue::Num(js_round(p)));
                    }
                    if prompt_seconds != 0.0 && !prompt_seconds.is_nan() {
                        js::set(r, "promptSeconds", JValue::Num(prompt_seconds));
                    }
                });
                if !text.contains(&marker) {
                    return Ok(finish(
                        "failed",
                        Some(
                            "The model did not recall the marker from the start of the prompt."
                                .into(),
                        ),
                        Some("recall"),
                    ));
                }
                if prompt_tokens.is_some_and(|t| t != 0.0 && !t.is_nan() && t < target * 0.8) {
                    return Ok(finish(
                        "failed",
                        Some("The engine accepted fewer prompt tokens than requested.".into()),
                        Some("recall"),
                    ));
                }
                Ok(finish("passed", None, None))
            }
        }
    }

    /// `tokensPerPadLine(model, signal)`: any failure reads as 16 tokens a line.
    async fn tokens_per_pad_line(&self, model: &str, abort: &Abort) -> f64 {
        if let Ok(r) = self
            .request(
                Call::post(
                    "/tokenize",
                    JValue::obj([
                        ("model", JValue::from(model)),
                        ("content", JValue::from(PAD.repeat(50))),
                    ]),
                    30_000,
                ),
                Some(abort.clone()),
            )
            .await
        {
            let n = js::items(r.get("body").get("tokens")).len();
            if r.get("ok").truthy()
                && n > 0
                && matches!(r.get("body").get("tokens"), JValue::Arr(_))
            {
                return n as f64 / 50.0;
            }
        }
        16.0
    }

    /// `errorBody(response)`: at most 2048 units of what a rejected prompt's body said, read for
    /// at most 2 s.
    async fn error_body(&self, body: &mut E::Body) -> String {
        if !body.has_body() {
            return String::new();
        }
        let mut bytes: Vec<u8> = Vec::new();
        let limit = ENGINE_TEXT_UNITS * 4;
        let deadline = self.engine.tick(ERROR_BODY_MS as f64);
        tokio::pin!(deadline);
        while bytes.len() < limit {
            tokio::select! {
                biased;
                r = body.read() => match r {
                    Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
                    Ok(None) | Err(_) => break,
                },
                () = &mut deadline => break,
            }
        }
        bytes.truncate(limit);
        wtf16::slice(&String::from_utf8_lossy(&bytes), ENGINE_TEXT_UNITS)
    }

    fn show_progress(&self, slot: Slot, index: usize, p: &Progress) {
        self.with_step(slot, index, |r| {
            if let Some(v) = p.progress {
                js::set(r, "progress", JValue::Num(v));
            }
            match p.eta_seconds {
                Some(v) => js::set(r, "etaSeconds", JValue::Num(v)),
                None => js::del(r, "etaSeconds"),
            }
        });
    }

    /// `streamLong(model, content, controller, record, budgetMs, wantDetail)`.
    #[allow(clippy::too_many_arguments)]
    async fn stream_long(
        &self,
        model: &str,
        content: &str,
        abort: &Abort,
        slot: Slot,
        index: usize,
        budget_ms: f64,
        want_detail: bool,
    ) -> Result<Long, Fail> {
        let started = self.engine.now();
        let body = JValue::obj([
            ("model", JValue::from(model)),
            ("stream", JValue::Bool(true)),
            ("return_progress", JValue::Bool(true)),
            ("temperature", JValue::Num(0.0)),
            ("cache_prompt", JValue::Bool(false)),
            ("max_tokens", JValue::Num(64.0)),
            (
                "chat_template_kwargs",
                JValue::obj([("enable_thinking", JValue::Bool(false))]),
            ),
            (
                "messages",
                JValue::Arr(vec![JValue::obj([
                    ("role", JValue::from("user")),
                    ("content", JValue::from(content)),
                ])]),
            ),
        ]);
        let mut response = match self
            .engine
            .stream("/v1/chat/completions", body, abort.clone())
            .await
        {
            Ok(r) => r,
            Err(e) if abort.aborted() => return Err(e),
            Err(_) => {
                return Ok(Long::Failed {
                    error: "The long prompt could not be sent.".into(),
                    status: JValue::Undefined,
                    detail: None,
                })
            }
        };
        if !response.ok() || !response.has_body() {
            let status = response.status();
            let detail = if want_detail {
                Some(self.error_body(&mut response).await)
            } else {
                None
            };
            return Ok(Long::Failed {
                error: format!(
                    "The engine rejected the long prompt (HTTP {}).",
                    plain(status)
                ),
                status: JValue::Num(status),
                detail,
            });
        }
        let mut reader = LongPrompt::new(budget_ms, started);
        let mut progress = Progress::default();
        while let Some(chunk) = response.read().await? {
            let fed = reader.feed(&chunk, &mut || self.engine.now(), &mut progress);
            self.show_progress(slot, index, &progress);
            match fed {
                Feed::Continue { save } => {
                    if save {
                        self.save();
                    }
                }
                Feed::Stop(outcome) => {
                    abort.abort();
                    progress.eta_seconds = None;
                    self.show_progress(slot, index, &progress);
                    return Ok(match outcome {
                        LongOutcome::OverBudget { predicted, eta_ms } => {
                            Long::OverBudget { predicted, eta_ms }
                        }
                        LongOutcome::EngineError { status, detail } => Long::Failed {
                            error: "The engine reported an error during the long prompt.".into(),
                            status: status.map_or(JValue::Null, JValue::Num),
                            detail: Some(detail),
                        },
                        LongOutcome::Passed { .. } => Long::Failed {
                            error: "The long prompt failed.".into(),
                            status: JValue::Undefined,
                            detail: None,
                        },
                    });
                }
            }
        }
        let outcome = reader.finish(self.engine.now(), &mut progress);
        self.show_progress(slot, index, &progress);
        Ok(match outcome {
            LongOutcome::Passed {
                text,
                prompt_tokens,
                prompt_per_second,
                prompt_seconds,
            } => Long::Passed {
                text,
                prompt_tokens,
                prompt_per_second,
                prompt_seconds,
            },
            LongOutcome::OverBudget { predicted, eta_ms } => Long::OverBudget { predicted, eta_ms },
            LongOutcome::EngineError { status, detail } => Long::Failed {
                error: "The engine reported an error during the long prompt.".into(),
                status: status.map_or(JValue::Null, JValue::Num),
                detail: Some(detail),
            },
        })
    }

    // ── the run ──────────────────────────────────────────────────────────────

    fn result_set(&self, key: &str, value: JValue) {
        self.with_job(Slot::State, |j| {
            if let Some(r) = js::get_mut(j, "result") {
                js::set(r, key, value);
            }
        });
    }

    /// `run(job, release)`.
    async fn run(self: &Arc<Self>) {
        if let Err(e) = self.run_inner().await {
            self.run_failed(&e).await;
        }
        let now = self.engine.now();
        {
            let mut i = self.inner();
            js::set(&mut i.job, "finishedAt", JValue::Num(now));
            js::del(&mut i.job, "originalText");
            i.cancel_requested = false;
        }
        self.save();
    }

    async fn run_inner(self: &Arc<Self>) -> Result<(), Fail> {
        let s = Slot::State;
        self.unload_all().await?;
        let (text, revision) = self.engine.presets_snapshot();
        self.job_set(s, "originalText", JValue::from(text));
        self.job_set(s, "originalRevision", JValue::from(revision));
        self.save();
        let grid = self.engine.ladder(js::number(&self.job_get(s, "native")))?;
        let budget = js::number(&self.job_get(s, "promptBudgetSeconds"));
        let mut steps = RunSteps {
            core: self,
            last: None,
        };
        let load_ctx = match bound_load(&grid, &mut steps).await {
            Ok(c) => c,
            Err(stop) => return Err(steps.fail(stop)),
        };
        self.job_set(
            s,
            "result",
            JValue::obj([("loadCtx", JValue::Num(load_ctx))]),
        );
        let chosen = match verify_long(&grid, load_ctx, budget, &mut steps).await {
            Ok(c) => c,
            Err(stop) => return Err(steps.fail(stop)),
        };
        self.result_set("verifiedCtx", JValue::Num(chosen));
        self.unload_all().await?;
        let model = self.model(s);
        let body = JValue::obj([
            ("model", JValue::from(model.as_str())),
            ("baseRevision", self.base_revision(s)),
            ("options", self.options(s, chosen)),
        ]);
        let applied = self.hooks.apply_unlocked(body).await?;
        if !applied.get("ok").truthy() {
            return Err(Fail::fatal(error_text(
                applied.get("body").get("error"),
                "Could not save the calibrated profile.",
            )));
        }
        let revision = self.engine.presets_get(&model).get("revision").clone();
        self.job_set(s, "lastRevision", revision.clone());
        self.hooks.on_write(revision.as_str().unwrap_or(""));
        self.save();
        self.result_set("appliedCtx", JValue::Num(chosen));
        self.job_set(s, "phase", JValue::from("Loading the calibrated profile"));
        self.save();
        let loaded = self.load_and_wait(&model).await;
        self.result_set("loaded", JValue::Bool(loaded));
        if !loaded {
            return Err(Fail::fatal("The calibrated profile did not load."));
        }
        self.job_set(s, "status", JValue::from("passed"));
        self.job_set(s, "phase", JValue::from("Done"));
        let _props = self.request(Call::get("/props", 8000), None).await;
        // The history entry's slots (the history itself is never read from a private calibrator).
        let _slots = self.engine.presets_get(&model);
        Ok(())
    }

    /// `run`'s `catch`: the job marked, and the original profile put back unless the file moved.
    async fn run_failed(&self, e: &Fail) {
        let s = Slot::State;
        let default = if e.cancelled {
            "Calibration cancelled."
        } else {
            "Calibration failed."
        };
        self.job_set(
            s,
            "status",
            JValue::from(if e.cancelled { "cancelled" } else { "failed" }),
        );
        self.job_set(
            s,
            "phase",
            JValue::from(if e.cancelled { "Cancelled" } else { "Failed" }),
        );
        if !e.cancelled {
            let m = if e.message.is_empty() {
                "Calibration failed."
            } else {
                e.message.as_str()
            };
            self.job_set(s, "error", JValue::from(m));
        }
        let error_or_default = |c: &Self| {
            let error = c.job_get(s, "error");
            if error.truthy() {
                error_text(&error, default)
            } else {
                default.to_string()
            }
        };
        // No coverage signal and no recovery on the managed gate: only the error's own flag.
        if !e.coverage_lost {
            let original = self.job_get(s, "originalText");
            let last = self.job_get(s, "lastRevision");
            let restorable = !original.is_nullish() && last.truthy();
            if restorable {
                let restored = async {
                    let last = last.as_str().unwrap_or("").to_string();
                    if self.engine.presets_snapshot().1 != last {
                        return Ok::<bool, Fail>(false);
                    }
                    self.engine
                        .presets_commit(JValue::obj([
                            ("baseRevision", JValue::from(last.as_str())),
                            ("text", original.clone()),
                        ]))
                        .await?;
                    let now = self.engine.presets_snapshot().1;
                    self.hooks.on_write(&now);
                    if self.engine.invalidate_baseline("calibration").await? {
                        let _ = self
                            .request(Call::get("/models?reload=1", 120_000), None)
                            .await;
                    } else {
                        let error = format!("{}{NOT_RELOADED}", error_or_default(self));
                        self.job_set(s, "error", JValue::from(error));
                    }
                    Ok(true)
                }
                .await
                .unwrap_or(false);
                self.job_set(s, "restored", JValue::Bool(restored));
            }
        }
        if self.job_get(s, "restored") == JValue::Bool(false) {
            let error = format!("{}{NOT_RESTORED}", error_or_default(self));
            self.job_set(s, "error", JValue::from(error));
        }
    }

    // ── probe ────────────────────────────────────────────────────────────────

    /// `probe(model, { ctx, fill, base = {}, baseRevision, promptBudgetSeconds = 120, evidence })`.
    async fn probe(self: &Arc<Self>, model: &str, opts: &JValue) -> Result<JValue, Fail> {
        let or = |v: &JValue, d: JValue| {
            if matches!(v, JValue::Undefined) {
                d
            } else {
                v.clone()
            }
        };
        let base = or(opts.get("base"), JValue::Obj(Vec::new()));
        let budget = or(opts.get("promptBudgetSeconds"), JValue::Num(120.0));
        let ctx = js::number(opts.get("ctx"));
        {
            let mut i = self.inner();
            i.cancel_requested = false;
            i.evidence.retain(|(s, _, _)| *s != Slot::Probe);
            i.probe = JValue::obj([
                ("model", JValue::from(model)),
                ("base", base),
                ("originalRevision", opts.get("baseRevision").clone()),
                ("promptBudgetSeconds", budget),
                ("fill", opts.get("fill").clone()),
                ("steps", JValue::Arr(Vec::new())),
                (
                    "evidenceWanted",
                    JValue::Bool(opts.get("evidence") == &JValue::Bool(true)),
                ),
            ]);
        }
        let passed = self.step(Slot::Probe, ctx, Kind::Long).await?;
        let i = self.inner();
        let steps = js::items(i.probe.get("steps"));
        let last = steps.len().checked_sub(1);
        let record = last
            .and_then(|n| steps.get(n))
            .cloned()
            .unwrap_or(JValue::Obj(Vec::new()));
        let truthy_or_null = |v: &JValue| if v.truthy() { v.clone() } else { JValue::Null };
        let nullish_or_null = |v: &JValue| {
            if v.is_nullish() {
                JValue::Null
            } else {
                v.clone()
            }
        };
        let cause = if passed {
            JValue::Null
        } else if record.get("cause").truthy() {
            record.get("cause").clone()
        } else {
            JValue::from("load")
        };
        let mut out = vec![
            ("passed".to_string(), JValue::Bool(passed)),
            ("cause".to_string(), cause),
            ("reason".to_string(), truthy_or_null(record.get("reason"))),
            (
                "revision".to_string(),
                truthy_or_null(i.probe.get("lastRevision")),
            ),
        ];
        if !passed {
            if let Some((_, _, ev)) = i
                .evidence
                .iter()
                .find(|(s, n, _)| *s == Slot::Probe && Some(*n) == last)
            {
                out.push(("evidence".to_string(), ev.clone()));
            }
        }
        out.push((
            "promptSeconds".to_string(),
            nullish_or_null(record.get("promptSeconds")),
        ));
        out.push((
            "promptPerSecond".to_string(),
            nullish_or_null(record.get("promptPerSecond")),
        ));
        Ok(JValue::Obj(out))
    }
}
