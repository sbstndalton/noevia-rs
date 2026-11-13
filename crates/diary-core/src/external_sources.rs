//! External diary-like source folders: detection and manual import support.
//!
//! Ported from `diary/agent/external_sources.py` (noevia-services, sha256 e1b8e730...a1d7) and
//! checked against it by `tests/external_sources_differential.rs` with fixtures recorded by
//! `tests/generate_external_sources_fixtures.py` (CPython 3.12.15, Unicode 15.0.0). Only the module's
//! functions are ported, not the `agent/app.py` routes. The module reads the named folders and never
//! writes, moves, renames or deletes anything in them. Unix only (paths are handled as bytes).
//!
//! # Python behaviour that is reproduced on purpose
//!
//! - `Path(p)` normalisation: duplicate and trailing slashes and `.` parts vanish, `..` stays, a root
//!   of exactly two slashes is kept, three or more collapse to one.
//! - `Path.expanduser()` only expands a leading `~` or `~user` in the first part of a relative path
//!   (so `./~x` expands too) and raises when no home directory is known.
//! - Filename dates use Unicode `\d` (general category Nd, Unicode 15.0, `int()` reads every such
//!   digit) and `str.strip` whitespace (which also strips U+001C to U+001F). The two patterns only
//!   ever look at the first match of each pattern; an impossible calendar date (year 0, month 13,
//!   30 February, ...) moves on to the next pattern, not to the next match.
//! - The extension filter is `Path.suffix.lower()`: a name needs a dot that is neither its first nor
//!   last character, and the Kelvin sign U+212A lower-cases to `k`, so `a.mar\u{212a}down` is accepted.
//! - `scan_source` follows `os.walk` (top-down, symlinked directories are listed as directories but not
//!   entered, symlinked files and broken links are files, unreadable directories are skipped), sorts
//!   each directory's file names by code point (a byte that is not UTF-8 sorts as the surrogate
//!   `U+DC00 + byte`, Python's `surrogateescape`), checks the 500 file cap before the stat of the next
//!   allowed file (so a vanished or broken 501st name still sets `truncated`), and finally sorts by
//!   `(date is None, date or "", rel_path)`.
//! - `resolve_import_target` ports `os.path.realpath` (user-space symlink resolution, `seen` table,
//!   `..` handling), raises on a symlink loop and on an embedded NUL like `Path.resolve()` does, and
//!   checks the allowed extension on the RESOLVED name (a link `a.md` to `a.bin` is refused, a link
//!   `a` to `a.md` is accepted).
//!
//! # Known differences
//!
//! 1. Directory order. Python's `os.listdir`/`scandir` and `std::fs::read_dir` both return the
//!    `getdents` order on Linux, which is filesystem specific (hash order on ext4, not creation
//!    order). Only the 500 file cap can make that order visible (which directory is cut); the fixture
//!    trees that hit the cap have at most one directory with allowed files per level.
//! 2. Names that are not UTF-8. Ordering is exact (see above) but `name` and `rel_path` are rendered
//!    with `U+FFFD` replacement; Python would hold lone surrogates that no JSON encoder can emit.
//! 3. Local time. Python calls libc `localtime_r`, this module chrono's `Local` (it reads `TZ` and
//!    `/etc/localtime` itself). The Python float `st_mtime` and its microsecond rounding (half-even,
//!    with carry into the next second) are reproduced by [`fromtimestamp_epoch_secs`]. Python raises
//!    `ValueError` when the local year is outside 1..9999 (including the day before year 1) and when
//!    the path contains a NUL byte; `infer_date` returns `(None, "unavailable")` instead, as it does
//!    for Python's caught `OSError`.
//! 4. `external_source_paths` takes the home directory as an argument. `~user` is looked up in
//!    `/etc/passwd` only (no NSS); `external_source_paths_with` takes any lookup. With no home
//!    known the result is [`ExternalSourcesError::NoHomeDirectory`] (Python's `RuntimeError`), and
//!    there is no fallback to the current user's passwd entry when `home` is `None`.
//! 5. `scan_source` and `resolve_import_target` return `Result`: Python exceptions that its callers
//!    do not catch (`PermissionError` and other `OSError`, symlink-loop `RuntimeError`, NUL
//!    `ValueError`) are [`ExternalSourcesError`] values. `RecursionError` on a chain of ~1000 symlinks
//!    is not modelled.
//! 6. The returned `serde_json::Value` objects compare equal to the Python dict but serialise their
//!    keys sorted, not in insertion order.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use chrono::{Datelike, Local, NaiveDate, TimeZone};
use serde_json::{json, Value};

