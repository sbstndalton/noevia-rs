//! noevia#1318: the native benchmark runner (`bench.py`): the one job slot, the prompt suite
//! (`start`, `_run`, `measure`, `loaded_aliases`), `cancel` and the job state that
//! `GET /api/v1/benchmark/progress`, `POST /api/v1/benchmark/start|cancel` and the overview's
//! `job` render (`api._bench_job`).
//!
//! Three per-request observations stay with the Python child, which keeps the GPU sampler and
//! the log parser: the sampled VRAM (`_peak_vram`), whether anything else used the backend
//! (`_other_traffic`) and a cold load's time (`_read_load_ms`). They are asked of its private
//! `GET /api/v1/internal/bench-observe`; when the child cannot answer they take the values
//! Python's own best-effort paths give (`[]`, not contended, no load time).
//!
//! Every place `bench.py` reads the clock, this reads the injected [`Clock`] in the same order,
//! so the differential test can pin both clocks exactly as the Python oracle does.
mod sweep;
use crate::bench_runs::json_string;
use gguf::py_float_repr;
use model_files::json::Value as Json;
use rusqlite::{types::Value, Connection, OpenFlags};
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
pub use sweep::{
    py_dumps, sweep_request, PyInt, Restarter, Sleeper, SweepDocker, SweepRequest, Sweeps,
    ORPHAN_ATTEMPTS, ORPHAN_RETRY, SWEEP_TIMEOUT_S,
};

/// `bench.DEFAULT_MAX_TOKENS` / `bench.MAX_TOKENS_CEILING`.
pub const DEFAULT_MAX_TOKENS: i64 = 3072;
pub const MAX_TOKENS_CEILING: i64 = 8192;
pub const OBSERVE_PATH: &str = "/api/v1/internal/bench-observe";
pub const OBSERVE_CAP: usize = 64 * 1024;
const LINES_CAP: usize = 200;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(900);
/// sqlite3's default `timeout=5.0`.
const BUSY_WAIT: Duration = Duration::from_millis(5000);
/// Bounds Python does not have: one streamed answer, and the models list.
const ANSWER_CAP: usize = 64 * 1024 * 1024;
const MODELS_CAP: usize = 8 * 1024 * 1024;
/// sqlite3 refuses more bound parameters than this (`SQLITE_MAX_VARIABLE_NUMBER`).
const MAX_VARIABLES: usize = 32766;

/// `time.perf_counter` and `time.time` as `bench` reads them.
#[derive(Clone, Copy)]
pub struct Clock {
    pub perf_counter: fn() -> f64,
    pub time: fn() -> f64,
}

fn perf_counter() -> f64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
}

fn wall() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            perf_counter,
            time: wall,
        }
    }
}

pub type Task = Pin<Box<dyn Future<Output = ()> + Send>>;
/// How a worker is started: `tokio::spawn` in production; tests hold it back.
pub type Spawner = Arc<dyn Fn(Task) + Send + Sync>;

/// `bench.JobState`.
#[derive(Debug, Clone, PartialEq)]
pub struct JobState {
    pub run_id: i64,
    pub status: String,
    pub backend: String,
    pub total: i64,
    pub done: i64,
    pub current: String,
    pub started_at: f64,
    pub finished_at: f64,
    pub error: String,
    pub unit: String,
    pub lines: Vec<String>,
}

impl Default for JobState {
    fn default() -> Self {
        Self {
            run_id: 0,
            status: "idle".into(),
            backend: String::new(),
            total: 0,
            done: 0,
            current: String::new(),
            started_at: 0.0,
            finished_at: 0.0,
            error: String::new(),
            unit: "requests".into(),
            lines: Vec::new(),
        }
    }
}

impl JobState {
    pub fn active(&self) -> bool {
        matches!(self.status.as_str(), "starting" | "running" | "cancelling")
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn pct(&self) -> i64 {
        if self.total == 0 {
            return 0;
        }
        (100.0 * self.done as f64 / self.total as f64).round_ties_even() as i64
    }

    fn elapsed(&self, clock: &Clock) -> f64 {
        // Python reads the clock before testing `started_at`, idle or not.
        let end = if self.finished_at == 0.0 {
            (clock.time)()
        } else {
            self.finished_at
        };
        if self.started_at == 0.0 {
            return 0.0;
        }
        (end - self.started_at).max(0.0)
    }

    #[allow(clippy::cast_precision_loss)]
    fn eta(&self, clock: &Clock) -> f64 {
        if self.done <= 0 || !matches!(self.status.as_str(), "running" | "cancelling") {
            return 0.0;
        }
        let per = self.elapsed(clock) / self.done as f64;
        (per * (self.total - self.done) as f64).max(0.0)
    }

    fn log(&mut self, line: String) {
        self.lines.push(line);
        if self.lines.len() > LINES_CAP {
            let drop = self.lines.len() - LINES_CAP;
            self.lines.drain(..drop);
        }
    }

    /// `api._bench_job()`: the dataclass fields, then `pct`, `elapsed`, `eta`, `active` (the
    /// clock is read in that order, as the properties read it).
    pub fn render(&self, clock: &Clock) -> String {
        let pct = self.pct();
        let elapsed = self.elapsed(clock);
        let eta = self.eta(clock);
        let lines: Vec<String> = self.lines.iter().map(|l| json_string(l)).collect();
        format!(
            "{{\"run_id\":{},\"status\":{},\"backend\":{},\"total\":{},\"done\":{},\"current\":{},\
             \"started_at\":{},\"finished_at\":{},\"error\":{},\"unit\":{},\"lines\":[{}],\
             \"pct\":{pct},\"elapsed\":{},\"eta\":{},\"active\":{}}}",
            self.run_id,
            json_string(&self.status),
            json_string(&self.backend),
            self.total,
            self.done,
            json_string(&self.current),
            float(self.started_at),
            float(self.finished_at),
            json_string(&self.error),
            json_string(&self.unit),
            lines.join(","),
            float(elapsed),
            float(eta),
            self.active(),
        )
    }
}

/// A float as FastAPI writes it (pydantic writes non-finite floats as null).
fn float(x: f64) -> String {
    if x.is_finite() {
        py_float_repr(x)
    } else {
        "null".into()
    }
}

/// The checked `POST /api/v1/benchmark/start` body (`api.benchmark_start`'s conversions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRequest {
    pub backend: String,
    pub aliases: Vec<String>,
    pub prompt_ids: Vec<i64>,
    pub reps: i64,
    pub max_tokens: i64,
}

