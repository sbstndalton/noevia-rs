//! The native owner of auto-tune's persisted state: the job, the per-model history and the per-model
//! settings in one file, written only here (temporary file, mode 0600, rename: the bytes the Node
//! actor used to write, so a file written by either side loads on the other).
//!
//! The Node actor (K5: start/recover and the engine work) stays a client. It holds a working replica
//! of the job and history, and every change it makes reaches the file through [`Owner::commit`],
//! fenced three ways: by the writer token the latest [`Owner::attach`] issued (a restarted peer
//! never inherits an older one's right to write), by the job revision (a change this owner made to
//! the job that the actor has not adopted yet refuses the actor's stale copy) and by the front's own
//! incarnation (a restarted front starts unbound, so a surviving actor is refused until it attaches
//! again). A refused commit means the actor must stop; it never re-acquires to continue old work.
//!
//! The public operations the five Node bridges used to answer (status facts, settings facts and
//! conditional commit, resume facts/prepare/commit, cancel facts/abort, clear facts/clear) are
//! methods here, with their opaque tokens (a fingerprint of this owner's state, the facts the
//! caller supplies from outside it, and a per-boot salt), so a token is only valid for the exact
//! state it was issued for. Nothing here awaits: the glue (bins/noevia-server autotune_state.rs)
//! asks the actor for the few facts only Node has (models.ini revision, system roles, router
//! listing, identities) and for the effects only Node can have (aborting a running tune, starting a
//! resumed one) before and after a call into this module, never inside the lock.

use js_json::{wtf16, JValue};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// Release pairing (noevia deploy/tools/assemble-release.sh `autotune_state_pairing`): a front that serves
/// the auto-tune state store is paired only with a core whose actor is its client
/// (`noevia-autotune-state-client/1`). Keep the quoted literal: the release check reads it from this file.
pub const AUTOTUNE_STATE_STORE_MARKER: &str = "noevia-autotune-state-store/1";

/// `(status, body)`: what a Node bridge answered.
pub type Answer = (u16, JValue);

const NOTE_REREAD: &str = "The model server re-read the restored models.ini.";
const MAX_STATE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_LOG: usize = 400;
const FINISHED: &[&str] = &["failed", "cancelled", "interrupted", "passed"];

fn unavailable() -> Answer {
    (
        503,
        JValue::obj([("error", "Auto-tune state is unavailable.".into())]),
    )
}
fn bad(message: &str) -> Answer {
    (400, JValue::obj([("error", message.into())]))
}
fn not_applied() -> Answer {
    (409, JValue::obj([("applied", false.into())]))
}
fn ok(body: JValue) -> Answer {
    (200, body)
}

// ---- small JValue helpers -------------------------------------------------------------------

fn obj_items(v: &JValue) -> Option<&Vec<(String, JValue)>> {
    match v {
        JValue::Obj(items) => Some(items),
        _ => None,
    }
}
fn has_own(v: &JValue, key: &str) -> bool {
    obj_items(v).is_some_and(|i| i.iter().any(|(k, _)| k == key))
}
fn set_key(v: &mut JValue, key: &str, value: JValue) {
    if let JValue::Obj(items) = v {
        if let Some(slot) = items.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value;
        } else {
            items.push((key.to_owned(), value));
            wtf16::order_keys(items);
        }
    }
}
fn remove_key(v: &mut JValue, key: &str) {
    if let JValue::Obj(items) = v {
        items.retain(|(k, _)| k != key);
    }
}
fn string_of(v: &JValue) -> JValue {
    match v {
        JValue::Str(s) => JValue::Str(s.clone()),
        _ => JValue::Null,
    }
}
fn hex_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}
fn canon(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_default()
}
/// A deep copy that leaves out every property `drop` names, at any depth: the old
/// `JSON.parse(JSON.stringify(value, (key, v) => drop(key) ? undefined : v))`.
fn strip(v: &JValue, drop: &dyn Fn(&str) -> bool) -> JValue {
    match v {
        JValue::Obj(items) => JValue::Obj(
            items
                .iter()
                .filter(|(k, x)| !drop(k) && !matches!(x, JValue::Undefined))
                .map(|(k, x)| (k.clone(), strip(x, drop)))
                .collect(),
        ),
        JValue::Arr(items) => JValue::Arr(items.iter().map(|x| strip(x, drop)).collect()),
        other => other.clone(),
    }
}
fn private_key(key: &str) -> bool {
    key.starts_with('_')
        || matches!(
            key,
            "originalText" | "lastRevision" | "originalRevision" | "beforeText"
        )
}
fn http_exact(key: &str) -> bool {
    matches!(
        key,
        "headers"
            | "authorization"
            | "apiKey"
            | "api_key"
            | "accessToken"
            | "refreshToken"
            | "httpConfig"
            | "requestConfig"
            | "baseUrl"
            | "url"
    )
}
fn http_normalized(key: &str) -> bool {
    let n: String = key
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        n.as_str(),
        "headers"
            | "authorization"
            | "apikey"
            | "accesstoken"
            | "refreshtoken"
            | "httpconfig"
            | "requestconfig"
            | "baseurl"
            | "url"
            | "credentials"
            | "password"
            | "secret"
            | "token"
            | "clientsecret"
    )
}
fn pick(v: &JValue, keys: &[&str]) -> JValue {
    JValue::Obj(
        keys.iter()
            .filter(|k| has_own(v, k))
            .map(|k| ((*k).to_owned(), v.get(k).clone()))
            .collect(),
    )
}
fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn is_base(s: &str) -> bool {
    let Some(rest) = s
        .strip_prefix("native-autotune-")
        .and_then(|r| r.strip_suffix(".json"))
    else {
        return false;
    };
    rest.len() == 16
        && rest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What only the Node actor can know when a settings or resume fact is built: the models.ini
/// revision, each named model's role/profile, and the environment switch Node reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outside {
    revision: Option<String>,
    allow_below: bool,
    models: Vec<(String, bool, bool)>,
}
impl Outside {
    pub fn parse(v: &JValue) -> Option<Self> {
        let revision = match v.get("revision") {
            JValue::Str(s) => Some(s.clone()),
            JValue::Null => None,
            _ => return None,
        };
        let allow_below = v.get("allowBelowQ5Kv").as_bool()?;
        let JValue::Obj(rows) = v.get("models") else {
            return None;
        };
        let mut models = Vec::new();
        for (name, row) in rows {
            models.push((
                name.clone(),
                row.get("configured").as_bool()?,
                row.get("system").as_bool()?,
            ));
        }
        Some(Self {
            revision,
            allow_below,
            models,
        })
    }
    pub fn allow_below(&self) -> bool {
        self.allow_below
    }
    fn model(&self, name: &str) -> Option<(bool, bool)> {
        self.models
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, c, s)| (*c, *s))
    }
    fn revision_value(&self) -> JValue {
        self.revision.clone().map_or(JValue::Null, JValue::Str)
    }
}

