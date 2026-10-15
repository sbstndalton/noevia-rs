//! `GET /api/export/conversations` (core `server/routes/export.cjs`, #1315): the signed-in user's
//! own chats as a ZIP, read from that user's directory on disk.
//!
//! What Node hands `buildExport`, read fresh for every request (owner decision 2026-10-12 on
//! sbstndalton/noevia#1315: the front exports what is on disk; a Node process's unsaved or stale
//! in-memory copy is an accepted, documented difference):
//!
//! - `freeChats`: `UI_DATA_DIR/users/<id>/free-chats.json`; missing is `[]`, and a JSON value that
//!   is not a list is `[]` (core workspace.cjs since #1420).
//! - `projects`: `projects.json`'s `projects` (missing file, `null`, a non-object or a missing /
//!   `null` `projects` is `[]`, core rust-projects.cjs `readProjectsFile`), with the Diary-internal
//!   projects left out (`diary-extras.cjs` `internalProject`: id `cowork-diary-extras` or starting
//!   with `cowork-chat-context-`). An entry Node's workspace load or that filter throws on (not an
//!   object, or an id that is not a string) is Node's 500.
//! - `readHistory(id)`: core projects.cjs `readHistory`, `JSON.parse(history-<safe id>.json).history
//!   || []` with every failure (an id that sanitises to nothing, a missing or unreadable file,
//!   invalid JSON) read as `[]`.
//!
//! Tenant scope: the only directory read is the one [`server_projects::store::user_dir`] derives
//! from the user id the request gate returned; chat ids name files only after Node's
//! `[^a-zA-Z0-9_-]` reduction, so no id reaches another directory. Nothing is written here (the
//! audit row is the caller's, after a successful build, as in Node).
//!
//! No lock is taken to read (#1824): every writer of projects.json and free-chats.json replaces
//! the file by rename (core workspace.cjs `atomicJson`, rust-projects.cjs `save` through it,
//! source-text.cjs's restore, server-store `write_shared_projects` for server-projects), so a
//! reader sees the old file or the new one, never half of one. The one non-rename copy, core
//! workspace.cjs `claimLegacy`'s `cpSync`, fills a user directory that did not exist before; a
//! read of a half-copied file fails to parse and is refused (500), never exported.
//!
//! Memory (#1824): a history file is read up to 12 MiB (#1830; 128 MiB per export), its values and depth
//! are counted on the text before a tree is built (500,000 values, depth 40, else Node's 413), the
//! text is moved, never copied, unless it must be decoded lossily, and the ZIP is made as chunks
//! that are sent in order, so the payload is held once. Peak resident memory of the largest
//! admitted export: tests/peak_memory.rs.
//!
//! Files are opened without following a symlink and without blocking on a FIFO; anything but a
//! regular file (or a directory, Node's empty history) is refused (500).
//!
//! Narrower admission than Node, as refusals, never as different bytes:
//! - a free-chats.json that is not JSON, or a projects.json whose `projects` is not a list or that
//!   is not JSON (Node at load would export an empty list, or keep its earlier copy): 500;
//! - a value this port cannot hold (js-json `Refusal::Unsupported`): 500;
//! - a metadata file over [`META_READ_CAP`] or the value / depth bounds: 500; a history file over
//!   [`HISTORY_READ_CAP`] or the value / depth bounds (even one that is not JSON, which Node would
//!   read as `[]`), more than [`TOTAL_READ_CAP`] of history in one export, or a chat over the
//!   Markdown foundation's per-chat bounds: Node's 413 (`too large to export`);
//! - a symlinked or non-regular (FIFO, socket, device) history or store file: 500.
use crate::assemble::{self, Limits};
use js_json::wtf16;
use js_json::JValue;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The largest free-chats.json or projects.json read.
pub const META_READ_CAP: u64 = 16 * 1024 * 1024;
/// The largest single history file read (#1824).
pub const HISTORY_READ_CAP: u64 = 12 * 1024 * 1024;
/// The most history bytes read for one export (#1824).
pub const TOTAL_READ_CAP: u64 = 128 * 1024 * 1024;
/// The most JSON values (every array, object, string, number and literal, keys included) one
/// file may hold, counted on the text before any tree is built (#1824).
pub const MAX_FILE_VALUES: usize = 500_000;
/// The deepest nesting one file may hold, counted the same way.
pub const MAX_FILE_DEPTH: usize = 40;

const DIARY_EXTRAS: &str = "cowork-diary-extras";
const CHAT_CONTEXT_PREFIX: &str = "cowork-chat-context-";

/// What the route answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 200, `application/zip`: the ZIP as the chunks it was made in (sent in order, never joined)
    /// and their total length; `day` is `new Date(when).toISOString().slice(0, 10)`.
    Zip {
        chunks: Vec<Vec<u8>>,
        len: usize,
        day: String,
    },
    /// 413 `{"error": message}`; nothing is audited or sent.
    TooLarge(String),
    /// 500 `{"error":"Internal error"}` (core http.cjs errorResponse for a thrown error).
    Internal,
}