/// What the three per-request observations from the child came back as.
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    pub peak_vram_json: String,
    pub contended: bool,
    pub load_ms: Option<f64>,
}

impl Default for Observed {
    fn default() -> Self {
        Self {
            peak_vram_json: "[]".into(),
            contended: false,
            load_ms: None,
        }
    }
}

/// Asks the child for the observations of one request (`None`: the child could not answer).
pub type Observer =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Option<Observed>> + Send>> + Send + Sync>;

/// noevia#1943: holds a claimed job slot while `start` still runs inside the request future.
/// If that future is dropped (client gone, request budget spent) before the worker owns the
/// job, the slot goes back to idle and a `bench_run` row already written is finished as an
/// error, as `_abort_start` would. The row id is shared with the blocking insert under a lock,
/// so a drop while the insert is still running is seen by whichever side finishes second.
pub(crate) struct StartGuard<'a> {
    runner: &'a Runner,
    slot: Arc<Mutex<StartSlot>>,
    armed: bool,
}

#[derive(Default)]
pub(crate) struct StartSlot {
    abandoned: bool,
    run_id: i64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

const DROPPED_NOTE: &str = "could not start: the request was dropped";

impl<'a> StartGuard<'a> {
    pub(crate) fn new(runner: &'a Runner) -> Self {
        Self {
            runner,
            slot: Arc::new(Mutex::new(StartSlot::default())),
            armed: true,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }

    /// A `bench_run` insert for the blocking pool: records the id for the guard, or finishes
    /// the row at once when the request was dropped while the insert ran.
    pub(crate) fn track<F>(
        &self,
        insert: F,
    ) -> impl FnOnce() -> Result<i64, Raised> + Send + 'static
    where
        F: FnOnce(&Path) -> Result<i64, Raised> + Send + 'static,
    {
        let slot = self.slot.clone();
        let database = self.runner.database.clone();
        let clock = self.runner.clock;
        move || {
            let run_id = insert(&database)?;
            let mut slot = lock(&slot);
            if slot.abandoned {
                let _ = finish_run(&database, run_id, "error", (clock.time)(), DROPPED_NOTE);
            } else {
                slot.run_id = run_id;
            }
            Ok(run_id)
        }
    }
}

impl Drop for StartGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.runner.with(|s| *s = JobState::default());
        let run_id = {
            let mut slot = lock(&self.slot);
            slot.abandoned = true;
            std::mem::take(&mut slot.run_id)
        };
        if run_id != 0 {
            let database = self.runner.database.clone();
            let finished = (self.runner.clock.time)();
            let job = move || {
                let _ = finish_run(&database, run_id, "error", finished, DROPPED_NOTE);
            };
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => drop(handle.spawn_blocking(job)),
                Err(_) => job(),
            }
        }
    }
}

/// The backend's docker-network address, as `bench._resolve_endpoint` derives it.
pub type Resolver = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> + Send + Sync,
>;

pub struct Runner {
    state: Mutex<JobState>,
    cancel: AtomicBool,
    database: PathBuf,
    clock: Clock,
    http: reqwest::Client,
    spawn: Spawner,
    observe: Observer,
    resolve: Resolver,
    sweeps: Option<Sweeps>,
}

/// A Python exception escaping `_run` / `start`: `"{type}: {message}"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raised(pub String);

fn raised(kind: &str, message: impl std::fmt::Display) -> Raised {
    Raised(format!("{kind}: {message}"))
}

