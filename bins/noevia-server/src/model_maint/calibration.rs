//! The native calibration worker (core `llamacpp-calibration.cjs` `createCalibrator`, #2303 follow-up).
//!
//! The public start/status/cancel policy lives in the front (`calibration_routes.rs`,
//! `calibration_cancel_routes.rs`), and the job runs here, in [`super::calibration_worker`] (its
//! launch and cancellation facts are [`super::calibration_facts`]); Core's public calibrator and its
//! boot `recover()` are off.
//! This module is the worker's runtime, ported piece by piece with the same state file, so an old
//! Core and this front read each other's jobs (rollback both ways):
//!
//! - [`StateFile`]: `native-calibration-<hash>.json`, `{ job, history }` exactly as Node writes it
//!   (unknown keys kept), written through a temp file and a rename, mode 0600.
//! - [`Maint::prepare_calibration_recovery`]: `recover()`, a job still marked running when the process
//!   stopped becomes `interrupted`, and the original profile goes back only if models.ini is still
//!   exactly what the job last wrote (compare-and-swap on `lastRevision`).
//! - [`ladder`], [`bound_load`], [`verify_long`]: `run()`'s search, quick load checks that only
//!   bound the size, then middle-out long-prompt tests that decide it. Steps are a [`Steps`]
//!   implementation, so the search is the same for the engine and for a synthetic fixture.
//! - [`LongPrompt`]: `streamLong()`'s reading of the engine's streamed prompt progress, its early
//!   stop when the prompt is predicted (or measured) over the time limit, and the measured result.
//!
//! Node's auto-tune keeps its own in-process calibrator (`contextFactory`, `probe`) and is not
//! affected by this module.

use super::ini::Candidate;
use super::Maint;
use js_json::{wtf16, JValue};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `HISTORY_PER_MODEL`.
pub const HISTORY_PER_MODEL: usize = 10;
/// `recover()`'s error and the restore failure suffix (`NOT_RESTORED`), as Node writes them.
pub const STOPPED: &str = "noevia stopped during calibration.";
pub const NOT_RESTORED: &str = " The original profile was not restored because models.ini changed since the run started; check this model's context size.";
/// `request('/models?reload=1', {}, 120000)`.
pub(super) const RELOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// llamacpp-manager.cjs `PRESET_STEP_MS`: the lease a models.ini write and its 120 s router re-read
/// need left (`maintenance.assertCovered(PRESET_STEP_MS)`).
pub(super) const PRESET_STEP: Duration = Duration::from_secs(150);

/// The state file name Core uses (`index.cjs` `calibrationStatePath`).
pub fn state_path(data_dir: &Path, suffix: &str) -> PathBuf {
    data_dir.join(format!("native-calibration-{suffix}.json"))
}

/// `toLocaleString('en-US')` for the whole token counts the messages show.
pub fn grouped(n: f64) -> String {
    if !n.is_finite() || n.fract() != 0.0 || n.abs() >= 1e21 {
        return wtf16::stringify(&JValue::Num(n)).unwrap_or_default();
    }
    let digits = format!("{}", n.abs() as u128);
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0.0 {
        out.insert(0, '-');
    }
    out
}

/// `${n}` (String(n)), as Node interpolates the budget and the forecast seconds.
pub fn plain(n: f64) -> String {
    wtf16::stringify(&JValue::Num(n)).unwrap_or_else(|| "null".into())
}

/// JS `Math.round` (halves toward +Infinity).
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

// ── State file ───────────────────────────────────────────────────────────────

/// `{ job, history }` and whatever else the file held, in the file's key order.
#[derive(Debug, Clone, PartialEq)]
pub struct State(pub Vec<(String, JValue)>);

static UNSET: JValue = JValue::Undefined;

impl State {
    pub fn empty() -> State {
        State(vec![
            ("job".into(), JValue::Null),
            ("history".into(), JValue::Obj(Vec::new())),
        ])
    }
    pub fn get(&self, key: &str) -> &JValue {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map_or(&UNSET, |(_, v)| v)
    }
    pub fn job(&self) -> &JValue {
        self.get("job")
    }
    pub fn job_mut(&mut self) -> Option<&mut Vec<(String, JValue)>> {
        self.0
            .iter_mut()
            .find(|(k, _)| k == "job")
            .and_then(|(_, v)| match v {
                JValue::Obj(f) => Some(f),
                _ => None,
            })
    }
    pub fn set_job(&mut self, job: JValue) {
        wtf16::set(&mut self.0, "job", job);
    }
    /// `state.history[model] = [entry, ...(state.history[model] || [])].slice(0, HISTORY_PER_MODEL)`.
    pub fn push_history(&mut self, model: &str, entry: JValue) {
        if !matches!(self.get("history"), JValue::Obj(_)) {
            wtf16::set(&mut self.0, "history", JValue::Obj(Vec::new()));
        }
        let Some((_, JValue::Obj(history))) = self.0.iter_mut().find(|(k, _)| k == "history")
        else {
            return;
        };
        let mut list = vec![entry];
        if let Some((_, JValue::Arr(old))) = history.iter().find(|(k, _)| k == model) {
            list.extend(old.iter().cloned());
        }
        list.truncate(HISTORY_PER_MODEL);
        wtf16::set(history, model, JValue::Arr(list));
    }
    pub fn to_value(&self) -> JValue {
        JValue::Obj(self.0.clone())
    }
}

