//! noevia#1318: native, read-only `GET /api/v1/settings`.
//!
//! The Python services route stays the single writer (`PUT /api/v1/settings`). This side only
//! reads `DATA_DIR/model_loader.db` with a read-only connection and reports whether a Hugging
//! Face token is stored plus Python's hint, never the token itself:
//!
//! ```python
//! token = db.get_setting("hf_token", "")
//! return {"hasToken": bool(token), "tokenHint": f"…{token[-4:]}" if len(token) > 8 else ""}
//! ```
//!
//! `len` and slicing count code points, so this module does too. A store failure answers
//! exactly like Python's unhandled exception: `500 text/plain; charset=utf-8`
//! `Internal Server Error`. Deliberate differences (see docs/native-settings.md): the database
//! file is never created or converted to WAL (a read-only connection to a WAL database can
//! still create its `-shm`/`-wal` side files, as SQLite needs them to read), and a stored value over [`VALUE_CAP`] bytes is refused
//! instead of being read in full.
use rusqlite::{types::ValueRef, OptionalExtension};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Largest stored value read. Real Hugging Face tokens are a few dozen bytes.
pub const VALUE_CAP: i32 = 64 * 1024;
/// Python's sqlite3 waits 5 s for a lock; the native request budget is also 5 s, so give up a
/// little earlier and answer 500 (as Python does) instead of the front's 504.
pub const BUSY_WAIT: Duration = Duration::from_millis(4000);
const DEADLINE: Duration = Duration::from_millis(4500);

/// What `db.get_setting("hf_token", "")` hands to `get_settings`.
#[derive(Debug, PartialEq, Eq)]
pub enum Stored {
    Text(String),
    Blob(Vec<u8>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Json(Vec<u8>),
    /// Python raised: no table, unreadable file, lock timeout, NULL/number/invalid UTF-8 value.
    Failed,
}

pub struct Settings {
    database: PathBuf,
}

impl Settings {
    pub fn new(root: &Path) -> Self {
        Self {
            database: root.join("model_loader.db"),
        }
    }

    pub async fn response(&self) -> Outcome {
        let database = self.database.clone();
        let deadline = Instant::now() + DEADLINE;
        // noevia#2006: admitted: a dropped request keeps its permit until the read returns.
        match crate::docker::spawn_blocking_admitted(move || read(&database, deadline)).await {
            Ok(Ok(stored)) => render(stored.as_ref()).map_or(Outcome::Failed, Outcome::Json),
            _ => Outcome::Failed,
        }
    }
}

/// noevia#1922: every store error is logged once (no values), then answered as Python's 500.
fn unreadable(error: rusqlite::Error) -> Unreadable {
    crate::sqlite_read::log_once("settings", &error);
    Unreadable
}

/// Any reason Python's handler would have raised (or the native bound refused).
#[derive(Debug, PartialEq, Eq)]
pub struct Unreadable;

/// One bounded read-only lookup. `Ok(None)` is "no row" (Python's default "").
/// Read-only means no write to the database file itself; on a WAL database SQLite may still
/// create the `-shm`/`-wal` side files next to it.
pub fn read(database: &Path, deadline: Instant) -> Result<Option<Stored>, Unreadable> {
    // noevia#1922: read-write without CREATE plus query_only (a closed WAL store has no -shm).
    let db = crate::sqlite_read::open(database, "settings").map_err(unreadable)?;
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, VALUE_CAP)
        .map_err(unreadable)?;
    db.set_limit(
        rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH,
        crate::sqlite_read::SQL_CAP,
    )
    .map_err(unreadable)?;
    db.busy_timeout(BUSY_WAIT).map_err(unreadable)?;
    db.progress_handler(1000, Some(move || Instant::now() >= deadline))
        .map_err(unreadable)?;
    let mut statement = db
        .prepare("SELECT value FROM kv WHERE key = ?1")
        .map_err(unreadable)?;
    statement
        .query_row(["hf_token"], |row| {
            Ok(match row.get_ref(0)? {
                ValueRef::Text(bytes) => std::str::from_utf8(bytes)
                    .map(|text| Some(Stored::Text(text.to_owned())))
                    .unwrap_or(None),
                ValueRef::Blob(bytes) => Some(Stored::Blob(bytes.to_vec())),
                // NULL, INTEGER and REAL make Python's `len(token)` raise.
                ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => None,
            })
        })
        .optional()
        .map_err(unreadable)?
        .map_or(Ok(None), |value| value.map(Some).ok_or(Unreadable))
}

