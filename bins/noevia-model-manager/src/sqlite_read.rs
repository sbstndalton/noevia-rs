//! noevia#1922: how native readers open `DATA_DIR/model_loader.db`.
//!
//! Python's `db._conn()` sets `journal_mode=WAL` on every connection, so the live store is a WAL
//! database. When Python holds no connection its `-wal`/`-shm` files are gone, and SQLite cannot
//! open a WAL database with `SQLITE_OPEN_READ_ONLY` unless it can find or create `-shm`
//! (SQLITE_CANTOPEN). Every native read then answered a bare 500.
//!
//! Readers therefore open read-write **without** `SQLITE_OPEN_CREATE` (a missing store is still
//! an error, never created), as Python's own connection does, and set `PRAGMA query_only` so the
//! connection cannot write. The journal mode is never changed. Only when the read-write open is
//! refused (a read-only directory or file) is a plain read-only open tried, which works for a
//! rollback-journal store or a WAL store whose `-shm` exists. The SQLite error of a failed open is
//! logged once per reader, so a refusal is never a silent 500.
use rusqlite::{Connection, OpenFlags};
use std::{collections::HashSet, path::Path, sync::Mutex};

static LOGGED: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);

/// Log `error` for `reader` the first time only.
pub fn log_once(reader: &'static str, error: &dyn std::fmt::Display) {
    let first = LOGGED
        .lock()
        .map(|mut seen| seen.get_or_insert_with(HashSet::new).insert(reader))
        .unwrap_or(false);
    if first {
        eprintln!("model-manager {reader}: model_loader.db could not be read: {error}");
    }
}

/// A connection that reads `database` and never writes, creates or converts it.
pub fn open(database: &Path, reader: &'static str) -> rusqlite::Result<Connection> {
    let attempt = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .or_else(|error| match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::CannotOpen | rusqlite::ErrorCode::ReadOnly) => {
            Connection::open_with_flags(
                database,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
        }
        _ => Err(error),
    })
    .and_then(|db| {
        db.pragma_update(None, "query_only", true)?;
        Ok(db)
    });
    if let Err(error) = &attempt {
        log_once(reader, error);
    }
    attempt
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A store as Python leaves it: WAL mode, every connection closed (no -wal/-shm).
    pub(crate) fn python_wal_store(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("model_loader.db");
        let c = Connection::open(&path).unwrap();
        let mode: String = c
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        c.execute_batch("CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL); INSERT INTO kv VALUES('k','v');")
            .unwrap();
        drop(c);
        assert!(!dir.join("model_loader.db-wal").exists());
        assert!(!dir.join("model_loader.db-shm").exists());
        let header = std::fs::read(&path).unwrap();
        assert_eq!(header.get(18..20), Some(&[2u8, 2][..]));
        path
    }

    #[test]
    fn plain_read_only_open_fails_on_a_closed_wal_store_in_a_read_only_dir() {
        // Documents the live failure: with no -shm and no way to create it, READ_ONLY fails.
        let dir = tempfile::tempdir().unwrap();
        let path = python_wal_store(dir.path());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let ro = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|c| c.query_row("SELECT value FROM kv", [], |r| r.get::<_, String>(0)));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ro.is_err());
    }

    #[test]
    fn reads_a_closed_wal_store_without_writing_or_converting_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = python_wal_store(dir.path());
        let db = open(&path, "test").unwrap();
        let v: String = db
            .query_row("SELECT value FROM kv", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "v");
        assert!(db.execute("INSERT INTO kv VALUES('x','y')", []).is_err());
        drop(db);
        let c = Connection::open(&path).unwrap();
        let mode: String = c
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn read_only_store_file_falls_back_to_a_read_only_open() {
        // models_loader.db itself not writable (dir writable): the read-write open is refused,
        // the read-only open creates the -shm it needs and reads.
        let dir = tempfile::tempdir().unwrap();
        let path = python_wal_store(dir.path());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let read = open(&path, "test-ro-file")
            .and_then(|db| db.query_row("SELECT value FROM kv", [], |r| r.get::<_, String>(0)));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read.unwrap(), "v");
    }

    #[test]
    fn never_creates_a_missing_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model_loader.db");
        assert!(open(&path, "test-missing").is_err());
        assert!(!path.exists());
    }

    #[test]
    fn read_only_directory_with_existing_shm_still_reads() {
        // A read-only /data where Python's connection is open (its -shm exists).
        let dir = tempfile::tempdir().unwrap();
        let path = python_wal_store(dir.path());
        let holder = Connection::open(&path).unwrap();
        let _: i64 = holder
            .query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0))
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let read = open(&path, "test-ro")
            .and_then(|db| db.query_row("SELECT value FROM kv", [], |r| r.get::<_, String>(0)));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        drop(holder);
        assert_eq!(read.unwrap(), "v");
    }
}