/// `ALLOWED_EXTENSIONS` of the Python module.
pub const ALLOWED_EXTENSIONS: [&str; 3] = [".txt", ".md", ".markdown"];

/// `MAX_FILES_PER_SOURCE` of the Python module.
pub const MAX_FILES_PER_SOURCE: usize = 500;

/// A Python exception the functions can raise, named after it by [`python_exception`](Self::python_exception).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalSourcesError {
    /// `RuntimeError("Could not determine home directory.")` from `Path.expanduser`.
    NoHomeDirectory,
    /// `RuntimeError("Symlink loop ...")` from `Path.resolve`.
    SymlinkLoop,
    /// `ValueError("embedded null byte")`.
    EmbeddedNul,
    /// Any other `OSError` that Python does not catch (for example `PermissionError`).
    Io(io::ErrorKind),
}

impl ExternalSourcesError {
    /// The Python exception class name this models.
    #[must_use]
    pub fn python_exception(&self) -> &'static str {
        match self {
            Self::NoHomeDirectory | Self::SymlinkLoop => "RuntimeError",
            Self::EmbeddedNul => "ValueError",
            Self::Io(_) => "OSError",
        }
    }
}

impl std::fmt::Display for ExternalSourcesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHomeDirectory => f.write_str("Could not determine home directory."),
            Self::SymlinkLoop => f.write_str("Symlink loop"),
            Self::EmbeddedNul => f.write_str("embedded null byte"),
            Self::Io(kind) => write!(f, "I/O error: {kind}"),
        }
    }
}

impl std::error::Error for ExternalSourcesError {}

// ---------------------------------------------------------------------------------------------
// Python path semantics on bytes (`pathlib.PurePosixPath` / `posixpath`).
// ---------------------------------------------------------------------------------------------

/// `PurePosixPath`: root of 0, 1 or 2 slashes plus the parts, `.` and empty parts dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PurePath {
    root_len: usize,
    parts: Vec<Vec<u8>>,
}

impl PurePath {
    fn parse(p: &[u8]) -> Self {
        let root_len = if p.first() != Some(&b'/') {
            0
        } else if p.get(1) != Some(&b'/') || p.get(2) == Some(&b'/') {
            1
        } else {
            2
        };
        let rest = p.get(root_len..).unwrap_or_default();
        let parts = rest
            .split(|b| *b == b'/')
            .filter(|s| !s.is_empty() && *s != b".")
            .map(<[u8]>::to_vec)
            .collect();
        Self { root_len, parts }
    }

    /// `str(path)`.
    fn render(&self) -> Vec<u8> {
        let mut out = vec![b'/'; self.root_len];
        let mut first = true;
        for part in &self.parts {
            if !first {
                out.push(b'/');
            }
            first = false;
            out.extend_from_slice(part);
        }
        if out.is_empty() {
            out.push(b'.');
        }
        out
    }

    fn starts_with(&self, prefix: &Self) -> bool {
        self.root_len == prefix.root_len && self.parts.starts_with(&prefix.parts)
    }
}

/// `Path.suffix.lower()` on a file name (code point semantics; lossy text is enough because only the
/// ASCII dot and the length matter).
fn path_suffix_lower(name: &[u8]) -> String {
    let text = String::from_utf8_lossy(name);
    let chars: Vec<char> = text.chars().collect();
    match chars.iter().rposition(|c| *c == '.') {
        Some(i) if i > 0 && i + 1 < chars.len() => {
            chars.iter().skip(i).collect::<String>().to_lowercase()
        }
        _ => String::new(),
    }
}

fn extension_allowed(name: &[u8]) -> bool {
    let suffix = path_suffix_lower(name);
    ALLOWED_EXTENSIONS.contains(&suffix.as_str())
}

/// `posixpath.join(a, b)`.
fn py_join(a: &[u8], b: &[u8]) -> Vec<u8> {
    if b.first() == Some(&b'/') {
        return b.to_vec();
    }
    let mut out = a.to_vec();
    if !out.is_empty() && out.last() != Some(&b'/') {
        out.push(b'/');
    }
    out.extend_from_slice(b);
    out
}

