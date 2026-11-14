//! sbstndalton/noevia#2641: Node's recovery from lost lease coverage (core
//! `inference-maintenance.cjs` `requireRecovery`, `scheduleRecovery`, `runRecovery`), for every lease of
//! this front on Node's gate. Only with `NOEVIA_RUST_LEASE_COVERAGE=node` ([`super::gate::Coverage`]).
//! The calibration worker and the native auto-tune runner keep their own repairs (a restore, the
//! trial's recovery, each with its own settle); while one is owed the driver waits, and a recovery
//! they completed discharges the driver's bare settle.
//!
//! A hold that lost its coverage owes `settle` when it is let go. The driver ([`drive`]) then runs one
//! attempt at a time: a fresh recovery hold (never the lost one), the settle step (every model unloaded
//! and the engine confirmed idle), each registered repair with a coverage check before and after, and
//! only then the recovery's completion (Node's proof and the authority's ticket), which opens chat
//! again. A failed attempt is retried after 5 s, doubling to 60 s (`recoveryRetryMs`,
//! `recoveryMaxDelayMs`). An attempt while another job of this process holds its admission waits
//! `recoveryRetryMs`, as Node's does while `maintenance` is set.
//!
//! Differences from Node, named: Node releases a failed attempt's hold before it schedules the
//! backoff, and that release (recoveries still pending) schedules the next attempt at 0 ms, so a
//! failed settle is retried at once (fixtures/maint-gate/README.md); this driver applies the backoff
//! Node intends. A recovery another worker already completed at the authority (its own covered
//! settle) discharges a bare `settle` without a second one.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The key a lost hold owes (Node's release: `requireRecovery('settle')`).
pub const SETTLE: &str = "settle";
const FINISHED: &str =
    "[models] recovery from interrupted model maintenance finished; chat may run again";

/// A registered repair (Node's `repair(ctx)`); `Err` to be retried.
pub type Repair =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// Node's `recoveries` map (key → repair or none), in insertion order.
pub struct Registry {
    keys: Mutex<Vec<(String, Option<Repair>)>>,
    wake: tokio::sync::Notify,
    kicked: AtomicBool,
    /// `recoveryRetryMs`.
    pub retry: Duration,
    /// `recoveryMaxDelayMs`.
    pub max_delay: Duration,
}

impl Default for Registry {
    fn default() -> Self {
        Registry::with_timing(Duration::from_secs(5), Duration::from_secs(60))
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("pending", &self.pending())
            .finish_non_exhaustive()
    }
}

/// What one attempt came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing was owed.
    Idle,
    /// Another job of this process holds its admission: try again after `retry`.
    Busy,
    /// It did not finish (the message was logged): retried with backoff.
    Failed,
    /// Every key owed when it started ran (more may have been owed meanwhile).
    Done,
    /// Another worker's covered recovery already completed the authority's ticket.
    Discharged,
}

fn failed_line(why: &str) -> String {
    format!("[models] recovery from interrupted model maintenance did not finish ({why}); chat stays paused and it is tried again")
}

