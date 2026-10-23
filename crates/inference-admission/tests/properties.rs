//! Properties of the admission scheduler over random event sequences (sbstndalton/noevia#2115):
//! arrivals (by id or alias, with or without autoload, with or without a lease), stream resumes,
//! finishes (some leaving a detached session), client cancels, lease releases, time, router
//! listings (fresh, or read earlier and answered late), loading and sleeping models, a small model
//! detected from its arguments, unloads that may get stuck or be refused, failed listings, an
//! unreachable router, preset reloads and admin unloads.
//!
//! The synthetic engine keeps its own state (which models it runs), and the harness keeps its own
//! record of the requests and leases it was given. Invariants after every step:
//! - an unload is asked only for a model on which the harness holds no request and no lease, and
//!   (except a repeated ask for a stuck unload) only while another model waits;
//! - the engine never runs two chat models at once: a chat model is only ever started on an engine
//!   that runs no other chat model (exempt and small models aside);
//! - without an admin force, at most one chat model has work, and it is the holder;
//! - nothing without a lease is admitted, and nothing is granted, while a preset reload runs;
//! - a request that cannot load is passed on only when its model is not the holder;
//! - a waiter is answered (granted or refused) by the first poll at or after its deadline;
//! - a group is granted only from the front of the queue, and nothing while an unload is stuck.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use inference_admission::{
    Action, Admission, Arrival, Config, Grant, LeaseKey, Listing, ModelRow, RowStatus, Scheduler,
    Ticket, UnloadOutcome, WaitKind, WaiterKey,
};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

const MODELS: [&str; 5] = [
    "route-code",
    "route-smart",
    "route-creative",
    "embed",
    "rerank",
];
/// Names clients use: every id, plus an alias of `route-code`.
const NAMES: [&str; 6] = [
    "route-code",
    "route-smart",
    "route-creative",
    "embed",
    "rerank",
    "code-alias",
];
const EMBED: &str = "embed";
/// Detected as small from its router arguments, not configured.
const RERANK: &str = "rerank";

fn cfg() -> Config {
    Config {
        exempt: [EMBED.to_owned()].into_iter().collect(),
        max_waiters: 6,
        max_leases: 3,
        max_wait_ms: 4_000,
        fair_hold_ms: 700,
        residency_grace_ms: 150,
        lease_idle_ms: 400,
        lease_hard_ms: 3_000,
        unload_timeout_ms: 1_500,
        router_down_grace_ms: 500,
        session_hold_ms: 900,
    }
}

#[derive(Clone, Debug)]
enum Op {
    Arrive {
        name: usize,
        lease_request: bool,
        use_lease: bool,
        max_wait: u64,
    },
    ArriveNoAutoload {
        name: usize,
        use_lease: bool,
    },
    Attach(Option<usize>),
    Finish(usize),
    FinishDetached(usize),
    Cancel(usize),
    Release(usize),
    Advance(u64),
    Observe,
    /// A listing read starts now; its answer comes later (`ApplyRead`).
    StartRead,
    ApplyRead(usize),
    ListingFailed,
    RouterDown,
    SetStuck(bool),
    Sleep(usize),
    UnloadReply(u8),
    AdminUnload {
        name: usize,
        force: bool,
    },
    BeginReload(bool),
    EndReload,
}

