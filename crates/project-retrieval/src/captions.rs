//! The image captions a project index caches (#1726): core `rag.cjs` `readCaption` and
//! `writeCaption`, over the `media_captions` table beside the index's image rows (one caption per
//! image, keyed by the image bytes' hash). Nothing calls this yet: the chat turn that makes captions
//! is Node's (`chat.cjs`, `multimodal.captionOnce`); a native turn reads and stores them here.
//!
//! What the caller checks first, as Node's `openIndex` does before either call: retrieval can run
//! (the vector extension registers), the account exists and the project has no tombstone in its
//! index outbox. Which process may write is the caller's too (the index has one writer).
//!
//! Named differences from Node:
//! - [`read_at`] opens the index read-only ([`crate::search::open_index`]): it never creates the
//!   index's directory and never upgrades an older index's schema, which Node's `openIndex` does on
//!   every read (the columns a read here needs are only `media_captions`').
//! - a stored caption that is not text (only a foreign writer could store one) reads as no caption;
//!   Node hands back whatever value the column holds.
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

/// `String(caption).slice(0, 4000)`.
pub const CAPTION_UNITS: usize = 4000;
/// `String(model).slice(0, 200)`.
pub const MODEL_UNITS: usize = 200;

/// A cached caption: `{ caption, model }`, `model` `null` when none was stored (or an empty one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caption {
    pub caption: String,
    pub model: Option<String>,
}

fn has_captions(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE name = 'media_captions'",
        [],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
}

