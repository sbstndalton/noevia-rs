//! The off-site backup (sbstndalton/noevia#2190 round 7), moved from core `offsite-service.cjs` and
//! `routes/offsite-backup.cjs`: the status, "Run now", the restore test, the copy to Google Drive,
//! the Google connect and disconnect, and the recovery-key download, administrators only. The Rust
//! front owns the jobs, their one-at-a-time rule, the nightly schedule, the status file and the
//! snapshot store; core refuses every method of these paths (`RUST_OFFSITE_BACKUP_OWNED`).
//!
//! - `GET  /api/admin/offsite-backup`                   the status (Node's shape, `google.owner` removed)
//! - `POST /api/admin/offsite-backup/{run,verify,copy}`  a snapshot + retention, the restore test, the copy to Drive
//! - `POST /api/admin/offsite-backup/google/{connect,disconnect}`
//! - `GET  /api/admin/offsite-backup/recovery-key`      the key as a text file download
//! - `POST /api/admin/offsite-backup/new-store`         "this destination is a new store": forgets the
//!   recorded destination and last backup so a legitimately empty destination can be initialised
//!
//! A recovery key or token is never in a response body beyond that download, never in a log, and
//! never in the status file. Jobs run as spawned tasks the handler awaits, so a dropped request
//! never strands the busy mark, a half-written status or a Drive sign-in.
//!
//! Scheduling: every 15 minutes (a slow check delays the next one rather than being followed by a
//! burst), a run starts when the clock is within six hours after `OFFSITE_BACKUP_HOUR`, the last
//! backup is over 20 hours old, no quiet window is open and the last failure's backoff (15 minutes
//! doubling to 2 hours, three scheduled attempts per window) is over. Two zones are named, never
//! guessed: the hour is read on the process clock as core's `getHours()` did (`TZ`, UTC without
//! one); the quiet windows are the NAS's 02:20-04:30 America/New_York and the appdata backup's
//! 04:05-04:25 on the host's clock (`OFFSITE_BACKUP_TIMEZONE`, default America/New_York). A scheduled
//! run also waits out a window that opens while it runs, between files
//! (offsite_backup_format::schedule).
//!
//! A job (a backup with its retention, the restore test, the copy to Drive) holds the one-at-a-time
//! mark and an exclusive lock on `<data dir>/offsite-backup.lock`, and ends at the latest after
//! [`JOB_DEADLINE`] (6 hours): a NAS that hangs a write can hold neither the mark nor the lock
//! forever. The backup is recorded as soon as its snapshot is stored (`lastBackup`, with whether it
//! is `incomplete` and how many files were `skipped`); a failure of the retention that follows is
//! recorded on its own and does not count as a failed backup.
use crate::gdrive::{now_ms, Drive, DriveState, Fail, Slot};
use crate::offsite_engine::{log, Engine, Pace, LOCK_HELD};
use crate::offsite_settings::{KeyError, Settings, StatusFile};
use crate::offsite_store::{DirStore, S3Store, Store};
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{header, HeaderValue, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::JValue;
use offsite_backup_format::schedule;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const ROOT: &str = "/api/admin/offsite-backup";
pub const ROUTES: &[&str] = &[
    "/api/admin/offsite-backup",
    "/api/admin/offsite-backup/copy",
    "/api/admin/offsite-backup/google/connect",
    "/api/admin/offsite-backup/google/disconnect",
    "/api/admin/offsite-backup/new-store",
    "/api/admin/offsite-backup/recovery-key",
    "/api/admin/offsite-backup/run",
    "/api/admin/offsite-backup/verify",
];
const OTHER_ADMIN: &str = "Another administrator is connecting Google Drive.";
const TICK: Duration = Duration::from_secs(15 * 60);
/// The longest a backup, the restore test or the copy to Drive may run before it is stopped.
pub const JOB_DEADLINE: Duration = Duration::from_secs(6 * 3600);
/// The lock file every job holds (in the data directory, left out of the backup).
pub const LOCK_FILE: &str = "offsite-backup.lock";

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

fn num(n: i64) -> JValue {
    JValue::Num(n as f64)
}

/// "6 hours", "90 seconds": a deadline in the words of an error message.
fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 && secs.is_multiple_of(3600) {
        let h = secs / 3600;
        format!("{h} hour{}", if h == 1 { "" } else { "s" })
    } else if secs >= 1 {
        format!("{secs} second{}", if secs == 1 { "" } else { "s" })
    } else {
        format!("{} ms", d.as_millis())
    }
}

/// Only short strings and numbers survive: the params end up in a page the client words.
fn clean_params(params: &[(String, JValue)]) -> JValue {
    let mut out = Vec::new();
    for (k, v) in params.iter().take(8) {
        if k.is_empty() || k.len() > 20 || !k.bytes().all(|b| b.is_ascii_alphabetic()) {
            continue;
        }
        match v {
            JValue::Num(_) => out.push((k.clone(), v.clone())),
            JValue::Str(t) => out.push((k.clone(), JValue::Str(js_json::js_slice(t, 80)))),
            _ => {}
        }
    }
    JValue::Obj(out)
}

