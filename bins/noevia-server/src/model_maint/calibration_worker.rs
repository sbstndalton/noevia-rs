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
    LongPrompt, Progress, State, StateFile, Steps, Stop, NOT_RESTORED,
};
use super::{Hold, HoldError, Maint};
use js_json::{wtf16, JValue};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

const PAD: &str = "This is synthetic padding for a context allocation validation.\n";
/// Node's calibration hold has no limit of its own: it lasts until the run releases it.
const UNBOUNDED: Duration = Duration::MAX;
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
    /// Node's `objectId(state.job)`: changes whenever the job object is replaced (launch, reload).
    pub(super) job_object: u64,
    /// Node's `objectId(inflight)`: 0 when no step controller is live.
    pub(super) controller: u64,
    pub(super) next_object: u64,
}

/// The native worker: one per process, owning its state file, controller and job.
pub struct Worker {
    pub(super) maint: Arc<Maint>,
    file: StateFile,
    inner: Mutex<Inner>,
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
            inner: Mutex::new(Inner {
                state,
                cancel_requested: false,
                inflight: None,
                launching: false,
                job_object: 1,
                controller: 0,
                next_object: 2,
            }),
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

    /// `save()`: the whole state through a temp file and a rename.
    async fn save(&self) -> std::io::Result<()> {
        let state = self.with(|i| i.state.clone());
        let file = self.file.clone();
        tokio::task::spawn_blocking(move || file.save(&state))
            .await
            .map_err(|_| std::io::Error::other("save task"))?
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

    fn job_remove(&self, key: &str) {
        self.with(|i| {
            if let Some(job) = i.state.job_mut() {
                job.retain(|(k, _)| k != key);
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
        self.with(|i| public(i.state.job()))
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

    /// `recover()` at boot, then the state read again (the job is now interrupted).
    pub async fn recover(&self) -> Option<Option<bool>> {
        let outcome = self.maint.recover_calibration().await;
        let file = self.file.clone();
        if let Ok(state) = tokio::task::spawn_blocking(move || file.load()).await {
            self.with(|i| {
                i.state = state;
                i.job_object = i.next_object;
                i.next_object += 1;
            });
        }
        outcome
    }

    /// `compareAndAbort(token)`: the running job is cancelled; its public snapshot, taken before the
    /// controller is aborted. `None` when no job is running.
    pub fn cancel(&self) -> Option<JValue> {
        self.with(|i| {
            if i.state.job().get("status").as_str() != Some("running") {
                return None;
            }
            let job = public(i.state.job());
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
        let token = hold.token();
        Self::race(
            abort,
            router.request(method, path, body, token.as_deref(), timeout),
        )
        .await?
        .map_err(|_| Thrown::Other("fetch failed".into()))
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

    /// `unloadAll()`: every live model asked to unload, then polled until none is live.
    async fn unload_all(&self, hold: &Hold) -> Result<(), Thrown> {
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
                Duration::from_secs(120),
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
        self.unload_all(hold).await?;
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
                Duration::from_secs(120),
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
                Duration::from_secs(30),
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

    /// `streamLong(...)`: the streamed long prompt, read with [`LongPrompt`].
    async fn stream_long(
        &self,
        hold: &Hold,
        model: &str,
        content: &str,
        abort: &Abort,
        index: usize,
        budget_ms: f64,
    ) -> Result<Long, Thrown> {
        let Some(router) = self.maint.router.as_ref() else {
            return Ok(Long::Failed("The long prompt could not be sent.".into()));
        };
        let started = now_ms();
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
            let busy = i.launching || i.state.job().get("status").as_str() == Some("running");
            if !busy {
                i.launching = true;
            }
            busy
        });
        if busy {
            return Err(Refusal::Running);
        }
        let launched = match self.take_hold(model).await {
            Ok(hold) => self.start(hold, model, plan).await,
            Err(r) => Err(r),
        };
        self.with(|i| i.launching = false);
        launched
    }

    /// `maintenance.hold(...)` for a run of `model`.
    pub(super) async fn take_hold(&self, model: &str) -> Result<Hold, Refusal> {
        if self.maint.router.is_none() || self.maint.ini.is_none() {
            return Err(Refusal::Unavailable);
        }
        let reason = format!("Chat is paused while noevia calibrates {model}. It will be available again when calibration finishes or is cancelled.");
        self.maint
            .acquire(&reason, UNBOUNDED, false)
            .await
            .map_err(|e| Refusal::Hold(hold_facts(&e)))
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
        let worker = Arc::clone(self);
        tokio::spawn(async move { worker.run(hold).await });
        Ok(self.job())
    }

    /// `run(job, release)`.
    async fn run(self: Arc<Self>, hold: Hold) {
        let outcome = self.run_inner(&hold).await;
        let model = self.job_get("model").as_str().unwrap_or("").to_owned();
        let budget = self.job_get("promptBudgetSeconds");
        if let Err(e) = outcome {
            let cancelled = e == Thrown::Cancelled;
            self.job_set(
                "status",
                JValue::Str(if cancelled { "cancelled" } else { "failed" }.into()),
            );
            self.job_set(
                "phase",
                JValue::Str(if cancelled { "Cancelled" } else { "Failed" }.into()),
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
            if matches!(e, Thrown::Fatal(_)) {
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
        self.job_set("finishedAt", JValue::Num(now_ms()));
        self.job_remove("originalText");
        self.with(|i| i.cancel_requested = false);
        let _ = self.save().await;
        hold.release().await;
    }

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
                Duration::from_secs(120),
                None,
            )
            .await;
        // The raw reload Node sends (no reload-guard record or forget, as in Node's run()).
        true
    }

    /// The search and the final write; the history entry on a pass.
    async fn run_inner(self: &Arc<Self>, hold: &Hold) -> Result<Option<JValue>, Thrown> {
        self.unload_all(hold).await?;
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
        self.unload_all(hold).await?;
        let model = self.job_get("model").as_str().unwrap_or("").to_owned();
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
            .request(
                hold,
                reqwest::Method::GET,
                "/props",
                None,
                Duration::from_secs(8),
                None,
            )
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
        let entry = JValue::Obj(entry);
        self.with(|i| i.state.push_history(&model, entry.clone()));
        Ok(Some(entry))
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
fn hold_facts(e: &HoldError) -> JValue {
    // Node: a busy refusal is `busy()` (409, no message, no code); any other gate refusal is the
    // GateError itself. Anything else had no integer status there (null).
    let (status, message, code) = match e {
        HoldError::Busy => (JValue::Num(409.0), JValue::Null, JValue::Null),
        HoldError::Gate {
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
        HoldError::NeedsGate | HoldError::NodeUnavailable => {
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
        use axum::body::Body;
        use axum::http::{Request, Response};
        use http_body_util::BodyExt;
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
            let (status, log, ctx_of) = (Arc::clone(&status), Arc::clone(&log), ctx_of.clone());
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
                    ("POST", "/gate/v1/maintenance") => json(201, json!({"hold": "0123456789abcdef0123456789abcdef", "idle_ttl_ms": 60000})),
                    ("GET", "/gate/v1/status") => json(200, json!({"maintenance": {"held": false}})),
                    (_, p) if p.starts_with("/gate/") => json(200, json!({})),
                    ("GET", "/models") => {
                        let v = status.lock().unwrap().clone();
                        let failed = v == "failed";
                        let value = if failed { "unloaded" } else { v.as_str() };
                        json(200, json!({"data": [{"id": "synthetic", "status": {"value": value, "failed": failed}}]}))
                    }
                    ("POST", "/models/unload") => {
                        *status.lock().unwrap() = "unloaded".into();
                        json(200, json!({"success": true}))
                    }
                    ("POST", "/models/load") => {
                        *status.lock().unwrap() = if ctx_of() <= 16384.0 { "loaded" } else { "failed" }.into();
                        json(200, json!({"success": true}))
                    }
                    ("POST", "/tokenize") => json(200, json!({"tokens": vec![1; 650]})),
                    ("GET", "/props") => json(200, json!({"build_info": "b-synthetic"})),
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
        let env = [
            ("MODEL_MANAGER_KIND".to_owned(), "llamacpp".to_owned()),
            (
                "MODEL_MANAGER_BASE_URL".to_owned(),
                format!("http://127.0.0.1:{port}"),
            ),
            (
                "INFERENCE_GATE_ADMIN_TOKEN".to_owned(),
                "synthetic-admin".to_owned(),
            ),
            (
                "LLAMACPP_PRESET_PATH".to_owned(),
                dir.join("models.ini").display().to_string(),
            ),
            ("UI_DATA_DIR".to_owned(), dir.display().to_string()),
        ];
        let get = |k: &str| env.iter().find(|(e, _)| e == k).map(|(_, v)| v.clone());
        let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
        let sizing = Arc::new(crate::model_sizing::Sizing::from_lookup(
            engine.kind(),
            Arc::clone(&engine),
            &get,
        ));
        let maint = Arc::new(Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap());
        let limits = Limits {
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
            if job.get("status").as_str() != Some("running") {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the synthetic run did not finish: {:?}", w.job());
    }

    const INI: &str =
        "[*]\nn-gpu-layers = 99\n\n[synthetic]\nmodel = /models/synthetic.gguf\nctx-size = 4096\n";

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
        let busy = hold_facts(&HoldError::Busy);
        assert_eq!(
            wtf16::stringify(&busy).unwrap(),
            r#"{"status":409,"publicMessage":null,"code":null}"#
        );
        let gate = hold_facts(&HoldError::Gate {
            status: 503,
            code: "engine_unavailable".into(),
            message: "The local model engine is not answering. Try again shortly.".into(),
        });
        assert_eq!(
            wtf16::stringify(&gate).unwrap(),
            r#"{"status":503,"publicMessage":"The local model engine is not answering. Try again shortly.","code":"engine_unavailable"}"#
        );
        let odd = hold_facts(&HoldError::Gate {
            status: 500,
            code: "bad code!".into(),
            message: "x".into(),
        });
        assert_eq!(odd.get("code"), &JValue::Null);
        assert_eq!(
            hold_facts(&HoldError::NodeUnavailable).get("status"),
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
}
