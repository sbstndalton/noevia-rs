//! Cross-process publication mutex shared with Core's models.ini writer.
//!
//! The SQLite transaction is only an OS-managed lock: model contents stay in models.ini.
//! Hold it on a blocking thread from the revision read through rename and directory fsync.
//! Closing the connection (including process death) releases it; it is never expired or stolen.
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn lock_path(file: &Path) -> Option<PathBuf> {
    let name = file.file_name()?.to_str()?;
    Some(file.with_file_name(format!(".{name}.noevia-publication.sqlite")))
}

pub struct Guard {
    _connection: rusqlite::Connection,
}

impl Guard {
    pub fn acquire(file: &Path) -> std::io::Result<Self> {
        let path = lock_path(file).ok_or_else(|| std::io::Error::other("invalid preset path"))?;
        Self::at(&path)
    }
}

/// Authority identity carried by a destructive operation to its destination.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Fence {
    pub incarnation: String,
    pub lease: String,
    pub generation: u64,
    pub operation: String,
    #[serde(default = "legacy_kind")]
    pub kind: String,
    #[serde(default, rename = "expectedRevision")]
    pub expected_revision: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AuthorityRecord {
    pub incarnation: String,
    pub lease: String,
    pub generation: u64,
    pub active: bool,
    pub until_ms: u64,
    pub recovery: bool,
}

