//! noevia#1318: native, read-only `GET /api/v1/models/updates`.
//!
//! Python (`model-manager/app/api.py` and `helpers._update_status_map`):
//!
//! ```python
//! return {"status": _update_status_map(services.snapshot_models_dir())}
//! ```
//!
//! For every model the models-folder snapshot lists (shards grouped, companions folded into their
//! folder's model, sorted by folder, companion flag and name), the stored Hugging Face check for
//! that file name becomes `{status, remote, checked_at, delta_days}`. Python stays the only writer
//! of `update_check` (`POST /api/v1/models/check-updates`, downloads).
//!
//! Exactness: the snapshot walk, shard key, companion rule, nanosecond mtime to `st_mtime` float,
//! `datetime.fromtimestamp` rounding, CPython's C `datetime.fromisoformat` (the 3.12 parser the
//! service runs, quirks included), `float.__repr__` and FastAPI's JSON rendering are ported and
//! differential-tested against the actual services app (`tools/gen-model-updates-oracle.py`).
//! Any condition where Python's handler raises answers FastAPI's exact
//! `500 text/plain; charset=utf-8` `Internal Server Error`.
//!
//! Deliberate differences (see `docs/native-model-updates.md`): the store is opened read-only and
//! never created; a store, ini or listing past the bounds below answers 500 (store/ini) or the
//! native 503 refusal (listing) instead of being read in full; the walk refuses (503) where the
//! kernel-contained walker cannot follow Python (a symlink leaving the models folder, an
//! unreadable folder, a non-UTF-8 name) instead of guessing.
use crate::sections::{access::Root, ini::Ini, Budget, Error as WalkError, INI_CAP};
use rusqlite::{types::ValueRef, Connection, OpenFlags};
use std::{
    collections::HashMap,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Body of the native refusal (503) when the bounded walk cannot answer.
pub const REFUSAL: &str = "Native model update status is unavailable";
/// Largest single stored value read; a larger one answers 500.
pub const VALUE_CAP: i32 = 64 * 1024;
/// Most `update_check` rows read; more answers 500.
pub const ROW_CAP: usize = 65_536;
/// Most model files listed; more answers the 503 refusal.
pub const MODEL_CAP: usize = 4096;
/// Python's sqlite3 waits 5 s for a lock; answer 500 (as Python does) inside the 5 s budget.
pub const BUSY_WAIT: Duration = Duration::from_millis(4000);
const DEADLINE: Duration = Duration::from_millis(4500);
const SKIP_DIRS: [&str; 6] = ["blobs", "refs", ".locks", ".cache", ".git", "__pycache__"];
const MAX_DEPTH: usize = 4;
const DAY_US: i128 = 86_400_000_000;
/// `date(1970, 1, 1).toordinal()`.
const EPOCH_ORDINAL: i64 = 719_163;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Json(Vec<u8>),
    /// Python's handler raised: FastAPI's plain 500.
    Failed,
    /// The bounded, kernel-contained walk could not answer: the native 503 refusal.
    Refused,
}

pub struct ModelUpdates {
    models: PathBuf,
    ini: PathBuf,
    database: PathBuf,
}

impl ModelUpdates {
    /// `models` and `ini` are Python's `MODELS_DIR` and `MODELS_INI_PATH`; `data` is `DATA_DIR`.
    pub fn new(models: &Path, ini: &Path, data: &Path) -> Self {
        Self {
            models: models.to_owned(),
            ini: ini.to_owned(),
            database: data.join("model_loader.db"),
        }
    }

    pub async fn response(&self) -> Outcome {
        let (models, ini, database) =
            (self.models.clone(), self.ini.clone(), self.database.clone());
        let deadline = Instant::now() + DEADLINE;
        tokio::task::spawn_blocking(move || compute(&models, &ini, &database, deadline, Root::new))
            .await
            .unwrap_or(Outcome::Failed)
    }
}

enum Stop {
    Failed,
    Refused,
}

/// The whole answer. `open` opens the models root (the kernel-contained walker in production).
pub(crate) fn compute(
    models: &Path,
    ini: &Path,
    database: &Path,
    deadline: Instant,
    open: impl FnOnce(&Path) -> Result<Root, WalkError>,
) -> Outcome {
    match answer(models, ini, database, deadline, open) {
        Ok(bytes) => Outcome::Json(bytes),
        Err(Stop::Failed) => Outcome::Failed,
        Err(Stop::Refused) => Outcome::Refused,
    }
}