impl Runner {
    pub fn new(
        data: &Path,
        clock: Clock,
        spawn: Spawner,
        observe: Observer,
        resolve: Resolver,
    ) -> Result<Arc<Self>, &'static str> {
        Self::with_sweeps(data, clock, spawn, observe, resolve, None)
    }

    /// The runner owning the job slot for prompt runs and llama-bench sweeps alike.
    pub fn with_sweeps(
        data: &Path,
        clock: Clock,
        spawn: Spawner,
        observe: Observer,
        resolve: Resolver,
        sweeps: Option<Sweeps>,
    ) -> Result<Arc<Self>, &'static str> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|_| "benchmark client could not be built")?;
        Ok(Arc::new(Self {
            state: Mutex::new(JobState::default()),
            cancel: AtomicBool::new(false),
            database: data.join("model_loader.db"),
            clock,
            http,
            spawn,
            observe,
            resolve,
            sweeps,
        }))
    }

    fn with<T>(&self, f: impl FnOnce(&mut JobState) -> T) -> T {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }

    /// `_bench_job()`.
    pub fn job(&self) -> String {
        self.with(|s| s.render(&self.clock))
    }

    pub fn clock(&self) -> Clock {
        self.clock
    }

    pub fn snapshot(&self) -> JobState {
        self.with(|s| s.clone())
    }

    /// `bench.cancel()`.
    pub fn cancel(&self) -> bool {
        let asked = self.with(|s| {
            if !s.active() {
                return false;
            }
            s.status = "cancelling".into();
            s.log("cancel requested — finishing the request in flight".into());
            true
        });
        if asked {
            self.cancel.store(true, Ordering::SeqCst);
        }
        asked
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// `bench.start(...)`: `Ok(Ok(()))` started, `Ok(Err(msg))` the 409 refusal, `Err` raised.
    pub async fn start(self: &Arc<Self>, req: StartRequest) -> Result<Result<(), String>, Raised> {
        let aliases: Vec<String> = req.aliases.into_iter().filter(|a| !a.is_empty()).collect();
        if crate::section_writes::cfg::strip(&req.backend).is_empty() {
            return Ok(Err("no backend selected".into()));
        }
        if aliases.is_empty() {
            return Ok(Err("no models selected".into()));
        }
        let database = self.database.clone();
        let ids = req.prompt_ids.clone();
        let prompts = blocking(move || prompts_by_ids(&database, &ids)).await?;
        if prompts.is_empty() {
            return Ok(Err("no prompts selected".into()));
        }
        let reps = (if req.reps == 0 { 1 } else { req.reps }).clamp(1, 10);
        let max_tokens = (if req.max_tokens == 0 {
            DEFAULT_MAX_TOKENS
        } else {
            req.max_tokens
        })
        .clamp(16, MAX_TOKENS_CEILING);
        let started = (self.clock.time)();
        let claimed = self.with(|s| {
            if s.active() {
                return false;
            }
            // noevia#1948: reset the flag inside the claim, not after it (Python clears
            // `_CANCEL` after resolving), so a cancel arriving while "starting" is kept.
            self.cancel.store(false, Ordering::SeqCst);
            #[allow(clippy::cast_possible_wrap)]
            let total = (aliases.len() as i64)
                .saturating_mul(prompts.len() as i64)
                .saturating_mul(reps);
            *s = JobState {
                status: "starting".into(),
                backend: req.backend.clone(),
                total,
                started_at: started,
                ..JobState::default()
            };
            true
        });
        if !claimed {
            return Ok(Err("a benchmark is already running".into()));
        }
        // noevia#1943: from here until the worker exists, a dropped request must release the slot.
        let mut guard = StartGuard::new(self);
        let base_url = match (self.resolve)(req.backend.clone()).await {
            Ok(url) => url,
            Err(e) => {
                guard.disarm();
                self.with(|s| *s = JobState::default());
                return Ok(Err(e));
            }
        };
        let backend = req.backend.clone();
        let insert =
            guard.track(move |database| create_run(database, &backend, reps, max_tokens, started));
        let run_id = match blocking(insert).await {
            Ok(id) => id,
            Err(e) => {
                guard.disarm();
                return Ok(Err(self.abort_start(0, &e).await));
            }
        };
        self.with(|s| {
            s.run_id = run_id;
            // A cancel during "starting" left "cancelling"; keep it so the worker unwinds.
            if s.status == "starting" {
                s.status = "running".into();
            }
        });
        guard.disarm();
        let me = self.clone();
        let backend = req.backend;
        (self.spawn)(Box::pin(async move {
            me.run(
                run_id, backend, base_url, aliases, prompts, reps, max_tokens,
            )
            .await;
        }));
        Ok(Ok(()))
    }

    /// `bench._abort_start`.
    async fn abort_start(&self, run_id: i64, e: &Raised) -> String {
        self.with(|s| *s = JobState::default());
        let message =
            e.0.split_once(": ")
                .map_or(e.0.as_str(), |(_, m)| m)
                .to_owned();
        if run_id != 0 {
            let database = self.database.clone();
            let note = format!("could not start: {message}");
            let finished = (self.clock.time)();
            let _ = blocking(move || finish_run(&database, run_id, "error", finished, &note)).await;
        }
        format!("could not start the benchmark: {message}")
    }

    /// `bench._run`.
    #[allow(clippy::too_many_arguments)]
    async fn run(
        self: Arc<Self>,
        run_id: i64,
        backend: String,
        base_url: String,
        aliases: Vec<String>,
        prompts: Vec<(String, String)>,
        reps: i64,
        max_tokens: i64,
    ) {
        let mut status = "done".to_owned();
        let mut err = String::new();
        if let Err(Raised(e)) = self
            .suite(
                run_id,
                &backend,
                &base_url,
                &aliases,
                &prompts,
                reps,
                max_tokens,
                &mut status,
            )
            .await
        {
            status = "error".into();
            err = e;
            let line = format!("failed: {err}");
            self.with(|s| s.log(line));
        }
        if self.cancelled() && status != "error" {
            status = "cancelled".into();
        }
        let finished = (self.clock.time)();
        let database = self.database.clone();
        let (st, note) = (status.clone(), err.clone());
        let _ = blocking(move || finish_run(&database, run_id, &st, finished, &note)).await;
        self.with(|s| {
            s.status = status;
            s.finished_at = finished;
            s.error = err;
            s.current = String::new();
        });
        self.cancel.store(false, Ordering::SeqCst);
    }

    #[allow(clippy::too_many_arguments)]
    async fn suite(
        &self,
        run_id: i64,
        backend: &str,
        base_url: &str,
        aliases: &[String],
        prompts: &[(String, String)],
        reps: i64,
        max_tokens: i64,
        status: &mut String,
    ) -> Result<(), Raised> {
        for alias in aliases {
            if self.cancelled() {
                *status = "cancelled".into();
                break;
            }
            let database = self.database.clone();
            let a = alias.clone();
            let variant_id = blocking(move || add_variant(&database, run_id, &a)).await?;
            let was_loaded = loaded_aliases(&self.http, base_url)
                .await?
                .iter()
                .any(|l| l == alias);
            let line = format!(
                "{alias}: starting{}",
                if was_loaded {
                    " (already resident)"
                } else {
                    ""
                }
            );
            self.with(|s| s.log(line));
            let mut first_request = true;
            for (prompt_name, prompt_body) in prompts {
                for rep in 1..=reps {
                    if self.cancelled() {
                        *status = "cancelled".into();
                        break;
                    }
                    let cold = first_request && !was_loaded;
                    first_request = false;
                    let current = format!("{alias} · {prompt_name} · rep {rep}/{reps}");
                    self.with(|s| s.current = current);
                    let t_req = (self.clock.time)();
                    let m = measure(
                        &self.http,
                        &self.clock,
                        base_url,
                        alias,
                        prompt_body,
                        max_tokens,
                    )
                    .await?;
                    let query =
                        observe_query(backend, alias, aliases, t_req, cold && m.err.is_empty());
                    let seen = (self.observe)(query).await.unwrap_or_default();
                    let database = self.database.clone();
                    let row = ResultRow {
                        variant_id,
                        prompt_name: prompt_name.clone(),
                        rep,
                        cold,
                        m: m.clone(),
                        peak_vram_json: seen.peak_vram_json.clone(),
                        contended: seen.contended,
                    };
                    blocking(move || add_result(&database, &row)).await?;
                    self.with(|s| s.done += 1);
                    if !m.err.is_empty() {
                        let line = format!("{alias} · {prompt_name} rep {rep}: {}", m.err);
                        self.with(|s| s.log(line));
                    } else if cold {
                        let database = self.database.clone();
                        let load = seen.load_ms;
                        blocking(move || set_load_ms(&database, variant_id, load)).await?;
                    }
                }
                if self.cancelled() {
                    break;
                }
            }
            if !self.cancelled() {
                let line = format!("{alias}: done");
                self.with(|s| s.log(line));
            }
        }
        Ok(())
    }
}

/// The child's observation query: backend, alias, the run's aliases (the set `_other_traffic`
/// excludes), the request's start time, and whether a cold load time is wanted.
pub fn observe_query(
    backend: &str,
    alias: &str,
    aliases: &[String],
    since: f64,
    cold: bool,
) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    q.append_pair("backend", backend);
    q.append_pair("alias", alias);
    for a in aliases {
        q.append_pair("aliases", a);
    }
    q.append_pair("since", &py_float_repr(since));
    q.append_pair("cold", if cold { "1" } else { "0" });
    q.finish()
}