/// `posixpath.split(p)`.
fn py_split(p: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let i = p.iter().rposition(|b| *b == b'/').map_or(0, |x| x + 1);
    let head = p.get(..i).unwrap_or_default();
    let tail = p.get(i..).unwrap_or_default();
    let head = if !head.is_empty() && !head.iter().all(|b| *b == b'/') {
        let end = head.iter().rposition(|b| *b != b'/').map_or(0, |x| x + 1);
        head.get(..end).unwrap_or_default()
    } else {
        head
    };
    (head.to_vec(), tail.to_vec())
}

/// `posixpath.normpath(p)`.
fn py_normpath(p: &[u8]) -> Vec<u8> {
    if p.is_empty() {
        return b".".to_vec();
    }
    let root_len = PurePath::parse(p).root_len;
    let rest = p.get(root_len..).unwrap_or_default();
    let mut comps: Vec<&[u8]> = Vec::new();
    for comp in rest.split(|b| *b == b'/') {
        if comp.is_empty() || comp == b"." {
            continue;
        }
        if comp != b".."
            || (root_len == 0 && comps.is_empty())
            || comps.last().is_some_and(|c| *c == b"..")
        {
            comps.push(comp);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let mut out = vec![b'/'; root_len];
    out.extend_from_slice(&comps.join(&b'/'));
    if out.is_empty() {
        out.push(b'.');
    }
    out
}

/// `posixpath.abspath(p)`.
fn py_abspath(p: &[u8]) -> Result<Vec<u8>, ExternalSourcesError> {
    if p.first() == Some(&b'/') {
        return Ok(py_normpath(p));
    }
    let cwd = std::env::current_dir().map_err(|e| ExternalSourcesError::Io(e.kind()))?;
    Ok(py_normpath(&py_join(cwd.as_os_str().as_bytes(), p)))
}

/// `errno` ELOOP (`io::ErrorKind::FilesystemLoop` is not stable).
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
const ELOOP: i32 = 62;
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
const ELOOP: i32 = 40;

fn os_error(e: &io::Error) -> ExternalSourcesError {
    if e.raw_os_error() == Some(ELOOP) {
        ExternalSourcesError::SymlinkLoop
    } else {
        ExternalSourcesError::Io(e.kind())
    }
}

/// `pathlib._ignore_error`: errors `exists`, `is_dir` and `is_file` turn into `False`.
fn ignored_stat_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) || matches!(e.raw_os_error(), Some(9) | Some(ELOOP)) // EBADF, ELOOP
}

enum StatFail {
    Nul,
    Os(io::Error),
}

fn stat_follow(p: &[u8]) -> Result<fs::Metadata, StatFail> {
    if p.contains(&0) {
        return Err(StatFail::Nul);
    }
    fs::metadata(OsStr::from_bytes(p)).map_err(StatFail::Os)
}

/// `Path.exists()` / `is_dir()` / `is_file()`: `kind(metadata)` or `False` when Python would say so.
fn stat_predicate(p: &[u8], kind: fn(&fs::Metadata) -> bool) -> Result<bool, ExternalSourcesError> {
    match stat_follow(p) {
        Ok(m) => Ok(kind(&m)),
        Err(StatFail::Nul) => Ok(false),
        Err(StatFail::Os(e)) if ignored_stat_error(&e) => Ok(false),
        Err(StatFail::Os(e)) => Err(ExternalSourcesError::Io(e.kind())),
    }
}

type Seen = HashMap<Vec<u8>, Option<Vec<u8>>>;