fn answer(
    models: &Path,
    ini: &Path,
    database: &Path,
    deadline: Instant,
    open: impl FnOnce(&Path) -> Result<Root, WalkError>,
) -> Result<Vec<u8>, Stop> {
    // services.snapshot_models_dir(): nothing is read from a missing folder, not even models.ini.
    let entries = match exists(models)? {
        None => Vec::new(),
        Some(directory) => {
            check_ini(ini)?;
            if directory {
                snapshot(&open(models).map_err(|_| Stop::Refused)?)?
            } else {
                Vec::new() // os.walk of a file yields nothing
            }
        }
    };
    if Instant::now() >= deadline {
        return Err(Stop::Refused);
    }
    let checks = read_checks(database, deadline)?;
    render(&entries, &checks)
}

/// `Path.exists()` (Python 3.12): `Some(is_dir)`, `None` when missing, 500 when stat raises.
fn exists(path: &Path) -> Result<Option<bool>, Stop> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(meta.is_dir())),
        Err(e) if ignorable(&e) => Ok(None),
        Err(_) => Err(Stop::Failed),
    }
}

/// pathlib's `_ignore_error`: ENOENT, ENOTDIR, EBADF, ELOOP.
fn ignorable(error: &std::io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        [
            rustix::io::Errno::NOENT,
            rustix::io::Errno::NOTDIR,
            rustix::io::Errno::BADF,
            rustix::io::Errno::LOOP,
        ]
        .iter()
        .any(|e| e.raw_os_error() == code)
    })
}

/// The snapshot reads models.ini twice (aliases, then `ini.sections_by_file()`); only failures
/// matter to this route: unreadable, not UTF-8 or not parseable raises (500). Over `INI_CAP` also
/// answers 500 (deliberate bound).
fn check_ini(path: &Path) -> Result<(), Stop> {
    if exists(path)?.is_none() {
        return Ok(());
    }
    let file = std::fs::File::open(path).map_err(|_| Stop::Failed)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve(INI_CAP.min(64 * 1024))
        .map_err(|_| Stop::Failed)?;
    file.take(INI_CAP as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Stop::Failed)?;
    if bytes.len() > INI_CAP {
        return Err(Stop::Failed);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| Stop::Failed)?;
    // Path.read_text decodes universal newlines before configparser sees the text.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    Ini::parse(&text, &mut Budget::default()).map_err(|_| Stop::Failed)?;
    Ok(())
}

/// One snapshot row this route uses: `display_name` and the newest part's `st_mtime`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub name: String,
    pub mtime: f64,
}

struct Found {
    subdir: String,
    name: String,
    mtime: f64,
}

fn join(relative: &str, name: &str) -> String {
    if relative == "." {
        name.to_owned()
    } else {
        format!("{relative}/{name}")
    }
}

/// `utils.iter_gguf`: sorted files of a folder (any name ending `.gguf`, case-insensitively,
/// that `is_file()` after following links), then its sorted real subfolders, four levels deep.
fn walk(
    root: &Root,
    relative: &str,
    depth: usize,
    out: &mut Vec<Found>,
    budget: &mut Budget,
) -> Result<(), Stop> {
    let mut entries = root.entries(relative, budget).map_err(|_| Stop::Refused)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in &entries {
        if entry.directory || !entry.name.to_lowercase().ends_with(".gguf") {
            continue;
        }
        let path = join(relative, &entry.name);
        let Some(meta) = root.stat(&path, budget).map_err(|_| Stop::Refused)? else {
            continue; // dangling link: is_file() is False
        };
        if !meta.is_file() {
            continue; // a link to a folder, a FIFO, ...
        }
        if out.len() >= MODEL_CAP {
            return Err(Stop::Refused);
        }
        out.try_reserve(1).map_err(|_| Stop::Refused)?;
        out.push(Found {
            subdir: if relative == "." {
                String::new()
            } else {
                relative.to_owned()
            },
            name: entry.name.clone(),
            // CPython's st_mtime: `sec + nsec * 1e-9` in double precision.
            mtime: meta.mtime() as f64 + meta.mtime_nsec() as f64 * 1e-9,
        });
    }
    if depth < MAX_DEPTH {
        for entry in entries {
            if entry.directory
                && !entry.name.starts_with('.')
                && !SKIP_DIRS.contains(&entry.name.as_str())
            {
                walk(root, &join(relative, &entry.name), depth + 1, out, budget)?;
            }
        }
    }
    Ok(())
}

