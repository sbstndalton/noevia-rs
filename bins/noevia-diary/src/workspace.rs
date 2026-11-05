//! D4b read routes over a ready managed tenant, byte-identical to app.py: GET /api/files
//! (workspace_files.py file_list; POST /api/file's file_read is ported but not wired), GET /api/storage-status
//! (managed_storage.py status) and GET /api/workspace-export (workspace_export.py archive with
//! WORKSPACE_EXPORT_IMPL=rust, i.e. the diary-workspace-export library).
//!
//! Every function here returns `Reply::Decline` for a state whose Python reply it does not
//! reproduce exactly (Python then serves the request). Nothing here writes: every SQLite
//! connection is opened read-only.

use crate::managed::{self, Managed};
use rusqlite::types::ValueRef;
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// workspace_files.MAX_FILE / MAX_FILES.
pub const MAX_FILE: usize = 512 * 1024;
pub const MAX_FILES: usize = 500;
/// workspace_export.MAX_TOTAL / MAX_FILE and the native header bound.
const EXPORT_MAX_TOTAL: i64 = 256 * 1024 * 1024;
const EXPORT_MAX_FILE: i64 = 64 * 1024 * 1024;
const EXPORT_MAX_HEADER: usize = 16 * 1024 * 1024;
/// diary_migration.SETTING_KEYS, in order (the order of Python's settings dict).
pub const SETTING_KEYS: [&str; 6] = [
    "monthly_prefix",
    "entry_layout",
    "entries_prefix",
    "index_file",
    "month_file_template",
    "index_enabled",
];
pub const EXPORT_ERROR: &str = "Workspace export could not be completed.";
const BLOCKED_ERROR: &str = "Backup endpoint is not approved. Ask an administrator to review the connection; app saves are retained.";

/// What a route produced: a JSON reply (status, value), raw bytes with headers, or a decline.
pub enum Reply {
    Json(u16, Vec<u8>),
    /// A 200 JSON reply with `Cache-Control: no-store` (the workspace-trash list).
    JsonNoStore(Vec<u8>),
    Zip(Vec<u8>),
    /// A plain-text reply (Starlette's 500 for an unhandled exception).
    Text(u16, String),
    Decline,
}

fn json_reply<T: Serialize>(status: u16, value: &T) -> Reply {
    match serde_json::to_vec(value) {
        Ok(body) => Reply::Json(status, body),
        Err(_) => Reply::Decline,
    }
}

fn detail(status: u16, text: &str) -> Reply {
    json_reply(status, &json!({ "detail": text }))
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ------------------------------------------------------------------ Python value formatting

/// A float whose JSON form is the same in Python (`float.__repr__`) and serde_json (ryu):
/// decimal notation, i.e. zero or 1e-4 <= |v| < 1e16. Anything else is declined.
pub fn float_ok(v: f64) -> bool {
    v.is_finite() && (v == 0.0 || (1e-4..1e16).contains(&v.abs()))
}

/// `json.dumps(s)` with the default ensure_ascii=True: `"`, `\`, \b \f \n \r \t, every other
/// character outside 0x20..=0x7e as \uXXXX (lowercase hex, astral planes as surrogate pairs).
pub fn ascii_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
        }
    }
    out.push('"');
    out
}

/// Code points whose lower-case mapping is newer than Python 3.12's Unicode 15 (added in
/// Unicode 16): Rust may lower them where Python does not, changing the sort order.
fn newer_case(c: char) -> bool {
    matches!(c as u32, 0x1C89..=0x1C8A | 0xA7CB..=0xA7CD | 0xA7DA..=0xA7DC | 0x10D50..=0x10D65 | 0x10D70..=0x10D85)
}

// ------------------------------------------------------------------ files

/// workspace_files.safe_path: Err(message) is a 400.
pub(crate) fn safe_path(path: &str, directory: bool) -> Result<(), &'static str> {
    if path.chars().count() > 500 || path.contains('\\') || path.contains('\0') {
        return Err("Invalid path");
    }
    if directory && path.is_empty() {
        return Ok(());
    }
    if path
        .split('/')
        .any(|p| p.is_empty() || p == "." || p == ".." || p.starts_with('.'))
    {
        return Err("Choose a path inside the diary folder");
    }
    if !directory && !path.to_lowercase().ends_with(".md") {
        return Err("Only Markdown files are supported");
    }
    Ok(())
}

struct Listed {
    name: String,
    is_dir: bool,
    /// `lastmod`: the `updated` of the last row that produced this name (None for a directory row).
    lastmod: Option<f64>,
}

