//! The admission state machine. See the crate docs for the rules; each public method documents
//! the part it implements. Every method takes `now`, a monotonic millisecond clock of the caller.

use crate::config::Config;
use crate::listing::Listing;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// A request or lease request waiting for the chat slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WaiterKey(pub u64);

/// A granted lease: the chat slot is held for one model across several requests (a chat turn
/// with tool rounds) until it is released or runs out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseKey(pub u64);

/// Why the gate answers a request itself instead of forwarding it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Refusal {
    /// Too many requests are already waiting.
    QueueFull,
    /// The chat slot did not become free within the wait limit.
    QueueTimeout,
    /// The engine router does not answer.
    EngineUnavailable,
    /// An unload did not finish in time; no other model is let in until it does.
    SwitchStuck,
    /// The lease is not (or no longer) known.
    LeaseUnknown,
    /// The lease holds another model.
    LeaseModelMismatch,
    /// An unload of a model that is in use, without an admin force.
    ModelInUse,
    /// A model switch or unload is in progress, or work is in flight (maintenance).
    EngineBusy,
    InvalidPath,
    InvalidJson,
    InvalidModel,
    InvalidLease,
    InvalidWait,
    BodyTooLarge,
    BodyTimeout,
    /// The gate's buffer for request bodies is full.
    GateBusy,
    /// The request asked for an admin force without the admin token.
    AdminRequired,
    /// The router lists no model or alias of that name.
    ModelNotFound,
    /// The query string is too long or has too many keys to be checked.
    InvalidQuery,
}

impl Refusal {
    /// Stable machine-readable code (clients branch on it).
    pub fn code(self) -> &'static str {
        match self {
            Self::QueueFull => "inference_queue_full",
            Self::QueueTimeout => "inference_queue_timeout",
            Self::EngineUnavailable => "engine_unavailable",
            Self::SwitchStuck => "engine_switch_stuck",
            Self::LeaseUnknown => "lease_unknown",
            Self::LeaseModelMismatch => "lease_model_mismatch",
            Self::ModelInUse => "model_in_use",
            Self::EngineBusy => "engine_busy",
            Self::InvalidPath => "invalid_path",
            Self::InvalidJson => "invalid_json",
            Self::InvalidModel => "invalid_model",
            Self::InvalidLease => "invalid_lease",
            Self::InvalidWait => "invalid_wait",
            Self::BodyTooLarge => "body_too_large",
            Self::BodyTimeout => "body_timeout",
            Self::GateBusy => "gate_busy",
            Self::AdminRequired => "admin_required",
            Self::ModelNotFound => "model_not_found",
            Self::InvalidQuery => "invalid_query",
        }
    }

    /// The HTTP status the gate answers with.
    pub fn http_status(self) -> u16 {
        match self {
            Self::QueueFull | Self::QueueTimeout | Self::SwitchStuck | Self::GateBusy => 503,
            Self::EngineUnavailable => 502,
            Self::LeaseUnknown | Self::ModelInUse | Self::EngineBusy => 409,
            Self::LeaseModelMismatch
            | Self::InvalidPath
            | Self::InvalidJson
            | Self::InvalidModel
            | Self::InvalidLease
            | Self::InvalidWait
            | Self::InvalidQuery => 400,
            Self::ModelNotFound => 404,
            Self::BodyTooLarge => 413,
            Self::BodyTimeout => 408,
            Self::AdminRequired => 403,
        }
    }

    /// Whether the same request may succeed later without changes.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::QueueFull
                | Self::QueueTimeout
                | Self::EngineUnavailable
                | Self::SwitchStuck
                | Self::LeaseUnknown
                | Self::ModelInUse
                | Self::EngineBusy
                | Self::GateBusy
        )
    }

    /// Plain text for people.
    pub fn message(self) -> &'static str {
        match self {
            Self::QueueFull => "Too many requests are waiting for the local model. Try again shortly.",
            Self::QueueTimeout => "Another chat was still using the local model. Try again when it has finished.",
            Self::EngineUnavailable => "The local model engine is not answering. Try again shortly.",
            Self::SwitchStuck => "The local model engine is still releasing the previous model. Try again shortly.",
            Self::LeaseUnknown => "This chat's turn on the local model has ended. Start the request again.",
            Self::LeaseModelMismatch => "The request names another model than its turn on the local model.",
            Self::ModelInUse => "The model is answering a request. Unload it when it is idle, or unload it anyway as an admin.",
            Self::EngineBusy => "The local model engine is busy switching or answering. Try again shortly.",
            Self::InvalidPath => "The request path is not valid.",
            Self::InvalidJson => "The request body is not valid JSON.",
            Self::InvalidModel => "The request does not name a valid model.",
            Self::InvalidLease => "The lease header is not valid.",
            Self::InvalidWait => "The wait limit is not valid.",
            Self::BodyTooLarge => "The request body is too large.",
            Self::BodyTimeout => "The request body did not arrive in time.",
            Self::GateBusy => "The inference gate is busy. Try again shortly.",
            Self::AdminRequired => "Unloading a model that is in use needs an admin.",
            Self::ModelNotFound => "The local model engine has no model of that name.",
            Self::InvalidQuery => "The request query is too long or has too many parameters.",
        }
    }
}

/// Whether a waiter wants one request run or a lease granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitKind {
    Request,
    Lease,
}