/// The ways the owner refuses; the glue maps each to a status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Unbound,
    State,
}

#[derive(Debug)]
enum Hold {
    Start(String),
    Prepare,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Listing,
    Identities,
}
#[derive(Debug)]
struct Prep {
    id: u64,
    stage: Stage,
    serial: u64,
    epoch: u64,
    baseline_rev: u64,
    owed: bool,
    before_doc: JValue,
    outside: Outside,
    noted: bool,
}

struct Bound {
    path: PathBuf,
    available: bool,
    doc: JValue,
    rev: u64,
    job_rev: u64,
    job_serial: u64,
    epoch: u64,
    cancelled: bool,
    hold: Option<Hold>,
    prep: Option<Prep>,
    writer: Option<String>,
    prep_seq: u64,
    actor: Option<String>,
}

/// The single owner. One per front process.
pub struct Owner {
    dir: PathBuf,
    salt: [u8; 32],
    inner: Mutex<Option<Bound>>,
    sessions: Mutex<Vec<String>>,
}

/// A prepared listing/identities call: what the actor must read, and the handle that settles it.
#[derive(Debug)]
pub struct Prepared {
    pub id: u64,
    pub stage: Stage,
    pub items: JValue,
    pub selectors: Vec<(usize, String, String)>,
}
/// The actor effect a successful abort still owes.
#[derive(Debug)]
pub struct AbortTicket {
    pub job_id: JValue,
    pub epoch: u64,
    pub reply: Answer,
    prior_cancelled: bool,
    serial: u64,
}
/// The actor effect a successful launching resume still owes.
#[derive(Debug)]
pub struct Launch {
    pub epoch: u64,
    pub job_rev: u64,
    pub job: JValue,
    pub history: JValue,
    serial: u64,
}
/// A resume commit's answer and the launch it owes the actor (a queue filter owes none).
#[derive(Debug)]
pub struct ResumeCommit {
    pub answer: Answer,
    pub launch: Option<Launch>,
}