/// `posixpath._joinrealpath(path, rest, strict=False, seen)` of CPython 3.12.15.
fn joinrealpath(
    mut path: Vec<u8>,
    rest_in: &[u8],
    seen: &mut Seen,
) -> Result<(Vec<u8>, bool), ExternalSourcesError> {
    let mut rest = rest_in;
    if rest.first() == Some(&b'/') {
        rest = rest.get(1..).unwrap_or_default();
        path = b"/".to_vec();
    }
    while !rest.is_empty() {
        let (name, after) = match rest.iter().position(|b| *b == b'/') {
            Some(i) => (
                rest.get(..i).unwrap_or_default(),
                rest.get(i + 1..).unwrap_or_default(),
            ),
            None => (rest, <&[u8]>::default()),
        };
        rest = after;
        if name.is_empty() || name == b"." {
            continue;
        }
        if name == b".." {
            if path.is_empty() {
                path = b"..".to_vec();
            } else {
                let (head, tail) = py_split(&path);
                path = head;
                if tail == b".." {
                    path = py_join(&py_join(&path, b".."), b"..");
                }
            }
            continue;
        }
        let newpath = py_join(&path, name);
        if newpath.contains(&0) {
            return Err(ExternalSourcesError::EmbeddedNul);
        }
        let is_link = fs::symlink_metadata(OsStr::from_bytes(&newpath))
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if !is_link {
            path = newpath;
            continue;
        }
        if let Some(cached) = seen.get(&newpath) {
            match cached {
                Some(resolved) => {
                    path.clone_from(resolved);
                    continue;
                }
                None => return Ok((py_join(&newpath, rest), false)),
            }
        }
        seen.insert(newpath.clone(), None);
        let target = fs::read_link(OsStr::from_bytes(&newpath)).map_err(|e| os_error(&e))?;
        let (resolved, ok) = joinrealpath(path, target.as_os_str().as_bytes(), seen)?;
        path = resolved;
        if !ok {
            return Ok((py_join(&path, rest), false));
        }
        seen.insert(newpath, Some(path.clone()));
    }
    Ok((path, true))
}

/// `Path(p).resolve()` (non-strict). `p` must already be the string form of a `Path`.
fn path_resolve(p: &[u8]) -> Result<Vec<u8>, ExternalSourcesError> {
    let (joined, _ok) = joinrealpath(Vec::new(), p, &mut Seen::new())?;
    let resolved = py_abspath(&joined)?;
    match stat_follow(&resolved) {
        Err(StatFail::Nul) => Err(ExternalSourcesError::EmbeddedNul),
        Err(StatFail::Os(e)) if e.raw_os_error() == Some(ELOOP) => {
            Err(ExternalSourcesError::SymlinkLoop)
        }
        _ => Ok(resolved),
    }
}

/// Code points of a file name as Python holds it: invalid UTF-8 bytes become `U+DC00 + byte`.
fn code_points(bytes: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    for chunk in bytes.utf8_chunks() {
        out.extend(chunk.valid().chars().map(u32::from));
        out.extend(chunk.invalid().iter().map(|b| 0xDC00 + u32::from(*b)));
    }
    out
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ---------------------------------------------------------------------------------------------
// external_source_paths
// ---------------------------------------------------------------------------------------------

/// Whose home directory `Path.expanduser` is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeOf<'a> {
    /// A bare `~`: `$HOME`, or the current user's passwd entry when `HOME` is not set.
    Current,
    /// `~name`: the passwd entry of `name`.
    User(&'a str),
}

/// `str.strip()` whitespace: Unicode White_Space plus U+001C to U+001F.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `Path(part).expanduser()` rendered as `str`.
fn expand_part(
    part: &str,
    lookup: &dyn Fn(HomeOf<'_>) -> Option<String>,
) -> Result<String, ExternalSourcesError> {
    let path = PurePath::parse(part.as_bytes());
    let first = path.parts.first();
    let rendered = match first {
        Some(first) if path.root_len == 0 && first.first() == Some(&b'~') => {
            let name = String::from_utf8_lossy(first.get(1..).unwrap_or_default()).into_owned();
            let home = if name.is_empty() {
                lookup(HomeOf::Current)
            } else {
                lookup(HomeOf::User(&name))
            };
            // An unknown home leaves "~..." in place and Path raises on it.
            let home = home.ok_or(ExternalSourcesError::NoHomeDirectory)?;
            let trimmed = home.trim_end_matches('/');
            let homedir = if trimmed.is_empty() { "/" } else { trimmed };
            if homedir.starts_with('~') {
                return Err(ExternalSourcesError::NoHomeDirectory);
            }
            let mut expanded = PurePath::parse(homedir.as_bytes());
            expanded.parts.extend(path.parts.iter().skip(1).cloned());
            expanded.render()
        }
        _ => path.render(),
    };
    Ok(String::from_utf8_lossy(&rendered).into_owned())
}

/// The value of `DIARY_EXTERNAL_SOURCES` as configured absolute paths, deduplicated, in declared
/// order. `lookup` answers `~` and `~user` (see [`HomeOf`]); `None` means unknown.
///
/// # Errors
///
/// [`ExternalSourcesError::NoHomeDirectory`] when a part starts with `~` and no home is known.
pub fn external_source_paths_with(
    raw: &str,
    lookup: &dyn Fn(HomeOf<'_>) -> Option<String>,
) -> Result<Vec<String>, ExternalSourcesError> {
    let mut paths: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let p = part.trim_matches(is_py_space);
        if p.is_empty() {
            continue;
        }
        let resolved = expand_part(p, lookup)?;
        if paths.contains(&resolved) {
            continue;
        }
        paths.push(resolved);
    }
    Ok(paths)
}

/// The home directory of `name` from `/etc/passwd`.
fn passwd_home(name: &str) -> Option<String> {
    let text = fs::read_to_string("/etc/passwd").ok()?;
    text.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next() != Some(name) {
            return None;
        }
        fields.nth(4).map(str::to_owned)
    })
}

