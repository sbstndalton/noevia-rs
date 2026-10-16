//! noevia#1318 slice 9b: llama-bench throughput sweeps (`bench.start_sweep`, `_run_sweep`,
//! `run_sweep_once`, `_free_gpu`, `db.bench_add_sweep`) and the startup recovery of leftover
//! sweep containers (`reap_orphans`, `_watch_orphans`), on the same job slot as the prompt
//! suite. Docker is reached only through [`SweepDocker`], whose Unix implementation is held to
//! `docker::sweep::allowed_sweep`.
use super::{
    blocking, count, create_run, finish_run, get, loaded_aliases, open, py_int, py_str_value,
    raised, raw, sql, Bind, JobState, Raised, Runner,
};
use crate::docker::sweep::{create_body, BackendRuntime, CreateBody};
use crate::section_writes::cfg;
use model_files::json::Value as Json;
use rusqlite::types::Value;
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

/// `run_sweep_once(timeout_s=1800)`.
pub const SWEEP_TIMEOUT_S: f64 = 1800.0;
/// `bench.ORPHAN_RETRY_S`.
pub const ORPHAN_RETRY: Duration = Duration::from_secs(5);
/// Python retries a stuck leftover forever; this gives up after an hour and reports it.
pub const ORPHAN_ATTEMPTS: usize = 720;
const INI_CAP: u64 = 16 * 1024 * 1024;

pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type Job<'a, T> = crate::Job<'a, T>;

/// The Docker operations a sweep makes. Errors are docker-py's `"{type}: {message}"`.
pub trait SweepDocker: Send + Sync {
    /// `_sweep_runtime`: the backend's image, mounts and GPU request, read live.
    fn runtime<'a>(&'a self, backend: &'a str) -> Job<'a, Result<BackendRuntime, String>>;
    /// `containers.run(detach=True)`: create (body checked against `runtime`) and start; a
    /// failed start removes the container it created. The new container's id.
    fn run<'a>(
        &'a self,
        body: &'a CreateBody,
        runtime: &'a BackendRuntime,
    ) -> Job<'a, Result<String, String>>;
    /// `cont.reload(); cont.status`.
    fn status<'a>(&'a self, id: &'a str) -> Job<'a, Result<String, String>>;
    fn kill<'a>(&'a self, id: &'a str) -> Job<'a, Result<(), String>>;
    /// `cont.logs(stdout=…, stderr=not stdout)`.
    fn logs<'a>(&'a self, id: &'a str, stdout: bool) -> Job<'a, Result<Vec<u8>, String>>;
    /// `remove(force=True)`; an `Err` starting `NotFound:` means it is already gone.
    fn remove<'a>(&'a self, id: &'a str) -> Job<'a, Result<(), String>>;
    /// `_sweep_containers`: ids of containers carrying exactly the sweep label.
    fn list_labelled(&self) -> Job<'_, Result<Vec<String>, String>>;
}

/// Restarts a backend (only one of the effective backends) and waits for Docker's answer.
pub type Restarter = Arc<dyn Fn(String) -> Fut<'static, Result<(), String>> + Send + Sync>;
/// `time.sleep`.
pub type Sleeper = Arc<dyn Fn(Duration) -> Fut<'static, ()> + Send + Sync>;

pub struct Sweeps {
    pub docker: Arc<dyn SweepDocker>,
    pub ini: PathBuf,
    pub restart: Restarter,
    pub sleep: Sleeper,
}

impl Sweeps {
    pub fn tokio_sleep() -> Sleeper {
        Arc::new(|d| Box::pin(tokio::time::sleep(d)))
    }
}

/// A Python int from the request: its `str()` and, when it fits, its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyInt {
    pub text: String,
    pub small: Option<i64>,
}

impl PyInt {
    fn of(n: i64) -> Self {
        Self {
            text: n.to_string(),
            small: Some(n),
        }
    }

    fn bind(&self) -> Result<i64, Raised> {
        self.small.ok_or_else(|| {
            raised(
                "OverflowError",
                "Python int too large to convert to SQLite INTEGER",
            )
        })
    }
}