#[cfg(test)]
type CommitPoint = (
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
);
#[cfg(test)]
pub(super) type CommitPause = std::sync::Arc<std::sync::Mutex<Option<CommitPoint>>>;
#[cfg(test)]
fn commit_pause(path: Option<&Path>) -> CommitPause {
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    type Registry = Mutex<std::collections::HashMap<PathBuf, Weak<Mutex<Option<CommitPoint>>>>>;
    static PAUSES: OnceLock<Registry> = OnceLock::new();
    let mut pauses = PAUSES
        .get_or_init(|| Mutex::new(Default::default()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let key = path.unwrap_or_else(|| Path::new("")).to_owned();
    if let Some(pause) = pauses.get(&key).and_then(Weak::upgrade) {
        return pause;
    }
    pauses.retain(|_, p| p.strong_count() > 0);
    let pause = Arc::new(Mutex::new(None));
    pauses.insert(key, Arc::downgrade(&pause));
    pause
}

/// `load()` / `save()` over the state file. `None`: no data directory, nothing persists.
#[derive(Debug, Clone)]
pub struct StateFile {
    pub path: Option<PathBuf>,
    #[cfg(test)]
    pub(super) commit_pause: CommitPause,
}

impl StateFile {
    /// `load()`: the file, or a fresh state when it is missing or not JSON; a missing or non-object
    /// history becomes `{}` (JS `typeof` counts arrays as objects, so an array is kept).
    pub fn load(&self) -> State {
        let Some(path) = &self.path else {
            return State::empty();
        };
        let parsed = std::fs::read(path)
            .ok()
            .and_then(|b| wtf16::parse(&String::from_utf8_lossy(&b)).ok());
        let mut state = match parsed {
            Some(JValue::Obj(fields)) => State(fields),
            _ => return State::empty(),
        };
        if !matches!(state.get("history"), JValue::Obj(_) | JValue::Arr(_)) {
            wtf16::set(&mut state.0, "history", JValue::Obj(Vec::new()));
        }
        state
    }

    /// Boot inspection must distinguish verified absence from an unknown durable obligation.
    pub(super) fn load_checked(&self) -> std::io::Result<State> {
        let Some(path) = &self.path else {
            return Ok(State::empty());
        };
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::empty()),
            Err(e) => return Err(e),
        };
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| std::io::Error::other("invalid calibration state"))?;
        let mut state = match wtf16::parse(text) {
            Ok(JValue::Obj(fields)) => State(fields),
            _ => return Err(std::io::Error::other("invalid calibration state")),
        };
        if !matches!(state.get("history"), JValue::Obj(_) | JValue::Arr(_)) {
            wtf16::set(&mut state.0, "history", JValue::Obj(Vec::new()));
        }
        Ok(state)
    }

    /// `save()`: a temp file beside it (mode 0600), then a rename over it.
    #[cfg(test)]
    pub fn save(&self, state: &State) -> std::io::Result<()> {
        self.save_if(state, || true)
    }

    /// A timed-out/superseded writer may finish its disk work, but must not rename its stale
    /// snapshot over a newer job. The worker serializes these commits and revokes this permit.
    pub(super) fn save_if(&self, state: &State, allowed: impl Fn() -> bool) -> std::io::Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let Some(path) = &self.path else {
            return Ok(());
        };
        let text = wtf16::stringify(&state.to_value())
            .ok_or_else(|| std::io::Error::other("state is not serializable"))?;
        let mut nonce = [0u8; 16];
        {
            use rand_core::RngCore;
            rand_core::OsRng
                .try_fill_bytes(&mut nonce)
                .map_err(|_| std::io::Error::other("no randomness"))?;
        }
        let temp = PathBuf::from(format!(
            "{}.{}",
            path.display(),
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ));
        #[cfg(test)]
        let mut completion = None;
        let written = (|| {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            if !allowed() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "state write ended",
                ));
            }
            #[cfg(test)]
            {
                let pause = self
                    .commit_pause
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take();
                if let Some((entered, release, done)) = pause {
                    let _ = entered.send(());
                    super::calibration_worker::test_changed();
                    let _ = release.recv_timeout(Duration::from_secs(30));
                    completion = Some(done);
                }
            }
            // The Worker holds its single serialization lock through both this final check
            // and rename/directory fsync. Revocation after the temporary write discards it.
            if !allowed() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "state write ended",
                ));
            }
            std::fs::rename(&temp, path)?;
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            std::fs::File::open(parent)?.sync_all()
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        #[cfg(test)]
        if let Some(done) = completion {
            let _ = done.send(());
            super::calibration_worker::test_changed();
        }
        written
    }
}

/// `recover()`'s change to an interrupted job, before and after the restore attempt.
/// `Some(Some((lastRevision, originalText)))` when there is a restore to try; `Some(None)` when
/// Node would try one with values that are not strings (its commit throws: not restored).
pub fn interrupt(job: &mut Vec<(String, JValue)>, at: f64) -> Option<Option<(String, String)>> {
    wtf16::set(job, "status", JValue::Str("interrupted".into()));
    wtf16::set(job, "finishedAt", JValue::Num(at));
    wtf16::set(job, "error", JValue::Str(STOPPED.into()));
    let get = |k: &str| job.iter().find(|(key, _)| key == k).map(|(_, v)| v);
    let original = get("originalText").filter(|v| !v.is_nullish());
    let last = get("lastRevision").filter(|v| v.truthy());
    match (original, last) {
        // Node passes the values as they are; both are strings in every file it writes.
        (Some(JValue::Str(text)), Some(JValue::Str(rev))) => {
            Some(Some((rev.clone(), text.clone())))
        }
        (Some(_), Some(_)) => Some(None),
        _ => None,
    }
}

/// `recover()`'s end: the restore outcome, the suffix, and the private profile text removed.
pub fn settle_interrupted(job: &mut Vec<(String, JValue)>, restored: Option<bool>) {
    if let Some(r) = restored {
        wtf16::set(job, "restored", JValue::Bool(r));
    }
    if restored == Some(false) {
        let error = job
            .iter()
            .find(|(k, _)| k == "error")
            .and_then(|(_, v)| v.as_str())
            .unwrap_or(STOPPED)
            .to_owned();
        wtf16::set(job, "error", JValue::Str(format!("{error}{NOT_RESTORED}")));
    }
    job.retain(|(k, _)| k != "originalText");
}

