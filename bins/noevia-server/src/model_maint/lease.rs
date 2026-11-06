//! The model-maintenance lease authority (port contract 2026-11-04 row J). It replaces core
//! `model-maintenance-lease.cjs` as the single arbiter of the exclusive maintenance hold that the
//! preset reload and calibration of this front share with Node's chat, tune, calibration, sweep,
//! idle-unload, delete and prune.
//!
//! Exactly one lease exists at a time. Its holder is this front (a [`super::Hold`] taken by
//! [`super::Maint::acquire`]) or Node (its maintenance gate, through the private store listener:
//! `POST /v1/maintenance-lease/{acquire,renew,release,held}` with the per-start secret; core
//! `model-maintenance-lease.cjs` `createNativeLeaseClient`). The timing is Core's, unchanged:
//!
//! - a lease not renewed within [`TTL`] (180 s) ends by itself, so a holder that dies frees the others;
//! - it is renewed every [`RENEW`] (10 s);
//! - however often it is renewed it ends at its hard cap: [`HARD`] (60 min) unless the holder asks
//!   for longer (`maxMs`, calibration), clamped to [`HARD`, [`MAX`] (24 h)].
//!
//! Fencing: each grant carries a fresh random id and a generation that only grows. A holder acts only
//! while [`Authority::covers`] its exact (id, generation): once its lease has expired, been released
//! or been replaced, nothing it holds is honoured again (renew answers ended; release is a no-op).
//! A restart starts an empty table with a new `epoch`: no lease of an earlier process is granted again.
//! A Node that restarts announces a new `boot`; leases a previous Node boot held are dropped then, so a
//! crashed Node does not keep everyone waiting for its TTL.
//!
//! Node's chat admission is synchronous and stays in Node, so a lease this front holds is backed by a
//! hold of Node's local admission (the latch, core `model-maintenance-lease.cjs` routes, reached
//! through [`super::node_lease`]); the latch has no authority of its own and ends with this lease.
//!
//! Every table operation is synchronous under a std mutex that is never held across an await.

use rand_core::RngCore;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::time::Instant;

/// The release marker Noevia's release assembly pairs with core's client (`noevia-maintenance-lease-client/2`).
pub const AUTHORITY_MARKER: &str = "noevia-maintenance-lease-authority/2";

/// model-maintenance-lease.cjs `TTL_MS`.
pub const TTL: Duration = Duration::from_millis(180_000);
/// `RENEW_MS`.
pub const RENEW: Duration = Duration::from_millis(10_000);
/// `HARD_MS`.
pub const HARD: Duration = Duration::from_millis(60 * 60 * 1000);
/// `MAX_MS`.
pub const MAX: Duration = Duration::from_millis(24 * 60 * 60 * 1000);
/// The longest a Node acquire may wait for the current holder (inference-maintenance.cjs asks ≤30 s).
pub const MAX_WAIT: Duration = Duration::from_secs(30);

/// Who holds a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// This front's own work.
    Front,
    /// Node's maintenance gate, for the Node incarnation this authority issued at its session.
    Node { incarnation: u64 },
}

/// The answer to a Node process start's session ([`Authority::node_session`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    /// The server-issued, strictly growing incarnation: every later operation carries it.
    pub incarnation: u64,
    /// A lease this front holds exists: the new Node must keep chat closed until it ends.
    pub front_held: bool,
}

/// A lease granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub id: String,
    pub generation: u64,
    pub epoch: String,
    pub ttl: Duration,
    pub renew_every: Duration,
    pub hard: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Another holder has it: 409.
    Busy,
    /// The caller's Node incarnation is not the active one (a newer Node registered): 410.
    Retired,
}

struct Entry {
    id: String,
    generation: u64,
    origin: Origin,
    renewed_until: Instant,
    hard_at: Instant,
    reason: String,
    /// A front lease: the Node incarnation whose chat latch backs it.
    bound_node: Option<u64>,
    /// A front lease whose Node restarted: it still excludes (until released or expired) but covers
    /// nothing, so no further step starts on it.
    fenced: bool,
    recovery_ticket: Option<String>,
}

/// The active Node process start: its incarnation and the (start time, boot) it registered with.
struct NodeStart {
    incarnation: u64,
    started: u64,
    boot: String,
}

struct State {
    journal_available: bool,
    pending_operations: bool,
    recovery_required: bool,
    recovery_ticket: Option<String>,
    holder: Option<Entry>,
    generation: u64,
    node: Option<NodeStart>,
    incarnations: u64,
}

struct Inner {
    journal: Option<PathBuf>,
    journal_anchor: Option<Mutex<model_publication::Anchor>>,
    state: Mutex<State>,
    epoch: String,
    changed: tokio::sync::Notify,
    ttl: Duration,
    hard: Duration,
    max: Duration,
}

struct AuthorityState<'a> {
    state: MutexGuard<'a, State>,
    journal: Option<model_publication::Guard>,
    epoch: &'a str,
    valid: bool,
    recovering: bool,
}
impl Deref for AuthorityState<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        &self.state
    }
}
impl DerefMut for AuthorityState<'_> {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.state
    }
}
impl Drop for AuthorityState<'_> {
    fn drop(&mut self) {
        let Some(journal) = self.journal.take() else {
            return;
        };
        if !self.valid {
            return;
        }
        let clear = self.recovering && !self.state.recovery_required;
        if self.state.pending_operations && self.state.holder.is_none() && !clear {
            self.state.recovery_required = true;
            if self.state.recovery_ticket.is_none() {
                self.state.recovery_ticket = Some(random_hex());
            }
        }
        let record = model_publication::AuthorityRecord {
            incarnation: self.epoch.to_owned(),
            lease: self
                .state
                .holder
                .as_ref()
                .map_or_else(String::new, |e| e.id.clone()),
            generation: self.state.generation,
            active: self.state.holder.as_ref().is_some_and(|e| !e.fenced),
            until_ms: self.state.holder.as_ref().map_or(0, |e| {
                model_publication::now_ms().saturating_add(
                    u64::try_from(
                        e.renewed_until
                            .min(e.hard_at)
                            .saturating_duration_since(Instant::now())
                            .as_millis(),
                    )
                    .unwrap_or(u64::MAX),
                )
            }),
            recovery: self.state.recovery_required,
        };
        let saved = (!clear || journal.clear_operations().is_ok())
            && journal.save_authority(&record).is_ok();
        if !saved || journal.commit().is_err() {
            self.state.journal_available = false;
            self.state.recovery_required = true;
            if let Some(e) = self.state.holder.as_mut() {
                e.fenced = true;
            }
        }
    }
}

