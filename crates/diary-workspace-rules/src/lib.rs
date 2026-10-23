//! Pure legacy-compatible rules used by the managed Diary workspace.
//!
//! This crate owns no database, filesystem, HTTP, or process boundary. Its caller
//! supplies the configured paths and locale month names from the Python store.
//! Legacy capsule reads intentionally accept the shipped Python decoder's fields;
//! new writer bytes use a separate checked API.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use regex::Regex;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const MAX_FILE: usize = 512 * 1024;
pub const MAX_ENTRIES: usize = 500;
pub const MAX_BYTES: usize = 50 * 1024 * 1024;
pub const TRASH_PREFIX: &str = ".noevia-trash/";

/// Python `_bounded`: no subtree bodies are read by `stat`, but mutation
/// callers pass their already loaded byte count here before planning writes.
pub fn check_tree_bounds(files: usize, dirs: usize, total_bytes: usize) -> Result<()> {
    if files.saturating_add(dirs) > MAX_ENTRIES || total_bytes > MAX_BYTES {
        Err(Error::Bounds)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidPath,
    InvalidTemplate,
    InvalidId,
    InvalidCapsule,
    TooLarge,
    CaptureRecord,
    Bounds,
    InvalidTimestamp,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Python `CorpusStore._join`: strip `/` around each nonempty component.
pub fn join(root: &str, parts: &[&str]) -> String {
    std::iter::once(root)
        .chain(parts.iter().copied())
        .filter(|part| !part.is_empty())
        .map(|part| part.trim_matches('/'))
        .collect::<Vec<_>>()
        .join("/")
}

/// Python `workspace_ops.rel_path`, including its different root option.
pub fn rel_path(path: &str, allow_root: bool) -> Result<String> {
    if path.chars().count() > 500 || path.contains(['\\', '\0']) {
        return Err(Error::InvalidPath);
    }
    let path = path.trim_matches('/');
    if path.is_empty() {
        return if allow_root {
            Ok(String::new())
        } else {
            Err(Error::InvalidPath)
        };
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || matches!(part, "." | "..") || part.starts_with('.'))
    {
        return Err(Error::InvalidPath);
    }
    Ok(path.to_owned())
}

/// Python `workspace_files.safe_path` for file paths; no slash trimming.
pub fn safe_file_path(path: &str) -> Result<()> {
    if path.chars().count() > 500
        || path.contains(['\\', '\0'])
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.starts_with('.'))
        || !path.to_lowercase().ends_with(".md")
    {
        return Err(Error::InvalidPath);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ProtectionConfig {
    pub remote_root: String,
    pub entries_prefix: String,
    pub monthly_prefix: String,
    pub month_file_template: String,
    pub index_file: String,
    /// `calendar.month_name[1..12]` from the caller's active Python locale.
    pub month_names: Vec<String>,
}

impl ProtectionConfig {
    pub fn protected(&self, full: &str) -> Result<bool> {
        let entries = join(&self.remote_root, &[&self.entries_prefix])
            .trim_end_matches('/')
            .to_owned();
        let memory = join(&self.remote_root, &["AI Memory"])
            .trim_end_matches('/')
            .to_owned();
        let index = join(&self.remote_root, &[&self.monthly_prefix, &self.index_file]);
        if full == index
            || full == entries
            || full == memory
            || full.starts_with(&format!("{entries}/"))
            || full.starts_with(&format!("{memory}/"))
        {
            return Ok(true);
        }
        if self.month_names.len() != 12 {
            return Err(Error::InvalidTemplate);
        }
        let template = join(
            &self.remote_root,
            &[&self.monthly_prefix, &self.month_file_template],
        );
        let mut pattern = regex::escape(&template);
        for (field, replacement) in [
            ("{year}", "[0-9]{4}"),
            ("{month02}", "[0-9]{2}"),
            ("{month}", "[0-9]{1,2}"),
        ] {
            pattern = pattern.replace(&regex::escape(field), replacement);
        }
        let months = self
            .month_names
            .iter()
            .map(|name| regex::escape(name))
            .collect::<Vec<_>>()
            .join("|");
        pattern = pattern.replace(&regex::escape("{month_name}"), &format!("(?:{months})"));
        let re = Regex::new(&format!(r"\A(?:{pattern})\z")).map_err(|_| Error::InvalidTemplate)?;
        Ok(re.is_match(full))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersion {
    pub path: String,
    pub version: String,
}

/// Python `folder_version`: path slices are Unicode code points; sorted lines
/// include every descendant file and explicit directory, including the root.
pub fn folder_version(files: &[FileVersion], dirs: &[String], full: &str) -> String {
    let offset = full.chars().count();
    let suffix = |path: &str| path.chars().skip(offset).collect::<String>();
    let mut lines = files
        .iter()
        .map(|f| format!("f\t{}\t{}", suffix(&f.path), f.version))
        .chain(dirs.iter().map(|d| format!("d\t{}", suffix(d))))
        .collect::<Vec<_>>();
    lines.sort();
    sha256(lines.join("\n").as_bytes())
}

/// Python `_replaced_name`: preserve the parent folder and timestamp suffix
/// while shortening a near-500-codepoint file name for a Trash restore beside
/// its current version. Caller supplies the original relative path and clock.
pub fn replaced_name(relative: &str, now: f64) -> Result<String> {
    if !now.is_finite() {
        return Err(Error::InvalidTimestamp);
    }
    let seconds = now.floor();
    if seconds < i64::MIN as f64 || seconds > i64::MAX as f64 {
        return Err(Error::InvalidTimestamp);
    }
    let date =
        chrono::DateTime::from_timestamp(seconds as i64, 0).ok_or(Error::InvalidTimestamp)?;
    let suffix = format!(" (replaced {}).md", date.format("%Y-%m-%d %H.%M.%S"));
    let budget = 500usize.saturating_sub(suffix.chars().count());
    let mut stem = if relative.to_lowercase().ends_with(".md") {
        relative
            .chars()
            .take(relative.chars().count().saturating_sub(3))
            .collect::<String>()
    } else {
        relative.to_owned()
    };
    if stem.chars().count() > budget {
        let (folder, name) = stem.rsplit_once('/').unwrap_or(("", stem.as_str()));
        let prefix = if folder.is_empty() {
            String::new()
        } else {
            format!("{folder}/")
        };
        let room = budget as isize - prefix.chars().count() as isize;
        if room < 1 {
            stem = stem
                .chars()
                .rev()
                .take(budget)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>();
            stem = stem
                .trim_start_matches('.')
                .trim_start_matches('/')
                .to_owned();
            if stem.is_empty() {
                stem = "file".into();
            }
        } else {
            let name = name.trim_start_matches('.');
            let name = if name.is_empty() { "file" } else { name };
            stem = format!(
                "{prefix}{}",
                name.chars().take(room as usize).collect::<String>()
            );
        }
    }
    Ok(format!("{stem}{suffix}"))
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The Python regex `<!--\\s*xid:[0-9a-fA-F-]+\\s*-->` is applied after
/// UTF-8 decode with replacement. Rust regex uses Unicode whitespace here.
pub fn trashable(raw: &[u8]) -> Result<()> {
    if raw.len() > MAX_FILE {
        return Err(Error::TooLarge);
    }
    let marker =
        Regex::new(r"<!--[\s\x{001c}-\x{001f}]*xid:[0-9a-fA-F-]+[\s\x{001c}-\x{001f}]*-->")
            .map_err(|_| Error::CaptureRecord)?;
    if marker.is_match(&String::from_utf8_lossy(raw)) {
        return Err(Error::CaptureRecord);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrashState {
    Trashed,
    Restored,
}
impl TrashState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trashed => "trashed",
            Self::Restored => "restored",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewCapsule<'a> {
    pub id: &'a str,
    pub path: &'a str,
    pub raw: &'a [u8],
    pub state: TrashState,
    pub trashed_at: f64,
    pub restored_at: Option<f64>,
}

/// Exact `json.dumps(sort_keys=True, ensure_ascii=False).encode()` order/spaces
/// for the seven (or restored eight) Trash-v1 fields. Checks the new-writer
/// contract; the legacy reader below is intentionally looser.
pub fn encode_writer(c: &NewCapsule<'_>) -> Result<Vec<u8>> {
    if normalize_uuid(c.id)? != c.id
        || !c.trashed_at.is_finite()
        || c.restored_at.is_some_and(|v| !v.is_finite())
        || (c.state == TrashState::Restored) != c.restored_at.is_some()
    {
        return Err(Error::InvalidCapsule);
    }
    safe_file_path(c.path)?;
    trashable(c.raw)?;
    let quoted = |v: &str| serde_json::to_string(v).map_err(|_| Error::InvalidCapsule);
    let id = quoted(c.id)?;
    let path = quoted(c.path)?;
    let data = quoted(&STANDARD.encode(c.raw))?;
    let version = quoted(&sha256(c.raw))?;
    let restored = c
        .restored_at
        .map(|at| format!(", \"restoredAt\": {}", python_float(at)))
        .unwrap_or_default();
    Ok(format!("{{\"data\": {data}, \"format\": \"noevia-trash-v1\", \"id\": {id}, \"path\": {path}{restored}, \"state\": \"{}\", \"trashedAt\": {}, \"version\": {version}}}", c.state.as_str(), python_float(c.trashed_at)).into_bytes())
}

fn python_space(c: char) -> bool {
    c.is_whitespace()
}

// Python 3.12 Unicode 15 Nd zero points; int(text, 16) accepts decimal
// digits from every block as digits 0..9, alongside ASCII a..f/A..F.
const DECIMAL_ZEROES: [u32; 68] = [
    0x30, 0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66,
    0xDE6, 0xE50, 0xED0, 0xF20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90,
    0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10,
    0x104A0, 0x10D30, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0, 0x11650,
    0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50, 0x11DA0, 0x11F50, 0x16A60, 0x16AC0,
    0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E950,
    0x1FBF0,
];
fn decimal_digit(c: char) -> Option<u32> {
    let cp = u32::from(c);
    DECIMAL_ZEROES
        .iter()
        .find_map(|zero| (cp >= *zero && cp < *zero + 10).then(|| cp - *zero))
}

/// Python `uuid.UUID(value)` for the string forms accepted by its constructor:
/// strip exact lowercase `urn:`/`uuid:` substrings, outer braces and hyphens,
/// then parse 32-character Python base-16 integer text. Writer output is canonical.
pub fn normalize_uuid(value: &str) -> Result<String> {
    let cleaned = value.replace("urn:", "").replace("uuid:", "");
    let cleaned = cleaned.trim_matches(['{', '}']).replace('-', "");
    if cleaned.chars().count() != 32 {
        return Err(Error::InvalidId);
    }
    let unsigned = cleaned.trim_matches(python_space);
    let unsigned = unsigned.strip_prefix('+').unwrap_or(unsigned);
    let (unsigned, prefixed) = match unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
    {
        Some(digits) => (digits, true),
        None => (unsigned, false),
    };
    let unsigned = if prefixed {
        unsigned.strip_prefix('_').unwrap_or(unsigned)
    } else {
        unsigned
    };
    if unsigned.is_empty()
        || unsigned.starts_with('_')
        || unsigned.ends_with('_')
        || unsigned.contains("__")
    {
        return Err(Error::InvalidId);
    }
    let mut compact = String::with_capacity(unsigned.len());
    for c in unsigned.chars() {
        if c == '_' {
            continue;
        }
        if let Some(digit) = decimal_digit(c) {
            compact.push(char::from_digit(digit, 16).ok_or(Error::InvalidId)?);
        } else if c.is_ascii_hexdigit() {
            compact.push(c);
        } else {
            return Err(Error::InvalidId);
        }
    }
    let number = u128::from_str_radix(&compact, 16).map_err(|_| Error::InvalidId)?;
    let hex = format!("{number:032x}");
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

#[derive(Debug, Clone)]
pub struct LegacyCapsule {
    /// Parsed object for inspection. A future writer must not reserialize this
    /// map as a lossless representation of unknown numeric fields.
    pub record: Map<String, Value>,
    /// Original JSON bytes retained for legacy receipt inspection.
    pub encoded: Vec<u8>,
    pub raw: Vec<u8>,
}

/// Python `workspace_trash.decode` semantics: required fields are checked,
/// unknown fields and timestamp types remain untouched. Errors are distinct
/// from a valid empty file. Caller passes the operation string used for lookup.
pub fn decode_legacy(data: &[u8], operation: &str) -> Result<LegacyCapsule> {
    let value: Value = serde_json::from_slice(data).map_err(|_| Error::InvalidCapsule)?;
    let record = value.as_object().ok_or(Error::InvalidCapsule)?;
    if record.get("format").and_then(Value::as_str) != Some("noevia-trash-v1")
        || record.get("id").and_then(Value::as_str) != Some(operation)
        || !matches!(
            record.get("state").and_then(Value::as_str),
            Some("trashed" | "restored")
        )
    {
        return Err(Error::InvalidCapsule);
    }
    let path = record
        .get("path")
        .and_then(Value::as_str)
        .ok_or(Error::InvalidCapsule)?;
    safe_file_path(path).map_err(|_| Error::InvalidCapsule)?;
    let encoded = record
        .get("data")
        .and_then(Value::as_str)
        .ok_or(Error::InvalidCapsule)?;
    let raw = STANDARD
        .decode(encoded)
        .map_err(|_| Error::InvalidCapsule)?;
    if raw.len() > MAX_FILE
        || record.get("version").and_then(Value::as_str) != Some(sha256(&raw).as_str())
    {
        return Err(Error::InvalidCapsule);
    }
    Ok(LegacyCapsule {
        record: record.clone(),
        encoded: data.to_vec(),
        raw,
    })
}

/// Strict new-capsule read: exact bytes emitted by the checked writer. A
/// legacy capsule can remain readable without being accepted as new output.
pub fn decode_strict(data: &[u8], operation: &str) -> Result<LegacyCapsule> {
    let decoded = decode_legacy(data, operation)?;
    let rec = &decoded.record;
    let state = match rec.get("state").and_then(Value::as_str) {
        Some("trashed") => TrashState::Trashed,
        Some("restored") => TrashState::Restored,
        _ => return Err(Error::InvalidCapsule),
    };
    let at = rec
        .get("trashedAt")
        .and_then(Value::as_f64)
        .ok_or(Error::InvalidCapsule)?;
    let restored = rec.get("restoredAt").and_then(Value::as_f64);
    let expected_count = if state == TrashState::Restored { 8 } else { 7 };
    if rec.len() != expected_count {
        return Err(Error::InvalidCapsule);
    }
    let expected = encode_writer(&NewCapsule {
        id: operation,
        path: rec
            .get("path")
            .and_then(Value::as_str)
            .ok_or(Error::InvalidCapsule)?,
        raw: &decoded.raw,
        state,
        trashed_at: at,
        restored_at: restored,
    })?;
    if expected != data {
        return Err(Error::InvalidCapsule);
    }
    Ok(decoded)
}

// Same shortest-decimal cutoff as CPython 3.12 JSON/repr, used by the existing
// noevia-diary-backup-v1 pure format crate. JSON encodes finite time.time() values.
fn python_float(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .into();
    }
    let mut buffer = ryu::Buffer::new();
    let raw = buffer.format_finite(value.abs());
    let (mantissa, exponent) = raw
        .split_once('e')
        .map(|(m, e)| (m, e.parse::<i32>().unwrap_or(0)))
        .unwrap_or((raw, 0));
    let integer_len = mantissa.find('.').unwrap_or(mantissa.len());
    let mut digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let leading = digits.bytes().take_while(|c| *c == b'0').count();
    let mut power = exponent + integer_len as i32 - 1;
    if leading > 0 {
        digits = digits.chars().skip(leading).collect();
        power -= leading as i32;
    }
    while digits.ends_with('0') && digits.len() > 1 {
        digits.pop();
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if !(-4..16).contains(&power) {
        let first = digits.get(..1).unwrap_or("");
        let tail = digits.get(1..).unwrap_or("");
        let fraction = if tail.is_empty() {
            String::new()
        } else {
            format!(".{tail}")
        };
        format!(
            "{sign}{first}{fraction}e{}{:02}",
            if power >= 0 { '+' } else { '-' },
            power.unsigned_abs()
        )
    } else {
        let position = power + 1;
        if position <= 0 {
            format!("{sign}0.{}{digits}", "0".repeat((-position) as usize))
        } else if position as usize >= digits.len() {
            format!(
                "{sign}{digits}{}.0",
                "0".repeat(position as usize - digits.len())
            )
        } else {
            let (left, right) = digits.split_at(position as usize);
            format!("{sign}{left}.{right}")
        }
    }
}
