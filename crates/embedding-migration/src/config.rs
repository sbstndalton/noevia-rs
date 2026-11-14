//! `embedding-config.cjs`: the one embedding configuration of the process. `active` is what
//! search and indexing use, `next` what a re-index builds towards, `previous` the rollback target
//! until `finish`. The environment only seeds it; once changed it is saved to
//! `embedding-config.json` (`{ version: 1, active, next, previous }`, two-space JSON), and that
//! file wins on the next start.

use crate::target::{json_of, normalize_target, same_target, seed_from_env, Target};
use crate::Failure;
use js_json::{wtf16, JValue};
use std::io::Write;
use std::path::{Path, PathBuf};

/// What this port writes in place of V8's `JSON.parse` message for a file that is not JSON.
pub const NOT_JSON: &str = "not valid JSON";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub active: Target,
    pub next: Option<Target>,
    pub previous: Option<Target>,
}

impl State {
    /// The saved file's bytes: `JSON.stringify({ version: 1, ...state }, null, 2)`.
    pub fn file_text(&self) -> String {
        let value = JValue::obj([
            ("version", JValue::Num(1.0)),
            ("active", self.active.to_json()),
            ("next", json_of(self.next.as_ref())),
            ("previous", json_of(self.previous.as_ref())),
        ]);
        wtf16::stringify_pretty(&value).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Saved,
    Environment,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Saved => "saved",
            Source::Environment => "environment",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    state: State,
    /// Where changes are saved; `None` for the in-memory stand-in of a damaged file.
    file: Option<PathBuf>,
    saved: bool,
    /// `refuseSeeded` of the active target: every change is refused with this reason.
    locked: Option<String>,
    damaged: Option<String>,
    /// The stand-in of a damaged file (`loadEmbeddingConfig`): its `setNext` always refuses.
    off: bool,
    /// `held`: the seeded values refused at start-up, with what they were.
    held_active: Option<Held>,
    held_next: Option<Held>,
    pub(crate) in_hold: bool,
}

/// Which seeded value a start-up refusal holds back (`refuseSeeded` / `releaseSeeded`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seeded {
    Active,
    Next,
}

/// `held[which]`: `{ reason, transient, target }` (the target as it was before the refusal).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    reason: String,
    transient: bool,
    target: Option<Target>,
}

fn bad_state(message: &str) -> Failure {
    Failure::new(409, "badState", message)
}

/// `readState`: `Ok(None)` when the file cannot be read (missing), `Err` with the damaged-file
/// message when it can but does not hold a valid version 1 configuration.
pub fn read_state(file: &Path) -> Result<Option<State>, String> {
    let Ok(bytes) = std::fs::read(file) else {
        return Ok(None);
    };
    parsed_state(file, &bytes)
}

fn damaged_message(file: &Path, why: &str) -> String {
    format!(
        "The saved embedding configuration {} cannot be read ({why}). Fix or delete it.",
        file.display()
    )
}

fn parsed_state(file: &Path, bytes: &[u8]) -> Result<Option<State>, String> {
    let parsed = parse_state(&String::from_utf8_lossy(bytes));
    parsed.map(Some).map_err(|why| damaged_message(file, &why))
}

