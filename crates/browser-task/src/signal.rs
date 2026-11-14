//! An `AbortSignal`: aborted once, and every hook registered before then runs once, on the thread
//! that aborts (hooks only signal or spawn; none blocks).

use std::sync::{Mutex, PoisonError};

type Hook = Box<dyn FnOnce() + Send>;

#[derive(Default)]
pub struct Signal {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    aborted: bool,
    next: u64,
    hooks: Vec<(u64, Hook)>,
}

impl Signal {
    pub fn new() -> Signal {
        Signal::default()
    }

    pub fn is_aborted(&self) -> bool {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).aborted
    }

    /// `controller.abort()`: false when it already was.
    pub fn abort(&self) -> bool {
        let hooks = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.aborted {
                return false;
            }
            state.aborted = true;
            std::mem::take(&mut state.hooks)
        };
        for (_, hook) in hooks {
            hook();
        }
        true
    }

    /// `addEventListener('abort', hook, { once: true })`. Already aborted: the hook is not kept and
    /// `None` is returned (callers check [`Signal::is_aborted`] themselves, as core's code does).
    pub fn on_abort(&self, hook: impl FnOnce() + Send + 'static) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.aborted {
            return None;
        }
        state.next += 1;
        let id = state.next;
        state.hooks.push((id, Box::new(hook)));
        Some(id)
    }

    /// `removeEventListener`.
    pub fn off(&self, id: Option<u64>) {
        if let Some(id) = id {
            self.state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .hooks
                .retain(|(h, _)| *h != id);
        }
    }
}

/// A value set once (the first set wins) and waited for: a promise's resolve/reject pair.
pub struct Slot<T: Clone> {
    value: Mutex<Option<T>>,
    set: std::sync::Condvar,
}

impl<T: Clone> Default for Slot<T> {
    fn default() -> Self {
        Slot { value: Mutex::new(None), set: std::sync::Condvar::new() }
    }
}

impl<T: Clone> Slot<T> {
    pub fn new() -> Slot<T> {
        Slot::default()
    }

    /// Settles it; false when it already was.
    pub fn settle(&self, value: T) -> bool {
        let mut slot = self.value.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_some() {
            return false;
        }
        *slot = Some(value);
        self.set.notify_all();
        true
    }

    pub fn wait(&self) -> T {
        let mut slot = self.value.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(v) = slot.as_ref() {
                return v.clone();
            }
            slot = self.set.wait(slot).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// `None` when it is not settled within `limit`.
    pub fn wait_for(&self, limit: std::time::Duration) -> Option<T> {
        let deadline = std::time::Instant::now() + limit;
        let mut slot = self.value.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(v) = slot.as_ref() {
                return Some(v.clone());
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            slot = self.set.wait_timeout(slot, left).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    pub fn peek(&self) -> Option<T> {
        self.value.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}