impl Outcome {
    /// The ZIP's bytes in one buffer (tests).
    pub fn zip_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Outcome::Zip { chunks, .. } => Some(chunks.concat()),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum HistoryFault {
    TooLarge,
    Unsupported,
}

/// The export of the user `user_id` under `data_dir` (UI_DATA_DIR) at `now_ms`.
pub fn export(data_dir: &Path, user_id: &str, now_ms: i64) -> Outcome {
    let Ok(dir) = server_projects::store::user_dir(data_dir, user_id) else {
        return Outcome::Internal; // workspace.cjs userDir throws
    };
    let Some(day) = day(now_ms) else {
        return Outcome::Internal;
    };
    let free = match free_chats(&dir) {
        Ok(v) => v,
        Err(()) => return Outcome::Internal,
    };
    let projects = match projects(&dir) {
        Ok(v) => v,
        Err(()) => return Outcome::Internal,
    };
    let mut read_total = 0u64;
    let limits = Limits::default();
    let built = assemble::build_export_chunks(
        &free,
        &projects,
        |id| read_history(&dir, id, &mut read_total),
        now_ms,
        limits,
    );
    let too_large = || Outcome::TooLarge(assemble::too_large_message(limits));
    match built {
        Ok((chunks, len)) => Outcome::Zip { chunks, len, day },
        Err(assemble::Error::TooLarge)
        | Err(assemble::Error::History(HistoryFault::TooLarge))
        | Err(assemble::Error::Markdown(crate::markdown::Error::Limit))
        | Err(assemble::Error::Zip(crate::Error::Limit)) => too_large(),
        Err(_) => Outcome::Internal,
    }
}

/// `YYYY-MM-DD` of `now_ms` (UTC), years 0..=9999 only.
fn day(now_ms: i64) -> Option<String> {
    let t = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms)?;
    if !(0..=9999).contains(&chrono::Datelike::year(&t)) {
        return None;
    }
    Some(t.format("%Y-%m-%d").to_string())
}

/// Why a file was not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadFault {
    /// Over the cap.
    TooLarge,
    /// Not a regular file reached without a symlink (a symlink, a FIFO, a socket, a device):
    /// refused, never followed or waited on (#1824).
    NotRegular,
    /// A directory, or a read error: what Node's readFileSync throws inside its try.
    Unreadable,
}

/// The file's bytes up to `cap`; `Ok(None)` when it is missing. Opened without following a
/// symlink and without blocking on a FIFO; only a regular file is read.
fn read_capped(file: &Path, cap: u64) -> Result<Option<Vec<u8>>, ReadFault> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let f = match options.open(file) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // ELOOP: the last component is a symlink.
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(ReadFault::NotRegular),
        Err(_) => return Err(ReadFault::Unreadable),
    };
    let meta = f.metadata().map_err(|_| ReadFault::Unreadable)?;
    if meta.is_dir() {
        return Err(ReadFault::Unreadable); // EISDIR in Node
    }
    if !meta.is_file() {
        return Err(ReadFault::NotRegular);
    }
    if meta.len() > cap {
        return Err(ReadFault::TooLarge);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(meta.len()).unwrap_or(0))
        .map_err(|_| ReadFault::TooLarge)?;
    f.take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| ReadFault::Unreadable)?;
    if bytes.len() as u64 > cap {
        return Err(ReadFault::TooLarge);
    }
    Ok(Some(bytes))
}

