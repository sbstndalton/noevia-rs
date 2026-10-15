//! core `server/short-read.cjs` `createSharedRead`: the answer is computed once; callers that
//! arrive while it is being computed share it, and it is kept for `ttl` after it lands (measured
//! from landing, not from the start). [`SharedRead::clear`] drops it at once, so the next caller
//! computes a fresh one (callers already waiting still get the one in flight).
//!
//! The computation runs on its own task, so a caller that goes away (a closed tab) never cancels
//! it for the others, as a JS promise is not cancelled by one awaiting caller leaving. A
//! computation that does not finish (the task panicked or the runtime is shutting down) is not
//! kept, as short-read.cjs drops a rejected answer, and its waiters get `None`.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// routes/models.cjs `createSharedRead(..., 2000)`.
pub const STATS_TTL: Duration = Duration::from_millis(2000);

struct Entry<T> {
    rx: watch::Receiver<Option<T>>,
    /// When the answer landed; `None` while it is being computed.
    landed: Mutex<Option<Instant>>,
}

pub struct SharedRead<T> {
    ttl: Duration,
    current: Mutex<Option<Arc<Entry<T>>>>,
}

impl<T: Clone + Send + Sync + 'static> SharedRead<T> {
    pub fn new(ttl: Duration) -> Self {
        SharedRead {
            ttl,
            current: Mutex::new(None),
        }
    }

    /// The shared answer, starting `load` only when there is none in flight or still fresh.
    pub async fn get<F, Fut>(&self, load: F) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let entry = {
            let mut slot = self.current.lock().ok()?;
            let fresh = slot.as_ref().filter(|e| match e.landed.lock() {
                Ok(l) => l.is_none_or(|at| at + self.ttl > Instant::now()),
                Err(_) => false,
            });
            match fresh {
                Some(e) => Arc::clone(e),
                None => {
                    let (tx, rx) = watch::channel(None);
                    let entry = Arc::new(Entry {
                        rx,
                        landed: Mutex::new(None),
                    });
                    *slot = Some(Arc::clone(&entry));
                    // Asked now, not on a later poll: a caller right behind this one shares it.
                    let future = load();
                    let landing = Arc::clone(&entry);
                    tokio::spawn(async move {
                        let value = future.await;
                        if let Ok(mut l) = landing.landed.lock() {
                            *l = Some(Instant::now());
                        }
                        let _ = tx.send(Some(value));
                    });
                    entry
                }
            }
        };
        let mut rx = entry.rx.clone();
        let got = rx
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|v| v.clone());
        if got.is_none() {
            // The computation ended without an answer: drop it if it is still the current one.
            if let Ok(mut slot) = self.current.lock() {
                if slot.as_ref().is_some_and(|e| Arc::ptr_eq(e, &entry)) {
                    *slot = None;
                }
            }
        }
        got
    }

    /// `clear()`: the next caller computes a fresh answer.
    pub fn clear(&self) {
        if let Ok(mut slot) = self.current.lock() {
            *slot = None;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counted(
        loads: &Arc<AtomicUsize>,
        delay: Duration,
    ) -> impl FnOnce() -> std::pin::Pin<Box<dyn Future<Output = usize> + Send>> {
        let loads = Arc::clone(loads);
        move || {
            let n = loads.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                n
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_share_one_load_and_it_is_kept_for_the_ttl_after_landing() {
        let read = Arc::new(SharedRead::new(STATS_TTL));
        let loads = Arc::new(AtomicUsize::new(0));
        let slow = Duration::from_millis(1500);
        let (a, b) = tokio::join!(
            read.get(counted(&loads, slow)),
            read.get(counted(&loads, slow))
        );
        assert_eq!((a, b), (Some(1), Some(1)));
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        // 1.5 s spent loading; the TTL starts when it landed, so 1.9 s later it is still shared.
        tokio::time::advance(Duration::from_millis(1900)).await;
        assert_eq!(read.get(counted(&loads, slow)).await, Some(1));
        tokio::time::advance(Duration::from_millis(100)).await;
        assert_eq!(read.get(counted(&loads, slow)).await, Some(2));
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn clear_makes_the_next_caller_load_while_waiters_keep_theirs() {
        let read = Arc::new(SharedRead::new(STATS_TTL));
        let loads = Arc::new(AtomicUsize::new(0));
        let slow = Duration::from_millis(500);
        let first = {
            let read = Arc::clone(&read);
            let loads = Arc::clone(&loads);
            tokio::spawn(async move { read.get(counted(&loads, slow)).await })
        };
        tokio::task::yield_now().await;
        read.clear();
        let second = read.get(counted(&loads, slow)).await;
        assert_eq!((first.await.unwrap(), second), (Some(1), Some(2)));
        assert_eq!(read.get(counted(&loads, slow)).await, Some(2));
        read.clear();
        assert_eq!(read.get(counted(&loads, slow)).await, Some(3));
    }

    #[tokio::test(start_paused = true)]
    async fn a_caller_that_leaves_does_not_cancel_the_load() {
        let read = Arc::new(SharedRead::new(STATS_TTL));
        let loads = Arc::new(AtomicUsize::new(0));
        let slow = Duration::from_millis(500);
        let gone = tokio::time::timeout(Duration::from_millis(10), read.get(counted(&loads, slow)));
        assert!(gone.await.is_err());
        assert_eq!(read.get(counted(&loads, slow)).await, Some(1));
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }
}
