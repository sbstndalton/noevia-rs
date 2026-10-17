//! noevia#1318 slice 10: the native download job table and the transfers (`downloader.py`'s
//! `DownloadManager`), behind `GET /api/v1/downloads`, `POST /api/v1/downloads`,
//! `POST /api/v1/downloads/{job_id}/cancel`, `POST /api/v1/downloads/clear` and
//! `POST /api/v1/sections/{name}/draft-heads/download`.
//!
//! The five routes move together because they share one in-memory job table. A start is still
//! planned by the Python child (its validation, Hugging Face lookups, URL `HEAD` and destination
//! names): the parent sends the request on with `x-noevia-download-plan: 1`, the child runs the
//! route without queueing and answers `{"status", "body", "jobs"}`, and this side queues `jobs`
//! exactly as `DownloadManager._make_job` would, then answers the route's own status and body.
//!
//! Every place `downloader.py` reads `time.time()`, this reads the injected clock.
mod engine;
pub mod pyfmt;
mod store;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

/// `PARALLEL_CHUNKS`.
pub const PARALLEL_CHUNKS: i64 = 8;
/// `PARALLEL_MIN_SIZE`.
pub const PARALLEL_MIN_SIZE: i64 = 32 * 1024 * 1024;
/// `SPEED_SAMPLE_INTERVAL_S`.
const SPEED_SAMPLE_INTERVAL_S: f64 = 0.5;
/// `SPEED_SAMPLE_MAX`.
const SPEED_SAMPLE_MAX: usize = 120;
/// The planning request header the child recognises (never forwarded from a client).
pub const PLAN_HEADER: &str = "x-noevia-download-plan";
/// The largest planning answer read from the child.
pub const PLAN_CAP: usize = 8 * 1024 * 1024;
/// Most files one start may queue (a sharded repo lists a few dozen).
pub const PLAN_FILES_CAP: usize = 1024;
/// noevia#1960: a body with no declared size (no `Content-Length` on the HEAD or the GET) is
/// abandoned once it passes 64 GiB. Hugging Face refuses files over 50 GB, so no Hub file is
/// cut short; a URL download of an undeclared larger file fails instead of filling the disk.
pub const UNKNOWN_SIZE_CAP: i64 = 64 * 1024 * 1024 * 1024;
/// Most jobs held. Python keeps every job until it is cleared; past this a start answers 503.
pub const JOB_CAP: usize = 4096;

/// The one origin that may receive the Hugging Face token (`hf.is_token_origin`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenOrigin {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl TokenOrigin {
    /// `https://huggingface.co` (port 443).
    pub fn hub() -> Self {
        Self {
            scheme: "https".into(),
            host: "huggingface.co".into(),
            port: 443,
        }
    }

    /// `hf.is_token_origin(url)`.
    pub fn allows(&self, url: &str) -> bool {
        if url.chars().any(char::is_whitespace) {
            return false;
        }
        let Ok(parsed) = reqwest::Url::parse(url) else {
            return false;
        };
        parsed.scheme() == self.scheme
            && parsed.host_str() == Some(self.host.as_str())
            && parsed.port_or_known_default() == Some(self.port)
            && parsed.username().is_empty()
            && parsed.password().is_none()
    }
}

/// Where the native job table writes and what it may send.
pub struct Config {
    /// `settings.models_dir` as configured (`MODELS_DIR`).
    pub models_dir: String,
    /// `DATA_DIR/model_loader.db`, for `db.record_download` and the token.
    pub database: PathBuf,
    /// `settings.max_concurrent_downloads`.
    pub max_concurrent: usize,
    pub token_origin: TokenOrigin,
    /// `PARALLEL_MIN_SIZE` (tests lower it).
    pub parallel_min: i64,
    /// noevia#1960: the most a single-stream body without a declared size may write
    /// ([`UNKNOWN_SIZE_CAP`]; tests lower it).
    pub unknown_cap: i64,
    /// `time.time()`.
    pub clock: fn() -> f64,
}

fn wall() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

impl Config {
    pub fn production(models_dir: String, data_dir: &str, max_concurrent: usize) -> Self {
        Self {
            models_dir,
            database: PathBuf::from(data_dir).join("model_loader.db"),
            max_concurrent,
            token_origin: TokenOrigin::hub(),
            parallel_min: PARALLEL_MIN_SIZE,
            unknown_cap: UNKNOWN_SIZE_CAP,
            clock: wall,
        }
    }
}

/// `ChunkState`.
#[derive(Debug, Clone)]
struct Chunk {
    idx: i64,
    start: i64,
    end: i64,
    downloaded: i64,
    samples: Vec<(f64, i64)>,
}

