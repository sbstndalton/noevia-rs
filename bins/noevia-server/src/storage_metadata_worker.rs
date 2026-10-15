//! Bounded storage-metadata jobs (noevia#1827). A small pool of slots; a request waits a bounded
//! time for one instead of being refused at once. Callers authenticate before asking for a slot.
//! A started job owns its slot until it completes, even if the request is cancelled. A panic
//! quarantines the worker for a bounded backoff, then admission recovers; both transitions log
//! one line. Blocking completion has no hard deadline or abort guarantee.
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

/// Concurrent jobs (two tabs opening Settings together both succeed).
pub const SLOTS: u32 = 2;
/// How long an authenticated request waits for a slot before a 503.
pub const SLOT_WAIT: Duration = Duration::from_secs(2);
/// How long a panic keeps the worker refusing before it admits again.
pub const QUARANTINE: Duration = Duration::from_secs(60);

pub struct Worker {
    capacity: Arc<Semaphore>,
    slots: u32,
    wait: Duration,
    backoff: Duration,
    quarantined: Arc<Mutex<Option<Instant>>>,
    stopped: AtomicBool,
}
#[derive(Debug, Clone, Copy)]
pub struct Unavailable;
impl Default for Worker {
    fn default() -> Self {
        Self::new(SLOTS, SLOT_WAIT, QUARANTINE)
    }
}
fn quarantine(state: &Mutex<Option<Instant>>, backoff: Duration) {
    if let Ok(mut q) = state.lock() {
        if q.is_none() {
            eprintln!(
                "noevia-server: storage metadata worker quarantined after a job panic; retrying in {}s",
                backoff.as_secs()
            );
        }
        *q = Some(Instant::now());
    }
}
impl Worker {
    pub fn new(slots: u32, wait: Duration, backoff: Duration) -> Self {
        Self {
            capacity: Arc::new(Semaphore::new(slots as usize)),
            slots,
            wait,
            backoff,
            quarantined: Arc::new(Mutex::new(None)),
            stopped: AtomicBool::new(false),
        }
    }

    /// Whether admission is open, recovering an expired quarantine (logged once).
    fn admit(&self) -> Result<(), Unavailable> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(Unavailable);
        }
        let mut q = self.quarantined.lock().map_err(|_| Unavailable)?;
        if let Some(since) = *q {
            if since.elapsed() < self.backoff {
                return Err(Unavailable);
            }
            *q = None;
            eprintln!("noevia-server: storage metadata worker recovered from quarantine");
        }
        Ok(())
    }

    pub async fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Unavailable> {
        self.admit()?;
        let permit = tokio::time::timeout(self.wait, Arc::clone(&self.capacity).acquire_owned())
            .await
            .map_err(|_| Unavailable)?
            .map_err(|_| Unavailable)?;
        // Re-check: a job may have panicked, or shutdown begun, while this request waited.
        self.admit()?;
        let state = Arc::clone(&self.quarantined);
        let backoff = self.backoff;
        let blocked = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
                Ok(result) => Some(result),
                Err(_) => {
                    // Set refusal before unwinding releases the slot.
                    quarantine(&state, backoff);
                    None
                }
            }
        });
        match blocked.await {
            Ok(Some(result)) => Ok(result),
            Ok(None) => Err(Unavailable),
            Err(_) => {
                quarantine(&self.quarantined, self.backoff);
                Err(Unavailable)
            }
        }
    }

    /// Stop admission and wait for every running job. A hung blocking job can hang shutdown.
    pub async fn shutdown(&self) -> Result<(), Unavailable> {
        self.stopped.store(true, Ordering::Release);
        let all = Arc::clone(&self.capacity)
            .acquire_many_owned(self.slots)
            .await
            .map_err(|_| Unavailable)?;
        self.capacity.close();
        drop(all);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    #[tokio::test]
    async fn cancellation_before_poll_does_not_admit() {
        let worker = Worker::default();
        let job = worker.run(|| true);
        drop(job);
        assert!(worker.run(|| true).await.unwrap());
        worker.shutdown().await.unwrap();
        assert!(worker.run(|| false).await.is_err());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_concurrent_jobs_both_run() {
        let worker = Arc::new(Worker::default());
        // Each job waits for the other: both must hold a slot at the same time.
        let barrier = Arc::new(Barrier::new(2));
        let (a, b) = (Arc::clone(&barrier), Arc::clone(&barrier));
        let (w1, w2) = (Arc::clone(&worker), Arc::clone(&worker));
        let one = tokio::spawn(async move { w1.run(move || a.wait().is_leader()).await });
        let two = tokio::spawn(async move { w2.run(move || b.wait().is_leader()).await });
        let (one, two) = (one.await.unwrap().unwrap(), two.await.unwrap().unwrap());
        assert!(one ^ two);
        worker.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn a_busy_pool_waits_boundedly_then_admits() {
        let worker = Arc::new(Worker::new(1, Duration::from_secs(2), QUARANTINE));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let w = Arc::clone(&worker);
        let first = tokio::spawn(async move {
            w.run(move || {
                let _ = entered_tx.send(());
                release_rx.recv().unwrap();
                1
            })
            .await
        });
        entered_rx.await.unwrap();
        let w = Arc::clone(&worker);
        let second = tokio::spawn(async move { w.run(|| 2).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        release_tx.send(()).unwrap();
        assert_eq!(first.await.unwrap().unwrap(), 1);
        assert_eq!(second.await.unwrap().unwrap(), 2);
        // With the slot held past the wait, the request is refused instead of queueing forever.
        let short = Arc::new(Worker::new(1, Duration::from_millis(30), QUARANTINE));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let s = Arc::clone(&short);
        let held = tokio::spawn(async move {
            s.run(move || {
                let _ = entered_tx.send(());
                release_rx.recv().unwrap();
            })
            .await
        });
        entered_rx.await.unwrap();
        assert!(short.run(|| ()).await.is_err());
        release_tx.send(()).unwrap();
        held.await.unwrap().unwrap();
        worker.shutdown().await.unwrap();
        short.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn cancelled_request_keeps_its_job_running() {
        let worker = Arc::new(Worker::new(1, Duration::from_millis(30), QUARANTINE));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let w = Arc::clone(&worker);
        let request = tokio::spawn(async move {
            w.run(move || {
                let _ = entered_tx.send(());
                release_rx.recv().unwrap();
                true
            })
            .await
        });
        entered_rx.await.unwrap();
        request.abort();
        let _ = request.await;
        // The aborted request's job still owns the only slot.
        assert!(worker.run(|| false).await.is_err());
        release_tx.send(()).unwrap();
        worker.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn panic_quarantines_then_recovers_after_the_backoff() {
        let worker = Worker::new(1, Duration::from_millis(30), Duration::from_millis(100));
        assert!(worker
            .run(|| panic!("synthetic worker panic"))
            .await
            .is_err());
        assert!(worker.quarantined.lock().unwrap().is_some());
        assert!(worker.run(|| true).await.is_err());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(worker.run(|| true).await.unwrap());
        assert!(worker.quarantined.lock().unwrap().is_none());
        worker.shutdown().await.unwrap();
    }
}
