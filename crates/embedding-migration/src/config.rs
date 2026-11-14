//! `embedding-config.cjs`'s `createEmbeddingConfig` / `loadEmbeddingConfig`: the one embedding
//! configuration (active / next / previous). The environment only seeds it; once a change is made
//! it is saved to the state file, which wins over the environment on the next start.
//!
//! As production runs it (a native owner present, a state file set) a change is accepted only
//! inside [`EmbeddingConfig::with_lock`]: an OS flock (LOCK_EX) on `<state file>.lock`, the same
//! lock core's `embedding-config-lock.cjs` (`flock -x -w 5`) and the native embedding worker's
//! publication take. The file is written to `<file>.<pid>.tmp` (mode 0600) and renamed over it.
//!
//! Differences from core, on purpose (each filed): a state file that exists but cannot be read
//! (permissions, an I/O error) is damaged here; core read it as absent and let the environment's
//! values take over, which its own comment says must not happen.

use std::fs::{File, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::js;
use crate::target::{
    normalize_target, opt_json, same_target, seed_from_env, Profiles, Refusal, State, Target,
};

/// `flock -w 5`: how long a change waits for the publication lock.
pub const LOCK_WAIT: Duration = Duration::from_secs(5);
const LOCK_RETRY: Duration = Duration::from_millis(10);

/// What `get()` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub active: Target,
    pub next: Option<Target>,
    pub previous: Option<Target>,
    /// `saved` once a file was read or a change written, else `environment`.
    pub source: &'static str,
    pub version: u64,
}

#[derive(Debug, Clone)]
struct Held {
    reason: String,
    transient: bool,
    target: Option<Target>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Active,
    Next,
}

#[derive(Debug)]
pub struct EmbeddingConfig {
    state: State,
    state_file: Option<PathBuf>,
    native_owner: bool,
    profiles: Profiles,
    saved: bool,
    saved_change: bool,
    version: u64,
    in_hold: bool,
    /// Set by `refuse_seeded` on the active target: changes are refused until a restart.
    locked: Option<String>,
    held_active: Option<Held>,
    held_next: Option<Held>,
    /// Why embedding is off (a damaged saved file, or a refused seeded active address).
    pub damaged: Option<String>,
    /// `loadEmbeddingConfig`'s fallback for a damaged file: every set-next is refused.
    off_for_damage: bool,
}

/// The start-up inputs.
pub struct Options<'a> {
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub state_file: Option<PathBuf>,
    pub fallback_model: &'a str,
    pub profiles: Profiles,
    /// The native embedding worker owns the pass and the publication lock (production).
    pub native_owner: bool,
}

fn bad_state(file: &Path, why: &str) -> Refusal {
    Refusal::new(
        409,
        "badState",
        format!(
            "The saved embedding configuration {} cannot be read ({why}). Fix or delete it.",
            file.display()
        ),
    )
}

/// `readState`: None when there is no file; the saved targets; or the `badState` error.
fn read_state(file: &Path, profiles: &Profiles) -> Result<Option<State>, Refusal> {
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(bad_state(file, &e.to_string())),
    };
    let text = String::from_utf8_lossy(&bytes);
    let s: Value = serde_json::from_str(&text).map_err(|e| bad_state(file, &e.to_string()))?;
    if s.get("version").and_then(Value::as_f64) != Some(1.0) {
        return Err(bad_state(file, "unknown version"));
    }
    let target = |key: &str| -> Result<Option<Target>, Refusal> {
        if !js::js_truthy(s.get(key)) {
            return Ok(None);
        }
        normalize_target(s.get(key), profiles).map(Some).map_err(|r| bad_state(file, &r.message))
    };
    let active = normalize_target(s.get("active"), profiles).map_err(|r| bad_state(file, &r.message))?;
    Ok(Some(State { active, next: target("next")?, previous: target("previous")? }))
}

fn pretty_target(t: Option<&Target>) -> String {
    let Some(t) = t else { return "null".into() };
    let s = |v: &str| Value::String(v.to_owned()).to_string();
    format!(
        "{{\n    \"baseUrl\": {},\n    \"model\": {},\n    \"profile\": {}\n  }}",
        t.base_url.as_deref().map_or_else(|| "null".into(), s),
        s(&t.model),
        s(&t.profile)
    )
}

/// The bytes core writes: `JSON.stringify({ version: 1, ...state }, null, 2)`, no final newline.
pub fn state_text(state: &State) -> String {
    format!(
        "{{\n  \"version\": 1,\n  \"active\": {},\n  \"next\": {},\n  \"previous\": {}\n}}",
        pretty_target(Some(&state.active)),
        pretty_target(state.next.as_ref()),
        pretty_target(state.previous.as_ref())
    )
}