impl Chunk {
    fn size(&self) -> i64 {
        self.end - self.start + 1
    }
    fn status(&self) -> &'static str {
        if self.downloaded >= self.size() {
            "done"
        } else if self.downloaded == 0 {
            "pending"
        } else {
            "downloading"
        }
    }
    #[allow(clippy::cast_precision_loss)]
    fn pct(&self) -> f64 {
        let s = self.size();
        if s != 0 {
            (self.downloaded as f64 / s as f64 * 100.0).min(100.0)
        } else {
            0.0
        }
    }
}

/// The mutable part of `DownloadJob`.
#[derive(Debug, Clone)]
struct State {
    status: &'static str,
    error: Option<String>,
    total: i64,
    downloaded: i64,
    started_at: Option<f64>,
    completed_at: Option<f64>,
    chunks: Vec<Chunk>,
    parallel: bool,
    samples: Vec<(f64, i64)>,
}

/// `DownloadJob`.
pub struct Job {
    id: String,
    repo_id: String,
    filename: String,
    url: String,
    dest: String,
    temp: String,
    state: Mutex<State>,
    cancel: tokio::sync::watch::Sender<bool>,
    /// noevia#1959: the directory handle the transfer opened (set once it starts).
    place: std::sync::OnceLock<Arc<engine::Place>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `(b1 - b0) / dt` over the samples within 5 s of `now`, else 0.0.
#[allow(clippy::cast_precision_loss)]
fn speed(samples: &[(f64, i64)], now: f64) -> f64 {
    let window: Vec<&(f64, i64)> = samples.iter().filter(|(t, _)| now - t <= 5.0).collect();
    let (Some((t0, b0)), Some((t1, b1))) = (window.first(), window.last()) else {
        return 0.0;
    };
    if window.len() < 2 {
        return 0.0;
    }
    let dt = t1 - t0;
    if dt > 0.0 {
        (b1 - b0) as f64 / dt
    } else {
        0.0
    }
}

fn speed_h(s: f64) -> String {
    if s > 0.0 {
        format!("{}/s", pyfmt::human_bytes(s.trunc()))
    } else {
        "—".into()
    }
}

fn opt_float(v: Option<f64>) -> String {
    v.map_or_else(|| "null".into(), pyfmt::float)
}

impl State {
    #[allow(clippy::cast_precision_loss)]
    fn pct(&self) -> f64 {
        if self.total <= 0 {
            return 0.0;
        }
        (100.0 * self.downloaded as f64 / self.total as f64).min(100.0)
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn eta(&self, s: f64) -> Option<i64> {
        if s <= 0.0 || self.total <= 0 {
            return None;
        }
        let remaining = (self.total - self.downloaded) as f64;
        Some(((remaining / s).trunc() as i64).max(0))
    }

    /// `speed_history_mbps[-60:]`.
    #[allow(clippy::cast_precision_loss)]
    fn history(&self) -> Vec<f64> {
        let mut out = Vec::new();
        for pair in self.samples.windows(2) {
            if let [(t0, b0), (t1, b1)] = pair {
                let dt = t1 - t0;
                if dt > 0.0 {
                    out.push((b1 - b0) as f64 / dt / (1024.0 * 1024.0));
                }
            }
        }
        let skip = out.len().saturating_sub(60);
        out.split_off(skip)
    }

    /// `_note_progress(job, delta)`.
    fn progress(&mut self, delta: i64, now: f64) {
        self.downloaded += delta;
        if self
            .samples
            .last()
            .is_none_or(|(t, _)| now - t >= SPEED_SAMPLE_INTERVAL_S)
        {
            self.samples.push((now, self.downloaded));
            if self.samples.len() > SPEED_SAMPLE_MAX {
                let cut = self.samples.len() - SPEED_SAMPLE_MAX;
                self.samples.drain(..cut);
            }
        }
    }

    /// `_restart(job)`.
    fn restart(&mut self, now: f64) {
        self.downloaded = 0;
        self.samples = vec![(now, 0)];
    }
}

impl Job {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn active(&self) -> bool {
        matches!(self.state().status, "queued" | "downloading")
    }