/// The authority. Cheap to clone; every clone is the same table.
#[derive(Clone)]
pub struct Authority {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority")
            .field("held", &self.held())
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

/// `capFor`: the hard cap a lease gets for `requested` (`None`: the default).
pub fn cap_for(requested: Option<Duration>, hard: Duration, max: Duration) -> Duration {
    match requested {
        None => hard,
        Some(r) => r.max(hard).min(hard.max(max)),
    }
}

impl Authority {
    pub fn new() -> Self {
        Self::with_timing(TTL, HARD, MAX)
    }

    pub fn with_timing(ttl: Duration, hard: Duration, max: Duration) -> Self {
        Authority {
            inner: Arc::new(Inner {
                journal: None,
                journal_anchor: None,
                state: Mutex::new(State {
                    journal_available: true,
                    pending_operations: false,
                    recovery_required: false,
                    recovery_ticket: None,
                    holder: None,
                    generation: 0,
                    node: None,
                    incarnations: 0,
                }),
                epoch: random_hex(),
                changed: tokio::sync::Notify::new(),
                ttl,
                hard,
                max,
            }),
        }
    }

    /// This process start's identity: grants of another epoch are never honoured.
    pub fn epoch(&self) -> &str {
        &self.inner.epoch
    }

    pub fn ttl(&self) -> Duration {
        self.inner.ttl
    }

    /// `renewMs`: `min(RENEW, max(1 ms, ttl / 3))`.
    pub fn renew_every(&self) -> Duration {
        RENEW.min((self.inner.ttl / 3).max(Duration::from_millis(1)))
    }

    fn lock(&self) -> AuthorityState<'_> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let journal = self
            .inner
            .journal
            .as_ref()
            .and_then(|path| model_publication::Guard::at(path).ok());
        let valid = match (&self.inner.journal, &journal) {
            (None, _) => true,
            (Some(_), Some(j)) => j
                .authority()
                .ok()
                .flatten()
                .is_some_and(|r| r.incarnation == self.inner.epoch),
            _ => false,
        };
        state.journal_available = valid;
        if !valid {
            state.recovery_required = true;
            if let Some(e) = state.holder.as_mut() {
                e.fenced = true;
            }
        }
        state.pending_operations = journal
            .as_ref()
            .is_some_and(|j| j.pending().unwrap_or(true));
        let recovering = state.recovery_required;
        AuthorityState {
            state,
            journal,
            valid,
            epoch: &self.inner.epoch,
            recovering,
        }
    }

    /// Bind this authority to the shared publication journal. A prior start's unfinished work
    /// becomes a recovery incident, never a grant inherited by this incarnation.
    pub fn with_publication(mut self, path: PathBuf) -> Result<Self, String> {
        let journal =
            model_publication::Guard::at(&path).map_err(|_| "preset authority unavailable")?;
        let old = journal
            .authority()
            .map_err(|_| "preset authority unavailable")?;
        let recovery = old.is_some_and(|r| r.active || r.recovery)
            || journal
                .pending()
                .map_err(|_| "preset authority unavailable")?;
        journal
            .save_authority(&model_publication::AuthorityRecord {
                incarnation: self.inner.epoch.clone(),
                recovery,
                ..Default::default()
            })
            .map_err(|_| "preset authority unavailable")?;
        journal
            .commit()
            .map_err(|_| "preset authority unavailable")?;
        let Some(inner) = Arc::get_mut(&mut self.inner) else {
            return Err("preset authority already shared".into());
        };
        inner.journal_anchor = Some(Mutex::new(
            model_publication::Anchor::open(&path).map_err(|_| "preset authority unavailable")?,
        ));
        inner.journal = Some(path);
        let state = inner
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.recovery_required = recovery;
        if recovery {
            state.recovery_ticket = Some(random_hex());
        }
        Ok(self)
    }

    pub(crate) fn begin_operation(
        &self,
        grant: &Grant,
        kind: &str,
        expected_revision: Option<&str>,
    ) -> Option<model_publication::Fence> {
        let state = self.lock();
        if !state.journal_available
            || !Self::matches(&state, &grant.id, Some(grant.generation))
            || state.holder.as_ref().is_none_or(|e| {
                e.fenced || Instant::now() >= e.renewed_until || Instant::now() >= e.hard_at
            })
        {
            return None;
        }
        let fence = model_publication::Fence {
            incarnation: grant.epoch.clone(),
            lease: grant.id.clone(),
            generation: grant.generation,
            operation: random_hex(),
            kind: kind.to_owned(),
            expected_revision: expected_revision.map(str::to_owned),
        };
        if let Some(journal) = &state.journal {
            journal.begin(&fence).ok()?;
        }
        Some(fence)
    }

    pub(crate) fn unknown_operation(&self, fence: &model_publication::Fence) {
        let mut state = self.lock();
        if !state.valid
            || !state
                .journal
                .as_ref()
                .is_some_and(|j| j.operation_exists(fence).unwrap_or(true))
        {
            return;
        }
        if let Some(journal) = &state.journal {
            let _ = journal.finish(fence, "recovery", None);
        }
        state.recovery_required = true;
        state
            .recovery_ticket
            .get_or_insert_with(|| fence.lease.clone());
        if let Some(e) = state.holder.as_mut() {
            if e.id == fence.lease && e.generation == fence.generation {
                e.fenced = true;
            }
        }
    }
    pub(crate) fn fence_grant(&self, grant: &Grant) {
        let mut state = self.lock();
        if !state.valid || !Self::matches(&state, &grant.id, Some(grant.generation)) {
            return;
        }
        state.recovery_required = true;
        state
            .recovery_ticket
            .get_or_insert_with(|| grant.id.clone());
        if let Some(e) = state.holder.as_mut() {
            e.fenced = true;
        }
    }

    pub(crate) fn settle_operation(&self, fence: &model_publication::Fence) -> bool {
        let state = self.lock();
        state.valid
            && state
                .journal
                .as_ref()
                .is_some_and(|j| j.settle(fence).is_ok())
    }