/// `mirrorView(raw, now)`: the copy's last result as the page shows it; a copy that has not
/// succeeded for two days is stale, whatever it last said.
pub fn mirror_view(raw: &JValue, now: i64) -> JValue {
    if !raw.truthy() {
        return JValue::obj([
            ("state", s("unknown")),
            ("at", JValue::Null),
            ("message", s("The copy to Google Drive has not run yet.")),
        ]);
    }
    let state = raw
        .get("state")
        .as_str()
        .filter(|t| ["ok", "waiting", "refused", "failed"].contains(t))
        .unwrap_or("unknown");
    let at = raw.get("at").as_num().filter(|n| n.is_finite());
    let message = raw
        .get("message")
        .as_str()
        .map(|m| js_json::js_slice(m, 300))
        .unwrap_or_default();
    let ids: Vec<(&str, JValue)> = match raw.get("messageId").as_str() {
        Some(id) if (1..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphabetic()) => {
            let params = match raw.get("messageParams") {
                JValue::Obj(p) => clean_params(p),
                _ => JValue::Obj(Vec::new()),
            };
            vec![("messageId", s(id)), ("messageParams", params)]
        }
        _ => Vec::new(),
    };
    if state == "ok" && at.is_some_and(|a| now as f64 - a > 2.0 * 86_400_000.0) {
        return JValue::obj([
            ("state", s("stale")),
            ("at", JValue::Num(at.unwrap_or(0.0))),
            (
                "message",
                s("The last copy to Drive is more than two days old."),
            ),
            ("messageId", s("stale")),
            ("messageParams", JValue::Obj(Vec::new())),
        ]);
    }
    let mut out = vec![
        ("state", s(state)),
        ("at", at.map_or(JValue::Null, JValue::Num)),
        ("message", JValue::Str(message)),
    ];
    out.extend(ids);
    JValue::obj(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Backup,
    Verify,
}

impl Job {
    fn label(self) -> &'static str {
        match self {
            Job::Backup => "backup",
            Job::Verify => "restore test",
        }
    }
}

struct Built {
    engine: Arc<Engine>,
    dir: Option<DirStore>,
    /// A digest naming the destination (never the credentials), so a changed destination is not
    /// taken for an unmounted one.
    dest_id: String,
}

pub struct Service {
    pub settings: Arc<Settings>,
    pub drive: Arc<Drive>,
    status: StatusFile,
    busy: Mutex<String>,
    built: tokio::sync::Mutex<Option<Arc<Built>>>,
    mirror_lock: tokio::sync::Mutex<()>,
    /// Whether a scheduled run is waiting out a quiet window (the status shows it).
    waiting: Arc<std::sync::atomic::AtomicBool>,
    /// The job deadline in milliseconds ([`JOB_DEADLINE`]; shortened by tests).
    deadline_ms: std::sync::atomic::AtomicU64,
}

struct Busy<'a>(&'a Service, String);
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        *self.0.busy.lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.1);
    }
}

impl Service {
    pub fn new(settings: Arc<Settings>, drive: Arc<Drive>) -> Service {
        Service {
            status: StatusFile::new(&settings.data_dir),
            settings,
            drive,
            busy: Mutex::new(String::new()),
            built: tokio::sync::Mutex::new(None),
            mirror_lock: tokio::sync::Mutex::new(()),
            waiting: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            deadline_ms: std::sync::atomic::AtomicU64::new(
                u64::try_from(JOB_DEADLINE.as_millis()).unwrap_or(u64::MAX),
            ),
        }
    }

    /// Shortens the job deadline (tests; production keeps [`JOB_DEADLINE`]).
    pub fn set_deadline(&self, d: Duration) {
        self.deadline_ms.store(
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX).max(1),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn deadline(&self) -> Duration {
        Duration::from_millis(self.deadline_ms.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn busy_label(&self) -> String {
        self.busy.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Marks the service busy with `label`; the mark is cleared (back to what it was) on drop.
    fn begin(&self, label: &str, restore: &str) -> Result<Busy<'_>, Fail> {
        let mut b = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        if !b.is_empty() {
            if self.waiting.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Fail::new(
                    format!(
                        "A scheduled {} is waiting for a quiet window to end and will carry on by itself.",
                        *b
                    ),
                    409,
                ));
            }
            return Err(Fail::new(format!("A {} is already running.", *b), 409));
        }
        *b = label.to_owned();
        Ok(Busy(self, restore.to_owned()))
    }

    /// `features.enabled('offsiteBackup')`, read per use.
    pub async fn enabled(&self, app: &Arc<App>) -> bool {
        let (a, env) = (Arc::clone(app), self.settings.feature_env);
        tokio::task::spawn_blocking(move || {
            let spec = server_features::spec(server_features::Feature::OffsiteBackup);
            let stored = a.writes.writer().ok().and_then(|w| {
                w.read(|r| {
                    r.row(
                        "SELECT CASE WHEN typeof(value)='text' AND value IN ('true','false') THEN value ELSE NULL END FROM settings WHERE key=?1",
                        [spec.setting],
                        |row| row.get::<_, Option<String>>(0),
                    )
                })
                .ok()
                .flatten()
                .flatten()
            });
            server_features::evaluate(server_features::Feature::OffsiteBackup, env, stored.as_deref(), true).enabled
        })
        .await
        .unwrap_or(false)
    }

    fn reason(&self, enabled: bool) -> Option<String> {
        if !enabled {
            return Some("Off-site backups are turned off (Settings → Features).".into());
        }
        let gaps = self.settings.missing();
        (!gaps.is_empty()).then(|| format!("Not configured: set {}.", gaps.join(", ")))
    }

    fn reason_code(&self, enabled: bool) -> Option<&'static str> {
        if !enabled {
            Some("off")
        } else if !self.settings.missing().is_empty() {
            Some("notConfigured")
        } else {
            None
        }
    }