/// `utils.shard_key(name)[0]`: `-NNNNN-of-NNNNN` removed before the extension.
fn shard_base(filename: &str) -> Result<String, Stop> {
    static SHARD: std::sync::OnceLock<Result<regex::Regex, regex::Error>> =
        std::sync::OnceLock::new();
    let regex = SHARD
        .get_or_init(|| regex::Regex::new(r"-(\d{5})-of-(\d{5})(\.[^.]+)$"))
        .as_ref()
        .map_err(|_| Stop::Failed)?;
    let Some(found) = regex.captures(filename) else {
        return Ok(filename.to_owned());
    };
    let (Some(all), Some(suffix)) = (found.get(0), found.get(3)) else {
        return Err(Stop::Failed);
    };
    Ok(format!(
        "{}{}",
        filename.get(..all.start()).ok_or(Stop::Failed)?,
        suffix.as_str()
    ))
}

/// `ini._is_companion(base)` without a path (so no head-size test), as the snapshot calls it:
/// a projector, imatrix data or a draft head. Python's existing file-name rule, ported as-is for
/// parity; this route adds no name-based logic of its own.
pub(crate) fn companion(filename: &str) -> bool {
    let low = filename.to_lowercase();
    low.contains("mmproj") || imatrix(&low) || model_autoconfig::files::looks_like_draft(filename)
}

/// `re.compile("imatrix", re.I).search(low)`: under IGNORECASE `i` also matches the dotless `ı`.
fn imatrix(low: &str) -> bool {
    let chars: Vec<char> = low.chars().collect();
    let pattern: Vec<char> = "imatrix".chars().collect();
    chars.windows(pattern.len()).any(|window| {
        window
            .iter()
            .zip(&pattern)
            .all(|(c, p)| c == p || (*p == 'i' && *c == '\u{0131}'))
    })
}

