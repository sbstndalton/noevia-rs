//! Process-owned worker lifetimes, including work detached from an HTTP request.
//! Register before spawning, release from the worker itself, and drain only after admission closes.
use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        OnceLock,
    },
};
#[cfg(feature = "native-routing")]
use tokio::time::Instant;
use tokio::{sync::Notify, task::JoinHandle};

#[derive(Default)]
pub(crate) struct Tracker {
    active: AtomicUsize,
    changed: Notify,
}
pub(crate) struct Held(&'static Tracker);
impl Drop for Held {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}
fn tracker() -> &'static Tracker {
    static TRACKER: OnceLock<Tracker> = OnceLock::new();
    TRACKER.get_or_init(Tracker::default)
}
fn register() -> Held {
    let tracker = tracker();
    tracker.active.fetch_add(1, Ordering::AcqRel);
    Held(tracker)
}
pub(crate) fn spawn<F>(work: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let held = register();
    tokio::spawn(async move {
        let _held = held;
        work.await
    })
}
pub(crate) fn spawn_blocking<F, T>(work: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let held = register();
    tokio::task::spawn_blocking(move || {
        let _held = held;
        work()
    })
}
#[cfg(feature = "native-benchmark-runner")]
pub(crate) fn spawn_blocking_on<F, T>(runtime: &tokio::runtime::Handle, work: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let held = register();
    runtime.spawn_blocking(move || {
        let _held = held;
        work()
    })
}
#[cfg(feature = "native-routing")]
pub(crate) async fn drain(deadline: Instant) -> Result<(), ()> {
    let tracker = tracker();
    loop {
        let notified = tracker.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if tracker.active.load(Ordering::Acquire) == 0 {
            return Ok(());
        }
        tokio::time::timeout_at(deadline, notified)
            .await
            .map_err(|_| ())?;
    }
}