    /// Core's `driveView()`: the connection and the copy's last result, with `owner`.
    fn drive_view(&self) -> Option<(DriveState, bool, JValue)> {
        if !self.settings.use_dir() {
            return None;
        }
        let g = self.drive.state(&Slot::Backup);
        let st = self.status.read();
        let copy_enabled = st.get("driveCopy").as_bool() != Some(false);
        let copy = if g.state == "connected" {
            mirror_view(st.get("mirror"), now_ms())
        } else {
            JValue::Null
        };
        Some((g, copy_enabled, copy))
    }

    fn google_js(&self, with_owner: bool) -> JValue {
        match self.drive_view() {
            None => JValue::Null,
            Some((g, copy_enabled, copy)) => {
                let JValue::Obj(mut p) = g.to_js(with_owner) else {
                    return JValue::Null;
                };
                p.push(("copyEnabled".into(), JValue::Bool(copy_enabled)));
                p.push(("copy".into(), copy));
                JValue::Obj(p)
            }
        }
    }

    /// `status()`; `with_owner` keeps `google.owner` (the route strips it for the page).
    pub async fn status(&self, app: &Arc<App>, with_owner: bool) -> JValue {
        let enabled = self.enabled(app).await;
        let reason = self.reason(enabled);
        let code = self.reason_code(enabled);
        let st = self.status.read();
        let st_or_null = |k: &str| {
            let v = st.get(k);
            if v.truthy() {
                v.clone()
            } else {
                JValue::Null
            }
        };
        let dir = self.settings.dir();
        let hour = self.settings.hour;
        let r = offsite_backup_format::RETENTION;
        let destination = match (&dir, self.settings.s3_endpoint()) {
            (Some(d), _) => JValue::Str(match self.settings.mirror() {
                Some(m) => format!("Folder {d}, mirrored to {m}"),
                None => format!("Folder {d}"),
            }),
            (None, Some(e)) => match url::Url::parse(e) {
                Ok(u) => {
                    let host = match u.port() {
                        Some(p) => format!("{}:{p}", u.host_str().unwrap_or_default()),
                        None => u.host_str().unwrap_or_default().to_owned(),
                    };
                    let bucket = self.settings.text("OFFSITE_BACKUP_S3_BUCKET");
                    JValue::Str(format!(
                        "{host} / {}",
                        if bucket.is_empty() { "?" } else { &bucket }
                    ))
                }
                Err(_) => JValue::Null,
            },
            _ => JValue::Null,
        };
        let busy = self.busy_label();
        let mut out = vec![
            ("enabled", JValue::Bool(enabled)),
            ("ready", JValue::Bool(reason.is_none())),
            ("reason", reason.as_deref().map_or(JValue::Null, s)),
            ("reasonCode", code.map_or(JValue::Null, s)),
            (
                "reasonGaps",
                JValue::Arr(if code == Some("notConfigured") {
                    self.settings.missing().into_iter().map(s).collect()
                } else {
                    Vec::new()
                }),
            ),
            (
                "busy",
                if busy.is_empty() {
                    JValue::Null
                } else {
                    JValue::Str(busy)
                },
            ),
            (
                "schedule",
                JValue::Str(format!("Daily at {hour:02}:00 (server time)")),
            ),
            ("scheduleHour", num(i64::from(hour))),
            (
                "retention",
                JValue::Str(format!(
                    "Keeps {} daily, {} weekly and {} monthly snapshots",
                    r.daily, r.weekly, r.monthly
                )),
            ),
            (
                "retentionPolicy",
                JValue::obj([
                    ("daily", num(r.daily as i64)),
                    ("weekly", num(r.weekly as i64)),
                    ("monthly", num(r.monthly as i64)),
                ]),
            ),
            (
                "destinationFolder",
                match &dir {
                    Some(d) => JValue::obj([
                        ("path", s(d)),
                        (
                            "mirror",
                            self.settings.mirror().map_or(JValue::Null, JValue::Str),
                        ),
                    ]),
                    None => JValue::Null,
                },
            ),
            ("destination", destination),
            ("google", self.google_js(with_owner)),
            ("paths", num(self.settings.paths.len() as i64)),
            ("lastBackup", st_or_null("lastBackup")),
            ("lastVerify", st_or_null("lastVerify")),
            ("lastError", st_or_null("lastError")),
            (
                "snapshots",
                match st.get("snapshots") {
                    JValue::Num(n) => JValue::Num(*n),
                    _ => JValue::Null,
                },
            ),
        ];
        // Fields core's status never had are added only when they say something, so the answer of a
        // feature that is off (what the recorded corpus holds) stays core's byte for byte: the zone
        // the hour is read in once the feature is on, the unreadable count once a retention counted.
        if enabled {
            out.push(("scheduleZone", s(self.settings.hour_zone().0.name())));
        }
        if let JValue::Num(n) = st.get("unreadableSnapshots") {
            out.push(("unreadableSnapshots", JValue::Num(*n)));
        }
        if self.waiting.load(std::sync::atomic::Ordering::Relaxed) {
            out.push(("waitingForQuietWindow", JValue::Bool(true)));
        }
        JValue::obj(out)
    }