/// [`external_source_paths_with`] with `home` as the home directory of a bare `~` (Python's
/// `$HOME`) and `/etc/passwd` for `~user`.
///
/// # Errors
///
/// As [`external_source_paths_with`].
pub fn external_source_paths(
    raw: &str,
    home: Option<&str>,
) -> Result<Vec<String>, ExternalSourcesError> {
    external_source_paths_with(raw, &|who| match who {
        HomeOf::Current => home.map(str::to_owned),
        HomeOf::User(name) => passwd_home(name),
    })
}

// ---------------------------------------------------------------------------------------------
// infer_date
// ---------------------------------------------------------------------------------------------

/// First code point of every Unicode 15.0.0 decimal digit (category Nd) block; each block is ten
/// consecutive code points with values 0 to 9. Derived from CPython 3.12.15 `re` `\d` and `int()`,
/// and checked against them for every code point by the differential test.
const DIGIT_ZEROS: [u32; 68] = [
    0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66,
    0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90,
    0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0, 0xff10,
    0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0, 0x11650,
    0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16a60, 0x16ac0,
    0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0, 0x1e950,
    0x1fbf0,
];

/// The decimal value of `c` when Python's `\d` matches it (Unicode 15.0.0 category Nd), which is
/// also what `int()` accepts.
#[must_use]
pub fn decimal_digit_value(c: char) -> Option<u32> {
    let cp = u32::from(c);
    let idx = DIGIT_ZEROS.partition_point(|zero| *zero <= cp);
    let zero = DIGIT_ZEROS.get(idx.checked_sub(1)?)?;
    let value = cp - zero;
    (value <= 9).then_some(value)
}

fn is_digit(c: char) -> bool {
    decimal_digit_value(c).is_some()
}

/// How many digits start at `start`, at most `cap`.
fn digit_run(chars: &[char], start: usize, cap: usize) -> usize {
    chars
        .iter()
        .skip(start)
        .take(cap)
        .take_while(|c| is_digit(**c))
        .count()
}

/// Python `int()` of `len` digits starting at `start`.
fn digits_value(chars: &[char], start: usize, len: usize) -> Option<u32> {
    chars
        .iter()
        .skip(start)
        .take(len)
        .try_fold(0u32, |acc, c| Some(acc * 10 + decimal_digit_value(*c)?))
}

/// The regex tail `(?:\D|$)` at `k` (a final newline is itself `\D`).
fn tail_ok(chars: &[char], k: usize) -> bool {
    chars.get(k).is_none_or(|c| !is_digit(*c))
}

/// A year, month and day as the regex groups captured them.
type DateParts = (u32, u32, u32);

/// A date pattern anchored at one character position.
type Anchored = fn(&[char], usize) -> Option<DateParts>;

/// `(\d{4})-(\d{1,2})-(\d{1,2})` anchored at `i`, with the regex's greedy-then-backtrack order.
fn match_hyphenated(chars: &[char], i: usize) -> Option<DateParts> {
    if digit_run(chars, i, 4) != 4 || chars.get(i + 4) != Some(&'-') {
        return None;
    }
    let year = digits_value(chars, i, 4)?;
    let month_at = i + 5;
    for month_len in (1..=digit_run(chars, month_at, 2)).rev() {
        if chars.get(month_at + month_len) != Some(&'-') {
            continue;
        }
        let day_at = month_at + month_len + 1;
        for day_len in (1..=digit_run(chars, day_at, 2)).rev() {
            if tail_ok(chars, day_at + day_len) {
                let month = digits_value(chars, month_at, month_len)?;
                let day = digits_value(chars, day_at, day_len)?;
                return Some((year, month, day));
            }
        }
    }
    None
}