/// One admitted request; handed back to [`Scheduler::finish`] when it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub model: String,
    pub lease: Option<LeaseKey>,
    /// A request that asked the router not to load anything (`autoload=false`): when it ends it
    /// leaves no residency grace, so a client polling such reads never keeps its model from a
    /// switch.
    pub read_only: bool,
}

/// What an admission gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    Request(Ticket),
    Lease(LeaseKey),
}

/// The answer to [`Scheduler::arrive`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arrival {
    Admitted(Admission),
    /// Waiting; `position` is the 1-based place of its model's group in the queue.
    Queued {
        waiter: WaiterKey,
        position: usize,
    },
    Refused(Refusal),
    /// Cannot load or use the slot (it names a model that is not resident and asks the router
    /// not to load it): forwarded without admission; the router answers it.
    Pass,
}

/// What a waiter is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Grant {
    Request(Ticket),
    Lease(LeaseKey),
}

/// Why a lease ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseEnd {
    Released,
    Idle,
    HardLimit,
    /// An admin forced its model out.
    Forced,
}

/// Work for the caller, drained by [`Scheduler::poll`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Ask the router to unload this idle model (`POST /models/unload`) and report the answer
    /// through [`Scheduler::unload_result`].
    Unload {
        model: String,
    },
    Granted {
        waiter: WaiterKey,
        grant: Grant,
        waited_ms: u64,
    },
    Rejected {
        waiter: WaiterKey,
        refusal: Refusal,
        waited_ms: u64,
    },
    LeaseEnded {
        lease: LeaseKey,
        end: LeaseEnd,
    },
}

/// The router's answer to an unload the scheduler asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnloadOutcome {
    /// Accepted; the process is stopping (the listing shows when it is gone).
    Accepted,
    /// The model was not running (already gone).
    NotRunning,
    /// The router answered with an error: it is up, and nothing is known to have changed.
    Refused,
    /// No answer at all: nothing is known to have changed.
    Failed,
}

#[derive(Clone, Debug)]
struct Waiter {
    model: String,
    kind: WaitKind,
    arrived: u64,
    deadline: u64,
}

#[derive(Clone, Debug)]
struct Group {
    model: String,
    first: u64,
    waiters: Vec<WaiterKey>,
}

#[derive(Clone, Debug)]
struct Lease {
    model: String,
    active: u32,
    last: u64,
    created: u64,
}

/// A detached session generation holding its model (see [`Scheduler::hold_session`]).
#[derive(Clone, Debug)]
struct SessionHold {
    model: String,
    until: u64,
    /// The hold is never extended past this.
    cap: u64,
    /// Numbers holds, so an answer about an older generation never ends a newer hold.
    generation: u64,
}

/// Most session holds tracked one by one; past this a hold falls back to a plain residency hold
/// of its model (which cannot be released early).
const MAX_SESSION_HOLDS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Switch {
    Idle,
    /// `attempt` counts unload requests for this switch (re-probes of a stuck one included).
    Unloading {
        model: String,
        since: u64,
        attempt: u32,
    },
    Stuck {
        model: String,
        since: u64,
        attempt: u32,
    },
    /// A preset reload is running: the router may unload any model, so nothing new is admitted.
    Maintenance {
        since: u64,
    },
}

/// How often a stuck unload is asked for again before only an admin force or the listing ends it.
const STUCK_RETRIES: u32 = 3;
/// Consecutive failed listings (without a connection error) before the router counts as down.
const LISTING_FAILURES_FOR_DOWN: u32 = 3;
/// After the router refused an unload, the next switch attempt waits this long.
const REFUSED_UNLOAD_BACKOFF_MS: u64 = 2_000;
/// The longest a maintenance state can last if its end is never reported.
const MAINTENANCE_CAP_MS: u64 = 10 * 60 * 1000;

/// Running totals, for the gate's metrics.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Counters {
    pub arrivals: u64,
    pub admitted: u64,
    pub queued: u64,
    pub granted: u64,
    pub rejected: BTreeMap<&'static str, u64>,
    pub switches: u64,
    pub unloads_refused: u64,
    pub unloads_forced: u64,
    pub leases_granted: u64,
    pub leases_ended: u64,
    pub wait_count: u64,
    pub wait_total_ms: u64,
    pub wait_max_ms: u64,
    pub finish_without_admission: u64,
}

/// A queued group, as reported.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct QueuedGroup {
    pub model: String,
    pub waiters: usize,
    pub waited_ms: u64,
}

/// The scheduler's state, as reported by `GET /gate/v1/status`. Model ids, counts and times only.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Status {
    pub holder: Option<String>,
    pub switch: &'static str,
    pub switch_model: Option<String>,
    pub in_flight: BTreeMap<String, u32>,
    pub leases: usize,
    pub queue: Vec<QueuedGroup>,
    pub router_up: bool,
    /// Detached session generations holding their model.
    pub session_holds: usize,
    pub counters: Counters,
}

/// The admission scheduler. See the crate docs.
#[derive(Clone, Debug)]
pub struct Scheduler {
    cfg: Config,
    holder: Option<String>,
    switch: Switch,
    in_flight: BTreeMap<String, u32>,
    leases: BTreeMap<LeaseKey, Lease>,
    waiters: BTreeMap<WaiterKey, Waiter>,
    queue: VecDeque<Group>,
    next_key: u64,
    listing: Option<Listing>,
    small: BTreeSet<String>,
    router_down_since: Option<u64>,
    listing_failures: u32,
    switch_backoff_until: u64,
    grace_until: BTreeMap<String, u64>,
    /// Conversation id to the hold of its detached generation.
    session_holds: BTreeMap<String, SessionHold>,
    /// Listing reads started so far. A listing is known by the number its read was started
    /// with, so a listing whose read started before a model's last work (or before an unload
    /// was asked for) is stale for it, however late its answer arrives.
    epoch: u64,
    /// The read number of the listing applied last; an older answer is ignored.
    listing_ticket: u64,
    /// The read number current when the running unload was asked for.
    switch_epoch: u64,
    touched_at: BTreeMap<String, u64>,
    actions: Vec<Action>,
    counters: Counters,
}