    async fn build(&self) -> Result<Arc<Built>, Fail> {
        let mut slot = self.built.lock().await;
        if let Some(b) = slot.as_ref() {
            return Ok(Arc::clone(b));
        }
        let (store, dir) = if let Some(d) = self.settings.dir() {
            let real = self.settings.check_destination(&d)?;
            let dir = DirStore::new(&real)?;
            (Store::Dir(dir.clone()), Some(dir))
        } else {
            let t = |k: &str| self.settings.text(k);
            let region = match t("OFFSITE_BACKUP_S3_REGION") {
                r if r.is_empty() => "us-east-1".to_owned(),
                r => r,
            };
            let prefix = match self.settings.text("OFFSITE_BACKUP_S3_PREFIX") {
                p if p.is_empty() => "noevia-backup".to_owned(),
                p => p,
            };
            (
                Store::S3(S3Store::new(
                    &t("OFFSITE_BACKUP_S3_ENDPOINT"),
                    &t("OFFSITE_BACKUP_S3_BUCKET"),
                    &region,
                    &t("OFFSITE_BACKUP_S3_ACCESS_KEY_ID"),
                    &t("OFFSITE_BACKUP_S3_SECRET_ACCESS_KEY"),
                    &prefix,
                )?),
                None,
            )
        };
        let key = self.settings.load_key().map_err(KeyError::fail)?;
        let paths = self
            .settings
            .paths
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
        let dest_id = {
            use sha2::{Digest, Sha256};
            let descriptor = match self.settings.dir() {
                Some(d) => format!("dir:{d}"),
                None => format!(
                    "s3:{}|{}|{}",
                    self.settings.text("OFFSITE_BACKUP_S3_ENDPOINT"),
                    self.settings.text("OFFSITE_BACKUP_S3_BUCKET"),
                    self.settings.text("OFFSITE_BACKUP_S3_PREFIX")
                ),
            };
            crate::gdrive::hex_of(&Sha256::digest(descriptor.as_bytes()))
                .chars()
                .take(16)
                .collect::<String>()
        };
        let built = Arc::new(Built {
            engine: Arc::new(
                Engine::new(store, key.as_slice(), paths)?
                    .with_lock_file(self.settings.data_dir.join(LOCK_FILE)),
            ),
            dir,
            dest_id,
        });
        *slot = Some(Arc::clone(&built));
        Ok(built)
    }

    fn failure_text(e: &Fail) -> String {
        js_json::js_slice(
            if e.message.is_empty() {
                "failed"
            } else {
                &e.message
            },
            300,
        )
    }

    /// The last failed backup, as the status file keeps it for the schedule's backoff.
    fn retry_of(&self) -> Option<schedule::Retry> {
        let st = self.status.read();
        let r = st.get("backupRetry");
        if !r.is_object() {
            return None;
        }
        let whole = |k: &str| r.get(k).as_num().filter(|n| n.is_finite() && *n >= 0.0);
        Some(schedule::Retry {
            at: whole("at")? as i64,
            failures: whole("failures")? as u32,
            attempts: whole("attempts").unwrap_or(0.0) as u32,
        })
    }

    /// A failed restore test or copy, or a backup that failed before its snapshot was stored.
    fn record_failure(&self, job: Job, e: &Fail, pace: &Pace) {
        let at = (pace.clock)();
        let mut error = vec![
            ("at", num(at)),
            ("during", s(job.label())),
            ("message", JValue::Str(Self::failure_text(e))),
        ];
        let mut patch = Vec::new();
        if job == Job::Backup {
            let zone = self.settings.hour_zone().0;
            let retry = schedule::Retry::failed(self.retry_of(), zone, at, pace.is_scheduled());
            error.push(("failures", num(i64::from(retry.failures))));
            patch.push((
                "backupRetry",
                JValue::obj([
                    ("at", num(retry.at)),
                    ("failures", num(i64::from(retry.failures))),
                    ("attempts", num(i64::from(retry.attempts))),
                ]),
            ));
        }
        patch.push(("lastError", JValue::obj(error)));
        self.status.patch(patch);
    }

    /// `exclusive(label, work)` for a backup or the restore test.
    pub async fn exclusive(
        self: &Arc<Self>,
        app: &Arc<App>,
        job: Job,
        pace: Pace,
    ) -> Result<JValue, Fail> {
        let (me, app) = (Arc::clone(self), Arc::clone(app));
        match tokio::spawn(async move { me.exclusive_task(&app, job, pace).await }).await {
            Ok(r) => r,
            Err(_) => Err(Fail::new(
                "The backup destination could not be reached.",
                502,
            )),
        }
    }

    async fn exclusive_task(
        self: &Arc<Self>,
        app: &Arc<App>,
        job: Job,
        pace: Pace,
    ) -> Result<JValue, Fail> {
        self.exclusive_inner(app, job, pace).await.0
    }

    /// [`Service::exclusive_task`], and whether a snapshot was stored (and so recorded) before the
    /// outcome, which can still be an error: retention that did not finish, or hit the deadline.
    async fn exclusive_inner(
        self: &Arc<Self>,
        app: &Arc<App>,
        job: Job,
        pace: Pace,
    ) -> (Result<JValue, Fail>, bool) {
        let enabled = self.enabled(app).await;
        if let Some(reason) = self.reason(enabled) {
            return (Err(Fail::new(reason, 409)), false);
        }
        let _busy = match self.begin(job.label(), "") {
            Ok(b) => b,
            Err(e) => return (Err(e), false),
        };
        let saved = std::sync::atomic::AtomicBool::new(false);
        let deadline = self.deadline();
        let outcome = match tokio::time::timeout(deadline, self.run_job(job, &pace, &saved)).await {
            Ok(r) => r,
            Err(_) => Err(Fail::new(
                format!(
                    "The {} did not finish within {} and was stopped.",
                    job.label(),
                    human_duration(deadline)
                ),
                504,
            )),
        };
        (pace.waiting)(false);
        let saved = saved.load(std::sync::atomic::Ordering::SeqCst);
        if let Err(e) = &outcome {
            if e.status == 409 && e.message == LOCK_HELD {
                // Another process's job holds the lock: nothing of ours failed, so there is no
                // failure to record and no backoff to start (the schedule asks again next tick).
            } else if saved {
                // The snapshot is stored and recorded; only what came after it failed.
                self.status.patch(vec![(
                    "lastError",
                    JValue::obj([
                        ("at", num((pace.clock)())),
                        ("during", s("backup")),
                        (
                            "message",
                            JValue::Str(js_json::js_slice(
                                &format!(
                                    "The snapshot was saved, but applying retention did not finish: {}",
                                    Self::failure_text(e)
                                ),
                                300,
                            )),
                        ),
                    ]),
                )]);
            } else {
                self.record_failure(job, e, &pace);
            }
        }
        (outcome, saved)
    }