/// `(\d{4})(\d{2})(\d{2})` anchored at `i`.
fn match_compact(chars: &[char], i: usize) -> Option<DateParts> {
    if digit_run(chars, i, 8) != 8 || !tail_ok(chars, i + 8) {
        return None;
    }
    Some((
        digits_value(chars, i, 4)?,
        digits_value(chars, i + 4, 2)?,
        digits_value(chars, i + 6, 2)?,
    ))
}

/// `rx.search(name)` for `(?:^|\D)<pattern>`: the leftmost start where the previous character is not
/// a digit (or the name starts), first match only.
fn search(chars: &[char], anchored: Anchored) -> Option<DateParts> {
    (0..chars.len()).find_map(|i| {
        let after_non_digit = i == 0 || chars.get(i - 1).is_some_and(|prev| !is_digit(*prev));
        if after_non_digit {
            anchored(chars, i)
        } else {
            None
        }
    })
}

/// `datetime(year, month, day).date()`: years 1 to 9999 only.
fn calendar_date(year: u32, month: u32, day: u32) -> Option<NaiveDate> {
    if !(1..=9999).contains(&year) {
        return None;
    }
    NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, day)
}

/// `_PyTime_Round(x, HALF_EVEN)`.
fn round_half_even(x: f64) -> f64 {
    let rounded = x.round();
    if (x - rounded).abs() == 0.5 {
        2.0 * (x / 2.0).round()
    } else {
        rounded
    }
}

/// The whole second `datetime.fromtimestamp(st_mtime)` converts to local time for a file whose
/// stat says `sec` seconds and `nsec` nanoseconds. CPython builds the float `sec + 1e-9 * nsec`,
/// rounds the fraction to microseconds half-even and carries into the second, so a time within half
/// a microsecond below a second boundary belongs to the next second. `None` when the float is not
/// finite or does not fit a `time_t`.
#[must_use]
pub fn fromtimestamp_epoch_secs(sec: i64, nsec: i64) -> Option<i64> {
    #[allow(clippy::cast_precision_loss)]
    let value = sec as f64 + 1e-9 * nsec as f64;
    if !value.is_finite() {
        return None;
    }
    let mut int_part = value.trunc();
    let micros = round_half_even((value - int_part) * 1e6);
    if micros >= 1e6 {
        int_part += 1.0;
    } else if micros < 0.0 {
        int_part -= 1.0;
    }
    if !(-9.2e18..9.2e18).contains(&int_part) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some(int_part as i64)
}

/// The local calendar date of the epoch second, `None` where Python raises (`ValueError` for years
/// outside 1..9999, also for the day before year 1) or the platform cannot convert.
fn local_date(epoch_secs: i64) -> Option<NaiveDate> {
    let date = Local.timestamp_opt(epoch_secs, 0).single()?.date_naive();
    let probe = Local
        .timestamp_opt(epoch_secs.checked_sub(86_400)?, 0)
        .single()?
        .date_naive();
    ((1..=9999).contains(&date.year()) && probe.year() >= 1).then_some(date)
}

/// Best-guess date for a source file: `(date, "filename")` from the first of the two filename
/// patterns (`YYYY-MM-DD`, then `YYYYMMDD`) that matches AND is a real calendar date, else
/// `(date, "mtime")` as a local-time date, else `(None, "unavailable")`.
#[must_use]
pub fn infer_date(path: &Path) -> (Option<NaiveDate>, &'static str) {
    let pure = PurePath::parse(path.as_os_str().as_bytes());
    let name = lossy(pure.parts.last().map_or(&[][..], Vec::as_slice));
    let chars: Vec<char> = name.chars().collect();
    for anchored in [match_hyphenated, match_compact] {
        if let Some((year, month, day)) = search(&chars, anchored) {
            if let Some(date) = calendar_date(year, month, day) {
                return (Some(date), "filename");
            }
        }
    }
    let mtime = stat_follow(&pure.render())
        .ok()
        .and_then(|m| fromtimestamp_epoch_secs(m.mtime(), m.mtime_nsec()))
        .and_then(local_date);
    match mtime {
        Some(date) => (Some(date), "mtime"),
        None => (None, "unavailable"),
    }
}

