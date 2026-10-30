//! Google Drive's token authority (sbstndalton/noevia#2190 round 7), ported from noevia-core's
//! `gdrive.cjs` (sign-in, sealed token file, refresh) and `drive-accounts.cjs` (whose Drive a
//! caller reaches). The Rust front owns all Google OAuth state: every account's device sign-in and
//! sealed refresh token (`google-drive-users/<id>.sealed`) and the off-site backup's slot
//! (`google-drive.sealed`, sealed under `OFFSITE_BACKUP_KEY_FILE`'s key), which doubles as the
//! administrator's personal connection (`ownsBackup`, [`owns_backup`]). Node's chat tools hold no
//! refresh token: they ask this broker (loopback internal store, `/v1/gdrive/{session,reject,
//! disconnect,forget}`, plus `view` and `policy`; connector_routes.rs) for a short-lived access token.
//!
//! The on-disk format is unchanged (`google-token-seal`: HKDF key, AES-256-GCM, base64 of
//! iv|tag|body), so a Node build can read what Rust saved. Concurrency:
//!
//! - one tokio mutex per slot serializes the refresh (single flight: a waiter uses the refresh that
//!   landed meanwhile). A refresh does not rewrite the file; the access token lives in memory,
//!   bound to the sign-in it was minted for ([`generation_of`]), so a disconnect or a newer sign-in
//!   during a refresh makes the result be discarded (the compare-and-swap of the ChatGPT broker);
//! - a short fence (a std mutex, never held across a network call) makes the file changes one step:
//!   a sign-in's "still wanted?" check plus its save, a disconnect's generation bump plus its delete;
//!   a disconnect therefore never waits for a refresh;
//! - every operation that changes state runs as a spawned task the handler awaits, and a sign-in's
//!   poll is a detached task, so a dropped request never strands a sign-in, a save or a revoke.
//!
//! After each committed change the broker rewrites `gdrive-connections.json` (no secret in it:
//! which accounts have a connection and in what state, and the backup slot's), whose file signal
//! tells Node's sync `connected(user)` and its short access-token cache to look again.
use crate::{identity, offsite_settings, App};
use js_json::JValue;
use rand_core::{OsRng, RngCore};
use server_auth::secrets::SecretKeys;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zeroize::Zeroizing;

pub const STATE_FILE: &str = "gdrive-connections.json";
pub const BACKUP_FILE: &str = "google-drive.sealed";
pub const USERS_DIR: &str = "google-drive-users";
const SCOPE: &str = "https://www.googleapis.com/auth/drive.file openid email";
const OAUTH: &str = "https://oauth2.googleapis.com";
const API: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3";
const TIMEOUT: Duration = Duration::from_secs(30);
const FORM_CAP: usize = 256 * 1024;
pub const JSON_CAP: usize = 8 * 1024 * 1024;

pub const NOT_REGISTERED: &str = "This noevia build has no Google sign-in registered.";
pub const NOT_CONNECTED: &str = "Google Drive is not connected.";
const REVOKED: &str = "Google no longer accepts this connection (access was removed in your Google account). Connect again.";
const UNREACHABLE: &str = "Google could not be reached to refresh the connection.";

// ---------------------------------------------------------------------------------------------
// Errors.

/// A failure with a public message and status. `id` and `params` name the message for the client to
/// translate (#624); the English text stays as the fallback.
#[derive(Debug, Clone)]
pub struct Fail {
    pub status: u16,
    pub message: String,
    pub id: Option<String>,
    pub params: Vec<(String, JValue)>,
    /// The broker's reason for a token refusal: `disconnected`, `reconnect` or `transient`.
    pub code: Option<&'static str>,
}

impl Fail {
    pub fn new(message: impl Into<String>, status: u16) -> Fail {
        Fail {
            status,
            message: message.into(),
            id: None,
            params: Vec::new(),
            code: None,
        }
    }

    pub fn with_id(mut self, id: &str, params: Vec<(&str, JValue)>) -> Fail {
        self.id = Some(id.to_owned());
        self.params = params.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
        self
    }

    fn coded(mut self, code: &'static str) -> Fail {
        self.code = Some(code);
        self
    }
}

// ---------------------------------------------------------------------------------------------
// Settings.

/// `GOOGLE_OAUTH_CLIENT_ID/SECRET` and the three endpoints. A different endpoint is accepted only
/// on a loopback address (a QA server), like the ChatGPT broker's; core took any URL.
#[derive(Clone)]
pub struct Settings {
    pub client_id: String,
    pub client_secret: Zeroizing<String>,
    pub oauth_base: String,
    pub api_base: String,
    pub upload_base: String,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("configured", &self.configured())
            .finish_non_exhaustive()
    }
}

fn loopback_url(raw: &str) -> Option<String> {
    let u = url::Url::parse(raw).ok()?;
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some()
    {
        return None;
    }
    let loopback = match u.host()? {
        url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    };
    loopback.then(|| raw.trim_end_matches('/').to_owned())
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Settings {
        let text = |k: &str| get(k).map(|v| v.trim().to_owned()).unwrap_or_default();
        let base = |k: &str, default: &str| {
            get(k)
                .filter(|v| !v.is_empty())
                .and_then(|v| loopback_url(&v))
                .unwrap_or_else(|| default.to_owned())
        };
        Settings {
            client_id: text("GOOGLE_OAUTH_CLIENT_ID"),
            client_secret: Zeroizing::new(text("GOOGLE_OAUTH_CLIENT_SECRET")),
            oauth_base: base("GOOGLE_OAUTH_BASE_URL", OAUTH),
            api_base: base("GOOGLE_DRIVE_API_BASE_URL", API),
            upload_base: base("GOOGLE_DRIVE_UPLOAD_BASE_URL", UPLOAD),
        }
    }

    pub fn configured(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty()
    }
}