fn op(allow_force: bool) -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0..6usize, any::<bool>(), any::<bool>(), 1..6_000u64).prop_map(|(name, lease_request, use_lease, max_wait)| Op::Arrive { name, lease_request, use_lease, max_wait }),
        1 => (0..6usize, any::<bool>()).prop_map(|(name, use_lease)| Op::ArriveNoAutoload { name, use_lease }),
        1 => prop::option::of(0..6usize).prop_map(Op::Attach),
        4 => (0..64usize).prop_map(Op::Finish),
        1 => (0..64usize).prop_map(Op::FinishDetached),
        1 => (0..64usize).prop_map(Op::Cancel),
        2 => (0..64usize).prop_map(Op::Release),
        5 => (0..1_200u64).prop_map(Op::Advance),
        2 => Just(Op::Observe),
        3 => Just(Op::StartRead),
        3 => (0..8usize).prop_map(Op::ApplyRead),
        1 => Just(Op::ListingFailed),
        1 => Just(Op::RouterDown),
        1 => any::<bool>().prop_map(Op::SetStuck),
        1 => (0..5usize).prop_map(Op::Sleep),
        1 => (0..4u8).prop_map(Op::UnloadReply),
        1 => (0..6usize, any::<bool>()).prop_map(move |(name, force)| Op::AdminUnload { name, force: force && allow_force }),
        1 => any::<bool>().prop_map(move |force| Op::BeginReload(force && allow_force)),
        1 => Just(Op::EndReload),
    ]
}

/// A synthetic engine: the models it runs and their state, and which are stopping.
#[derive(Default)]
struct Engine {
    running: BTreeMap<String, RowStatus>,
    stopping: BTreeSet<String>,
    stuck: bool,
}

