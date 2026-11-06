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
        let acquire = || -> rusqlite::Result<Self> {
            let connection = rusqlite::Connection::open(path)?;
            connection.busy_timeout(Duration::from_secs(5))?;
            connection.execute_batch("BEGIN EXCLUSIVE")?;
            Ok(Self {
                _connection: connection,
            })
        };
        acquire().map_err(|_| std::io::Error::other("preset publication lock unavailable"))
    }
}