/// `readCaption` over an open index: the caption cached for `content_hash`, or `None` (no table,
/// no row, or any SQL failure: Node's `catch { return null; }`).
pub fn read(conn: &Connection, content_hash: &str) -> Option<Caption> {
    if !has_captions(conn).ok()? {
        return None;
    }
    let row = conn
        .query_row(
            "SELECT caption, model FROM media_captions WHERE content_hash = ?",
            [content_hash],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .ok()??;
    Some(Caption {
        caption: row.0,
        // `row.model || null`.
        model: row.1.filter(|m| !m.is_empty()),
    })
}

/// `readCaption` from the index file at `path`: `None` when there is no file (a read never creates
/// an index, #1776) or it cannot be opened.
pub fn read_at(path: &Path, content_hash: &str) -> Option<Caption> {
    let conn = crate::search::open_index(path).ok()??;
    read(&conn, content_hash)
}

/// JS `s.slice(0, n)` over UTF-16 units, as SQLite then stores it: a surrogate pair cut in half
/// leaves a lone high surrogate, which the driver writes as U+FFFD (V8's UTF-8 conversion).
pub fn js_slice(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut units = 0usize;
    for c in s.chars() {
        let w = c.len_utf16();
        if units + w > n {
            if units < n {
                out.push('\u{fffd}');
            }
            break;
        }
        units += w;
        out.push(c);
    }
    out
}

/// `writeCaption` over a writable index connection: stores `caption` (and `model`) for an image
/// this index holds and answers `true`; `false`, with nothing stored and no table made, for a hash
/// no image row has, or on any SQL failure. A stored caption is replaced (and its `created_at`
/// renewed, `INSERT OR REPLACE`).
pub fn write(conn: &Connection, content_hash: &str, caption: &str, model: Option<&str>) -> bool {
    let image = conn
        .query_row(
            "SELECT 1 FROM chunks WHERE kind = 'image' AND content_hash = ?",
            [content_hash],
            |_| Ok(()),
        )
        .optional();
    if !matches!(image, Ok(Some(()))) {
        return false;
    }
    // `ensureCaptions(db)`.
    if conn
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS media_captions (content_hash TEXT PRIMARY KEY, caption TEXT NOT NULL, model TEXT, created_at TEXT NOT NULL DEFAULT (datetime('now')))",
        )
        .is_err()
    {
        return false;
    }
    // `model ? String(model).slice(0, 200) : null`: an empty model is no model.
    let model = model
        .filter(|m| !m.is_empty())
        .map(|m| js_slice(m, MODEL_UNITS));
    conn.execute(
        "INSERT OR REPLACE INTO media_captions (content_hash, caption, model) VALUES (?, ?, ?)",
        (content_hash, js_slice(caption, CAPTION_UNITS), model),
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// The index tables Node's `openIndex` makes (chunks with the #1726 `kind` and `meta` columns).
    fn index() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qa-project.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE chunks (id INTEGER PRIMARY KEY, file TEXT NOT NULL, chunk_no INTEGER NOT NULL,
               body TEXT NOT NULL, content_hash TEXT NOT NULL, updated_at TEXT NOT NULL DEFAULT (datetime('now')),
               version TEXT, direct INTEGER, kind TEXT, meta TEXT);
             INSERT INTO chunks (file, chunk_no, body, content_hash, kind) VALUES
               ('QA chart.png', 0, 'QA chart', 'qa-image-hash', 'image'),
               ('QA notes.txt', 0, 'QA notes', 'qa-text-hash', NULL),
               ('QA doc.pdf', 1, 'QA page', 'qa-text-hash-2', 'text');",
        )
        .unwrap();
        (dir, path, conn)
    }

    #[test]
    fn no_table_no_row_no_file_read_as_no_caption() {
        let (dir, path, conn) = index();
        assert_eq!(read(&conn, "qa-image-hash"), None);
        assert!(!has_captions(&conn).unwrap(), "a read makes no table");
        assert_eq!(read_at(&path, "qa-image-hash"), None);
        let missing = dir.path().join("rag").join("qa-missing.db");
        assert_eq!(read_at(&missing, "qa-image-hash"), None);
        assert!(
            !dir.path().join("rag").exists(),
            "a read never creates an index"
        );
    }

    #[test]
    fn a_caption_is_stored_only_for_an_image_the_index_holds() {
        let (_dir, _path, conn) = index();
        for hash in ["qa-text-hash", "qa-text-hash-2", "qa-unknown"] {
            assert!(!write(&conn, hash, "QA caption", Some("qa-vision")));
        }
        assert!(
            !has_captions(&conn).unwrap(),
            "a refused write makes no table"
        );
        assert!(write(&conn, "qa-image-hash", "QA caption", Some("qa-vision")));
        assert_eq!(
            read(&conn, "qa-image-hash"),
            Some(Caption {
                caption: "QA caption".into(),
                model: Some("qa-vision".into())
            })
        );
        // Another hash of the same index still has none.
        assert_eq!(read(&conn, "qa-text-hash"), None);
    }

    #[test]
    fn a_write_replaces_and_an_empty_model_is_null() {
        let (_dir, path, conn) = index();
        assert!(write(&conn, "qa-image-hash", "QA first", Some("qa-vision")));
        assert!(write(&conn, "qa-image-hash", "QA second", Some("")));
        let stored: Option<String> = conn
            .query_row(
                "SELECT model FROM media_captions WHERE content_hash = 'qa-image-hash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, None, "`model ? ... : null`");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_captions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(conn);
        // Read back through the read-only open, as a turn would.
        assert_eq!(
            read_at(&path, "qa-image-hash"),
            Some(Caption {
                caption: "QA second".into(),
                model: None
            })
        );
        assert!(write(
            &Connection::open(&path).unwrap(),
            "qa-image-hash",
            "QA third",
            None
        ));
        assert_eq!(
            read_at(&path, "qa-image-hash").map(|c| c.caption),
            Some("QA third".into())
        );
    }

    #[test]
    fn a_stored_empty_model_reads_as_null() {
        // `row.model || null`: a foreign writer's empty string reads as no model.
        let (_dir, _path, conn) = index();
        conn.execute_batch(
            "CREATE TABLE media_captions (content_hash TEXT PRIMARY KEY, caption TEXT NOT NULL, model TEXT, created_at TEXT NOT NULL DEFAULT (datetime('now')));
             INSERT INTO media_captions (content_hash, caption, model) VALUES ('qa-image-hash', 'QA', '');",
        )
        .unwrap();
        assert_eq!(
            read(&conn, "qa-image-hash"),
            Some(Caption {
                caption: "QA".into(),
                model: None
            })
        );
    }

    #[test]
    fn caption_and_model_are_cut_in_utf16_units() {
        assert_eq!(js_slice("abc", 2), "ab");
        assert_eq!(js_slice("abc", 3), "abc");
        assert_eq!(js_slice("", 3), "");
        // "é" is one unit, "😀" two: a pair cut in half leaves its high surrogate, stored as U+FFFD.
        assert_eq!(js_slice("é😀", 2), "é\u{fffd}");
        assert_eq!(js_slice("é😀", 3), "é😀");
        assert_eq!(js_slice("😀😀", 1), "\u{fffd}");
        let (_dir, _path, conn) = index();
        let long = format!("{}😀tail", "Q".repeat(CAPTION_UNITS - 1));
        let model = "m".repeat(MODEL_UNITS + 50);
        assert!(write(&conn, "qa-image-hash", &long, Some(&model)));
        let got = read(&conn, "qa-image-hash").unwrap();
        assert_eq!(
            got.caption,
            format!("{}\u{fffd}", "Q".repeat(CAPTION_UNITS - 1))
        );
        assert_eq!(got.model.unwrap().len(), MODEL_UNITS);
    }

    #[test]
    fn a_damaged_index_reads_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qa-damaged.db");
        std::fs::write(&path, b"not a database at all, just QA bytes").unwrap();
        assert_eq!(read_at(&path, "qa-image-hash"), None);
        // An index without the chunks table (never one of Node's) refuses the write.
        let conn = Connection::open_in_memory().unwrap();
        assert!(!write(&conn, "qa-image-hash", "QA", None));
    }
}
