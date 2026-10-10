//! At most [`PERMITS`] Argon2 computations at a time, as in Node, where @node-rs/argon2 runs on
//! libuv's pool of 4 threads. The front runs requests on a blocking pool of hundreds of threads;
//! unbounded, a burst of sign-ins could hold hundreds of 19 MiB hashes in memory at once.

use std::sync::{Condvar, Mutex};

pub const PERMITS: usize = 4;

static IN_USE: Mutex<usize> = Mutex::new(0);
static FREED: Condvar = Condvar::new();

struct Permit;

impl Drop for Permit {
    fn drop(&mut self) {
        let mut n = IN_USE.lock().unwrap_or_else(|p| p.into_inner());
        *n = n.saturating_sub(1);
        FREED.notify_one();
    }
}

fn acquire() -> Permit {
    let mut n = IN_USE.lock().unwrap_or_else(|p| p.into_inner());
    while *n >= PERMITS {
        n = FREED.wait(n).unwrap_or_else(|p| p.into_inner());
    }
    *n += 1;
    Permit
}

/// Runs `f` (one Argon2 hash or verification) under a permit.
pub fn bounded<T>(f: impl FnOnce() -> T) -> T {
    let _permit = acquire();
    f()
}

/// `verify(hash, password)`, bounded.
pub fn verify(hash: &str, password: &str) -> bool {
    bounded(|| server_auth::password::verify(hash, password))
}

/// The decoy verification for a missing account, bounded.
pub fn burn(password: &str) -> bool {
    bounded(|| server_auth::password::burn(password))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn never_more_than_the_permits() {
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let (live, peak) = (Arc::clone(&live), Arc::clone(&peak));
                std::thread::spawn(move || {
                    bounded(|| {
                        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        live.fetch_sub(1, Ordering::SeqCst);
                    })
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= PERMITS);
        assert!(peak.load(Ordering::SeqCst) >= 2);
    }
}
