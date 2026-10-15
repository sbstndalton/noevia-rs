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
//! Narrower admission than Node, as refusals, never as different bytes:
//! - a free-chats.json that is not JSON, or a projects.json whose `projects` is not a list or that
//!   is not JSON (Node at load would export an empty list, or keep its earlier copy): 500;
//! - a value this port cannot hold (js-json `Refusal::Unsupported`): 500;
//! - a metadata file over [`META_READ_CAP`]: 500; a history file over [`HISTORY_READ_CAP`], or
//!   more than [`TOTAL_READ_CAP`] of history read in one export, or a chat over the Markdown
//!   foundation's per-chat bounds: Node's 413 (`too large to export`), whose limit it is in spirit.
use crate::assemble::{self, Limits};
use js_json::wtf16;
use js_json::JValue;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The largest free-chats.json or projects.json read.
pub const META_READ_CAP: u64 = 64 * 1024 * 1024;
/// The largest single history file read.
pub const HISTORY_READ_CAP: u64 = 64 * 1024 * 1024;
/// The most history bytes read for one export.
pub const TOTAL_READ_CAP: u64 = 512 * 1024 * 1024;

const DIARY_EXTRAS: &str = "cowork-diary-extras";
const CHAT_CONTEXT_PREFIX: &str = "cowork-chat-context-";

/// What the route answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 200, `application/zip`; `day` is `new Date(when).toISOString().slice(0, 10)`.
    Zip { bytes: Vec<u8>, day: String },
    /// 413 `{"error": message}`; nothing is audited or sent.
    TooLarge(String),
    /// 500 `{"error":"Internal error"}` (core http.cjs errorResponse for a thrown error).
    Internal,
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
    let built = assemble::build_export(
        &free,
        &projects,
        |id| read_history(&dir, id, &mut read_total),
        now_ms,
        limits,
    );
    let too_large = || Outcome::TooLarge(assemble::too_large_message(limits));
    match built {
        Ok(bytes) => Outcome::Zip { bytes, day },
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

/// The file's bytes up to `cap`: `Ok(None)` when it is missing, `Err(true)` when it is larger,
/// `Err(false)` for any other read failure.
fn read_capped(file: &Path, cap: u64) -> Result<Option<Vec<u8>>, bool> {
    let f = match std::fs::File::open(file) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(false),
    };
    let mut bytes = Vec::new();
    f.take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| false)?;
    if bytes.len() as u64 > cap {
        return Err(true);
    }
    Ok(Some(bytes))
}

/// `JSON.parse(fs.readFileSync(file, 'utf8'))`, lone surrogates kept: `Ok(None)` where JSON.parse
/// throws, `Err` where this port cannot hold the value.
fn parse(bytes: &[u8]) -> Result<Option<JValue>, ()> {
    match wtf16::parse(&String::from_utf8_lossy(bytes)) {
        Ok(v) => Ok(Some(v)),
        Err(wtf16::Refusal::Invalid) => Ok(None),
        Err(wtf16::Refusal::Unsupported) => Err(()),
    }
}

fn free_chats(dir: &Path) -> Result<JValue, ()> {
    let file = dir.join(server_projects::chats::FREE_CHATS_FILE);
    let Some(bytes) = read_capped(&file, META_READ_CAP).map_err(|_| ())? else {
        return Ok(JValue::Arr(Vec::new()));
    };
    match parse(&bytes)? {
        None => Err(()),
        Some(v @ JValue::Arr(_)) => Ok(v),
        Some(_) => Ok(JValue::Arr(Vec::new())),
    }
}

fn projects(dir: &Path) -> Result<JValue, ()> {
    let file = dir.join(server_projects::store::PROJECTS_FILE);
    let Some(bytes) = read_capped(&file, META_READ_CAP).map_err(|_| ())? else {
        return Ok(JValue::Arr(Vec::new()));
    };
    let parsed = parse(&bytes)?.ok_or(())?;
    let list = match &parsed {
        JValue::Obj(_) | JValue::Arr(_) => parsed.get("projects"),
        _ => return Ok(JValue::Arr(Vec::new())),
    };
    let items = match list {
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
        visible.push(project.clone());
    }
    Ok(JValue::Arr(visible))
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
        Ok(None) | Err(false) => return empty(),
        Err(true) => return Err(HistoryFault::TooLarge),
    };
    *total = total.saturating_add(bytes.len() as u64);
    if *total > TOTAL_READ_CAP {
        return Err(HistoryFault::TooLarge);
    }
    let parsed = match parse(&bytes) {
        Ok(Some(v)) => v,
        Ok(None) => return empty(),
        Err(()) => return Err(HistoryFault::Unsupported),
    };
    // `.history || []`; `null.history` throws inside the try (also []).
    let history = parsed.get("history");
    if history.truthy() {
        Ok(history.clone())
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