/// ManagedCorpusBackend.list_dir with lastmod. None: a value Python would read differently.
fn list_dir(store: &Managed, path: &str) -> Result<Option<Vec<Listed>>, managed::Error> {
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    };
    let conn = managed::open_ro(store.db_path())?;
    let mut stmt = conn.prepare(
        "SELECT path,version,updated FROM files UNION ALL SELECT path,NULL,NULL FROM directories",
    )?;
    let mut rows = stmt.query([])?;
    let mut entries: Vec<Listed> = Vec::new();
    while let Some(row) = rows.next()? {
        let p: String = row.get(0)?;
        let version: Option<String> = row.get(1)?;
        let updated = match row.get_ref(2)? {
            ValueRef::Null => None,
            ValueRef::Real(f) => Some(f),
            ValueRef::Integer(i) => Some(i as f64),
            _ => return Ok(None),
        };
        let Some(rest) = p.strip_prefix(&prefix) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        let name = rest.split('/').next().unwrap_or("").to_string();
        let entry = Listed {
            is_dir: rest.contains('/') || version.is_none(),
            lastmod: updated,
            name,
        };
        match entries.iter_mut().find(|e| e.name == entry.name) {
            Some(slot) => *slot = entry,
            None => entries.push(entry),
        }
    }
    if entries.iter().any(|e| e.name.chars().any(newer_case)) {
        return Ok(None);
    }
    entries.sort_by_key(|e| e.name.to_lowercase());
    // Distinct names with the same lower-case form keep the scan order, which the query planner
    // may choose differently from Python's SQLite: leave such folders to Python.
    if entries
        .windows(2)
        .any(|w| matches!(w, [a, b] if a.name.to_lowercase() == b.name.to_lowercase()))
    {
        return Ok(None);
    }
    Ok(Some(entries))
}

#[derive(Serialize)]
struct FileEntry {
    path: String,
    name: String,
    #[serde(rename = "isDir")]
    is_dir: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified: Option<f64>,
}

#[derive(Serialize)]
struct Files {
    files: Vec<FileEntry>,
}

/// GET /api/files?path= (file_list).
pub fn files(store: &Managed, path: &str) -> Reply {
    if let Err(msg) = safe_path(path, true) {
        return detail(400, msg);
    }
    let entries = match list_dir(store, path) {
        Ok(Some(e)) => e,
        _ => return Reply::Decline,
    };
    let mut out = Vec::new();
    for item in entries {
        if item.name.is_empty() || item.name.contains('/') || item.name.starts_with('.') {
            continue;
        }
        if item.is_dir || item.name.to_lowercase().ends_with(".md") {
            // _modified: a positive number, else nothing.
            let modified = item.lastmod.filter(|v| *v > 0.0);
            if modified.is_some_and(|v| !float_ok(v)) {
                return Reply::Decline;
            }
            let rel = if path.is_empty() {
                item.name.clone()
            } else {
                format!("{path}/{}", item.name)
            };
            out.push(FileEntry {
                path: rel,
                name: item.name,
                is_dir: item.is_dir,
                modified,
            });
        }
        if out.len() > MAX_FILES {
            return detail(413, "This folder has too many files; choose a subfolder");
        }
    }
    json_reply(200, &Files { files: out })
}

