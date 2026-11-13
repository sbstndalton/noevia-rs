//! `EmbeddingConfig`: the active / next / previous targets, saved to `embedding-config.json`
//! once changed at runtime, plus the seeded-address refusals held in memory.
//!
//! The Python class guards itself with a lock; here the mutating methods take `&mut self` and a
//! caller that shares the value wraps it in a `Mutex` (see the module docs).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

use super::pystr::{path_join, path_normalize, path_parent, str_or_empty, truthy};
use super::target::{cfg_get, normalize_target, normalize_target_from, seed_from_cfg};
use super::EmbeddingConfigError;
use crate::pyjson::dumps_str;
use crate::Target;

/// The three targets, always in the order active, next, previous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub active: Target,
    pub next: Option<Target>,
    pub previous: Option<Target>,
}

/// Where the targets came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Saved,
    Environment,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Saved => "saved",
            Self::Environment => "environment",
        }
    }
}

/// `"active"` or `"next"`: the two seeded addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Active,
    Next,
}

impl Which {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Next => "next",
        }
    }
}

/// `EmbeddingConfig.get()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub active: Target,
    pub next: Option<Target>,
    pub previous: Option<Target>,
    pub source: Source,
    pub version: u64,
    /// Only a non-empty message counts as damage, like Python's truthiness test.
    pub damaged: Option<String>,
}

impl Snapshot {
    /// The dictionary Python returns (`damaged` only when set).
    pub fn to_json(&self) -> Value {
        let opt = |t: &Option<Target>| t.as_ref().map_or(Value::Null, Target::to_json);
        let mut out = json!({
            "active": self.active.to_json(),
            "next": opt(&self.next),
            "previous": opt(&self.previous),
            "source": self.source.as_str(),
            "version": self.version,
        });
        if let (Some(damaged), Some(map)) = (&self.damaged, out.as_object_mut()) {
            map.insert("damaged".to_owned(), Value::String(damaged.clone()));
        }
        out
    }
}

/// The refusals `refuse_seeded` takes (a Python dict with the keys `active` and `next`). An empty
/// reason counts as none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeededReasons {
    pub active: Option<String>,
    pub next: Option<String>,
}

impl SeededReasons {
    pub fn only(which: Which, reason: impl Into<String>) -> Self {
        let reason = Some(reason.into());
        match which {
            Which::Active => Self {
                active: reason,
                next: None,
            },
            Which::Next => Self {
                active: None,
                next: reason,
            },
        }
    }
}

/// What a change can fail with: a refusal (code and status) or a failed write.
#[derive(Debug)]
pub enum ChangeError {
    Config(EmbeddingConfigError),
    /// Python's `OSError` from `_persist`; nothing was changed in memory.
    Io(std::io::Error),
}

impl std::fmt::Display for ChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => e.fmt(f),
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ChangeError {}

impl From<EmbeddingConfigError> for ChangeError {
    fn from(e: EmbeddingConfigError) -> Self {
        Self::Config(e)
    }
}

#[derive(Debug, Clone)]
struct Held {
    reason: String,
    transient: bool,
    /// `self._state[which]` when it was refused (a next that was never set holds none).
    target: Option<Target>,
}

#[derive(Debug)]
pub struct EmbeddingConfig {
    state_file: Option<PathBuf>,
    version: u64,
    damaged: Option<String>,
    held_active: Option<Held>,
    held_next: Option<Held>,
    source: Source,
    state: State,
}

/// Python `OSError.__str__` for the errors a read can raise: `[Errno N] <strerror>: '<path>'`.
fn os_error_text(err: &std::io::Error, path: &Path) -> String {
    let shown = err.to_string();
    match err.raw_os_error() {
        Some(code) => {
            let suffix = format!(" (os error {code})");
            let strerror = shown.strip_suffix(&suffix).unwrap_or(&shown);
            let p = path.to_string_lossy();
            format!("[Errno {code}] {strerror}: {}", super::pystr::str_repr(&p))
        }
        None => shown,
    }
}

/// `type(value).__name__` for the decoded JSON types `json.loads` produces.
fn python_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `data.get("version") != 1` is false: `1`, `1.0` and `true` all equal 1 in Python.
fn is_version_one(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64() == Some(1) || n.as_f64() == Some(1.0),
        _ => false,
    }
}