impl Registry {
    pub fn with_timing(retry: Duration, max_delay: Duration) -> Registry {
        Registry {
            keys: Mutex::new(Vec::new()),
            wake: tokio::sync::Notify::new(),
            kicked: AtomicBool::new(false),
            retry,
            max_delay,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(String, Option<Repair>)>> {
        self.keys.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `requireRecovery(key, repair)`: owed until an attempt runs it; a repair given replaces the
    /// key's. Does not start an attempt ([`Self::kick`]).
    pub fn require(&self, key: &str, repair: Option<Repair>) {
        let mut keys = self.lock();
        match keys.iter_mut().find(|(k, _)| k == key) {
            Some((_, slot)) => {
                if repair.is_some() {
                    *slot = repair;
                }
            }
            None => keys.push((key.to_owned(), repair)),
        }
    }

    /// `recoveryPending()`.
    pub fn pending(&self) -> bool {
        !self.lock().is_empty()
    }

    /// `scheduleRecovery(0)`: an attempt starts now unless one runs or is already scheduled.
    pub fn kick(&self) {
        self.kicked.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }

    /// Takes a kick (a kick while an attempt runs or one is scheduled changes nothing).
    pub(crate) fn take_kick(&self) -> bool {
        self.kicked.swap(false, Ordering::AcqRel)
    }

    fn entries(&self) -> Vec<(String, Option<Repair>)> {
        self.lock().clone()
    }

    fn done(&self, key: &str) {
        self.lock().retain(|(k, _)| k != key);
    }

    /// Drops the keys that have no repair of their own (a bare settle).
    fn discharge(&self) {
        self.lock().retain(|(_, r)| r.is_some());
    }

    /// The `finally` of `runRecovery`: the next attempt's delay (none: wait for a kick) and the line
    /// it logs. `delay` is `recoveryDelay`.
    pub fn after(
        &self,
        delay: &mut Duration,
        outcome: &Outcome,
    ) -> (Option<Duration>, Option<String>) {
        match outcome {
            Outcome::Idle => (None, None),
            Outcome::Busy => (Some(self.retry), None),
            Outcome::Discharged if !self.pending() => {
                *delay = Duration::ZERO;
                (None, None)
            }
            Outcome::Done if !self.pending() => {
                *delay = Duration::ZERO;
                (None, Some(FINISHED.to_owned()))
            }
            _ if self.pending() => {
                let doubled = if delay.is_zero() {
                    self.retry
                } else {
                    *delay * 2
                };
                *delay = self.max_delay.min(self.retry.max(doubled));
                (Some(*delay), None)
            }
            _ => (None, None),
        }
    }
}

/// What an attempt does with the outside world (the front's: [`super::Maint`]'s).
pub trait Env: Send + Sync {
    type Hold: Send + Sync;
    /// Dropping it gives this process's admission back.
    type Fence: Send;
    /// This process's admission for the attempt; `None` while another job of it holds it.
    fn reserve(&self) -> Option<Self::Fence>;
    /// Whether the authority's recovery is no longer required (another worker completed it).
    fn discharged(&self) -> bool;
    /// A fresh recovery hold; `Err(message)` when it cannot be taken.
    fn hold(&self) -> impl Future<Output = Result<Self::Hold, String>> + Send;
    /// The settle step (Node's `setRecoverySettle`).
    fn settle(&self, hold: &Self::Hold) -> impl Future<Output = Result<(), String>> + Send;
    /// `assertCovered()` on the fresh hold.
    fn covered(&self, hold: &Self::Hold) -> Result<(), String>;
    /// Opens admission again: the recovery proven and completed.
    fn complete(&self, hold: &Self::Hold) -> impl Future<Output = Result<(), String>> + Send;
    fn release(&self, hold: Self::Hold) -> impl Future<Output = ()> + Send;
    fn log(&self, line: String);
}

/// One `runRecovery` attempt.
pub async fn attempt<E: Env>(registry: &Registry, env: &E) -> Outcome {
    if !registry.pending() {
        return Outcome::Idle;
    }
    let Some(fence) = env.reserve() else {
        return Outcome::Busy;
    };
    if env.discharged() {
        registry.discharge();
        if !registry.pending() {
            return Outcome::Discharged;
        }
    }
    let hold = match env.hold().await {
        Ok(h) => h,
        Err(why) => {
            env.log(failed_line(&why));
            return Outcome::Failed;
        }
    };
    let run = async {
        env.settle(&hold).await?;
        for (key, repair) in registry.entries() {
            env.covered(&hold)?;
            if let Some(repair) = repair {
                repair().await?;
            }
            env.covered(&hold)?;
            registry.done(&key);
        }
        if registry.pending() {
            return Ok(false);
        }
        if let Err(why) = env.complete(&hold).await {
            // The owed settle stays owed, so it is tried again.
            registry.require(SETTLE, None);
            return Err(why);
        }
        Ok(true)
    };
    let outcome = match run.await {
        Ok(_) => Outcome::Done,
        Err(why) => {
            env.log(failed_line(&why));
            Outcome::Failed
        }
    };
    env.release(hold).await;
    drop(fence);
    outcome
}

/// The driver: runs an attempt when kicked, or when the backoff it scheduled falls due. Runs forever.
pub async fn drive<E: Env>(registry: Arc<Registry>, env: E) {
    let mut delay = Duration::ZERO;
    let mut due: Option<tokio::time::Instant> = None;
    loop {
        match due {
            Some(at) => tokio::time::sleep_until(at).await,
            None => {
                let kicked = registry.wake.notified();
                tokio::pin!(kicked);
                kicked.as_mut().enable();
                if !(registry.take_kick() && registry.pending()) {
                    kicked.await;
                    continue;
                }
            }
        }
        let outcome = attempt(&registry, &env).await;
        registry.take_kick();
        let (next, line) = registry.after(&mut delay, &outcome);
        if let Some(line) = line {
            env.log(line);
        }
        due = next.map(|d| tokio::time::Instant::now() + d);
    }
}

/// The recovery hold's limit (as calibration's recovery asks) and its reason.
const LEASE: Duration = Duration::from_secs(15 * 60);
const REASON: &str = "Chat is paused while noevia recovers from interrupted model maintenance.";
/// Node's `RECOVERY_SETTLE_MS` and `RECOVERY_POLL_MS`.
const SETTLE_LIMIT: Duration = Duration::from_secs(120);
const SETTLE_POLL: Duration = Duration::from_millis(500);

/// The front's side of an attempt: this process's admission, the authority's recovery hold, the
/// router and Node's recovery proof.
pub struct FrontEnv {
    pub(super) maint: Arc<super::Maint>,
}

/// This process's admission, held for one attempt.
pub struct Fence(Option<super::gate::AdmissionFence>);

impl Drop for Fence {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f.release();
        }
    }
}

/// A fresh recovery hold and the authority's recovery ticket it was taken for.
pub struct Fresh {
    hold: super::Hold,
    ticket: String,
}

/// Node's covered dispatch: `assertCovered()` first, and the call ends with the coverage error once
/// the hold is lost while it runs.
pub(super) async fn dispatch<T>(
    hold: &super::Hold,
    call: impl Future<Output = T>,
) -> Result<T, String> {
    hold.assert_covered(Duration::ZERO)?;
    tokio::select! {
        r = call => Ok(r),
        () = hold.lost_wait() => Err(hold.coverage_error()),
    }
}

fn status(row: &serde_json::Value) -> Option<&str> {
    row.get("status")?.get("value")?.as_str()
}

impl Env for FrontEnv {
    type Hold = Fresh;
    type Fence = Fence;

