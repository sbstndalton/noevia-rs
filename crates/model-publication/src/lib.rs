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

#[derive(Clone, Debug, serde::Serialize)]
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
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(io_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(io_error)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(io_error)?;
        connection
            .execute_batch("BEGIN EXCLUSIVE")
            .map_err(io_error)?;
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
        self._connection.execute("UPDATE preset_operations SET state=?5,revision=?6 WHERE id=?1 AND incarnation=?2 AND lease=?3 AND generation=?4 AND state IN ('issued','running','recovery')", rusqlite::params![f.operation,f.incarnation,f.lease,f.generation.to_string(),state,revision]).map_err(io_error)?;
        Ok(())
    }
    pub fn settle(&self, f: &Fence) -> std::io::Result<()> {
        self.finish(f, "settled", None)
    }
    pub fn status(&self, f: &Fence) -> std::io::Result<Option<Receipt>> {
        use rusqlite::OptionalExtension;
        self._connection.query_row("SELECT state,revision FROM preset_operations WHERE id=?1 AND incarnation=?2 AND lease=?3 AND generation=?4", rusqlite::params![f.operation,f.incarnation,f.lease,f.generation.to_string()], |r| Ok(Receipt {state:r.get(0)?,revision:r.get(1)?})).optional().map_err(io_error)
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