    /// Transfer an uncertain effect to the recovery incident while its original hold remains
    /// continuously covered. That hold may still perform its conditional rollback; no new holder
    /// is admitted, and the uncertain receipt remains pending until fresh covered reconciliation.
    pub(crate) fn defer_operation_to_recovery(&self, fence: &model_publication::Fence) -> bool {
        let mut state = self.lock();
        if !state.valid
            || !Self::matches(&state, &fence.lease, Some(fence.generation))
            || state.holder.as_ref().is_none_or(|e| {
                e.fenced || Instant::now() >= e.renewed_until || Instant::now() >= e.hard_at
            })
            || state.journal.as_ref().is_none_or(|j| {
                j.status(fence).ok().flatten().is_none()
                    || j.finish(fence, "recovery", None).is_err()
            })
        {
            return false;
        }
        state.recovery_required = true;
        state.recovery_ticket.get_or_insert_with(random_hex);
        true
    }

    pub(crate) fn pending_operation_kinds(&self) -> std::io::Result<Vec<String>> {
        let state = self.lock();
        match &state.journal {
            Some(j) => j.pending_kinds(),
            None => Ok(Vec::new()),
        }
    }
    #[allow(dead_code)]
    pub(crate) fn operation_status(
        &self,
        fence: &model_publication::Fence,
    ) -> std::io::Result<Option<model_publication::Receipt>> {
        let path = self
            .inner
            .journal
            .as_ref()
            .ok_or_else(|| std::io::Error::other("Maintenance authority unavailable."))?;
        model_publication::read_receipt(path, fence)
    }