/// `int(v)`, keeping the digits of a value past 64 bits.
fn py_int_text(v: &Json) -> Result<PyInt, Raised> {
    let small = py_int(v)?;
    if let Some(n) = small {
        return Ok(PyInt::of(n));
    }
    let text = match v {
        Json::Int(n) => n.to_string(),
        Json::Float(f) => format!("{:.0}", f.trunc()),
        Json::Str(s) => {
            let t = cfg::strip(s);
            let (neg, d) = match t.as_bytes().first() {
                Some(b'-') => (true, t.get(1..).unwrap_or_default()),
                Some(b'+') => (false, t.get(1..).unwrap_or_default()),
                _ => (false, t),
            };
            let digits: String = d.chars().filter(char::is_ascii_digit).collect();
            let digits = digits.trim_start_matches('0');
            if digits.is_empty() {
                "0".into()
            } else if neg {
                format!("-{digits}")
            } else {
                digits.to_owned()
            }
        }
        _ => String::new(),
    };
    Ok(PyInt { text, small: None })
}

/// The checked `POST /api/v1/benchmark/sweep` body (`api.benchmark_sweep`'s conversions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepRequest {
    pub backend: String,
    pub aliases: Vec<String>,
    pub n_prompt: PyInt,
    pub n_gen: PyInt,
    pub depths: String,
    pub reps: PyInt,
}

pub fn sweep_request(body: &Json) -> Result<SweepRequest, Raised> {
    let field = |k: &str| body.get(k).filter(|v| v.truthy());
    let backend = field("backend").map_or(String::new(), py_str_value);
    let aliases = field("aliases")
        .map_or(Ok(Vec::new()), super::iterate)?
        .iter()
        .map(py_str_value)
        .collect();
    let int_or = |k: &str, default: i64| match field(k) {
        None => Ok(PyInt::of(default)),
        Some(v) => py_int_text(v),
    };
    let n_prompt = int_or("nPrompt", 512)?;
    let n_gen = int_or("nGen", 128)?;
    let depths = field("depths").map_or_else(|| "0,4096,16384".to_owned(), py_str_value);
    let reps = int_or("reps", 3)?;
    Ok(SweepRequest {
        backend,
        aliases,
        n_prompt,
        n_gen,
        depths,
        reps,
    })
}

/// One alias to sweep: `(alias, model path, llama-bench flags from its section)`.
type Target = (String, String, Vec<String>);

/// `ini.get_section(a)` / `sweep_args_for_section(a)` for every alias, from one read.
fn targets(ini: &Path, aliases: &[String]) -> Result<Result<Vec<Target>, String>, Raised> {
    let text = match std::fs::metadata(ini) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(raised("OSError", e)),
        Ok(m) if m.len() > INI_CAP => return Err(raised("OSError", "models.ini is too large")),
        Ok(_) => {
            let bytes = std::fs::read(ini).map_err(|e| raised("OSError", e))?;
            String::from_utf8(bytes)
                .map_err(|_| raised("UnicodeDecodeError", "models.ini is not UTF-8"))?
        }
    };
    let cp = cfg::parse_allowing(&text)
        .map_err(|e| raised("ParsingError", e.message().unwrap_or_default()))?;
    let mut out = Vec::new();
    for a in aliases {
        let model = cp
            .items(a)
            .and_then(|v| v.get("model").map(|m| cfg::strip(m).to_owned()))
            .unwrap_or_default();
        if model.is_empty() {
            return Ok(Err(format!("section '{a}' has no model path")));
        }
        out.push((
            a.clone(),
            model,
            crate::benchmark_overview::sweep_args(&cp, a),
        ));
    }
    Ok(Ok(out))
}

/// `json.dumps(v, sort_keys=True)` (ASCII escapes, `", "` / `": "` separators).
pub fn py_dumps(v: &Json) -> String {
    let mut out = String::new();
    dump(v, &mut out);
    out
}

fn dump(v: &Json, out: &mut String) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(n) => out.push_str(&n.to_string()),
        Json::Float(f) if f.is_nan() => out.push_str("NaN"),
        Json::Float(f) if f.is_infinite() => {
            out.push_str(if *f > 0.0 { "Infinity" } else { "-Infinity" });
        }
        Json::Float(f) => out.push_str(&gguf::py_float_repr(*f)),
        Json::Str(s) => dump_str(s, out),
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump(item, out);
            }
            out.push(']');
        }
        Json::Obj(pairs) => {
            // A dict keeps the last of duplicate keys.
            let mut keys: Vec<&String> = Vec::new();
            for (k, _) in pairs {
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump_str(k, out);
                out.push_str(": ");
                if let Some(value) = v.get(k) {
                    dump(value, out);
                }
            }
            out.push('}');
        }
    }
}