    async fn run_job(
        &self,
        job: Job,
        pace: &Pace,
        saved: &std::sync::atomic::AtomicBool,
    ) -> Result<JValue, Fail> {
        let built = self.build().await?;
        // One job at a time across processes too; held until this function returns.
        let _lock = built.engine.lock_job()?;
        match job {
            Job::Backup => {
                let expect_store = {
                    let st = self.status.read();
                    st.get("lastBackup").truthy()
                        && st
                            .get("backupDestination")
                            .as_str()
                            .is_none_or(|d| d == built.dest_id)
                };
                let snap = built.engine.backup_expecting(pace, expect_store).await?;
                // Recorded now, before retention: a retention failure must not look like a failed
                // backup and be repeated all night (every repeat would add a snapshot).
                let last = JValue::obj([
                    ("at", num((pace.clock)())),
                    ("id", s(&snap.id)),
                    ("files", num(snap.files as i64)),
                    ("uploadedBytes", JValue::Num(snap.uploaded_bytes as f64)),
                    ("incomplete", JValue::Bool(snap.incomplete)),
                    ("skipped", num(snap.skipped as i64)),
                ]);
                self.status.patch(vec![
                    ("lastBackup", last.clone()),
                    ("backupDestination", s(&built.dest_id)),
                    ("backupRetry", JValue::Null),
                    ("lastError", JValue::Null),
                ]);
                saved.store(true, std::sync::atomic::Ordering::SeqCst);
                // Retention runs outside a quiet window too.
                pace.checkpoint().await;
                match built.engine.forget(std::slice::from_ref(&snap.id)).await {
                    Ok(retention) => {
                        self.status.patch(vec![
                            ("snapshots", num(retention.kept as i64)),
                            ("unreadableSnapshots", num(retention.unreadable as i64)),
                        ]);
                    }
                    Err(e) => {
                        self.status.patch(vec![(
                            "lastError",
                            JValue::obj([
                                ("at", num((pace.clock)())),
                                ("during", s("backup")),
                                (
                                    "message",
                                    JValue::Str(js_json::js_slice(
                                        &format!(
                                            "The snapshot was saved, but applying retention failed: {}",
                                            Self::failure_text(&e)
                                        ),
                                        300,
                                    )),
                                ),
                            ]),
                        )]);
                        log(
                            "offsite.retention.failed",
                            vec![("message", s(&js_json::js_slice(&e.message, 200)))],
                        );
                    }
                }
                Ok(last)
            }
            Job::Verify => {
                let (id, files, at) = built.engine.verify().await?;
                let last = JValue::obj([
                    ("id", s(&id)),
                    ("files", num(files as i64)),
                    ("verifiedAt", num(at)),
                ]);
                // A restore test that passes clears its own failure, not a backup's: the last
                // backup's error stays until a backup works.
                let mut patch = vec![("lastVerify", last.clone())];
                if self.status.read().get("lastError").get("during").as_str()
                    == Some(Job::Verify.label())
                {
                    patch.push(("lastError", JValue::Null));
                }
                self.status.patch(patch);
                Ok(last)
            }
        }
    }

    /// `runNow()`: a snapshot and retention, then (in the background) the copy to Drive. The whole
    /// of it, the copy's kick-off included, is one spawned task the handler awaits, so a client that
    /// goes away cannot lose the copy it asked for.
    pub async fn run_now(self: &Arc<Self>, app: &Arc<App>, pace: Pace) -> Result<JValue, Fail> {
        let (me, app) = (Arc::clone(self), Arc::clone(app));
        let task = tokio::spawn(async move {
            let (outcome, saved) = me.exclusive_inner(&app, Job::Backup, pace.clone()).await;
            // Start the mirror only after the backup mark was released: the upload runs in the
            // background, so a slow Drive connection cannot hold the response. A stored snapshot is
            // copied even when what came after it (retention) failed or hit the deadline: the
            // snapshot is as good as any, and the next night's copy would be a day late.
            if saved && me.busy_label().is_empty() {
                let copier = Arc::clone(&me);
                tokio::spawn(async move {
                    let _ = copier.copy_to_drive(&pace).await;
                });
            }
            outcome
        });
        match task.await {
            Ok(r) => r,
            Err(_) => Err(Fail::new(
                "The backup destination could not be reached.",
                502,
            )),
        }
    }