    /// Wait for the original destination receipt; never repeat the effect or acquire a new permit.
    /// None is unknown/recovery-required, including reconciliation that cannot prove original success.
    #[allow(dead_code)]
    pub(crate) async fn wait_for_settlement(
        &self,
        fence: &model_publication::Fence,
        limit: Duration,
    ) -> Option<model_publication::Receipt> {
        let deadline = Instant::now() + limit;
        loop {
            let (authority, proof) = (self.clone(), fence.clone());
            let receipt =
                tokio::task::spawn_blocking(move || authority.operation_status(&proof)).await;
            match receipt {
                Ok(Ok(Some(receipt)))
                    if matches!(receipt.state.as_str(), "settled" | "conflict") =>
                {
                    return Some(receipt)
                }
                Ok(Ok(Some(receipt))) if matches!(receipt.state.as_str(), "issued" | "running") => {
                }
                _ => {
                    self.unknown_operation(fence);
                    return None;
                }
            }
            if Instant::now() >= deadline {
                self.unknown_operation(fence);
                return None;
            }
            tokio::time::sleep(
                Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
        }
    }

    /// Ends the current lease if it has expired. True when it did.
    fn expire(state: &mut State, now: Instant) -> bool {
        let gone = state
            .holder
            .as_ref()
            .is_some_and(|e| now >= e.renewed_until || now >= e.hard_at);
        if gone {
            if let Some(e) = state.holder.take() {
                if e.origin == Origin::Front {
                    state.recovery_required = true;
                    state.recovery_ticket.get_or_insert_with(|| e.id.clone());
                }
                let why = if now >= e.hard_at {
                    "passed its limit"
                } else {
                    "was not renewed"
                };
                eprintln!(
                    "[models] a maintenance lease ({}) {why}; chat and tuning may run again",
                    e.reason
                );
            }
        }
        gone
    }

    /// Registers a Node process start (`boot`, started at `started` ms since the epoch on the shared
    /// host clock). The same start gets its incarnation again. A later start is issued the next
    /// incarnation: the leases of earlier Node incarnations end, and a front lease backed by an earlier
    /// incarnation's chat latch is fenced (it keeps excluding until released or expired, so the new Node
    /// keeps chat closed, but no further step may start on it). An earlier or ambiguous start (older, or
    /// the same start time with another boot) is retired and changes nothing.
    pub fn node_session(&self, boot: &str, started: u64) -> Result<Session, Refusal> {
        let now = Instant::now();
        let mut state = self.lock();
        Self::expire(&mut state, now);
        let issued = state
            .node
            .as_ref()
            .is_none_or(|n| n.boot != boot && started > n.started);
        let incarnation = match state.node.as_ref() {
            Some(n) if n.boot == boot => n.incarnation,
            Some(n) if started <= n.started => return Err(Refusal::Retired),
            _ => {
                state.incarnations += 1;
                let incarnation = state.incarnations;
                state.node = Some(NodeStart {
                    incarnation,
                    started,
                    boot: boot.to_owned(),
                });
                let mut freed = false;
                if let Some(e) = state.holder.as_mut() {
                    match e.origin {
                        Origin::Node { incarnation: i } if i != incarnation => freed = true,
                        Origin::Front if e.bound_node != Some(incarnation) && !e.fenced => {
                            e.fenced = true;

                            eprintln!("[models] Node restarted while this front held the maintenance lease; its work stops before its next step");
                        }
                        _ => {}
                    }
                }
                if state.holder.as_ref().is_some_and(|e| e.fenced) {
                    state.recovery_required = true;
                    if state.recovery_ticket.is_none() {
                        state.recovery_ticket = state.holder.as_ref().map(|e| e.id.clone());
                    }
                }
                if freed {
                    state.holder = None;
                    eprintln!("[models] Node restarted; the maintenance lease its previous start held has ended");
                    let front_held = state.recovery_required;
                    drop(state);
                    self.inner.changed.notify_waiters();
                    return Ok(Session {
                        incarnation,
                        front_held,
                    });
                }
                incarnation
            }
        };
        let front_held = state.recovery_required
            || state
                .holder
                .as_ref()
                .is_some_and(|e| e.origin == Origin::Front);
        drop(state);
        if issued {
            // Waiting acquires of retired incarnations end now.
            self.inner.changed.notify_waiters();
        }
        Ok(Session {
            incarnation,
            front_held,
        })
    }

    /// Whether `incarnation` is the active Node start.
    pub fn node_active(&self, incarnation: u64) -> bool {
        self.lock()
            .node
            .as_ref()
            .is_some_and(|n| n.incarnation == incarnation)
    }

    /// Takes the lease now, or refuses with [`Refusal::Busy`]. Check and set in one step.
    pub fn try_acquire(
        &self,
        origin: Origin,
        requested: Option<Duration>,
        reason: &str,
    ) -> Result<Grant, Refusal> {
        self.take(origin, requested, reason, false)
    }

    pub(crate) fn try_recovery(
        &self,
        requested: Option<Duration>,
        reason: &str,
    ) -> Result<Grant, Refusal> {
        self.take(Origin::Front, requested, reason, true)
    }

    pub(crate) fn recovery_pending(&self) -> bool {
        let state = self.lock();
        state.recovery_required || state.pending_operations
    }
    pub(crate) fn recovery_ticket(&self) -> Option<String> {
        self.lock().recovery_ticket.clone()
    }
    pub(crate) fn require_recovery(&self) -> String {
        let mut state = self.lock();
        state.recovery_required = true;
        if state.recovery_ticket.is_none() {
            state.recovery_ticket = Some(
                state
                    .holder
                    .as_ref()
                    .map_or_else(random_hex, |e| e.id.clone()),
            );
        }
        state.recovery_ticket.clone().unwrap_or_default()
    }
    pub(crate) fn complete_recovery(&self, ticket: &str) -> bool {
        let mut state = self.lock();
        if state.recovery_ticket.as_deref() != Some(ticket) {
            return false;
        }
        state.recovery_required = false;
        state.recovery_ticket = None;
        drop(state);
        self.inner.changed.notify_waiters();
        true
    }

    fn take(
        &self,
        origin: Origin,
        requested: Option<Duration>,
        reason: &str,
        recovery: bool,
    ) -> Result<Grant, Refusal> {
        let now = Instant::now();
        let cap = cap_for(requested, self.inner.hard, self.inner.max);
        let mut state = self.lock();
        // An acquire never changes the active Node start; a retired incarnation gets nothing.
        if let Origin::Node { incarnation } = origin {
            if state
                .node
                .as_ref()
                .is_none_or(|n| n.incarnation != incarnation)
            {
                return Err(Refusal::Retired);
            }
        }
        Self::expire(&mut state, now);
        if !state.journal_available
            || state.holder.is_some()
            || (state.recovery_required && !recovery)
        {
            return Err(Refusal::Busy);
        }
        let bound_node = state.node.as_ref().map(|n| n.incarnation);
        state.generation += 1;
        let entry = Entry {
            id: random_hex(),
            generation: state.generation,
            origin: origin.clone(),
            renewed_until: now + self.inner.ttl,
            hard_at: now + cap,
            reason: reason.chars().take(300).collect(),
            bound_node: (origin == Origin::Front).then_some(bound_node).flatten(),
            fenced: false,
            recovery_ticket: recovery.then(|| state.recovery_ticket.clone()).flatten(),
        };
        let grant = Grant {
            id: entry.id.clone(),
            generation: entry.generation,
            epoch: self.inner.epoch.clone(),
            ttl: self.inner.ttl,
            renew_every: self.renew_every(),
            hard: cap,
        };
        state.holder = Some(entry);
        Ok(grant)
    }

    /// Like [`Self::try_acquire`], waiting up to `wait` for the current lease to end. Cancel-safe:
    /// a dropped future has taken nothing (the grant is made in the same step that returns it).
    pub async fn acquire(
        &self,
        origin: Origin,
        requested: Option<Duration>,
        reason: &str,
        wait: Duration,
    ) -> Result<Grant, Refusal> {
        let deadline = Instant::now() + wait.min(MAX_WAIT);
        loop {
            let notified = self.inner.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.try_acquire(origin.clone(), requested, reason) {
                Ok(g) => return Ok(g),
                Err(Refusal::Retired) => return Err(Refusal::Retired),
                Err(r) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(r);
                    }
                    // Wake at the release, the current lease's own end, or the deadline.
                    let until = self.ends_at().map_or(deadline, |e| e.min(deadline));
                    tokio::select! {
                        () = notified => {}
                        () = tokio::time::sleep_until(until) => {}
                    }
                }
            }
        }
    }

    /// When the current lease ends unless renewed (`None`: none is held).
    fn ends_at(&self) -> Option<Instant> {
        self.lock()
            .holder
            .as_ref()
            .map(|e| e.renewed_until.min(e.hard_at))
    }

    fn matches(state: &State, id: &str, generation: Option<u64>) -> bool {
        state
            .holder
            .as_ref()
            .is_some_and(|e| e.id == id && generation.is_none_or(|g| g == e.generation))
    }

    /// Keeps the lease `id` from expiring. False when it has ended (or passed its hard cap).
    pub fn renew(&self, id: &str, generation: Option<u64>) -> bool {
        let now = Instant::now();
        let mut state = self.lock();
        let ended = Self::expire(&mut state, now);
        let ok = Self::matches(&state, id, generation);
        if ok {
            if let Some(e) = state.holder.as_mut() {
                e.renewed_until = now + self.inner.ttl;
            }
        }
        drop(state);
        if ended {
            self.inner.changed.notify_waiters();
        }
        ok
    }

    fn owned_by(state: &State, origin: &Origin, id: &str, generation: Option<u64>) -> bool {
        Self::matches(state, id, generation)
            && state.holder.as_ref().is_some_and(|e| &e.origin == origin)
    }

    /// [`Self::renew`], only for a lease `origin` holds (a Node incarnation renews only its own).
    pub fn renew_as(&self, origin: &Origin, id: &str, generation: Option<u64>) -> bool {
        if !Self::owned_by(&self.lock(), origin, id, generation) {
            return false;
        }
        self.renew(id, generation)
    }

    /// [`Self::release`], only for a lease `origin` holds.
    pub fn release_as(&self, origin: &Origin, id: &str, generation: Option<u64>) -> bool {
        if !Self::owned_by(&self.lock(), origin, id, generation) {
            return false;
        }
        self.release(id, generation)
    }

    /// Updates the reason the lease shows (`setReason`); false when it has ended.
    pub fn set_reason(&self, id: &str, reason: &str) -> bool {
        let mut state = self.lock();
        if !Self::matches(&state, id, None) {
            return false;
        }
        if let Some(e) = state.holder.as_mut() {
            e.reason = reason.chars().take(300).collect();
        }
        true
    }

    /// Ends the lease `id`; idempotent. True when it was the current lease.
    pub fn release(&self, id: &str, generation: Option<u64>) -> bool {
        let mut state = self.lock();
        let ours = Self::matches(&state, id, generation);
        if ours {
            state.holder = None;
        }
        drop(state);
        if ours {
            self.inner.changed.notify_waiters();
        }
        ours
    }

    /// The fence: whether (`id`, `generation`) is the current, unexpired lease and stays so for `step`.
    pub fn covers(&self, id: &str, generation: u64, step: Duration) -> bool {
        let now = Instant::now();
        let state = self.lock();
        state.journal_available
            && state.holder.as_ref().is_some_and(|e| {
                e.id == id
                    && e.generation == generation
                    && !e.fenced
                    && now + step < e.renewed_until
                    && now + step < e.hard_at
            })
    }

    /// Whether any lease is held now.
    pub fn held(&self) -> bool {
        let now = Instant::now();
        let mut state = self.lock();
        let ended = Self::expire(&mut state, now);
        let held = state.holder.is_some();
        drop(state);
        if ended {
            self.inner.changed.notify_waiters();
        }
        held
    }

    /// Whether this front (not Node) holds it now.
    pub fn held_by_front(&self) -> bool {
        if self.lock().recovery_required {
            return true;
        }
        self.held()
            && self
                .lock()
                .holder
                .as_ref()
                .is_some_and(|e| e.origin == Origin::Front)
    }
}

