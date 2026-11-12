//! The inference admission lease authority (port contract 2026-11-04 row H). It replaces the lease
//! table of core `inference-admission-lease.cjs` (`createLeases`): this front issues every admission
//! lease, owns its lifetime (renewal, the hard cap, expiry, release) and decides when coverage is lost.
//!
//! What a lease holds in Node (its maintenance gate's inference count, the inference memory budget and
//! the gate's one-model queue, a machine's turn count, the Diary exchange's throttle and the backfill's
//! pause) are counts in Node's memory that Node's own synchronous chat and workers read, so Node keeps
//! them as a *latch*: `PUT /internal/inference-admission/lease/{id}` takes them for a lease this
//! authority issued and holds them until this authority gives the lease back (or stops renewing it for
//! a TTL). The latch has no authority of its own: it never issues ids, never extends a lease past this
//! authority's hard cap, and refuses (fails closed) without a session with this authority.
//!
//! Fencing, as the model-maintenance lease authority does it:
//!
//! - every Node process start registers (`POST /v1/inference-admission/session {boot, startedAt}` on
//!   the project-store listener) and is issued the next incarnation, in order, by this authority; an
//!   earlier or ambiguous start is retired (410) and changes nothing;
//! - every latch call carries this authority's `epoch` and the Node incarnation; Node refuses a call of
//!   another epoch or incarnation, so a restarted front never inherits a latch an earlier front took, and
//!   a restarted Node never honours a lease of its previous start;
//! - a lease is bound to the incarnation it was taken under: once a newer Node start registers, the
//!   lease covers nothing (its holder's [`crate::Lease::lost`] fires) and it is never taken again silently;
//! - the lease id is issued before the latch is asked for (a ticket), and the ticket is an RAII guard:
//!   a caller that goes away at any await gives the ticket back, including to Node, so a latch whose
//!   answer was lost is released too.
//!
//! Every table operation is synchronous under a std mutex that is never held across an await.

use rand_core::RngCore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tokio::time::Instant;

/// The release marker the release assembly pairs with core's latch (`noevia-inference-admission-latch/1`).
pub const AUTHORITY_MARKER: &str = "noevia-inference-admission-authority/1";
/// inference-admission-lease.cjs `TTL_MS`: a lease Node does not see renewed within it ends there.
pub const TTL: Duration = Duration::from_millis(30_000);
/// `HARD_MS`: however often it is renewed, a lease ends here.
pub const HARD: Duration = Duration::from_millis(6 * 60 * 60 * 1000);
/// `MAX_WAIT_MS`: the longest Node waits in the engine's queue for a lease.
pub const MAX_WAIT: Duration = Duration::from_millis(120_000);
/// The session route on the project-store listener.
pub const SESSION_ROUTE: &str = "/v1/inference-admission/session";
/// The body limit of [`SESSION_ROUTE`].
pub const BODY_LIMIT: usize = 4096;

/// A Node start's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub incarnation: u64,
    pub epoch: String,
}

/// A Node start earlier than (or ambiguous with) the registered one: 410.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retired;

/// What a ticket names: the lease, this authority's epoch and the Node incarnation it is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub id: String,
    pub epoch: String,
    pub incarnation: u64,
}

struct Entry {
    incarnation: u64,
    hard_at: Instant,
}

struct NodeStart {
    incarnation: u64,
    started: u64,
    boot: String,
}

struct State {
    node: Option<NodeStart>,
    incarnations: u64,
    leases: HashMap<String, Entry>,
}

struct Inner {
    state: Mutex<State>,
    epoch: String,
    changed: tokio::sync::Notify,
    hard: Duration,
}

/// The authority. Cheap to clone; every clone is the same table.
#[derive(Clone)]
pub struct Authority {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority")
            .field("leases", &self.held())
            .finish_non_exhaustive()
    }
}

impl Default for Authority {
    fn default() -> Self {
        Self::new()
    }
}

fn random_hex() -> String {
    let mut b = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// This process's one authority: every [`crate::Admission`] built with [`crate::Admission::new`] and
/// the session route share it.
pub fn global() -> &'static Authority {
    static ONE: OnceLock<Authority> = OnceLock::new();
    ONE.get_or_init(Authority::new)
}

impl Authority {
    pub fn new() -> Self {
        Self::with_hard(HARD)
    }

