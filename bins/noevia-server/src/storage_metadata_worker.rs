//! One nonqueueing metadata job. The coordinator owns completion after request cancellation.
//! Admission is bounded; blocking completion has no hard deadline or abort guarantee.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::{
    sync::{oneshot, Semaphore},
    task::JoinHandle,
};

#[derive(Default)]
struct State {
    observer: Option<JoinHandle<()>>,
    stopped: bool,
}
pub struct Worker {
    capacity: Arc<Semaphore>,
    quarantine: Arc<AtomicBool>,
    state: Mutex<State>,
}
#[derive(Debug, Clone, Copy)]
pub struct Unavailable;
impl Default for Worker {
    fn default() -> Self {
        Self {
            capacity: Arc::new(Semaphore::new(1)),
            quarantine: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(State::default()),
        }
    }
}
impl Worker {
    pub async fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Unavailable> {
        let receive = {
            let mut state = self.state.lock().map_err(|_| Unavailable)?;
            if state.stopped
                || self.quarantine.load(Ordering::Acquire)
                || state.observer.as_ref().is_some_and(|h| !h.is_finished())
            {
                return Err(Unavailable);
            }
            let permit = Arc::clone(&self.capacity)
                .try_acquire_owned()
                .map_err(|_| Unavailable)?;
            let quarantine = Arc::clone(&self.quarantine);
            let blocked = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
                    Ok(result) => Some(result),
                    Err(_) => {
                        // Set refusal before unwinding releases admission. A panic does not
                        // mean the semaphore remains held; the quarantine is the refusal.
                        quarantine.store(true, Ordering::Release);
                        None
                    }
                }
            });
            let (send, receive) = oneshot::channel();
            let quarantine = Arc::clone(&self.quarantine);
            state.observer = Some(tokio::spawn(async move {
                match blocked.await {
                    Ok(Some(result)) => {
                        let _ = send.send(result);
                    }
                    _ => {
                        quarantine.store(true, Ordering::Release);
                    }
                }
            }));
            receive
        };
        receive.await.map_err(|_| Unavailable)
    }

    /// Stop admission and wait for the owned observer. A hung blocking job can hang shutdown.
    pub async fn shutdown(&self) -> Result<(), Unavailable> {
        let observer = {
            let mut state = self.state.lock().map_err(|_| Unavailable)?;
            state.stopped = true;
            state.observer.take()
        };
        if let Some(observer) = observer {
            if observer.await.is_err() {
                self.quarantine.store(true, Ordering::Release);
                return Err(Unavailable);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancellation_before_poll_does_not_admit() {
        let worker = Worker::default();
        let job = worker.run(|| true);
        drop(job);
        assert!(worker.run(|| true).await.unwrap());
        worker.shutdown().await.unwrap();
        assert!(worker.run(|| false).await.is_err());
    }
    #[tokio::test]
    async fn cancelled_request_keeps_running_job_and_refuses_second() {
        let worker = Arc::new(Worker::default());
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
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
        assert!(worker.run(|| false).await.is_err());
        release_tx.send(()).unwrap();
        worker.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn panic_quarantines_after_permit_is_dropped() {
        let worker = Worker::default();
        assert!(worker
            .run(|| panic!("synthetic worker panic"))
            .await
            .is_err());
        assert!(worker.quarantine.load(Ordering::Acquire));
        assert!(worker.run(|| true).await.is_err());
        worker.shutdown().await.unwrap();
    }
}