// ---- The private store-listener routes Node's maintenance gate uses (core createNativeLeaseClient). ----

/// The bodies are tiny (`BODY_LIMIT` in Core).
pub const BODY_LIMIT: usize = 4096;
pub const ROUTE_PREFIX: &str = "/v1/maintenance-lease";
pub const OPS: &[&str] = &[
    "acquire",
    "renew",
    "release",
    "held",
    "session",
    "complete-recovery",
    "operation-begin",
    "operation-status",
    "operation-settle",
];

fn answer(status: u16, body: &serde_json::Value) -> axum::http::Response<axum::body::Body> {
    let mut res = axum::http::Response::new(axum::body::Body::from(body.to_string()));
    *res.status_mut() = axum::http::StatusCode::from_u16(status)
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    res.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("application/json"),
    );
    res.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    res
}

fn bad(message: &str) -> axum::http::Response<axum::body::Body> {
    answer(400, &serde_json::json!({ "error": message }))
}

fn is_hex32(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A whole number of milliseconds (`Number.isSafeInteger`), absent as `None`; `Err` otherwise.
fn millis(body: &serde_json::Value, key: &str) -> Result<Option<i64>, ()> {
    const SAFE: i64 = 9_007_199_254_740_991;
    match body.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .filter(|n| (-SAFE..=SAFE).contains(n))
            .or_else(|| {
                v.as_f64()
                    .filter(|f| f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_991.0)
                    .map(|f| f as i64)
            })
            .map(Some)
            .ok_or(()),
    }
}

fn retired() -> axum::http::Response<axum::body::Body> {
    answer(
        410,
        &serde_json::json!({
            "error": "This Node start was replaced by a newer one.",
            "code": "incarnation_retired",
        }),
    )
}