    /// "This destination is a new store": forgets the destination the last backup was made to and
    /// the last backup itself (and the counts and failure that belong to that store), so that the
    /// refusal of an empty destination ([`EMPTY_DESTINATION`]) no longer applies and the next backup
    /// starts a new chain there. For a replaced disk or a recreated bucket at the same path or name,
    /// which no digest of the destination can tell from an unmounted one. Refused while a job runs,
    /// and when the destination holds a store (`config`) already: then it is not new, and nothing
    /// is changed. Runs as a spawned task, so a client that leaves cannot half-do it.
    pub async fn new_store(self: &Arc<Self>, app: &Arc<App>) -> Result<JValue, Fail> {
        let (me, app) = (Arc::clone(self), Arc::clone(app));
        match tokio::spawn(async move { me.new_store_task(&app).await }).await {
            Ok(r) => r,
            Err(_) => Err(Fail::new(
                "The backup destination could not be reached.",
                502,
            )),
        }
    }

    async fn new_store_task(self: &Arc<Self>, app: &Arc<App>) -> Result<JValue, Fail> {
        let enabled = self.enabled(app).await;
        if let Some(reason) = self.reason(enabled) {
            return Err(Fail::new(reason, 409));
        }
        let _busy = self.begin("new store", "")?;
        let deadline = self.deadline();
        let held = tokio::time::timeout(deadline, async {
            let built = self.build().await?;
            let _lock = built.engine.lock_job()?;
            if built.engine.has_config().await? {
                return Err(Fail::new(
                    "This destination already holds a backup store, so it is not a new one; nothing was changed.",
                    409,
                ));
            }
            Ok::<(), Fail>(())
        })
        .await
        .unwrap_or_else(|_| {
            Err(Fail::new(
                format!(
                    "The destination did not answer within {}.",
                    human_duration(deadline)
                ),
                504,
            ))
        });
        held?;
        let mut patch = vec![
            ("lastBackup", JValue::Null),
            ("backupDestination", JValue::Null),
            ("backupRetry", JValue::Null),
            ("snapshots", JValue::Null),
            ("unreadableSnapshots", JValue::Null),
        ];
        // The refusal this answers goes; any other failure stays until something works.
        let refused = self
            .status
            .read()
            .get("lastError")
            .get("message")
            .as_str()
            .is_some_and(|m| m.starts_with("The backup destination is empty"));
        if refused {
            patch.push(("lastError", JValue::Null));
        }
        self.status.patch(patch);
        log("offsite.new-store", vec![]);
        Ok(JValue::obj([("newStore", JValue::Bool(true))]))
    }

    /// `copyNow()`.
    pub async fn copy_now(self: &Arc<Self>, app: &Arc<App>) -> Result<JValue, Fail> {
        let enabled = self.enabled(app).await;
        if let Some(reason) = self.reason(enabled) {
            return Err(Fail::new(reason, 409));
        }
        let busy = self.busy_label();
        if !busy.is_empty() {
            return Err(Fail::new(format!("A {busy} is already running."), 409));
        }
        let me = Arc::clone(self);
        match tokio::spawn(async move { me.copy_to_drive(&Pace::none()).await }).await {
            Ok(Ok(Some(r))) => Ok(r),
            Ok(Ok(None)) => Ok(JValue::Null),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(Fail::new(
                "The backup destination could not be reached.",
                502,
            )),
        }
    }

    /// Copies the local encrypted store to Drive and records the result for the page.
    pub async fn copy_to_drive(self: &Arc<Self>, pace: &Pace) -> Result<Option<JValue>, Fail> {
        if !self.settings.use_dir()
            || self.drive.state(&Slot::Backup).state != "connected"
            || self.status.read().get("driveCopy").as_bool() == Some(false)
        {
            return Ok(None);
        }
        let Ok(_busy) = self.begin("copy to Google Drive", "") else {
            return Ok(None);
        };
        let _serial = self.mirror_lock.lock().await;
        let outcome = async {
            let built = self.build().await?;
            let dir = built
                .dir
                .as_ref()
                .ok_or_else(|| Fail::new("no folder", 409))?;
            let deadline = self.deadline();
            match tokio::time::timeout(
                deadline,
                crate::offsite_mirror::mirror(
                    &self.drive,
                    dir,
                    crate::offsite_mirror::MAX_DELETE,
                    pace,
                ),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(Fail::new(
                    format!(
                        "The copy to Drive did not finish within {} and was stopped.",
                        human_duration(deadline)
                    ),
                    504,
                )),
            }
        }
        .await;
        (pace.waiting)(false);
        match outcome {
            Ok(r) => {
                self.status.patch(vec![(
                    "mirror",
                    JValue::obj([
                        ("state", s("ok")),
                        ("at", num(now_ms())),
                        (
                            "message",
                            JValue::Str(format!("Copied {} snapshots.", r.snapshots)),
                        ),
                    ]),
                )]);
                Ok(Some(JValue::obj([
                    ("uploaded", num(r.uploaded as i64)),
                    ("bytes", JValue::Num(r.bytes as f64)),
                    ("removed", num(r.removed as i64)),
                    ("snapshots", num(r.snapshots as i64)),
                    ("folder", JValue::Str(r.folder)),
                    ("newFolder", JValue::Bool(r.new_folder)),
                ])))
            }
            Err(e) => {
                let message = js_json::js_slice(
                    if e.message.is_empty() {
                        "The copy to Drive did not finish."
                    } else {
                        &e.message
                    },
                    300,
                );
                let state = if message.contains("looks empty") {
                    "refused"
                } else if message.contains("Waiting") {
                    "waiting"
                } else {
                    "failed"
                };
                let mut pairs = vec![
                    ("state", s(state)),
                    ("at", num(now_ms())),
                    ("message", JValue::Str(message)),
                ];
                if let Some(id) = &e.id {
                    pairs.push(("messageId", s(id)));
                    pairs.push(("messageParams", clean_params(&e.params)));
                }
                self.status.patch(vec![("mirror", JValue::obj(pairs))]);
                log("gdrive.failed", vec![]);
                Err(e)
            }
        }
    }

