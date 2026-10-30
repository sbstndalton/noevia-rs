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
//!
//! A recovery key or token is never in a response body beyond that download, never in a log, and
//! never in the status file. Jobs run as spawned tasks the handler awaits, so a dropped request
//! never strands the busy mark, a half-written status or a Drive sign-in.
//!
//! Scheduling: every 15 minutes, a run starts when the server clock is within six hours after
//! `OFFSITE_BACKUP_HOUR`, the last backup is over 20 hours old and no quiet window is open (the
//! appdata backup 04:05-04:25 server time, the NAS 02:20-04:30 America/New_York); a scheduled run
//! also waits out a window that opens while it runs (offsite_backup_format::schedule).
use crate::gdrive::{now_ms, Drive, DriveState, Fail, Slot};
use crate::offsite_engine::{log, Engine, Pace};
use crate::offsite_settings::{KeyError, Settings, StatusFile};
use crate::offsite_store::{DirStore, S3Store, Store};
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{header, HeaderValue, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::JValue;
use offsite_backup_format::schedule::{self, Zone};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const ROOT: &str = "/api/admin/offsite-backup";
pub const ROUTES: &[&str] = &[
    "/api/admin/offsite-backup",
    "/api/admin/offsite-backup/copy",
    "/api/admin/offsite-backup/google/connect",
    "/api/admin/offsite-backup/google/disconnect",
    "/api/admin/offsite-backup/recovery-key",
    "/api/admin/offsite-backup/run",
    "/api/admin/offsite-backup/verify",
];
const OTHER_ADMIN: &str = "Another administrator is connecting Google Drive.";
const TICK: Duration = Duration::from_secs(15 * 60);

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

fn num(n: i64) -> JValue {
    JValue::Num(n as f64)
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
        }
    }

    fn busy_label(&self) -> String {
        self.busy.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Marks the service busy with `label`; the mark is cleared (back to what it was) on drop.
    fn begin(&self, label: &str, restore: &str) -> Result<Busy<'_>, Fail> {
        let mut b = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        if !b.is_empty() {
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
        let built = Arc::new(Built {
            engine: Arc::new(Engine::new(store, key.as_slice(), paths)?),
            dir,
        });
        *slot = Some(Arc::clone(&built));
        Ok(built)
    }

    fn record_error(&self, during: &str, e: &Fail) {
        let message = js_json::js_slice(
            if e.message.is_empty() {
                "failed"
            } else {
                &e.message
            },
            300,
        );
        self.status.patch(vec![(
            "lastError",
            JValue::obj([
                ("at", num(now_ms())),
                ("during", s(during)),
                ("message", JValue::Str(message)),
            ]),
        )]);
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
        let enabled = self.enabled(app).await;
        if let Some(reason) = self.reason(enabled) {
            return Err(Fail::new(reason, 409));
        }
        let _busy = self.begin(job.label(), "")?;
        let outcome = async {
            let built = self.build().await?;
            match job {
                Job::Backup => {
                    let snap = built.engine.backup(&pace).await?;
                    let retention = built.engine.forget().await?;
                    let last = JValue::obj([
                        ("at", num(now_ms())),
                        ("id", s(&snap.id)),
                        ("files", num(snap.files as i64)),
                        ("uploadedBytes", JValue::Num(snap.uploaded_bytes as f64)),
                    ]);
                    self.status.patch(vec![
                        ("lastBackup", last.clone()),
                        ("snapshots", num(retention.kept as i64)),
                        ("lastError", JValue::Null),
                    ]);
                    Ok(last)
                }
                Job::Verify => {
                    let (id, files, at) = built.engine.verify(&std::env::temp_dir()).await?;
                    let last = JValue::obj([
                        ("id", s(&id)),
                        ("files", num(files as i64)),
                        ("verifiedAt", num(at)),
                    ]);
                    self.status.patch(vec![
                        ("lastVerify", last.clone()),
                        ("lastError", JValue::Null),
                    ]);
                    Ok(last)
                }
            }
        }
        .await;
        if let Err(e) = &outcome {
            self.record_error(job.label(), e);
        }
        outcome
    }

    /// `runNow()`: a snapshot and retention, then (in the background) the copy to Drive.
    pub async fn run_now(self: &Arc<Self>, app: &Arc<App>, pace: Pace) -> Result<JValue, Fail> {
        let saved = self.exclusive(app, Job::Backup, pace.clone()).await?;
        // Start the mirror only after the backup mark was released: the upload runs in the
        // background, so a slow Drive connection cannot hold the response.
        if self.busy_label().is_empty() {
            let me = Arc::clone(self);
            tokio::spawn(async move {
                let _ = me.copy_to_drive(&pace).await;
            });
        }
        Ok(saved)
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
            crate::offsite_mirror::mirror(&self.drive, dir, crate::offsite_mirror::MAX_DELETE, pace)
                .await
        }
        .await;
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

    /// One check of the schedule against `clock` (Date.now(), replaceable in tests): whether a
    /// scheduled run was due, outside the quiet windows, and was made.
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
        let (zone, _) = Zone::from_tz(self.settings.tz.as_deref());
        let now = clock();
        if schedule::quiet_until(zone, now).is_some() {
            return false;
        }
        let last = self
            .status
            .read()
            .get("lastBackup")
            .get("at")
            .as_num()
            .unwrap_or(0.0) as i64;
        if !schedule::due(zone, now, self.settings.hour, last) {
            return false;
        }
        let mut pace = Pace::scheduled(zone);
        pace.clock = clock;
        let waiting = Arc::clone(&self.waiting);
        pace.waiting = Arc::new(move |w| waiting.store(w, std::sync::atomic::Ordering::Relaxed));
        if let Err(e) = self.run_now(app, pace).await {
            log(
                "offsite.failed",
                vec![("message", s(&js_json::js_slice(&e.message, 200)))],
            );
        }
        true
    }

    /// Checks every 15 minutes; runs once a day in the configured hour (and the grace after it).
    pub fn spawn_scheduler(self: &Arc<Self>, app: &Arc<App>) {
        if let (_, false) = Zone::from_tz(self.settings.tz.as_deref()) {
            eprintln!("noevia-server: TZ is not a zone the off-site backup schedule knows; it reads the server clock as UTC");
        }
        let (me, app) = (Arc::clone(self), Arc::clone(app));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
            loop {
                ticker.tick().await;
                me.tick_at(&app, Arc::new(now_ms)).await;
            }
        });
    }
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