fn dump_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (' '..='~').contains(&c) => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// `int(entry.get(k) or 0)` with its `str()`.
fn int_or_zero(entry: &Json, key: &str) -> Result<(Bind, String), Raised> {
    match entry.get(key).filter(|v| v.truthy()) {
        None => Ok((Bind::Val(Value::Integer(0)), "0".into())),
        Some(v) => {
            let text = py_int_text(v)?.text;
            Ok((count(Some(v))?, text))
        }
    }
}

/// `str(entry.get(k) or "")`.
fn str_or_empty(entry: &Json, key: &str) -> String {
    entry
        .get(key)
        .filter(|v| v.truthy())
        .map_or(String::new(), py_str_value)
}

/// `db.bench_add_sweep(run_id, alias, entry)`.
fn add_sweep(database: &Path, run_id: i64, alias: &str, entry: &Json) -> Result<(), Raised> {
    get(entry, "n_prompt")?;
    let (npr, npr_text) = int_or_zero(entry, "n_prompt")?;
    let (ngen, ngen_text) = int_or_zero(entry, "n_gen")?;
    let (ndep, ndep_text) = int_or_zero(entry, "n_depth")?;
    let mut test = if npr_text == "0" {
        format!("tg{ngen_text}")
    } else {
        format!("pp{npr_text}")
    };
    if ndep_text != "0" {
        test.push_str(&format!(" @ d{ndep_text}"));
    }
    let binds = [
        Bind::Val(Value::Integer(run_id)),
        Bind::Val(Value::Text(alias.to_owned())),
        Bind::Val(Value::Text(str_or_empty(entry, "model_type"))),
        Bind::Val(Value::Text(test)),
        npr,
        ngen,
        ndep,
        raw(entry.get("avg_ts")),
        raw(entry.get("stddev_ts")),
        raw(entry.get("n_gpu_layers")),
        raw(entry.get("n_cpu_moe")),
        raw(entry.get("n_ubatch")),
        Bind::Val(Value::Text(str_or_empty(entry, "type_k"))),
        Bind::Val(Value::Text(str_or_empty(entry, "type_v"))),
        raw(entry.get("flash_attn")),
        Bind::Val(Value::Text(str_or_empty(entry, "split_mode"))),
        Bind::Val(Value::Text(py_dumps(entry))),
    ];
    let mut vals = Vec::with_capacity(binds.len());
    for (i, b) in binds.iter().enumerate() {
        vals.push(b.value(i + 1)?);
    }
    let db = open(database)?;
    db.execute(
        "INSERT INTO bench_sweep(run_id, alias, model_type, test, n_prompt, n_gen, n_depth, \
         avg_ts, stddev_ts, n_gpu_layers, n_cpu_moe, n_ubatch, type_k, type_v, flash_attn, \
         split_mode, raw_json) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        rusqlite::params_from_iter(vals.iter()),
    )
    .map_err(sql)?;
    Ok(())
}

/// The line `run_sweep_once` reports from llama-bench's stderr.
fn stderr_line(errtxt: &str) -> String {
    let lines = cfg::splitlines(cfg::strip(errtxt));
    let mut line = String::new();
    for cand in lines.iter().rev() {
        let c = cfg::strip(cand);
        let lower = c.to_lowercase();
        if !c.is_empty() && (lower.contains("error") || lower.contains("failed")) {
            line = c.to_owned();
            break;
        }
    }
    if line.is_empty() {
        line = lines
            .last()
            .map_or("no output", |l| cfg::strip(l))
            .to_owned();
    }
    line.chars().take(200).collect()
}

/// What `json.loads(raw[start:])` gives for llama-bench's stdout, `None` for a ValueError.
fn entries(raw: &[u8]) -> Option<Vec<Json>> {
    let text = String::from_utf8_lossy(raw);
    let start = text.find('[')?;
    let parsed = model_files::json::parse_with_limits(
        text.get(start..)?,
        model_files::json::Limits {
            depth: 512,
            string_chars: usize::MAX,
            ..model_files::json::Limits::default()
        },
    )
    .ok()?;
    match parsed {
        Json::Arr(items) => Some(items),
        _ => None,
    }
}

impl Runner {
    fn sweeps(&self) -> Option<&Sweeps> {
        self.sweeps.as_ref()
    }