/// The child's `{"peak_vram_json": str, "contended": bool, "load_ms": float|null}`.
pub fn parse_observed(bytes: &[u8]) -> Option<Observed> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    Some(Observed {
        peak_vram_json: v.get("peak_vram_json")?.as_str()?.to_owned(),
        contended: v.get("contended")?.as_bool()?,
        load_ms: match v.get("load_ms")? {
            serde_json::Value::Null => None,
            n => Some(n.as_f64()?),
        },
    })
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, Raised> + Send + 'static,
) -> Result<T, Raised> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| raised("RuntimeError", "worker failed"))?
}

// ------------------------------------------------------------------ store (db.bench_*)

fn open(database: &Path) -> Result<Connection, Raised> {
    let db = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sql)?;
    db.set_limit(
        rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH,
        crate::sqlite_read::SQL_CAP,
    )
    .map_err(sql)?;
    db.busy_timeout(BUSY_WAIT).map_err(sql)?;
    Ok(db)
}

/// sqlite3's exception for a store error (the message is SQLite's own).
fn sql(e: rusqlite::Error) -> Raised {
    crate::sqlite_read::log_once("bench_runner", &e);
    let kind = match &e {
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, rusqlite::ErrorCode::ConstraintViolation) =>
        {
            "IntegrityError"
        }
        _ => "OperationalError",
    };
    let message = match &e {
        rusqlite::Error::SqliteFailure(_, Some(m)) => m.clone(),
        other => other.to_string(),
    };
    raised(kind, message)
}

/// `db.prompts_by_ids(ids)`: `(name, body)` in name order.
fn prompts_by_ids(database: &Path, ids: &[i64]) -> Result<Vec<(String, String)>, Raised> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if ids.len() > MAX_VARIABLES {
        return Err(raised("OperationalError", "too many SQL variables"));
    }
    let db = open(database)?;
    let marks = vec!["?"; ids.len()].join(",");
    let mut statement = db
        .prepare(&format!(
            "SELECT * FROM prompts WHERE id IN ({marks}) ORDER BY name"
        ))
        .map_err(sql)?;
    let names: Vec<String> = statement
        .column_names()
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let name_at = names.iter().position(|n| n == "name");
    let body_at = names.iter().position(|n| n == "body");
    let (Some(name_at), Some(body_at)) = (name_at, body_at) else {
        return Err(raised("IndexError", "No item with that key"));
    };
    let mut rows = statement
        .query(rusqlite::params_from_iter(ids.iter()))
        .map_err(sql)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(sql)? {
        let text = |i: usize| -> Result<String, Raised> {
            match row.get_ref(i).map_err(sql)? {
                rusqlite::types::ValueRef::Text(t) => std::str::from_utf8(t)
                    .map(str::to_owned)
                    .map_err(|_| raised("OperationalError", "Could not decode to UTF-8 column")),
                _ => Err(raised("TypeError", "prompt text is not a string")),
            }
        };
        out.push((text(name_at)?, text(body_at)?));
    }
    Ok(out)
}

fn create_run(
    database: &Path,
    backend: &str,
    reps: i64,
    max_tokens: i64,
    started: f64,
) -> Result<i64, Raised> {
    let db = open(database)?;
    db.execute(
        "INSERT INTO bench_run(backend, status, started_at, reps, max_tokens) \
         VALUES(?, 'running', ?, ?, ?)",
        rusqlite::params![backend, started, reps, max_tokens],
    )
    .map_err(sql)?;
    Ok(db.last_insert_rowid())
}

fn finish_run(
    database: &Path,
    run_id: i64,
    status: &str,
    finished: f64,
    note: &str,
) -> Result<(), Raised> {
    let db = open(database)?;
    db.execute(
        "UPDATE bench_run SET status = ?, finished_at = ?, note = ? WHERE id = ?",
        rusqlite::params![status, finished, note, run_id],
    )
    .map_err(sql)?;
    Ok(())
}