/// `Path.exists()`: errors of the kinds Python ignores are "no", others are raised.
fn path_exists(path: &Path) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(e) => match e.raw_os_error() {
            // ENOENT, EBADF, ENOTDIR, ELOOP; an embedded NUL is a ValueError that also reads as "no".
            Some(2 | 9 | 20 | 40) => Ok(false),
            _ if e.kind() == std::io::ErrorKind::InvalidInput => Ok(false),
            _ => Err(e),
        },
    }
}

impl EmbeddingConfig {
    /// `EmbeddingConfig(cfg, state_file)`. A saved file wins over the environment; a damaged one
    /// fails closed (see [`EmbeddingConfig::get`]). An error is returned only where Python's
    /// `Path.exists()` raises (for example a permission error on the directory).
    pub fn new(cfg: &Value, state_file: Option<&Path>) -> std::io::Result<Self> {
        let mut config = Self {
            state_file: state_file.map(Path::to_path_buf),
            version: 0,
            damaged: None,
            held_active: None,
            held_next: None,
            source: Source::Environment,
            state: State {
                active: Target::new(None, "unavailable", "unavailable"),
                next: None,
                previous: None,
            },
        };
        let saved = config.read()?;
        config.source = if saved.is_some() {
            Source::Saved
        } else {
            Source::Environment
        };
        config.state = saved.unwrap_or_else(|| seed_from_cfg(cfg));
        Ok(config)
    }

    /// `_read`: `None` when there is no file; any damage gives the unavailable target and the
    /// damaged message (nothing is ever saved over the file).
    fn read(&mut self) -> std::io::Result<Option<State>> {
        let Some(path) = self.state_file.clone() else {
            return Ok(None);
        };
        if !path_exists(&path)? {
            return Ok(None);
        }
        match Self::parse_file(&path) {
            Ok(state) => Ok(Some(state)),
            Err(exc) => {
                self.damaged = Some(format!(
                    "The saved embedding configuration {} cannot be read ({exc}). Fix or delete it.",
                    path.to_string_lossy()
                ));
                Ok(Some(State {
                    active: Target::new(None, "unavailable", "unavailable"),
                    next: None,
                    previous: None,
                }))
            }
        }
    }