/// The exact bytes FastAPI renders (`json.dumps(ensure_ascii=False, separators=(",", ":"))`).
/// `None` when Python itself would raise. Never contains more than the last four code points.
pub fn render(stored: Option<&Stored>) -> Option<Vec<u8>> {
    let (has_token, hint) = match stored {
        None => (false, String::new()),
        Some(Stored::Text(token)) => (!token.is_empty(), text_hint(token)),
        Some(Stored::Blob(bytes)) => (!bytes.is_empty(), blob_hint(bytes)),
    };
    serde_json::to_vec(&serde_json::json!({"hasToken": has_token, "tokenHint": hint})).ok()
}

/// `f"…{token[-4:]}" if len(token) > 8 else ""`, counted in code points.
pub fn text_hint(token: &str) -> String {
    let count = token.chars().count();
    if count <= 8 {
        return String::new();
    }
    let tail: String = token.chars().skip(count - 4).collect();
    format!("…{tail}")
}

/// Python formats a bytes value with `repr`, e.g. `…b'blob'`. Only a hand-edited store holds
/// one (the writer stores `str`); matched exactly so both owners agree byte for byte.
fn blob_hint(bytes: &[u8]) -> String {
    let Some(tail) = bytes
        .len()
        .checked_sub(4)
        .and_then(|start| bytes.get(start..))
    else {
        return String::new();
    };
    if bytes.len() <= 8 {
        return String::new();
    }
    let quote = if tail.contains(&b'\'') && !tail.contains(&b'"') {
        b'"'
    } else {
        b'\''
    };
    let mut out = format!("…b{}", quote as char);
    for &b in tail {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            _ if b == quote => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out.push(quote as char);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn hint_counts_code_points_not_bytes() {
        assert_eq!(text_hint(""), "");
        assert_eq!(text_hint("abcdefgh"), "");
        assert_eq!(text_hint("abcdefghi"), "…fghi");
        assert_eq!(text_hint(&"\u{1F600}".repeat(8)), "");
        assert_eq!(
            text_hint("a\u{1F600}\u{1F601}\u{1F602}\u{1F603}\u{1F600}\u{1F601}\u{1F602}\u{1F603}"),
            "…\u{1F600}\u{1F601}\u{1F602}\u{1F603}"
        );
    }

    #[test]
    fn hint_never_reveals_more_than_four_code_points() {
        let token = "hf_SYNTHETICsecretMaterial0123456789";
        let body = String::from_utf8(render(Some(&Stored::Text(token.into()))).unwrap()).unwrap();
        assert!(!body.contains("SYNTHETIC"));
        assert_eq!(body, r#"{"hasToken":true,"tokenHint":"…6789"}"#);
    }

    #[test]
    fn blob_hint_matches_python_bytes_repr() {
        assert_eq!(blob_hint(b"synthetic-blob"), "…b'blob'");
        assert_eq!(blob_hint(b"synthetic\x00'\\\n"), "…b\"\\x00'\\\\\\n\"");
        assert_eq!(blob_hint(b"synthetic'\"\xff\t"), "…b'\\'\"\\xff\\t'");
        assert_eq!(blob_hint(b"12345678"), "");
    }

    /// noevia#2006: a request dropped while the store is read (the client went away) keeps its
    /// admission permit until the read has returned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_read_keeps_its_permit_until_the_read_ends() {
        use crate::test_support::{all_permits_back, dropped_in_blocking_work, test_state};
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(crate::LEGACY_BASE);
        state.settings = Some(std::sync::Arc::new(Settings::new(dir.path())));
        let state = std::sync::Arc::new(state);
        let request = axum::http::Request::get("/api/v1/settings")
            .body(axum::body::Body::empty())
            .unwrap();
        let open = dropped_in_blocking_work(&state, request).await;
        open.send(()).unwrap();
        all_permits_back(&state).await;
    }
}