    /// `api._job(j)` as FastAPI writes it.
    fn render(&self, now: f64) -> String {
        let s = self.state().clone();
        let sp = speed(&s.samples, now);
        let eta = s.eta(sp);
        let history: Vec<String> = s.history().into_iter().map(pyfmt::float).collect();
        let chunks: Vec<String> = s
            .chunks
            .iter()
            .map(|c| {
                format!(
                    "{{\"index\":{},\"pct\":{},\"status\":{},\"speed\":{}}}",
                    c.idx,
                    pyfmt::float(pyfmt::round1(c.pct())),
                    pyfmt::string(c.status()),
                    pyfmt::float(speed(&c.samples, now))
                )
            })
            .collect();
        format!(
            "{{\"id\":{},\"repo\":{},\"filename\":{},\"status\":{},\"error\":{},\"bytes\":{},\"downloaded\":{},\"pct\":{},\"speed\":{},\"speedH\":{},\"eta\":{},\"etaH\":{},\"startedAt\":{},\"completedAt\":{},\"parallel\":{},\"history\":[{}],\"chunks\":[{}]}}",
            pyfmt::string(&self.id),
            pyfmt::string(&self.repo_id),
            pyfmt::string(&self.filename),
            pyfmt::string(s.status),
            s.error.as_deref().map_or_else(|| "null".into(), pyfmt::string),
            s.total,
            s.downloaded,
            pyfmt::float(pyfmt::round1(s.pct())),
            pyfmt::float(sp),
            pyfmt::string(&speed_h(sp)),
            eta.map_or_else(|| "null".into(), |e| e.to_string()),
            pyfmt::string(&pyfmt::eta_h(eta)),
            opt_float(s.started_at),
            opt_float(s.completed_at),
            s.parallel,
            history.join(","),
            chunks.join(",")
        )
    }
}

/// One file the child's plan would have queued (`_make_job`'s arguments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub repo_id: String,
    pub filename: String,
    pub url: String,
    pub total_bytes: i64,
}

/// The child's planning answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub status: u16,
    pub body: String,
    pub jobs: Vec<Planned>,
}