    /// `connectGoogle(owner, onConnected)`.
    pub async fn connect_google(
        self: &Arc<Self>,
        owner: &str,
        hook: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<DriveState, Fail> {
        if !self.settings.use_dir() {
            return Err(Fail::new(
                "Google Drive needs a backup folder on this server (OFFSITE_BACKUP_DIR).",
                409,
            ));
        }
        self.build().await?;
        // Once approved, copy right away so the page can show the first result; the caller's hook
        // must not block that first copy.
        let me = Arc::clone(self);
        let handle = tokio::runtime::Handle::current();
        let after: Box<dyn FnOnce() + Send> = Box::new(move || {
            if let Some(h) = hook {
                h();
            }
            handle.spawn(async move {
                let _ = me.copy_to_drive(&Pace::none()).await;
            });
        });
        self.drive
            .connect(Slot::Backup, Some(owner.to_owned()), Some(after))
            .await
    }

    /// `setDriveCopy(on)`: whether backups are copied to the connected Drive (on unless turned off).
    pub async fn set_drive_copy(self: &Arc<Self>, on: bool) -> JValue {
        self.status.patch(vec![("driveCopy", JValue::Bool(on))]);
        if on
            && self.drive.state(&Slot::Backup).state == "connected"
            && self.busy_label().is_empty()
        {
            let me = Arc::clone(self);
            tokio::spawn(async move {
                let _ = me.copy_to_drive(&Pace::none()).await;
            });
        }
        self.google_js(true)
    }

    /// The copy settings for the Connectors page: `(copyEnabled, copy)`.
    pub fn copy_view(&self) -> (bool, JValue) {
        match self.drive_view() {
            Some((_, enabled, copy)) => (enabled, copy),
            None => (true, JValue::Null),
        }
    }

    pub fn last_backup(&self) -> JValue {
        let v = self.status.read().get("lastBackup").clone();
        if v.truthy() {
            v
        } else {
            JValue::Null
        }
    }

    /// The backup key, for the administrator to keep in a password manager.
    pub fn recovery_key(&self) -> Result<String, KeyError> {
        let key = self.settings.load_key()?;
        Ok(key.iter().map(|b| format!("{b:02x}")).collect())
    }

    // ── the nightly schedule ──

    /// The pace of a scheduled run: it waits out the quiet windows (the host's zone for the
    /// appdata window), tells the status when it does, and stamps the records with `clock`.
    pub fn scheduled_pace(&self, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Pace {
        let mut pace = Pace::scheduled(self.settings.quiet_zone().0);
        pace.clock = clock;
        let waiting = Arc::clone(&self.waiting);
        pace.waiting = Arc::new(move |w| waiting.store(w, std::sync::atomic::Ordering::Relaxed));
        pace
    }

    /// One check of the schedule against `clock` (Date.now(), replaceable in tests): whether a
    /// scheduled run was due, outside the quiet windows, and was made (whether or not it worked:
    /// a failure is recorded and the next attempt waits out its backoff).
    pub async fn tick_at(
        self: &Arc<Self>,
        app: &Arc<App>,
        clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> bool {
        if !self.busy_label().is_empty() {
            return false;
        }
        let enabled = self.enabled(app).await;
        if self.reason(enabled).is_some() {
            return false;
        }
        let (hour_zone, _) = self.settings.hour_zone();
        let (quiet_zone, _) = self.settings.quiet_zone();
        let now = clock();
        if schedule::quiet_until(quiet_zone, now).is_some() {
            return false;
        }
        let last = self
            .status
            .read()
            .get("lastBackup")
            .get("at")
            .as_num()
            .unwrap_or(0.0) as i64;
        if !schedule::due(hour_zone, now, self.settings.hour, last, self.retry_of()) {
            return false;
        }
        if let Err(e) = self.run_now(app, self.scheduled_pace(clock)).await {
            log(
                "offsite.failed",
                vec![("message", s(&js_json::js_slice(&e.message, 200)))],
            );
        }
        true
    }

    /// Checks every 15 minutes; runs once a day in the configured hour (and the grace after it).
    pub fn spawn_scheduler(self: &Arc<Self>, app: &Arc<App>) {
        let (hour_zone, hour_warning) = self.settings.hour_zone();
        let (quiet_zone, quiet_warning) = self.settings.quiet_zone();
        for warning in hour_warning.into_iter().chain(quiet_warning) {
            eprintln!("noevia-server: WARNING {warning}");
            log("offsite.schedule.warning", vec![("message", s(&warning))]);
        }
        log(
            "offsite.schedule",
            vec![
                ("hour", num(i64::from(self.settings.hour))),
                ("hourZone", s(hour_zone.name())),
                ("quietZone", s(quiet_zone.name())),
            ],
        );
        let (me, app) = (Arc::clone(self), Arc::clone(app));
        tokio::spawn(async move {
            let mut ticker = scheduler_interval(TICK);
            loop {
                ticker.tick().await;
                me.tick_at(&app, Arc::new(now_ms)).await;
            }
        });
    }
}

/// The schedule's timer: first check one period from now. A check that outlasts its period (a
/// backup is hours) delays the next one by a full period instead of firing the missed ones back
/// to back, which after a long failed run would be a burst of retries.
pub fn scheduler_interval(period: Duration) -> tokio::time::Interval {
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker
}

// ---------------------------------------------------------------------------------------------
// HTTP.

fn answer(status: u16, body: &JValue) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        js_json::stringify(body).unwrap_or_else(|| "{}".into()),
        true,
    )
}

fn error_body(e: &Fail) -> JValue {
    let mut p = vec![("error", JValue::Str(e.message.clone()))];
    if let Some(id) = &e.id {
        p.push(("errorId", s(id)));
        p.push(("errorParams", JValue::Obj(e.params.clone())));
    }
    JValue::obj(p)
}

/// Another administrator's sign-in is waiting for approval: its code belongs to them alone, and
/// approving it would bind the wrong Google account to the backups (#868).
fn foreign_pending(google: &JValue, user: &str) -> bool {
    google.get("state").as_str() == Some("pending")
        && google
            .get("owner")
            .as_str()
            .is_some_and(|o| !o.is_empty() && o != user)
}

fn without(google: &JValue, keys: &[&str]) -> JValue {
    match google {
        JValue::Obj(p) => JValue::Obj(
            p.iter()
                .filter(|(k, _)| !keys.contains(&k.as_str()))
                .cloned()
                .collect(),
        ),
        other => other.clone(),
    }
}

fn public_status(status: JValue, user: &str) -> JValue {
    let JValue::Obj(mut p) = status else {
        return status;
    };
    let Some(slot) = p.iter_mut().find(|(k, _)| k == "google") else {
        return JValue::Obj(p);
    };
    if !slot.1.is_object() {
        return JValue::Obj(p);
    }
    let foreign = foreign_pending(&slot.1, user);
    let mut g = without(&slot.1, &["owner"]);
    if foreign {
        g = without(&g, &["userCode", "verificationUrl", "expiresAt"]);
        if let JValue::Obj(gp) = &mut g {
            // Core: `{ ...rest, message: OTHER_ADMIN }` (message after the other keys).
            gp.retain(|(k, _)| k != "message");
            gp.push(("message".into(), s(OTHER_ADMIN)));
        }
    }
    slot.1 = g;
    JValue::Obj(p)
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let Some(service) = app.offsite.as_ref().map(Arc::clone) else {
        return app.proxy.forward(conn.peer, req).await;
    };
    if !ROUTES.contains(&path.as_str()) {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let now = now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || gate.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if id.user.role != server_auth::Role::Admin {
        return answer(403, &JValue::obj([("error", s("Administrator required"))]));
    }
    let user = id.user.id.clone();
    let method = parts.method;
    let not_allowed = || answer(405, &JValue::obj([("error", s("method not allowed"))]));
    if path == ROOT {
        return if method == Method::GET {
            answer(200, &public_status(service.status(app, true).await, &user))
        } else {
            not_allowed()
        };
    }
    let action = path
        .strip_prefix("/api/admin/offsite-backup/")
        .unwrap_or_default();
    if action == "recovery-key" {
        if method != Method::GET {
            return not_allowed();
        }
        return match service.recovery_key() {
            Ok(key) => {
                let mut res = Response::new(Body::from(format!(
                    "noevia backup recovery key\n\nKeep this in a password manager. Without it, no backup can be restored.\nDo not store it in the same Google Drive as the backups.\n\n{key}\n"
                )));
                let h = res.headers_mut();
                reply::security_headers(h, true);
                h.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                );
                h.insert(
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_static(
                        "attachment; filename=\"noevia-backup-recovery-key.txt\"",
                    ),
                );
                h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                res
            }
            Err(e) => answer(
                409,
                &JValue::obj([(
                    "error",
                    s(e.public().unwrap_or("The backup key could not be read.")),
                )]),
            ),
        };
    }
    if method != Method::POST {
        return not_allowed();
    }
    let foreign = |service: &Service| foreign_pending(&service.google_js(true), &user);
    let result: Result<JValue, Fail> = match action {
        "run" => service.run_now(app, Pace::none()).await,
        "verify" => service.exclusive(app, Job::Verify, Pace::none()).await,
        "copy" => service.copy_now(app).await,
        "new-store" => service.new_store(app).await,
        "google/connect" => {
            if foreign(&service) {
                return answer(409, &JValue::obj([("error", s(OTHER_ADMIN))]));
            }
            match service.connect_google(&user, None).await {
                Ok(g) => {
                    let google = g.to_js(true);
                    // Two connects can pass the check together; the later one then gets the first
                    // one's pending sign-in back: never another admin's code (#892).
                    if foreign_pending(&google, &user) {
                        return answer(409, &JValue::obj([("error", s(OTHER_ADMIN))]));
                    }
                    Ok(without(&google, &["owner"]))
                }
                Err(e) => Err(e),
            }
        }
        "google/disconnect" => {
            if foreign(&service) {
                return answer(409, &JValue::obj([("error", s(OTHER_ADMIN))]));
            }
            service
                .drive
                .disconnect(Slot::Backup)
                .await
                .map(|g| g.to_js(true))
        }
        _ => return answer(404, &JValue::obj([("error", s("not found"))])),
    };
    match result {
        Ok(v) => answer(
            200,
            &if matches!(v, JValue::Null) {
                JValue::Obj(Vec::new())
            } else {
                v
            },
        ),
        Err(e) => {
            let status = if e.status == 0 { 502 } else { e.status };
            let mut body = error_body(&e);
            if e.message.is_empty() {
                body = JValue::obj([("error", s("The backup destination could not be reached."))]);
            }
            answer(status, &body)
        }
    }
}