/// `db.bench_add_variant(run_id, alias)`: `INSERT OR IGNORE`, else the existing row's id.
fn add_variant(database: &Path, run_id: i64, alias: &str) -> Result<i64, Raised> {
    let db = open(database)?;
    let changed = db
        .execute(
            "INSERT OR IGNORE INTO bench_variant(run_id, alias, argv_json) VALUES(?, ?, ?)",
            rusqlite::params![run_id, alias, "{}"],
        )
        .map_err(sql)?;
    if changed > 0 {
        return Ok(db.last_insert_rowid());
    }
    use rusqlite::OptionalExtension;
    Ok(db
        .query_row(
            "SELECT id FROM bench_variant WHERE run_id = ? AND alias = ?",
            rusqlite::params![run_id, alias],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(sql)?
        .unwrap_or(0))
}

fn set_load_ms(database: &Path, variant_id: i64, load_ms: Option<f64>) -> Result<(), Raised> {
    let db = open(database)?;
    db.execute(
        "UPDATE bench_variant SET load_ms = ? WHERE id = ?",
        rusqlite::params![load_ms, variant_id],
    )
    .map_err(sql)?;
    Ok(())
}

struct ResultRow {
    variant_id: i64,
    prompt_name: String,
    rep: i64,
    cold: bool,
    m: Measured,
    peak_vram_json: String,
    contended: bool,
}

/// `db.bench_add_result(variant_id, **kw)`, with its NOT NULL defaults.
fn add_result(database: &Path, r: &ResultRow) -> Result<(), Raised> {
    let m = &r.m;
    let opt = |x: Option<f64>| x.map_or(Value::Null, Value::Real);
    let vals: Vec<Value> = vec![
        Value::Integer(r.variant_id),
        Value::Text(r.prompt_name.clone()),
        Value::Integer(r.rep),
        Value::Integer(i64::from(r.cold)),
        opt(m.ttft_ms),
        opt(m.ttft_answer_ms),
        Value::Integer(m.truncated),
        opt(m.total_ms),
        m.prompt_n.value(9)?,
        m.prompt_tps.value(10)?,
        m.gen_n.value(11)?,
        m.gen_tps.value(12)?,
        m.draft_n.value(13)?,
        opt(m.draft_acc),
        Value::Text(r.peak_vram_json.clone()),
        Value::Integer(i64::from(r.contended)),
        Value::Text(m.err.clone()),
        Value::Text(m.response_text.clone()),
    ];
    let db = open(database)?;
    db.execute(
        "INSERT INTO bench_result(variant_id, prompt_name, rep, cold, ttft_ms, ttft_answer_ms, \
         truncated, total_ms, prompt_n, prompt_tps, gen_n, gen_tps, draft_n, draft_acc, \
         peak_vram_json, contended, err, response_text) \
         VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        rusqlite::params_from_iter(vals.iter()),
    )
    .map_err(sql)?;
    Ok(())
}

// ------------------------------------------------------------------ llama-server

/// `measure()`'s answer; an empty `err` is a completed request. Numbers the server reported are
/// kept as the values sqlite3 would bind.
#[derive(Debug, Clone, PartialEq)]
pub struct Measured {
    pub err: String,
    pub ttft_ms: Option<f64>,
    pub ttft_answer_ms: Option<f64>,
    pub truncated: i64,
    pub total_ms: Option<f64>,
    pub prompt_n: Bind,
    pub prompt_tps: Bind,
    pub gen_n: Bind,
    pub gen_tps: Bind,
    pub draft_n: Bind,
    pub draft_acc: Option<f64>,
    pub response_text: String,
}

impl Measured {
    fn failed(err: String) -> Self {
        Self {
            err,
            ttft_ms: None,
            ttft_answer_ms: None,
            truncated: 0,
            total_ms: None,
            prompt_n: Bind::Val(Value::Null),
            prompt_tps: Bind::Val(Value::Null),
            gen_n: Bind::Val(Value::Null),
            gen_tps: Bind::Val(Value::Null),
            draft_n: Bind::Val(Value::Null),
            draft_acc: None,
            response_text: String::new(),
        }
    }
}

/// Python's type name for a JSON value (what an AttributeError/TypeError message names).
fn type_name(v: &Json) -> &'static str {
    match v {
        Json::Null => "NoneType",
        Json::Bool(_) => "bool",
        Json::Int(_) => "int",
        Json::Float(_) => "float",
        Json::Str(_) => "str",
        Json::Arr(_) => "list",
        Json::Obj(_) => "dict",
    }
}

fn no_get(v: &Json) -> Raised {
    raised(
        "AttributeError",
        format!("'{}' object has no attribute 'get'", type_name(v)),
    )
}

/// `for x in value`: the items Python iterates (a dict gives its keys, a str its characters).
fn iterate(v: &Json) -> Result<Vec<Json>, Raised> {
    Ok(match v {
        Json::Arr(items) => items.clone(),
        Json::Obj(pairs) => pairs.iter().map(|(k, _)| Json::Str(k.clone())).collect(),
        Json::Str(s) => s.chars().map(|c| Json::Str(c.to_string())).collect(),
        other => {
            return Err(raised(
                "TypeError",
                format!("'{}' object is not iterable", type_name(other)),
            ))
        }
    })
}

/// `dict.get(key)` that raises like Python on a non-dict.
fn get<'a>(v: &'a Json, key: &str) -> Result<Option<&'a Json>, Raised> {
    match v {
        Json::Obj(_) => Ok(v.get(key)),
        other => Err(no_get(other)),
    }
}

/// `str(value)`.
pub fn py_str_value(v: &Json) -> String {
    if v.truthy() {
        return crate::prompts::py_str(Some(v)).unwrap_or_default();
    }
    match v {
        Json::Null => "None".into(),
        Json::Bool(_) => "False".into(),
        Json::Int(_) => "0".into(),
        Json::Float(f) => py_float_repr(*f),
        Json::Str(_) => String::new(),
        Json::Arr(_) => "[]".into(),
        Json::Obj(_) => "{}".into(),
    }
}

/// `int(value)` (Python 3.12): `Ok(Some(n))`, `Ok(None)` for an int past 64 bits (which sqlite3
/// and the store refuse later), `Err` for what `int()` itself raises.
pub fn py_int(v: &Json) -> Result<Option<i64>, Raised> {
    match v {
        Json::Bool(b) => Ok(Some(i64::from(*b))),
        Json::Int(n) => Ok(n.to_string().parse().ok()),
        Json::Float(f) if f.is_nan() => {
            Err(raised("ValueError", "cannot convert float NaN to integer"))
        }
        Json::Float(f) if f.is_infinite() => Err(raised(
            "OverflowError",
            "cannot convert float infinity to integer",
        )),
        #[allow(clippy::cast_possible_truncation)]
        Json::Float(f) => {
            let t = f.trunc();
            Ok((t.abs() < 9.2e18).then_some(t as i64))
        }
        Json::Str(s) => {
            let t = crate::section_writes::cfg::strip(s);
            let (neg, d) = match t.as_bytes().first() {
                Some(b'-') => (true, t.get(1..).unwrap_or_default()),
                Some(b'+') => (false, t.get(1..).unwrap_or_default()),
                _ => (false, t),
            };
            if d.is_empty()
                || d.starts_with('_')
                || d.ends_with('_')
                || d.contains("__")
                || !d.bytes().all(|b| b.is_ascii_digit() || b == b'_')
            {
                return Err(raised(
                    "ValueError",
                    format!(
                        "invalid literal for int() with base 10: {}",
                        crate::prompts::py_repr_str(s).unwrap_or_default()
                    ),
                ));
            }
            let digits: String = d.chars().filter(char::is_ascii_digit).collect();
            Ok(digits.parse::<i64>().ok().map(|n| if neg { -n } else { n }))
        }
        other => Err(raised(
            "TypeError",
            format!(
                "int() argument must be a string, a bytes-like object or a real number, not '{}'",
                type_name(other)
            ),
        )),
    }
}