impl Maint {
    /// The calibration state file of this engine.
    pub fn calibration_state(&self) -> StateFile {
        let path = self.data_dir.as_ref().map(|d| state_path(d, &self.suffix));
        StateFile {
            #[cfg(test)]
            commit_pause: commit_pause(path.as_deref()),
            path,
        }
    }

    /// `recover()`: a run still marked running when the worker stopped. The preset goes back only
    /// if the file is still exactly what the job last wrote, so an operator's later edit survives.
    /// Returns a prepared snapshot only. The Worker persists it through its single writer and
    /// retains the admission fence while any earlier blocking commit remains unresolved.
    pub(super) async fn prepare_calibration_recovery(
        &self,
        mut state: State,
        wake: Option<std::sync::Arc<tokio::sync::Notify>>,
    ) -> Option<PreparedRecovery> {
        self.ini.as_ref()?;
        let decided = matches!(
            state.job().get("terminalDecision").as_str(),
            Some("passed" | "fenced")
        );
        if state.job().get("terminalDecision").as_str() == Some("passed") {
            if let Some(job) = state.job_mut() {
                job.retain(|(k, _)| k != "terminalDecision" && k != "pendingTombstone");
            }
            return Some(PreparedRecovery {
                state,
                restored: None,
                decided: true,
            });
        }
        let running = state.job().get("status").as_str() == Some("running");
        let pending_fence = state.job().get("pendingTombstone").truthy();
        let pending = match (
            state
                .job()
                .get("pendingRestore")
                .get("baseRevision")
                .as_str(),
            state.job().get("pendingRestore").get("text").as_str(),
        ) {
            (Some(base), Some(text)) => Some((base.to_owned(), text.to_owned())),
            _ => None,
        };
        if !running && pending.is_none() && !pending_fence {
            return None;
        }
        if let Some(store) = self.evidence.clone() {
            let model = state.job().get("model").as_str().unwrap_or("").to_owned();
            let id = state.job().get("id").as_str().unwrap_or("").to_owned();
            let mut wait = Duration::from_secs(1);
            loop {
                let (store, model, id) = (std::sync::Arc::clone(&store), model.clone(), id.clone());
                match tokio::task::spawn_blocking(move || store.tombstone(&model, &id)).await {
                    Ok(Ok(())) => break,
                    Ok(Err(e)) => {
                        eprintln!("[models] calibration fence recovery failed: {:?}", e.kind())
                    }
                    Err(_) => eprintln!("[models] calibration fence recovery failed: Other"),
                }
                if let Some(wake) = &wake {
                    tokio::select! { () = tokio::time::sleep(wait) => {}, () = wake.notified() => {} }
                } else {
                    tokio::time::sleep(wait).await;
                }
                wait = (wait * 2).min(Duration::from_secs(60));
            }
        }
        let job = state.job_mut()?;
        let interrupted = if running {
            interrupt(job, now_ms())
        } else {
            None
        };
        let restore = pending.map(Some).or(interrupted);
        let restored = match restore {
            None => None,
            Some(None) => Some(false),
            Some(Some((base_revision, text))) => {
                let c = Candidate {
                    base_revision,
                    text,
                    before: String::new(),
                };
                // The Worker owns one recovery path for both restart and live loss. Preparation
                // records intent only; it never writes presets or dispatches to the router.
                wtf16::set(
                    job,
                    "pendingRestore",
                    JValue::obj([
                        ("baseRevision", JValue::Str(c.base_revision)),
                        ("text", JValue::Str(c.text)),
                    ]),
                );
                Some(false)
            }
        };
        let needs_restore = state.job().get("pendingRestore").truthy();
        if let Some(job) = state.job_mut() {
            if running {
                settle_interrupted(job, restored);
            } else if let Some(restored) = restored {
                wtf16::set(job, "restored", JValue::Bool(restored));
            }
            if pending_fence && !decided {
                wtf16::set(job, "status", JValue::Str("failed".into()));
                wtf16::set(job, "phase", JValue::Str("Failed".into()));
                wtf16::set(
                    job,
                    "error",
                    JValue::Str("Calibration finalization failed.".into()),
                );
                wtf16::set(job, "finishedAt", JValue::Num(now_ms()));
            }
            job.retain(|(k, _)| {
                (k != "pendingRestore" || needs_restore)
                    && k != "pendingTombstone"
                    && k != "terminalDecision"
            });
        }
        Some(PreparedRecovery {
            state,
            restored,
            decided,
        })
    }
}

/// Recovery prepares a result; only the Worker owns persistence and its commit ordering.
pub(super) struct PreparedRecovery {
    pub state: State,
    pub restored: Option<bool>,
    pub decided: bool,
}

/// What the Worker published after recovery preparation and its serialized save.
#[derive(Debug, Clone, PartialEq)]
pub enum Recovery {
    /// No job was running (or there is no models.ini): nothing changed.
    Nothing,
    /// The job is interrupted in `state`, and the file says so. `restored`: the profile restore's
    /// outcome (`None` when there was none to try).
    Saved {
        restored: Option<bool>,
        state: State,
    },
    /// As `Saved`, except that writing the file failed: `state` is right, the file still says
    /// running.
    Unsaved {
        restored: Option<bool>,
        state: State,
    },
}

impl Recovery {
    /// `Some(restored)` when there was a running job to recover.
    pub fn restored(&self) -> Option<Option<bool>> {
        match self {
            Recovery::Nothing => None,
            Recovery::Saved { restored, .. } | Recovery::Unsaved { restored, .. } => {
                Some(*restored)
            }
        }
    }
}

// ── Profile writes (llamacpp-manager.cjs `applyUnlocked`) ─────────────────────

