//! A timer task owned by whoever set it: dropping it stops it.
use tokio::task::JoinHandle;

pub(crate) struct Timer {
    pub(crate) generation: u64,
    handle: Option<JoinHandle<()>>,
}

impl Timer {
    pub(crate) fn new(generation: u64, handle: JoinHandle<()>) -> Self {
        Timer {
            generation,
            handle: Some(handle),
        }
    }

    /// The timer has fired and is now doing its work: it must not abort itself when this goes.
    pub(crate) fn disarm(mut self) {
        self.handle.take();
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