/// `services.snapshot_models_dir().ggufs`, reduced to what the update map reads.
fn snapshot(root: &Root) -> Result<Vec<Entry>, Stop> {
    let mut budget = Budget::default();
    let mut found = Vec::new();
    walk(root, ".", 0, &mut found, &mut budget)?;
    struct Group {
        subdir: String,
        base: String,
        mtime: f64,
        companion: bool,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    for file in found {
        let base = shard_base(&file.name)?;
        let key = (file.subdir.clone(), base.clone());
        match index.get(&key) {
            Some(&i) => {
                let group = groups.get_mut(i).ok_or(Stop::Failed)?;
                if file.mtime > group.mtime {
                    group.mtime = file.mtime;
                }
            }
            None => {
                index.insert(key, groups.len());
                groups.push(Group {
                    companion: companion(&base),
                    subdir: file.subdir,
                    base,
                    mtime: file.mtime,
                });
            }
        }
    }
    // A companion inside a folder that holds a model is folded into that model's row.
    let with_model: std::collections::HashSet<&str> = groups
        .iter()
        .filter(|g| !g.companion && !g.subdir.is_empty())
        .map(|g| g.subdir.as_str())
        .collect();
    let mut surviving: Vec<(String, bool, String, &Group)> = groups
        .iter()
        .filter(|g| {
            !(g.companion && !g.subdir.is_empty() && with_model.contains(g.subdir.as_str()))
        })
        .map(|g| {
            (
                g.subdir.to_lowercase(),
                g.companion,
                g.base.to_lowercase(),
                g,
            )
        })
        .collect();
    // Stable, like list.sort: ties keep first-seen order.
    surviving.sort_by(|a, b| (&a.0, a.1, &a.2).cmp(&(&b.0, b.1, &b.2)));
    Ok(surviving
        .into_iter()
        .map(|(_, _, _, g)| Entry {
            name: g.base.clone(),
            mtime: g.mtime,
        })
        .collect())
}

/// A stored column value as `sqlite3` hands it to Python.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stored {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

fn stored(value: ValueRef<'_>) -> Result<Stored, Stop> {
    Ok(match value {
        ValueRef::Null => Stored::Null,
        ValueRef::Integer(i) => Stored::Int(i),
        ValueRef::Real(f) => Stored::Real(f),
        // sqlite3 decodes every TEXT value strictly; invalid UTF-8 raises.
        ValueRef::Text(bytes) => Stored::Text(
            std::str::from_utf8(bytes)
                .map_err(|_| Stop::Failed)?
                .to_owned(),
        ),
        ValueRef::Blob(bytes) => Stored::Blob(bytes.to_vec()),
    })
}

/// `db.all_update_checks()`: text file name -> (hf_last_modified, checked_at); the last row wins.
fn read_checks(
    database: &Path,
    deadline: Instant,
) -> Result<HashMap<String, (Stored, Stored)>, Stop> {
    let failed = |_| Stop::Failed;
    let db = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(failed)?;
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, VALUE_CAP)
        .map_err(failed)?;
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH, 1024)
        .map_err(failed)?;
    db.busy_timeout(BUSY_WAIT).map_err(failed)?;
    db.progress_handler(1000, Some(move || Instant::now() >= deadline))
        .map_err(failed)?;
    let mut statement = db
        .prepare("SELECT filename, repo_id, hf_last_modified, checked_at FROM update_check")
        .map_err(failed)?;
    let mut rows = statement.query([]).map_err(failed)?;
    let mut out = HashMap::new();
    let mut count = 0usize;
    while let Some(row) = rows.next().map_err(failed)? {
        count += 1;
        if count > ROW_CAP {
            return Err(Stop::Failed);
        }
        let filename = stored(row.get_ref(0).map_err(failed)?)?;
        stored(row.get_ref(1).map_err(failed)?)?; // repo_id: decoded (and so checked), unused
        let remote = stored(row.get_ref(2).map_err(failed)?)?;
        let checked = stored(row.get_ref(3).map_err(failed)?)?;
        // Only a str key can equal a display name.
        if let Stored::Text(name) = filename {
            out.insert(name, (remote, checked));
        }
    }
    Ok(out)
}

/// `_update_status_map` plus FastAPI's rendering, in Python's dict insertion order.
fn render(entries: &[Entry], checks: &HashMap<String, (Stored, Stored)>) -> Result<Vec<u8>, Stop> {
    let mut order: Vec<(&str, String)> = Vec::new();
    let mut position: HashMap<&str, usize> = HashMap::new();
    for entry in entries {
        let Some((remote, checked)) = checks.get(&entry.name) else {
            continue;
        };
        let value = status(remote, checked, entry.mtime)?;
        match position.get(entry.name.as_str()) {
            Some(&i) => order.get_mut(i).ok_or(Stop::Failed)?.1 = value,
            None => {
                position.insert(&entry.name, order.len());
                order.push((&entry.name, value));
            }
        }
    }
    let mut out = String::from("{\"status\":{");
    for (i, (name, value)) in order.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_string(name));
        out.push(':');
        out.push_str(value);
    }
    out.push_str("}}");
    Ok(out.into_bytes())
}

fn json_string(text: &str) -> String {
    // serde_json escapes exactly what json.dumps(ensure_ascii=False) escapes.
    serde_json::to_string(text).unwrap_or_default()
}

