//! The model folder sync worker (Core `server/model-folder-sync.cjs`, moved).
//!
//! A GGUF that appears in the models folder becomes a usable model on its own: this asks the model
//! manager for the files with no settings, registers each with safe defaults (the manager writes
//! `models.ini` under its own lock and answers with the new revision) and reloads the engine's
//! presets once. Tuning and context measurement stay separate and optional.
//!
//! Conservative as Node was: a file the manager refuses (400 a draft head, 409 a name or file already
//! taken, 404 gone) is remembered in `UI_DATA_DIR/model-folder-sync.json` (`{"skip":[..]}`, the last
//! 500) and never retried; any other failure is retried on the next tick. Registration never unloads a
//! running model: the reload is the guarded one (`Maint::reload_inner`, nothing is unloaded).
//!
//! What is stricter than Node, on purpose (the brief for the move; recorded in the oracle's
//! `intentional` list):
//! - The whole batch runs under ONE exclusion ([`Hold`]). It is checked before every registration
//!   and before the reload; a lost or unconfirmable exclusion ends the batch: nothing more is
//!   registered or reloaded and the exclusion is never taken again within the run.
//! - The scan is repeated under the exclusion, so a file another writer already took, or one that
//!   went away, is not acted on from a stale list.
//! - The reload is fenced by the `models.ini` revision the last registration answered with; a file
//!   that changed since belongs to whoever changed it, and its owner reloads.
//! - A file is registered only when it is settled ([`files`]): a regular `.gguf` inside the models
//!   root (no symlink is followed), no download temp beside it, old enough, a GGUF header, and
//!   unchanged across a short probe.
//!
//! Every dependency is a narrow trait ([`Env`], [`Hold`]); [`live`] is the production implementation
//! on `Maint` (the inference gate's maintenance hold, or a lease on Node's own gate) and the model
//! management service. See `docs/model-folder-sync.md` for the interfaces the in-flight maintenance
//! lease authority (J), load path (A) and auto-tune (K) should replace them with.

pub mod files;
pub mod live;

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The pairing marker: `FEATURES` carries it, Core's `rust-model-folder-sync.cjs` names it.
pub const MARKER: &str = "rust-model-folder-sync-v1";
/// Statuses the manager answers a file with that mean "never retry" (Node `RETRY_NEVER`).
const RETRY_NEVER: [u16; 3] = [400, 409, 404];
/// The skip list kept on disk.
const KEEP: usize = 500;
/// Node: first scan 20 s after start, then every 15 minutes.
pub const FIRST_DELAY: Duration = Duration::from_secs(20);
pub const INTERVAL: Duration = Duration::from_secs(15 * 60);
/// The longest one registration may take (the manager call times out at 120 s).
pub const REGISTER_LIMIT: Duration = Duration::from_secs(120);

/// A failed call to the model management service: its HTTP status when it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    pub status: Option<u16>,
    pub message: String,
}

/// What a registration answered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Registered {
    /// The `models.ini` revision the manager reports after its write.
    pub revision: Option<String>,
}

/// Why no exclusion was granted. Both are transient: nothing was changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Someone else holds the engine.
    Busy,
    /// The arbiter did not answer or could not grant it (fail closed).
    Unavailable,
}

/// Whether a file may be registered yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    Yes,
    /// Not yet (a partial download, still changing, no header yet): looked at again next tick.
    Not(String),
}

/// The result of the guarded reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reloaded {
    Yes,
    /// Refused or failed: the engine offers the models after its next reload.
    No,
}

/// An exclusion on the engine's configuration. Dropping it gives it back (it must be safe to drop at
/// any await point); [`Hold::release`] gives it back and waits.
pub trait Hold: Send + Sync + 'static {
    /// Still excluding everyone else, as last confirmed. False once lost or when it cannot be
    /// confirmed. The worker stops all writes the moment this is false and never takes another.
    fn excluded(&self) -> bool;
    /// May a step of at most `step` start without the exclusion ending under it?
    fn may_start(&self, step: Duration) -> bool;
    fn release(self) -> impl Future<Output = ()> + Send;
}

/// Everything the worker needs from outside.
pub trait Env: Send + Sync + 'static {
    type Hold: Hold;
    /// Calibration or auto-tune of this process is running: do nothing this tick.
    fn busy(&self) -> bool;
    fn take(&self) -> impl Future<Output = Result<Self::Hold, Refusal>> + Send;
    /// Stems with no settings, sorted, as the manager lists them.
    fn list_unregistered(&self) -> impl Future<Output = Result<Vec<String>, Fault>> + Send;
    fn settled(&self, stem: &str) -> impl Future<Output = Settled> + Send;
    fn register(&self, stem: &str) -> impl Future<Output = Result<Registered, Fault>> + Send;
    /// The revision of `models.ini` on disk now.
    fn revision(&self) -> impl Future<Output = Option<String>> + Send;
    /// The guarded reload, under `hold` (nothing is unloaded).
    fn reload(&self, hold: &Self::Hold) -> impl Future<Output = Reloaded> + Send;
}