/// A value as sqlite3 will bind it when the row is written: an int past 64 bits and a list or
/// dict raise then, in parameter order.
#[derive(Debug, Clone, PartialEq)]
pub enum Bind {
    Val(Value),
    /// An int past 64 bits (its float value, for the draft ratio).
    Big(f64),
    /// A list or dict.
    Bad(&'static str),
}

impl Bind {
    fn value(&self, index: usize) -> Result<Value, Raised> {
        match self {
            Bind::Val(v) => Ok(v.clone()),
            Bind::Big(_) => Err(raised(
                "OverflowError",
                "Python int too large to convert to SQLite INTEGER",
            )),
            Bind::Bad(t) => Err(raised(
                "ProgrammingError",
                format!("Error binding parameter {index}: type '{t}' is not supported"),
            )),
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn number(&self) -> f64 {
        match self {
            Bind::Val(Value::Integer(n)) => *n as f64,
            Bind::Big(f) => *f,
            _ => 0.0,
        }
    }
}

/// `int(x or 0)`: `int()`'s own errors raise here.
fn count(v: Option<&Json>) -> Result<Bind, Raised> {
    match v.filter(|v| v.truthy()) {
        None => Ok(Bind::Val(Value::Integer(0))),
        Some(v) => Ok(match py_int(v)? {
            Some(n) => Bind::Val(Value::Integer(n)),
            None => Bind::Big(match v {
                Json::Int(n) => n.to_string().parse().unwrap_or(f64::INFINITY),
                Json::Float(f) => f.trunc(),
                Json::Str(t) => {
                    let digits: String = t
                        .chars()
                        .filter(|c| *c == '-' || c.is_ascii_digit())
                        .collect();
                    digits.parse().unwrap_or(f64::INFINITY)
                }
                _ => f64::INFINITY,
            }),
        }),
    }
}

/// A raw `timings` value.
fn raw(v: Option<&Json>) -> Bind {
    match v {
        None | Some(Json::Null) => Bind::Val(Value::Null),
        Some(Json::Bool(b)) => Bind::Val(Value::Integer(i64::from(*b))),
        Some(Json::Int(n)) => match n.to_string().parse() {
            Ok(n) => Bind::Val(Value::Integer(n)),
            Err(_) => Bind::Big(n.to_string().parse().unwrap_or(f64::INFINITY)),
        },
        // SQLite stores NaN as NULL.
        Some(Json::Float(f)) if f.is_nan() => Bind::Val(Value::Null),
        Some(Json::Float(f)) => Bind::Val(Value::Real(*f)),
        Some(Json::Str(s)) => Bind::Val(Value::Text(s.clone())),
        Some(Json::Arr(_)) => Bind::Bad("list"),
        Some(Json::Obj(_)) => Bind::Bad("dict"),
    }
}

/// `round(x, n)`.
fn round(x: f64, digits: usize) -> f64 {
    crate::bench_runs::py_round(crate::bench_runs::Num::Float(x), digits).map_or(x, |n| match n {
        crate::bench_runs::Num::Float(f) => f,
        #[allow(clippy::cast_precision_loss)]
        crate::bench_runs::Num::Int(i) => i as f64,
    })
}

/// What an httpx error is named as, with a short native description (the texts differ).
fn http_error(e: &reqwest::Error) -> String {
    let kind = if e.is_connect() {
        if e.is_timeout() {
            "ConnectTimeout"
        } else {
            "ConnectError"
        }
    } else if e.is_timeout() {
        "ReadTimeout"
    } else if e.is_body() || e.is_decode() {
        "ReadError"
    } else {
        "RemoteProtocolError"
    };
    format!("{kind}: {}", short(e))
}

fn short(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        text = s.to_string();
        source = s.source();
    }
    text
}

/// `str.splitlines()` fed in pieces, as httpx's `LineDecoder` feeds it.
#[derive(Default)]
struct Lines {
    buffer: String,
}

impl Lines {
    fn push(&mut self, text: &str) -> Vec<String> {
        self.buffer.push_str(text);
        let mut out = Vec::new();
        while let Some((i, c)) = self.buffer.char_indices().find(|(_, c)| is_newline(*c)) {
            if c == '\r' {
                let rest = self.buffer.get(i + 1..).unwrap_or_default();
                if rest.is_empty() {
                    break; // a trailing \r may be the first half of \r\n
                }
                let end = if rest.starts_with('\n') { i + 2 } else { i + 1 };
                out.push(self.buffer.get(..i).unwrap_or_default().to_owned());
                self.buffer.drain(..end);
            } else {
                out.push(self.buffer.get(..i).unwrap_or_default().to_owned());
                self.buffer.drain(..i + c.len_utf8());
            }
        }
        out
    }

    fn flush(&mut self) -> Vec<String> {
        let rest = std::mem::take(&mut self.buffer);
        crate::section_writes::cfg::splitlines(&rest)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }
}

fn is_newline(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\u{0b}'
            | '\u{0c}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// UTF-8 decoding with replacement, fed in pieces (httpx's `TextDecoder`).
#[derive(Default)]
struct Utf8 {
    pending: Vec<u8>,
}

impl Utf8 {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => self.pending.len(),
        };
        let text =
            String::from_utf8_lossy(self.pending.get(..valid).unwrap_or_default()).into_owned();
        self.pending.drain(..valid);
        text
    }

