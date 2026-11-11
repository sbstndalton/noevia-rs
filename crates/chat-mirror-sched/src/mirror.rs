//! The per-account Diary mirror schedule (core chat-vault-mirror.cjs `createChatVaultMirror`,
//! its `schedule` and `run`).
//!
//! - Saves are debounced per account (`delay`), but an account whose chats keep changing is
//!   still looked at at least every `max_wait` from the first change of the burst.
//! - One pass at a time per account. A change that lands while one runs is remembered
//!   (`again`) and starts the next pass on the debounce once the running one ends.
//! - The chats named since the last pass (the dirty set) travel with the pass; a failed pass puts
//!   them back. An empty set means "look at everything" (a rescan, a project change, a nudge).
//! - A failed pass backs off 1, 2, 4 ... times `retry`, capped at `max_retry` and then repeating
//!   there; the step is logged once. Any new change starts the backoff over.
//! - An account with nothing pending and nothing running is forgotten.
//!
//! Failure isolation: each account has its own state and its own timer; an executor that fails or
//! panics fails that account's pass only.
use crate::timer::{lock, Timer};
use crate::{Log, MirrorExecutor, MirrorOutcome};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct Timings {
    pub delay: Duration,
    pub max_wait: Duration,
    pub retry: Duration,
    pub max_retry: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            delay: Duration::from_millis(5_000),
            max_wait: Duration::from_millis(30_000),
            retry: Duration::from_millis(60_000),
            max_retry: Duration::from_secs(60 * 60),
        }
    }
}

struct User {
    timer: Option<Timer>,
    first_at: Instant,
    dirty: HashSet<String>,
    running: bool,
    again: bool,
    failures: u32,
    logged_delay: Duration,
}

struct Inner {
    users: Mutex<HashMap<String, User>>,
    next: AtomicU64,
    timings: Timings,
    exec: Arc<dyn MirrorExecutor>,
    log: Arc<dyn Log>,
}

#[derive(Clone)]
pub struct Mirror {
    inner: Arc<Inner>,
}

impl Mirror {
    pub fn new(exec: Arc<dyn MirrorExecutor>, log: Arc<dyn Log>, timings: Timings) -> Self {
        Mirror {
            inner: Arc::new(Inner {
                users: Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
                timings,
                exec,
                log,
            }),
        }
    }

    /// Asks for `user`'s mirror to look again: at `chat` if named, at everything otherwise.
    /// `immediate` (the person just switched the mirror on) skips the debounce.
    pub fn schedule(&self, user: &str, chat: Option<&str>, immediate: bool) {
        if user.is_empty() {
            return;
        }
        let mut users = lock(&self.inner.users);
        let u = users.entry(user.to_string()).or_insert_with(|| User {
            timer: None,
            first_at: Instant::now(),
            dirty: HashSet::new(),
            running: false,
            again: false,
            failures: 0,
            logged_delay: Duration::ZERO,
        });
        if let Some(chat) = chat.filter(|c| !c.is_empty()) {
            u.dirty.insert(chat.to_string());
        }
        // A new nudge (a fresh save, or the person switching the mirror on) starts the backoff over.
        u.failures = 0;
        u.logged_delay = Duration::ZERO;
        arm_debounce(&self.inner, user, u, immediate);
    }

    /// Whether the account has a pass pending or running.
    pub fn pending(&self, user: &str) -> bool {
        lock(&self.inner.users).contains_key(user)
    }
}

/// The debounce step shared by `schedule` and the again-after-a-pass paths. Called with the
/// account's state locked.
fn arm_debounce(inner: &Arc<Inner>, user: &str, u: &mut User, immediate: bool) {
    if u.running {
        u.again = true;
        return;
    }
    let now = Instant::now();
    if u.timer.take().is_none() {
        u.first_at = now;
    }
    // Debounced, but a chat that keeps changing is still mirrored at least every max_wait.
    let wait = if immediate {
        Duration::ZERO
    } else {
        inner
            .timings
            .delay
            .min((u.first_at + inner.timings.max_wait).saturating_duration_since(now))
    };
    set_timer(inner, user, u, wait);
}

fn set_timer(inner: &Arc<Inner>, user: &str, u: &mut User, wait: Duration) {
    let generation = inner.next.fetch_add(1, Ordering::Relaxed);
    let weak: Weak<Inner> = Arc::downgrade(inner);
    let who = user.to_string();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        if let Some(inner) = weak.upgrade() {
            fire(inner, who, generation).await;
        }
    });
    u.timer = Some(Timer::new(generation, handle));
}

async fn fire(inner: Arc<Inner>, user: String, generation: u64) {
    {
        let mut users = lock(&inner.users);
        let Some(u) = users.get_mut(&user) else {
            return;
        };
        match u.timer.take() {
            Some(t) if t.generation == generation => t.disarm(),
            other => {
                // A newer timer replaced this one while it was waking: it is not ours to run.
                u.timer = other;
                return;
            }
        }
    }
    run(inner, user).await;
}

async fn run(inner: Arc<Inner>, user: String) {
    let dirty: HashSet<String> = {
        let mut users = lock(&inner.users);
        let Some(u) = users.get_mut(&user) else {
            return;
        };
        if u.running {
            return;
        }
        u.running = true;
        u.again = false;
        std::mem::take(&mut u.dirty)
    };
    let exec = Arc::clone(&inner.exec);
    let who = user.clone();
    let list: Vec<String> = dirty.iter().cloned().collect();
    // The executor runs as its own task: a panic fails this account's pass, not the scheduler.
    let outcome = match tokio::spawn(exec.sync(&who, list)).await {
        Ok(o) => o,
        Err(_) => {
            inner
                .log
                .line("chat-vault-mirror: sync failed (executor stopped)");
            MirrorOutcome::Failed
        }
    };
    let mut users = lock(&inner.users);
    let Some(u) = users.get_mut(&user) else {
        return;
    };
    u.running = false;
    let failed = outcome == MirrorOutcome::Failed;
    if failed && u.again {
        // Nudged while this pass ran: the nudge already reset the backoff; go again on the debounce.
        u.dirty.extend(dirty);
        u.again = false;
        u.failures = 0;
        u.logged_delay = Duration::ZERO;
        arm_debounce(&inner, &user, u, false);
    } else if failed {
        // Exponential backoff (#758): 1, 2, 4 ... x retry, capped at max_retry, then hourly for good.
        u.dirty.extend(dirty);
        u.failures = u.failures.saturating_add(1);
        let step = u.failures.saturating_sub(1).min(30);
        let wait = inner
            .timings
            .max_retry
            .min(inner.timings.retry.saturating_mul(1u32 << step.min(20)));
        if wait != u.logged_delay {
            u.logged_delay = wait;
            inner.log.line(&format!(
                "chat-vault-mirror: retrying in {}s after {} failed pass(es)",
                wait.as_secs_f64().round(),
                u.failures
            ));
        }
        u.first_at = Instant::now();
        set_timer(&inner, &user, u, wait);
    } else if u.again {
        u.again = false;
        u.failures = 0;
        u.logged_delay = Duration::ZERO;
        arm_debounce(&inner, &user, u, false);
    } else if u.timer.is_none() {
        users.remove(&user);
    }
}