/// [`read_state`] that only takes a missing file as none: a file that exists but cannot be read
/// (a directory in its place, no permission, an I/O error) is damaged, as the native worker's
/// `load_config` holds it, so a change never writes the environment's seed over saved state.
/// The reason is the I/O error's kind, never the file's content.
pub fn read_state_strict(file: &Path) -> Result<Option<State>, String> {
    match std::fs::read(file) {
        Ok(bytes) => parsed_state(file, &bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(damaged_message(file, &e.kind().to_string())),
    }
}

fn parse_state(raw: &str) -> Result<State, String> {
    let s = wtf16::parse(raw).map_err(|_| NOT_JSON.to_string())?;
    if s.get("version") != &JValue::Num(1.0) {
        return Err("unknown version".into());
    }
    let target = |v: &JValue| normalize_target(v).map_err(|f| f.message);
    let optional = |v: &JValue| v.truthy().then(|| target(v)).transpose();
    Ok(State {
        active: target(s.get("active"))?,
        next: optional(s.get("next"))?,
        previous: optional(s.get("previous"))?,
    })
}

/// Writes `bytes` to `file` as Node's `persist()` does: the directory made, a `<file>.<pid>.tmp`
/// written with mode 0600 (and synced), then renamed over `file`.
pub fn write_atomic(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
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
    let written = options.open(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    let renamed = written.and_then(|()| std::fs::rename(&tmp, file));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    renamed
}

impl Config {
    /// `loadEmbeddingConfig({ env, stateFile })`: the saved file, else the environment's targets.
    /// A damaged file gives the stand-in (embedding off: profile `unavailable`, nothing saved,
    /// set-next refused) with [`Config::damaged`] set. Also returns the lines Node logs.
    pub fn load(file: PathBuf, env: &dyn Fn(&str) -> Option<String>) -> (Config, Vec<String>) {
        let read = read_state(&file);
        Config::load_read(file, read, env)
    }

    /// [`Config::load`] over [`read_state_strict`]: a file that exists but cannot be read is
    /// damaged, not missing. Its stand-in also refuses every change that writes (409 `badState`
    /// with the damaged-file message), where Node's in-memory stand-in takes them: a host that
    /// reads the file on every request must not answer a change it cannot keep.
    pub fn load_strict(
        file: PathBuf,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> (Config, Vec<String>) {
        let read = read_state_strict(&file);
        let (mut config, warnings) = Config::load_read(file, read, env);
        if config.off {
            config.locked = config.damaged.clone();
        }
        (config, warnings)
    }

    fn load_read(
        file: PathBuf,
        read: Result<Option<State>, String>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> (Config, Vec<String>) {
        match read {
            Ok(Some(state)) => (Config::new(state, Some(file), true), Vec::new()),
            Ok(None) => {
                let (state, warnings) = seed_from_env(env);
                (Config::new(state, Some(file), false), warnings)
            }
            Err(message) => {
                let unavailable = |key: &str| {
                    matches!(key, "EMBEDDING_MODEL" | "EMBEDDING_PROFILE")
                        .then(|| "unavailable".to_string())
                };
                let (state, _) = seed_from_env(&unavailable);
                let warning = format!("[embedding] {message} Embeddings are off until then.");
                let mut config = Config::new(state, None, false);
                config.off = true;
                config.damaged = Some(message);
                (config, vec![warning])
            }
        }
    }

    pub fn new(state: State, file: Option<PathBuf>, saved: bool) -> Config {
        Config {
            state,
            file,
            saved,
            locked: None,
            damaged: None,
            off: false,
            held_active: None,
            held_next: None,
            in_hold: false,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
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
    pub fn source(&self) -> Source {
        if self.saved {
            Source::Saved
        } else {
            Source::Environment
        }
    }
    /// Why embedding is off (a damaged file, or a seeded active address refused at start-up).
    pub fn damaged(&self) -> Option<&str> {
        self.damaged.as_deref()
    }
    /// Whether changes take the config lock (`withLock`): only when they are saved.
    pub fn holds_lock(&self) -> bool {
        self.file.is_some()
    }

    fn change(&mut self, next: State) -> Result<(), Failure> {
        if self.file.is_some() && !self.in_hold {
            return Err(Failure::plain(
                "Embedding configuration requires the publication lock",
            ));
        }
        if let Some(reason) = &self.locked {
            return Err(bad_state(reason));
        }
        if let Some(file) = &self.file {
            write_atomic(file, next.file_text().as_bytes())
                .map_err(|e| Failure::plain(e.to_string()))?;
        }
        self.state = next;
        self.saved = true;
        Ok(())
    }

    pub fn set_next(&mut self, target: Target) -> Result<(), Failure> {
        if let (true, Some(message)) = (self.off, &self.damaged) {
            return Err(bad_state(message));
        }
        if same_target(Some(&target), Some(&self.state.active)) {
            return Err(Failure::new(
                409,
                "alreadyActive",
                "That model, profile and address are already active.",
            ));
        }
        let next = State {
            next: Some(target),
            ..self.state.clone()
        };
        self.change(next)
    }

    pub fn clear_next(&mut self) -> Result<(), Failure> {
        let next = State {
            next: None,
            ..self.state.clone()
        };
        self.change(next)
    }

    /// `next` becomes active, the old active `previous`; refused when `expected` (what the caller
    /// checked) is no longer the next target.
    pub fn switch_to_next(&mut self, expected: Option<&Target>) -> Result<(), Failure> {
        let Some(next) = self.state.next.clone() else {
            return Err(Failure::new(409, "noNext", "No re-index is under way."));
        };
        if expected.is_some() && !same_target(Some(&next), expected) {
            return Err(Failure::new(409, "nextChanged", "The re-index target changed while the switch was being checked. Nothing was switched."));
        }
        let previous = Some(self.state.active.clone());
        self.change(State {
            active: next,
            next: None,
            previous,
        })
    }

    pub fn rollback(&mut self) -> Result<(), Failure> {
        let Some(previous) = self.state.previous.clone() else {
            return Err(no_previous("return to"));
        };
        let old = Some(self.state.active.clone());
        self.change(State {
            active: previous,
            next: None,
            previous: old,
        })
    }

    pub fn finish(&mut self) -> Result<(), Failure> {
        if self.state.previous.is_none() {
            return Err(no_previous("remove"));
        }
        let next = State {
            previous: None,
            ..self.state.clone()
        };
        self.change(next)
    }

    /// `refuseSeeded({ active?, next? })` (the start-up address guard) of a refusal that stays until
    /// the process restarts: [`Config::refuse_seeded_as`] with `transient` false.
    pub fn refuse_seeded(&mut self, active: Option<&str>, next: Option<&str>) {
        self.refuse_seeded_as(active, next, false);
    }

    /// `refuseSeeded({ active?, next? }, { transient })`, in memory only and only while nothing was
    /// saved: a refused next is held back (dropped); a refused active turns embedding off (profile
    /// `unavailable`) and locks every change with its reason. Each value is held with what it was;
    /// refusing it again only replaces the reason and `transient`. A `transient` refusal (the name
    /// did not resolve yet) can be taken back by [`Config::release_seeded`].
    pub fn refuse_seeded_as(&mut self, active: Option<&str>, next: Option<&str>, transient: bool) {
        if self.saved || self.off {
            return;
        }
        if let Some(reason) = next.filter(|r| !r.is_empty()) {
            match &mut self.held_next {
                Some(h) => {
                    h.reason = reason.to_string();
                    h.transient = transient;
                }
                None => {
                    self.held_next = Some(Held {
                        reason: reason.to_string(),
                        transient,
                        target: self.state.next.take(),
                    });
                }
            }
        }
        if let Some(reason) = active.filter(|r| !r.is_empty()) {
            match &mut self.held_active {
                Some(h) => {
                    h.reason = reason.to_string();
                    h.transient = transient;
                }
                None => {
                    self.held_active = Some(Held {
                        reason: reason.to_string(),
                        transient,
                        target: Some(self.state.active.clone()),
                    });
                    self.state.active.profile = "unavailable".into();
                }
            }
            self.locked = Some(reason.to_string());
            self.damaged = Some(reason.to_string());
        }
    }

    /// `releaseSeeded(which)`: takes back a transient refusal once the address resolves to this
    /// network. Nothing happens when that value is not held, its refusal was not transient, or a
    /// change was saved since (or the file was). A released next never replaces a next that is set
    /// by then; a released active is used again and unlocks changes.
    pub fn release_seeded(&mut self, which: Seeded) {
        let slot = match which {
            Seeded::Active => &mut self.held_active,
            Seeded::Next => &mut self.held_next,
        };
        if !slot.as_ref().is_some_and(|h| h.transient) || self.saved {
            return;
        }
        let Some(held) = slot.take() else {
            return;
        };
        match which {
            Seeded::Next => {
                if self.state.next.is_none() {
                    self.state.next = held.target;
                }
            }
            Seeded::Active => {
                if let Some(target) = held.target {
                    self.state.active = target;
                }
                self.locked = None;
                self.damaged = None;
            }
        }
    }
}

pub(crate) fn no_previous(what: &str) -> Failure {
    Failure::new(
        409,
        "noPrevious",
        format!("There is no previous embedding model to {what}."),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qa-embedding-config-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn strict_reading_takes_only_a_missing_file_as_none() {
        let dir = scratch("strict");
        let file = dir.join("embedding-config.json");
        assert_eq!(read_state_strict(&file), Ok(None));
        // A directory where the file belongs exists but cannot be read (also as root).
        std::fs::create_dir(&file).unwrap();
        assert_eq!(read_state(&file), Ok(None), "Node's readState: missing");
        let Err(why) = read_state_strict(&file) else {
            panic!("a file that cannot be read is damaged");
        };
        assert!(why.contains("cannot be read (") && why.ends_with("Fix or delete it."));
        let none = |_: &str| None;
        let (config, warnings) = Config::load_strict(file.clone(), &none);
        assert_eq!(config.damaged(), Some(why.as_str()));
        assert_eq!(config.source(), Source::Environment);
        assert_eq!(warnings.len(), 1);
        let mut config = config;
        let refused = config.clear_next().err().map(|f| (f.status, f.code, f.message));
        assert_eq!(refused, Some((Some(409), Some("badState"), why.clone())));
        let (lenient, _) = Config::load(file.clone(), &none);
        assert_eq!(lenient.damaged(), None);
        // A damaged text reads the same either way.
        std::fs::remove_dir(&file).unwrap();
        std::fs::write(&file, "{qa damaged").unwrap();
        assert_eq!(read_state_strict(&file), read_state(&file));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
