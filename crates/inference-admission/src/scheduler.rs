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
            | Self::InvalidWait => 400,
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
    /// No answer, or an error: nothing is known to have changed.
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum Switch {
    Idle,
    Unloading { model: String, since: u64 },
    Stuck { model: String, since: u64 },
}

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
    grace_until: BTreeMap<String, u64>,
    /// Listings observed so far, and how many had been observed when a model last had work: a
    /// listing older than the work is stale for that model.
    observes: u64,
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
            grace_until: BTreeMap::new(),
            observes: 0,
            touched_at: BTreeMap::new(),
            actions: Vec::new(),
            counters: Counters::default(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Whether `model` never takes the chat slot.
    pub fn exempt(&self, model: &str) -> bool {
        self.cfg.exempt.contains(model) || self.small.contains(model)
    }

    /// The chat model holding the slot, if any.
    pub fn holder(&self) -> Option<&str> {
        self.holder.as_deref()
    }

    /// Requests in flight on `model`.
    pub fn in_flight(&self, model: &str) -> u32 {
        self.in_flight.get(model).copied().unwrap_or(0)
    }

    /// Live leases on `model`.
    pub fn live_leases(&self, model: &str) -> usize {
        self.leases.values().filter(|l| l.model == model).count()
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
            Switch::Idle => None,
            Switch::Unloading { model, .. } | Switch::Stuck { model, .. } => Some(model),
        }
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

    fn active(&self, model: &str) -> bool {
        self.in_flight(model) > 0 || self.live_leases(model) > 0
    }

    fn in_grace(&self, model: &str, now: u64) -> bool {
        self.grace_until
            .get(model)
            .is_some_and(|&until| now < until)
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

    /// A request (or lease request) that may load or run on `model` arrives.
    ///
    /// - A request carrying a live lease for `model` always runs (its turn continues).
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
        if self.router_down() {
            return self.refuse(Refusal::EngineUnavailable);
        }
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

    fn may_admit_now(&self, now: u64, model: &str) -> bool {
        if self.switching() {
            return false;
        }
        match self.holder.as_deref() {
            None => self.queue.is_empty(),
            Some(holder) if holder == model => {
                let others_wait = self.queue.iter().any(|g| g.model != holder);
                !others_wait || (self.active(holder) && !self.draining(now))
            }
            Some(_) => false,
        }
    }

    fn touch(&mut self, model: &str) {
        self.touched_at.insert(model.to_owned(), self.observes);
    }

    fn admit_request(&mut self, model: &str, lease: Option<LeaseKey>) -> Arrival {
        self.touch(model);
        *self.in_flight.entry(model.to_owned()).or_insert(0) += 1;
        self.counters.admitted += 1;
        Arrival::Admitted(Admission::Request(Ticket {
            model: model.to_owned(),
            lease,
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
                if !self.exempt(&ticket.model) {
                    self.grace_until.insert(
                        ticket.model.clone(),
                        now.saturating_add(self.cfg.residency_grace_ms),
                    );
                }
            }
        }
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

    /// The router answered `GET /models`.
    pub fn observe(&mut self, listing: Listing) {
        self.router_down_since = None;
        self.observes += 1;
        self.small = listing
            .rows
            .iter()
            .filter(|r| r.small)
            .map(|r| r.id.clone())
            .collect();
        if let Some(model) = self.switch_model().map(str::to_owned) {
            let gone = !listing.status_of(&model).is_some_and(|s| s.resident());
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

    /// The router did not answer (a listing or a forwarded request failed to connect).
    pub fn router_unreachable(&mut self, now: u64) {
        self.router_down_since.get_or_insert(now);
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
            UnloadOutcome::Failed => {
                self.switch = Switch::Idle;
                self.router_unreachable(now);
            }
        }
    }

    /// An explicit unload (`POST /models/unload`, `DELETE /models`). Refused while the model is in
    /// use unless `force` (the caller checked the admin token). On `Ok`, the caller forwards the
    /// unload and reports the answer through [`Scheduler::unload_result`]; new work for any chat
    /// model waits until the listing shows the model gone.
    pub fn admin_unload(&mut self, now: u64, model: &str, force: bool) -> Result<(), Refusal> {
        if self.router_down() {
            self.reject_count(Refusal::EngineUnavailable);
            return Err(Refusal::EngineUnavailable);
        }
        let same = self.switch_model() == Some(model);
        if self.switching() && !(same && force) {
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
        if !same {
            self.switch = Switch::Unloading {
                model: model.to_owned(),
                since: now,
            };
        }
        Ok(())
    }

    /// A router preset reload. Allowed only while nothing is in flight, no lease is live and no
    /// switch runs, unless forced by an admin.
    pub fn maintenance_check(&mut self, force: bool) -> Result<(), Refusal> {
        if self.router_down() {
            self.reject_count(Refusal::EngineUnavailable);
            return Err(Refusal::EngineUnavailable);
        }
        if force {
            return Ok(());
        }
        if self.switching() || !self.in_flight.is_empty() || !self.leases.is_empty() {
            self.reject_count(Refusal::EngineBusy);
            return Err(Refusal::EngineBusy);
        }
        Ok(())
    }

    /// Advances time: ends leases, refuses expired waiters, notices a stuck unload, and when the
    /// holder is idle and another model waits, asks for its unload or grants the next group.
    /// Returns the actions for the caller.
    pub fn poll(&mut self, now: u64) -> Vec<Action> {
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
        if let Switch::Unloading { model, since } = &self.switch {
            if now.saturating_sub(*since) >= self.cfg.unload_timeout_ms {
                self.switch = Switch::Stuck {
                    model: model.clone(),
                    since: *since,
                };
            }
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
        if self.observes <= touched {
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
                if self.active(&holder) || self.in_grace(&holder, now) {
                    return;
                }
                self.counters.switches += 1;
                self.switch = Switch::Unloading {
                    model: holder.clone(),
                    since: now,
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
            counters: self.counters.clone(),
        }
    }
}
