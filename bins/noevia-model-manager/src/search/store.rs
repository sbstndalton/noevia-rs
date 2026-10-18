//! The three things the search reads from, and writes to, `DATA_DIR/model_loader.db`: the
//! `trusted_quantisers` setting (`db.get_setting`), the owner avatar cache (`db.get_avatar`,
//! `db.set_avatar`). The connection is opened as Python opens it (read-write, no create, a 5 s
//! busy wait); every statement commits on its own.
use rusqlite::{types::ValueRef, Connection, OpenFlags, OptionalExtension};
use std::{path::Path, time::Duration};

/// sqlite3's default `timeout=5.0`.
const BUSY_WAIT: Duration = Duration::from_millis(5000);

/// A store error: Python's handler would have raised.
#[derive(Debug, PartialEq, Eq)]
pub struct Unreadable;

/// What sqlite3 hands Python for one column.
#[derive(Debug, Clone, PartialEq)]
pub enum Stored {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

fn logged(error: &rusqlite::Error) -> Unreadable {
    crate::sqlite_read::log_once("search", error);
    Unreadable
}

pub fn open(database: &Path) -> Result<Connection, Unreadable> {
    let db = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| logged(&e))?;
    db.set_limit(
        rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH,
        crate::sqlite_read::SQL_CAP,
    )
    .map_err(|e| logged(&e))?;
    db.busy_timeout(BUSY_WAIT).map_err(|e| logged(&e))?;
    Ok(db)
}

/// TEXT that is not UTF-8 raises in Python.
fn stored(value: ValueRef<'_>) -> Result<Stored, Unreadable> {
    Ok(match value {
        ValueRef::Null => Stored::Null,
        ValueRef::Integer(n) => Stored::Int(n),
        ValueRef::Real(f) => Stored::Real(f),
        ValueRef::Text(t) => {
            Stored::Text(std::str::from_utf8(t).map_err(|_| Unreadable)?.to_owned())
        }
        ValueRef::Blob(b) => Stored::Blob(b.to_vec()),
    })
}

/// `SELECT value FROM kv WHERE key = ?`.
pub fn setting(db: &Connection, key: &str) -> Result<Option<Stored>, Unreadable> {
    db.query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| {
        Ok(stored(row.get_ref(0)?))
    })
    .optional()
    .map_err(|e| logged(&e))?
    .transpose()
}

/// `SELECT url, fetched_at FROM avatar_cache WHERE owner = ?`.
pub fn avatar(db: &Connection, owner: &str) -> Result<Option<(Stored, Stored)>, Unreadable> {
    db.query_row(
        "SELECT url, fetched_at FROM avatar_cache WHERE owner = ?1",
        [owner],
        |row| Ok((stored(row.get_ref(0)?), stored(row.get_ref(1)?))),
    )
    .optional()
    .map_err(|e| logged(&e))?
    .map(|(url, fetched)| Ok((url?, fetched?)))
    .transpose()
}

/// `INSERT INTO avatar_cache ... ON CONFLICT(owner) DO UPDATE`.
pub fn set_avatar(
    db: &Connection,
    owner: &str,
    url: &str,
    fetched_at: f64,
) -> Result<(), Unreadable> {
    db.execute(
        "INSERT INTO avatar_cache(owner, url, fetched_at) VALUES(?1, ?2, ?3) \
         ON CONFLICT(owner) DO UPDATE SET url = excluded.url, fetched_at = excluded.fetched_at",
        rusqlite::params![owner, url, fetched_at],
    )
    .map(|_| ())
    .map_err(|e| logged(&e))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn store() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model_loader.db");
        let c = Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE avatar_cache (owner TEXT PRIMARY KEY, url TEXT NOT NULL, fetched_at REAL NOT NULL);",
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn avatars_round_trip_and_update() {
        let (_dir, path) = store();
        let db = open(&path).unwrap();
        assert_eq!(avatar(&db, "o").unwrap(), None);
        set_avatar(&db, "o", "https://x/a.png", 5.5).unwrap();
        set_avatar(&db, "p", "", 6.0).unwrap();
        assert_eq!(
            avatar(&db, "o").unwrap(),
            Some((Stored::Text("https://x/a.png".into()), Stored::Real(5.5)))
        );
        set_avatar(&db, "o", "", 7.0).unwrap();
        assert_eq!(
            avatar(&db, "o").unwrap(),
            Some((Stored::Text(String::new()), Stored::Real(7.0)))
        );
    }

    #[test]
    fn settings_keep_their_type() {
        let (_dir, path) = store();
        let db = open(&path).unwrap();
        assert_eq!(setting(&db, "k").unwrap(), None);
        db.execute("INSERT INTO kv VALUES('k', 7)", []).unwrap();
        db.execute("INSERT INTO kv VALUES('t', 'a,b')", []).unwrap();
        db.execute("INSERT INTO kv VALUES('b', x'00ff')", [])
            .unwrap();
        // A TEXT column keeps a number as its text.
        assert_eq!(setting(&db, "k").unwrap(), Some(Stored::Text("7".into())));
        assert_eq!(setting(&db, "t").unwrap(), Some(Stored::Text("a,b".into())));
        assert_eq!(setting(&db, "b").unwrap(), Some(Stored::Blob(vec![0, 255])));
    }

    #[test]
    fn a_missing_store_is_an_error_not_created() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open(&dir.path().join("model_loader.db")).is_err());
        assert!(!dir.path().join("model_loader.db").exists());
    }
}