    pub fn with_hard(hard: Duration) -> Self {
        Authority {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    node: None,
                    incarnations: 0,
                    leases: HashMap::new(),
                }),
                epoch: random_hex(),
                changed: tokio::sync::Notify::new(),
                hard,
            }),
        }
    }

    pub fn epoch(&self) -> &str {
        &self.inner.epoch
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.inner.state.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        }
    }

    /// Registers a Node process start. The same start gets its incarnation again; a later start the
    /// next one (leases of earlier incarnations cover nothing from then on); an earlier or ambiguous
    /// start is [`Retired`] and changes nothing.
    pub fn node_session(&self, boot: &str, started: u64) -> Result<Session, Retired> {
        let mut state = self.lock();
        let incarnation = match state.node.as_ref() {
            Some(n) if n.boot == boot => n.incarnation,
            Some(n) if started <= n.started => return Err(Retired),
            _ => {
                state.incarnations += 1;
                let incarnation = state.incarnations;
                state.node = Some(NodeStart {
                    incarnation,
                    started,
                    boot: boot.to_owned(),
                });
                let stale = state
                    .leases
                    .values()
                    .filter(|e| e.incarnation != incarnation)
                    .count();
                drop(state);
                if stale > 0 {
                    eprintln!("[models] Node restarted; {stale} inference lease(s) of its previous start no longer hold the engine");
                }
                self.inner.changed.notify_waiters();
                return Ok(Session {
                    incarnation,
                    epoch: self.inner.epoch.clone(),
                });
            }
        };
        Ok(Session {
            incarnation,
            epoch: self.inner.epoch.clone(),
        })
    }

    /// Issues a lease id bound to the active Node start. `None` without one: admission fails closed.
    pub fn issue(&self) -> Option<Ticket> {
        let now = Instant::now();
        let mut state = self.lock();
        let incarnation = state.node.as_ref()?.incarnation;
        let id = random_hex();
        state.leases.insert(
            id.clone(),
            Entry {
                incarnation,
                hard_at: now + self.inner.hard,
            },
        );
        Some(Ticket {
            id,
            epoch: self.inner.epoch.clone(),
            incarnation,
        })
    }

    /// Whether lease `id` still holds: issued here, not ended, bound to the active Node start and
    /// before its hard cap.
    pub fn covers(&self, id: &str) -> bool {
        let now = Instant::now();
        let state = self.lock();
        let active = state.node.as_ref().map(|n| n.incarnation);
        state
            .leases
            .get(id)
            .is_some_and(|e| Some(e.incarnation) == active && now < e.hard_at)
    }

    /// When lease `id` reaches its hard cap.
    pub fn hard_at(&self, id: &str) -> Option<Instant> {
        self.lock().leases.get(id).map(|e| e.hard_at)
    }

    /// Ends lease `id`. False when it had already ended.
    pub fn end(&self, id: &str) -> bool {
        let gone = self.lock().leases.remove(id).is_some();
        if gone {
            self.inner.changed.notify_waiters();
        }
        gone
    }

    /// How many leases are held.
    pub fn held(&self) -> usize {
        self.lock().leases.len()
    }

    /// Resolves at the next change of the table or of the Node start. Enable it before checking
    /// [`Self::covers`] so a change in between is not missed.
    pub fn changed(&self) -> tokio::sync::futures::Notified<'_> {
        self.inner.changed.notified()
    }

    /// The session route's answer: `(status, body)`.
    pub fn answer_session(&self, body: &serde_json::Value) -> (u16, serde_json::Value) {
        let boot = body
            .get("boot")
            .and_then(serde_json::Value::as_str)
            .filter(|b| !b.is_empty() && b.len() <= 128);
        let started = body.get("startedAt").and_then(serde_json::Value::as_u64);
        let (Some(boot), Some(started)) = (boot, started) else {
            return (
                400,
                serde_json::json!({ "error": "A boot and a start time are required." }),
            );
        };
        match self.node_session(boot, started) {
            Ok(s) => (
                200,
                serde_json::json!({
                    "incarnation": s.incarnation,
                    "epoch": s.epoch,
                    "ttlMs": TTL.as_millis() as u64,
                }),
            ),
            Err(Retired) => (
                410,
                serde_json::json!({ "error": "A newer start of this server has registered." }),
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn no_node_session_issues_nothing() {
        let a = Authority::new();
        assert!(a.issue().is_none());
    }

    #[test]
    fn incarnations_are_ordered_and_retired_starts_change_nothing() {
        let a = Authority::new();
        let one = a.node_session("a", 10).unwrap();
        assert_eq!(a.node_session("a", 10).unwrap(), one);
        let t = a.issue().unwrap();
        assert!(a.covers(&t.id));
        let two = a.node_session("b", 20).unwrap();
        assert!(two.incarnation > one.incarnation);
        assert!(!a.covers(&t.id), "a lease of the previous Node start");
        // An old start's delayed session, and an ambiguous one, are retired.
        assert_eq!(a.node_session("a", 10), Err(Retired));
        assert_eq!(a.node_session("c", 20), Err(Retired));
        assert_eq!(
            a.node_session("b", 20).unwrap().incarnation,
            two.incarnation
        );
        let t2 = a.issue().unwrap();
        assert_eq!(t2.incarnation, two.incarnation);
        assert!(a.covers(&t2.id));
        assert!(a.end(&t2.id));
        assert!(!a.end(&t2.id));
        assert!(!a.covers(&t2.id));
    }

    #[tokio::test]
    async fn the_hard_cap_ends_coverage() {
        let a = Authority::with_hard(Duration::from_millis(30));
        a.node_session("a", 1).unwrap();
        let t = a.issue().unwrap();
        assert!(a.covers(&t.id));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!a.covers(&t.id));
    }

    #[test]
    fn epochs_differ_between_authorities() {
        assert_ne!(Authority::new().epoch(), Authority::new().epoch());
    }

    #[test]
    fn the_session_answer_validates_and_retires() {
        let a = Authority::new();
        assert_eq!(a.answer_session(&serde_json::json!({})).0, 400);
        let (s, b) = a.answer_session(&serde_json::json!({"boot":"x","startedAt":5}));
        assert_eq!(
            (s, b.get("incarnation").and_then(serde_json::Value::as_u64)),
            (200, Some(1))
        );
        assert_eq!(
            b.get("ttlMs").and_then(serde_json::Value::as_u64),
            Some(30_000)
        );
        assert_eq!(
            a.answer_session(&serde_json::json!({"boot":"y","startedAt":4}))
                .0,
            410
        );
    }
}