// ---------------------------------------------------------------------------------------------
// scan_source
// ---------------------------------------------------------------------------------------------

struct Entry {
    name: Vec<u8>,
    rel_path: Vec<u8>,
    size: u64,
    date: Option<String>,
    date_source: &'static str,
}

/// `entry.is_dir()` of `os.scandir` (follows symlinks, errors mean "not a directory").
fn entry_is_dir(entry: &fs::DirEntry) -> bool {
    match entry.file_type() {
        Ok(t) if t.is_dir() => true,
        Ok(t) if t.is_symlink() => fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()),
        _ => false,
    }
}

fn is_symlink(path: &[u8]) -> bool {
    !path.contains(&0)
        && fs::symlink_metadata(OsStr::from_bytes(path)).is_ok_and(|m| m.file_type().is_symlink())
}

/// List diary-like files under one configured source path (read only): a JSON object with `path`,
/// `exists`, `files` (each `{name, rel_path, size, date, date_source}`), `total`, `truncated` and,
/// for a path that is not a directory, `error`.
///
/// # Errors
///
/// An `OSError` Python would raise from `Path.exists()` or `Path.is_dir()` on the source itself
/// (for example a permission error on a parent directory).
pub fn scan_source(source_path: &str) -> Result<Value, ExternalSourcesError> {
    let root = PurePath::parse(source_path.as_bytes()).render();
    if !stat_predicate(&root, |_| true)? {
        return Ok(json!({
            "path": source_path, "exists": false, "files": [], "total": 0, "truncated": false,
        }));
    }
    if !stat_predicate(&root, fs::Metadata::is_dir)? {
        return Ok(json!({
            "path": source_path, "exists": true, "error": "not a directory",
            "files": [], "total": 0, "truncated": false,
        }));
    }

    let mut files: Vec<Entry> = Vec::new();
    let mut truncated = false;
    // `os.walk` keeps a stack of directories; (absolute-or-given path, parts relative to the root).
    let mut stack: Vec<(Vec<u8>, Vec<Vec<u8>>)> = vec![(root, Vec::new())];
    while let Some((top, rel_dir)) = stack.pop() {
        let Ok(read) = fs::read_dir(OsStr::from_bytes(&top)) else {
            continue;
        };
        let mut dirs: Vec<Vec<u8>> = Vec::new();
        let mut nondirs: Vec<Vec<u8>> = Vec::new();
        let mut broken = false;
        for item in read {
            let Ok(entry) = item else {
                broken = true;
                break;
            };
            let name = entry.file_name().as_bytes().to_vec();
            if entry_is_dir(&entry) {
                dirs.push(name);
            } else {
                nondirs.push(name);
            }
        }
        if broken {
            continue;
        }
        nondirs.sort_by_cached_key(|n| code_points(n));
        for fname in &nondirs {
            if !extension_allowed(fname) {
                continue;
            }
            if files.len() >= MAX_FILES_PER_SOURCE {
                truncated = true;
                break;
            }
            let full = py_join(&top, fname);
            let Ok(meta) = stat_follow(&full) else {
                continue; // vanished mid-scan, a broken link, a loop
            };
            let (date, date_source) = infer_date(Path::new(OsStr::from_bytes(&full)));
            let mut rel_path = rel_dir.join(&b'/');
            if !rel_dir.is_empty() {
                rel_path.push(b'/');
            }
            rel_path.extend_from_slice(fname);
            files.push(Entry {
                name: fname.clone(),
                rel_path,
                size: meta.len(),
                date: date.map(|d| d.to_string()),
                date_source,
            });
        }
        if truncated {
            break;
        }
        for dirname in dirs.iter().rev() {
            let new_path = py_join(&top, dirname);
            if !is_symlink(&new_path) {
                let mut rel = rel_dir.clone();
                rel.push(dirname.clone());
                stack.push((new_path, rel));
            }
        }
    }

    files.sort_by_cached_key(|f| {
        (
            f.date.is_none(),
            f.date.clone().unwrap_or_default(),
            code_points(&f.rel_path),
        )
    });
    let listed: Vec<Value> = files
        .iter()
        .map(|f| {
            json!({
                "name": lossy(&f.name),
                "rel_path": lossy(&f.rel_path),
                "size": f.size,
                "date": f.date,
                "date_source": f.date_source,
            })
        })
        .collect();
    Ok(json!({
        "path": source_path,
        "exists": true,
        "total": listed.len(),
        "files": listed,
        "truncated": truncated,
    }))
}