/// How a profile write ended, as calibration reads `applyUnlocked`'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Written and the router re-read it.
    Ok,
    /// `{ ok: false, body: { error } }`: the job records the error (or its own default) and stops.
    Refused(Option<String>),
    /// The write threw: inside a step the size failed; at the end the run failed with this text.
    Threw(String),
    /// Written, but its re-read failed with the hold no longer excluding everyone: nothing more ran
    /// on the lost lease (llamacpp-manager.cjs applyUnlocked's loss branch, #2550). Putting back
    /// `before` while models.ini is still at `revision` is owed to a recovery under a fresh hold.
    LeaseLost { revision: String, before: String },
    /// Not written: the hold could no longer cover the write and its re-read
    /// (`maintenance.assertCovered(PRESET_STEP_MS)` threw its coverage error before the commit,
    /// #2608). The caller ends as for a lost lease; there is nothing of this write to restore.
    Uncovered,
}

pub(super) const UNLOAD_FIRST: &str = "Unload all router models and finish downloads before applying a profile. Other clients must remain stopped.";
pub(super) const RELOAD_UNCERTAIN: &str = "Reload outcome is uncertain and the file changed again. Stop inference and inspect native presets before retrying.";
pub(super) const RELOAD_STUCK: &str = "Reload failed or timed out, and the previous preset file could not be restored: models.ini still holds the new settings. Check router and Model Loader health before retrying.";
pub(super) const RELOAD_UNCONFIRMED: &str = "Reload failed or timed out, and restoring the previous preset file could not be confirmed. Stop inference and inspect native presets before retrying.";
pub(super) const RELOAD_RESTORED: &str = "Reload failed or timed out. Previous preset file restored; check router health before retrying.";

impl Maint {
    /// `applyUnlocked({ model, baseRevision, options }, { backup })`, under the caller's hold: the
    /// router lists the model and nothing is loaded, the profile is prepared and committed, then the
    /// router re-reads models.ini; a failed re-read puts this write's own previous text back.
    pub async fn apply_unlocked(
        &self,
        holding: &super::Hold,
        model: &str,
        base_revision: &str,
        options: &[(String, Option<String>)],
        backup: bool,
    ) -> Applied {
        self.apply_unlocked_with_status(holding, model, base_revision, options, backup)
            .await
            .0
    }

    /// [`Maint::apply_unlocked`] with the HTTP status Node's `applyUnlocked` answers (200 written,
    /// 404 not installed, 409 something loaded, 503 a reload or restore failure; a thrown write is
    /// 500): the native auto-tune runner treats a 409 as fatal, as Node's does. `Ok((status,
    /// error))`; `Err(Some((revision, before)))` when the lease was lost after the write
    /// ([`Applied::LeaseLost`]), `Err(None)` when the hold could not cover the write and nothing
    /// was written ([`Applied::Uncovered`]): both are Node's coverage error.
    pub async fn apply_unlocked_reply(
        &self,
        holding: &super::Hold,
        model: &str,
        base_revision: &str,
        options: &[(String, Option<String>)],
        backup: bool,
    ) -> Result<(u16, Option<String>), Option<(String, String)>> {
        let (applied, status) = self
            .apply_unlocked_with_status(holding, model, base_revision, options, backup)
            .await;
        Ok(match applied {
            Applied::LeaseLost { revision, before } => return Err(Some((revision, before))),
            Applied::Uncovered => return Err(None),
            Applied::Ok => (200, None),
            Applied::Refused(e) => (
                status,
                Some(e.unwrap_or_else(|| {
                    "Could not save settings; the profile may have changed.".into()
                })),
            ),
            Applied::Threw(e) => (500, Some(e)),
        })
    }