/// The lock file of `file`: `<file>.lock`.
pub fn lock_path(file: &Path) -> PathBuf {
    let mut p = file.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// Holds the flock until dropped.
#[derive(Debug)]
pub struct Flock {
    _file: File,
}

/// Takes the flock on `<file>.lock`, waiting up to [`LOCK_WAIT`].
pub fn flock(file: &Path) -> Result<Flock, Refusal> {
    let unavailable = || Refusal::plain("Embedding config lock unavailable");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let f = options.open(lock_path(file)).map_err(|_| unavailable())?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(Flock { _file: f }),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(LOCK_RETRY),
            Err(TryLockError::WouldBlock) => return Err(Refusal::plain("Embedding config lock timed out")),
            Err(TryLockError::Error(_)) => return Err(unavailable()),
        }
    }
}

impl EmbeddingConfig {
    /// `createEmbeddingConfig`: Err(`badState`) for a damaged state file. Returns the seeding
    /// warning core logs, if any.
    pub fn create(o: Options<'_>) -> Result<(Self, Option<String>), Refusal> {
        let saved = match &o.state_file {
            Some(file) => read_state(file, &o.profiles)?,
            None => None,
        };
        let (state, warning) = match saved.clone() {
            Some(s) => (s, None),
            None => seed_from_env(o.env, o.fallback_model, &o.profiles),
        };
        Ok((
            Self {
                state,
                state_file: o.state_file,
                native_owner: o.native_owner,
                profiles: o.profiles,
                saved: saved.is_some(),
                saved_change: false,
                version: 0,
                in_hold: false,
                locked: None,
                held_active: None,
                held_next: None,
                damaged: None,
                off_for_damage: false,
            },
            warning,
        ))
    }

    /// `loadEmbeddingConfig`: a damaged state file does not stop the server. Embedding is then off,
    /// fail closed (active profile `unavailable`, kept in memory only, every set-next refused).
    /// Returns the lines core logs.
    pub fn load(o: Options<'_>) -> (Self, Vec<String>) {
        let profiles = o.profiles.clone();
        match Self::create(o) {
            Ok((config, warning)) => (config, warning.into_iter().collect()),
            Err(err) => {
                let off_env = |k: &str| {
                    matches!(k, "EMBEDDING_MODEL" | "EMBEDDING_PROFILE").then(|| "unavailable".to_owned())
                };
                let (state, _) = seed_from_env(&off_env, "default", &profiles);
                let config = Self {
                    state,
                    state_file: None,
                    native_owner: false,
                    profiles,
                    saved: false,
                    saved_change: false,
                    version: 0,
                    in_hold: false,
                    locked: None,
                    held_active: None,
                    held_next: None,
                    damaged: Some(err.message.clone()),
                    off_for_damage: true,
                };
                (config, vec![format!("[embedding] {} Embeddings are off until then.", err.message)])
            }
        }
    }

    pub fn profiles(&self) -> &Profiles {
        &self.profiles
    }

    pub fn get(&self) -> Snapshot {
        Snapshot {
            active: self.state.active.clone(),
            next: self.state.next.clone(),
            previous: self.state.previous.clone(),
            source: if self.saved_change || self.saved { "saved" } else { "environment" },
            version: self.version,
        }
    }

    pub fn active(&self) -> &Target {
        &self.state.active
    }

    pub fn next(&self) -> Option<&Target> {
        self.state.next.as_ref()
    }