// ---------------------------------------------------------------------------------------------
// resolve_import_target
// ---------------------------------------------------------------------------------------------

/// Safely resolve a listed relative path under a configured source: the resolved path, or `None`
/// for an empty argument, an absolute or `..` containing `rel_path`, a result that is outside the
/// resolved source root, not a regular file, or whose resolved name has no allowed extension.
///
/// # Errors
///
/// What `Path.resolve()` and `Path.is_file()` raise: a symlink loop, a NUL byte, or an `OSError`
/// other than not-found / not-a-directory / loop.
pub fn resolve_import_target(
    source_path: &str,
    rel_path: &str,
) -> Result<Option<PathBuf>, ExternalSourcesError> {
    if source_path.is_empty() || rel_path.is_empty() {
        return Ok(None);
    }
    let root = PurePath::parse(source_path.as_bytes());
    let candidate = PurePath::parse(rel_path.as_bytes());
    if candidate.root_len != 0 || candidate.parts.iter().any(|p| p == b"..") {
        return Ok(None);
    }
    let mut joined = root.clone();
    joined.parts.extend(candidate.parts);
    let full = path_resolve(&joined.render())?;
    let root_resolved = path_resolve(&root.render())?;
    if !PurePath::parse(&full).starts_with(&PurePath::parse(&root_resolved)) {
        return Ok(None);
    }
    if !stat_predicate(&full, fs::Metadata::is_file)? {
        return Ok(None);
    }
    let name = PurePath::parse(&full)
        .parts
        .last()
        .cloned()
        .unwrap_or_default();
    if !extension_allowed(&name) {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(OsStr::from_bytes(&full))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normpath_and_split_follow_posixpath() {
        // Values recorded from CPython 3.12.15 posixpath.
        let cases: [(&str, &str); 9] = [
            ("/a/b/../c//./d/", "/a/c/d"),
            ("//a/../..", "//"),
            ("///a", "/a"),
            ("../a/..", ".."),
            ("a/../..", ".."),
            ("", "."),
            ("/..", "/"),
            ("./a/./b/..", "a"),
            ("/.", "/"),
        ];
        for (input, want) in cases {
            assert_eq!(py_normpath(input.as_bytes()), want.as_bytes(), "{input:?}");
        }
        assert_eq!(py_split(b"/a//"), (b"/a".to_vec(), Vec::new()));
        assert_eq!(py_split(b"//"), (b"//".to_vec(), Vec::new()));
        assert_eq!(py_split(b"a"), (Vec::new(), b"a".to_vec()));
        assert_eq!(py_split(b"/a/b"), (b"/a".to_vec(), b"b".to_vec()));
    }

    #[test]
    fn pure_path_str_form() {
        let cases: [(&str, &str); 5] = [
            ("", "."),
            ("//", "//"),
            ("///x", "/x"),
            ("a//b/./c/", "a/b/c"),
            ("/.", "/"),
        ];
        for (input, want) in cases {
            assert_eq!(
                PurePath::parse(input.as_bytes()).render(),
                want.as_bytes(),
                "{input:?}"
            );
        }
    }

    #[test]
    fn date_sort_key_puts_missing_dates_last() {
        let key = |date: Option<&str>, rel: &str| {
            (
                date.is_none(),
                date.unwrap_or_default().to_owned(),
                code_points(rel.as_bytes()),
            )
        };
        let mut keys = vec![
            key(None, "a"),
            key(Some("2026-01-02"), "b"),
            key(Some("2026-01-02"), "a"),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![
                key(Some("2026-01-02"), "a"),
                key(Some("2026-01-02"), "b"),
                key(None, "a")
            ]
        );
    }

    #[test]
    fn invalid_utf8_sorts_as_surrogate_escape() {
        let mut names: Vec<&[u8]> = vec![
            b"\xff",
            "\u{e000}".as_bytes(),
            "\u{d7ff}".as_bytes(),
            "\u{1f600}".as_bytes(),
        ];
        names.sort_by_key(|n| code_points(n));
        assert_eq!(
            names,
            vec![
                "\u{d7ff}".as_bytes(),
                b"\xff",
                "\u{e000}".as_bytes(),
                "\u{1f600}".as_bytes()
            ]
        );
    }
}