/// POST /api/file (a read with a body) is not wired: D5 forwards bodies only on write routes.
/// `file_read` is its byte-identical port, kept for when it is.
///
/// FastAPI's `body: dict` for POST /api/file: Some(object) when the body is JSON that Python
/// parses the same way (UTF-8 JSON object, JSON media type or none). None: decline (Python's
/// 422 and its lenient decoders stay with it).
pub fn file_body(
    content_type: Option<&[u8]>,
    body: &[u8],
) -> Option<serde_json::Map<String, Value>> {
    if let Some(raw) = content_type {
        let text = std::str::from_utf8(raw).ok().filter(|t| t.is_ascii())?;
        let media = text
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if media != "application/json" {
            return None;
        }
    }
    if body.is_empty() {
        return None;
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

#[derive(Serialize)]
struct FileRead {
    path: String,
    content: Option<String>,
    version: Option<String>,
}

/// POST /api/file (file_read).
pub fn file_read(store: &Managed, body: &serde_json::Map<String, Value>) -> Reply {
    let path = match body.get("path") {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return detail(400, "Invalid path"),
    };
    if let Err(msg) = safe_path(&path, false) {
        return detail(400, msg);
    }
    let text = match store.get_text(&path) {
        Ok(t) => t,
        Err(_) => return Reply::Decline,
    };
    if text.as_ref().is_some_and(|t| t.len() > MAX_FILE) {
        return detail(413, "File exceeds the 512 KiB editor limit");
    }
    let version = text.as_ref().map(|t| sha256_hex(t.as_bytes()));
    json_reply(
        200,
        &FileRead {
            path,
            content: text,
            version,
        },
    )
}

// ------------------------------------------------------------------ storage status

#[derive(Serialize)]
struct Status {
    mode: &'static str,
    backup: &'static str,
    #[serde(rename = "lastBackedUp")]
    last_backed_up: Option<f64>,
    #[serde(rename = "retryAt")]
    retry_at: Option<f64>,
    error: Option<String>,
}

/// ManagedCorpusBackend.destination for a descriptor. Ok(None): not a WebDAV destination.
/// Err: a value whose json.dumps this side does not reproduce.
fn destination(storage: &serde_json::Map<String, Value>) -> Result<Option<String>, ()> {
    if !matches!(
        storage.get("kind").and_then(Value::as_str),
        Some("webdav" | "nextcloud")
    ) {
        return Ok(None);
    }
    let mut parts = Vec::new();
    for key in ["baseUrl", "username", "corpusRoot"] {
        match storage.get(key) {
            None => parts.push(ascii_json_str("")),
            Some(Value::String(s)) => parts.push(ascii_json_str(s)),
            Some(_) => return Err(()),
        }
    }
    Ok(Some(sha256_hex(
        format!("[{}]", parts.join(",")).as_bytes(),
    )))
}

fn opt_float(v: ValueRef<'_>) -> Result<Option<f64>, ()> {
    let f = match v {
        ValueRef::Null => return Ok(None),
        ValueRef::Real(f) => f,
        ValueRef::Integer(i) => i as f64,
        _ => return Err(()),
    };
    if float_ok(f) {
        Ok(Some(f))
    } else {
        Err(())
    }
}

/// GET /api/storage-status (managed.status(_request_storage(request))). `storage` is the strictly
/// decoded descriptor (None when absent or not an object).
pub fn storage_status(store: &Managed, storage: Option<&serde_json::Map<String, Value>>) -> Reply {
    match status_inner(store, storage) {
        Ok(Some(s)) => json_reply(200, &s),
        _ => Reply::Decline,
    }
}

fn status_inner(
    store: &Managed,
    storage: Option<&serde_json::Map<String, Value>>,
) -> Result<Option<Status>, managed::Error> {
    let storage = storage.filter(|s| !s.is_empty());
    let conn = managed::open_ro(store.db_path())?;
    let meta = |key: &str| -> rusqlite::Result<Option<String>> {
        conn.query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
            .optional()
    };
    let mode = if meta("active")?.as_deref() == Some("1") {
        "managed"
    } else {
        "legacy"
    };
    if let Some(s) = storage {
        if s.get("kind").and_then(Value::as_str) == Some("blocked") {
            return Ok(Some(Status {
                mode,
                backup: "failed",
                last_backed_up: None,
                retry_at: None,
                error: Some(BLOCKED_ERROR.to_string()),
            }));
        }
    }
    let Ok(dest) = storage.map_or(Ok(None), destination) else {
        return Ok(None);
    };
    // int(meta('generation', '0')): a value Rust parses is read the same by Python.
    let Ok(generation) = meta("generation")?
        .unwrap_or_else(|| "0".into())
        .parse::<i64>()
    else {
        return Ok(None);
    };
    let Some(dest) = dest else {
        return Ok(Some(Status {
            mode,
            backup: "not_configured",
            last_backed_up: None,
            retry_at: None,
            error: None,
        }));
    };
    let mut stmt = conn.prepare(
        "SELECT generation,last_success,retry_at,error FROM backups WHERE destination=?",
    )?;
    let mut rows = stmt.query([&dest])?;
    let Some(row) = rows.next()? else {
        return Ok(Some(Status {
            mode,
            backup: "pending",
            last_backed_up: None,
            retry_at: None,
            error: None,
        }));
    };
    let ValueRef::Integer(row_generation) = row.get_ref(0)? else {
        return Ok(None);
    };
    let error = match row.get_ref(3)? {
        ValueRef::Null => None,
        ValueRef::Text(t) => match std::str::from_utf8(t) {
            Ok(s) => Some(s.to_string()),
            Err(_) => return Ok(None),
        },
        _ => return Ok(None),
    };
    let (Ok(last), Ok(retry)) = (opt_float(row.get_ref(1)?), opt_float(row.get_ref(2)?)) else {
        return Ok(None);
    };
    let failed = error.as_deref().is_some_and(|e| !e.is_empty());
    Ok(Some(Status {
        mode,
        backup: if failed {
            "failed"
        } else if row_generation != generation {
            "pending"
        } else {
            "complete"
        },
        last_backed_up: last,
        retry_at: if failed { retry } else { None },
        error,
    }))
}

// ------------------------------------------------------------------ workspace export

/// The export settings: Python's `corpus_settings(st.cfg)` for a managed tenant, as the exact
/// text `json.dumps(..., ensure_ascii=False, separators=(",", ":"))` gives. None when a value is
/// not a plain scalar (this side reproduces str, bool and integers only) or a stored setting
/// would make Python's config overlay behave differently (a dotted key).
pub fn export_settings_text(
    base: &serde_json::Map<String, Value>,
    tenant: &managed::Settings,
) -> Option<String> {
    if tenant.keys().any(|k| k.contains('.')) {
        return None;
    }
    let mut parts = Vec::new();
    for key in SETTING_KEYS {
        let value = tenant.get(key).or_else(|| base.get(key));
        let text = match value {
            None | Some(Value::Null) => continue,
            Some(Value::String(s)) => serde_json::to_string(s).ok()?,
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::Number(n)) if n.is_i64() || n.is_u64() => n.to_string(),
            Some(_) => return None,
        };
        parts.push(format!("{}:{text}", serde_json::to_string(key).ok()?));
    }
    Some(format!("{{{}}}", parts.join(",")))
}

/// GET /api/workspace-export for a managed tenant with WORKSPACE_EXPORT_IMPL=rust: the same
/// single read transaction and checks as workspace_export.archive, then the same
/// diary-workspace-export frame the Python adapter sends its binary, built in-process.
pub fn export(store: &Managed, settings_text: &str) -> Reply {
    match export_inner(store, settings_text) {
        Ok(r) => r,
        Err(_) => Reply::Decline,
    }
}

fn export_inner(store: &Managed, settings_text: &str) -> Result<Reply, managed::Error> {
    let mut conn = managed::open_ro(store.db_path())?;
    let tx = conn.transaction()?;
    // Text bodies make Python's bytes() fail (an unhandled error): not reproduced here.
    let non_blob: i64 = tx.query_row(
        "SELECT count(*) FROM files WHERE typeof(data) != 'blob'",
        [],
        |r| r.get(0),
    )?;
    if non_blob != 0 {
        return Ok(Reply::Decline);
    }
    let (count, total, largest): (i64, i64, i64) = tx.query_row(
        "SELECT count(*),coalesce(sum(length(data)),0),coalesce(max(length(data)),0) FROM files",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if count > 5000 || total > EXPORT_MAX_TOTAL || largest > EXPORT_MAX_FILE {
        return Ok(detail(
            409,
            "Export exceeds the 5,000 file / 64 MiB per file / 256 MiB safety limit",
        ));
    }
    let directories: Vec<String> = {
        let mut stmt = tx.prepare("SELECT path FROM directories ORDER BY path LIMIT 5002")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };
    if directories.len() > 5000 {
        return Ok(detail(409, "Export contains too many directories"));
    }
    let mut metadata = Vec::new();
    let mut bodies: Vec<u8> = Vec::new();
    {
        let mut stmt = tx.prepare("SELECT path,data FROM files ORDER BY path")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let ValueRef::Blob(data) = row.get_ref(1)? else {
                return Ok(Reply::Decline);
            };
            metadata.push(json!({"path": path, "bytes": data.len()}));
            bodies.extend_from_slice(data);
        }
    }
    drop(tx);
    let header = format!(
        "{{\"settings\":{settings_text},\"directories\":{},\"files\":{}}}",
        serde_json::to_string(&directories)?,
        serde_json::to_string(&metadata)?,
    );
    if header.len() > EXPORT_MAX_HEADER {
        return Ok(detail(409, EXPORT_ERROR));
    }
    let mut frame = Vec::with_capacity(header.len() + bodies.len() + 64);
    frame.extend_from_slice(diary_workspace_export::MAGIC);
    frame.extend_from_slice(&(header.len() as u32).to_be_bytes());
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(&bodies);
    drop(bodies);
    Ok(match diary_workspace_export::export(frame.as_slice()) {
        Ok(zip) => Reply::Zip(zip),
        Err(_) => detail(409, EXPORT_ERROR),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const BS: &str = "\\";

    #[test]
    fn ascii_json_matches_python_dumps() {
        assert_eq!(ascii_json_str("a\"b\\c"), r#""a\"b\\c""#);
        let u = |code: &str| format!("{}u{code}", BS);
        assert_eq!(
            ascii_json_str("\u{e9}\u{7f}\u{1}"),
            format!("\"{}{}{}\"", u("00e9"), u("007f"), u("0001"))
        );
        assert_eq!(
            ascii_json_str("\u{1f600}"),
            format!("\"{}{}\"", u("d83d"), u("de00"))
        );
        assert_eq!(ascii_json_str("\n\t\u{8}"), r#""\n\t\b""#);
    }

    #[test]
    fn floats_in_decimal_range_only() {
        assert!(float_ok(1_762_280_000.25) && float_ok(0.0) && float_ok(0.0001));
        assert!(!float_ok(1e16) && !float_ok(0.00001) && !float_ok(f64::NAN));
        assert_eq!(
            serde_json::to_string(&1_762_280_000.0f64).ok().as_deref(),
            Some("1762280000.0")
        );
    }

    #[test]
    fn safe_path_is_workspace_files_safe_path() {
        assert_eq!(safe_path("", true), Ok(()));
        assert_eq!(
            safe_path("", false),
            Err("Choose a path inside the diary folder")
        );
        assert_eq!(safe_path("a\\b.md", false), Err("Invalid path"));
        assert_eq!(
            safe_path(".git", true),
            Err("Choose a path inside the diary folder")
        );
        assert_eq!(
            safe_path("a/notes.txt", false),
            Err("Only Markdown files are supported")
        );
        assert_eq!(safe_path("A/B.MD", false), Ok(()));
        assert_eq!(safe_path(&"é".repeat(501), true), Err("Invalid path"));
        assert_eq!(safe_path(&"é".repeat(500), true), Ok(()));
    }

    #[test]
    fn destination_is_python_digest() {
        // hashlib.sha256(json.dumps(["https://dav.invalid/\u{fc}","u",""],separators=(",",":")).encode())
        let mut d = serde_json::Map::new();
        d.insert("kind".into(), json!("webdav"));
        d.insert("baseUrl".into(), json!("https://dav.invalid/\u{fc}"));
        d.insert("username".into(), json!("u"));
        let expect =
            sha256_hex(format!("[\"https://dav.invalid/{}u00fc\",\"u\",\"\"]", BS).as_bytes());
        assert_eq!(destination(&d), Ok(Some(expect)));
        d.insert("username".into(), json!(5));
        assert_eq!(destination(&d), Err(()));
        d.insert("kind".into(), json!("s3"));
        assert_eq!(destination(&d), Ok(None));
    }

    #[test]
    fn export_settings_follow_python_overlay() {
        let mut base = serde_json::Map::new();
        base.insert("entry_layout".into(), json!("daily"));
        base.insert("index_enabled".into(), json!(true));
        base.insert("monthly_prefix".into(), Value::Null);
        let mut tenant = managed::Settings::new();
        tenant.insert("index_enabled".into(), json!(false));
        tenant.insert("entries_prefix".into(), json!("Ent\"ries"));
        assert_eq!(
            export_settings_text(&base, &tenant).as_deref(),
            Some(r#"{"entry_layout":"daily","entries_prefix":"Ent\"ries","index_enabled":false}"#)
        );
        tenant.insert("index_file".into(), json!(1.5));
        assert!(export_settings_text(&base, &tenant).is_none());
        let mut dotted = managed::Settings::new();
        dotted.insert("a.b".into(), json!("x"));
        assert!(export_settings_text(&base, &dotted).is_none());
    }

    #[test]
    fn file_body_accepts_only_what_fastapi_parses_alike() {
        assert!(file_body(None, br#"{"path":"a.md"}"#).is_some());
        assert!(file_body(
            Some(b"application/json; charset=utf-8"),
            br#"{"path":"a.md"}"#
        )
        .is_some());
        assert!(file_body(Some(b"text/plain"), br#"{"path":"a.md"}"#).is_none());
        assert!(file_body(None, b"[1]").is_none());
        assert!(file_body(None, b"").is_none());
        assert!(file_body(None, b"{\"x\":NaN}").is_none());
    }
}
