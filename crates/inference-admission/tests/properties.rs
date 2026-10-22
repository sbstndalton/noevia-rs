//! Properties of the admission scheduler over random event sequences (sbstndalton/noevia#2115):
//! arrivals, finishes, client cancels, lease releases, time, router listings (with unloads that may
//! get stuck), an unreachable router and admin unloads.
//!
//! Invariants after every step:
//! - an unload is asked only for a model with nothing in flight and no live lease, only while
//!   another model waits, and never while the queue is empty;
//! - without an admin force, at most one chat model has work (requests in flight or live leases),
//!   and it is the holder;
//! - a waiter is answered (granted or refused) by the first poll at or after its deadline;
//! - a group is granted only from the front of the queue;
//! - nothing is granted while an unload is stuck.
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

const MODELS: [&str; 4] = ["route-code", "route-smart", "route-creative", "embed"];
const EMBED: &str = "embed";

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
    }
}

#[derive(Clone, Debug)]
enum Op {
    Arrive {
        model: usize,
        lease_request: bool,
        use_lease: bool,
        max_wait: u64,
    },
    Finish(usize),
    Cancel(usize),
    Release(usize),
    Advance(u64),
    Observe,
    RouterDown,
    SetStuck(bool),
    UnloadReply(u8),
    AdminUnload {
        model: usize,
        force: bool,
    },
}

fn op(allow_force: bool) -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (0..4usize, any::<bool>(), any::<bool>(), 1..6_000u64).prop_map(|(model, lease_request, use_lease, max_wait)| Op::Arrive { model, lease_request, use_lease, max_wait }),
        4 => (0..64usize).prop_map(Op::Finish),
        1 => (0..64usize).prop_map(Op::Cancel),
        2 => (0..64usize).prop_map(Op::Release),
        5 => (0..1_200u64).prop_map(Op::Advance),
        4 => Just(Op::Observe),
        1 => Just(Op::RouterDown),
        1 => any::<bool>().prop_map(Op::SetStuck),
        1 => (0..3u8).prop_map(Op::UnloadReply),
        1 => (0..4usize, any::<bool>()).prop_map(move |(model, force)| Op::AdminUnload { model, force: force && allow_force }),
    ]
}

/// A synthetic engine: which models are resident, and which are stopping.
#[derive(Default)]
struct Engine {
    resident: BTreeSet<String>,
    stopping: BTreeSet<String>,
    stuck: bool,
}

impl Engine {
    fn listing(&mut self) -> Listing {
        if !self.stuck {
            for m in std::mem::take(&mut self.stopping) {
                self.resident.remove(&m);
            }
        }
        Listing {
            rows: MODELS
                .iter()
                .map(|m| ModelRow {
                    id: (*m).to_owned(),
                    status: if self.resident.contains(*m) {
                        RowStatus::Loaded
                    } else {
                        RowStatus::Unloaded
                    },
                    small: false,
                })
                .collect(),
        }
    }
}

struct Harness {
    s: Scheduler,
    engine: Engine,
    now: u64,
    tickets: Vec<Ticket>,
    leases: Vec<(LeaseKey, String)>,
    waiters: BTreeMap<WaiterKey, (u64, String)>,
    forced: bool,
    last_unload: Option<String>,
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
            forced: false,
            last_unload: None,
        };
        let listing = h.engine.listing();
        h.s.observe(listing);
        h
    }

    fn admitted_model(&mut self, model: &str) {
        if !self.s.exempt(model) {
            self.engine.resident.insert(model.to_owned());
        }
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::Arrive {
                model,
                lease_request,
                use_lease,
                max_wait,
            } => {
                let model = MODELS[*model];
                let lease = if *use_lease && !*lease_request {
                    self.leases
                        .iter()
                        .find(|(_, m)| m == model)
                        .map(|(k, _)| *k)
                } else {
                    None
                };
                let kind = if *lease_request {
                    WaitKind::Lease
                } else {
                    WaitKind::Request
                };
                match self.s.arrive(self.now, model, kind, lease, *max_wait) {
                    Arrival::Admitted(Admission::Request(t)) => {
                        self.admitted_model(model);
                        self.tickets.push(t);
                    }
                    Arrival::Admitted(Admission::Lease(k)) => {
                        self.leases.push((k, model.to_owned()))
                    }
                    Arrival::Queued { waiter, .. } => {
                        let deadline = self.now + (*max_wait).clamp(1, cfg().max_wait_ms);
                        self.waiters.insert(waiter, (deadline, model.to_owned()));
                    }
                    Arrival::Refused(_) => {}
                }
            }
            Op::Finish(i) => {
                if !self.tickets.is_empty() {
                    let t = self.tickets.remove(i % self.tickets.len());
                    self.s.finish(self.now, &t);
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
            Op::RouterDown => self.s.router_unreachable(self.now),
            Op::SetStuck(stuck) => self.engine.stuck = *stuck,
            Op::UnloadReply(code) => {
                if let Some(model) = self.last_unload.take() {
                    let outcome = match code {
                        0 => UnloadOutcome::Accepted,
                        1 => UnloadOutcome::NotRunning,
                        _ => UnloadOutcome::Failed,
                    };
                    match outcome {
                        UnloadOutcome::NotRunning => {
                            self.engine.resident.remove(&model);
                            self.engine.stopping.remove(&model);
                        }
                        UnloadOutcome::Failed => {
                            self.engine.stopping.remove(&model);
                        }
                        UnloadOutcome::Accepted => {}
                    }
                    self.s.unload_result(self.now, &model, outcome);
                }
            }
            Op::AdminUnload { model, force } => {
                let model = MODELS[*model];
                if self.s.admin_unload(self.now, model, *force).is_ok() {
                    if *force {
                        self.forced = true;
                    }
                    self.engine.stopping.insert(model.to_owned());
                    self.last_unload = Some(model.to_owned());
                }
            }
        }
    }

    /// Polls and checks every invariant.
    fn poll_and_check(&mut self) -> Result<(), TestCaseError> {
        let queue_before = self.s.queued_models();
        let waiters_before = self.waiters.clone();
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
                    prop_assert_eq!(
                        self.s.in_flight(model),
                        0,
                        "unload with a request in flight"
                    );
                    prop_assert_eq!(self.s.live_leases(model), 0, "unload with a live lease");
                    prop_assert!(effective_front.is_some(), "unload while nothing waits");
                    prop_assert!(
                        effective_front != Some(model),
                        "unload of the model the front waits for"
                    );
                    self.engine.stopping.insert(model.clone());
                    self.last_unload = Some(model.clone());
                }
                Action::Granted { waiter, grant, .. } => {
                    prop_assert!(!self.s.stuck(), "granted while stuck");
                    self.waiters.remove(waiter);
                    match grant {
                        Grant::Request(t) => {
                            granted_models.insert(t.model.clone());
                            let model = t.model.clone();
                            self.admitted_model(&model);
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
            let busy: BTreeSet<&str> = MODELS
                .iter()
                .copied()
                .filter(|m| !self.s.exempt(m))
                .filter(|m| self.s.in_flight(m) > 0 || self.s.live_leases(m) > 0)
                .collect();
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

    /// Whatever happened, once every client is done and time passes, the queue drains: each waiter
    /// is answered and nothing stays in flight.
    #[test]
    fn everything_is_answered_once_clients_finish(ops in prop::collection::vec(op(false), 1..120)) {
        let mut h = Harness::new();
        for op in &ops {
            h.apply(op);
            h.poll_and_check()?;
        }
        h.engine.stuck = false;
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