/// Answers one Node operation. `body` is the already-authorised request's JSON.
///
/// `session {boot, startedAt}` registers the process start and returns `{epoch, incarnation,
/// frontHeld}`; every other operation carries that `incarnation`, and one that is not the active
/// incarnation is a 410 (`incarnation_retired`) that changes nothing.
pub async fn answer_node(
    authority: &Authority,
    op: &str,
    body: &serde_json::Value,
) -> axum::http::Response<axum::body::Body> {
    use serde_json::json;
    let reason = body
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let id = || {
        body.get("lease")
            .and_then(serde_json::Value::as_str)
            .filter(|l| is_hex32(l))
    };
    if op == "session" {
        let Some(boot) = body
            .get("boot")
            .and_then(serde_json::Value::as_str)
            .filter(|b| !b.is_empty() && b.len() <= 64)
        else {
            return bad("boot required");
        };
        let Some(started) = millis(body, "startedAt")
            .ok()
            .flatten()
            .and_then(|m| u64::try_from(m).ok())
        else {
            return bad("startedAt required");
        };
        return match authority.node_session(boot, started) {
            Ok(s) => answer(
                200,
                &json!({ "epoch": authority.epoch(), "incarnation": s.incarnation, "frontHeld": s.front_held }),
            ),
            Err(_) => retired(),
        };
    }
    if op.starts_with("operation-") {
        let Some(value) = body.get("maintenance") else {
            return bad("maintenance required");
        };
        if op == "operation-begin" {
            let Some(lease) = value.get("lease").and_then(serde_json::Value::as_str) else {
                return bad("lease required");
            };
            let Some(generation) = value.get("generation").and_then(serde_json::Value::as_u64)
            else {
                return bad("generation required");
            };
            if value.get("incarnation").and_then(serde_json::Value::as_str)
                != Some(authority.epoch())
            {
                return answer(409, &json!({"error":"Stale maintenance operation."}));
            }
            let kind = body
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if kind.is_empty()
                || kind.len() > 64
                || !kind
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return bad("invalid operation kind");
            }
            let expected = body
                .get("expectedRevision")
                .and_then(serde_json::Value::as_str);
            if expected.is_some_and(|r| r.len() != 64 || !r.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return bad("invalid expected revision");
            }
            let grant = Grant {
                id: lease.to_owned(),
                generation,
                epoch: authority.epoch().to_owned(),
                ttl: authority.ttl(),
                renew_every: authority.renew_every(),
                hard: HARD,
            };
            return match authority.begin_operation(&grant, kind, expected) {
                Some(fence) => answer(201, &json!({"maintenance":fence,"state":"issued"})),
                None => answer(409, &json!({"error":"Stale maintenance operation."})),
            };
        }
        let Ok(fence) = serde_json::from_value::<model_publication::Fence>(value.clone()) else {
            return bad("invalid maintenance operation");
        };
        let state = authority.lock();
        let Some(journal) = state.journal.as_ref().filter(|_| state.valid) else {
            return answer(503, &json!({"error":"Maintenance authority unavailable."}));
        };
        if op == "operation-settle" {
            let terminal = body
                .get("state")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("recovery");
            if !matches!(terminal, "settled" | "conflict" | "recovery") {
                return bad("invalid terminal state");
            }
            let revision = body.get("revision").and_then(serde_json::Value::as_str);
            if journal.finish(&fence, terminal, revision).is_err() {
                return answer(503, &json!({"error":"Maintenance authority unavailable."}));
            }
        }
        return match journal.status(&fence) {
            Ok(Some(receipt)) => answer(
                200,
                &json!({"state":receipt.state,"revision":receipt.revision}),
            ),
            Ok(None) => answer(404, &json!({"state":"unknown","recoveryRequired":true})),
            Err(_) => answer(503, &json!({"error":"Maintenance authority unavailable."})),
        };
    }
    let Some(incarnation) = body.get("incarnation").and_then(serde_json::Value::as_u64) else {
        return bad("incarnation required");
    };
    if !authority.node_active(incarnation) {
        return retired();
    }
    let origin = Origin::Node { incarnation };
    match op {
        "acquire" => {
            let (Ok(max), Ok(wait)) = (millis(body, "maxMs"), millis(body, "waitMs")) else {
                return bad("maxMs must be a whole number of milliseconds.");
            };
            let max = max.map(|m| Duration::from_millis(u64::try_from(m).unwrap_or(0)));
            let wait = Duration::from_millis(wait.and_then(|w| u64::try_from(w).ok()).unwrap_or(0));
            let grant = if body.get("recovery").and_then(serde_json::Value::as_bool) == Some(true) {
                authority.take(origin, max, reason, true)
            } else {
                authority.acquire(origin, max, reason, wait).await
            };
            match grant {
                Ok(g) => answer(
                    201,
                    &json!({
                        "lease": g.id,
                        "generation": g.generation,
                        "epoch": g.epoch,
                        "incident": authority.recovery_ticket(),
                        "ttlMs": u64::try_from(g.ttl.as_millis()).unwrap_or(u64::MAX),
                        "renewMs": u64::try_from(g.renew_every.as_millis()).unwrap_or(u64::MAX),
                        "hardMs": u64::try_from(g.hard.as_millis()).unwrap_or(u64::MAX),
                    }),
                ),
                Err(Refusal::Busy) => answer(
                    409,
                    &json!({
                        "error": "Inference is busy. Wait for requests to finish before changing native presets.",
                        "code": "maintenance_held",
                        "chatActive": false,
                    }),
                ),
                Err(Refusal::Retired) => retired(),
            }
        }
        "complete-recovery" => {
            let Some(id) = id() else {
                return bad("lease required");
            };
            let Some(generation) = body.get("generation").and_then(serde_json::Value::as_u64)
            else {
                return bad("generation required");
            };
            let incident = body.get("incident").and_then(serde_json::Value::as_str);
            let mut state = authority.lock();
            let covered = Authority::owned_by(&state, &origin, id, Some(generation))
                && state.holder.as_ref().is_some_and(|e| {
                    !e.fenced
                        && e.recovery_ticket.as_deref() == incident
                        && Instant::now() < e.renewed_until
                        && Instant::now() < e.hard_at
                });
            if !covered || state.recovery_ticket.as_deref() != incident {
                return answer(409, &json!({"error":"Stale maintenance operation."}));
            }
            state.recovery_required = false;
            state.recovery_ticket = None;
            answer(200, &json!({}))
        }
        "renew" => {
            let Some(id) = id() else {
                return answer(404, &json!({ "error": "Not found" }));
            };
            let generation = body.get("generation").and_then(serde_json::Value::as_u64);
            if !authority.renew_as(&origin, id, generation) {
                return answer(404, &json!({ "error": "Not found" }));
            }
            if !reason.is_empty() {
                authority.set_reason(id, reason);
            }
            answer(200, &json!({}))
        }
        "release" => {
            if let Some(id) = id() {
                let generation = body.get("generation").and_then(serde_json::Value::as_u64);
                authority.release_as(&origin, id, generation);
            }
            let mut res = axum::http::Response::new(axum::body::Body::empty());
            *res.status_mut() = axum::http::StatusCode::NO_CONTENT;
            res
        }
        "held" => answer(
            200,
            &json!({ "held": authority.held(), "front": authority.held_by_front() }),
        ),
        _ => answer(404, &json!({ "error": "Not found" })),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Registers Node start `boot` at `started` and returns its origin.
    fn node(a: &Authority, boot: &str, started: u64) -> Origin {
        Origin::Node {
            incarnation: a.node_session(boot, started).unwrap().incarnation,
        }
    }

    #[tokio::test]
    async fn publication_receipt_waits_for_the_same_operation_after_a_caller_timeout() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("authority.sqlite");
        let a = Authority::new().with_publication(path.clone()).unwrap();
        let g = a.try_acquire(Origin::Front, None, "publish").unwrap();
        let proof = a.begin_operation(&g, "reload", None).unwrap();
        let (file, p) = (path.clone(), proof.clone());
        let destination = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let guard = model_publication::Guard::at(&file).unwrap();
            guard.finish(&p, "settled", None).unwrap();
            guard.commit().unwrap();
        });
        let receipt = a
            .wait_for_settlement(&proof, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(receipt.state, "settled");
        destination.await.unwrap();
        assert_eq!(
            a.operation_status(&proof).unwrap().unwrap().state,
            "settled"
        );
        assert!(a.covers(&g.id, g.generation, Duration::ZERO));
    }
    #[test]
    fn an_uncertain_reload_keeps_only_its_original_hold_for_conditional_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let a = Authority::new()
            .with_publication(dir.path().join("authority.sqlite"))
            .unwrap();
        let grant = a.try_acquire(Origin::Front, None, "preset").unwrap();
        let reload = a.begin_operation(&grant, "reload", None).unwrap();
        assert!(a.defer_operation_to_recovery(&reload));
        assert_eq!(
            a.operation_status(&reload).unwrap().unwrap().state,
            "recovery"
        );
        assert!(a.recovery_pending());
        assert!(a.covers(&grant.id, grant.generation, Duration::ZERO));
        let rollback = a.begin_operation(&grant, "restore", None).unwrap();
        assert!(a.settle_operation(&rollback));
        assert!(a.release(&grant.id, Some(grant.generation)));
        assert!(a.try_acquire(Origin::Front, None, "competitor").is_err());
        assert!(
            !a.defer_operation_to_recovery(&reload),
            "never adopt a replacement hold"
        );
        assert_eq!(
            a.operation_status(&reload).unwrap().unwrap().state,
            "recovery"
        );
    }
    #[tokio::test]
    async fn publication_unknown_timeout_fences_its_holder_and_survives_a_fresh_authority() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("authority.sqlite");
        let a = Authority::new().with_publication(path.clone()).unwrap();
        let g = a.try_acquire(Origin::Front, None, "publish").unwrap();
        let proof = a.begin_operation(&g, "reload", None).unwrap();
        assert!(a
            .wait_for_settlement(&proof, Duration::from_millis(5))
            .await
            .is_none());
        assert!(!a.covers(&g.id, g.generation, Duration::ZERO));
        assert!(a.recovery_pending());
        let restarted = Authority::new().with_publication(path).unwrap();
        assert!(restarted.node_session("node", 1).unwrap().front_held);
        assert_eq!(
            restarted.try_acquire(Origin::Front, None, "new work"),
            Err(Refusal::Busy)
        );
    }

    #[test]
    fn r5_late_completion_cannot_clear_a_newer_authority_incident() {
        let a = Authority::new();
        let first = a.require_recovery();
        assert!(a.complete_recovery(&first));
        let second = a.require_recovery();
        assert_ne!(first, second);
        assert!(!a.complete_recovery(&first));
        assert_eq!(a.recovery_ticket().as_deref(), Some(second.as_str()));
        assert!(a.node_session("node", 1).unwrap().front_held);
        assert!(a.complete_recovery(&second));
    }

    #[tokio::test(start_paused = true)]
    async fn r4_node_restart_retains_barrier_between_lost_release_and_fresh_recovery() {
        let a = Authority::new();
        a.node_session("a", 1).unwrap();
        let old = a.try_acquire(Origin::Front, None, "trial").unwrap();
        a.node_session("b", 2).unwrap();
        a.release(&old.id, Some(old.generation));
        assert!(
            a.held_by_front(),
            "recovery-required survives old latch release"
        );
        assert!(
            a.node_session("c", 3).unwrap().front_held,
            "restarted Node must reconstruct admission barrier"
        );
        assert_eq!(
            a.try_acquire(
                Origin::Node { incarnation: 3 },
                None,
                "chat-adjacent writer"
            ),
            Err(Refusal::Busy)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn exclusive_hold_and_busy() {
        let a = Authority::new();
        let n = node(&a, "n1", 1);
        let g = a.try_acquire(Origin::Front, None, "reload").unwrap();
        assert_eq!(g.id.len(), 32);
        assert_eq!(g.ttl, TTL);
        assert_eq!(g.renew_every, RENEW);
        assert_eq!(g.hard, HARD);
        assert_eq!(a.try_acquire(n.clone(), None, "tune"), Err(Refusal::Busy));
        assert_eq!(a.try_acquire(Origin::Front, None, "x"), Err(Refusal::Busy));
        assert!(a.release(&g.id, Some(g.generation)));
        let g2 = a.try_acquire(n, None, "tune").unwrap();
        assert!(g2.generation > g.generation);
        assert_ne!(g2.id, g.id);
    }

    #[tokio::test(start_paused = true)]
    async fn caps_are_clamped_like_core() {
        assert_eq!(cap_for(None, HARD, MAX), HARD);
        assert_eq!(cap_for(Some(Duration::from_secs(1)), HARD, MAX), HARD);
        assert_eq!(
            cap_for(Some(Duration::from_secs(8 * 3600)), HARD, MAX),
            Duration::from_secs(8 * 3600)
        );
        assert_eq!(
            cap_for(Some(Duration::from_secs(48 * 3600)), HARD, MAX),
            MAX
        );
        let a = Authority::new();
        let g = a
            .try_acquire(
                Origin::Front,
                Some(Duration::from_secs(9 * 3600)),
                "calibration",
            )
            .unwrap();
        assert_eq!(g.hard, Duration::from_secs(9 * 3600));
    }

    #[tokio::test(start_paused = true)]
    async fn renewal_keeps_it_and_missing_renewal_ends_it() {
        let a = Authority::new();
        let n = node(&a, "n", 1);
        let g = a.try_acquire(Origin::Front, None, "r").unwrap();
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(170)).await;
            assert!(a.renew(&g.id, Some(g.generation)));
        }
        tokio::time::advance(TTL).await;
        assert!(!a.held());
        assert!(
            !a.renew(&g.id, Some(g.generation)),
            "an expired lease is not renewed"
        );
        a.complete_recovery(&a.recovery_ticket().unwrap()); // synthetic settlement of the expired front work
        assert!(a.try_acquire(n, None, "t").is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn hard_expiry_ends_it_however_often_renewed() {
        let a = Authority::new();
        let g = a.try_acquire(Origin::Front, None, "r").unwrap();
        let mut renewed = 0;
        while a.renew(&g.id, Some(g.generation)) {
            renewed += 1;
            tokio::time::advance(RENEW).await;
            assert!(renewed < 1000);
        }
        assert_eq!(renewed, 360, "60 min of 10 s renewals");
        assert!(!a.held());
    }

    #[tokio::test(start_paused = true)]
    async fn lost_coverage_is_fenced() {
        let a = Authority::new();
        let g = a.try_acquire(Origin::Front, None, "r").unwrap();
        assert!(a.covers(&g.id, g.generation, Duration::from_secs(120)));
        tokio::time::advance(Duration::from_secs(70)).await;
        assert!(!a.covers(&g.id, g.generation, Duration::from_secs(120)));
        tokio::time::advance(TTL).await;
        assert!(!a.covers(&g.id, g.generation, Duration::ZERO));
        assert!(!a.held()); // observe expiry before synthetic recovery completion
        a.complete_recovery(&a.recovery_ticket().unwrap());
        let g2 = a.try_acquire(Origin::Front, None, "r").unwrap();
        assert!(!a.covers(&g.id, g.generation, Duration::ZERO));
        assert!(!a.renew(&g.id, Some(g.generation)));
        assert!(
            !a.release(&g.id, Some(g.generation)),
            "a stale release frees nothing"
        );
        assert!(a.covers(&g2.id, g2.generation, Duration::ZERO));
        assert!(
            !a.covers(&g2.id, g.generation, Duration::ZERO),
            "wrong generation"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn release_is_idempotent() {
        let a = Authority::new();
        let g = a.try_acquire(Origin::Front, None, "r").unwrap();
        assert!(a.release(&g.id, None));
        assert!(!a.release(&g.id, None));
        assert!(!a.held());
    }

    #[tokio::test(start_paused = true)]
    async fn restart_grants_no_stale_lease() {
        let before = Authority::new();
        let n = node(&before, "boot-a", 1);
        let g = before.try_acquire(n, None, "t").unwrap();
        let after = Authority::new();
        assert_ne!(before.epoch(), after.epoch());
        assert!(!after.held());
        assert!(!after.renew(&g.id, Some(g.generation)));
        assert!(!after.covers(&g.id, g.generation, Duration::ZERO));
        assert!(after.try_acquire(Origin::Front, None, "r").is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_node_restart_drops_its_previous_starts_lease() {
        let a = Authority::new();
        let old = node(&a, "boot-a", 10);
        let g = a.try_acquire(old.clone(), None, "t").unwrap();
        assert_eq!(
            a.node_session("boot-a", 10).unwrap().incarnation,
            1,
            "same start, same incarnation"
        );
        assert!(a.held());
        let new = node(&a, "boot-b", 20);
        assert_eq!(new, Origin::Node { incarnation: 2 });
        assert!(!a.held());
        assert!(!a.renew_as(&old, &g.id, None));
        assert_eq!(a.try_acquire(old, None, "t"), Err(Refusal::Retired));
    }

    // Review P2: an old start's delayed session or waiting acquire never supersedes a newer start.
    #[tokio::test(start_paused = true)]
    async fn an_old_starts_session_and_waiting_acquire_cannot_revoke_a_newer_starts_lease() {
        let a = Authority::new();
        let old = node(&a, "boot-a", 10);
        let front = a.try_acquire(Origin::Front, None, "reload").unwrap();
        // A waits behind the front lease.
        let waiting = {
            let (a, old) = (a.clone(), old.clone());
            tokio::spawn(async move { a.acquire(old, None, "t", Duration::from_secs(30)).await })
        };
        tokio::time::advance(Duration::from_millis(10)).await;
        // B starts, and gets the lease once the front releases.
        let new = node(&a, "boot-b", 20);
        a.release(&front.id, Some(front.generation));
        a.complete_recovery(&a.recovery_ticket().unwrap());
        let b = a.try_acquire(new.clone(), None, "delete").unwrap();
        assert_eq!(
            waiting.await.unwrap(),
            Err(Refusal::Retired),
            "A's wait ends retired"
        );
        // A's delayed session (older start, or same start time with another boot) is retired.
        assert_eq!(a.node_session("boot-a", 10), Err(Refusal::Retired));
        assert_eq!(a.node_session("boot-z", 20), Err(Refusal::Retired));
        assert!(
            a.covers(&b.id, b.generation, Duration::ZERO),
            "B's lease is untouched"
        );
        assert!(a.node_active(2) && !a.node_active(1));
        let _ = new;
    }

    // Review P1: a Node restart while the front holds fences the front lease but keeps it excluding.
    #[tokio::test(start_paused = true)]
    async fn a_node_restart_fences_a_front_lease_but_keeps_exclusion_until_release() {
        let a = Authority::new();
        let _ = node(&a, "boot-a", 10);
        let f = a.try_acquire(Origin::Front, None, "reload").unwrap();
        assert!(a.covers(&f.id, f.generation, Duration::ZERO));
        let s = a.node_session("boot-b", 20).unwrap();
        assert!(s.front_held, "the new Node is told to keep chat closed");
        assert!(
            !a.covers(&f.id, f.generation, Duration::ZERO),
            "no further front step"
        );
        assert!(
            a.held_by_front(),
            "still excluding while its in-flight step settles"
        );
        assert!(
            a.renew(&f.id, Some(f.generation)),
            "a live holder keeps it excluding"
        );
        let new = Origin::Node {
            incarnation: s.incarnation,
        };
        assert_eq!(a.try_acquire(new.clone(), None, "t"), Err(Refusal::Busy));
        a.release(&f.id, Some(f.generation));
        assert!(a.node_session("boot-b", 20).unwrap().front_held);
        a.complete_recovery(&a.recovery_ticket().unwrap());
        assert!(!a.node_session("boot-b", 20).unwrap().front_held);
        assert!(a.try_acquire(new, None, "t").is_ok());
        // A front lease taken under the current start is not fenced by the same start again.
        let a = Authority::new();
        let _ = node(&a, "boot-c", 5);
        let f = a.try_acquire(Origin::Front, None, "reload").unwrap();
        let _ = a.node_session("boot-c", 5).unwrap();
        assert!(a.covers(&f.id, f.generation, Duration::ZERO));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_acquires_grant_exactly_one() {
        let a = Authority::new();
        let n = node(&a, "n", 1);
        let start = Arc::new(tokio::sync::Barrier::new(64));
        let mut tasks = Vec::new();
        for i in 0..64 {
            let (a, start, n) = (a.clone(), Arc::clone(&start), n.clone());
            tasks.push(tokio::spawn(async move {
                start.wait().await;
                let origin = if i % 2 == 0 { Origin::Front } else { n };
                a.try_acquire(origin, None, "race").is_ok()
            }));
        }
        let mut won = 0;
        for t in tasks {
            won += usize::from(t.await.unwrap());
        }
        assert_eq!(won, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiter_gets_it_on_release_and_cancellation_takes_nothing() {
        let a = Authority::new();
        let n = node(&a, "n", 1);
        let g = a.try_acquire(Origin::Front, None, "r").unwrap();
        let waiting = {
            let (a, n) = (a.clone(), n.clone());
            tokio::spawn(async move { a.acquire(n, None, "t", Duration::from_secs(30)).await })
        };
        tokio::time::advance(Duration::from_secs(1)).await;
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert!(a.covers(&g.id, g.generation, Duration::ZERO));
        let waiter = {
            let (a, n) = (a.clone(), n.clone());
            tokio::spawn(async move { a.acquire(n, None, "t", Duration::from_secs(30)).await })
        };
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!waiter.is_finished());
        a.release(&g.id, Some(g.generation));
        let got = waiter.await.unwrap().unwrap();
        assert!(got.generation > g.generation);
        let busy = a
            .acquire(Origin::Front, None, "r", Duration::from_secs(5))
            .await;
        assert_eq!(busy, Err(Refusal::Busy));
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiter_is_woken_by_expiry() {
        let a = Authority::with_timing(Duration::from_secs(3), HARD, MAX);
        let n = node(&a, "n", 1);
        let _g = a.try_acquire(Origin::Front, None, "r").unwrap();
        let got = a.acquire(n, None, "t", Duration::from_secs(10)).await;
        assert_eq!(got, Err(Refusal::Busy));
        assert!(
            a.held_by_front(),
            "expiry requires settlement before new writers"
        );
    }
}