impl Owner {
    pub fn new(dir: &Path) -> Self {
        let mut salt = [0u8; 32];
        OsRng.fill_bytes(&mut salt);
        Self {
            dir: dir.to_owned(),
            salt,
            inner: Mutex::new(None),
            sessions: Mutex::new(Vec::new()),
        }
    }
    fn lock(&self) -> MutexGuard<'_, Option<Bound>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn tok(&self, tag: &str, parts: &[String]) -> String {
        let mut h = Sha256::new();
        h.update(self.salt);
        h.update(tag.as_bytes());
        for p in parts {
            h.update([0x1f]);
            h.update(p.as_bytes());
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    // ---- the actor's side: attach, commit, start hold, sync --------------------------------

    /// Binds the owner to the state file `base` (a name, never a path) and issues a writer token.
    /// Every earlier writer, start hold and preparation is void from here on, and the run epoch
    /// moves so no earlier opaque token names the new incarnation's run.
    pub fn attach(&self, base: &str, available: bool) -> Result<JValue, Fault> {
        if !is_base(base) {
            return Err(Fault::State);
        }
        let mut guard = self.lock();
        self.attach_locked(&mut guard, base, available)
    }

    fn attach_locked(
        &self,
        guard: &mut Option<Bound>,
        base: &str,
        available: bool,
    ) -> Result<JValue, Fault> {
        if !is_base(base) {
            return Err(Fault::State);
        }
        let path = self.dir.join(base);
        let rebinding = guard.as_ref().is_some_and(|b| b.path != path);
        if rebinding {
            return Err(Fault::State);
        }
        let writer = hex_token(16);
        match guard.as_mut() {
            Some(b) => {
                if available && !b.available {
                    b.doc = load(&b.path)?;
                }
                b.available = available;
                b.writer = Some(writer.clone());
                b.hold = None;
                b.prep = None;
                b.cancelled = false;
                b.epoch += 1;
                b.rev += 1;
            }
            None => {
                let doc = if available {
                    load(&path)?
                } else {
                    JValue::Obj(Vec::new())
                };
                *guard = Some(Bound {
                    path,
                    available,
                    doc,
                    rev: 1,
                    job_rev: 1,
                    job_serial: 1,
                    epoch: 1,
                    cancelled: false,
                    hold: None,
                    prep: None,
                    writer: Some(writer.clone()),
                    prep_seq: 0,
                    actor: None,
                });
            }
        }
        let b = guard.as_ref().ok_or(Fault::State)?;
        Ok(snapshot(b, Some(&writer)))
    }

    /// Reserve once per actor process. A delayed attach cannot overtake a newer reservation;
    /// retries by a retired process can never allocate a newer incarnation.
    pub fn attach_actor(&self, body: &JValue) -> Answer {
        let Some(actor) = body
            .get("actor")
            .as_str()
            .filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        else {
            return bad("Invalid actor session.");
        };
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = match sessions.iter().position(|a| a == actor) {
            Some(i) => i,
            None if body.get("open").as_bool() == Some(true) => {
                sessions.push(actor.to_owned());
                sessions.len() - 1
            }
            None => return Self::fenced("session", None),
        };
        if index + 1 != sessions.len() {
            return Self::fenced("retired", None);
        }
        if body.get("open").as_bool() == Some(true) {
            return ok(JValue::obj([(
                "incarnation",
                JValue::Num((index + 1) as f64),
            )]));
        }
        if body.get("incarnation").as_num() != Some((index + 1) as f64) {
            return Self::fenced("session", None);
        }
        // Repeated delivery is idempotent, including a response lost after acquisition.
        let base = body.get("base").as_str().unwrap_or_default();
        let Some(available) = body.get("available").as_bool() else {
            return bad("Invalid attach.");
        };
        let mut guard = self.lock();
        if let Some(b) = guard.as_ref() {
            if b.actor.as_deref() == Some(actor) {
                return ok(snapshot(b, b.writer.as_deref()));
            }
            // A live front cannot prove another process has stopped its engine effects. Only
            // hand over an idle, settled writer. A restarted front is unbound and may recover disk.
            if b.hold.is_some()
                || b.doc.get("job").get("status").as_str() == Some("running")
                || b.doc.get("job").get("_reloadPending").truthy()
            {
                return Self::fenced("busy", None);
            }
        }
        match self.attach_locked(&mut guard, base, available) {
            Ok(snapshot) => {
                if let Some(b) = guard.as_mut() {
                    b.actor = Some(actor.to_owned());
                }
                ok(snapshot)
            }
            Err(_) => unavailable(),
        }
    }

    fn writer_ok(b: &Bound, body: &JValue) -> bool {
        match (&b.writer, body.get("writer")) {
            (Some(w), JValue::Str(given)) => w == given,
            _ => false,
        }
    }
    fn fenced(why: &str, b: Option<&Bound>) -> Answer {
        let mut fields = vec![("fenced".to_owned(), JValue::from(why))];
        if let Some(b) = b {
            fields.push(("state".to_owned(), snapshot(b, None)));
        }
        (409, JValue::Obj(fields))
    }

    /// The actor's save: replaces the job and the history (never the settings), if the writer
    /// token and the job revision are current.
    pub fn commit(&self, body: &JValue) -> Answer {
        let mut guard = self.lock();
        let Some(b) = guard.as_mut() else {
            return Self::fenced("unbound", None);
        };
        if !b.available || !Self::writer_ok(b, body) {
            return Self::fenced("writer", None);
        }
        if body.get("jobRev").as_num() != Some(b.job_rev as f64) {
            return Self::fenced("job", Some(b));
        }
        let register = body.get("register").as_bool() == Some(true);
        let history = body.get("history");
        if !matches!(history, JValue::Obj(_)) {
            return bad("Invalid auto-tune state.");
        }
        let job = body.get("job");
        if !matches!(job, JValue::Obj(_) | JValue::Undefined | JValue::Null) {
            return bad("Invalid auto-tune state.");
        }
        let mut next = b.doc.clone();
        set_key(&mut next, "history", history.clone());
        match job {
            JValue::Undefined => remove_key(&mut next, "job"),
            other => set_key(&mut next, "job", other.clone()),
        }
        if let Err(e) = persist(&b.path, &next) {
            return (e, JValue::obj([("error", "state write failed".into())]));
        }
        let before_rev = b.rev;
        b.doc = next;
        b.rev += 1;
        if register {
            b.job_serial += 1;
            b.epoch += 1;
            b.job_rev += 1;
            b.cancelled = false;
        }
        // A reload note the actor commits during a preparation is the one change that preparation
        // allows across its await; anything else leaves the revision behind the baseline.
        let after_doc = b.doc.clone();
        let rev = b.rev;
        if let Some(p) = b.prep.as_mut() {
            if p.stage == Stage::Identities
                && p.owed
                && !p.noted
                && before_rev == p.baseline_rev
                && reload_note_only(p.before_doc.get("job"), after_doc.get("job"))
                && {
                    let mut expected = p.before_doc.clone();
                    set_key(&mut expected, "job", after_doc.get("job").clone());
                    same_fields(&expected, &after_doc)
                }
            {
                p.noted = true;
                p.baseline_rev = rev;
            }
        }
        ok(JValue::obj([
            ("rev", JValue::Num(b.rev as f64)),
            ("jobRev", JValue::Num(b.job_rev as f64)),
            ("epoch", JValue::Num(b.epoch as f64)),
            ("settings", b.doc.get("settings").clone()),
        ]))
    }

    /// The actor's start hold: `begin` is refused while another start, a preparation or a running
    /// job exists; `end` releases only this writer's own hold.
    pub fn start_hold(&self, body: &JValue, begin: bool) -> Answer {
        let mut guard = self.lock();
        let Some(b) = guard.as_mut() else {
            return Self::fenced("unbound", None);
        };
        if !b.available || !Self::writer_ok(b, body) {
            return Self::fenced("writer", None);
        }
        let writer = body.get("writer").as_str().unwrap_or_default().to_owned();
        if begin {
            if b.hold.is_some() || b.doc.get("job").get("status").as_str() == Some("running") {
                return (409, JValue::obj([("busy", true.into())]));
            }
            b.hold = Some(Hold::Start(writer));
            b.rev += 1;
        } else if matches!(&b.hold, Some(Hold::Start(w)) if *w == writer) {
            b.hold = None;
            b.rev += 1;
        }
        ok(JValue::obj([("rev", JValue::Num(b.rev as f64))]))
    }

    /// The actor's re-read after a refused commit, or before it starts a tune.
    pub fn sync(&self, body: &JValue) -> Answer {
        let guard = self.lock();
        let Some(b) = guard.as_ref() else {
            return Self::fenced("unbound", None);
        };
        if !b.available || !Self::writer_ok(b, body) {
            return Self::fenced("writer", None);
        }
        if body.get("settingsOnly").as_bool() == Some(true) {
            return ok(JValue::obj([
                ("rev", JValue::Num(b.rev as f64)),
                ("jobRev", JValue::Num(b.job_rev as f64)),
                ("epoch", JValue::Num(b.epoch as f64)),
                (
                    "state",
                    JValue::obj([("settings", b.doc.get("settings").clone())]),
                ),
            ]));
        }
        ok(snapshot(b, None))
    }

    // ---- the front's side -------------------------------------------------------------------

    /// `None`: not bound yet (the actor has not attached since the front started).
    pub fn availability(&self) -> Option<bool> {
        self.lock().as_ref().map(|b| b.available)
    }

    /// Whether a job is running (for the glue's abort bookkeeping and tests).
    pub fn running(&self) -> bool {
        self.lock()
            .as_ref()
            .is_some_and(|b| b.doc.get("job").get("status").as_str() == Some("running"))
    }

    pub fn state_snapshot(&self) -> Option<JValue> {
        self.lock().as_ref().map(|b| snapshot(b, None))
    }

    /// The bridge's availability preamble. `Err` carries the answer to send.
    fn live<'a>(g: &'a mut MutexGuard<'_, Option<Bound>>) -> Result<&'a mut Bound, Answer> {
        match g.as_mut() {
            None => Err(unavailable()),
            Some(b) if !b.available => Err(ok(JValue::obj([("available", false.into())]))),
            Some(b) => Ok(b),
        }
    }

    pub fn status_facts(&self, keys: &[JValue], outside_below: bool) -> Answer {
        let mut g = self.lock();
        let b = match Self::live(&mut g) {
            Ok(b) => b,
            Err(a) => return a,
        };
        if keys.len() > 2
            || keys.iter().any(|k| match k {
                JValue::Str(s) => wtf16::len(s) > 65536,
                _ => true,
            })
        {
            return unavailable();
        }
        let hidden = |k: &str| private_key(k) || http_normalized(k);
        let copy = |v: &JValue| strip(v, &hidden);
        const FIELDS: &[&str] = &[
            "id",
            "model",
            "bulk",
            "mode",
            "promptBudgetSeconds",
            "status",
            "phase",
            "startedAt",
            "log",
            "models",
            "queueProgress",
            "finishedAt",
            "error",
            "failedPhase",
            "failedStep",
            "restored",
            "skipped",
            "waiting",
            "result",
            "queue",
            "steps",
        ];
        const HISTORY: &[&str] = &[
            "at",
            "kv",
            "context",
            "plan",
            "probes",
            "kvFallback",
            "spec",
            "specLabel",
            "generation",
            "acceptance",
            "ubatch",
            "promptPerSecond",
            "quality",
            "extensions",
            "sampling",
            "baseline",
            "serving",
            "loaded",
            "version",
            "signature",
        ];
        let job = b.doc.get("job");
        let job_present = !matches!(job, JValue::Undefined);
        let selected = match job {
            JValue::Undefined | JValue::Null => job.clone(),
            JValue::Obj(_) => copy(&pick(job, FIELDS)),
            _ => return unavailable(),
        };
        let history = b.doc.get("history");
        let settings = b.doc.get("settings");
        let mut entries = Vec::new();
        for key in keys {
            let JValue::Str(name) = key else {
                return unavailable();
            };
            let hist = if has_own(history, name) {
                match history.get(name) {
                    falsy if !falsy.truthy() => falsy.clone(),
                    JValue::Arr(rows) => {
                        let mut out = Vec::new();
                        for row in rows {
                            if !matches!(row, JValue::Obj(_)) {
                                return unavailable();
                            }
                            out.push(copy(&pick(row, HISTORY)));
                        }
                        JValue::Arr(out)
                    }
                    _ => return unavailable(),
                }
            } else {
                JValue::Null
            };
            let q5 = has_own(settings, name)
                && settings.get(name).get("allowQ5Kv").as_bool() == Some(true);
            entries.push(JValue::obj([
                ("key", key.clone()),
                ("history", hist),
                ("allowQ5Kv", q5.into()),
            ]));
        }
        ok(JValue::obj([
            ("available", true.into()),
            ("jobPresent", job_present.into()),
            ("job", selected),
            ("entries", JValue::Arr(entries)),
            ("allowBelowQ5Kv", outside_below.into()),
        ]))
    }

    // ---- settings -----------------------------------------------------------------------------

    fn settings_of(b: &Bound, model: &str) -> bool {
        let s = b.doc.get("settings");
        has_own(s, model) && s.get(model).get("allowQ5Kv").as_bool() == Some(true)
    }
    fn settings_snapshot(&self, b: &Bound, model: &str, o: &Outside) -> Option<JValue> {
        let (configured, system) = o.model(model)?;
        let job = b.doc.get("job");
        let guard_job = if job.truthy() {
            let models = match job.get("models") {
                JValue::Arr(items) => JValue::Arr(
                    items
                        .iter()
                        .map(|m| {
                            if m.truthy() {
                                JValue::obj([
                                    ("model", m.get("model").clone()),
                                    ("base", m.get("base").clone()),
                                    ("status", m.get("status").clone()),
                                ])
                            } else {
                                m.clone()
                            }
                        })
                        .collect(),
                ),
                other => other.clone(),
            };
            JValue::obj([
                ("id", job.get("id").clone()),
                ("startedAt", job.get("startedAt").clone()),
                ("status", job.get("status").clone()),
                ("models", models),
            ])
        } else {
            JValue::Null
        };
        let guard = JValue::obj([
            ("configured", configured.into()),
            ("system", system.into()),
            ("revision", o.revision_value()),
            ("starting", b.hold.is_some().into()),
            ("job", guard_job),
            ("allowQ5Kv", Self::settings_of(b, model).into()),
            ("allowBelowQ5Kv", o.allow_below.into()),
        ]);
        let token = self.tok(
            "settings",
            &[b.rev.to_string(), model.to_owned(), canon(&guard)],
        );
        let drop_private = |k: &str| k.starts_with('_') || private_key(k);
        let history = |key: &str| {
            if has_own(b.doc.get("history"), key) {
                match b.doc.get("history").get(key) {
                    v if v.truthy() => v.clone(),
                    _ => JValue::Arr(Vec::new()),
                }
            } else {
                JValue::Arr(Vec::new())
            }
        };
        Some(JValue::obj([
            ("token", token.into()),
            ("guard", guard),
            (
                "job",
                if job.truthy() {
                    strip(job, &drop_private)
                } else {
                    job.clone()
                },
            ),
            ("history", history(model)),
            ("longHistory", history(&format!("{model}-long"))),
        ]))
    }
    pub fn settings_names(model: &JValue) -> Vec<String> {
        model
            .as_str()
            .map(|m| vec![m.to_owned()])
            .unwrap_or_default()
    }
    pub fn settings_facts(&self, model: &JValue, o: Option<&Outside>) -> Answer {
        let mut g = self.lock();
        let b = match Self::live(&mut g) {
            Ok(b) => b,
            Err(a) => return a,
        };
        let mut answer = vec![
            ("available".to_owned(), JValue::Bool(true)),
            ("settingsAvailable".to_owned(), JValue::Bool(true)),
        ];
        if let JValue::Str(m) = model {
            let Some(facts) = o.and_then(|o| self.settings_snapshot(b, m, o)) else {
                return unavailable();
            };
            answer.push(("facts".to_owned(), facts));
        }
        ok(JValue::Obj(answer))
    }
    pub fn settings_commit(&self, model: &str, allow: bool, token: &str, o: &Outside) -> Answer {
        let mut g = self.lock();
        let b = match Self::live(&mut g) {
            Ok(b) => b,
            Err(a) => return a,
        };
        let Some(fresh) = self.settings_snapshot(b, model, o) else {
            return unavailable();
        };
        if fresh.get("token").as_str() != Some(token) {
            return not_applied();
        }
        let mut next = b.doc.clone();
        let mut settings = next.get("settings").clone();
        if allow {
            set_key(
                &mut settings,
                model,
                JValue::obj([("allowQ5Kv", true.into())]),
            );
        } else {
            remove_key(&mut settings, model);
        }
        set_key(&mut next, "settings", settings);
        if persist(&b.path, &next).is_err() {
            return unavailable();
        }
        b.doc = next;
        b.rev += 1;
        let Some(facts) = self.settings_snapshot(b, model, o) else {
            return unavailable();
        };
        ok(JValue::obj([("applied", true.into()), ("facts", facts)]))
    }

    // ---- cancel ----------------------------------------------------------------------------

    fn generation(&self, b: &Bound) -> String {
        self.tok("run", &[b.job_serial.to_string(), b.epoch.to_string()])
    }
    fn cancel_token(&self, b: &Bound) -> String {
        self.tok(
            "cancel",
            &[
                b.rev.to_string(),
                b.job_serial.to_string(),
                b.epoch.to_string(),
                b.hold.is_some().to_string(),
                b.cancelled.to_string(),
            ],
        )
    }
    fn cancel_job(b: &Bound) -> JValue {
        const KEYS: &[&str] = &[
            "id",
            "model",
            "bulk",
            "mode",
            "promptBudgetSeconds",
            "status",
            "phase",
            "startedAt",
            "log",
            "models",
            "queueProgress",
            "finishedAt",
            "error",
            "failedPhase",
            "failedStep",
            "restored",
            "skipped",
            "waiting",
            "result",
        ];
        let job = b.doc.get("job");
        if matches!(job, JValue::Undefined | JValue::Null) {
            return JValue::Null;
        }
        strip(&pick(job, KEYS), &|k| private_key(k) || http_exact(k))
    }
    pub fn cancel_facts(&self) -> Answer {
        let mut g = self.lock();
        let b = match Self::live(&mut g) {
            Ok(b) => b,
            Err(a) => return a,
        };
        ok(JValue::obj([
            ("available", true.into()),
            ("token", self.cancel_token(b).into()),
            ("generation", self.generation(b).into()),
            ("job", Self::cancel_job(b)),
        ]))
    }
    /// Compares the token and records the cancellation request; the glue then reaches the actor.
    pub fn cancel_begin(&self, token: &str) -> Result<AbortTicket, Answer> {
        let mut g = self.lock();
        let b = Self::live(&mut g)?;
        let job = b.doc.get("job");
        if token != self.cancel_token(b) || !job.truthy() {
            return Err(not_applied());
        }
        let reply = ok(JValue::obj([
            ("applied", true.into()),
            ("generation", self.generation(b).into()),
            ("job", Self::cancel_job(b)),
        ]));
        let ticket = AbortTicket {
            job_id: job.get("id").clone(),
            epoch: b.epoch,
            reply,
            prior_cancelled: b.cancelled,
            serial: b.job_serial,
        };
        b.cancelled = true;
        b.rev += 1;
        Ok(ticket)
    }
    /// The actor did not take the abort: the request is withdrawn, so a retry may reach it again.
    pub fn cancel_undo(&self, t: &AbortTicket) {
        let mut g = self.lock();
        if let Some(b) = g.as_mut() {
            if b.job_serial == t.serial && b.epoch == t.epoch {
                b.cancelled = t.prior_cancelled;
                b.rev += 1;
            }
        }
    }

    // ---- clear -----------------------------------------------------------------------------

    fn clear_guard(b: &Bound) -> JValue {
        let job = b.doc.get("job");
        JValue::obj([
            ("starting", b.hold.is_some().into()),
            ("jobPresent", job.truthy().into()),
            (
                "status",
                if job.truthy() {
                    match job.get("status") {
                        JValue::Undefined => JValue::Null,
                        s => s.clone(),
                    }
                } else {
                    JValue::Null
                },
            ),
            (
                "reloadPending",
                (job.truthy() && job.get("_reloadPending").truthy()).into(),
            ),
        ])
    }
    fn clear_token(&self, b: &Bound, generation: &str) -> String {
        let job = b.doc.get("job");
        self.tok(
            "clear",
            &[
                generation.to_owned(),
                canon(&Self::clear_guard(b)),
                if job.truthy() {
                    canon(job)
                } else {
                    "null".into()
                },
            ],
        )
    }
    pub fn clear_facts(&self) -> Answer {
        let mut g = self.lock();
        let b = match Self::live(&mut g) {
            Ok(b) => b,
            Err(a) => return a,
        };
        let generation = self.tok("clear-gen", &[b.rev.to_string()]);
        let token = self.clear_token(b, &generation);
        let mut fields = vec![
            ("available".to_owned(), JValue::Bool(true)),
            ("generation".to_owned(), generation.into()),
            ("token".to_owned(), token.into()),
        ];
        if let JValue::Obj(guard) = Self::clear_guard(b) {
            fields.extend(guard);
        }
        ok(JValue::Obj(fields))
    }
    pub fn clear(&self, token: &str) -> Answer {
        let mut g = self.lock();
        let b = match Self::live(&mut g) {
            Ok(b) => b,
            Err(a) => return a,
        };
        let generation = self.tok("clear-gen", &[b.rev.to_string()]);
        if self.clear_token(b, &generation) != token {
            return not_applied();
        }
        let job = b.doc.get("job");
        if b.hold.is_some()
            || !job.truthy()
            || !FINISHED.contains(&job.get("status").as_str().unwrap_or(""))
            || job.get("_reloadPending").truthy()
        {
            return not_applied();
        }
        let mut next = b.doc.clone();
        remove_key(&mut next, "job");
        if persist(&b.path, &next).is_err() {
            return unavailable();
        }
        b.doc = next;
        b.rev += 1;
        b.job_rev += 1;
        b.job_serial += 1;
        ok(JValue::obj([("applied", true.into())]))
    }

    // ---- resume ----------------------------------------------------------------------------

    pub fn resume_names(&self) -> Option<Vec<String>> {
        let g = self.lock();
        let b = g.as_ref()?;
        let mut out = Vec::new();
        if let JValue::Arr(models) = b.doc.get("job").get("models") {
            for m in models {
                for k in ["model", "base"] {
                    if let Some(s) = m.get(k).as_str() {
                        if !out.iter().any(|x| x == s) {
                            out.push(s.to_owned());
                        }
                    }
                }
            }
        }
        Some(out)
    }
    fn resume_systems(b: &Bound, o: &Outside) -> Option<Vec<String>> {
        let mut out = Vec::new();
        if let JValue::Arr(models) = b.doc.get("job").get("models") {
            for m in models {
                out.push(match m.get("model").as_str() {
                    Some(name) => o.model(name)?.1.to_string(),
                    None => "false".to_owned(),
                });
            }
        }
        Some(out)
    }
    fn resume_token(&self, b: &Bound, o: &Outside) -> Option<String> {
        let mut parts = vec![
            b.rev.to_string(),
            b.job_serial.to_string(),
            b.epoch.to_string(),
            b.hold.is_some().to_string(),
            b.cancelled.to_string(),
            o.revision.clone().unwrap_or_else(|| "\u{0}null".into()),
        ];
        parts.extend(Self::resume_systems(b, o)?);
        Some(self.tok("resume", &parts))
    }
    fn resume_queue(b: &Bound, o: &Outside) -> Option<JValue> {
        let status = |v: &JValue| match v.as_str() {
            Some(
                s @ ("pending" | "running" | "passed" | "failed" | "interrupted" | "cancelled"
                | "skipped"),
            ) => JValue::from(s),
            _ => JValue::Null,
        };
        let models = match b.doc.get("job").get("models") {
            JValue::Arr(m) => m,
            _ => return Some(JValue::Null),
        };
        let mut out = Vec::new();
        for item in models {
            if !matches!(item, JValue::Obj(_) | JValue::Arr(_)) {
                out.push(JValue::Null);
                continue;
            }
            let configured = |field: &str| -> Option<bool> {
                match item.get(field).as_str() {
                    Some(n) => Some(o.model(n)?.0),
                    None => Some(false),
                }
            };
            let identity = match item.get("_identity") {
                JValue::Str(s) if is_hex64(s) => JValue::from(s.as_str()),
                _ => JValue::Null,
            };
            let phases = match item.get("phases") {
                JValue::Arr(phases) => JValue::Arr(
                    phases
                        .iter()
                        .map(|p| {
                            if !matches!(p, JValue::Obj(_) | JValue::Arr(_)) {
                                return JValue::Null;
                            }
                            let steps = match p.get("steps") {
                                JValue::Arr(steps) => JValue::Arr(
                                    steps
                                        .iter()
                                        .map(|s| JValue::obj([("status", status(s.get("status")))]))
                                        .collect(),
                                ),
                                _ => JValue::Null,
                            };
                            JValue::obj([
                                ("id", string_of(p.get("id"))),
                                ("status", status(p.get("status"))),
                                (
                                    "restored",
                                    p.get("restored")
                                        .as_bool()
                                        .map_or(JValue::Null, JValue::Bool),
                                ),
                                ("steps", steps),
                            ])
                        })
                        .collect(),
                ),
                _ => JValue::Null,
            };
            out.push(JValue::obj([
                ("model", string_of(item.get("model"))),
                ("base", string_of(item.get("base"))),
                ("mode", string_of(item.get("mode"))),
                ("status", status(item.get("status"))),
                ("sectionCreated", item.get("sectionCreated").truthy().into()),
                ("identitySet", item.get("_identity").truthy().into()),
                ("identity", identity),
                ("planned", item.get("plan").truthy().into()),
                ("configured", configured("model")?.into()),
                ("baseConfigured", configured("base")?.into()),
                ("phases", phases),
            ]));
        }
        Some(JValue::Arr(out))
    }
    fn resume_facts_of(&self, b: &Bound, o: &Outside) -> Option<Vec<(String, JValue)>> {
        let j = b.doc.get("job");
        let job = if j.truthy() {
            JValue::obj([
                (
                    "status",
                    match j.get("status").as_str() {
                        Some(
                            s @ ("pending" | "running" | "passed" | "failed" | "interrupted"
                            | "cancelled" | "skipped"),
                        ) => JValue::from(s),
                        _ => JValue::Null,
                    },
                ),
                ("model", string_of(j.get("model"))),
                ("unsafe", j.get("_unsafe").truthy().into()),
                ("revision", string_of(j.get("_revision"))),
                ("pendingRevision", string_of(j.get("_pendingRevision"))),
                ("models", Self::resume_queue(b, o)?),
            ])
        } else {
            JValue::Null
        };
        Some(vec![
            ("token".to_owned(), self.resume_token(b, o)?.into()),
            ("generation".to_owned(), self.generation(b).into()),
            ("starting".to_owned(), b.hold.is_some().into()),
            ("revision".to_owned(), o.revision_value()),
            ("job".to_owned(), job),
        ])
    }
    pub fn resume_facts(&self, o: &Outside) -> Answer {
        let mut g = self.lock();
        let b = match Self::live(&mut g) {
            Ok(b) => b,
            Err(a) => return a,
        };
        let Some(mut fields) = self.resume_facts_of(b, o) else {
            return unavailable();
        };
        fields.insert(0, ("available".to_owned(), true.into()));
        ok(JValue::Obj(fields))
    }

    /// Takes the preparation hold if `token` still names this state. `Err` is the bridge answer.
    pub fn resume_prepare_begin(
        &self,
        token: &str,
        stage: &JValue,
        selectors: &JValue,
        o: &Outside,
    ) -> Result<Prepared, Answer> {
        let mut g = self.lock();
        let b = Self::live(&mut g)?;
        if b.prep.is_some()
            || b.hold.is_some()
            || !b.doc.get("job").truthy()
            || self.resume_token(b, o).as_deref() != Some(token)
        {
            return Err(not_applied());
        }
        let stage = match stage.as_str() {
            Some("listing") => Stage::Listing,
            Some("identities") => Stage::Identities,
            _ => return Err(unavailable()),
        };
        let picks: &[JValue] = match selectors {
            JValue::Arr(a) if a.len() <= 512 => a,
            JValue::Undefined => &[],
            _ => return Err(unavailable()),
        };
        let JValue::Arr(models) = b.doc.get("job").get("models").clone() else {
            return Err(unavailable());
        };
        if models.len() > 512 {
            return Err(unavailable());
        }
        let mut seen = Vec::new();
        let mut chosen = Vec::new();
        for p in picks {
            let index = p.get("index").as_num();
            let field = p.get("field").as_str();
            let (Some(index), Some(field)) = (index, field) else {
                return Err(unavailable());
            };
            if index.fract() != 0.0
                || index < 0.0
                || index >= models.len() as f64
                || !matches!(field, "model" | "base")
                || seen.contains(&(index as usize))
            {
                return Err(unavailable());
            }
            let Some(id) = models
                .get(index as usize)
                .and_then(|m| m.get(field).as_str().map(str::to_owned))
            else {
                return Err(unavailable());
            };
            seen.push(index as usize);
            chosen.push((index as usize, field.to_owned(), id));
        }
        // The synchronous repair of a job one revision behind models.ini (#1093), only after the
        // exact state comparison above.
        if stage == Stage::Listing {
            let mut job = b.doc.get("job").clone();
            if !matches!(job.get("_pendingRevision"), JValue::Undefined) {
                if let (Some(rev), JValue::Str(pending)) =
                    (o.revision.as_deref(), job.get("_pendingRevision").clone())
                {
                    if rev == pending {
                        set_key(&mut job, "_revision", JValue::Str(pending));
                    }
                }
                remove_key(&mut job, "_pendingRevision");
                let mut next = b.doc.clone();
                set_key(&mut next, "job", job);
                if persist(&b.path, &next).is_err() {
                    return Err(unavailable());
                }
                b.doc = next;
                b.rev += 1;
                b.job_rev += 1;
            }
        }
        b.prep_seq += 1;
        let id = b.prep_seq;
        let owed = b.doc.get("job").get("_reloadPending").truthy();
        b.hold = Some(Hold::Prepare);
        b.rev += 1;
        b.prep = Some(Prep {
            id,
            stage,
            serial: b.job_serial,
            epoch: b.epoch,
            baseline_rev: b.rev,
            owed,
            before_doc: b.doc.clone(),
            outside: o.clone(),
            noted: false,
        });
        let items = JValue::Arr(
            models
                .iter()
                .map(|m| {
                    JValue::obj([
                        ("model", m.get("model").clone()),
                        ("base", m.get("base").clone()),
                    ])
                })
                .collect(),
        );
        Ok(Prepared {
            id,
            stage,
            items,
            selectors: chosen,
        })
    }
    /// Settles a preparation: releases the hold and, if nothing but the allowed reload note changed
    /// the state meanwhile, answers with fresh facts plus the actor's `result` fields.
    pub fn resume_prepare_end(&self, id: u64, result: Result<JValue, bool>, o: &Outside) -> Answer {
        let mut g = self.lock();
        let b = match Self::live(&mut g) {
            Ok(b) => b,
            Err(a) => return a,
        };
        let Some(p) = b.prep.take_if(|p| p.id == id) else {
            return not_applied();
        };
        b.hold = None;
        b.rev += 1;
        let current = b.job_serial == p.serial
            && b.epoch == p.epoch
            && b.rev == p.baseline_rev + 1
            && p.outside == *o;
        let result = match result {
            Ok(r) if current => r,
            Ok(_) | Err(false) => return not_applied(),
            Err(true) => {
                return (
                    409,
                    JValue::obj([("applied", false.into()), ("fault", "reload".into())]),
                )
            }
        };
        let Some(mut fields) = self.resume_facts_of(b, o) else {
            return unavailable();
        };
        fields.insert(0, ("applied".to_owned(), true.into()));
        if let JValue::Obj(extra) = result {
            fields.extend(extra);
        }
        ok(JValue::Obj(fields))
    }
    /// A preparation the glue abandoned (the caller went away, the actor did not answer).
    pub fn resume_prepare_abandon(&self, id: u64) {
        let mut g = self.lock();
        if let Some(b) = g.as_mut() {
            if b.prep.take_if(|p| p.id == id).is_some() {
                b.hold = None;
                b.rev += 1;
            }
        }
    }

    pub fn resume_commit(&self, token: &str, plan: &JValue, o: &Outside) -> ResumeCommit {
        let refuse = |a: Answer| ResumeCommit {
            answer: a,
            launch: None,
        };
        let mut g = self.lock();
        let b = match Self::live(&mut g) {
            Ok(b) => b,
            Err(a) => return refuse(a),
        };
        if b.prep.is_some()
            || b.hold.is_some()
            || !b.doc.get("job").truthy()
            || self.resume_token(b, o).as_deref() != Some(token)
        {
            return refuse(not_applied());
        }
        let mut job = b.doc.get("job").clone();
        let Some(next_job) = apply_plan(&mut job, plan) else {
            return refuse(unavailable());
        };
        let launching = next_job;
        let mut next = b.doc.clone();
        set_key(&mut next, "job", job.clone());
        if persist(&b.path, &next).is_err() {
            return refuse(unavailable());
        }
        b.doc = next;
        b.rev += 1;
        b.job_rev += 1;
        if launching {
            b.epoch += 1;
            b.cancelled = false;
        }
        let Some(mut fields) = self.resume_facts_of(b, o) else {
            return refuse(unavailable());
        };
        fields.insert(0, ("applied".to_owned(), true.into()));
        fields.push(("publicJob".to_owned(), public_job(&job)));
        ResumeCommit {
            answer: ok(JValue::Obj(fields)),
            launch: launching.then(|| Launch {
                epoch: b.epoch,
                job_rev: b.job_rev,
                job: b.doc.get("job").clone(),
                history: b.doc.get("history").clone(),
                serial: b.job_serial,
            }),
        }
    }
    /// The actor did not take the launch: the job stops as interrupted (fail closed), so no
    /// running job exists without a runner. A newer change to the job is left alone.
    pub fn launch_failed(&self, l: &Launch) {
        let mut g = self.lock();
        let Some(b) = g.as_mut() else { return };
        if b.job_serial != l.serial || b.epoch != l.epoch || b.job_rev != l.job_rev {
            return;
        }
        let mut job = b.doc.get("job").clone();
        if job.get("status").as_str() != Some("running") {
            return;
        }
        set_key(&mut job, "status", "interrupted".into());
        set_key(&mut job, "phase", "Interrupted".into());
        set_key(&mut job, "finishedAt", JValue::Num(now_ms()));
        set_key(
            &mut job,
            "error",
            "The resume could not start. Resume again.".into(),
        );
        let mut next = b.doc.clone();
        set_key(&mut next, "job", job);
        if persist(&b.path, &next).is_ok() {
            b.doc = next;
            b.rev += 1;
            b.job_rev += 1;
        }
    }
}

fn snapshot(b: &Bound, writer: Option<&str>) -> JValue {
    let mut fields = vec![
        ("rev".to_owned(), JValue::Num(b.rev as f64)),
        ("jobRev".to_owned(), JValue::Num(b.job_rev as f64)),
        ("epoch".to_owned(), JValue::Num(b.epoch as f64)),
    ];
    if let Some(w) = writer {
        fields.push(("writer".to_owned(), w.into()));
    }
    fields.push(("state".to_owned(), b.doc.clone()));
    JValue::Obj(fields)
}

fn public_job(job: &JValue) -> JValue {
    const KEYS: &[&str] = &[
        "id",
        "model",
        "bulk",
        "mode",
        "promptBudgetSeconds",
        "status",
        "phase",
        "startedAt",
        "log",
        "models",
        "queueProgress",
        "finishedAt",
        "error",
        "failedPhase",
        "failedStep",
        "restored",
        "skipped",
        "waiting",
        "result",
    ];
    strip(&pick(job, KEYS), &|k| private_key(k) || http_exact(k))
}

/// Whether `after` is `before` with the reload flag cleared and exactly the re-read note appended.
fn reload_note_only(before: &JValue, after: &JValue) -> bool {
    let JValue::Obj(_) = after else { return false };
    let Some(note) = (match after.get("log") {
        JValue::Arr(log) => log.last(),
        _ => None,
    }) else {
        return false;
    };
    if note.get("text").as_str() != Some(NOTE_REREAD)
        || !note.get("at").as_num().is_some_and(f64::is_finite)
    {
        return false;
    }
    let mut expected = before.clone();
    set_key(&mut expected, "_reloadPending", false.into());
    let mut log = match expected.get("log") {
        JValue::Arr(l) => l.clone(),
        _ => Vec::new(),
    };
    log.push(note.clone());
    if log.len() > MAX_LOG {
        log.remove(0);
    }
    set_key(&mut expected, "log", JValue::Arr(log));
    // `j.log ||= []` creates the key last; the comparison is by value, not by key position.
    same_fields(&expected, after)
}
fn same_fields(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Obj(x), JValue::Obj(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.iter().any(|(k2, v2)| k == k2 && same_fields(v, v2)))
        }
        (JValue::Arr(x), JValue::Arr(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same_fields(p, q))
        }
        _ => a == b,
    }
}