impl Engine {
    /// The listing as read now (a read also lets time pass for the engine: stops finish unless
    /// stuck, loads finish).
    fn listing(&mut self) -> Listing {
        if !self.stuck {
            for m in std::mem::take(&mut self.stopping) {
                self.running.remove(&m);
            }
        }
        for status in self.running.values_mut() {
            if *status == RowStatus::Loading {
                *status = RowStatus::Loaded;
            }
        }
        Listing {
            rows: MODELS
                .iter()
                .map(|m| ModelRow {
                    id: (*m).to_owned(),
                    status: self.running.get(*m).copied().unwrap_or(RowStatus::Unloaded),
                    small: *m == RERANK,
                    aliases: if *m == "route-code" {
                        vec!["code-alias".to_owned()]
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
        }
    }

    fn chat_models_running(&self) -> BTreeSet<String> {
        self.running
            .keys()
            .filter(|m| m.as_str() != EMBED && m.as_str() != RERANK)
            .cloned()
            .collect()
    }
}

fn canonical(name: &str) -> &str {
    if name == "code-alias" {
        "route-code"
    } else {
        name
    }
}

struct Harness {
    s: Scheduler,
    engine: Engine,
    now: u64,
    tickets: Vec<Ticket>,
    leases: Vec<(LeaseKey, String)>,
    waiters: BTreeMap<WaiterKey, (u64, String)>,
    reads: Vec<(u64, Listing)>,
    forced: bool,
    last_unload: Option<String>,
    violation: Option<String>,
}

impl Harness {
    fn new() -> Self {
        let mut h = Self {
            s: Scheduler::new(cfg()),
            engine: Engine::default(),
            now: 0,
            tickets: Vec::new(),
            leases: Vec::new(),
            waiters: BTreeMap::new(),
            reads: Vec::new(),
            forced: false,
            last_unload: None,
            violation: None,
        };
        let listing = h.engine.listing();
        h.s.observe(listing);
        h
    }

    fn exempt(model: &str) -> bool {
        model == EMBED || model == RERANK
    }

    /// The engine starts (or wakes) `model` for a request that may load it.
    fn engine_serves(&mut self, model: &str, may_load: bool) {
        let model = canonical(model).to_owned();
        match self.engine.running.get(&model).copied() {
            Some(RowStatus::Sleeping) => {
                self.engine.running.insert(model, RowStatus::Loaded);
            }
            Some(_) => {}
            None if may_load => {
                if !Self::exempt(&model) {
                    let others: Vec<String> = self
                        .engine
                        .chat_models_running()
                        .into_iter()
                        .filter(|m| *m != model)
                        .collect();
                    if !others.is_empty() && self.violation.is_none() {
                        self.violation =
                            Some(format!("{model} started while the engine runs {others:?}"));
                    }
                }
                self.engine.running.insert(model, RowStatus::Loading);
            }
            None => {}
        }
    }

    fn lease_for(&self, name: &str) -> Option<LeaseKey> {
        let model = canonical(name);
        self.leases
            .iter()
            .find(|(_, m)| m == model)
            .map(|(k, _)| *k)
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::Arrive {
                name,
                lease_request,
                use_lease,
                max_wait,
            } => {
                let name = NAMES[*name];
                let lease = if *use_lease && !*lease_request {
                    self.lease_for(name)
                } else {
                    None
                };
                let kind = if *lease_request {
                    WaitKind::Lease
                } else {
                    WaitKind::Request
                };
                let reloading = self.s.in_maintenance();
                match self.s.arrive(self.now, name, kind, lease, *max_wait) {
                    Arrival::Admitted(Admission::Request(t)) => {
                        if reloading && lease.is_none() && self.violation.is_none() {
                            self.violation = Some(format!("{name} admitted during a reload"));
                        }
                        self.engine_serves(&t.model, true);
                        self.tickets.push(t);
                    }
                    Arrival::Admitted(Admission::Lease(k)) => {
                        if reloading && self.violation.is_none() {
                            self.violation = Some(format!("lease on {name} during a reload"));
                        }
                        let model = self.s.lease_model(k).unwrap().to_owned();
                        self.leases.push((k, model));
                    }
                    Arrival::Queued { waiter, .. } => {
                        let deadline = self.now + (*max_wait).clamp(1, cfg().max_wait_ms);
                        self.waiters
                            .insert(waiter, (deadline, canonical(name).to_owned()));
                    }
                    Arrival::Refused(_) => {}
                    Arrival::Pass => {
                        if self.violation.is_none() {
                            self.violation = Some("a request that may load was passed on".into());
                        }
                    }
                }
            }
            Op::ArriveNoAutoload { name, use_lease } => {
                let name = NAMES[*name];
                let lease = if *use_lease {
                    self.lease_for(name)
                } else {
                    None
                };
                let holder = self.s.holder().map(str::to_owned);
                match self.s.arrive_without_autoload(self.now, name, lease, 4_000) {
                    Arrival::Admitted(Admission::Request(t)) => {
                        self.engine_serves(&t.model, false);
                        self.tickets.push(t);
                    }
                    Arrival::Pass => {
                        if holder.as_deref() == Some(canonical(name)) && self.violation.is_none() {
                            self.violation =
                                Some(format!("{name} passed on although it holds the slot"));
                        }
                    }
                    Arrival::Queued { waiter, .. } => {
                        if lease.is_none() && self.violation.is_none() {
                            self.violation = Some(format!("{name} queued although it cannot load"));
                        }
                        self.waiters
                            .insert(waiter, (self.now + 4_000, canonical(name).to_owned()));
                    }
                    Arrival::Admitted(Admission::Lease(_)) => {
                        self.violation = Some("a lease from a plain request".into());
                    }
                    Arrival::Refused(_) => {}
                }
            }
            Op::Attach(name) => {
                if let Some(t) = self.s.attach(name.map(|i| NAMES[i])) {
                    self.tickets.push(t);
                }
            }
            Op::Finish(i) => {
                if !self.tickets.is_empty() {
                    let t = self.tickets.remove(i % self.tickets.len());
                    self.s.finish(self.now, &t);
                }
            }
            Op::FinishDetached(i) => {
                if !self.tickets.is_empty() {
                    let t = self.tickets.remove(i % self.tickets.len());
                    self.s.finish(self.now, &t);
                    self.s
                        .hold_session(self.now, &format!("conv-{i}"), &t.model);
                }
            }
            Op::Cancel(i) => {
                if !self.waiters.is_empty() {
                    let key = *self.waiters.keys().nth(i % self.waiters.len()).unwrap();
                    self.waiters.remove(&key);
                    self.s.cancel(key);
                }
            }
            Op::Release(i) => {
                if !self.leases.is_empty() {
                    let (k, _) = self.leases.remove(i % self.leases.len());
                    self.s.release_lease(k);
                }
            }
            Op::Advance(ms) => self.now += ms,
            Op::Observe => {
                let listing = self.engine.listing();
                self.s.observe(listing);
            }
            Op::StartRead => {
                let ticket = self.s.listing_requested();
                let listing = self.engine.listing();
                self.reads.push((ticket, listing));
            }
            Op::ApplyRead(i) => {
                if !self.reads.is_empty() {
                    let (ticket, listing) = self.reads.remove(i % self.reads.len());
                    self.s.observe_at(ticket, listing);
                }
            }
            Op::ListingFailed => self.s.listing_failed(self.now),
            Op::RouterDown => self.s.router_unreachable(self.now),
            Op::SetStuck(stuck) => self.engine.stuck = *stuck,
            Op::Sleep(i) => {
                let model = MODELS[*i];
                if self.engine.running.get(model) == Some(&RowStatus::Loaded)
                    && !self.tickets.iter().any(|t| t.model == model)
                {
                    self.engine
                        .running
                        .insert(model.to_owned(), RowStatus::Sleeping);
                }
            }
            Op::UnloadReply(code) => {
                if let Some(model) = self.last_unload.take() {
                    let outcome = match code {
                        0 => UnloadOutcome::Accepted,
                        1 => UnloadOutcome::NotRunning,
                        2 => UnloadOutcome::Refused,
                        _ => UnloadOutcome::Failed,
                    };
                    match outcome {
                        UnloadOutcome::NotRunning => {
                            self.engine.running.remove(&model);
                            self.engine.stopping.remove(&model);
                        }
                        UnloadOutcome::Refused | UnloadOutcome::Failed => {
                            self.engine.stopping.remove(&model);
                        }
                        UnloadOutcome::Accepted => {}
                    }
                    self.s.unload_result(self.now, &model, outcome);
                }
            }
            Op::AdminUnload { name, force } => {
                let name = NAMES[*name];
                if self.s.admin_unload(self.now, name, *force).is_ok() {
                    let model = canonical(name).to_owned();
                    if *force {
                        self.forced = true;
                    }
                    if self.engine.running.contains_key(&model) {
                        self.engine.stopping.insert(model.clone());
                    }
                    self.last_unload = Some(model);
                }
            }
            Op::BeginReload(force) => {
                if self.s.begin_maintenance(self.now, *force).is_ok() && *force {
                    self.forced = true;
                }
            }
            Op::EndReload => self.s.end_maintenance(),
        }
    }

    /// Polls and checks every invariant.
    fn poll_and_check(&mut self) -> Result<(), TestCaseError> {
        if let Some(violation) = self.violation.take() {
            return Err(TestCaseError::fail(violation));
        }
        let queue_before = self.s.queued_models();
        let waiters_before = self.waiters.clone();
        let stuck_before = self.s.stuck();
        let reloading = self.s.in_maintenance();
        let actions = self.s.poll(self.now);
        let rejected: BTreeSet<WaiterKey> = actions
            .iter()
            .filter_map(|a| match a {
                Action::Rejected { waiter, .. } => Some(*waiter),
                _ => None,
            })
            .collect();
        // The front group once this poll's refusals are taken out.
        let effective_front = queue_before.iter().find(|model| {
            waiters_before
                .iter()
                .any(|(k, (_, m))| m == *model && !rejected.contains(k))
        });
        let mut granted_models = BTreeSet::new();
        for action in &actions {
            match action {
                Action::Unload { model } => {
                    let requests = self.tickets.iter().filter(|t| t.model == *model).count();
                    let leases = self.leases.iter().filter(|(_, m)| m == model).count();
                    prop_assert_eq!(requests, 0, "unload of {} with a request in flight", model);
                    prop_assert_eq!(leases, 0, "unload of {} with a live lease", model);
                    if !stuck_before {
                        prop_assert!(effective_front.is_some(), "unload while nothing waits");
                        prop_assert!(
                            effective_front != Some(model),
                            "unload of the model the front waits for"
                        );
                    }
                    if self.engine.running.contains_key(model) {
                        self.engine.stopping.insert(model.clone());
                    }
                    self.last_unload = Some(model.clone());
                }
                Action::Granted { waiter, grant, .. } => {
                    prop_assert!(!self.s.stuck(), "granted while stuck");
                    prop_assert!(!reloading, "granted during a reload");
                    self.waiters.remove(waiter);
                    match grant {
                        Grant::Request(t) => {
                            granted_models.insert(t.model.clone());
                            let model = t.model.clone();
                            self.engine_serves(&model, true);
                            self.tickets.push(t.clone());
                        }
                        Grant::Lease(k) => {
                            let model = self.s.lease_model(*k).unwrap().to_owned();
                            granted_models.insert(model.clone());
                            self.leases.push((*k, model));
                        }
                    }
                }
                Action::Rejected { waiter, .. } => {
                    self.waiters.remove(waiter);
                }
                Action::LeaseEnded { lease, .. } => {
                    self.leases.retain(|(k, _)| k != lease);
                }
            }
        }
        if let Some(violation) = self.violation.take() {
            return Err(TestCaseError::fail(violation));
        }
        prop_assert!(granted_models.len() <= 1, "one group per poll");
        if let Some(model) = granted_models.iter().next() {
            prop_assert_eq!(effective_front, Some(model), "granted from the front only");
        }
        for (waiter, (deadline, _)) in &self.waiters {
            prop_assert!(
                *deadline > self.now,
                "waiter {:?} not answered at its deadline",
                waiter
            );
        }
        if !self.forced {
            let mut busy: BTreeSet<&str> = self
                .tickets
                .iter()
                .map(|t| t.model.as_str())
                .chain(self.leases.iter().map(|(_, m)| m.as_str()))
                .collect();
            busy.retain(|m| !Self::exempt(m));
            prop_assert!(busy.len() <= 1, "two chat models busy: {:?}", busy);
            if let Some(model) = busy.iter().next() {
                prop_assert_eq!(
                    self.s.holder(),
                    Some(*model),
                    "the busy model holds the slot"
                );
            }
        }
        Ok(())
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    #[test]
    fn invariants_hold_without_admin_force(ops in prop::collection::vec(op(false), 1..160)) {
        let mut h = Harness::new();
        for op in &ops {
            h.apply(op);
            h.poll_and_check()?;
        }
    }

    #[test]
    fn invariants_hold_with_admin_force(ops in prop::collection::vec(op(true), 1..160)) {
        let mut h = Harness::new();
        for op in &ops {
            h.apply(op);
            h.poll_and_check()?;
        }
    }

    /// Whatever happened, once every client is done, the reload has ended and time passes, the
    /// queue drains: each waiter is answered and nothing stays in flight.
    #[test]
    fn everything_is_answered_once_clients_finish(ops in prop::collection::vec(op(false), 1..120)) {
        let mut h = Harness::new();
        for op in &ops {
            h.apply(op);
            h.poll_and_check()?;
        }
        h.engine.stuck = false;
        h.s.end_maintenance();
        for _ in 0..40 {
            while let Some(t) = h.tickets.pop() {
                h.s.finish(h.now, &t);
            }
            while let Some((k, _)) = h.leases.pop() {
                h.s.release_lease(k);
            }
            h.now += 500;
            let listing = h.engine.listing();
            h.s.observe(listing);
            h.poll_and_check()?;
        }
        prop_assert!(h.waiters.is_empty(), "waiters left: {:?}", h.waiters);
        prop_assert_eq!(h.s.waiting(), 0);
    }
}