    /// `bench.start_sweep(...)`: `Ok(Ok(()))` started, `Ok(Err(msg))` the 409, `Err` raised.
    pub async fn start_sweep(
        self: &Arc<Self>,
        req: SweepRequest,
    ) -> Result<Result<(), String>, Raised> {
        let Some(sweeps) = self.sweeps() else {
            return Err(raised("RuntimeError", "sweeps are not configured"));
        };
        let aliases: Vec<String> = req.aliases.into_iter().filter(|a| !a.is_empty()).collect();
        if cfg::strip(&req.backend).is_empty() {
            return Ok(Err("no backend selected".into()));
        }
        if aliases.is_empty() {
            return Ok(Err("no models selected".into()));
        }
        let started = (self.clock.time)();
        let claimed = self.with(|s| {
            if s.active() {
                return false;
            }
            *s = JobState {
                status: "starting".into(),
                backend: req.backend.clone(),
                started_at: started,
                ..JobState::default()
            };
            true
        });
        if !claimed {
            return Ok(Err("a benchmark is already running".into()));
        }
        let ini = sweeps.ini.clone();
        let names = aliases.clone();
        let targets = match blocking(move || targets(&ini, &names)).await {
            Ok(Ok(t)) => t,
            Ok(Err(refusal)) => {
                self.with(|s| *s = JobState::default());
                return Ok(Err(refusal));
            }
            Err(e) => return Ok(Err(self.abort_start(0, &e).await)),
        };
        self.cancel.store(false, Ordering::SeqCst);
        let database = self.database.clone();
        let backend = req.backend.clone();
        let (reps, n_gen) = (req.reps.clone(), req.n_gen.clone());
        let run_id = match blocking(move || {
            create_run(&database, &backend, reps.bind()?, n_gen.bind()?, started)
        })
        .await
        {
            Ok(id) => id,
            Err(e) => return Ok(Err(self.abort_start(0, &e).await)),
        };
        #[allow(clippy::cast_possible_wrap)]
        let total = targets.len() as i64;
        self.with(|s| {
            s.run_id = run_id;
            s.status = "running".into();
            s.total = total;
            s.unit = "models".into();
        });
        let me = self.clone();
        (self.spawn)(Box::pin(async move {
            me.run_sweep(
                run_id,
                req.backend,
                targets,
                req.n_prompt,
                req.n_gen,
                req.depths,
                req.reps,
            )
            .await;
        }));
        Ok(Ok(()))
    }