/// Reads `{"status", "body", "jobs"}`; `None` for anything else.
pub fn parse_plan(bytes: &[u8]) -> Option<Plan> {
    if bytes.len() > PLAN_CAP {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let object = value.as_object()?;
    let status = u16::try_from(object.get("status")?.as_u64()?).ok()?;
    if !(100..=599).contains(&status) {
        return None;
    }
    let body = object.get("body")?.as_str()?.to_owned();
    let list = object.get("jobs")?.as_array()?;
    if list.len() > PLAN_FILES_CAP || (status != 200 && !list.is_empty()) {
        return None;
    }
    let mut jobs = Vec::with_capacity(list.len());
    for job in list {
        let job = job.as_object()?;
        let text = |key: &str| job.get(key)?.as_str().map(str::to_owned);
        jobs.push(Planned {
            repo_id: text("repo_id")?,
            filename: text("filename")?,
            url: text("url")?,
            total_bytes: job.get("total_bytes")?.as_i64()?,
        });
    }
    Some(Plan { status, body, jobs })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Queued {
    Done,
    /// `safe_dest` raised (Python's 500).
    Raised,
    /// More than [`JOB_CAP`] jobs would be held.
    Full,
}

/// The job table (`DownloadManager`).
pub struct Downloads {
    config: Config,
    jobs: Mutex<Vec<Arc<Job>>>,
    permits: Arc<tokio::sync::Semaphore>,
    http: engine::Http,
    ids: std::collections::hash_map::RandomState,
    serial: std::sync::atomic::AtomicU64,
}

impl Downloads {
    pub fn new(config: Config) -> Result<Arc<Self>, &'static str> {
        if config.max_concurrent == 0 {
            return Err("max concurrent downloads must be at least 1");
        }
        Ok(Arc::new(Self {
            permits: Arc::new(tokio::sync::Semaphore::new(config.max_concurrent)),
            http: engine::Http::new()?,
            config,
            jobs: Mutex::new(Vec::new()),
            ids: std::collections::hash_map::RandomState::new(),
            serial: std::sync::atomic::AtomicU64::new(0),
        }))
    }

    fn now(&self) -> f64 {
        (self.config.clock)()
    }

    /// `uuid.uuid4().hex[:12]`: 48 unpredictable bits per process, unique in the table.
    fn new_id(&self, jobs: &[Arc<Job>]) -> String {
        use std::hash::BuildHasher;
        loop {
            let n = self
                .serial
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let hash = self.ids.hash_one((n, std::process::id()));
            let id = format!("{:012x}", hash & 0xffff_ffff_ffff);
            if !jobs.iter().any(|j| j.id == id) {
                return id;
            }
        }
    }

    /// `manager.snapshot()`: active first, then the most recent.
    fn snapshot(&self) -> Vec<Arc<Job>> {
        let mut jobs: Vec<Arc<Job>> = lock(&self.jobs).clone();
        let key = |j: &Arc<Job>| {
            let s = j.state();
            let rank = match s.status {
                "downloading" => 0,
                "queued" => 1,
                _ => 2,
            };
            let when = s
                .completed_at
                .filter(|t| *t != 0.0)
                .or(s.started_at.filter(|t| *t != 0.0))
                .unwrap_or(0.0);
            (rank, -when)
        };
        jobs.sort_by(|a, b| {
            let (ra, wa) = key(a);
            let (rb, wb) = key(b);
            ra.cmp(&rb)
                .then(wa.partial_cmp(&wb).unwrap_or(std::cmp::Ordering::Equal))
        });
        jobs
    }

    /// `GET /api/v1/downloads`: `{"jobs": [_job(j) for j in manager.snapshot()]}`.
    pub fn list(&self) -> String {
        let now = self.now();
        let jobs: Vec<String> = self.snapshot().iter().map(|j| j.render(now)).collect();
        format!("{{\"jobs\":[{}]}}", jobs.join(","))
    }

    /// The `activeDownloads` count `GET /api/v1/overview` reports.
    pub fn active(&self) -> usize {
        lock(&self.jobs).iter().filter(|j| j.active()).count()
    }

    /// noevia#1966: the destinations of queued and running jobs relative to the models folder
    /// (`safe_dest` keeps them as joined, so this is the file name as planned, normalised).
    pub fn active_destinations(&self) -> Vec<String> {
        let prefix = format!("{}/", self.config.models_dir.trim_end_matches('/'));
        lock(&self.jobs)
            .iter()
            .filter(|j| j.active())
            .filter_map(|j| j.dest.strip_prefix(&prefix).map(str::to_owned))
            .collect()
    }

    /// `manager.cancel(job_id)`.
    pub fn cancel(&self, id: &str) -> bool {
        let job = lock(&self.jobs).iter().find(|j| j.id == id).cloned();
        match job {
            Some(job) if job.active() => {
                job.cancel.send_replace(true);
                true
            }
            _ => false,
        }
    }

    /// `manager.clear_finished()`.
    pub fn clear(&self) -> usize {
        let mut jobs = lock(&self.jobs);
        let before = jobs.len();
        jobs.retain(|j| !matches!(j.state().status, "done" | "error" | "canceled"));
        before - jobs.len()
    }

    /// `_make_job` for every planned file, in order. A file whose destination already has a
    /// queued or running job is not queued again.
    ///
    /// noevia#1958: all-or-nothing. Every destination is resolved and the cap is checked for the
    /// whole plan before anything is inserted, so a start refused part-way (`Raised` for a later
    /// file, or `Full`) leaves the table exactly as it was instead of orphaning `queued` jobs
    /// that never get a worker.
    pub fn queue(self: &Arc<Self>, planned: Vec<Planned>) -> Queued {
        let mut started = Vec::new();
        {
            let mut jobs = lock(&self.jobs);
            let mut fresh: Vec<(Planned, String)> = Vec::new();
            for p in planned {
                let dest = match pyfmt::safe_dest(&self.config.models_dir, &p.filename) {
                    pyfmt::Dest::Inside(dest) => dest,
                    pyfmt::Dest::Outside | pyfmt::Dest::Raised => return Queued::Raised,
                };
                if jobs.iter().any(|j| j.dest == dest && j.active())
                    || fresh.iter().any(|(_, d)| *d == dest)
                {
                    continue;
                }
                fresh.push((p, dest));
            }
            if jobs.len().saturating_add(fresh.len()) > JOB_CAP {
                return Queued::Full;
            }
            for (p, dest) in fresh {
                let job = Arc::new(Job {
                    id: self.new_id(&jobs),
                    repo_id: p.repo_id,
                    filename: p.filename,
                    url: p.url,
                    temp: pyfmt::temp_path(&dest),
                    dest,
                    state: Mutex::new(State {
                        status: "queued",
                        error: None,
                        total: p.total_bytes,
                        downloaded: 0,
                        started_at: None,
                        completed_at: None,
                        chunks: Vec::new(),
                        parallel: false,
                        samples: Vec::new(),
                    }),
                    cancel: tokio::sync::watch::channel(false).0,
                    place: std::sync::OnceLock::new(),
                });
                jobs.push(job.clone());
                started.push(job);
            }
        }
        for job in started {
            let me = self.clone();
            tokio::spawn(async move { me.run(job).await });
        }
        Queued::Done
    }
}

/// `GET /api/v1/overview` comes from the child, whose own job table is empty while this side
/// owns downloads: put this side's `activeDownloads` into its answer. The key is replaced in the
/// bytes (the rest of the body is left exactly as Python wrote it), and only when it occurs once,
/// as a top-level integer.
pub fn patch_overview(body: &[u8], active: usize) -> Option<Vec<u8>> {
    const KEY: &[u8] = b"\"activeDownloads\":";
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value.as_object()?.get("activeDownloads")?.as_u64()?;
    let mut found = body
        .windows(KEY.len())
        .enumerate()
        .filter(|(_, w)| *w == KEY);
    let (at, _) = found.next()?;
    if found.next().is_some() {
        return None;
    }
    let start = at + KEY.len();
    let digits = body
        .get(start..)?
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(body.get(..start)?);
    out.extend_from_slice(active.to_string().as_bytes());
    out.extend_from_slice(body.get(start + digits..)?);
    Some(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../tests/fixtures/downloads-python.v1.json")).unwrap()
    }

    fn samples(v: &serde_json::Value) -> Vec<(f64, i64)> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_f64().unwrap(), p[1].as_i64().unwrap()))
            .collect()
    }

    /// `api._job(j)` for crafted jobs with `time.time()` pinned, byte for byte.
    #[test]
    #[allow(clippy::indexing_slicing)]
    fn rendering_matches_python() {
        let fixture = fixture();
        let now = fixture["render"]["now"].as_f64().unwrap();
        for (i, case) in fixture["render"]["cases"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let spec = &case["spec"];
            let status: &'static str = match spec["status"].as_str().unwrap() {
                "queued" => "queued",
                "downloading" => "downloading",
                "done" => "done",
                "error" => "error",
                other => panic!("{other}"),
            };
            let chunks = spec
                .get("chunks")
                .and_then(|c| c.as_array())
                .map(|list| {
                    list.iter()
                        .map(|c| Chunk {
                            idx: c["idx"].as_i64().unwrap(),
                            start: c["start"].as_i64().unwrap(),
                            end: c["end"].as_i64().unwrap(),
                            downloaded: c["downloaded"].as_i64().unwrap(),
                            samples: samples(&c["samples"]),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let job = Job {
                id: format!("render{i:06}"),
                repo_id: "synthetic-owner/r".into(),
                filename: format!("f{i}.gguf"),
                url: "http://x/f".into(),
                dest: "/m/f".into(),
                temp: "/m/f.download".into(),
                state: Mutex::new(State {
                    status,
                    error: spec["error"].as_str().map(str::to_owned),
                    total: spec["total"].as_i64().unwrap(),
                    downloaded: spec["downloaded"].as_i64().unwrap(),
                    started_at: spec["started"].as_f64(),
                    completed_at: spec["completed"].as_f64(),
                    chunks,
                    parallel: spec["parallel"].as_bool().unwrap_or(false),
                    samples: samples(&spec["samples"]),
                }),
                cancel: tokio::sync::watch::channel(false).0,
                place: std::sync::OnceLock::new(),
            };
            assert_eq!(
                job.render(now),
                case["json"].as_str().unwrap(),
                "render case {i}"
            );
        }
    }

    /// `_validator(headers)` and `_http_date(value)` against Python.
    #[test]
    fn validators_and_dates_match_python() {
        let fixture = fixture();
        let v = &fixture["validator"];
        let now = v["now"].as_f64().unwrap();
        for case in v["validators"].as_array().unwrap() {
            let headers = case["headers"].as_object().unwrap().clone();
            let get = |name: &str| {
                headers
                    .get(name)
                    .and_then(|x| x.as_str())
                    .map(str::to_owned)
            };
            assert_eq!(
                pyfmt::validator(&get, now),
                case["validator"].as_str().unwrap(),
                "{headers:?}"
            );
        }
        for case in v["dates"].as_array().unwrap() {
            let value = case["value"].as_str().unwrap();
            assert_eq!(
                pyfmt::http_date(Some(value)),
                case["time"].as_f64(),
                "{value:?}"
            );
        }
    }

    #[test]
    fn the_overview_count_is_replaced_in_place() {
        let body = br#"{"models":3,"activeDownloads":0,"disk":{"free":1.5}}"#;
        assert_eq!(
            patch_overview(body, 2).unwrap(),
            br#"{"models":3,"activeDownloads":2,"disk":{"free":1.5}}"#.to_vec()
        );
        // A key inside a string is not the key, and a nested or doubled key is left alone.
        assert!(patch_overview(br#"{"x":"\"activeDownloads\":1"}"#, 2).is_none());
        assert!(patch_overview(br#"{"a":{"activeDownloads":1}}"#, 2).is_none());
        assert!(patch_overview(br#"{"activeDownloads":null}"#, 2).is_none());
    }

    #[test]
    fn plans_are_read_strictly() {
        let ok = br#"{"status":200,"body":"{\"queued\":[\"a\"]}","jobs":[{"repo_id":"o/r","filename":"a/a.gguf","url":"https://example.test/a","total_bytes":5}]}"#;
        assert_eq!(parse_plan(ok).unwrap().jobs.len(), 1);
        for bad in [
            &br#"{"status":400,"body":"x","jobs":[{"repo_id":"o/r","filename":"a","url":"u","total_bytes":5}]}"#[..],
            br#"{"status":200,"body":"x","jobs":[{"repo_id":"o/r","filename":"a","url":"u","total_bytes":"5"}]}"#,
            br#"{"status":99,"body":"x","jobs":[]}"#,
            br#"{"status":200,"jobs":[]}"#,
            b"[]",
        ] {
            assert!(parse_plan(bad).is_none(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn token_origin_is_exact() {
        let hub = TokenOrigin::hub();
        assert!(hub.allows("https://huggingface.co/o/r/resolve/main/x.gguf"));
        assert!(hub.allows("https://HuggingFace.co:443/x"));
        for url in [
            "http://huggingface.co/x",
            "https://huggingface.co:8443/x",
            "https://cdn.huggingface.co/x",
            "https://user@huggingface.co/x",
            "https://huggingface.co/a b",
            "https://huggingface.co.example.test/x",
        ] {
            assert!(!hub.allows(url), "{url}");
        }
    }

    // ------------------------------------------------ review of slice 10 (#1958 #1959 #1960)

    fn table(models: &std::path::Path, unknown_cap: i64, max_concurrent: usize) -> Arc<Downloads> {
        Downloads::new(Config {
            models_dir: models.to_str().unwrap().to_owned(),
            database: models.join("absent.db"),
            max_concurrent,
            token_origin: TokenOrigin::hub(),
            parallel_min: PARALLEL_MIN_SIZE,
            unknown_cap,
            clock: wall,
        })
        .unwrap()
    }

    fn planned(filename: &str, url: &str, total_bytes: i64) -> Planned {
        Planned {
            repo_id: "example.test".into(),
            filename: filename.into(),
            url: url.into(),
            total_bytes,
        }
    }

    fn filler(n: usize) -> Arc<Job> {
        Arc::new(Job {
            id: format!("{n:012x}"),
            repo_id: "example.test".into(),
            filename: format!("filler-{n}.gguf"),
            url: "http://127.0.0.1:9/x".into(),
            dest: format!("/synthetic/filler-{n}.gguf"),
            temp: format!("/synthetic/filler-{n}.gguf.download"),
            state: Mutex::new(State {
                status: "done",
                error: None,
                total: 1,
                downloaded: 1,
                started_at: None,
                completed_at: None,
                chunks: Vec::new(),
                parallel: false,
                samples: Vec::new(),
            }),
            cancel: tokio::sync::watch::channel(false).0,
            place: std::sync::OnceLock::new(),
        })
    }

    fn ids(t: &Downloads) -> Vec<String> {
        lock(&t.jobs).iter().map(|j| j.id.clone()).collect()
    }

    /// noevia#1958: a start refused part-way leaves the table as it was.
    #[test]
    fn a_refused_start_queues_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().canonicalize().unwrap().join("models");
        std::fs::create_dir(&models).unwrap();
        let t = table(&models, UNKNOWN_SIZE_CAP, 1);
        // Room for one more job, a plan of two.
        lock(&t.jobs).extend((0..JOB_CAP - 1).map(filler));
        let before = ids(&t);
        let plan = vec![
            planned("a.gguf", "http://127.0.0.1:9/a", 1),
            planned("b.gguf", "http://127.0.0.1:9/b", 1),
        ];
        assert_eq!(t.queue(plan), Queued::Full);
        assert_eq!(ids(&t), before);
        // A later file that escapes the models folder.
        lock(&t.jobs).clear();
        let plan = vec![
            planned("a.gguf", "http://127.0.0.1:9/a", 1),
            planned("../x", "http://127.0.0.1:9/x", 1),
        ];
        assert_eq!(t.queue(plan), Queued::Raised);
        assert!(ids(&t).is_empty());
        assert_eq!(t.active(), 0);
        // The same file twice in one plan is one job (as before), and fits exactly.
        lock(&t.jobs).extend((0..JOB_CAP - 1).map(filler));
        let plan = vec![
            planned("dup.gguf", "http://127.0.0.1:9/a", 1),
            planned("dup.gguf", "http://127.0.0.1:9/a", 1),
        ];
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async { assert_eq!(t.queue(plan), Queued::Done) });
        assert_eq!(lock(&t.jobs).len(), JOB_CAP);
    }

    /// A local server: every request's head goes to `answer`, whose bytes are written.
    async fn server(answer: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let answer = Arc::new(answer);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let answer = answer.clone();
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => data.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&data).into_owned();
                    let _ = sock.write_all(&answer(&head)).await;
                });
            }
        });
        port
    }

    async fn settle(t: &Downloads) -> Vec<serde_json::Value> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let list: serde_json::Value = serde_json::from_str(&t.list()).unwrap();
            let jobs = list["jobs"].as_array().unwrap().clone();
            if jobs
                .iter()
                .all(|j| !matches!(j["status"].as_str(), Some("queued" | "downloading")))
            {
                return jobs;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "jobs never settled: {jobs:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn small(head: &str) -> Vec<u8> {
        let body = if head.starts_with("HEAD ") {
            ""
        } else {
            "0123456789"
        };
        format!("HTTP/1.1 200 OK\r\ncontent-length: 10\r\nconnection: close\r\n\r\n{body}")
            .into_bytes()
    }

    /// noevia#1959: a `.download` (or validator) symlink planted inside the models folder is
    /// never written through; the file it points at is untouched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn planted_temp_symlinks_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let models = root.join("models");
        std::fs::create_dir(&models).unwrap();
        let outside = root.join("outside.bin");
        std::fs::write(&outside, b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, models.join("a.gguf.download")).unwrap();
        std::os::unix::fs::symlink(&outside, models.join("b.gguf.download.validator")).unwrap();
        let port = server(small).await;
        let t = table(&models, UNKNOWN_SIZE_CAP, 2);
        let url = format!("http://127.0.0.1:{port}/f");
        assert_eq!(
            t.queue(vec![
                planned("a.gguf", &url, 10),
                planned("b.gguf", &url, 10)
            ]),
            Queued::Done
        );
        let jobs = settle(&t).await;
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        let a = jobs.iter().find(|j| j["filename"] == "a.gguf").unwrap();
        assert_eq!(a["status"], "error", "{a}");
        assert!(!models.join("a.gguf").exists());
        // The validator link is replaced only by unlinking it, never written through.
        let b = jobs.iter().find(|j| j["filename"] == "b.gguf").unwrap();
        assert_eq!(b["status"], "done", "{b}");
        assert_eq!(std::fs::read(models.join("b.gguf")).unwrap(), b"0123456789");
        assert!(std::fs::symlink_metadata(models.join("b.gguf.download.validator")).is_err());
    }

    /// noevia#1959: a folder swapped for a symlink while the job waited for a slot is caught
    /// when the transfer starts; nothing is created outside.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_folder_swapped_while_queued_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let models = root.join("models");
        let away = root.join("away");
        std::fs::create_dir_all(models.join("sub")).unwrap();
        std::fs::create_dir(&away).unwrap();
        // One slot, held by a job whose server never answers.
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hang = silent.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = silent.accept().await {
                held.push(sock);
            }
        });
        let port = server(small).await;
        let t = table(&models, UNKNOWN_SIZE_CAP, 1);
        assert_eq!(
            t.queue(vec![planned(
                "hold.gguf",
                &format!("http://127.0.0.1:{hang}/h"),
                1
            )]),
            Queued::Done
        );
        assert_eq!(
            t.queue(vec![planned(
                "sub/x.gguf",
                &format!("http://127.0.0.1:{port}/x"),
                10
            )]),
            Queued::Done
        );
        std::fs::remove_dir(models.join("sub")).unwrap();
        std::os::unix::fs::symlink(&away, models.join("sub")).unwrap();
        let first = lock(&t.jobs)[0].id.clone();
        assert!(t.cancel(&first));
        let jobs = settle(&t).await;
        let x = jobs.iter().find(|j| j["filename"] == "sub/x.gguf").unwrap();
        assert_eq!(x["status"], "error");
        assert_eq!(
            x["error"],
            "ValueError: download destination is outside the models folder"
        );
        assert_eq!(std::fs::read_dir(&away).unwrap().count(), 0);
    }

    /// noevia#1967: the same with two levels below the swapped folder (`safe_dest` refuses it
    /// first here; `make_parents_never_creates_through_an_outside_link` covers a swap that comes
    /// after that check).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_swapped_folder_gets_no_new_levels_through_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let models = root.join("models");
        let away = root.join("away");
        std::fs::create_dir_all(models.join("sub")).unwrap();
        std::fs::create_dir(&away).unwrap();
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hang = silent.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = silent.accept().await {
                held.push(sock);
            }
        });
        let port = server(small).await;
        let t = table(&models, UNKNOWN_SIZE_CAP, 1);
        assert_eq!(
            t.queue(vec![planned(
                "hold.gguf",
                &format!("http://127.0.0.1:{hang}/h"),
                1
            )]),
            Queued::Done
        );
        assert_eq!(
            t.queue(vec![planned(
                "sub/deeper/x.gguf",
                &format!("http://127.0.0.1:{port}/x"),
                10
            )]),
            Queued::Done
        );
        std::fs::remove_dir(models.join("sub")).unwrap();
        std::os::unix::fs::symlink(&away, models.join("sub")).unwrap();
        let first = lock(&t.jobs)[0].id.clone();
        assert!(t.cancel(&first));
        let jobs = settle(&t).await;
        let x = jobs
            .iter()
            .find(|j| j["filename"] == "sub/deeper/x.gguf")
            .unwrap();
        assert_eq!(x["status"], "error");
        assert_eq!(
            x["error"],
            "ValueError: download destination is outside the models folder"
        );
        assert_eq!(
            std::fs::read_dir(&away).unwrap().count(),
            0,
            "away/ stays empty"
        );
    }

    /// noevia#1967: the folder swapped for a link between `safe_dest` and the creation of the
    /// parents. `create_dir_all` made `away/deeper` there; the handle walk creates nothing.
    #[test]
    fn make_parents_never_creates_through_an_outside_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let models = root.join("models");
        let away = root.join("away");
        std::fs::create_dir(&models).unwrap();
        std::fs::create_dir(&away).unwrap();
        std::os::unix::fs::symlink(&away, models.join("sub")).unwrap();
        let m = models.to_str().unwrap();
        let dest = format!("{m}/sub/deeper/x.gguf");
        assert!(!engine::make_parents(m, &dest).unwrap());
        assert_eq!(
            std::fs::read_dir(&away).unwrap().count(),
            0,
            "away/ stays empty"
        );
        // A link at the second level, and a `..` that would climb above the models folder.
        std::fs::create_dir(models.join("real")).unwrap();
        std::os::unix::fs::symlink(&away, models.join("real/link")).unwrap();
        assert!(!engine::make_parents(m, &format!("{m}/real/link/deeper/x.gguf")).unwrap());
        assert!(!engine::make_parents(m, &format!("{m}/real/../../escape/x.gguf")).unwrap());
        assert_eq!(std::fs::read_dir(&away).unwrap().count(), 0);
        assert!(!root.join("escape").exists());
        // What the old full-path creation did with the same link.
        std::fs::create_dir_all(models.join("sub/deeper")).unwrap();
        assert!(away.join("deeper").is_dir());
    }

    #[test]
    fn make_parents_creates_levels_and_reports_errors() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().canonicalize().unwrap().join("models");
        std::fs::create_dir(&models).unwrap();
        let m = models.to_str().unwrap();
        assert!(engine::make_parents(m, &format!("{m}/flat.gguf")).unwrap());
        assert!(engine::make_parents(m, &format!("{m}/a/./b/../c/x.gguf")).unwrap());
        assert!(models.join("a/b").is_dir() && models.join("a/c").is_dir());
        assert!(engine::make_parents(m, &format!("{m}/a/c/x.gguf")).unwrap()); // exist_ok
        std::fs::write(models.join("file"), b"").unwrap();
        let e = engine::make_parents(m, &format!("{m}/file/x/y.gguf")).unwrap_err();
        assert!(
            matches!(e.raw_os_error(), Some(n) if n == libc_enotdir()),
            "{e}"
        );
    }

    fn libc_enotdir() -> i32 {
        rustix::io::Errno::NOTDIR.raw_os_error()
    }

    /// noevia#1967: new levels are created one at a time, and a link that stays inside the
    /// models folder is still followed (as `mkdir(parents=True)` would).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nested_levels_are_created_and_inside_links_followed() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().canonicalize().unwrap().join("models");
        std::fs::create_dir_all(models.join("real")).unwrap();
        std::os::unix::fs::symlink(models.join("real"), models.join("alias")).unwrap();
        let port = server(small).await;
        let t = table(&models, UNKNOWN_SIZE_CAP, 2);
        assert_eq!(
            t.queue(vec![
                planned("a/b/c/x.gguf", &format!("http://127.0.0.1:{port}/x"), 10),
                planned("alias/d/y.gguf", &format!("http://127.0.0.1:{port}/x"), 10),
            ]),
            Queued::Done
        );
        let jobs = settle(&t).await;
        assert!(jobs.iter().all(|j| j["status"] == "done"), "{jobs:?}");
        assert!(models.join("a/b/c/x.gguf").is_file());
        assert!(models.join("real/d/y.gguf").is_file());
    }

    /// noevia#1960: a single-stream body past its declared size stops (it is not written to
    /// the end), and one with no declared size stops at the cap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn single_stream_bodies_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().canonicalize().unwrap().join("models");
        std::fs::create_dir(&models).unwrap();
        let endless = server_endless().await;
        let t = table(&models, 3 * 1024 * 1024, 2);
        assert_eq!(
            t.queue(vec![
                planned(
                    "declared.gguf",
                    &format!("http://127.0.0.1:{endless}/declared"),
                    100
                ),
                planned("open.gguf", &format!("http://127.0.0.1:{endless}/open"), 0),
            ]),
            Queued::Done
        );
        let jobs = settle(&t).await;
        let declared = jobs
            .iter()
            .find(|j| j["filename"] == "declared.gguf")
            .unwrap();
        assert_eq!(declared["status"], "error");
        let error = declared["error"].as_str().unwrap();
        assert!(
            error.starts_with("RuntimeError: downloaded file is over ")
                && error.ends_with(" bytes, expected 100; discarded it, nothing was installed"),
            "{error}"
        );
        let open = jobs.iter().find(|j| j["filename"] == "open.gguf").unwrap();
        assert_eq!(
            open["error"],
            "RuntimeError: download passed 3145728 bytes without a declared size; discarded it, nothing was installed"
        );
        assert_eq!(std::fs::read_dir(&models).unwrap().count(), 0);
    }

    /// HEAD: 100 bytes for `/declared`, 404 otherwise; GET: a close-delimited endless body.
    async fn server_endless() -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => data.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&data).into_owned();
                    if head.starts_with("HEAD ") {
                        let reply: &[u8] = if head.contains(" /declared ") {
                            b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\n"
                        } else {
                            b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        };
                        let _ = sock.write_all(reply).await;
                        return;
                    }
                    if sock
                        .write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n")
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let zeros = vec![0u8; 64 * 1024];
                    while sock.write_all(&zeros).await.is_ok() {}
                });
            }
        });
        port
    }
}