impl Scheduler {
    /// A scheduler with nothing known about the engine yet.
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            holder: None,
            switch: Switch::Idle,
            in_flight: BTreeMap::new(),
            leases: BTreeMap::new(),
            waiters: BTreeMap::new(),
            queue: VecDeque::new(),
            next_key: 1,
            listing: None,
            small: BTreeSet::new(),
            router_down_since: None,
            listing_failures: 0,
            switch_backoff_until: 0,
            grace_until: BTreeMap::new(),
            session_holds: BTreeMap::new(),
            epoch: 0,
            listing_ticket: 0,
            switch_epoch: 0,
            touched_at: BTreeMap::new(),
            actions: Vec::new(),
            counters: Counters::default(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The id the router resolves `name` to (an alias names its model); `name` itself when the
    /// last listing does not know it.
    pub fn canon(&self, name: &str) -> String {
        self.listing
            .as_ref()
            .and_then(|l| l.canonical(name))
            .unwrap_or(name)
            .to_owned()
    }

    /// Whether the last listing knows `name` as a model id or alias (`true` before any listing).
    pub fn knows(&self, name: &str) -> bool {
        self.listing
            .as_ref()
            .is_none_or(|l| l.canonical(name).is_some())
    }

    /// Whether `model` (an id or alias) never takes the chat slot.
    pub fn exempt(&self, model: &str) -> bool {
        if self.cfg.exempt.contains(model) {
            return true;
        }
        let id = self.canon(model);
        if self.cfg.exempt.contains(&id) || self.small.contains(&id) {
            return true;
        }
        self.listing
            .as_ref()
            .and_then(|l| l.row(&id))
            .is_some_and(|row| row.aliases.iter().any(|a| self.cfg.exempt.contains(a)))
    }

    /// The chat model holding the slot, if any.
    pub fn holder(&self) -> Option<&str> {
        self.holder.as_deref()
    }

    /// Requests in flight on `model` (an id or alias).
    pub fn in_flight(&self, model: &str) -> u32 {
        let id = self.canon(model);
        self.in_flight.get(&id).copied().unwrap_or(0)
    }

    /// Live leases on `model` (an id or alias).
    pub fn live_leases(&self, model: &str) -> usize {
        let id = self.canon(model);
        self.leases.values().filter(|l| l.model == id).count()
    }

    /// The model a lease holds.
    pub fn lease_model(&self, lease: LeaseKey) -> Option<&str> {
        self.leases.get(&lease).map(|l| l.model.as_str())
    }

    /// Whether the switch is stuck on an unload.
    pub fn stuck(&self) -> bool {
        matches!(self.switch, Switch::Stuck { .. })
    }

    /// Whether the router is currently considered unreachable.
    pub fn router_down(&self) -> bool {
        self.router_down_since.is_some()
    }

    /// Whether an unload (by the scheduler or an admin) is in progress or stuck.
    pub fn switching(&self) -> bool {
        !matches!(self.switch, Switch::Idle)
    }

    /// The model being unloaded or stuck, if any.
    pub fn switch_model(&self) -> Option<&str> {
        match &self.switch {
            Switch::Idle | Switch::Maintenance { .. } => None,
            Switch::Unloading { model, .. } | Switch::Stuck { model, .. } => Some(model),
        }
    }

    /// Whether a preset reload is running.
    pub fn in_maintenance(&self) -> bool {
        matches!(self.switch, Switch::Maintenance { .. })
    }

    /// Waiters in the queue.
    pub fn waiting(&self) -> usize {
        self.waiters.len()
    }

    /// The models of the queued groups, front first.
    pub fn queued_models(&self) -> Vec<String> {
        self.queue.iter().map(|g| g.model.clone()).collect()
    }

    /// A waiter's 1-based group position, or `None` when it no longer waits.
    pub fn position(&self, waiter: WaiterKey) -> Option<usize> {
        self.queue
            .iter()
            .position(|g| g.waiters.contains(&waiter))
            .map(|i| i + 1)
    }

    /// `model` is an id (callers canonicalise first).
    fn active(&self, model: &str) -> bool {
        self.in_flight.get(model).is_some_and(|n| *n > 0)
            || self.leases.values().any(|l| l.model == model)
    }

    fn in_grace(&self, model: &str, now: u64) -> bool {
        self.grace_until
            .get(model)
            .is_some_and(|&until| now < until)
            || self
                .session_holds
                .values()
                .any(|h| h.model == model && now < h.until)
    }

    /// A group for another model has waited past the fair hold: the holder takes no new work.
    fn draining(&self, now: u64) -> bool {
        let Some(holder) = self.holder.as_deref() else {
            return false;
        };
        self.queue
            .iter()
            .any(|g| g.model != holder && now.saturating_sub(g.first) >= self.cfg.fair_hold_ms)
    }

    fn next(&mut self) -> u64 {
        let key = self.next_key;
        self.next_key = self.next_key.wrapping_add(1).max(1);
        key
    }

    fn reject_count(&mut self, refusal: Refusal) {
        *self.counters.rejected.entry(refusal.code()).or_insert(0) += 1;
    }

    fn refuse(&mut self, refusal: Refusal) -> Arrival {
        self.reject_count(refusal);
        Arrival::Refused(refusal)
    }

    fn lease_capacity_left(&self) -> bool {
        let queued_leases = self
            .waiters
            .values()
            .filter(|w| w.kind == WaitKind::Lease)
            .count();
        self.leases.len() + queued_leases < self.cfg.max_leases
    }

    /// A request (or lease request) that may load or run on `model` (an id or alias) arrives.
    ///
    /// - A request carrying a live lease for `model` always runs (its turn continues), even while
    ///   the router is thought to be down (its own forward then fails if it really is).
    /// - An exempt model runs at once and never takes the slot.
    /// - With the slot free and no queue, or on the holder while no other model waits (or while
    ///   the holder is busy and the fair hold has not passed), it runs at once.
    /// - Otherwise it joins its model's group in the queue, for at most `max_wait_ms` (capped by
    ///   the configured maximum).
    pub fn arrive(
        &mut self,
        now: u64,
        model: &str,
        kind: WaitKind,
        lease: Option<LeaseKey>,
        max_wait_ms: u64,
    ) -> Arrival {
        self.counters.arrivals += 1;
        let id = self.canon(model);
        let model = id.as_str();
        if let Some(key) = lease {
            let Some(held) = self.leases.get_mut(&key) else {
                return self.refuse(Refusal::LeaseUnknown);
            };
            if held.model != model || kind == WaitKind::Lease {
                return self.refuse(Refusal::LeaseModelMismatch);
            }
            held.active += 1;
            held.last = now;
            return self.admit_request(model, Some(key));
        }
        if self.router_down() {
            return self.refuse(Refusal::EngineUnavailable);
        }
        // A preset reload may restart any model: chat work queues behind it (below); exempt work,
        // which never queues, is refused until it ends.
        if self.in_maintenance() && self.exempt(model) {
            return self.refuse(Refusal::EngineBusy);
        }
        if self.exempt(model) {
            return match kind {
                WaitKind::Request => self.admit_request(model, None),
                WaitKind::Lease => self.admit_lease(now, model),
            };
        }
        if self.stuck() {
            return self.refuse(Refusal::SwitchStuck);
        }
        if self.may_admit_now(now, model) {
            self.holder = Some(model.to_owned());
            return match kind {
                WaitKind::Request => self.admit_request(model, None),
                WaitKind::Lease => self.admit_lease(now, model),
            };
        }
        if self.waiters.len() >= self.cfg.max_waiters
            || (kind == WaitKind::Lease && !self.lease_capacity_left())
        {
            return self.refuse(Refusal::QueueFull);
        }
        let key = WaiterKey(self.next());
        let wait = max_wait_ms.clamp(1, self.cfg.max_wait_ms);
        self.waiters.insert(
            key,
            Waiter {
                model: model.to_owned(),
                kind,
                arrived: now,
                deadline: now.saturating_add(wait),
            },
        );
        let position = match self.queue.iter().position(|g| g.model == model) {
            Some(index) => {
                if let Some(group) = self.queue.get_mut(index) {
                    group.waiters.push(key);
                }
                index + 1
            }
            None => {
                self.queue.push_back(Group {
                    model: model.to_owned(),
                    first: now,
                    waiters: vec![key],
                });
                self.queue.len()
            }
        };
        self.counters.queued += 1;
        Arrival::Queued {
            waiter: key,
            position,
        }
    }

    /// A request that names `model` but asks the router not to load it (`autoload=false`). It
    /// never queues and never causes a switch:
    /// - with a live lease it is handled like any request of that lease;
    /// - when the model is resident here (the holder, or an exempt model the router lists as
    ///   resident) it is admitted and counted at once, even while other models wait (it runs on
    ///   the loaded model and loads nothing), and leaves no residency grace when it ends.
    ///   This bypasses the fair hold on purpose: such a request can never cause a switch, so
    ///   queueing it would only make it wait for, and then trigger, a needless unload. The cost
    ///   is that overlapping `autoload=false` generations on the holder keep it busy longer;
    ///   clients use `autoload=false` for cheap reads. It is
    ///   refused while the router is unreachable, a preset reload runs or its model is being
    ///   unloaded;
    /// - otherwise it cannot load or use the slot and is passed to the router, which answers that
    ///   the model is not loaded.
    pub fn arrive_without_autoload(
        &mut self,
        now: u64,
        model: &str,
        lease: Option<LeaseKey>,
        max_wait_ms: u64,
    ) -> Arrival {
        let id = self.canon(model);
        if lease.is_some() {
            return self.arrive(now, &id, WaitKind::Request, lease, max_wait_ms);
        }
        let resident_here = self.holder.as_deref() == Some(id.as_str())
            || (self.exempt(&id)
                && self
                    .listing
                    .as_ref()
                    .and_then(|l| l.status_of(&id))
                    .is_some_and(|s| s.resident()));
        self.counters.arrivals += 1;
        if !resident_here {
            return Arrival::Pass;
        }
        if self.router_down() {
            return self.refuse(Refusal::EngineUnavailable);
        }
        if self.in_maintenance() || self.switch_model() == Some(id.as_str()) {
            return self.refuse(Refusal::EngineBusy);
        }
        match self.admit_request(&id, None) {
            Arrival::Admitted(Admission::Request(mut ticket)) => {
                ticket.read_only = true;
                Arrival::Admitted(Admission::Request(ticket))
            }
            other => other,
        }
    }

    fn may_admit_now(&self, now: u64, model: &str) -> bool {
        if self.switching() {
            return false;
        }
        match self.holder.as_deref() {
            None => self.queue.is_empty(),
            Some(holder) if holder == model => {
                let others_wait = self.queue.iter().any(|g| g.model != holder);
                // Work in flight, a live lease or the residency grace keeps the holder's next
                // calls riding along until the fair hold has passed.
                !others_wait
                    || ((self.active(holder) || self.in_grace(holder, now)) && !self.draining(now))
            }
            Some(_) => false,
        }
    }

    fn touch(&mut self, model: &str) {
        self.touched_at.insert(model.to_owned(), self.epoch);
    }

    fn admit_request(&mut self, model: &str, lease: Option<LeaseKey>) -> Arrival {
        self.touch(model);
        *self.in_flight.entry(model.to_owned()).or_insert(0) += 1;
        self.counters.admitted += 1;
        Arrival::Admitted(Admission::Request(Ticket {
            model: model.to_owned(),
            lease,
            read_only: false,
        }))
    }

    fn admit_lease(&mut self, now: u64, model: &str) -> Arrival {
        if self.leases.len() >= self.cfg.max_leases {
            return self.refuse(Refusal::QueueFull);
        }
        let key = self.create_lease(now, model);
        self.counters.admitted += 1;
        Arrival::Admitted(Admission::Lease(key))
    }

    fn create_lease(&mut self, now: u64, model: &str) -> LeaseKey {
        self.touch(model);
        let key = LeaseKey(self.next());
        self.leases.insert(
            key,
            Lease {
                model: model.to_owned(),
                active: 0,
                last: now,
                created: now,
            },
        );
        self.counters.leases_granted += 1;
        key
    }

    /// An admitted request ended (completed, failed or its client went away). A request without
    /// a lease leaves its model the residency grace.
    pub fn finish(&mut self, now: u64, ticket: &Ticket) {
        match self.in_flight.get_mut(&ticket.model) {
            Some(count) if *count > 0 => {
                *count -= 1;
                if *count == 0 {
                    self.in_flight.remove(&ticket.model);
                }
            }
            _ => {
                self.counters.finish_without_admission += 1;
                return;
            }
        }
        self.touch(&ticket.model);
        match ticket.lease {
            Some(key) => {
                if let Some(lease) = self.leases.get_mut(&key) {
                    lease.active = lease.active.saturating_sub(1);
                    lease.last = now;
                }
            }
            None => {
                if !ticket.read_only && !self.exempt(&ticket.model) {
                    self.extend_grace(
                        &ticket.model,
                        now.saturating_add(self.cfg.residency_grace_ms),
                    );
                }
            }
        }
    }

    fn extend_grace(&mut self, model: &str, until: u64) {
        let entry = self.grace_until.entry(model.to_owned()).or_insert(0);
        *entry = (*entry).max(until);
    }

    /// A session generation (conversation `conv_id`) on `model` lost its client before its
    /// answer ended: the router may go on generating it for a later resume, so the model is held
    /// for the session hold (never switched away meanwhile; the fair hold still stops new work
    /// riding along). The hold is tracked per conversation and numbered: a new generation of the
    /// same conversation replaces it with a fresh hold (new number, hold and cap from now), as
    /// the router replaces the session. [`Scheduler::release_session`] ends it early (an
    /// explicit stop, or the generation ended), [`Scheduler::release_session_generation`] ends it
    /// only if it is still the hold of that number (a lookup answer about it), and
    /// [`Scheduler::extend_session`] renews it while the router still runs the generation, up to
    /// the lease hard limit after it began.
    pub fn hold_session(&mut self, now: u64, conv_id: &str, model: &str) {
        let id = self.canon(model);
        if self.exempt(&id) {
            return;
        }
        let until = now.saturating_add(self.cfg.session_hold_ms);
        if !self.session_holds.contains_key(conv_id)
            && self.session_holds.len() >= MAX_SESSION_HOLDS
        {
            self.extend_grace(&id, until);
            return;
        }
        let cap = now.saturating_add(self.cfg.session_hold_ms.max(self.cfg.lease_hard_ms));
        let generation = self.next();
        self.session_holds.insert(
            conv_id.to_owned(),
            SessionHold {
                model: id,
                until,
                cap,
                generation,
            },
        );
    }

    /// The router still runs the detached generation of `conv_id` whose hold has number
    /// `generation`: the hold is renewed for another session hold (never past its cap).
    /// `false`: no live hold of that number.
    pub fn extend_session(&mut self, now: u64, conv_id: &str, generation: u64) -> bool {
        let hold_ms = self.cfg.session_hold_ms;
        match self.session_holds.get_mut(conv_id) {
            Some(hold) if now < hold.until && hold.generation == generation => {
                hold.until = hold.until.max(now.saturating_add(hold_ms)).min(hold.cap);
                true
            }
            _ => false,
        }
    }

    /// The session `conv_id` was stopped or its generation ended: its hold ends now. `false`: no
    /// such hold.
    pub fn release_session(&mut self, conv_id: &str) -> bool {
        self.session_holds.remove(conv_id).is_some()
    }

    /// The router reports the generation whose hold has number `generation` done: that hold
    /// ends now. A hold made since (a newer generation) is kept. `false`: no hold of that number.
    pub fn release_session_generation(&mut self, conv_id: &str, generation: u64) -> bool {
        if self
            .session_holds
            .get(conv_id)
            .is_some_and(|h| h.generation == generation)
        {
            self.session_holds.remove(conv_id);
            true
        } else {
            false
        }
    }

    /// The live session holds: conversation id and hold number.
    pub fn held_sessions(&self, now: u64) -> Vec<(String, u64)> {
        self.session_holds
            .iter()
            .filter(|(_, h)| now < h.until)
            .map(|(c, h)| (c.clone(), h.generation))
            .collect()
    }

    /// The model a live session hold keeps, if any.
    pub fn session_hold_model(&self, now: u64, conv_id: &str) -> Option<&str> {
        self.session_holds
            .get(conv_id)
            .filter(|h| now < h.until)
            .map(|h| h.model.as_str())
    }

    /// A stream resume (`GET /v1/stream`) of a session on `model` (the holder when the session's
    /// model is not known). It runs on that model without loading anything, so it is counted in
    /// flight when the model is resident here and no unload of it is under way; `None`: not
    /// counted (the router answers it, typically that the stream is gone).
    pub fn attach(&mut self, model: Option<&str>) -> Option<Ticket> {
        let id = match model {
            Some(model) => self.canon(model),
            None => self.holder.clone()?,
        };
        if self.switch_model() == Some(id.as_str()) {
            return None;
        }
        let resident = self.holder.as_deref() == Some(id.as_str())
            || (self.exempt(&id)
                && self
                    .listing
                    .as_ref()
                    .and_then(|l| l.status_of(&id))
                    .is_some_and(|s| s.resident()));
        if !resident {
            return None;
        }
        self.touch(&id);
        *self.in_flight.entry(id.clone()).or_insert(0) += 1;
        Some(Ticket {
            model: id,
            lease: None,
            read_only: false,
        })
    }

    /// The client ends its lease. Its requests still in flight keep counting until they end.
    pub fn release_lease(&mut self, lease: LeaseKey) -> bool {
        if self.leases.remove(&lease).is_some() {
            self.counters.leases_ended += 1;
            true
        } else {
            false
        }
    }

    /// A waiter's client went away.
    pub fn cancel(&mut self, waiter: WaiterKey) {
        if self.waiters.remove(&waiter).is_none() {
            return;
        }
        for group in &mut self.queue {
            group.waiters.retain(|w| *w != waiter);
        }
        self.queue.retain(|g| !g.waiters.is_empty());
    }

    /// A read of the router's listing starts; its answer goes to [`Scheduler::observe_at`] with
    /// the returned number.
    pub fn listing_requested(&mut self) -> u64 {
        self.epoch = self.epoch.saturating_add(1);
        self.epoch
    }

    /// A listing read just now (both steps at once).
    pub fn observe(&mut self, listing: Listing) {
        let ticket = self.listing_requested();
        self.observe_at(ticket, listing);
    }

    /// The router answered the listing read numbered `ticket`.
    pub fn observe_at(&mut self, ticket: u64, listing: Listing) {
        self.router_down_since = None;
        self.listing_failures = 0;
        if ticket <= self.listing_ticket {
            // A read that started before the listing already applied: older news.
            return;
        }
        self.listing_ticket = ticket;
        self.small = listing
            .rows
            .iter()
            .filter(|r| r.small)
            .map(|r| r.id.clone())
            .collect();
        if let Some(model) = self.switch_model().map(str::to_owned) {
            // Only a read started after the unload was asked for can show the model gone.
            let gone = ticket > self.switch_epoch
                && !listing.status_of(&model).is_some_and(|s| s.resident());
            if gone {
                self.switch = Switch::Idle;
                if self.holder.as_deref() == Some(model.as_str()) && !self.active(&model) {
                    self.holder = None;
                }
            }
        }
        if !self.switching() && self.holder.is_none() {
            let resident = listing
                .rows
                .iter()
                .find(|r| r.status.resident() && !self.exempt(&r.id));
            if let Some(row) = resident {
                self.holder = Some(row.id.clone());
            }
        }
        self.listing = Some(listing);
    }

    /// The router could not be reached at all (a connection was refused or failed).
    pub fn router_unreachable(&mut self, now: u64) {
        self.router_down_since.get_or_insert(now);
    }

    /// A listing failed without a connection error (a slow or malformed answer). Only several in
    /// a row count as the router being down, so one slow listing never fails a turn.
    pub fn listing_failed(&mut self, now: u64) {
        self.listing_failures = self.listing_failures.saturating_add(1);
        if self.listing_failures >= LISTING_FAILURES_FOR_DOWN {
            self.router_unreachable(now);
        }
    }

    /// The router answered an unload this scheduler asked for.
    pub fn unload_result(&mut self, now: u64, model: &str, outcome: UnloadOutcome) {
        if self.switch_model() != Some(model) {
            return;
        }
        match outcome {
            UnloadOutcome::Accepted => {}
            UnloadOutcome::NotRunning => {
                self.switch = Switch::Idle;
                if self.holder.as_deref() == Some(model) && !self.active(model) {
                    self.holder = None;
                }
            }
            UnloadOutcome::Refused => {
                // The router is up but did not unload: try the switch again a little later.
                self.switch = Switch::Idle;
                self.switch_backoff_until = now.saturating_add(REFUSED_UNLOAD_BACKOFF_MS);
            }
            UnloadOutcome::Failed => {
                self.switch = Switch::Idle;
                self.router_unreachable(now);
            }
        }
    }

    /// An explicit unload (`POST /models/unload`, `DELETE /models`) of `model` (an id or alias).
    /// Refused while the model is in use unless `force` (the caller checked the admin token). On
    /// `Ok`, the caller forwards the unload and reports the answer through
    /// [`Scheduler::unload_result`] (it must, whatever happens to its client). When the model holds
    /// the chat slot, new chat work waits until the listing shows it gone; an exempt or
    /// non-resident model never blocks chat admission.
    pub fn admin_unload(&mut self, now: u64, model: &str, force: bool) -> Result<(), Refusal> {
        let id = self.canon(model);
        let model = id.as_str();
        if self.router_down() {
            self.reject_count(Refusal::EngineUnavailable);
            return Err(Refusal::EngineUnavailable);
        }
        let same = self.switch_model() == Some(model);
        if self.in_maintenance() || (self.switching() && !(same && force)) {
            self.counters.unloads_refused += 1;
            self.reject_count(Refusal::EngineBusy);
            return Err(Refusal::EngineBusy);
        }
        if !force && self.active(model) {
            self.counters.unloads_refused += 1;
            self.reject_count(Refusal::ModelInUse);
            return Err(Refusal::ModelInUse);
        }
        if force && self.active(model) {
            self.counters.unloads_forced += 1;
            let ended: Vec<LeaseKey> = self
                .leases
                .iter()
                .filter(|(_, l)| l.model == model)
                .map(|(k, _)| *k)
                .collect();
            for lease in ended {
                self.leases.remove(&lease);
                self.counters.leases_ended += 1;
                self.actions.push(Action::LeaseEnded {
                    lease,
                    end: LeaseEnd::Forced,
                });
            }
        }
        let holds_slot = self.holder.as_deref() == Some(model)
            || (!self.exempt(model)
                && self
                    .listing
                    .as_ref()
                    .and_then(|l| l.status_of(model))
                    .is_some_and(|s| s.resident()));
        if !same && holds_slot {
            self.switch_epoch = self.epoch;
            self.switch = Switch::Unloading {
                model: model.to_owned(),
                since: now,
                attempt: 1,
            };
        }
        Ok(())
    }

    /// A router preset reload starts. The router unloads every model whose preset changed, busy or
    /// not, so it is allowed only while nothing is in flight, no lease is live and no switch runs
    /// (an admin may force it past work in flight). On `Ok` the scheduler is in maintenance: no
    /// new work is admitted (it queues) until [`Scheduler::end_maintenance`], which the caller must
    /// report however the reload ends.
    pub fn begin_maintenance(&mut self, now: u64, force: bool) -> Result<(), Refusal> {
        if self.router_down() {
            self.reject_count(Refusal::EngineUnavailable);
            return Err(Refusal::EngineUnavailable);
        }
        let busy = !self.in_flight.is_empty() || !self.leases.is_empty();
        if self.switching() || (busy && !force) {
            self.reject_count(Refusal::EngineBusy);
            return Err(Refusal::EngineBusy);
        }
        self.switch = Switch::Maintenance { since: now };
        Ok(())
    }

    /// The preset reload ended (answered, failed or timed out).
    pub fn end_maintenance(&mut self) {
        if self.in_maintenance() {
            self.switch = Switch::Idle;
        }
    }

    /// Advances time: ends leases, refuses expired waiters, notices a stuck unload, and when the
    /// holder is idle and another model waits, asks for its unload or grants the next group.
    /// Returns the actions for the caller.
    pub fn poll(&mut self, now: u64) -> Vec<Action> {
        self.session_holds.retain(|_, h| now < h.until);
        self.expire_leases(now);
        if let Some(since) = self.router_down_since {
            if now.saturating_sub(since) >= self.cfg.router_down_grace_ms {
                self.reject_all(now, Refusal::EngineUnavailable);
            }
        }
        let expired: Vec<WaiterKey> = self
            .waiters
            .iter()
            .filter(|(_, w)| now >= w.deadline)
            .map(|(k, _)| *k)
            .collect();
        for key in expired {
            self.reject(now, key, Refusal::QueueTimeout);
        }
        match &self.switch {
            Switch::Unloading {
                model,
                since,
                attempt,
            } if now.saturating_sub(*since) >= self.cfg.unload_timeout_ms => {
                self.switch = Switch::Stuck {
                    model: model.clone(),
                    since: now,
                    attempt: *attempt,
                };
            }
            // A stuck unload is asked for again (bounded) while its model is idle: the first
            // request may never have reached the router.
            Switch::Stuck {
                model,
                since,
                attempt,
            } if *attempt < STUCK_RETRIES
                && now.saturating_sub(*since) >= self.cfg.unload_timeout_ms
                && !self.active(model) =>
            {
                let model = model.clone();
                self.switch_epoch = self.epoch;
                self.switch = Switch::Unloading {
                    model: model.clone(),
                    since: now,
                    attempt: attempt.saturating_add(1),
                };
                self.actions.push(Action::Unload { model });
            }
            Switch::Maintenance { since } if now.saturating_sub(*since) >= MAINTENANCE_CAP_MS => {
                self.switch = Switch::Idle;
            }
            _ => {}
        }
        if self.stuck() {
            self.reject_all(now, Refusal::SwitchStuck);
        }
        self.clear_departed_holder(now);
        if !self.switching() && !self.router_down() {
            self.advance_queue(now);
        }
        std::mem::take(&mut self.actions)
    }

    fn expire_leases(&mut self, now: u64) {
        let ended: Vec<(LeaseKey, LeaseEnd)> = self
            .leases
            .iter()
            .filter_map(|(key, lease)| {
                if now.saturating_sub(lease.created) >= self.cfg.lease_hard_ms {
                    Some((*key, LeaseEnd::HardLimit))
                } else if lease.active == 0
                    && now.saturating_sub(lease.last) >= self.cfg.lease_idle_ms
                {
                    Some((*key, LeaseEnd::Idle))
                } else {
                    None
                }
            })
            .collect();
        for (lease, end) in ended {
            self.leases.remove(&lease);
            self.counters.leases_ended += 1;
            self.actions.push(Action::LeaseEnded { lease, end });
        }
    }

    /// The holder is idle and the router lists it as not resident (it went away): the slot is
    /// free without an unload.
    fn clear_departed_holder(&mut self, now: u64) {
        let Some(holder) = self.holder.clone() else {
            return;
        };
        if self.switching() || self.active(&holder) || self.in_grace(&holder, now) {
            return;
        }
        // Only a listing read after the model's last work can show that it went away.
        let touched = self.touched_at.get(&holder).copied().unwrap_or(0);
        if self.listing_ticket <= touched {
            return;
        }
        let resident = self
            .listing
            .as_ref()
            .and_then(|l| l.status_of(&holder))
            .is_some_and(|s| s.resident());
        if self.listing.is_some() && !resident {
            self.holder = None;
        }
    }

    fn advance_queue(&mut self, now: u64) {
        let Some(front) = self.queue.front().map(|g| g.model.clone()) else {
            return;
        };
        match self.holder.clone() {
            None => self.grant_front(now),
            Some(holder) if holder == front => self.grant_front(now),
            Some(holder) => {
                if self.active(&holder)
                    || self.in_grace(&holder, now)
                    || now < self.switch_backoff_until
                {
                    return;
                }
                self.counters.switches += 1;
                self.switch_epoch = self.epoch;
                self.switch = Switch::Unloading {
                    model: holder.clone(),
                    since: now,
                    attempt: 1,
                };
                self.actions.push(Action::Unload { model: holder });
            }
        }
    }

    fn grant_front(&mut self, now: u64) {
        let Some(group) = self.queue.pop_front() else {
            return;
        };
        self.holder = Some(group.model.clone());
        self.touch(&group.model);
        for key in group.waiters {
            let Some(waiter) = self.waiters.remove(&key) else {
                continue;
            };
            let waited_ms = now.saturating_sub(waiter.arrived);
            let grant = match waiter.kind {
                WaitKind::Request => {
                    *self.in_flight.entry(waiter.model.clone()).or_insert(0) += 1;
                    Grant::Request(Ticket {
                        model: waiter.model.clone(),
                        lease: None,
                        read_only: false,
                    })
                }
                WaitKind::Lease => {
                    if self.leases.len() >= self.cfg.max_leases {
                        self.reject_count(Refusal::QueueFull);
                        self.actions.push(Action::Rejected {
                            waiter: key,
                            refusal: Refusal::QueueFull,
                            waited_ms,
                        });
                        continue;
                    }
                    Grant::Lease(self.create_lease(now, &waiter.model))
                }
            };
            self.counters.granted += 1;
            self.counters.wait_count += 1;
            self.counters.wait_total_ms = self.counters.wait_total_ms.saturating_add(waited_ms);
            self.counters.wait_max_ms = self.counters.wait_max_ms.max(waited_ms);
            self.actions.push(Action::Granted {
                waiter: key,
                grant,
                waited_ms,
            });
        }
    }

    fn reject(&mut self, now: u64, key: WaiterKey, refusal: Refusal) {
        let Some(waiter) = self.waiters.remove(&key) else {
            return;
        };
        for group in &mut self.queue {
            group.waiters.retain(|w| *w != key);
        }
        self.queue.retain(|g| !g.waiters.is_empty());
        self.reject_count(refusal);
        self.actions.push(Action::Rejected {
            waiter: key,
            refusal,
            waited_ms: now.saturating_sub(waiter.arrived),
        });
    }

    fn reject_all(&mut self, now: u64, refusal: Refusal) {
        let keys: Vec<WaiterKey> = self.waiters.keys().copied().collect();
        for key in keys {
            self.reject(now, key, refusal);
        }
    }

    /// The state for `GET /gate/v1/status`.
    pub fn status(&self, now: u64) -> Status {
        Status {
            holder: self.holder.clone(),
            switch: match self.switch {
                Switch::Idle => "idle",
                Switch::Unloading { .. } => "unloading",
                Switch::Stuck { .. } => "stuck",
                Switch::Maintenance { .. } => "maintenance",
            },
            switch_model: self.switch_model().map(str::to_owned),
            in_flight: self.in_flight.clone(),
            leases: self.leases.len(),
            queue: self
                .queue
                .iter()
                .map(|g| QueuedGroup {
                    model: g.model.clone(),
                    waiters: g.waiters.len(),
                    waited_ms: now.saturating_sub(g.first),
                })
                .collect(),
            router_up: !self.router_down(),
            session_holds: self.session_holds.len(),
            counters: self.counters.clone(),
        }
    }
}