    /// `bench._run_sweep`.
    #[allow(clippy::too_many_arguments)]
    async fn run_sweep(
        self: Arc<Self>,
        run_id: i64,
        backend: String,
        targets: Vec<Target>,
        n_prompt: PyInt,
        n_gen: PyInt,
        depths: String,
        reps: PyInt,
    ) {
        let mut status = "done".to_owned();
        let mut err = String::new();
        let result = self
            .sweep_targets(
                run_id,
                &backend,
                &targets,
                &n_prompt,
                &n_gen,
                &depths,
                &reps,
                &mut status,
            )
            .await;
        if let Err(Raised(e)) = result {
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
    async fn sweep_targets(
        &self,
        run_id: i64,
        backend: &str,
        targets: &[Target],
        n_prompt: &PyInt,
        n_gen: &PyInt,
        depths: &str,
        reps: &PyInt,
        status: &mut String,
    ) -> Result<(), Raised> {
        if let Ok(base_url) = (self.resolve)(backend.to_owned()).await {
            self.free_gpu(backend, &base_url).await;
        }
        for (alias, model_path, extra) in targets {
            if self.cancelled() {
                *status = "cancelled".into();
                break;
            }
            let current = format!("{alias} - llama bench");
            self.with(|s| s.current = current);
            let flags = if extra.is_empty() {
                "section defaults".to_owned()
            } else {
                extra.join(" ")
            };
            let line = format!("{alias}: {flags}");
            self.with(|s| s.log(line));
            let (found, e) = self
                .run_sweep_once(backend, model_path, extra, n_prompt, n_gen, depths, reps)
                .await?;
            if !e.is_empty() {
                let line = format!("{alias}: {e}");
                self.with(|s| s.log(line));
                if e == "cancelled" {
                    *status = "cancelled".into();
                    break;
                }
            }
            for entry in &found {
                let database = self.database.clone();
                let (a, entry) = (alias.clone(), entry.clone());
                blocking(move || add_sweep(&database, run_id, &a, &entry)).await?;
            }
            if !found.is_empty() {
                let line = format!("{alias}: {} measurements", found.len());
                self.with(|s| s.log(line));
            }
            self.with(|s| s.done += 1);
        }
        Ok(())
    }

    /// `bench._free_gpu`. Unlike Python (noevia#1940) the restart is awaited before the router
    /// is polled, and a refused or failed restart is reported instead of polled past.
    async fn free_gpu(&self, backend: &str, base_url: &str) {
        let Some(sweeps) = self.sweeps() else {
            return;
        };
        match loaded_aliases(&self.http, base_url).await {
            Ok(loaded) if !loaded.is_empty() => {}
            _ => return,
        }
        self.with(|s| {
            s.log(
                "freeing GPU: restarting the backend so the sweep has the cards to itself".into(),
            );
        });
        if let Err(e) = (sweeps.restart)(backend.to_owned()).await {
            let line = format!("could not restart {backend}: {e}");
            self.with(|s| s.log(line));
            return;
        }
        for _ in 0..60 {
            if self.cancelled() {
                return;
            }
            let up = self
                .http
                .get(format!("{base_url}/v1/models"))
                .timeout(Duration::from_secs(3))
                .send()
                .await
                .is_ok_and(|r| r.status() == reqwest::StatusCode::OK);
            if up {
                self.with(|s| s.log("backend back up, GPUs clear".into()));
                return;
            }
            (sweeps.sleep)(Duration::from_secs(1)).await;
        }
        self.with(|s| s.log("backend did not answer within 60s; continuing anyway".into()));
    }

    /// `bench.run_sweep_once`: `(entries, error)`; a Docker failure while watching raises.
    #[allow(clippy::too_many_arguments)]
    async fn run_sweep_once(
        &self,
        backend: &str,
        model_path: &str,
        extra: &[String],
        n_prompt: &PyInt,
        n_gen: &PyInt,
        depths: &str,
        reps: &PyInt,
    ) -> Result<(Vec<Json>, String), Raised> {
        let Some(sweeps) = self.sweeps() else {
            return Ok((Vec::new(), "sweeps are not configured".into()));
        };
        let docker = &sweeps.docker;
        let runtime = match docker.runtime(backend).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Ok((Vec::new(), e)),
            Err(e) => return Ok((Vec::new(), e.to_owned())),
        };
        let mut cmd: Vec<String> = [
            "bench",
            "-m",
            model_path,
            "-p",
            &n_prompt.text,
            "-n",
            &n_gen.text,
            "-r",
            &reps.text,
            "-o",
            "json",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let depths = cfg::strip(depths);
        if !depths.is_empty() {
            cmd.push("-d".into());
            cmd.push(depths.to_owned());
        }
        cmd.extend(extra.iter().cloned());
        let body = create_body(&runtime, model_path, cmd);
        let id = match docker.run(&body, &runtime).await {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => return Ok((Vec::new(), e)),
            Err(e) => return Ok((Vec::new(), e.to_owned())),
        };
        let outcome = self.watch(sweeps, &id).await;
        // finally: cont.remove(force=True), errors swallowed.
        let _ = docker.remove(&id).await;
        outcome
    }

    async fn watch(&self, sweeps: &Sweeps, id: &str) -> Result<(Vec<Json>, String), Raised> {
        let docker = &sweeps.docker;
        let flat = |r: Result<Result<Vec<u8>, String>, &'static str>| match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(Raised(e)),
            Err(e) => Err(Raised(e.to_owned())),
        };
        let deadline = (self.clock.time)() + SWEEP_TIMEOUT_S;
        loop {
            let status = match docker.status(id).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => return Err(Raised(e)),
                Err(e) => return Err(Raised(e.to_owned())),
            };
            if status != "running" && status != "created" {
                break;
            }
            if self.cancelled() {
                let _ = docker.kill(id).await;
                return Ok((Vec::new(), "cancelled".into()));
            }
            if (self.clock.time)() > deadline {
                let _ = docker.kill(id).await;
                return Ok((Vec::new(), "timed out after 1800s".into()));
            }
            (sweeps.sleep)(Duration::from_secs(1)).await;
        }
        let out = flat(docker.logs(id, true).await)?;
        if let Some(found) = entries(&out) {
            return Ok((found, String::new()));
        }
        let err = flat(docker.logs(id, false).await)?;
        Ok((Vec::new(), stderr_line(&String::from_utf8_lossy(&err))))
    }