fn indices(list: &JValue, length: usize) -> Option<Vec<usize>> {
    let JValue::Arr(items) = list else {
        return None;
    };
    if items.len() > length {
        return None;
    }
    let mut out: Vec<usize> = Vec::new();
    for n in items {
        let n = n.as_num()?;
        if n.fract() != 0.0 || n < 0.0 || n >= length as f64 {
            return None;
        }
        let n = n as usize;
        if out.last().is_some_and(|last| *last >= n) {
            return None;
        }
        out.push(n);
    }
    Some(out)
}

/// The resume patch (Core compareAndResume), applied to `job` in place. `Some(launch)`; `None`:
/// the patch is invalid and `job` is left as it was.
fn apply_plan(job: &mut JValue, plan: &JValue) -> Option<bool> {
    let original = job.clone();
    let result = (|| -> Option<bool> {
        let JValue::Arr(models) = job.get("models").clone() else {
            return None;
        };
        if models.len() > 512 || !matches!(plan, JValue::Obj(_)) {
            return None;
        }
        match plan.get("kind").as_str() {
            Some("filter") => {
                let keep = indices(plan.get("keep"), models.len())?;
                let JValue::Arr(skipped) = plan.get("skipped") else {
                    return None;
                };
                let complete = plan.get("complete").as_bool()?;
                let model_index = match plan.get("model") {
                    JValue::Null => None,
                    JValue::Num(n)
                        if n.fract() == 0.0 && *n >= 0.0 && keep.contains(&(*n as usize)) =>
                    {
                        Some(*n as usize)
                    }
                    _ => return None,
                };
                let removed: Vec<usize> = (0..models.len()).filter(|i| !keep.contains(i)).collect();
                if skipped.len() != removed.len() {
                    return None;
                }
                let mut notes = Vec::new();
                for (p, i) in skipped.iter().zip(&removed) {
                    let reason = p.get("reason").as_str()?;
                    if p.get("index").as_num() != Some(*i as f64) || wtf16::len(reason) > 256 {
                        return None;
                    }
                    notes.push(JValue::obj([
                        ("model", models.get(*i)?.get("model").clone()),
                        ("reason", reason.into()),
                    ]));
                }
                set_key(
                    job,
                    "models",
                    JValue::Arr(
                        keep.iter()
                            .map(|i| models.get(*i).cloned())
                            .collect::<Option<Vec<_>>>()?,
                    ),
                );
                let mut all = match job.get("skipped") {
                    JValue::Arr(a) => a.clone(),
                    _ => Vec::new(),
                };
                all.extend(notes);
                set_key(job, "skipped", JValue::Arr(all));
                if let Some(i) = model_index {
                    set_key(job, "model", models.get(i)?.get("model").clone());
                }
                if complete {
                    set_key(job, "status", "passed".into());
                    set_key(job, "phase", "Done".into());
                    set_key(job, "finishedAt", JValue::Num(now_ms()));
                    remove_key(job, "error");
                }
                Some(false)
            }
            Some("launch") => {
                let JValue::Arr(reset) = plan.get("reset") else {
                    return None;
                };
                let idx: Vec<JValue> = reset.iter().map(|p| p.get("index").clone()).collect();
                let ids = indices(&JValue::Arr(idx), models.len())?;
                let mut models = models;
                for (item, mi) in reset.iter().zip(ids) {
                    let JValue::Arr(phases) = models.get(mi)?.get("phases").clone() else {
                        return None;
                    };
                    let JValue::Arr(patches) = item.get("phases") else {
                        return None;
                    };
                    let pidx: Vec<JValue> =
                        patches.iter().map(|p| p.get("index").clone()).collect();
                    let pids = indices(&JValue::Arr(pidx), phases.len())?;
                    let mut phases = phases;
                    for (patch, pi) in patches.iter().zip(pids) {
                        let JValue::Arr(steps) = phases.get(pi)?.get("steps").clone() else {
                            return None;
                        };
                        let keep = match patch.get("keepSteps") {
                            JValue::Null => None,
                            other => Some(indices(other, steps.len())?),
                        };
                        let mut phase = phases.get(pi)?.clone();
                        set_key(&mut phase, "status", "pending".into());
                        remove_key(&mut phase, "reason");
                        remove_key(&mut phase, "restored");
                        let steps = match keep {
                            None => JValue::Arr(
                                steps
                                    .iter()
                                    .map(|s| {
                                        let mut s = s.clone();
                                        set_key(&mut s, "status", "pending".into());
                                        remove_key(&mut s, "reason");
                                        s
                                    })
                                    .collect(),
                            ),
                            Some(keep) => JValue::Arr(
                                keep.iter()
                                    .map(|i| steps.get(*i).cloned())
                                    .collect::<Option<Vec<_>>>()?,
                            ),
                        };
                        set_key(&mut phase, "steps", steps);
                        *phases.get_mut(pi)? = phase;
                    }
                    let mut row = models.get(mi)?.clone();
                    set_key(&mut row, "status", "pending".into());
                    remove_key(&mut row, "error");
                    remove_key(&mut row, "_foreignYields");
                    set_key(&mut row, "phases", JValue::Arr(phases));
                    *models.get_mut(mi)? = row;
                }
                // The first thing the actor's run does, synchronously, before it first waits: so the
                // answer the caller gets (and the file) already names the item it begins with.
                let first = models
                    .iter()
                    .find(|m| m.get("status").as_str() != Some("passed"))
                    .map(|m| m.get("model").clone());
                set_key(job, "models", JValue::Arr(models));
                set_key(job, "status", "running".into());
                remove_key(job, "error");
                remove_key(job, "finishedAt");
                remove_key(job, "failedPhase");
                remove_key(job, "failedStep");
                match first {
                    Some(model) => {
                        set_key(job, "model", model);
                        set_key(job, "phase", "Waiting for chat to become idle".into());
                        set_key(job, "waiting", true.into());
                        Some(true)
                    }
                    None => {
                        // Nothing is left to run: the run's loop would end at once as passed.
                        set_key(job, "status", "passed".into());
                        set_key(job, "phase", "Done".into());
                        set_key(job, "waiting", false.into());
                        set_key(job, "finishedAt", JValue::Num(now_ms()));
                        Some(false)
                    }
                }
            }
            _ => None,
        }
    })();
    if result.is_none() {
        *job = original;
    }
    result
}