// ---------------------------------------------------------------------------------------------
// Small helpers.

pub fn now_ms() -> i64 {
    identity::now_ms()
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    OsRng.fill_bytes(&mut b);
    b
}

pub fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes `text` to `file` with mode 0600 through a temp file and a rename.
pub fn write_atomic(file: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        u64::from_le_bytes(random::<8>())
    ));
    let tmp = PathBuf::from(tmp);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    out.write_all(text.as_bytes())?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(&tmp, file).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The email in an ID token, for display only (it came straight from Google over TLS).
pub fn email_of(id_token: &str) -> Option<String> {
    use base64::Engine as _;
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let v = js_json::parse(&String::from_utf8_lossy(&bytes)).ok()?;
    let email = v.get("email");
    email
        .truthy()
        .then(|| email.as_str().map(str::to_owned))
        .flatten()
}

/// A sign-in's identity for the compare-and-swap: a digest of the refresh token and the time it was
/// connected, never the token itself, and stable across a reseal under a new key.
pub fn generation_of(record: &JValue) -> String {
    let token = record.get("refreshToken").as_str().unwrap_or("");
    let at = js_json::to_js_string(record.get("connectedAt")).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.update([0]);
    h.update(at.as_bytes());
    hex_of(&h.finalize())
}

/// Whether an id is one core's `/^[\w-]+$/` accepts (a user id used in a file name).
pub fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// ---------------------------------------------------------------------------------------------
// Whose Drive.

/// What `drive-accounts.cjs` `ownsBackup` reads about the backup slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupFacts {
    pub usable: bool,
    /// `connected`, `error`, `pending`, or anything else (`disconnected`, `not-configured`).
    pub state: String,
    pub owner: Option<String>,
}