    /// `bench.reap_orphans()` at startup: `(reaped, stuck)`. Never fails.
    pub async fn reap_orphans(self: &Arc<Self>) -> (usize, usize) {
        let Some(sweeps) = self.sweeps() else {
            return (0, 0);
        };
        if self.with(|s| s.active()) {
            return (0, 0);
        }
        let found = match sweeps.docker.list_labelled().await {
            Ok(Ok(ids)) => ids,
            Ok(Err(e)) => {
                eprintln!(
                    "benchmark orphan check skipped: {}",
                    e.split(':').next().unwrap_or("")
                );
                return (0, 0);
            }
            Err(_) => return (0, 0),
        };
        let mut reaped = 0;
        let mut stuck = Vec::new();
        for id in found {
            if stop_and_remove(sweeps, &id).await {
                reaped += 1;
            } else {
                stuck.push(id);
            }
        }
        if reaped > 0 {
            eprintln!("removed {reaped} leftover llama-bench container(s) from a previous run");
        }
        let n = stuck.len();
        if n > 0 {
            eprintln!(
                "{n} leftover llama-bench container(s) could not be stopped; reporting the benchmark as active"
            );
            let started = (self.clock.time)();
            self.with(|s| {
                *s = JobState {
                    status: "running".into(),
                    started_at: started,
                    unit: "models".into(),
                    total: 1,
                    current: "stopping a llama-bench left over from before a restart".into(),
                    lines: vec![
                        "a llama-bench container from before a restart is still running; stopping it"
                            .into(),
                    ],
                    ..JobState::default()
                };
            });
            let me = self.clone();
            (self.spawn)(Box::pin(async move { me.watch_orphans(stuck).await }));
        }
        (reaped, n)
    }

    /// `bench._watch_orphans`, bounded at [`ORPHAN_ATTEMPTS`].
    async fn watch_orphans(self: Arc<Self>, mut pending: Vec<String>) {
        let Some(sweeps) = self.sweeps() else {
            return;
        };
        let mut attempts = 0;
        while !pending.is_empty() && attempts < ORPHAN_ATTEMPTS {
            attempts += 1;
            (sweeps.sleep)(ORPHAN_RETRY).await;
            let mut left = Vec::new();
            for id in pending {
                if !stop_and_remove(sweeps, &id).await {
                    left.push(id);
                }
            }
            pending = left;
        }
        let finished = (self.clock.time)();
        let error = if pending.is_empty() {
            "a benchmark was interrupted by a model-manager restart; its leftover container was stopped"
        } else {
            eprintln!(
                "{} leftover llama-bench container(s) still could not be stopped; giving up",
                pending.len()
            );
            "a llama-bench container left over from before a restart could not be stopped; see the model manager log"
        };
        self.with(|s| {
            s.status = "error".into();
            s.finished_at = finished;
            s.error = error.into();
            s.current = String::new();
        });
    }
}

/// `bench._stop_and_remove`: gone (or already gone) is true.
async fn stop_and_remove(sweeps: &Sweeps, id: &str) -> bool {
    match sweeps.docker.remove(id).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => e.starts_with("NotFound:"),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn dumps_follows_python() {
        let v = model_files::json::parse(
            r#"{"b": [1, 2.5, 1e100, NaN, -Infinity, true, null], "a": "é\u0001\"😀", "b": 3}"#,
        )
        .unwrap();
        assert_eq!(
            py_dumps(&v),
            r#"{"a": "\u00e9\u0001\"\ud83d\ude00", "b": 3}"#
        );
        let v = model_files::json::parse(r#"[1, 2.5, 1e100, NaN, -Infinity, true, null, 1e16]"#)
            .unwrap();
        assert_eq!(
            py_dumps(&v),
            "[1, 2.5, 1e+100, NaN, -Infinity, true, null, 1e+16]"
        );
    }

    #[test]
    fn stderr_line_prefers_the_last_error() {
        assert_eq!(
            stderr_line("a\nmain: failed to load model\nbye\n"),
            "main: failed to load model"
        );
        assert_eq!(stderr_line("  \n"), "no output");
        assert_eq!(stderr_line("one\n two \n"), "two");
    }
}
