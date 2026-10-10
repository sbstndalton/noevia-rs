//! The lock projects.json writers share with Node during the transition (core
//! `server/rust-projects.cjs` withFileLock; keep the constants equal): an OS advisory lock,
//! flock(2) LOCK_EX, on `<file>.lock` (mode 0600), a long-lived file that is created once and
//! never unlinked (unlinking would let two writers lock two different inodes). flock and fcntl
//! locks do not interoperate on Linux, so both sides use flock: Rust through std's
//! `File::try_lock` (flock(LOCK_EX|LOCK_NB) on Linux), Node through util-linux `flock(1)` on an
//! inherited descriptor. A writer that dies releases the lock with its descriptor: no staleness
//! rule. A writer waits up to [`WAIT_MS`], retrying every [`RETRY_MS`], then gives up (Busy).
//! A hold is one read-modify-write.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const SUFFIX: &str = ".lock";
pub const WAIT_MS: i64 = 3_000;
pub const RETRY_MS: u64 = 5;

#[derive(Debug)]
pub enum LockError {
    /// Held by another writer for longer than [`WAIT_MS`]: Node's 503 PROJECTS_BUSY.
    Busy,
    Io(std::io::Error),
}

/// Held until dropped (closing the descriptor releases the flock; the file stays).
#[derive(Debug)]
pub struct FileLock {
    _file: File,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The lock file's path for `file`.
pub fn path_for(file: &Path) -> PathBuf {
    let mut path = file.as_os_str().to_owned();
    path.push(SUFFIX);
    PathBuf::from(path)
}

/// Takes the lock for `file` (the data file, not the lock's own name).
pub fn acquire(file: &Path) -> Result<FileLock, LockError> {
    acquire_with(file, &now_ms, &|ms| {
        std::thread::sleep(Duration::from_millis(ms))
    })
}

/// [`acquire`] with the clock and the sleep supplied (tests).
pub fn acquire_with(
    file: &Path,
    now: &dyn Fn() -> i64,
    sleep: &dyn Fn(u64),
) -> Result<FileLock, LockError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let f = options.open(path_for(file)).map_err(LockError::Io)?;
    let deadline = now().saturating_add(WAIT_MS);
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(FileLock { _file: f }),
            Err(TryLockError::WouldBlock) => {
                if now() >= deadline {
                    return Err(LockError::Busy);
                }
                sleep(RETRY_MS);
            }
            Err(TryLockError::Error(e)) => return Err(LockError::Io(e)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn exclusive_released_on_drop_never_unlinked_mode() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("projects.json");
        let lock_path = dir.path().join("projects.json.lock");
        {
            let _held = acquire(&file).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&lock_path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            let clock = Cell::new(now_ms());
            let now = || clock.get();
            let sleep = |ms: u64| clock.set(clock.get() + i64::try_from(ms).unwrap());
            assert!(matches!(
                acquire_with(&file, &now, &sleep),
                Err(LockError::Busy)
            ));
            assert!(clock.get() - now_ms() >= WAIT_MS - 1_000);
        }
        assert!(lock_path.exists(), "the lock file is long-lived");
        drop(acquire(&file).unwrap());
    }

    #[test]
    fn threads_racing_are_never_both_inside() {
        let dir = tempfile::tempdir().unwrap();
        let file = std::sync::Arc::new(dir.path().join("projects.json"));
        let inside = std::sync::Arc::new(AtomicUsize::new(0));
        let total = std::sync::Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (file, inside, total) = (file.clone(), inside.clone(), total.clone());
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        let _held = acquire(&file).unwrap();
                        assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0, "two inside");
                        total.fetch_add(1, Ordering::SeqCst);
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(total.load(Ordering::SeqCst), 800);
    }
}