/// What one run did, in Node's result shape ([`RunResult::to_json`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunResult {
    pub added: Vec<String>,
    /// `None` where Node's result has no `skipped`.
    pub skipped: Option<Vec<String>>,
    pub failed: Option<Vec<String>>,
    pub deferred: bool,
    pub error: Option<String>,
    /// Settled-check refusals (Rust only): stem and why.
    pub unsettled: Vec<(String, String)>,
    /// The exclusion was lost or could not be confirmed during the batch (Rust only).
    pub lost: bool,
}

impl RunResult {
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::{json, Map, Value};
        let mut m = Map::new();
        m.insert("added".into(), json!(self.added));
        if let Some(s) = &self.skipped {
            m.insert("skipped".into(), json!(s));
        }
        if let Some(f) = &self.failed {
            m.insert("failed".into(), json!(f));
        }
        if self.deferred {
            m.insert("deferred".into(), Value::Bool(true));
        }
        if let Some(e) = &self.error {
            m.insert("error".into(), json!(e));
        }
        if !self.unsettled.is_empty() {
            m.insert(
                "unsettled".into(),
                json!(self.unsettled.iter().map(|(s, _)| s).collect::<Vec<_>>()),
            );
        }
        if self.lost {
            m.insert("exclusionLost".into(), Value::Bool(true));
        }
        Value::Object(m)
    }
}

type Log = Arc<dyn Fn(&str) + Send + Sync>;

struct Skips {
    list: Vec<String>,
    file: Option<PathBuf>,
}

impl Skips {
    fn load(file: Option<PathBuf>) -> Skips {
        let mut list: Vec<String> = Vec::new();
        if let Some(path) = &file {
            if let Some(text) = std::fs::read(path)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
            {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    for s in v
                        .get("skip")
                        .and_then(|s| s.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|s| s.as_str())
                    {
                        if !list.iter().any(|x| x == s) {
                            list.push(s.to_owned());
                        }
                    }
                }
            }
        }
        Skips { list, file }
    }

    fn has(&self, stem: &str) -> bool {
        self.list.iter().any(|s| s == stem)
    }

    /// Best effort, as Node: a state file that cannot be written only costs a retry after a restart.
    fn remember(&mut self, stem: &str) {
        if !self.has(stem) {
            self.list.push(stem.to_owned());
        }
        let Some(path) = &self.file else { return };
        let start = self.list.len().saturating_sub(KEEP);
        let body =
            serde_json::json!({ "skip": self.list.get(start..).unwrap_or_default() }).to_string();
        let _ = write_private(path, body.as_bytes());
    }
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// One run at a time; cleared when the run ends, finishes or is dropped at any await.
struct RunSlot<'a>(&'a AtomicBool);
impl<'a> RunSlot<'a> {
    fn take(flag: &'a AtomicBool) -> Option<RunSlot<'a>> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| RunSlot(flag))
    }
}
impl Drop for RunSlot<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The worker. `run` is one scan; [`spawn`] schedules it.
pub struct Worker<E: Env> {
    env: E,
    skips: Mutex<Skips>,
    running: AtomicBool,
    log: Log,
}

impl<E: Env> Worker<E> {
    pub fn new(env: E, state_file: Option<PathBuf>, log: Log) -> Worker<E> {
        Worker {
            env,
            skips: Mutex::new(Skips::load(state_file)),
            running: AtomicBool::new(false),
            log,
        }
    }

    pub fn skipped(&self) -> Vec<String> {
        self.skips
            .lock()
            .map(|s| s.list.clone())
            .unwrap_or_default()
    }

    fn skip_has(&self, stem: &str) -> bool {
        self.skips.lock().is_ok_and(|s| s.has(stem))
    }

    fn remember(&self, stem: &str) {
        if let Ok(mut s) = self.skips.lock() {
            s.remember(stem);
        }
    }

    pub async fn run(&self) -> RunResult {
        let Some(_slot) = RunSlot::take(&self.running) else {
            return RunResult {
                skipped: Some(Vec::new()),
                ..RunResult::default()
            };
        };
        // #872: calibration and auto-tune restore models.ini by revision; a write in the middle of
        // one breaks that restore. Wait for the next tick.
        if self.env.busy() {
            return RunResult {
                deferred: true,
                ..RunResult::default()
            };
        }
        let listed = match self.env.list_unregistered().await {
            Ok(l) => l,
            Err(f) => return self.scan_failed(&f.message),
        };
        if listed.iter().all(|s| self.skip_has(s)) {
            return RunResult {
                skipped: Some(self.skipped()),
                failed: Some(Vec::new()),
                ..RunResult::default()
            };
        }
        // Nothing is written before the exclusion is held: busy is transient, never a refusal list.
        let hold = match self.env.take().await {
            Ok(h) => h,
            Err(_) => {
                return RunResult {
                    skipped: Some(self.skipped()),
                    deferred: true,
                    ..RunResult::default()
                }
            }
        };
        let result = self.batch(&hold).await;
        hold.release().await;
        result
    }