/// One `{status, remote, checked_at, delta_days}` object.
fn status(remote: &Stored, checked: &Stored, mtime: f64) -> Result<String, Stop> {
    let checked = checked_json(checked)?;
    let unknown = |remote: &str| {
        format!(
            "{{\"status\":\"unknown\",\"remote\":{},\"checked_at\":{checked},\"delta_days\":null}}",
            json_string(remote)
        )
    };
    // `row.get("hf_last_modified") or ""`, then `remote.partition(".")`.
    let text = match remote {
        Stored::Null => return Ok(unknown("")),
        Stored::Int(0) => return Ok(unknown("")),
        Stored::Real(f) if *f == 0.0 => return Ok(unknown("")),
        Stored::Blob(b) if b.is_empty() => return Ok(unknown("")),
        Stored::Text(t) if t.is_empty() => return Ok(unknown("")),
        Stored::Text(t) => t,
        // int/float have no partition(); bytes.partition(str) raises TypeError.
        Stored::Int(_) | Stored::Real(_) | Stored::Blob(_) => return Err(Stop::Failed),
    };
    let candidate = match text.split_once('.') {
        Some((core, frac)) if !frac.is_empty() => {
            let frac: String = frac.trim_end_matches('Z').chars().take(6).collect();
            format!("{core}.{frac}+00:00")
        }
        Some((core, _)) => core.replace('Z', "+00:00"),
        None => text.replace('Z', "+00:00"),
    };
    let Some(parsed) = fromisoformat(&candidate) else {
        return Ok(unknown(text));
    };
    let local = from_timestamp(mtime).ok_or(Stop::Failed)?;
    // A naive datetime minus an aware one raises TypeError.
    let offset = parsed.offset_us.ok_or(Stop::Failed)?;
    let remote_us = parsed.local_us() - i128::from(offset);
    let delta = remote_us - local;
    Ok(format!(
        "{{\"status\":\"{}\",\"remote\":\"{:04}-{:02}-{:02}\",\"checked_at\":{checked},\"delta_days\":{}}}",
        if remote_us > local { "stale" } else { "up-to-date" },
        parsed.year,
        parsed.month,
        parsed.day,
        delta.div_euclid(DAY_US)
    ))
}

/// FastAPI's JSON for `checked_at`: a non-finite float renders `null`; bytes must be UTF-8.
fn checked_json(value: &Stored) -> Result<String, Stop> {
    Ok(match value {
        Stored::Null => "null".to_owned(),
        Stored::Int(i) => i.to_string(),
        Stored::Real(f) if !f.is_finite() => "null".to_owned(),
        Stored::Real(f) => py_float_repr(*f),
        Stored::Text(t) => json_string(t),
        Stored::Blob(b) => json_string(std::str::from_utf8(b).map_err(|_| Stop::Failed)?),
    })
}

/// `float.__repr__`: shortest round-trip digits, positional for 1e-4 <= |x| < 1e16.
pub(crate) fn py_float_repr(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_owned();
    }
    let formatted = format!("{value:e}");
    let (mantissa, exponent) = formatted.split_once('e').unwrap_or((&formatted, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        if point <= 0 {
            let zeros = "0".repeat(usize::try_from(-point).unwrap_or(0));
            format!("{sign}0.{zeros}{digits}")
        } else {
            let point = usize::try_from(point).unwrap_or(0);
            if digits.len() <= point {
                format!("{sign}{digits}{}.0", "0".repeat(point - digits.len()))
            } else {
                format!("{sign}{}.{}", &digits[..point], &digits[point..])
            }
        }
    } else {
        let (first, rest) = digits.split_at(1.min(digits.len()));
        let mantissa = if rest.is_empty() {
            first.to_owned()
        } else {
            format!("{first}.{rest}")
        };
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        format!("{sign}{mantissa}e{exp_sign}{:02}", exponent.unsigned_abs())
    }
}

/// `datetime.fromtimestamp(t, tz=timezone.utc)` as microseconds since the epoch: the fraction
/// is rounded half-even to microseconds; a year outside 1..=9999 raises.
pub(crate) fn from_timestamp(seconds: f64) -> Option<i128> {
    if !seconds.is_finite() {
        return None;
    }
    let mut whole = seconds.trunc();
    let mut micros = round_half_even((seconds - whole) * 1e6);
    if micros >= 1e6 {
        micros -= 1e6;
        whole += 1.0;
    } else if micros < 0.0 {
        micros += 1e6;
        whole -= 1.0;
    }
    if !(-9.2e18..9.2e18).contains(&whole) {
        return None;
    }
    let whole = whole as i64;
    let (year, _, _) = civil(whole.div_euclid(86_400));
    if !(1..=9999).contains(&year) {
        return None;
    }
    Some(i128::from(whole) * 1_000_000 + micros as i128)
}