fn legacy_kind() -> String {
    "legacy".into()
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Receipt {
    pub state: String,
    pub revision: Option<String>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl Guard {
    pub fn at(path: &Path) -> std::io::Result<Self> {
        let connection = rusqlite::Connection::open(path).map_err(io_error)?;
        share_reader_files(path)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(io_error)?;
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let opened = connection
                .pragma_update(None, "journal_mode", "WAL")
                .and_then(|()| connection.pragma_update(None, "synchronous", "FULL"))
                .and_then(|()| connection.execute_batch("BEGIN EXCLUSIVE"));
            match opened {
                Ok(()) => break,
                Err(rusqlite::Error::SqliteFailure(code, _))
                    if matches!(
                        code.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    ) && std::time::Instant::now() < until =>
                {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(e) => return Err(io_error(e)),
            }
        }
        share_reader_files(path)?;
        Ok(Self {
            _connection: connection,
        })
    }

    pub fn authority(&self) -> std::io::Result<Option<AuthorityRecord>> {
        use rusqlite::OptionalExtension;
        self._connection.execute_batch("CREATE TABLE IF NOT EXISTS preset_authority (singleton INTEGER PRIMARY KEY CHECK(singleton=1), incarnation TEXT NOT NULL, lease TEXT NOT NULL, generation TEXT NOT NULL, active INTEGER NOT NULL, until_ms TEXT NOT NULL, recovery INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS preset_operations (id TEXT PRIMARY KEY, incarnation TEXT NOT NULL, lease TEXT NOT NULL, generation TEXT NOT NULL); CREATE TABLE IF NOT EXISTS preset_revision(singleton INTEGER PRIMARY KEY CHECK(singleton=1),revision TEXT NOT NULL);").map_err(io_error)?;
        let columns = {
            let mut statement = self
                ._connection
                .prepare("PRAGMA table_info(preset_operations)")
                .map_err(io_error)?;
            let rows = statement
                .query_map([], |r| r.get::<_, String>(1))
                .map_err(io_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(io_error)?
        };
        for (name, definition) in [
            ("kind", "TEXT NOT NULL DEFAULT 'legacy'"),
            ("expected_revision", "TEXT"),
            ("state", "TEXT NOT NULL DEFAULT 'issued'"),
            ("revision", "TEXT"),
            ("created_ms", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            if !columns.iter().any(|column| column == name) {
                self._connection
                    .execute_batch(&format!(
                        "ALTER TABLE preset_operations ADD COLUMN {name} {definition}"
                    ))
                    .map_err(io_error)?;
            }
        }
        self._connection
            .pragma_update(None, "user_version", 2)
            .map_err(io_error)?;
        self._connection.query_row("SELECT incarnation,lease,CAST(generation AS TEXT),active,CAST(until_ms AS TEXT),recovery FROM preset_authority WHERE singleton=1", [], |r| Ok(AuthorityRecord { incarnation:r.get(0)?,lease:r.get(1)?,generation:r.get::<_,String>(2)?.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,active:r.get(3)?,until_ms:r.get::<_,String>(4)?.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,recovery:r.get(5)? })).optional().map_err(io_error)
    }

    pub fn save_authority(&self, r: &AuthorityRecord) -> std::io::Result<()> {
        self._connection.execute("INSERT INTO preset_authority VALUES(1,?1,?2,?3,?4,?5,?6) ON CONFLICT(singleton) DO UPDATE SET incarnation=excluded.incarnation,lease=excluded.lease,generation=excluded.generation,active=excluded.active,until_ms=excluded.until_ms,recovery=excluded.recovery", rusqlite::params![r.incarnation,r.lease,r.generation.to_string(),r.active,r.until_ms.to_string(),r.recovery]).map_err(io_error)?;
        Ok(())
    }

    pub fn pending(&self) -> std::io::Result<bool> {
        self._connection
            .query_row("SELECT EXISTS(SELECT 1 FROM preset_operations WHERE state IN ('issued','running','recovery'))", [], |r| {
                r.get(0)
            })
            .map_err(io_error)
    }

    pub fn begin(&self, f: &Fence) -> std::io::Result<()> {
        self.validate_lease(f)?;
        self._connection.execute("INSERT INTO preset_operations(id,incarnation,lease,generation,kind,expected_revision,state,created_ms) VALUES(?1,?2,?3,?4,?5,?6,'issued',?7)", rusqlite::params![f.operation,f.incarnation,f.lease,f.generation.to_string(),f.kind,f.expected_revision,now_ms().to_string()]).map_err(io_error)?;
        Ok(())
    }

    fn validate_lease(&self, f: &Fence) -> std::io::Result<()> {
        let record = self.authority()?.ok_or_else(conflict)?;
        if !record.active
            || record.incarnation != f.incarnation
            || record.lease != f.lease
            || record.generation != f.generation
            || now_ms() >= record.until_ms
        {
            return Err(conflict());
        }
        Ok(())
    }

    pub fn validate(&self, f: &Fence) -> std::io::Result<()> {
        self.validate_lease(f)?;
        let valid: bool = self._connection.query_row("SELECT EXISTS(SELECT 1 FROM preset_operations WHERE id=?1 AND incarnation=?2 AND lease=?3 AND generation=?4 AND kind=?5 AND expected_revision IS ?6 AND state IN ('issued','running'))", rusqlite::params![f.operation,f.incarnation,f.lease,f.generation.to_string(),f.kind,f.expected_revision], |r| r.get(0)).map_err(io_error)?;
        if valid {
            Ok(())
        } else {
            Err(conflict())
        }
    }

    pub fn no_other_publishers(&self, f: &Fence) -> std::io::Result<()> {
        let pending:bool=self._connection.query_row("SELECT EXISTS(SELECT 1 FROM preset_operations WHERE id<>?1 AND kind IN ('pull','register') AND state IN ('issued','running','recovery'))",[&f.operation],|r|r.get(0)).map_err(io_error)?;
        if pending {
            Err(conflict())
        } else {
            Ok(())
        }
    }
    pub fn pending_kinds(&self) -> std::io::Result<Vec<String>> {
        let mut statement=self._connection.prepare("SELECT DISTINCT kind FROM preset_operations WHERE state IN ('issued','running','recovery')").map_err(io_error)?;
        let rows = statement.query_map([], |r| r.get(0)).map_err(io_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(io_error)
    }
    pub fn observe_revision(&self, revision: &str) -> std::io::Result<()> {
        self._connection.execute("INSERT INTO preset_revision VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET revision=excluded.revision", [revision]).map_err(io_error)?;
        Ok(())
    }

    pub fn running(&self, f: &Fence) -> std::io::Result<()> {
        self.validate(f)?;
        self._connection
            .execute(
                "UPDATE preset_operations SET state='running' WHERE id=?1 AND state='issued'",
                [&f.operation],
            )
            .map_err(io_error)?;
        Ok(())
    }
    pub fn finish(&self, f: &Fence, state: &str, revision: Option<&str>) -> std::io::Result<()> {
        if !matches!(state, "settled" | "conflict" | "recovery") {
            return Err(conflict());
        }
        self._connection.execute("UPDATE preset_operations SET state=?5,revision=?6 WHERE id=?1 AND incarnation=?2 AND lease=?3 AND generation=?4 AND kind=?7 AND expected_revision IS ?8 AND state IN ('issued','running','recovery')", rusqlite::params![f.operation,f.incarnation,f.lease,f.generation.to_string(),state,revision,f.kind,f.expected_revision]).map_err(io_error)?;
        Ok(())
    }
    pub fn settle(&self, f: &Fence) -> std::io::Result<()> {
        self.finish(f, "settled", None)
    }
    pub fn status(&self, f: &Fence) -> std::io::Result<Option<Receipt>> {
        use rusqlite::OptionalExtension;
        self._connection.query_row("SELECT state,revision FROM preset_operations WHERE id=?1 AND incarnation=?2 AND lease=?3 AND generation=?4 AND kind=?5 AND expected_revision IS ?6", rusqlite::params![f.operation,f.incarnation,f.lease,f.generation.to_string(),f.kind,f.expected_revision], |r| Ok(Receipt {state:r.get(0)?,revision:r.get(1)?})).optional().map_err(io_error)
    }
    pub fn expected(&self, f: &Fence, actual: &str) -> std::io::Result<()> {
        self.validate(f)?;
        if f.expected_revision
            .as_deref()
            .is_some_and(|expected| expected != actual)
        {
            return Err(conflict());
        }
        Ok(())
    }

    pub fn operation_exists(&self, f: &Fence) -> std::io::Result<bool> {
        self._connection.query_row("SELECT EXISTS(SELECT 1 FROM preset_operations WHERE id=?1 AND incarnation=?2 AND lease=?3 AND generation=?4 AND state IN ('issued','running','recovery'))", rusqlite::params![f.operation,f.incarnation,f.lease,f.generation.to_string()], |r| r.get(0)).map_err(io_error)
    }

    pub fn clear_operations(&self) -> std::io::Result<()> {
        self._connection
            .execute("UPDATE preset_operations SET state='reconciled' WHERE state IN ('issued','running','recovery')", [])
            .map_err(io_error)?;
        Ok(())
    }

    pub fn commit(self) -> std::io::Result<()> {
        self._connection.execute_batch("COMMIT").map_err(io_error)
    }
}

fn share_reader_files(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // The dedicated volume is RW only in web/loader; Gate's separate UID mounts it RO.
    for suffix in ["", "-wal", "-shm"] {
        let file = PathBuf::from(format!("{}{suffix}", path.display()));
        match std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn io_error(_: rusqlite::Error) -> std::io::Error {
    std::io::Error::other("preset authority unavailable")
}
fn conflict() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "stale maintenance operation",
    )
}

/// Admission-side read of the same authority. No database or schema is created here.
/// An unavailable or locked journal refuses admission while the publisher settles.
pub fn allows(path: &Path, fence: Option<&Fence>) -> bool {
    let read = || -> rusqlite::Result<bool> {
        let db = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        db.busy_timeout(Duration::from_millis(25))?;
        db.execute_batch("BEGIN")?;
        let r = db.query_row("SELECT incarnation,lease,CAST(generation AS TEXT),active,CAST(until_ms AS TEXT),recovery FROM preset_authority WHERE singleton=1", [], |r| Ok(AuthorityRecord {incarnation:r.get(0)?,lease:r.get(1)?,generation:r.get::<_,String>(2)?.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,active:r.get(3)?,until_ms:r.get::<_,String>(4)?.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,recovery:r.get(5)?}))?;
        if let Some(f) = fence {
            if !r.active
                || r.incarnation != f.incarnation
                || r.lease != f.lease
                || r.generation != f.generation
                || now_ms() >= r.until_ms
            {
                return Ok(false);
            }
            if let Some(expected) = &f.expected_revision {
                let actual: String = db.query_row(
                    "SELECT revision FROM preset_revision WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?;
                if expected != &actual {
                    return Ok(false);
                }
            }
            return db.query_row("SELECT EXISTS(SELECT 1 FROM preset_operations WHERE id=?1 AND incarnation=?2 AND lease=?3 AND generation=?4 AND kind=?5 AND expected_revision IS ?6 AND state IN ('issued','running'))", rusqlite::params![f.operation,f.incarnation,f.lease,f.generation.to_string(),f.kind,f.expected_revision], |r| r.get(0));
        }
        let pending: bool =
            db.query_row("SELECT EXISTS(SELECT 1 FROM preset_operations WHERE state IN ('issued','running','recovery'))", [], |r| {
                r.get(0)
            })?;
        Ok(!r.active && !r.recovery && !pending)
    };
    read().unwrap_or(false)
}

/// A bounded, read-only WAL snapshot for a caller waiting on an already issued operation.
pub fn read_receipt(path: &Path, f: &Fence) -> std::io::Result<Option<Receipt>> {
    use rusqlite::OptionalExtension;
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(io_error)?;
    db.busy_timeout(Duration::from_millis(25))
        .map_err(io_error)?;
    db.query_row("SELECT state,revision FROM preset_operations WHERE id=?1 AND incarnation=?2 AND lease=?3 AND generation=?4 AND kind=?5 AND expected_revision IS ?6",rusqlite::params![f.operation,f.incarnation,f.lease,f.generation.to_string(),f.kind,f.expected_revision],|r|Ok(Receipt {state:r.get(0)?,revision:r.get(1)?})).optional().map_err(io_error)
}

/// Keeps WAL/shm available to a read-only gate while this authority is running.
pub struct Anchor {
    _connection: rusqlite::Connection,
}
impl Anchor {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let connection = rusqlite::Connection::open(path).map_err(io_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(io_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(io_error)?;
        let _: i64 = connection
            .query_row("SELECT count(*) FROM preset_authority", [], |r| r.get(0))
            .map_err(io_error)?;
        Ok(Self {
            _connection: connection,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    fn fence() -> Fence {
        Fence {
            incarnation: "front-a".into(),
            lease: "hold-a".into(),
            generation: 1,
            operation: "op-a".into(),
            kind: "preset-write".into(),
            expected_revision: Some("revision-a".into()),
        }
    }
    fn record(generation: u64) -> AuthorityRecord {
        AuthorityRecord {
            incarnation: "front-a".into(),
            lease: "hold-a".into(),
            generation,
            active: true,
            until_ms: now_ms() + 60_000,
            recovery: false,
        }
    }
    #[test]
    fn publication_readonly_wal_snapshot_does_not_block_writer_and_exact_generation_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("authority.sqlite");
        let f = fence();
        let journal = Guard::at(&file).unwrap();
        journal.authority().unwrap();
        journal.save_authority(&record(1)).unwrap();
        journal.observe_revision("revision-a").unwrap();
        journal.begin(&f).unwrap();
        journal.commit().unwrap();
        let _anchor = Anchor::open(&file).unwrap();
        let writer = Guard::at(&file).unwrap();
        writer.save_authority(&record(2)).unwrap();
        // Gate opens read-only and reads the last committed WAL snapshot while a writer owns it.
        assert!(allows(&file, Some(&f)));
        assert!(!allows(&file, None));
        writer.commit().unwrap();
        assert!(!allows(&file, Some(&f)));
        let fresh = Fence {
            generation: 2,
            operation: "op-b".into(),
            ..f.clone()
        };
        let journal = Guard::at(&file).unwrap();
        journal.begin(&fresh).unwrap();
        journal.commit().unwrap();
        assert!(allows(&file, Some(&fresh)));
    }
    #[test]
    fn publication_empty_and_old_journals_migrate_without_preventing_legacy_file_writes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("authority.sqlite");
        assert!(!allows(&file, None)); // absent during startup is tolerated without creating a file
        assert!(!file.exists());
        let connection = rusqlite::Connection::open(&file).unwrap();
        connection.execute_batch("CREATE TABLE preset_operations(id TEXT PRIMARY KEY,incarnation TEXT NOT NULL,lease TEXT NOT NULL,generation TEXT NOT NULL); INSERT INTO preset_operations VALUES('old-op','old-front','old-hold','1')").unwrap();
        drop(connection);
        let journal = Guard::at(&file).unwrap();
        assert!(journal.authority().unwrap().is_none());
        assert!(journal.pending().unwrap());
        journal.save_authority(&record(1)).unwrap();
        journal.begin(&fence()).unwrap();
        journal.commit().unwrap();
        // An older image ignores the extra volume and request metadata. It can still write its file.
        let ini = dir.path().join("models.ini");
        std::fs::write(&ini, "legacy").unwrap();
        let journal = Guard::at(&file).unwrap();
        assert!(journal.validate(&fence()).is_ok());
        journal
            .finish(&fence(), "settled", Some("revision-b"))
            .unwrap();
        journal.commit().unwrap();
        assert_eq!(std::fs::read_to_string(ini).unwrap(), "legacy");
    }
    #[test]
    fn publication_pull_intent_blocks_cleanup_until_the_actual_publisher_settles() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("authority.sqlite");
        let pull = Fence {
            kind: "pull".into(),
            operation: "pull-op".into(),
            ..fence()
        };
        let cleanup = Fence {
            kind: "delete".into(),
            operation: "cleanup-op".into(),
            ..fence()
        };
        let journal = Guard::at(&file).unwrap();
        journal.authority().unwrap();
        journal.save_authority(&record(1)).unwrap();
        journal.begin(&pull).unwrap();
        journal.running(&pull).unwrap();
        journal.begin(&cleanup).unwrap();
        assert!(
            journal.no_other_publishers(&cleanup).is_err(),
            "HTTP launch acknowledgement is not publication settlement"
        );
        journal.commit().unwrap();
        let journal = Guard::at(&file).unwrap();
        assert!(
            journal.no_other_publishers(&cleanup).is_err(),
            "intent survives a client/process boundary"
        );
        journal.finish(&pull, "settled", None).unwrap();
        assert!(journal.no_other_publishers(&cleanup).is_ok());
        journal.commit().unwrap();
    }

    #[test]
    fn publication_terminal_receipt_cannot_authorize_another_effect_or_clear_a_later_operation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("authority.sqlite");
        let a = fence();
        let b = Fence {
            operation: "op-b".into(),
            ..a.clone()
        };
        let journal = Guard::at(&file).unwrap();
        journal.authority().unwrap();
        journal.save_authority(&record(1)).unwrap();
        journal.begin(&a).unwrap();
        journal.finish(&a, "settled", Some("revision-b")).unwrap();
        journal.begin(&b).unwrap();
        journal.finish(&a, "settled", None).unwrap();
        assert!(journal.validate(&a).is_err());
        assert!(journal.validate(&b).is_ok());
        assert!(journal.pending().unwrap());
        journal.commit().unwrap();
    }
}
