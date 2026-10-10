//! The lock projects.json writers share with Node during the transition (core
//! `server/rust-projects.cjs` withFileLock; keep the constants equal): `<file>.lock`, created with
//! O_CREAT|O_EXCL and mode 0600, holding `"<pid> <ms>\n"`, removed on release. A writer waits up
//! to [`WAIT_MS`], retrying every [`RETRY_MS`]; a lock older than [`STALE_MS`] belongs to a writer
//! that died mid-write and is removed. A hold is one read-modify-write.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const SUFFIX: &str = ".lock";
pub const STALE_MS: i64 = 10_000;
pub const WAIT_MS: i64 = 3_000;
pub const RETRY_MS: u64 = 5;

#[derive(Debug)]
pub enum LockError {
    /// Held by a live writer for longer than [`WAIT_MS`]: Node's 503 PROJECTS_BUSY.
    Busy,
    Io(std::io::Error),
}

/// Held until dropped.
#[derive(Debug)]
pub struct FileLock {
    path: PathBuf,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Removed as stale by another writer: nothing to release.
        let _ = std::fs::remove_file(&self.path);
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn mtime_ms(m: &std::fs::Metadata) -> Option<i64> {
    let t = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(t.as_millis()).ok()
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
    let mut path = file.as_os_str().to_owned();
    path.push(SUFFIX);
    let path = PathBuf::from(path);
    let deadline = now().saturating_add(WAIT_MS);
    loop {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut f) => {
                let lock = FileLock { path };
                f.write_all(format!("{} {}\n", std::process::id(), now()).as_bytes())
                    .map_err(LockError::Io)?;
                return Ok(lock);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let held = match std::fs::metadata(&path) {
                    Ok(m) => Some(m),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(LockError::Io(e)),
                };
                if let Some(m) = &held {
                    if mtime_ms(m).is_some_and(|t| now().saturating_sub(t) > STALE_MS) {
                        match std::fs::remove_file(&path) {
                            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                                return Err(LockError::Io(e))
                            }
                            _ => continue,
                        }
                    }
                }
                if now() >= deadline {
                    return Err(LockError::Busy);
                }
                if held.is_some() {
                    sleep(RETRY_MS);
                }
            }
            Err(e) => return Err(LockError::Io(e)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn exclusive_released_on_drop_mode_and_content() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("projects.json");
        let lock_path = dir.path().join("projects.json.lock");
        {
            let _held = acquire(&file).unwrap();
            let text = std::fs::read_to_string(&lock_path).unwrap();
            let (pid, ms) = text.trim_end().split_once(' ').unwrap();
            assert_eq!(pid, std::process::id().to_string());
            assert!(ms.parse::<i64>().unwrap() > 0);
            assert!(text.ends_with('\n'));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&lock_path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
        }
        assert!(!lock_path.exists());
    }

    #[test]
    fn a_live_lock_is_waited_for_then_busy_a_stale_one_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("projects.json");
        let lock_path = dir.path().join("projects.json.lock");
        std::fs::write(&lock_path, "1 1\n").unwrap();
        let clock = Cell::new(now_ms());
        let slept = Cell::new(0u64);
        let now = || clock.get();
        let sleep = |ms: u64| {
            slept.set(slept.get() + ms);
            clock.set(clock.get() + i64::try_from(ms).unwrap());
        };
        assert!(matches!(
            acquire_with(&file, &now, &sleep),
            Err(LockError::Busy)
        ));
        assert!(i64::try_from(slept.get()).unwrap() >= WAIT_MS);
        assert!(lock_path.exists(), "a live lock is not broken");
        // The same file, older than STALE_MS.
        let old =
            SystemTime::now() - Duration::from_millis(u64::try_from(STALE_MS).unwrap() + 1000);
        std::fs::File::options()
            .write(true)
            .open(&lock_path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let held = acquire(&file).unwrap();
        drop(held);
        assert!(!lock_path.exists());
    }
}