    /// Also busy while the auto-tune runner owes repairs: its repair worker runs their settle and
    /// completes the recovery (the calibration worker keeps admission itself until its own has run).
    fn reserve(&self) -> Option<Fence> {
        if self.maint.repairs_owed.load(Ordering::Acquire) > 0 {
            return None;
        }
        super::gate::AdmissionFence::reserve(Arc::clone(&self.maint.local)).map(|f| Fence(Some(f)))
    }

    fn discharged(&self) -> bool {
        !self.maint.authority.recovery_pending()
    }

    async fn hold(&self) -> Result<Fresh, String> {
        match self.maint.acquire_recovery(REASON, LEASE).await {
            Ok(hold) => Ok(Fresh {
                ticket: self.maint.authority.recovery_ticket().unwrap_or_default(),
                hold,
            }),
            Err(super::HoldRefusal::Busy) => Err("Inference is busy.".into()),
            Err(_) => Err("The maintenance lease authority is not answering, so nothing was changed. Try again shortly.".into()),
        }
    }

    /// Node's settle step (llamacpp-manager.cjs `setRecoverySettle`): until the router lists no model
    /// loaded, loading, sleeping or unloading, every loaded or sleeping one is asked to unload; bounded.
    async fn settle(&self, fresh: &Fresh) -> Result<(), String> {
        let Some(router) = self.maint.router.as_ref() else {
            return Ok(());
        };
        let hold = &fresh.hold;
        let token = hold.token();
        let deadline = tokio::time::Instant::now() + SETTLE_LIMIT;
        loop {
            let listing = dispatch(hold, router.models(token.as_deref())).await?;
            let Some(rows) = listing
                .ok()
                .filter(|r| r.ok)
                .and_then(|r| super::router::rows(&r.body).cloned())
            else {
                return Err("The model server did not answer.".into());
            };
            let live: Vec<&serde_json::Value> = rows
                .iter()
                .filter(|r| {
                    matches!(
                        status(r),
                        Some("loaded" | "loading" | "sleeping" | "unloading")
                    )
                })
                .collect();
            if live.is_empty() {
                return Ok(());
            }
            for row in live {
                if matches!(status(row), Some("loaded" | "sleeping")) {
                    if let Some(id) = row.get("id").and_then(serde_json::Value::as_str) {
                        let _ = dispatch(hold, router.unload(id, token.as_deref())).await?;
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("The model server did not confirm every model stopped.".into());
            }
            tokio::time::sleep(SETTLE_POLL).await;
        }
    }

    fn covered(&self, fresh: &Fresh) -> Result<(), String> {
        fresh.hold.assert_covered(Duration::ZERO)
    }

    /// The recovery proven to Node under the fresh hold, then completed at the authority, as the
    /// other recoveries do; otherwise it stays required (and Node's barrier up).
    async fn complete(&self, fresh: &Fresh) -> Result<(), String> {
        let (hold, ticket) = (&fresh.hold, fresh.ticket.as_str());
        let proven = hold.excluded()
            && (hold.gated_now()
                || match hold.recovery_proof() {
                    Some((id, generation)) => {
                        self.maint
                            .node
                            .complete_recovery(ticket, id, generation)
                            .await
                    }
                    None => false,
                })
            && hold.excluded();
        if proven && self.maint.authority.complete_recovery(ticket) {
            return Ok(());
        }
        self.maint.authority.require_recovery();
        if !hold.gated_now() {
            let _ = self.maint.node.recovery_barrier(true, ticket).await;
        }
        Err("The model service did not confirm the recovery.".into())
    }

    async fn release(&self, fresh: Fresh) {
        fresh.hold.release().await;
    }

    fn log(&self, line: String) {
        eprintln!("{line}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_is_nodes_5_s_doubling_to_60_s() {
        let r = Registry::default();
        r.require(SETTLE, None);
        let mut delay = Duration::ZERO;
        let seen: Vec<u64> = (0..7)
            .map(|_| r.after(&mut delay, &Outcome::Failed).0.unwrap().as_secs())
            .collect();
        assert_eq!(seen, [5, 10, 20, 40, 60, 60, 60]);
        assert_eq!(r.after(&mut delay, &Outcome::Busy).0, Some(r.retry));
        assert_eq!(
            delay,
            Duration::from_secs(60),
            "a busy retry keeps the delay"
        );
        r.done(SETTLE);
        let (next, line) = r.after(&mut delay, &Outcome::Done);
        assert_eq!((next, line.as_deref()), (None, Some(FINISHED)));
        assert_eq!(delay, Duration::ZERO);
    }

    #[test]
    fn a_key_is_owed_once_and_a_repair_given_later_replaces_none() {
        let r = Registry::default();
        r.require(SETTLE, None);
        r.require("qa:repair", None);
        let repair: Repair = Arc::new(|| Box::pin(async { Ok(()) }));
        r.require("qa:repair", Some(repair));
        r.require("qa:repair", None);
        let keys = r.entries();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].0, SETTLE);
        assert!(keys[1].1.is_some(), "a later bare require keeps the repair");
        r.discharge();
        assert_eq!(r.entries().len(), 1, "only the bare settle is discharged");
    }
}