/// The values (every array, object, string, number and literal, keys included) and the deepest
/// nesting in a JSON-ish text, counted without allocating; `false` past either bound. Strings
/// are skipped as Node's parser would, so brackets inside them do not count.
fn within_bounds(text: &str) -> bool {
    let (mut values, mut depth, mut in_string, mut escaped, mut in_token) =
        (0usize, 0usize, false, false, false);
    for b in text.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        let token = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b'.');
        if token && !in_token {
            values += 1;
        }
        in_token = token;
        match b {
            b'"' => {
                in_string = true;
                values += 1;
            }
            b'[' | b'{' => {
                values += 1;
                depth += 1;
                if depth > MAX_FILE_DEPTH {
                    return false;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if values > MAX_FILE_VALUES {
            return false;
        }
    }
    true
}

/// `fs.readFileSync(file, 'utf8')`: the bytes as text, moved when they are UTF-8 (the usual case)
/// and replaced by their lossy decoding otherwise, so at most one copy is held at a time.
fn text_of(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

/// `JSON.parse(text)`, lone surrogates kept: `Ok(None)` where JSON.parse throws, `Err` where this
/// port cannot hold the value.
fn parse(text: &str) -> Result<Option<JValue>, ()> {
    match wtf16::parse(text) {
        Ok(v) => Ok(Some(v)),
        Err(wtf16::Refusal::Invalid) => Ok(None),
        Err(wtf16::Refusal::Unsupported) => Err(()),
    }
}

/// A metadata file's value: `Ok(None)` when it is missing; any refusal is a 500.
fn read_meta(file: &Path) -> Result<Option<JValue>, ()> {
    let Some(bytes) = read_capped(file, META_READ_CAP).map_err(|_| ())? else {
        return Ok(None);
    };
    let text = text_of(bytes);
    if !within_bounds(&text) {
        return Err(());
    }
    // Not JSON: Node would read an empty list (or keep its copy); refused here.
    parse(&text)?.ok_or(()).map(Some)
}

fn free_chats(dir: &Path) -> Result<JValue, ()> {
    match read_meta(&dir.join(server_projects::chats::FREE_CHATS_FILE))? {
        Some(v @ JValue::Arr(_)) => Ok(v),
        _ => Ok(JValue::Arr(Vec::new())),
    }
}

fn projects(dir: &Path) -> Result<JValue, ()> {
    let Some(parsed) = read_meta(&dir.join(server_projects::store::PROJECTS_FILE))? else {
        return Ok(JValue::Arr(Vec::new()));
    };
    let items = match take(parsed, "projects") {
        JValue::Undefined | JValue::Null => return Ok(JValue::Arr(Vec::new())),
        JValue::Arr(items) => items,
        _ => return Err(()),
    };
    let mut visible = Vec::with_capacity(items.len());
    for project in items {
        // The workspace load reads `project.provider` and sets `project.modes` (a TypeError on null
        // and on a primitive in strict mode); internalProject calls `project.id.startsWith`.
        let JValue::Obj(_) = project else {
            return Err(());
        };
        let Some(id) = project.get("id").as_str() else {
            return Err(());
        };
        if id == DIARY_EXTRAS || id.starts_with(CHAT_CONTEXT_PREFIX) {
            continue;
        }
        visible.push(project);
    }
    Ok(JValue::Arr(visible))
}

/// `value[key]`, moved out of an object (`undefined` for anything else: an array's or a
/// primitive's own `projects` / `history` is undefined).
fn take(value: JValue, key: &str) -> JValue {
    match value {
        JValue::Obj(items) => items
            .into_iter()
            .find(|(k, _)| k == key)
            .map_or(JValue::Undefined, |(_, v)| v),
        _ => JValue::Undefined,
    }
}

/// core chat-lists.cjs `safeChatId(id)`: `String(id ?? '')` reduced to `[A-Za-z0-9_-]`; `None`
/// where `String()` throws.
fn safe_chat_id(id: &JValue) -> Option<String> {
    let s = if id.is_nullish() {
        String::new()
    } else {
        js_json::to_js_string(id).ok()?
    };
    Some(
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect(),
    )
}

/// The history file Node's readHistory would read for this chat id, if any.
pub fn history_file(dir: &Path, id: &JValue) -> Option<PathBuf> {
    let safe = safe_chat_id(id)?;
    if safe.is_empty() {
        return None; // historyPath throws 'invalid chat id', inside readHistory's try
    }
    Some(dir.join(format!("history-{safe}.json")))
}

fn read_history(dir: &Path, id: &JValue, total: &mut u64) -> Result<JValue, HistoryFault> {
    let empty = || Ok(JValue::Arr(Vec::new()));
    let Some(file) = history_file(dir, id) else {
        return empty();
    };
    let bytes = match read_capped(&file, HISTORY_READ_CAP) {
        Ok(Some(b)) => b,
        Ok(None) | Err(ReadFault::Unreadable) => return empty(),
        Err(ReadFault::TooLarge) => return Err(HistoryFault::TooLarge),
        Err(ReadFault::NotRegular) => return Err(HistoryFault::Unsupported),
    };
    *total = total.saturating_add(bytes.len() as u64);
    if *total > TOTAL_READ_CAP {
        return Err(HistoryFault::TooLarge);
    }
    let text = text_of(bytes);
    if !within_bounds(&text) {
        return Err(HistoryFault::TooLarge);
    }
    let parsed = match parse(&text) {
        Ok(Some(v)) => v,
        Ok(None) => return empty(),
        Err(()) => return Err(HistoryFault::Unsupported),
    };
    drop(text);
    // `.history || []`; `null.history` throws inside the try (also []).
    let history = take(parsed, "history");
    if history.truthy() {
        Ok(history)
    } else {
        empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn chat_ids_never_leave_the_users_directory() {
        let dir = Path::new("/d/users/u");
        for (id, want) in [
            (
                JValue::Str("../../other/history-x".into()),
                "history-otherhistory-x.json",
            ),
            (JValue::Str("a/b\\c".into()), "history-abc.json"),
            (JValue::Num(12.0), "history-12.json"),
        ] {
            assert_eq!(history_file(dir, &id).unwrap(), dir.join(want));
        }
        for id in [
            JValue::Null,
            JValue::Undefined,
            JValue::Str("../..".into()),
            JValue::Str(String::new()),
        ] {
            assert_eq!(history_file(dir, &id), None);
        }
    }

    #[test]
    fn day_is_utc_and_bounded() {
        assert_eq!(day(0).unwrap(), "1970-01-01");
        assert_eq!(day(1_800_000_000_000).unwrap(), "2027-01-15");
        assert_eq!(day(i64::MAX), None);
    }
}
