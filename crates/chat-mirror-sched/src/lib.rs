//! The chat change-journal consumer and the Diary mirror / per-chat brain schedulers
//! (sbstndalton/noevia#2153, #741, #742), ported from noevia-core `chat-change-journal.cjs`,
//! `chat-vault-mirror.cjs` (the scheduling half) and `chat-brain.cjs` (the scheduling half).
//!
//! What lives here is the *when*: which users are read, how a journal entry becomes a scheduled
//! mirror pass or brain build, the debounce, the coalescing, the backoff and the retry. What a
//! mirror pass or a brain build *does* (Diary file writes, the reasoner call) is the work
//! executor behind [`MirrorExecutor`] and [`BrainExecutor`]; the server binary supplies one that
//! calls the Node process's private loopback endpoints. That executor is the part still to port.
//!
//! No lock is held across an await, every timer is owned by the scheduler that set it (dropping
//! the scheduler stops them), and an executor that fails or panics fails only its own user's pass.

use std::future::Future;
use std::pin::Pin;

pub mod brain;
pub mod journal;
pub mod mirror;
mod timer;

/// Grep marker for noevia deploy/tools/assemble-release.sh: this build consumes the chat change
/// journal natively and schedules the Diary mirror and the chat brains (the paired Core keeps only
/// the work executors).
pub const MARKER: &str = "noevia-chat-mirror-scheduler/1";

/// A boxed, owned future (the executors' methods return one so a scheduler can spawn it and
/// survive its panic).
pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// What one mirror pass came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorOutcome {
    /// Every chat of the pass is in the vault.
    Done,
    /// The mirror is off (or unavailable) for this account: nothing was written, nothing to retry.
    Off,
    /// Some note failed, or the executor could not be reached: retried on the backoff.
    Failed,
}

/// The mirror's work: one pass for one account over the chats that changed since the last one
/// (`dirty`; empty means "look at everything"). Never called twice at once for one account.
pub trait MirrorExecutor: Send + Sync + 'static {
    fn sync(&self, user: &str, dirty: Vec<String>) -> BoxFut<MirrorOutcome>;
}

/// The reason a brain build ended: `built`, `current`, `off`, `gone`, or a builder fallback
/// (`deadline`, `error`, `budget`, `invalid-json`, `schema`, `empty`, `no-model`).
pub type BrainReason = String;

/// The brain's work: build (or find current) one chat's brain. The scheduler runs one at a time
/// across all accounts.
pub trait BrainExecutor: Send + Sync + 'static {
    fn run(&self, user: &str, chat: &str) -> BoxFut<BrainReason>;
}

/// Where the scheduling code writes what Node's `log` callbacks wrote.
pub trait Log: Send + Sync + 'static {
    fn line(&self, text: &str);
}

/// Logs to stderr, as the server's other workers do.
pub struct StderrLog;

impl Log for StderrLog {
    fn line(&self, text: &str) {
        eprintln!("noevia-server: {text}");
    }
}
