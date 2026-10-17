//! The two store touches of a download: the token for each request (`hf.get_token()`, via the
//! update check's reader) and `db.record_download(...)` when a job ends.
use rusqlite::{Connection, OpenFlags};
use std::{path::Path, time::Duration};

/// sqlite3's default `timeout=5.0`.
const BUSY_WAIT: Duration = Duration::from_millis(5000);

pub(super) use crate::check_updates::{hub_auth, HubAuth};

/// `db.record_download(...)`. A failure is logged and otherwise ignored: Python's write runs in
/// the task's `finally`, after the job's state is final.
#[allow(clippy::too_many_arguments)]
pub(super) fn record(
    database: &Path,
    repo_id: &str,
    filename: &str,
    dest_path: &str,
    total_bytes: i64,
    status: &str,
    error: Option<&str>,
    started_at: f64,
    completed_at: Option<f64>,
) {
    let write = || -> rusqlite::Result<()> {
        let db = Connection::open_with_flags(
            database,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(BUSY_WAIT)?;
        db.execute(
            "INSERT INTO download_history(repo_id, filename, dest_path, total_bytes, status, error, started_at, completed_at) \
             VALUES(?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![repo_id, filename, dest_path, total_bytes, status, error, started_at, completed_at],
        )?;
        Ok(())
    };
    if let Err(e) = write() {
        crate::sqlite_read::log_once("downloads", &e);
    }
}