    /// The exception text of anything that makes the file unreadable.
    fn parse_file(path: &Path) -> Result<State, String> {
        let bytes = std::fs::read(path).map_err(|e| os_error_text(&e, path))?;
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            format!(
                "'utf-8' codec can't decode byte 0x{:02x} in position {}: invalid data",
                bytes.get(e.valid_up_to()).copied().unwrap_or(0),
                e.valid_up_to()
            )
        })?;
        let data: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let Value::Object(map) = &data else {
            return Err(format!(
                "'{}' object has no attribute 'get'",
                python_type_name(&data)
            ));
        };
        if !is_version_one(map.get("version")) {
            return Err("unknown version".to_owned());
        }
        let target = |key: &str| -> Result<Option<Target>, String> {
            match map.get(key) {
                Some(v) if truthy(v) => normalize_target(v).map(Some).map_err(|e| e.message),
                _ => Ok(None),
            }
        };
        let active =
            normalize_target(map.get("active").unwrap_or(&Value::Null)).map_err(|e| e.message)?;
        Ok(State {
            active,
            next: target("next")?,
            previous: target("previous")?,
        })
    }

    fn held_slot(&mut self, which: Which) -> &mut Option<Held> {
        match which {
            Which::Active => &mut self.held_active,
            Which::Next => &mut self.held_next,
        }
    }

    /// An address the environment seeded is not on this network. A refused next is dropped; a
    /// refused active turns embedding off (unknown profile: every embed call refuses) and locks
    /// changes like a damaged file, because falling back to the chat engine would send it the
    /// same text. `transient` (the name did not resolve yet) can be undone by `release_seeded`.
    /// Does nothing once the state is saved.
    pub fn refuse_seeded(&mut self, reasons: &SeededReasons, transient: bool) {
        if self.source != Source::Environment {
            return;
        }
        for (which, reason) in [
            (Which::Active, &reasons.active),
            (Which::Next, &reasons.next),
        ] {
            let Some(reason) = reason.as_ref().filter(|r| !r.is_empty()) else {
                continue;
            };
            let target = match which {
                Which::Active => Some(self.state.active.clone()),
                Which::Next => self.state.next.clone(),
            };
            let unavailable = Target::new(
                self.state.active.base_url.as_deref(),
                &self.state.active.model,
                "unavailable",
            );
            let slot = self.held_slot(which);
            if let Some(held) = slot.as_mut() {
                held.reason.clone_from(reason);
                held.transient = transient;
            } else {
                *slot = Some(Held {
                    reason: reason.clone(),
                    transient,
                    target,
                });
                match which {
                    Which::Next => self.state.next = None,
                    Which::Active => self.state.active = unavailable,
                }
            }
            if which == Which::Active {
                self.damaged = Some(reason.clone());
            }
        }
        self.version += 1;
    }

    /// Takes back a transient refusal once the address resolves to this network.
    pub fn release_seeded(&mut self, which: Which) {
        if self.source != Source::Environment {
            return;
        }
        let Some(held) = self.held_slot(which).take_if(|h| h.transient) else {
            return;
        };
        match which {
            Which::Next => {
                // not over an administrator's own choice
                if self.state.next.is_none() {
                    self.state.next = held.target;
                }
            }
            Which::Active => {
                if let Some(target) = held.target {
                    self.state.active = target;
                }
                self.damaged = None;
            }
        }
        self.version += 1;
    }

    /// `_persist`: `{version: 1, active, next, previous}` as `json.dump(indent=2)` writes it,
    /// through a new `<name>.<pid>.<n>.tmp` (`create_new`, mode 0600: never an existing file or a
    /// symlink; known difference 12) and a rename. A failed write or rename removes the temp file.
    fn persist(&self, state: &State) -> std::io::Result<()> {
        let Some(path) = &self.state_file else {
            return Ok(());
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let (mut file, tmp) = create_temp(path, &name)?;
        let written = file.write_all(state_json(state).as_bytes());
        drop(file);
        let result = written.and_then(|()| std::fs::rename(&tmp, path));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    fn change(&mut self, state: State) -> Result<Snapshot, ChangeError> {
        if let Some(damaged) = self.damaged.as_ref().filter(|d| !d.is_empty()) {
            return Err(EmbeddingConfigError::new(damaged.clone(), "badState", 409).into());
        }
        self.persist(&state).map_err(ChangeError::Io)?;
        self.state = state;
        self.version += 1;
        self.source = Source::Saved;
        Ok(self.get())
    }

    pub fn get(&self) -> Snapshot {
        Snapshot {
            active: self.state.active.clone(),
            next: self.state.next.clone(),
            previous: self.state.previous.clone(),
            source: self.source,
            version: self.version,
            damaged: self.damaged.clone().filter(|d| !d.is_empty()),
        }
    }

    pub fn source(&self) -> Source {
        self.source
    }

    pub fn active(&self) -> Target {
        self.state.active.clone()
    }

    pub fn next(&self) -> Option<Target> {
        self.state.next.clone()
    }

    pub fn previous(&self) -> Option<Target> {
        self.state.previous.clone()
    }

    /// `set_next(raw)` for a decoded JSON target.
    pub fn set_next(&mut self, raw: &Value) -> Result<Snapshot, ChangeError> {
        let target = normalize_target(raw)?;
        self.set_next_normalized(target)
    }

    /// `set_next(target)` for a `Target`, which Python normalises through its JSON form.
    pub fn set_next_target(&mut self, target: &Target) -> Result<Snapshot, ChangeError> {
        let target = normalize_target_from(target)?;
        self.set_next_normalized(target)
    }

    fn set_next_normalized(&mut self, target: Target) -> Result<Snapshot, ChangeError> {
        if target == self.state.active {
            return Err(EmbeddingConfigError::new(
                "That model, profile and address are already active.",
                "alreadyActive",
                409,
            )
            .into());
        }
        let mut state = self.state.clone();
        state.next = Some(target);
        self.change(state)
    }

    pub fn clear_next(&mut self) -> Result<Snapshot, ChangeError> {
        let mut state = self.state.clone();
        state.next = None;
        self.change(state)
    }

    /// next becomes active; the old active is kept as previous. `expected` is what the caller
    /// checked readiness for: if next is something else by now, nothing switches.
    pub fn switch_to_next(&mut self, expected: Option<&Target>) -> Result<Snapshot, ChangeError> {
        let Some(next) = self.state.next.clone() else {
            return Err(
                EmbeddingConfigError::new("No re-index is under way.", "noNext", 409).into(),
            );
        };
        if expected.is_some_and(|e| *e != next) {
            return Err(EmbeddingConfigError::new(
                "The re-index target changed while the switch was being checked. Nothing was switched.",
                "nextChanged",
                409,
            )
            .into());
        }
        self.change(State {
            active: next,
            next: None,
            previous: Some(self.state.active.clone()),
        })
    }

    pub fn rollback(&mut self) -> Result<Snapshot, ChangeError> {
        let Some(previous) = self.state.previous.clone() else {
            return Err(EmbeddingConfigError::new(
                "There is no previous embedding model to return to.",
                "noPrevious",
                409,
            )
            .into());
        };
        self.change(State {
            active: previous,
            next: None,
            previous: Some(self.state.active.clone()),
        })
    }

    pub fn finish(&mut self) -> Result<Snapshot, ChangeError> {
        if self.state.previous.is_none() {
            return Err(EmbeddingConfigError::new(
                "There is no previous embedding model to remove.",
                "noPrevious",
                409,
            )
            .into());
        }
        let mut state = self.state.clone();
        state.previous = None;
        self.change(state)
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A new, private temp file beside `path`. A name that already exists is skipped.
fn create_temp(path: &Path, name: &str) -> std::io::Result<(std::fs::File, PathBuf)> {
    let mut last = None;
    for _ in 0..8 {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_file_name(format!("{name}.{}.{n}.tmp", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&tmp) {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::from(std::io::ErrorKind::AlreadyExists)))
}

/// `json.dump({"version": 1, "active": ..., "next": ..., "previous": ...}, indent=2)`.
fn state_json(state: &State) -> String {
    fn target(t: &Target, indent: &str) -> String {
        let base = t
            .base_url
            .as_deref()
            .map_or_else(|| "null".to_owned(), dumps_str);
        format!(
            "{{\n{indent}  \"base_url\": {base},\n{indent}  \"model\": {},\n{indent}  \"profile\": {}\n{indent}}}",
            dumps_str(&t.model),
            dumps_str(&t.profile),
        )
    }
    let opt = |t: &Option<Target>| {
        t.as_ref()
            .map_or_else(|| "null".to_owned(), |t| target(t, "  "))
    };
    format!(
        "{{\n  \"version\": 1,\n  \"active\": {},\n  \"next\": {},\n  \"previous\": {}\n}}",
        target(&state.active, "  "),
        opt(&state.next),
        opt(&state.previous),
    )
}

/// `state_path_for(cfg)`: `retrieval.embedding_state_path` when set, else `embedding-config.json`
/// beside `retrieval.db_path` (or in `.` for no path or `:memory:`), written the way `pathlib`
/// would normalise it. Python raises `TypeError` for a truthy non-string explicit path; that is
/// a [`crate::Error::Value`] here.
pub fn state_path_for(cfg: &Value) -> crate::Result<PathBuf> {
    if let Some(explicit) = cfg_get(cfg, "retrieval.embedding_state_path").filter(|v| truthy(v)) {
        return match explicit {
            Value::String(s) => Ok(PathBuf::from(path_normalize(s))),
            _ => Err(crate::Error::Value(
                "retrieval.embedding_state_path is not a path",
            )),
        };
    }
    let db = str_or_empty(cfg_get(cfg, "retrieval.db_path"));
    let base = if !db.is_empty() && db != ":memory:" {
        path_parent(&db)
    } else {
        ".".to_owned()
    };
    Ok(PathBuf::from(path_join(&base, "embedding-config.json")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_file_layout_is_json_dump_indent_two() {
        let state = State {
            active: Target::new(None, "qa-embed-a", "nomic-v1"),
            next: Some(Target::new(
                Some("http://qa-embed.invalid:8080"),
                "qa-èmbed",
                "embeddinggemma-2",
            )),
            previous: None,
        };
        let want = "{\n  \"version\": 1,\n  \"active\": {\n    \"base_url\": null,\n    \"model\": \"qa-embed-a\",\n    \"profile\": \"nomic-v1\"\n  },\n  \"next\": {\n    \"base_url\": \"http://qa-embed.invalid:8080\",\n    \"model\": \"qa-\\u00e8mbed\",\n    \"profile\": \"embeddinggemma-2\"\n  },\n  \"previous\": null\n}";
        assert_eq!(state_json(&state), want);
    }
}