// ---- the state file ---------------------------------------------------------------------------

/// Loads the file the way the Node actor did: a missing or unreadable file or invalid JSON is an
/// empty state; a file that is JSON but not an object is refused (Node threw on it).
fn load(path: &Path) -> Result<JValue, Fault> {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len > MAX_STATE_BYTES {
        return Err(Fault::State);
    }
    let text = std::fs::read(path)
        .ok()
        .and_then(|b| String::from_utf8(b).ok());
    let mut doc = match text.and_then(|t| wtf16::parse(&t).ok()) {
        Some(v @ JValue::Obj(_)) => v,
        Some(_) => return Err(Fault::State),
        None => JValue::Obj(Vec::new()),
    };
    for key in ["history", "settings"] {
        match doc.get(key) {
            JValue::Obj(_) => {}
            v if !v.truthy() => set_key(&mut doc, key, JValue::Obj(Vec::new())),
            _ => return Err(Fault::State),
        }
    }
    Ok(doc)
}

/// Writes `doc` as the actor did: a temporary file beside it (mode 0600), then a rename.
fn persist(path: &Path, doc: &JValue) -> Result<(), u16> {
    let text = wtf16::stringify(doc).ok_or(500u16)?;
    let tmp = path.with_file_name(format!(
        "{}.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state"),
        hex_token(16)
    ));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        503u16
    })
}

#[cfg(test)]
mod tests;
