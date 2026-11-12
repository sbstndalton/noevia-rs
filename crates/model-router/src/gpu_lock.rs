//! The optional GPU lock: an exclusive flock on a shared file (for example the eval queue's), held
//! from just before a backend starts until it is stopped and its containers are verified gone.

use rustix::fs::{flock, FlockOperation};
use std::path::Path;
use std::time::Duration;
use tokio::time::{sleep, Instant};

/// Holding the lock; dropping it closes the file, which releases the flock.
#[derive(Debug)]
pub struct GpuLock {
    _file: std::fs::File,
}

/// Try the lock every 250 ms until `wait` passes. `Err` says why it was not taken.
pub async fn acquire(path: &Path, wait: Duration) -> Result<GpuLock, String> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("gpu lock {}: {e}", path.display()))?;
    let deadline = Instant::now() + wait;
    loop {
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(GpuLock { _file: file }),
            Err(e) if e == rustix::io::Errno::WOULDBLOCK || e == rustix::io::Errno::INTR => {}
            Err(e) => return Err(format!("gpu lock {}: {e}", path.display())),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "gpu lock {} is held by another job (waited {:.0}s)",
                path.display(),
                wait.as_secs_f64()
            ));
        }
        sleep(Duration::from_millis(250)).await;
    }
}