    fn scan_failed(&self, message: &str) -> RunResult {
        (self.log)(&format!("[models] folder scan failed: {message}"));
        RunResult {
            error: Some(message.to_owned()),
            ..RunResult::default()
        }
    }

    /// Registration and the one reload, under the exclusion.
    async fn batch(&self, hold: &E::Hold) -> RunResult {
        // The scan again, now that nobody else can write: a stale list could name a file that is
        // gone or already taken.
        let fresh_list = match self.env.list_unregistered().await {
            Ok(l) => l,
            Err(f) => return self.scan_failed(&f.message),
        };
        let fresh: Vec<String> = fresh_list
            .into_iter()
            .filter(|s| !self.skip_has(s))
            .collect();
        let mut out = RunResult::default();
        let mut failed: Vec<String> = Vec::new();
        let mut revision: Option<String> = None;
        for stem in &fresh {
            if !hold.excluded() || !hold.may_start(REGISTER_LIMIT) {
                out.lost = true;
                out.deferred = true;
                break;
            }
            if let Settled::Not(why) = self.env.settled(stem).await {
                out.unsettled.push((stem.clone(), why));
                continue;
            }
            // The check above takes what it takes: ask again right at the write.
            if !hold.excluded() || !hold.may_start(REGISTER_LIMIT) {
                out.lost = true;
                out.deferred = true;
                break;
            }
            match self.env.register(stem).await {
                Ok(done) => {
                    out.added.push(stem.clone());
                    if done.revision.is_some() {
                        revision = done.revision;
                    }
                }
                Err(f) if f.status.is_some_and(|s| RETRY_NEVER.contains(&s)) => {
                    self.remember(stem);
                }
                Err(f) => failed.push(format!(
                    "{stem} ({})",
                    if f.message.is_empty() {
                        "error"
                    } else {
                        &f.message
                    }
                )),
            }
        }
        let mut reload: Option<Reloaded> = None;
        let mut note = String::new();
        if !out.added.is_empty() {
            if !hold.excluded() {
                out.lost = true;
                out.deferred = true;
                note = " The exclusion ended first, so the presets were not reloaded; the engine will offer them after its next reload.".into();
            } else {
                let on_disk = self.env.revision().await;
                let fenced = match (&revision, &on_disk) {
                    (Some(wrote), Some(now)) => wrote != now,
                    (Some(_), None) => true,
                    (None, _) => false,
                };
                if fenced {
                    note = " models.ini changed after the last registration, so the presets were not reloaded; the engine will offer them after its next reload.".into();
                } else if !hold.excluded() {
                    out.lost = true;
                    out.deferred = true;
                    note = " The exclusion ended first, so the presets were not reloaded; the engine will offer them after its next reload.".into();
                } else {
                    reload = Some(self.env.reload(hold).await);
                }
            }
        }
        if !out.added.is_empty() {
            if reload != Some(Reloaded::Yes) && note.is_empty() {
                note = " The engine will offer them after its next reload.".into();
            }
            (self.log)(&format!(
                "[models] set up {} new model{} from the models folder: {}.{}",
                out.added.len(),
                if out.added.len() == 1 { "" } else { "s" },
                out.added.join(", "),
                note
            ));
        }
        if !failed.is_empty() {
            (self.log)(&format!("[models] could not set up: {}", failed.join(", ")));
        }
        for (stem, why) in &out.unsettled {
            (self.log)(&format!(
                "[models] {stem} is not ready to set up yet: {why}"
            ));
        }
        out.skipped = Some(self.skipped());
        out.failed = Some(failed);
        out
    }
}

/// Settings of the schedule.
#[derive(Clone, Copy, Debug)]
pub struct Schedule {
    pub first: Duration,
    pub every: Duration,
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule {
            first: FIRST_DELAY,
            every: INTERVAL,
        }
    }
}

/// Scans on a timer until `stop` is sent. Dropping the handle's task aborts the scan in flight; the
/// exclusion it holds gives itself back (see [`Hold`]).
pub fn spawn<E: Env>(
    worker: Arc<Worker<E>>,
    schedule: Schedule,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut wait = schedule.first;
        loop {
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                _ = stop.wait_for(|s| *s) => return,
            }
            tokio::select! {
                _ = worker.run() => {}
                _ = stop.wait_for(|s| *s) => return,
            }
            wait = schedule.every;
        }
    })
}

#[cfg(test)]
mod tests;