/// `ownsBackup(user)`: the backup slot is this administrator's Drive. Shared with Node's mirror of
/// the rule by `tests/fixtures/gdrive-owns-backup.v1.json`.
pub fn owns_backup(admin: bool, user: &str, backup: &BackupFacts, personal_file: bool) -> bool {
    if !admin || !backup.usable {
        return false;
    }
    match backup.state.as_str() {
        "connected" | "error" => backup.owner.as_deref().is_none_or(|o| o == user),
        // Someone's sign-in is waiting: only the administrator who started it may see its code or
        // finish it (a second administrator completing it would bind their Google account, #868). A
        // pending sign-in with no recorded initiator keeps the old rule.
        "pending" if backup.owner.is_some() => backup.owner.as_deref() == Some(user),
        // Nothing saved yet: an administrator connecting from Connectors fills the backup slot,
        // unless they already have a personal connection.
        _ => !personal_file,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Slot {
    Backup,
    User(String),
}

impl Slot {
    fn key(&self) -> String {
        match self {
            Slot::Backup => "backup".to_owned(),
            Slot::User(id) => format!("u:{id}"),
        }
    }

    fn file(&self, data_dir: &Path) -> PathBuf {
        match self {
            Slot::Backup => data_dir.join(BACKUP_FILE),
            Slot::User(id) => data_dir.join(USERS_DIR).join(format!("{id}.sealed")),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Memory and state.

struct Pending {
    user_code: String,
    verification_url: String,
    expires_at: i64,
    owner: Option<String>,
}

struct Access {
    token: Zeroizing<String>,
    until: i64,
    generation: String,
}

#[derive(Default)]
struct SlotMem {
    pending: Option<Pending>,
    poll_error: Option<String>,
    access: Option<Access>,
    /// Bumped by a new sign-in, a disconnect: a poll or a refresh from an older one stops.
    generation: u64,
}

/// `state()` of one connection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DriveState {
    pub configured: bool,
    pub state: &'static str,
    pub message: Option<String>,
    pub email: Option<String>,
    pub connected_at: Option<f64>,
    pub owner: Option<String>,
    pub user_code: Option<String>,
    pub verification_url: Option<String>,
    pub expires_at: Option<i64>,
}

impl DriveState {
    /// Core's `state()` object, in its key order; `owner` only when asked (the backup route strips it).
    pub fn to_js(&self, with_owner: bool) -> JValue {
        let s = |v: &str| JValue::Str(v.to_owned());
        let mut p: Vec<(&str, JValue)> = vec![
            ("configured", JValue::Bool(self.configured)),
            ("state", s(self.state)),
        ];
        match self.state {
            "not-configured" => p.push(("message", s(self.message.as_deref().unwrap_or_default()))),
            "pending" => {
                p.push(("userCode", s(self.user_code.as_deref().unwrap_or_default())));
                p.push((
                    "verificationUrl",
                    s(self.verification_url.as_deref().unwrap_or_default()),
                ));
                p.push((
                    "expiresAt",
                    JValue::Num(self.expires_at.unwrap_or(0) as f64),
                ));
                if with_owner {
                    p.push(("owner", self.owner.as_deref().map_or(JValue::Null, s)));
                }
            }
            "error" => p.push(("message", s(self.message.as_deref().unwrap_or_default()))),
            "connected" => {
                p.push(("email", self.email.as_deref().map_or(JValue::Null, s)));
                p.push((
                    "connectedAt",
                    self.connected_at.map_or(JValue::Null, JValue::Num),
                ));
                if with_owner {
                    p.push(("owner", self.owner.as_deref().map_or(JValue::Null, s)));
                }
            }
            _ => {
                if let Some(m) = &self.message {
                    p.push(("message", s(m)));
                }
            }
        }
        JValue::obj(p)
    }
}

enum Saved {
    /// No file (or it cannot be read at all).
    Missing,
    /// A file no key opens.
    Broken,
    Record(JValue),
}

struct SavedFile {
    saved: Saved,
}

// ---------------------------------------------------------------------------------------------
// The broker.

pub struct Drive {
    pub settings: Settings,
    pub off: Arc<offsite_settings::Settings>,
    client: Option<reqwest::Client>,
    mem: Mutex<HashMap<String, SlotMem>>,
    fence: Mutex<()>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    data_dir: PathBuf,
    previous_key_env: Option<Zeroizing<String>>,
    keys: std::sync::OnceLock<Arc<SecretKeys>>,
    /// The first publish of the state file after start (it reconciles the file with the disk).
    published: std::sync::atomic::AtomicBool,
}

/// What a Drive API reply carried.
pub struct ApiReply {
    pub status: u16,
    pub body: Vec<u8>,
    pub capped: bool,
}

pub struct ApiRequest<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: Vec<(&'a str, String)>,
    pub body: Option<Vec<u8>>,
    pub cap: usize,
}

impl Drive {
    pub fn new(
        config: &crate::config::Config,
        off: Arc<offsite_settings::Settings>,
    ) -> Option<Drive> {
        let data_dir = config.data_dir.clone()?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("noevia")
            .build()
            .ok();
        Some(Drive {
            settings: config.google.clone(),
            off,
            client,
            mem: Mutex::new(HashMap::new()),
            fence: Mutex::new(()),
            locks: Mutex::new(HashMap::new()),
            data_dir,
            previous_key_env: config
                .rust_chatgpt_status
                .as_ref()
                .and_then(|s| s.previous_key_env.0.clone())
                .map(Zeroizing::new),
            keys: std::sync::OnceLock::new(),
            published: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn mem(&self) -> std::sync::MutexGuard<'_, HashMap<String, SlotMem>> {
        self.mem.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fence(&self) -> std::sync::MutexGuard<'_, ()> {
        self.fence.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn with_slot<R>(&self, slot: &Slot, f: impl FnOnce(&mut SlotMem) -> R) -> R {
        let mut m = self.mem();
        f(m.entry(slot.key()).or_default())
    }

    fn lock_for(&self, slot: &Slot) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, l| Arc::strong_count(l) > 1);
        Arc::clone(
            m.entry(slot.key())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// The key snapshot (core loads its keys once at start).
    fn keys(&self) -> Option<Arc<SecretKeys>> {
        if let Some(k) = self.keys.get() {
            return Some(Arc::clone(k));
        }
        let loaded = server_auth::secrets::load_existing(
            &self.data_dir,
            self.previous_key_env.as_deref().map(String::as_str),
        )
        .ok()?;
        Some(Arc::clone(self.keys.get_or_init(|| Arc::new(loaded))))
    }

    /// The sealers that may open a slot's file: the current key and, for an account's own file, the
    /// retired one (#866). `None`: no key can be had at all.
    fn sealers(
        &self,
        slot: &Slot,
    ) -> Option<(google_token_seal::Sealer, Option<google_token_seal::Sealer>)> {
        match slot {
            Slot::Backup => {
                let key = self.off.load_key().ok()?;
                Some((google_token_seal::Sealer::new(key.as_slice()).ok()?, None))
            }
            Slot::User(_) => {
                let keys = self.keys()?;
                let (cur, prev) = keys.derive_with(|k| {
                    google_token_seal::derive_secret_key(k, google_token_seal::USER_LABEL)
                });
                Some((
                    google_token_seal::Sealer::new(cur.as_slice()).ok()?,
                    prev.and_then(|p| google_token_seal::Sealer::new(p.as_slice()).ok()),
                ))
            }
        }
    }

    fn read_saved(&self, slot: &Slot) -> SavedFile {
        let Ok(text) = std::fs::read_to_string(slot.file(&self.data_dir)) else {
            return SavedFile {
                saved: Saved::Missing,
            };
        };
        let Some((cur, prev)) = self.sealers(slot) else {
            return SavedFile {
                saved: Saved::Broken,
            };
        };
        for sealer in std::iter::once(&cur).chain(prev.as_ref()) {
            if let Ok(plain) = sealer.open(&text) {
                if let Ok(v) = js_json::parse(&String::from_utf8_lossy(&plain)) {
                    let record = if v.is_object() {
                        v
                    } else {
                        JValue::Obj(Vec::new())
                    };
                    return SavedFile {
                        saved: Saved::Record(record),
                    };
                }
            }
        }
        SavedFile {
            saved: Saved::Broken,
        }
    }

    fn save(&self, slot: &Slot, record: &JValue) -> Result<(), Fail> {
        let (sealer, _) = self
            .sealers(slot)
            .ok_or_else(|| Fail::new("The Google connection could not be saved.", 502))?;
        let text = Zeroizing::new(
            js_json::stringify(record)
                .ok_or_else(|| Fail::new("The Google connection could not be saved.", 502))?,
        );
        let sealed = sealer
            .seal(text.as_bytes(), &random::<12>())
            .map_err(|_| Fail::new("The Google connection could not be saved.", 502))?;
        let file = slot.file(&self.data_dir);
        if let (Slot::User(_), Some(dir)) = (slot, file.parent()) {
            use std::os::unix::fs::DirBuilderExt as _;
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir);
        }
        write_atomic(&file, &sealed)
            .map_err(|_| Fail::new("The Google connection could not be saved.", 502))
    }

    /// `savePatch`: adds fields to a readable, connected record; never re-saves a broken or missing one.
    pub fn save_patch(&self, slot: &Slot, patch: Vec<(&str, JValue)>) -> bool {
        let _fence = self.fence();
        let SavedFile {
            saved: Saved::Record(JValue::Obj(mut pairs)),
        } = self.read_saved(slot)
        else {
            return false;
        };
        if !JValue::Obj(pairs.clone()).get("refreshToken").truthy() {
            return false;
        }
        for (k, v) in patch {
            match pairs.iter_mut().find(|(n, _)| n == k) {
                Some(s) => s.1 = v,
                None => pairs.push((k.to_owned(), v)),
            }
        }
        self.save(slot, &JValue::Obj(pairs)).is_ok()
    }

    /// The saved record's field, for the mirror's folder memory.
    pub fn saved_field(&self, slot: &Slot, field: &str) -> Option<String> {
        match self.read_saved(slot).saved {
            Saved::Record(r) => r.get(field).as_str().map(str::to_owned),
            _ => None,
        }
    }

    /// `state()`.
    pub fn state(&self, slot: &Slot) -> DriveState {
        let now = now_ms();
        if !self.settings.configured() {
            return DriveState {
                configured: false,
                state: "not-configured",
                message: Some(NOT_REGISTERED.into()),
                ..DriveState::default()
            };
        }
        let (pending, poll_error) = self.with_slot(slot, |m| {
            (
                m.pending.as_ref().filter(|p| p.expires_at > now).map(|p| {
                    (
                        p.user_code.clone(),
                        p.verification_url.clone(),
                        p.expires_at,
                        p.owner.clone(),
                    )
                }),
                m.poll_error.clone(),
            )
        });
        if let Some((code, url, expires, owner)) = pending {
            return DriveState {
                configured: true,
                state: "pending",
                user_code: Some(code),
                verification_url: Some(url),
                expires_at: Some(expires),
                owner,
                ..DriveState::default()
            };
        }
        match self.read_saved(slot).saved {
            Saved::Broken => DriveState {
                configured: true,
                state: "error",
                message: Some("The saved Google connection could not be read (was the backup key replaced?). Connect again.".into()),
                ..DriveState::default()
            },
            Saved::Record(r) if r.get("refreshToken").truthy() => DriveState {
                configured: true,
                state: "connected",
                email: r.get("email").as_str().filter(|e| !e.is_empty()).map(str::to_owned),
                connected_at: r.get("connectedAt").as_num().filter(|n| *n != 0.0),
                owner: r.get("owner").as_str().filter(|o| !o.is_empty()).map(str::to_owned),
                ..DriveState::default()
            },
            _ => DriveState {
                configured: true,
                state: "disconnected",
                message: poll_error,
                ..DriveState::default()
            },
        }
    }

    // ── whose Drive ──

    pub fn backup_facts(&self) -> BackupFacts {
        let usable = self.off.drive_usable();
        let s = self.state(&Slot::Backup);
        BackupFacts {
            usable,
            state: s.state.to_owned(),
            owner: s.owner,
        }
    }

    /// `forUser(user)`: the slot this account's tools and Connectors page use, and whether it is the
    /// backup's.
    pub fn slot_for(&self, user: &str, admin: bool) -> Result<Slot, Fail> {
        if user.is_empty() {
            return Err(Fail::new("Sign in first.", 401));
        }
        let personal_file = safe_id(user)
            && self
                .data_dir
                .join(USERS_DIR)
                .join(format!("{user}.sealed"))
                .exists();
        if owns_backup(admin, user, &self.backup_facts(), personal_file) {
            return Ok(Slot::Backup);
        }
        if !safe_id(user) {
            return Err(Fail::new("Google Drive could not be reached.", 502));
        }
        // core's forUser made the directory for a personal connection (mode 0700) the first time
        // an account's Drive was asked for; the replay's state comparison sees it.
        {
            use std::os::unix::fs::DirBuilderExt as _;
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(self.data_dir.join(USERS_DIR));
        }
        Ok(Slot::User(user.to_owned()))
    }

    // ── the state file Node reads ──

    /// Rewrites `gdrive-connections.json`: the accounts that have a connection file and in what
    /// state, and the backup slot's. No secret, no email. Called after every committed change.
    pub fn publish(&self) {
        // Nothing connected, nothing pending and no earlier file to correct: nothing to say (a
        // disconnect of an account that never connected leaves the data directory as core did).
        let pending = self
            .mem()
            .values()
            .any(|m| m.pending.as_ref().is_some_and(|p| p.expires_at > now_ms()));
        if !pending && !self.has_state() {
            return;
        }
        let backup = self.state(&Slot::Backup);
        let mut personal: Vec<(String, JValue)> = Vec::new();
        if let Ok(dir) = std::fs::read_dir(self.data_dir.join(USERS_DIR)) {
            for entry in dir.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(id) = name.strip_suffix(".sealed") else {
                    continue;
                };
                if !safe_id(id) {
                    continue;
                }
                let state = match self.read_saved(&Slot::User(id.to_owned())).saved {
                    Saved::Broken => "error",
                    Saved::Record(r) if r.get("refreshToken").truthy() => "connected",
                    _ => "disconnected",
                };
                personal.push((id.to_owned(), JValue::Str(state.into())));
            }
        }
        personal.sort_by(|a, b| a.0.cmp(&b.0));
        let doc = JValue::obj([
            ("version", JValue::Num(1.0)),
            ("at", JValue::Num(now_ms() as f64)),
            ("nonce", JValue::Str(hex_of(&random::<8>()))),
            (
                "backup",
                JValue::obj([
                    ("usable", JValue::Bool(self.off.drive_usable())),
                    ("state", JValue::Str(backup.state.into())),
                    ("owner", backup.owner.map_or(JValue::Null, JValue::Str)),
                    (
                        "expiresAt",
                        backup
                            .expires_at
                            .map_or(JValue::Null, |e| JValue::Num(e as f64)),
                    ),
                ]),
            ),
            ("personal", JValue::Obj(personal)),
        ]);
        let _ = write_atomic(
            &self.data_dir.join(STATE_FILE),
            &js_json::stringify(&doc).unwrap_or_default(),
        );
        self.published
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Whether there is anything to say: a backup slot file, a personal connection file, or a state
    /// file from an earlier start that a restart may have made untrue.
    fn has_state(&self) -> bool {
        let personal = std::fs::read_dir(self.data_dir.join(USERS_DIR))
            .map(|d| {
                d.flatten()
                    .any(|e| e.file_name().to_string_lossy().ends_with(".sealed"))
            })
            .unwrap_or(false);
        personal
            || self.data_dir.join(BACKUP_FILE).exists()
            || self.data_dir.join(STATE_FILE).exists()
    }

    /// The first publish of a start: brings the state file up to date with the disk, but writes
    /// none while nothing is connected (Node reads a missing file as "nobody is").
    pub fn publish_startup(&self) {
        if self.has_state() {
            self.publish();
        }
        self.published
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// [`Drive::publish_startup`] once per start, at the first internal use.
    pub fn publish_once(&self) {
        if !self.published.load(std::sync::atomic::Ordering::Acquire) {
            self.publish_startup();
        }
    }

    // ── Google ──

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        cap: usize,
    ) -> Result<(u16, Vec<u8>, bool), ()> {
        let exchange = async {
            let mut response = request.send().await.map_err(|_| ())?;
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Err(());
            }
            let mut body = Vec::new();
            let mut capped = false;
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        let room = cap.saturating_sub(body.len());
                        if chunk.len() > room {
                            body.extend_from_slice(chunk.get(..room).unwrap_or_default());
                            capped = true;
                            break;
                        }
                        body.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(_) => return Err(()),
                }
            }
            Ok((status, body, capped))
        };
        tokio::time::timeout(TIMEOUT, exchange)
            .await
            .unwrap_or(Err(()))
    }

    /// `form(url, fields)`: a form POST whose reply is read as JSON (`{}` when it is not).
    async fn form(&self, url: &str, fields: &[(&str, &str)]) -> Result<(u16, JValue), ()> {
        let client = self.client.as_ref().ok_or(())?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter())
            .finish();
        let request = client
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body);
        let (status, bytes, capped) = self.send(request, FORM_CAP).await?;
        let json = if capped {
            JValue::Obj(Vec::new())
        } else {
            js_json::parse(&String::from_utf8_lossy(&bytes)).unwrap_or(JValue::Obj(Vec::new()))
        };
        Ok((status, json))
    }

    // ── sign-in ──

    /// Starts Google's device sign-in for `slot` and polls for approval in a detached task. The
    /// slot's `owner` is who started it. `on_connected` runs once, after the token is saved.
    pub async fn connect(
        self: &Arc<Self>,
        slot: Slot,
        owner: Option<String>,
        on_connected: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<DriveState, Fail> {
        let me = Arc::clone(self);
        match tokio::spawn(async move { me.connect_task(slot, owner, on_connected).await }).await {
            Ok(r) => r,
            Err(_) => Err(Fail::new("Google Drive could not be reached.", 502)),
        }
    }

    async fn connect_task(
        self: &Arc<Self>,
        slot: Slot,
        owner: Option<String>,
        on_connected: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<DriveState, Fail> {
        if !self.settings.configured() {
            return Err(Fail::new(NOT_REGISTERED, 409));
        }
        let cur = self.state(&slot);
        if cur.state == "pending" {
            return Ok(cur);
        }
        let device_url = format!("{}/device/code", self.settings.oauth_base);
        let started = self
            .form(
                &device_url,
                &[
                    ("client_id", self.settings.client_id.as_str()),
                    ("scope", SCOPE),
                ],
            )
            .await;
        let body = match started {
            Ok((200, body)) if body.get("device_code").truthy() => body,
            _ => {
                return Err(Fail::new(
                    "Google did not start the sign-in. Try again in a minute.",
                    502,
                ))
            }
        };
        let now = now_ms();
        let device_code = body.get("device_code").as_str().unwrap_or("").to_owned();
        let interval_s = body
            .get("interval")
            .as_num()
            .filter(|n| *n != 0.0)
            .unwrap_or(5.0)
            .max(1.0);
        let expires_in = body
            .get("expires_in")
            .as_num()
            .filter(|n| *n != 0.0)
            .unwrap_or(1800.0);
        let verification = body
            .get("verification_url")
            .as_str()
            .filter(|u| !u.is_empty())
            .or_else(|| body.get("verification_uri").as_str())
            .unwrap_or("")
            .to_owned();
        let mine = {
            let mut m = self.mem();
            let entry = m.entry(slot.key()).or_default();
            // Someone else's connect finished starting while this one waited on Google: keep theirs (#868).
            if entry.pending.as_ref().is_some_and(|p| p.expires_at > now) {
                drop(m);
                return Ok(self.state(&slot));
            }
            entry.generation += 1;
            entry.poll_error = None;
            entry.pending = Some(Pending {
                user_code: body.get("user_code").as_str().unwrap_or("").to_owned(),
                verification_url: verification,
                expires_at: now + (expires_in * 1000.0) as i64,
                owner: owner.clone(),
            });
            entry.generation
        };
        self.publish();
        let me = Arc::clone(self);
        let poll_slot = slot.clone();
        tokio::spawn(async move {
            me.poll(
                poll_slot,
                mine,
                device_code,
                (interval_s * 1000.0) as u64,
                owner,
                on_connected,
            )
            .await;
        });
        Ok(self.state(&slot))
    }

    async fn poll(
        self: Arc<Self>,
        slot: Slot,
        mine: u64,
        device_code: String,
        mut interval_ms: u64,
        owner: Option<String>,
        mut on_connected: Option<Box<dyn FnOnce() + Send>>,
    ) {
        let token_url = format!("{}/token", self.settings.oauth_base);
        loop {
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            let alive = self.with_slot(&slot, |m| {
                if m.generation != mine {
                    return None;
                }
                Some(m.pending.as_ref().map(|p| p.expires_at))
            });
            let Some(Some(expires_at)) = alive else {
                return;
            };
            if now_ms() > expires_at {
                self.with_slot(&slot, |m| {
                    if m.generation == mine {
                        m.pending = None;
                        m.poll_error =
                            Some("The code expired before it was used. Connect again.".into());
                    }
                });
                self.publish();
                return;
            }
            let reply = self
                .form(
                    &token_url,
                    &[
                        ("client_id", self.settings.client_id.as_str()),
                        ("client_secret", self.settings.client_secret.as_str()),
                        ("device_code", &device_code),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ],
                )
                .await;
            // Offline for a moment: keep trying until the code expires.
            let Ok((status, body)) = reply else { continue };
            if self.with_slot(&slot, |m| m.generation != mine) {
                return;
            }
            if status == 200 && body.get("refresh_token").truthy() {
                let committed = self.commit_sign_in(&slot, mine, &body, owner.as_deref());
                self.publish();
                if committed {
                    if let Some(hook) = on_connected.take() {
                        hook();
                    }
                }
                return;
            }
            match body.get("error").as_str() {
                Some("authorization_pending") => {}
                Some("slow_down") => interval_ms += 5000,
                other => {
                    let message = if other == Some("access_denied") {
                        "Access was declined on Google’s page. Connect again to retry."
                    } else {
                        "Google ended the sign-in. Connect again."
                    };
                    self.with_slot(&slot, |m| {
                        if m.generation == mine {
                            m.pending = None;
                            m.poll_error = Some(message.into());
                        }
                    });
                    self.publish();
                    return;
                }
            }
        }
    }

    /// The sign-in's commit point: still wanted (nothing disconnected or restarted it since) and
    /// saved, in one step under the fence.
    fn commit_sign_in(&self, slot: &Slot, mine: u64, body: &JValue, owner: Option<&str>) -> bool {
        let _fence = self.fence();
        if self.with_slot(slot, |m| m.generation != mine) {
            return false;
        }
        let mut record = vec![
            ("refreshToken", body.get("refresh_token").clone()),
            (
                "email",
                body.get("id_token")
                    .as_str()
                    .and_then(email_of)
                    .map_or(JValue::Null, JValue::Str),
            ),
            ("connectedAt", JValue::Num(now_ms() as f64)),
        ];
        if let Some(o) = owner.filter(|o| !o.is_empty()) {
            record.push(("owner", JValue::Str(o.to_owned())));
        }
        let record = JValue::obj(record);
        if self.save(slot, &record).is_err() {
            // Core threw out of the poll here and left the sign-in pending; say so instead.
            self.with_slot(slot, |m| {
                m.pending = None;
                m.poll_error = Some("The connection could not be saved. Connect again.".into());
            });
            return false;
        }
        let access = body.get("access_token").as_str().map(|t| Access {
            token: Zeroizing::new(t.to_owned()),
            until: now_ms()
                + (body
                    .get("expires_in")
                    .as_num()
                    .filter(|n| *n != 0.0)
                    .unwrap_or(3600.0) as i64
                    - 60)
                    * 1000,
            generation: generation_of(&record),
        });
        self.with_slot(slot, |m| {
            m.pending = None;
            m.access = access;
        });
        true
    }

    // ── disconnect ──

    /// Revokes the token at Google (best effort), then deletes the file. A sign-in that landed
    /// while the revoke was in flight is left alone.
    pub async fn disconnect(self: &Arc<Self>, slot: Slot) -> Result<DriveState, Fail> {
        let me = Arc::clone(self);
        match tokio::spawn(async move { me.disconnect_task(slot).await }).await {
            Ok(r) => Ok(r),
            Err(_) => Err(Fail::new("Google Drive could not be reached.", 502)),
        }
    }

    async fn disconnect_task(&self, slot: Slot) -> DriveState {
        let (saved, seen) = {
            let _fence = self.fence();
            self.with_slot(&slot, |m| {
                m.generation += 1;
                m.pending = None;
                m.poll_error = None;
                m.access = None;
            });
            match self.read_saved(&slot).saved {
                Saved::Record(r) => (
                    r.get("refreshToken").as_str().map(str::to_owned),
                    Some(generation_of(&r)),
                ),
                _ => (None, None),
            }
        };
        if let Some(token) = saved {
            let url = format!("{}/revoke", self.settings.oauth_base);
            let _ = self.form(&url, &[("token", &token)]).await;
        }
        {
            let _fence = self.fence();
            let current = match self.read_saved(&slot).saved {
                Saved::Record(r) => Some(generation_of(&r)),
                _ => None,
            };
            // Missing, broken or the same sign-in: delete; a newer sign-in stays.
            if seen.is_none() || current.is_none() || current == seen {
                let _ = std::fs::remove_file(slot.file(&self.data_dir));
            }
        }
        self.publish();
        self.state(&slot)
    }

    /// `removeUser`: an account was deleted; its connection is revoked and forgotten.
    pub async fn forget(self: &Arc<Self>, user: &str) {
        if !safe_id(user) {
            return;
        }
        let slot = Slot::User(user.to_owned());
        if slot.file(&self.data_dir).exists() {
            let _ = self.disconnect(slot.clone()).await;
        }
        self.mem().remove(&slot.key());
        self.publish();
    }

    // ── tokens ──

    fn cached(&self, slot: &Slot, current: &str, rejected: Option<&str>) -> Option<(String, i64)> {
        self.with_slot(slot, |m| {
            let a = m.access.as_ref()?;
            if a.until <= now_ms() || a.generation != current {
                return None;
            }
            if rejected.is_some_and(|r| r == a.token.as_str()) {
                m.access = None;
                return None;
            }
            Some((a.token.to_string(), a.until))
        })
    }

    /// A current access token for `slot`, refreshed under the slot's lock when the cached one is
    /// gone, expired, or the one a 401 answered (`rejected`). Returns `(token, expiresAt, generation)`.
    pub async fn token(
        self: &Arc<Self>,
        slot: &Slot,
        rejected: Option<&str>,
    ) -> Result<(Zeroizing<String>, i64, String), Fail> {
        let me = Arc::clone(self);
        let (slot, rejected) = (slot.clone(), rejected.map(str::to_owned));
        match tokio::spawn(async move { me.token_task(&slot, rejected.as_deref()).await }).await {
            Ok(r) => r,
            Err(_) => Err(Fail::new(UNREACHABLE, 502).coded("transient")),
        }
    }

    fn sign_in_of(&self, slot: &Slot) -> Result<(String, String), Fail> {
        match self.read_saved(slot).saved {
            Saved::Record(r) if r.get("refreshToken").truthy() => Ok((
                r.get("refreshToken").as_str().unwrap_or("").to_owned(),
                generation_of(&r),
            )),
            Saved::Broken => Err(Fail::new(NOT_CONNECTED, 409).coded("reconnect")),
            _ => Err(Fail::new(NOT_CONNECTED, 409).coded("disconnected")),
        }
    }

    async fn token_task(
        self: &Arc<Self>,
        slot: &Slot,
        rejected: Option<&str>,
    ) -> Result<(Zeroizing<String>, i64, String), Fail> {
        let (_, generation) = self.sign_in_of(slot)?;
        if let Some((t, until)) = self.cached(slot, &generation, rejected) {
            return Ok((Zeroizing::new(t), until, generation));
        }
        let lock = self.lock_for(slot);
        let _flight = lock.lock().await;
        // A waiter uses a refresh that landed while it waited.
        let (refresh_token, generation) = self.sign_in_of(slot)?;
        if let Some((t, until)) = self.cached(slot, &generation, rejected) {
            return Ok((Zeroizing::new(t), until, generation));
        }
        let epoch = self.with_slot(slot, |m| m.generation);
        let url = format!("{}/token", self.settings.oauth_base);
        let reply = self
            .form(
                &url,
                &[
                    ("client_id", self.settings.client_id.as_str()),
                    ("client_secret", self.settings.client_secret.as_str()),
                    ("refresh_token", &refresh_token),
                    ("grant_type", "refresh_token"),
                ],
            )
            .await;
        let (status, body) = reply.map_err(|_| Fail::new(UNREACHABLE, 502).coded("transient"))?;
        let Some(access) = body.get("access_token").as_str().filter(|_| status == 200) else {
            return Err(if body.get("error").as_str() == Some("invalid_grant") {
                Fail::new(REVOKED, 409).coded("reconnect")
            } else {
                Fail::new(UNREACHABLE, 502).coded("transient")
            });
        };
        let until = now_ms()
            + (body
                .get("expires_in")
                .as_num()
                .filter(|n| *n != 0.0)
                .unwrap_or(3600.0) as i64
                - 60)
                * 1000;
        // Commit: only while this sign-in is still the stored one (a disconnect or a newer sign-in
        // during the refresh wins; the token is not cached for them).
        let kept = {
            let _fence = self.fence();
            let still = self
                .sign_in_of(slot)
                .map(|(_, g)| g == generation)
                .unwrap_or(false)
                && self.with_slot(slot, |m| m.generation) == epoch;
            if still {
                self.with_slot(slot, |m| {
                    m.access = Some(Access {
                        token: Zeroizing::new(access.to_owned()),
                        until,
                        generation: generation.clone(),
                    });
                });
            }
            still
        };
        if !kept {
            return Err(Fail::new(NOT_CONNECTED, 409).coded("disconnected"));
        }
        Ok((Zeroizing::new(access.to_owned()), until, generation))
    }

    /// The second 401 on a fresh token: drop the cached access token of THAT sign-in (by
    /// generation); a newer sign-in is left alone. Returns whether one was dropped.
    pub fn reject(&self, slot: &Slot, generation: &str) -> bool {
        self.with_slot(slot, |m| match &m.access {
            Some(a) if a.generation == generation => {
                m.access = None;
                true
            }
            _ => false,
        })
    }

    // ── Drive API for the mirror ──

    /// A Drive API exchange for `slot` with a current access token.
    pub async fn api(self: &Arc<Self>, slot: &Slot, req: ApiRequest<'_>) -> Result<ApiReply, Fail> {
        let (token, _, _) = self.token(slot, None).await?;
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| Fail::new("Google Drive could not be reached.", 502))?;
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|_| Fail::new("Google Drive could not be reached.", 502))?;
        let mut builder = client.request(method, req.url).bearer_auth(token.as_str());
        for (k, v) in req.headers {
            builder = builder.header(k, v);
        }
        if let Some(body) = req.body {
            builder = builder.body(body);
        }
        let (status, body, capped) = self
            .send(builder, req.cap)
            .await
            .map_err(|_| Fail::new("Google Drive could not be reached.", 502))?;
        Ok(ApiReply {
            status,
            body,
            capped,
        })
    }

    /// `call(url, init)`: a JSON answer, or core's failure messages.
    pub async fn call(
        self: &Arc<Self>,
        slot: &Slot,
        method: &str,
        url: &str,
        content_type: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> Result<JValue, Fail> {
        let headers = content_type
            .map(|c| vec![("content-type", c.to_owned())])
            .unwrap_or_default();
        let r = self
            .api(
                slot,
                ApiRequest {
                    method,
                    url,
                    headers,
                    body,
                    cap: JSON_CAP,
                },
            )
            .await?;
        if !(200..300).contains(&r.status) {
            return Err(if r.status == 404 {
                Fail::new(
                    "Google Drive has no such file, or noevia cannot see it.",
                    404,
                )
            } else {
                answered(r.status)
            });
        }
        if r.status == 204 {
            return Ok(JValue::Null);
        }
        if r.capped {
            return Err(Fail::new("Google Drive could not be reached.", 502));
        }
        js_json::parse(&String::from_utf8_lossy(&r.body))
            .map_err(|_| Fail::new("Google Drive could not be reached.", 502))
    }
}

/// `Google Drive answered <status>.` with its translatable id.
pub fn answered(status: u16) -> Fail {
    Fail::new(format!("Google Drive answered {status}."), 502).with_id(
        "driveAnswered",
        vec![("status", JValue::Num(f64::from(status)))],
    )
}

/// The drive of the app, or `None` before the data directory is known.
pub fn of(app: &App) -> Option<Arc<Drive>> {
    app.drive.as_ref().map(Arc::clone)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;

    #[test]
    fn the_ownership_rule_matches_the_shared_cases() {
        let cases =
            js_json::parse(include_str!("../tests/fixtures/gdrive-owns-backup.v1.json")).unwrap();
        let JValue::Arr(cases) = cases.get("cases") else {
            panic!("cases")
        };
        assert!(cases.len() >= 12);
        for c in cases {
            let b = c.get("backup");
            let facts = BackupFacts {
                usable: b.get("usable").as_bool().unwrap(),
                state: b.get("state").as_str().unwrap().to_owned(),
                owner: b.get("owner").as_str().map(str::to_owned),
            };
            let got = owns_backup(
                c.get("admin").as_bool().unwrap(),
                c.get("user").as_str().unwrap(),
                &facts,
                c.get("personalFile").as_bool().unwrap(),
            );
            assert_eq!(
                got,
                c.get("owns").as_bool().unwrap(),
                "{}",
                c.get("name").as_str().unwrap()
            );
        }
    }

    #[test]
    fn email_and_generation_and_ids() {
        use base64::Engine as _;
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"email":"ada@example.test"}"#);
        assert_eq!(
            email_of(&format!("h.{claims}.s")).as_deref(),
            Some("ada@example.test")
        );
        assert_eq!(email_of("nope"), None);
        let r = js_json::parse(r#"{"refreshToken":"r1","connectedAt":5}"#).unwrap();
        let g = generation_of(&r);
        assert_eq!(g.len(), 64);
        assert!(!g.contains("r1"));
        let resealed =
            js_json::parse(r#"{"connectedAt":5,"refreshToken":"r1","folderId":"f"}"#).unwrap();
        assert_eq!(
            generation_of(&resealed),
            g,
            "a patch or reseal keeps the sign-in"
        );
        assert_ne!(
            generation_of(&js_json::parse(r#"{"refreshToken":"r2","connectedAt":5}"#).unwrap()),
            g
        );
        assert!(safe_id("aaaa-bbb_1") && !safe_id("a/b") && !safe_id("") && !safe_id("a b"));
        assert_eq!(
            encode_uri_component("name contains 'x' & y"),
            "name%20contains%20'x'%20%26%20y"
        );
    }

    #[test]
    fn endpoints_are_loopback_only() {
        let map: HashMap<&str, &str> = HashMap::from([
            ("GOOGLE_OAUTH_CLIENT_ID", " id "),
            ("GOOGLE_OAUTH_CLIENT_SECRET", "s"),
            ("GOOGLE_OAUTH_BASE_URL", "http://127.0.0.1:9/"),
            ("GOOGLE_DRIVE_API_BASE_URL", "https://evil.example/drive/v3"),
        ]);
        let s = Settings::from_lookup(&|k: &str| map.get(k).map(|v| (*v).to_owned()));
        assert!(s.configured());
        assert_eq!(s.client_id, "id");
        assert_eq!(s.oauth_base, "http://127.0.0.1:9");
        assert_eq!(s.api_base, API, "a non-loopback endpoint is ignored");
        assert_eq!(s.upload_base, UPLOAD);
    }
}
