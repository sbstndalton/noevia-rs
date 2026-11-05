//! The per-chat brain schedule (core chat-brain.cjs `createBrainScheduler`, its `schedule` and
//! the retry in `run`).
//!
//! Every change to a chat restarts that chat's idle timer; when the chat has been quiet for
//! `idle`, its brain is built (the executor skips a chat whose stored brain already matches) and
//! the mirror is nudged for that chat. Builds run one at a time across all accounts, in the order
//! their timers fired, so the reasoner never runs twice at once. A failed build keeps the previous
//! brain and is retried once after `retry`, but only for the reasons that can pass on their own.
//!
//! Deviation from the Node scheduler, recorded: when a build fails while a newer change has
//! already set the chat's idle timer, the retry does not replace that timer (Node's retry cleared
//! it, delaying the newer change's build by `retry`).
use crate::timer::{lock, Timer};
use crate::{BrainExecutor, Log};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct Timings {
    pub idle: Duration,
    pub retry: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            idle: Duration::from_secs(3 * 60),
            retry: Duration::from_secs(15 * 60),
        }
    }
}

/// The reasons a failed build is worth one retry (a missing model or an empty chat is not).
pub const RETRYABLE: [&str; 5] = ["deadline", "error", "budget", "invalid-json", "schema"];

/// Called after a new brain was stored: the mirror is nudged for that chat.
pub type Built = Arc<dyn Fn(&str, &str) + Send + Sync>;

type Key = (String, String);

struct Inner {
    timers: Mutex<HashMap<Key, Timer>>,
    gate: tokio::sync::Mutex<()>,
    next: AtomicU64,
    timings: Timings,
    exec: Arc<dyn BrainExecutor>,
    built: Built,
    log: Arc<dyn Log>,
}

#[derive(Clone)]
pub struct Brains {
    inner: Arc<Inner>,
}

impl Brains {
    pub fn new(
        exec: Arc<dyn BrainExecutor>,
        built: Built,
        log: Arc<dyn Log>,
        timings: Timings,
    ) -> Self {
        Brains {
            inner: Arc::new(Inner {
                timers: Mutex::new(HashMap::new()),
                gate: tokio::sync::Mutex::new(()),
                next: AtomicU64::new(1),
                timings,
                exec,
                built,
                log,
            }),
        }
    }

    /// A change to `chat`: its brain is built after it has been quiet for the idle time.
    pub fn schedule(&self, user: &str, chat: &str) {
        arm(
            &self.inner,
            user,
            chat,
            self.inner.timings.idle,
            false,
            true,
        );
    }

    pub fn pending(&self, user: &str, chat: &str) -> bool {
        lock(&self.inner.timers).contains_key(&(user.to_string(), chat.to_string()))
    }
}

fn arm(inner: &Arc<Inner>, user: &str, chat: &str, delay: Duration, retried: bool, replace: bool) {
    if user.is_empty() || chat.is_empty() {
        return;
    }
    let key: Key = (user.to_string(), chat.to_string());
    let mut timers = lock(&inner.timers);
    if !replace && timers.contains_key(&key) {
        return;
    }
    let generation = inner.next.fetch_add(1, Ordering::Relaxed);
    let weak: Weak<Inner> = Arc::downgrade(inner);
    let k = key.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        if let Some(inner) = weak.upgrade() {
            fire(inner, k, generation, retried).await;
        }
    });
    // Replacing the old timer drops it, which stops it.
    timers.insert(key, Timer::new(generation, handle));
}

async fn fire(inner: Arc<Inner>, key: Key, generation: u64, retried: bool) {
    {
        let mut timers = lock(&inner.timers);
        match timers.remove(&key) {
            Some(t) if t.generation == generation => t.disarm(),
            Some(other) => {
                timers.insert(key, other);
                return;
            }
            None => return,
        }
    }
    // One build at a time across all accounts, in the order the timers fired.
    let _turn = inner.gate.lock().await;
    let exec = Arc::clone(&inner.exec);
    let reason = match tokio::spawn(exec.run(&key.0, &key.1)).await {
        Ok(r) => r,
        Err(_) => {
            inner
                .log
                .line("chat-brain: build failed (executor stopped)");
            "error".to_string()
        }
    };
    if reason == "built" {
        // The mirror nudge never fails a build.
        (inner.built)(&key.0, &key.1);
    } else if !retried && RETRYABLE.contains(&reason.as_str()) {
        arm(&inner, &key.0, &key.1, inner.timings.retry, true, false);
    }
}