    pub fn previous(&self) -> Option<&Target> {
        self.state.previous.as_ref()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Every model name this process may embed with now or after a switch or rollback.
    pub fn model_names(&self) -> Vec<String> {
        std::iter::once(&self.state.active)
            .chain(self.state.next.iter())
            .chain(self.state.previous.iter())
            .map(|t| t.model.clone())
            .collect()
    }

    /// `withLock(action)`: runs `action` holding the publication flock when a native owner and a
    /// state file are set (else directly). A lock that cannot be had is a plain error (500).
    pub fn with_lock<R>(&mut self, action: impl FnOnce(&mut Self) -> R) -> Result<R, Refusal> {
        let Some(file) = self.state_file.clone().filter(|_| self.native_owner && !self.in_hold) else {
            return Ok(action(self));
        };
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Refusal::plain(e.to_string()))?;
        }
        let hold = flock(&file)?;
        self.in_hold = true;
        let out = action(self);
        self.in_hold = false;
        drop(hold);
        Ok(out)
    }

    fn persist(&self, state: &State) -> std::io::Result<()> {
        let Some(file) = &self.state_file else { return Ok(()) };
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut tmp = file.as_os_str().to_owned();
        tmp.push(format!(".{}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&tmp)?;
        f.write_all(state_text(state).as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, file)
    }

    fn change(&mut self, next: State) -> Result<Snapshot, Refusal> {
        if self.native_owner && self.state_file.is_some() && !self.in_hold {
            return Err(Refusal::plain("Embedding configuration requires the publication lock"));
        }
        if let Some(reason) = &self.locked {
            return Err(Refusal::new(409, "badState", reason.clone()));
        }
        self.persist(&next).map_err(|e| Refusal::plain(e.to_string()))?;
        self.state = next;
        self.version += 1;
        self.saved_change = true;
        Ok(self.get())
    }

    /// `setNext(target)`: `target` already normalized by the caller (the migration normalizes
    /// first, as core does).
    pub fn set_next(&mut self, t: Target) -> Result<Snapshot, Refusal> {
        if self.off_for_damage {
            let why = self.damaged.clone().unwrap_or_default();
            return Err(Refusal::new(409, "badState", why));
        }
        if same_target(Some(&t), Some(&self.state.active)) {
            return Err(Refusal::new(409, "alreadyActive", "That model, profile and address are already active."));
        }
        let next = State { next: Some(t), ..self.state.clone() };
        self.change(next)
    }

    pub fn clear_next(&mut self) -> Result<Snapshot, Refusal> {
        let next = State { next: None, ..self.state.clone() };
        self.change(next)
    }

    fn no_next() -> Refusal {
        Refusal::new(409, "noNext", "No re-index is under way.")
    }

    /// `switchToNext(expected)`: next becomes active, the old active is kept as previous. When
    /// `expected` is given and next is something else by now, nothing switches.
    pub fn switch_to_next(&mut self, expected: Option<&Target>) -> Result<Snapshot, Refusal> {
        let Some(next) = self.state.next.clone() else { return Err(Self::no_next()) };
        if expected.is_some_and(|e| !same_target(Some(&next), Some(e))) {
            return Err(Refusal::new(409, "nextChanged", "The re-index target changed while the switch was being checked. Nothing was switched."));
        }
        let after = State { active: next, next: None, previous: Some(self.state.active.clone()) };
        self.change(after)
    }

    pub fn rollback(&mut self) -> Result<Snapshot, Refusal> {
        let Some(previous) = self.state.previous.clone() else {
            return Err(Refusal::new(409, "noPrevious", "There is no previous embedding model to return to."));
        };
        let after = State { active: previous, next: None, previous: Some(self.state.active.clone()) };
        self.change(after)
    }

    /// Forget the rollback target (the caller drops its vectors).
    pub fn finish(&mut self) -> Result<Snapshot, Refusal> {
        if self.state.previous.is_none() {
            return Err(Refusal::new(409, "noPrevious", "There is no previous embedding model to remove."));
        }
        let after = State { previous: None, ..self.state.clone() };
        self.change(after)
    }

    fn held_slot(&mut self, which: Which) -> &mut Option<Held> {
        match which {
            Which::Active => &mut self.held_active,
            Which::Next => &mut self.held_next,
        }
    }

    /// `refuseSeeded(reasons, { transient })`: a seeded address that is not on this network. In
    /// memory only. A refused next is dropped; a refused active turns embedding off (profile
    /// `unavailable`) and locks changes.
    pub fn refuse_seeded(&mut self, active: Option<&str>, next: Option<&str>, transient: bool) -> Snapshot {
        if self.saved || self.saved_change {
            return self.get();
        }
        for (which, reason) in [(Which::Active, active), (Which::Next, next)] {
            let Some(reason) = reason.filter(|r| !r.is_empty()) else { continue };
            let target = match which {
                Which::Active => Some(self.state.active.clone()),
                Which::Next => self.state.next.clone(),
            };
            let slot = self.held_slot(which);
            if let Some(h) = slot {
                h.reason = reason.to_owned();
                h.transient = transient;
            } else {
                *slot = Some(Held { reason: reason.to_owned(), transient, target });
                match which {
                    Which::Next => self.state.next = None,
                    Which::Active => self.state.active.profile = "unavailable".into(),
                }
            }
            if which == Which::Active {
                self.locked = Some(reason.to_owned());
                self.damaged = Some(reason.to_owned());
            }
        }
        self.version += 1;
        self.get()
    }

    /// `releaseSeeded(which)`: takes back a transient refusal once the address is on this network.
    pub fn release_seeded(&mut self, which: Which) -> Snapshot {
        if self.saved || self.saved_change {
            return self.get();
        }
        let Some(h) = self.held_slot(which).take() else { return self.get() };
        if !h.transient {
            *self.held_slot(which) = Some(h);
            return self.get();
        }
        match which {
            Which::Next => {
                if self.state.next.is_none() {
                    self.state.next = h.target;
                }
            }
            Which::Active => {
                if let Some(t) = h.target {
                    self.state.active = t;
                }
                self.locked = None;
                self.damaged = None;
            }
        }
        self.version += 1;
        self.get()
    }

    /// Whether changes are refused (`refuseSeeded` on the active target).
    pub fn is_locked(&self) -> bool {
        self.locked.is_some()
    }
}

/// The configuration's three targets as core's `config.get()` JSON (without version).
pub fn snapshot_json(s: &Snapshot) -> Value {
    json!({
        "active": s.active.to_json(),
        "next": opt_json(s.next.as_ref()),
        "previous": opt_json(s.previous.as_ref()),
        "source": s.source,
    })
}