    fn flush(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

/// `bench.measure(base_url, alias, prompt, max_tokens)`. `Err` is an exception that escapes it
/// (not an httpx error), which ends the run.
pub async fn measure(
    http: &reqwest::Client,
    clock: &Clock,
    base_url: &str,
    alias: &str,
    prompt: &str,
    max_tokens: i64,
) -> Result<Measured, Raised> {
    use futures_util::StreamExt;
    // json.dumps(payload, ensure_ascii=False, separators=(",", ":")), in the dict's order.
    let body = format!(
        "{{\"model\":{},\"messages\":[{{\"role\":\"user\",\"content\":{}}}],\"max_tokens\":{max_tokens},\
         \"temperature\":0.0,\"stream\":true,\"stream_options\":{{\"include_usage\":true}}}}",
        json_string(alias),
        json_string(prompt),
    );
    let t0 = (clock.perf_counter)();
    let mut ttft: Option<f64> = None;
    let mut ttft_answer: Option<f64> = None;
    let mut finish_reason = String::new();
    let mut timings = Json::Obj(Vec::new());
    let mut answer: Vec<Json> = Vec::new();
    let response = match http
        .post(format!("{base_url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Ok(Measured::failed(http_error(&e))),
    };
    if response.status() != reqwest::StatusCode::OK {
        let status = response.status().as_u16();
        let mut text = Utf8::default();
        let mut decoded = String::new();
        let mut stream = response.bytes_stream();
        let mut size = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => return Ok(Measured::failed(http_error(&e))),
            };
            size = size.saturating_add(chunk.len());
            if size > ANSWER_CAP {
                break;
            }
            decoded.push_str(&text.push(&chunk));
        }
        decoded.push_str(&text.flush());
        let head: String = decoded.chars().take(200).collect();
        return Ok(Measured::failed(format!("HTTP {status}: {head}")));
    }
    let mut stream = response.bytes_stream();
    let mut text = Utf8::default();
    let mut lines = Lines::default();
    let mut size = 0usize;
    let mut done = false;
    'stream: loop {
        let batch = match stream.next().await {
            Some(Ok(chunk)) => {
                size = size.saturating_add(chunk.len());
                if size > ANSWER_CAP {
                    return Ok(Measured::failed(
                        "ReadError: the streamed answer exceeded its limit".into(),
                    ));
                }
                lines.push(&text.push(&chunk))
            }
            Some(Err(e)) => return Ok(Measured::failed(http_error(&e))),
            None => {
                done = true;
                let mut rest = lines.push(&text.flush());
                rest.extend(lines.flush());
                rest
            }
        };
        for line in batch {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let data = crate::section_writes::cfg::strip(data);
            if data == "[DONE]" {
                break 'stream;
            }
            let Ok(chunk) = model_files::json::parse_with_limits(
                data,
                model_files::json::Limits {
                    depth: 512,
                    string_chars: usize::MAX,
                    ..model_files::json::Limits::default()
                },
            ) else {
                continue;
            };
            if let Some(t) = get(&chunk, "timings")?.filter(|t| t.truthy()) {
                timings = t.clone();
            }
            let choices = get(&chunk, "choices")?
                .filter(|c| c.truthy())
                .map_or(Ok(Vec::new()), iterate)?;
            for ch in &choices {
                let delta = match get(ch, "delta")?.filter(|d| d.truthy()) {
                    Some(d) => d.clone(),
                    None => Json::Obj(Vec::new()),
                };
                if let Some(reason) = get(ch, "finish_reason")?.filter(|r| r.truthy()) {
                    finish_reason = py_str_value(reason);
                }
                let content = get(&delta, "content")?.filter(|c| c.truthy()).cloned();
                let visible = content.is_some()
                    || get(&delta, "reasoning_content")?.is_some_and(Json::truthy);
                if visible && ttft.is_none() {
                    ttft = Some((clock.perf_counter)() - t0);
                }
                if let Some(content) = content {
                    if ttft_answer.is_none() {
                        ttft_answer = Some((clock.perf_counter)() - t0);
                    }
                    answer.push(content);
                }
            }
        }
        if done {
            break;
        }
    }
    let total = (clock.perf_counter)() - t0;
    let draft_n = count(get(&timings, "draft_n")?)?;
    let draft_ok = count(get(&timings, "draft_n_accepted")?)?;
    let truncated = i64::from(finish_reason == "length");
    let prompt_n = count(get(&timings, "prompt_n")?)?;
    let prompt_tps = raw(get(&timings, "prompt_per_second")?);
    let gen_n = count(get(&timings, "predicted_n")?)?;
    let gen_tps = raw(get(&timings, "predicted_per_second")?);
    let (dn, dok) = (draft_n.number(), draft_ok.number());
    let draft_acc = (dn != 0.0).then(|| round(dok / dn, 4));
    let mut response_text = String::new();
    for (i, part) in answer.iter().enumerate() {
        match part {
            Json::Str(s) => response_text.push_str(s),
            other => {
                return Err(raised(
                    "TypeError",
                    format!(
                        "sequence item {i}: expected str instance, {} found",
                        type_name(other)
                    ),
                ))
            }
        }
    }
    Ok(Measured {
        err: String::new(),
        ttft_ms: ttft.map(|t| round(t * 1000.0, 2)),
        ttft_answer_ms: ttft_answer.map(|t| round(t * 1000.0, 2)),
        truncated,
        total_ms: Some(round(total * 1000.0, 2)),
        prompt_n,
        prompt_tps,
        gen_n,
        gen_tps,
        draft_n: if dn == 0.0 {
            Bind::Val(Value::Null)
        } else {
            draft_n
        },
        draft_acc,
        response_text,
    })
}