    async fn apply_unlocked_with_status(
        &self,
        holding: &super::Hold,
        model: &str,
        base_revision: &str,
        options: &[(String, Option<String>)],
        backup: bool,
    ) -> (Applied, u16) {
        let (Some(router), Some(ini)) = (self.router.as_ref(), self.ini.clone()) else {
            return (
                Applied::Threw("Native preset editing; configure LLAMACPP_PRESET_PATH".into()),
                500,
            );
        };
        let token = holding.token();
        let listing = match router.models(token.as_deref()).await {
            Ok(r) => r,
            Err(_) => return (Applied::Threw("fetch failed".into()), 500),
        };
        if !listing.ok {
            let status = listing.status;
            return (
                Applied::Refused(
                    listing
                        .body
                        .as_ref()
                        .and_then(|b| b.get("error"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                ),
                status,
            );
        }
        let Some(rows) = super::router::rows(&listing.body) else {
            return (Applied::Threw("Invalid router model listing".into()), 500);
        };
        if !rows
            .iter()
            .any(|m| m.get("id").and_then(Value::as_str) == Some(model))
        {
            return (
                Applied::Refused(Some("Choose an installed model".into())),
                404,
            );
        }
        let status = |m: &Value| {
            m.get("status")
                .and_then(|s| s.get("value"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        if rows
            .iter()
            .any(|m| status(m).as_deref() != Some("unloaded"))
        {
            return (Applied::Refused(Some(UNLOAD_FIRST.into())), 409);
        }
        let limits = self.sizing.cache_ram_limits();
        let (file, m, base, opts) = (
            ini.file().to_path_buf(),
            model.to_owned(),
            base_revision.to_owned(),
            options.to_vec(),
        );
        let candidate = match tokio::task::spawn_blocking(move || {
            super::ini::prepare(&file, &m, &base, &opts, limits)
        })
        .await
        {
            Ok(Ok(c)) => c,
            Ok(Err(t)) => return (Applied::Threw(t.message), t.status.unwrap_or(500)),
            Err(_) => return (Applied::Threw("Internal error".into()), 500),
        };
        // Row J review P1 (#2608): the write and its re-read are one step; it starts only while the
        // hold can still cover it, or nothing is written (Node throws its 503 coverage error here).
        // Node's rule for the step (no lapse within the margin, the step inside the hard cap; not
        // marking a loss here, #2641), and the re-read's own rule, which narrows the chance that a
        // write that starts is refused its re-read (the commit takes time).
        if !holding.fits(PRESET_STEP) || !holding.may_start(super::reload::RELOAD_LIMIT) {
            return (Applied::Uncovered, 503);
        }
        if let Err(e) = ini.commit(&candidate, backup).await {
            // A settled Model Loader outcome carries its own message; nothing was reloaded.
            if e.status >= 500 {
                return (Applied::Refused(Some(e.message)), e.status);
            }
            return (Applied::Threw(e.message), e.status);
        }
        #[cfg(test)]
        after_commit(ini.file(), holding);
        let reload = self.router_reload(router, holding, None).await;
        if let Ok(Ok(reply)) = &reload {
            if reply.ok {
                return (Applied::Ok, 200);
            }
        }
        // The lease was lost after the write, or can no longer cover the re-read (Node's coverage
        // check counts that as lost too): nothing more runs on it, not even the restore.
        if !holding.excluded() || matches!(reload, Ok(Err("lease_lost"))) {
            return (
                Applied::LeaseLost {
                    revision: super::ini::sha(&candidate.text),
                    before: candidate.before,
                },
                503,
            );
        }
        // Restore only our own version; never overwrite an operator's later edit.
        let restore = super::ini::Candidate {
            base_revision: super::ini::sha(&candidate.text),
            text: candidate.before.clone(),
            before: candidate.text.clone(),
        };
        match ini.commit(&restore, true).await {
            Err(e) if e.status == 409 => (Applied::Refused(Some(RELOAD_UNCERTAIN.into())), 503),
            Err(e) if e.retryable => (Applied::Refused(Some(RELOAD_STUCK.into())), 503),
            Err(_) => (Applied::Refused(Some(RELOAD_UNCONFIRMED.into())), 503),
            Ok(()) => {
                let _ = self.router_reload(router, holding, None).await;
                (Applied::Refused(Some(RELOAD_RESTORED.into())), 503)
            }
        }
    }
}

/// Test hooks run right after a models.ini commit of `apply_unlocked` for that file (a lease that
/// ends between a write and its re-read).
#[cfg(test)]
pub(super) type AfterCommit = std::sync::Arc<dyn Fn(&super::Hold) + Send + Sync>;
#[cfg(test)]
pub(super) static AFTER_COMMIT: std::sync::Mutex<Vec<(std::path::PathBuf, AfterCommit)>> =
    std::sync::Mutex::new(Vec::new());
#[cfg(test)]
fn after_commit(file: &std::path::Path, holding: &super::Hold) {
    let hooks: Vec<AfterCommit> = AFTER_COMMIT.lock().map_or_else(
        |_| Vec::new(),
        |h| {
            h.iter()
                .filter(|(p, _)| p == file)
                .map(|(_, f)| std::sync::Arc::clone(f))
                .collect()
        },
    );
    for hook in hooks {
        hook(holding);
    }
}

impl Maint {
    /// The manager's `routerReload(120000)` for the native auto-tune runner: the router's reply,
    /// or `None` where the request did not answer.
    pub(super) async fn router_reload_reply(
        &self,
        router: &super::router::Router,
        holding: &super::Hold,
    ) -> Option<super::router::Reply> {
        match self.router_reload(router, holding, None).await {
            Ok(Ok(reply)) => Some(reply),
            Ok(Err(_)) => Some(super::router::Reply {
                ok: false,
                status: 409,
                body: None,
            }),
            Err(()) => None,
        }
    }
}

// ── Search ───────────────────────────────────────────────────────────────────

/// `ladder(native)`: the Rust context candidates (and the native length) between 4096 and the
/// native length, or 131072 when it is unknown, ascending and without repeats.
pub fn ladder(native: f64) -> Vec<f64> {
    let ceiling = if native > 0.0 { native } else { 131072.0 };
    let mut grid: Vec<f64> = llamacpp_autoconfig::CTX_CANDIDATES
        .iter()
        .copied()
        .chain((native > 0.0).then_some(native))
        .filter(|&c| c >= 4096.0 && c <= ceiling)
        .collect();
    grid.sort_by(f64::total_cmp);
    grid.dedup();
    grid
}

/// `run()`'s starting size: the first candidate from 8K, else the largest.
pub fn start_of(grid: &[f64]) -> Option<f64> {
    grid.iter()
        .copied()
        .find(|&c| c >= 8192.0)
        .or_else(|| grid.last().copied())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Load,
    Long,
}

/// Why a run stopped short of a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    Cancelled,
    /// A fatal error, with the message the job records.
    Fatal(String),
}

/// One measured step at a size: `Ok(true)` passed, `Ok(false)` failed (the next size is tried).
#[allow(async_fn_in_trait)]
pub trait Steps {
    async fn step(&mut self, ctx: f64, kind: Kind) -> Result<bool, Stop>;
}

/// `run()`'s load bound: doubling from `start` while loads pass, then up to three bisections
/// between the last pass and the first failure. The largest size that loaded.
pub async fn bound_load(grid: &[f64], steps: &mut impl Steps) -> Result<f64, Stop> {
    let (Some(start), Some(&top)) = (start_of(grid), grid.last()) else {
        return Err(Stop::Fatal(
            "Calibration is unavailable: the context ladder is empty.".into(),
        ));
    };
    let (mut last_pass, mut first_fail, mut ctx) = (0.0, 0.0, start);
    loop {
        if steps.step(ctx, Kind::Load).await? {
            last_pass = ctx;
            if ctx >= top {
                break;
            }
            ctx = grid
                .iter()
                .copied()
                .find(|&c| c >= ctx * 2.0)
                .unwrap_or(top);
        } else {
            first_fail = ctx;
            break;
        }
    }
    if last_pass == 0.0 {
        return Err(Stop::Fatal(format!(
            "The model did not load even at {} tokens.",
            grouped(start)
        )));
    }
    for _ in 0..3 {
        if first_fail == 0.0 {
            break;
        }
        let between: Vec<f64> = grid
            .iter()
            .copied()
            .filter(|&c| c > last_pass && c < first_fail)
            .collect();
        let Some(&mid) = between.get(between.len() / 2) else {
            break;
        };
        if steps.step(mid, Kind::Load).await? {
            last_pass = mid;
        } else {
            first_fail = mid;
        }
    }
    Ok(last_pass)
}

/// `run()`'s decision: middle-out long-prompt tests over the sizes from `start` to the load bound,
/// at most seven, up on a pass and down on a failure. The largest size that passed.
pub async fn verify_long(
    grid: &[f64],
    load_ctx: f64,
    budget_seconds: f64,
    steps: &mut impl Steps,
) -> Result<f64, Stop> {
    let start = start_of(grid).unwrap_or(load_ctx);
    let sizes: Vec<f64> = grid
        .iter()
        .copied()
        .filter(|&c| c >= start && c <= load_ctx)
        .collect();
    let (mut low, mut high) = (-1i64, sizes.len() as i64);
    let mut idx = (sizes.len() as i64 - 1).div_euclid(2);
    let mut tests = 0;
    while high - low > 1 && tests < 7 {
        let Some(&size) = usize::try_from(idx).ok().and_then(|i| sizes.get(i)) else {
            break;
        };
        if steps.step(size, Kind::Long).await? {
            low = idx;
        } else {
            high = idx;
        }
        idx = low + 1.max((high - low).div_euclid(2));
        tests += 1;
        if idx >= high {
            break;
        }
    }
    match usize::try_from(low).ok().and_then(|i| sizes.get(i)) {
        Some(&chosen) => Ok(chosen),
        None => Err(Stop::Fatal(format!(
            "No size passed the long-prompt test within {} s, down to {} tokens.",
            plain(budget_seconds),
            grouped(start)
        ))),
    }
}

// ── Long prompt stream ───────────────────────────────────────────────────────

/// What the step record shows while the long prompt runs (`record.progress`, `record.etaSeconds`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    pub progress: Option<f64>,
    pub eta_seconds: Option<f64>,
}

/// How a long prompt ended.
#[derive(Debug, Clone, PartialEq)]
pub enum LongOutcome {
    Passed {
        text: String,
        prompt_tokens: Option<f64>,
        prompt_per_second: Option<f64>,
        prompt_seconds: f64,
    },
    /// Over the time limit: forecast from progress (`predicted`) or measured.
    OverBudget { predicted: bool, eta_ms: f64 },
    /// The engine reported an error event. The detail is evidence only, never a client's text.
    EngineError { status: Option<f64>, detail: String },
}

/// One event read: go on (and whether to persist the step's progress now), or stop reading.
#[derive(Debug, Clone, PartialEq)]
pub enum Feed {
    Continue { save: bool },
    Stop(LongOutcome),
}

/// `ENGINE_TEXT_UNITS`.
pub const ENGINE_TEXT_UNITS: usize = 2048;

/// `Number(v)` for the engine's JSON numbers (strings as ECMAScript reads them, else NaN).
fn js_number(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Null => 0.0,
        Value::String(s) => {
            llamacpp_autoconfig::string_to_number(&s.encode_utf16().collect::<Vec<_>>())
                .unwrap_or(f64::NAN)
        }
        _ => f64::NAN,
    }
}

/// `Number(v) || fallback`.
fn or_number(v: Option<&Value>, fallback: Option<f64>) -> Option<f64> {
    let n = v.map_or(f64::NAN, js_number);
    if n != 0.0 && !n.is_nan() {
        Some(n)
    } else {
        fallback
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `` `${v || ''}` `` for a delta field.
fn text_of(v: Option<&Value>) -> String {
    if !truthy(v) {
        return String::new();
    }
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => {
            wtf16::stringify(&JValue::Num(n.as_f64().unwrap_or(f64::NAN))).unwrap_or_default()
        }
        Some(Value::Bool(true)) => "true".into(),
        // Objects read as "[object Object]" in JS; arrays join their items.
        Some(Value::Object(_)) => "[object Object]".into(),
        // Array.prototype.toString: items joined by commas, null items empty.
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::Null => String::new(),
                Value::Bool(false) => "false".into(),
                Value::Number(n) if n.as_f64() == Some(0.0) => "0".into(),
                other => text_of(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(_) | None => String::new(),
    }
}

/// `streamLong()`'s reading of one streamed long prompt: server-sent events split on blank lines,
/// `data:` lines parsed, progress and the forecast checked on every event.
#[derive(Debug, Clone)]
pub struct LongPrompt {
    budget_ms: f64,
    started_at: f64,
    last_save: f64,
    buffer: Vec<u8>,
    text: String,
    prompt_tokens: Option<f64>,
    prompt_per_second: Option<f64>,
    prompt_ms: Option<f64>,
    /// The record when a save fell due in the chunk being fed (Node saves inside its read loop,
    /// so the rest of that chunk, and a stop in it, come after the save).
    due_save: Option<Progress>,
}

impl LongPrompt {
    pub fn new(budget_ms: f64, started_at: f64) -> LongPrompt {
        LongPrompt {
            budget_ms,
            started_at,
            last_save: 0.0,
            buffer: Vec::new(),
            text: String::new(),
            prompt_tokens: None,
            prompt_per_second: None,
            prompt_ms: None,
            due_save: None,
        }
    }

    /// After a feed: the record as it was when Node's loop saved in that chunk (a save that fell
    /// due there, before the rest of the chunk or a stop in it), if one did. Should two fall due
    /// in one chunk (1.5 s apart within it), this is the later one and Node's earlier save is
    /// not repeated: one chunk's events are read in microseconds, and under a recorded clock
    /// they are read at one instant.
    pub fn take_due_save(&mut self) -> Option<Progress> {
        self.due_save.take()
    }

    /// One chunk of the body. `now` is read per event, as Node reads its clock.
    pub fn feed(
        &mut self,
        chunk: &[u8],
        now: &mut impl FnMut() -> f64,
        record: &mut Progress,
    ) -> Feed {
        self.buffer.extend_from_slice(chunk);
        self.due_save = None;
        let mut save = false;
        while let Some(cut) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            let event: Vec<u8> = self.buffer.drain(..cut + 2).take(cut).collect();
            let event = String::from_utf8_lossy(&event).into_owned();
            for line in event.split('\n') {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                // #1047: the engine's own words stay out of the step's reason.
                if truthy(value.get("error")) {
                    let e = value.get("error").unwrap_or(&Value::Null);
                    let code = js_number(e.get("code").unwrap_or(&Value::Null));
                    // `String(value.error.message ?? '')`.
                    let detail = match e.get("message") {
                        None | Some(Value::Null) => String::new(),
                        Some(Value::Bool(false)) => "false".into(),
                        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => "0".into(),
                        Some(Value::String(s)) if s.is_empty() => String::new(),
                        other => text_of(other),
                    };
                    return Feed::Stop(LongOutcome::EngineError {
                        status: (code != 0.0 && !code.is_nan()).then_some(code),
                        detail: wtf16::slice(&detail, ENGINE_TEXT_UNITS),
                    });
                }
                if let Some(progress) = value.get("prompt_progress").filter(|p| truthy(Some(*p))) {
                    let total = progress.get("total").map_or(f64::NAN, js_number);
                    if total > 0.0 {
                        let processed = progress.get("processed").map_or(f64::NAN, js_number);
                        let cache = or_number(progress.get("cache"), Some(0.0)).unwrap_or(0.0);
                        let time_ms = progress.get("time_ms").map_or(f64::NAN, js_number);
                        let done = (processed - cache).max(0.0);
                        let remaining = total - processed;
                        record.progress = Some(js_round(processed / total * 100.0));
                        let elapsed = now() - self.started_at;
                        // The first one-token event would predict a runaway time: wait for a batch.
                        if done >= 512.0 && time_ms >= 500.0 {
                            let eta_ms = elapsed + remaining * (time_ms / done);
                            record.eta_seconds =
                                Some(js_round((eta_ms - elapsed).max(0.0) / 1000.0));
                            if (elapsed > 5000.0 && eta_ms > self.budget_ms * 1.15)
                                || eta_ms > self.budget_ms * 2.0
                            {
                                record.eta_seconds = None;
                                return Feed::Stop(LongOutcome::OverBudget {
                                    predicted: true,
                                    eta_ms,
                                });
                            }
                        }
                        let t = now();
                        if t - self.last_save > 1500.0 {
                            self.last_save = t;
                            save = true;
                            self.due_save = Some(record.clone());
                        }
                    }
                }
                let t = now();
                if t - self.started_at > self.budget_ms * 1.5 {
                    record.eta_seconds = None;
                    return Feed::Stop(LongOutcome::OverBudget {
                        predicted: false,
                        eta_ms: t - self.started_at,
                    });
                }
                let delta = value
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("delta"));
                self.text
                    .push_str(&text_of(delta.and_then(|d| d.get("content"))));
                self.text
                    .push_str(&text_of(delta.and_then(|d| d.get("reasoning_content"))));
                if let Some(timings) = value.get("timings").filter(|t| truthy(Some(*t))) {
                    self.prompt_tokens = or_number(timings.get("prompt_n"), self.prompt_tokens);
                    self.prompt_per_second =
                        or_number(timings.get("prompt_per_second"), self.prompt_per_second);
                    self.prompt_ms = or_number(timings.get("prompt_ms"), self.prompt_ms);
                }
                if let Some(n) = value
                    .get("usage")
                    .and_then(|u| u.get("prompt_tokens"))
                    .filter(|v| truthy(Some(*v)))
                {
                    self.prompt_tokens = Some(js_number(n));
                }
            }
        }
        Feed::Continue { save }
    }

    /// The body ended: judged by the engine's measured prompt time, else the wall time.
    pub fn finish(self, now: f64, record: &mut Progress) -> LongOutcome {
        record.eta_seconds = None;
        let took = self.prompt_ms.unwrap_or(now - self.started_at);
        if took > self.budget_ms {
            return LongOutcome::OverBudget {
                predicted: false,
                eta_ms: took,
            };
        }
        LongOutcome::Passed {
            text: self.text,
            prompt_tokens: self.prompt_tokens,
            prompt_per_second: self.prompt_per_second,
            prompt_seconds: js_round(took / 100.0) / 10.0,
        }
    }
}

/// The step's reason for an over-budget long prompt (`stepOnce`).
pub fn over_budget_reason(predicted: bool, eta_ms: f64, budget_seconds: f64) -> String {
    let seconds = plain((eta_ms / 1000.0).ceil());
    let budget = plain(budget_seconds);
    if predicted {
        format!("Filling this context would take about {seconds} s, over the {budget} s limit.")
    } else {
        format!("Filling this context took {seconds} s, over the {budget} s limit.")
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    /// A synthetic engine: loads pass up to `load_max`, long prompts up to `long_max`.
    struct Synthetic {
        load_max: f64,
        long_max: f64,
        seen: Vec<(f64, Kind)>,
    }
    impl Steps for Synthetic {
        async fn step(&mut self, ctx: f64, kind: Kind) -> Result<bool, Stop> {
            self.seen.push((ctx, kind));
            Ok(match kind {
                Kind::Load => ctx <= self.load_max,
                Kind::Long => ctx <= self.long_max,
            })
        }
    }

    type Run = (Result<(f64, f64), Stop>, Vec<(f64, Kind)>);

    fn run(native: f64, load_max: f64, long_max: f64) -> Run {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut s = Synthetic {
            load_max,
            long_max,
            seen: Vec::new(),
        };
        let grid = ladder(native);
        let r = rt.block_on(async {
            let load = bound_load(&grid, &mut s).await?;
            let chosen = verify_long(&grid, load, 120.0, &mut s).await?;
            Ok((load, chosen))
        });
        (r, s.seen)
    }

    #[test]
    fn ladder_matches_node_bounds() {
        let grid = ladder(32768.0);
        assert_eq!(grid.first(), Some(&4096.0));
        assert_eq!(grid.last(), Some(&32768.0));
        assert!(grid.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(ladder(0.0).last(), Some(&131072.0));
        assert_eq!(ladder(20000.0).last(), Some(&20000.0));
        assert!(ladder(2048.0).is_empty());
        assert_eq!(start_of(&ladder(4096.0)), Some(4096.0));
    }

    #[test]
    fn load_doubles_then_bisects_and_long_goes_middle_out() {
        // Node: 8192 → 16384 → 32768 (fail) → between 16384..32768 → ... then long middle-out.
        let (r, seen) = run(65536.0, 24576.0, 16384.0);
        let (load, chosen) = r.unwrap();
        assert_eq!(load, 24576.0);
        assert_eq!(chosen, 16384.0);
        assert_eq!(
            &seen[..3],
            &[
                (8192.0, Kind::Load),
                (16384.0, Kind::Load),
                (32768.0, Kind::Load)
            ]
        );
        assert!(seen.iter().filter(|(_, k)| *k == Kind::Long).count() <= 7);
    }

    #[test]
    fn fatal_messages_are_node_text() {
        let (r, _) = run(65536.0, 0.0, 0.0);
        assert_eq!(
            r,
            Err(Stop::Fatal(
                "The model did not load even at 8,192 tokens.".into()
            ))
        );
        let (r, _) = run(65536.0, 65536.0, 0.0);
        assert_eq!(
            r,
            Err(Stop::Fatal(
                "No size passed the long-prompt test within 120 s, down to 8,192 tokens.".into()
            ))
        );
    }

    #[test]
    fn stream_stops_on_forecast_and_keeps_engine_words_out_of_reasons() {
        let mut clock = 0.0;
        let mut now = || {
            clock += 6000.0;
            clock
        };
        let mut lp = LongPrompt::new(10_000.0, 0.0);
        let mut rec = Progress::default();
        let event = br#"data: {"prompt_progress":{"total":100000,"processed":1000,"cache":0,"time_ms":1000}}

"#;
        match lp.feed(event, &mut now, &mut rec) {
            Feed::Stop(LongOutcome::OverBudget {
                predicted: true, ..
            }) => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(rec.progress, Some(1.0));
        let mut lp = LongPrompt::new(10_000.0, 0.0);
        let mut now = || 1.0;
        let error = b"data: {\"error\":{\"code\":500,\"message\":\"secret engine words\"}}\n\n";
        match lp.feed(error, &mut now, &mut rec) {
            Feed::Stop(LongOutcome::EngineError {
                status: Some(s),
                detail,
            }) => {
                assert_eq!(s, 500.0);
                assert_eq!(detail, "secret engine words");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stream_result_uses_measured_prompt_time() {
        let mut lp = LongPrompt::new(10_000.0, 0.0);
        let mut rec = Progress::default();
        let mut now = || 100.0;
        let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"CAL-1\"}}]}\n\ndata: {\"timings\":{\"prompt_n\":900,\"prompt_per_second\":450.4,\"prompt_ms\":2049}}\n\ndata: [DONE]\n\n";
        assert_eq!(
            lp.feed(body, &mut now, &mut rec),
            Feed::Continue { save: false }
        );
        assert_eq!(
            lp.finish(20_000.0, &mut rec),
            LongOutcome::Passed {
                text: "CAL-1".into(),
                prompt_tokens: Some(900.0),
                prompt_per_second: Some(450.4),
                prompt_seconds: 2.0,
            }
        );
    }

    #[test]
    fn interrupted_job_restores_only_through_its_own_revision_and_drops_private_text() {
        let JValue::Obj(fields) = wtf16::parse(r#"{"job":{"id":"j","model":"synthetic","status":"running","originalText":"[a]\n","lastRevision":"r1"},"history":{},"extra":1}"#)
            .unwrap() else { panic!("object") };
        let mut state = State(fields);
        let c = interrupt(state.job_mut().unwrap(), 5.0);
        assert_eq!(c, Some(Some(("r1".into(), "[a]\n".into()))));
        settle_interrupted(state.job_mut().unwrap(), Some(false));
        let text = wtf16::stringify(&state.to_value()).unwrap();
        assert!(!text.contains("originalText"));
        assert!(text.contains("\"extra\":1"));
        assert_eq!(
            state.job().get("error").as_str(),
            Some(format!("{STOPPED}{NOT_RESTORED}").as_str())
        );
        assert_eq!(state.job().get("status").as_str(), Some("interrupted"));
    }

    #[test]
    fn history_keeps_newest_ten_per_model() {
        let mut state = State::empty();
        for i in 0..12 {
            state.push_history("m", JValue::Num(f64::from(i)));
        }
        let JValue::Arr(list) = state.get("history").get("m") else {
            panic!("history");
        };
        assert_eq!(list.len(), HISTORY_PER_MODEL);
        assert_eq!(list[0], JValue::Num(11.0));
    }

    #[test]
    fn grouped_matches_en_us() {
        assert_eq!(grouped(8192.0), "8,192");
        assert_eq!(grouped(131072.0), "131,072");
        assert_eq!(grouped(512.0), "512");
        assert_eq!(
            over_budget_reason(true, 12_345.0, 120.0),
            "Filling this context would take about 13 s, over the 120 s limit."
        );
        assert_eq!(
            over_budget_reason(false, 2_000_000.0, 1800.0),
            "Filling this context took 2000 s, over the 1800 s limit."
        );
    }
}