fn round_half_even(x: f64) -> f64 {
    let rounded = x.round();
    if (x - rounded).abs() == 0.5 {
        2.0 * (x / 2.0).round()
    } else {
        rounded
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `date(y, m, d).toordinal()` (0001-01-01 is 1), for already-valid fields.
fn ordinal(year: i64, month: i64, day: i64) -> i64 {
    let y = year - 1;
    let before_month: i64 = (1..month).map(|m| days_in_month(year, m)).sum();
    y * 365 + y / 4 - y / 100 + y / 400 + before_month + day
}

/// A parsed `datetime`; `offset_us` is `None` for a naive one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    pub microsecond: i64,
    pub offset_us: Option<i64>,
}

impl Parsed {
    /// Wall-clock fields as microseconds since 1970-01-01 (offset not applied).
    fn local_us(&self) -> i128 {
        let days = i128::from(ordinal(self.year, self.month, self.day) - EPOCH_ORDINAL);
        let seconds = days * 86_400 + i128::from(self.hour * 3600 + self.minute * 60 + self.second);
        seconds * 1_000_000 + i128::from(self.microsecond)
    }
}

/// CPython 3.12's C `datetime.fromisoformat` (`Modules/_datetimemodule.c`), byte for byte:
/// separator located by position over the UTF-8 bytes, a non-ASCII separator skipped by its
/// lead byte, digit runs read past the nominal end, the first `Z`/`+`/`-` taken as the zone,
/// a zero zone offset returning UTC, and the constructor's range checks. `None` is ValueError.
pub(crate) fn fromisoformat(text: &str) -> Option<Parsed> {
    if text.chars().count() < 7 {
        return None;
    }
    let len = text.len();
    let mut buffer = text.as_bytes().to_vec();
    buffer.push(0); // C reads the NUL terminator
    let at = |i: usize| buffer.get(i).copied().unwrap_or(0);
    let separator = separator(&at, len)?;
    let (year, month, day) = parse_date(&at, separator)?;
    let mut time = Time::default();
    if len > separator {
        let lead = at(separator);
        let width = if lead & 0x80 == 0 {
            1
        } else {
            match lead & 0xf0 {
                0xe0 => 3,
                0xf0 => 4,
                _ => 2,
            }
        };
        let start = separator + width;
        // `len -= (p - dt_ptr)`: valid UTF-8 never puts the start past the end.
        if start > len {
            return None;
        }
        time = parse_time(&at, start, len)?;
    }
    let offset_us = if time.zone {
        if time.zone_seconds == 0 {
            Some(0)
        } else {
            let total = time.zone_seconds * 1_000_000 + time.zone_micros;
            if total.abs() >= 86_400_000_000 {
                return None;
            }
            Some(total)
        }
    } else {
        None
    };
    let valid = (1..=9999).contains(&year)
        && (1..=12).contains(&month)
        && day >= 1
        && day <= days_in_month(year, month)
        && (0..=23).contains(&time.hour)
        && (0..=59).contains(&time.minute)
        && (0..=59).contains(&time.second)
        && (0..=999_999).contains(&time.micros);
    valid.then_some(Parsed {
        year,
        month,
        day,
        hour: time.hour,
        minute: time.minute,
        second: time.second,
        microsecond: time.micros,
        offset_us,
    })
}

fn digit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// `parse_digits`: exactly `count` ASCII digits starting at `p`.
fn digits(at: &impl Fn(usize) -> u8, p: usize, count: usize) -> Option<i64> {
    let mut value = 0i64;
    for i in 0..count {
        let c = at(p + i);
        if !digit(c) {
            return None;
        }
        value = value * 10 + i64::from(c - b'0');
    }
    Some(value)
}

/// `_find_isoformat_datetime_separator`.
fn separator(at: &impl Fn(usize) -> u8, len: usize) -> Option<usize> {
    if len == 7 {
        return Some(7);
    }
    if at(4) == b'-' {
        if at(5) == b'W' {
            if len > 8 && at(8) == b'-' {
                if len == 9 {
                    return None;
                }
                if len > 10 && digit(at(10)) {
                    return Some(8);
                }
                return Some(10);
            }
            return Some(8);
        }
        return Some(10);
    }
    if at(4) == b'W' {
        let mut idx = 7;
        while idx < len && digit(at(idx)) {
            idx += 1;
        }
        if idx < 9 {
            return Some(idx);
        }
        return Some(if idx % 2 == 0 { 7 } else { 8 });
    }
    Some(8)
}

/// `parse_isoformat_date`, with the ISO week form resolved by `iso_to_ymd`.
fn parse_date(at: &impl Fn(usize) -> u8, len: usize) -> Option<(i64, i64, i64)> {
    let year = digits(at, 0, 4)?;
    let mut p = 4;
    let dashed = at(p) == b'-';
    if dashed {
        p += 1;
    }
    if at(p) == b'W' {
        p += 1;
        let week = digits(at, p, 2)?;
        p += 2;
        let weekday = if p < len {
            if dashed {
                if at(p) != b'-' {
                    return None;
                }
                p += 1;
            }
            digits(at, p, 1)?
        } else {
            1
        };
        return iso_to_ymd(year, week, weekday);
    }
    let month = digits(at, p, 2)?;
    p += 2;
    if dashed {
        if at(p) != b'-' {
            return None;
        }
        p += 1;
    }
    let day = digits(at, p, 2)?;
    Some((year, month, day))
}

fn iso_to_ymd(year: i64, week: i64, day: i64) -> Option<(i64, i64, i64)> {
    if !(1..=9999).contains(&year) {
        return None;
    }
    let first = ordinal(year, 1, 1);
    let first_weekday = (first + 6) % 7; // 0 is Monday
    if !(1..53).contains(&week) {
        let long_year = week == 53 && (first_weekday == 3 || (first_weekday == 2 && leap(year)));
        if !long_year {
            return None;
        }
    }
    if !(1..8).contains(&day) {
        return None;
    }
    let mut monday = first - first_weekday;
    if first_weekday > 3 {
        monday += 7;
    }
    let target = monday + (week - 1) * 7 + day - 1;
    Some(civil(target - EPOCH_ORDINAL))
}

#[derive(Default)]
struct Time {
    hour: i64,
    minute: i64,
    second: i64,
    micros: i64,
    zone: bool,
    zone_seconds: i64,
    zone_micros: i64,
}

/// `parse_hh_mm_ss_ff`: `(more_after, [h, m, s], us)`; `None` is a negative return code.
fn clock(at: &impl Fn(usize) -> u8, start: usize, end: usize) -> Option<(bool, [i64; 3], i64)> {
    let mut values = [0i64; 3];
    let mut p = start;
    let mut separated = true;
    for i in 0..3 {
        let value = digits(at, p, 2)?;
        if let Some(slot) = values.get_mut(i) {
            *slot = value;
        }
        p += 2;
        let c = at(p);
        p += 1;
        if i == 0 {
            separated = c == b':';
        }
        if p >= end {
            return Some((c != 0, values, 0));
        }
        if separated && c == b':' {
            continue;
        }
        if c == b'.' || c == b',' {
            break;
        }
        if separated {
            return None; // malformed time separator
        }
        p -= 1;
    }
    // Fraction: up to six digits, scaled to microseconds; further digits are skipped.
    let count = end.saturating_sub(p).min(6);
    if count == 0 {
        return None;
    }
    let mut micros = digits(at, p, count)?;
    p += count;
    for _ in count..6 {
        micros *= 10;
    }
    while digit(at(p)) {
        p += 1;
    }
    Some((at(p) != 0, values, micros))
}

/// `parse_isoformat_time` over `[start, end)`.
fn parse_time(at: &impl Fn(usize) -> u8, start: usize, end: usize) -> Option<Time> {
    // do { ... } while (++pos < end): the first byte is always inspected.
    let mut zone = start;
    loop {
        if matches!(at(zone), b'Z' | b'+' | b'-') {
            break;
        }
        zone += 1;
        if zone >= end {
            break;
        }
    }
    let (more, [hour, minute, second], micros) = clock(at, start, zone)?;
    let mut time = Time {
        hour,
        minute,
        second,
        micros,
        ..Time::default()
    };
    if zone == end {
        // No zone: anything left over is an error.
        return (!more).then_some(time);
    }
    if at(zone) == b'Z' {
        if at(zone + 1) != 0 {
            return None;
        }
        time.zone = true;
        return Some(time);
    }
    let sign = if at(zone) == b'-' { -1 } else { 1 };
    let (more, [h, m, s], us) = clock(at, zone + 1, end)?;
    if more {
        return None;
    }
    time.zone = true;
    time.zone_seconds = sign * (h * 3600 + m * 60 + s);
    time.zone_micros = sign * us;
    Some(time)
}

#[cfg(test)]
mod tests;