/// `bench.loaded_aliases(base_url)`: the ids whose `status.value` is `"loaded"`.
pub async fn loaded_aliases(http: &reqwest::Client, base_url: &str) -> Result<Vec<String>, Raised> {
    use futures_util::StreamExt;
    let Ok(response) = http
        .get(format!("{base_url}/v1/models"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
    else {
        return Ok(Vec::new());
    };
    if !response.status().is_success() {
        return Ok(Vec::new());
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return Ok(Vec::new());
        };
        if body.len().saturating_add(chunk.len()) > MODELS_CAP {
            return Ok(Vec::new());
        }
        body.extend_from_slice(&chunk);
    }
    let Ok(text) = crate::prompts::decode_json_bytes(&body, MODELS_CAP) else {
        return Ok(Vec::new());
    };
    let Ok(v) = model_files::json::parse_with_limits(
        &text,
        model_files::json::Limits {
            depth: 512,
            string_chars: usize::MAX,
            ..model_files::json::Limits::default()
        },
    ) else {
        return Ok(Vec::new());
    };
    let data = get(&v, "data")?
        .filter(|d| d.truthy())
        .map_or(Ok(Vec::new()), iterate)?;
    let mut out = Vec::new();
    for m in &data {
        let status = get(m, "status")?.filter(|s| s.truthy());
        let value = match status {
            Some(s) => get(s, "value")?,
            None => None,
        };
        if value == Some(&Json::Str("loaded".into())) {
            let id = get(m, "id")?.filter(|i| i.truthy());
            out.push(id.map_or(String::new(), py_str_value));
        }
    }
    Ok(out)
}

/// `api.benchmark_start`'s body conversions, before `bench.start`: `Err` is the 500 they raise.
pub fn start_request(body: &Json) -> Result<StartRequest, Raised> {
    let field = |k: &str| body.get(k).filter(|v| v.truthy());
    let backend = field("backend").map_or(String::new(), py_str_value);
    let aliases = field("aliases")
        .map_or(Ok(Vec::new()), iterate)?
        .iter()
        .map(py_str_value)
        .collect();
    let mut prompt_ids = Vec::new();
    for p in field("promptIds").map_or(Ok(Vec::new()), iterate)? {
        match py_int(&p)? {
            Some(n) => prompt_ids.push(n),
            None => {
                return Err(raised(
                    "OverflowError",
                    "Python int too large to convert to SQLite INTEGER",
                ))
            }
        }
    }
    let int_or = |k: &str, default: i64| -> Result<i64, Raised> {
        match field(k) {
            None => Ok(default),
            // Past 64 bits only the clamp sees it.
            Some(v) => Ok(py_int(v)?.unwrap_or_else(|| {
                if matches!(v, Json::Int(n) if n.to_string().starts_with('-'))
                    || matches!(v, Json::Float(f) if *f < 0.0)
                    || matches!(v, Json::Str(s) if crate::section_writes::cfg::strip(s).starts_with('-'))
                {
                    i64::MIN
                } else {
                    i64::MAX
                }
            })),
        }
    };
    Ok(StartRequest {
        backend,
        aliases,
        prompt_ids,
        reps: int_or("reps", 3)?,
        max_tokens: int_or("maxTokens", DEFAULT_MAX_TOKENS)?,
    })
}

/// The bound on one backend restart (`t=15` plus the start), noevia#1944.
pub const RESTART_TIMEOUT: Duration = Duration::from_secs(60);

/// Where the production runner reaches everything it does not own.
pub struct Production {
    pub data: PathBuf,
    pub ini: PathBuf,
    /// `MODELS_DIR`: sweeps mount backend mounts from here only.
    pub models_dir: String,
    pub socket: PathBuf,
    pub docker: Arc<dyn crate::DockerRead>,
    pub configured: Vec<String>,
    pub legacy: reqwest::Client,
    pub legacy_base: String,
    pub token: String,
}

/// The runner `main` builds: workers on tokio, observations from the child's private route,
/// backends resolved and sweep containers handled through the sweep allowlist, and backend
/// restarts limited to the effective backends.
pub fn production(p: Production) -> Result<Arc<Runner>, &'static str> {
    use crate::docker::sweep::{BackendNames, CALL_TIMEOUT};
    // The effective backends, as `services.restart_llama_backend` reads them: the configured
    // names, plus discovered llama.cpp containers when the inventory answers in time.
    let effective = {
        let (docker, configured) = (p.docker.clone(), p.configured.clone());
        move || {
            let (docker, configured) = (docker.clone(), configured.clone());
            async move {
                let inventory = tokio::time::timeout(CALL_TIMEOUT, docker.containers())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
                crate::effective_names(&configured, &inventory).unwrap_or_default()
            }
        }
    };
    let names = effective.clone();
    let backends: BackendNames = Arc::new(move || Box::pin(names()));
    let sweep_docker = Arc::new(crate::docker::sweep::UnixSweepDocker::new(
        p.socket,
        &p.models_dir,
        backends,
    ));
    let resolver_docker = sweep_docker.clone();
    let (legacy, base, token) = (p.legacy, p.legacy_base, p.token);
    let observe: Observer = Arc::new(move |query: String| {
        let request = legacy
            .get(format!("{base}{OBSERVE_PATH}?{query}"))
            .timeout(Duration::from_secs(10));
        let request = if token.is_empty() {
            request
        } else {
            request.header("x-model-loader-token", token.clone())
        };
        Box::pin(async move {
            use futures_util::StreamExt;
            let response = request.send().await.ok()?;
            if response.status() != reqwest::StatusCode::OK {
                return None;
            }
            let mut stream = response.bytes_stream();
            let mut body = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.ok()?;
                if body.len().saturating_add(chunk.len()) > OBSERVE_CAP {
                    return None;
                }
                body.extend_from_slice(&chunk);
            }
            parse_observed(&body)
        })
    });
    let resolve: Resolver = Arc::new(move |backend: String| {
        let docker = resolver_docker.clone();
        Box::pin(async move { docker.endpoint(&backend).await })
    });
    let docker = p.docker;
    let restart: Restarter = Arc::new(move |name: String| {
        let (docker, effective) = (docker.clone(), effective.clone());
        Box::pin(async move {
            // services.restart_llama_backend: only an effective backend, by name.
            if !effective().await.contains(&name) {
                return Err("container not in configured list".to_owned());
            }
            // noevia#1944: `restart?t=15` waits up to 15 s for the stop; bound the whole call.
            tokio::time::timeout(RESTART_TIMEOUT, docker.restart_backend(&name))
                .await
                .map_err(|_| "ReadTimeout: Docker did not answer the restart in time".to_owned())?
                .map_err(str::to_owned)
        })
    });
    Runner::with_sweeps(
        &p.data,
        Clock::default(),
        Arc::new(|task| {
            tokio::spawn(task);
        }),
        observe,
        resolve,
        Some(Sweeps {
            docker: sweep_docker,
            ini: p.ini,
            restart,
            sleep: Sweeps::tokio_sleep(),
        }),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn lines_follow_splitlines_across_pieces() {
        let mut l = Lines::default();
        assert_eq!(l.push("a\r"), Vec::<String>::new());
        assert_eq!(l.push("\nb\rc"), vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(l.push("\u{2028}d"), vec!["c".to_owned()]);
        assert_eq!(l.flush(), vec!["d".to_owned()]);
    }

    #[test]
    fn utf8_waits_for_split_characters() {
        let mut u = Utf8::default();
        let bytes = "é".as_bytes();
        assert_eq!(u.push(&bytes[..1]), "");
        assert_eq!(u.push(&bytes[1..]), "é");
        assert_eq!(u.push(b"\xffx"), "\u{fffd}x");
    }

    #[test]
    fn job_renders_like_bench_job() {
        let clock = Clock {
            perf_counter: || 0.0,
            time: || 110.0,
        };
        let s = JobState {
            status: "running".into(),
            total: 3,
            done: 1,
            started_at: 100.0,
            ..JobState::default()
        };
        assert_eq!(
            s.render(&clock),
            r#"{"run_id":0,"status":"running","backend":"","total":3,"done":1,"current":"","started_at":100.0,"finished_at":0.0,"error":"","unit":"requests","lines":[],"pct":33,"elapsed":10.0,"eta":20.0,"active":true}"#
        );
    }

    #[test]
    fn int_conversion_is_pythons() {
        let p = |t: &str| py_int(&model_files::json::parse(t).unwrap());
        assert_eq!(p("\" 1_0 \"").unwrap(), Some(10));
        assert_eq!(p("2.9").unwrap(), Some(2));
        assert_eq!(p("true").unwrap(), Some(1));
        assert!(p("\"x\"").is_err());
        assert!(p("[1]").is_err());
        assert_eq!(p("1180591620717411303424").unwrap(), None);
    }
}
