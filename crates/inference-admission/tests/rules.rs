//! The scheduler's rules, one case each (sbstndalton/noevia#2115). Model ids are synthetic route
//! names: `route-code` and `route-smart` are two chat models that cannot share the engine,
//! `embed` is a small exempt model.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use inference_admission::{
    Action, Admission, Arrival, Config, Grant, LeaseEnd, LeaseKey, Listing, ModelRow, Refusal,
    RowStatus, Scheduler, Ticket, UnloadOutcome, WaitKind, WaiterKey,
};

const CODE: &str = "route-code";
const SMART: &str = "route-smart";
const EMBED: &str = "embed";

fn cfg() -> Config {
    Config {
        exempt: [EMBED.to_owned()].into_iter().collect(),
        max_waiters: 8,
        max_leases: 4,
        max_wait_ms: 10_000,
        fair_hold_ms: 1_000,
        residency_grace_ms: 200,
        lease_idle_ms: 500,
        lease_hard_ms: 5_000,
        unload_timeout_ms: 2_000,
        router_down_grace_ms: 300,
        session_hold_ms: 3_000,
    }
}

fn listing(rows: &[(&str, RowStatus)]) -> Listing {
    Listing {
        rows: rows
            .iter()
            .map(|(id, status)| ModelRow {
                id: (*id).to_owned(),
                status: *status,
                small: false,
                aliases: Vec::new(),
            })
            .collect(),
    }
}

fn admitted(arrival: Arrival) -> Ticket {
    match arrival {
        Arrival::Admitted(Admission::Request(ticket)) => ticket,
        other => panic!("expected an admitted request, got {other:?}"),
    }
}

fn lease(arrival: Arrival) -> LeaseKey {
    match arrival {
        Arrival::Admitted(Admission::Lease(key)) => key,
        other => panic!("expected an admitted lease, got {other:?}"),
    }
}

fn queued(arrival: Arrival) -> WaiterKey {
    match arrival {
        Arrival::Queued { waiter, .. } => waiter,
        other => panic!("expected a queued waiter, got {other:?}"),
    }
}

fn unloads(actions: &[Action]) -> Vec<String> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Unload { model } => Some(model.clone()),
            _ => None,
        })
        .collect()
}

fn granted(actions: &[Action]) -> Vec<(WaiterKey, Grant)> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Granted { waiter, grant, .. } => Some((*waiter, grant.clone())),
            _ => None,
        })
        .collect()
}

fn rejected(actions: &[Action]) -> Vec<(WaiterKey, Refusal)> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Rejected {
                waiter, refusal, ..
            } => Some((*waiter, *refusal)),
            _ => None,
        })
        .collect()
}

/// A scheduler whose engine has `model` loaded (and nothing else).
fn with_loaded(model: &str) -> Scheduler {
    let mut s = Scheduler::new(cfg());
    s.observe(listing(&[(model, RowStatus::Loaded)]));
    s
}

#[test]
fn an_empty_engine_admits_at_once_and_the_model_takes_the_slot() {
    let mut s = Scheduler::new(cfg());
    s.observe(listing(&[]));
    let t = admitted(s.arrive(0, CODE, WaitKind::Request, None, 1000));
    assert_eq!(s.holder(), Some(CODE));
    assert_eq!(s.in_flight(CODE), 1);
    s.finish(10, &t);
    assert_eq!(s.in_flight(CODE), 0);
}

#[test]
fn the_holder_takes_more_work_while_nothing_else_waits() {
    let mut s = with_loaded(CODE);
    admitted(s.arrive(0, CODE, WaitKind::Request, None, 1000));
    admitted(s.arrive(1, CODE, WaitKind::Request, None, 1000));
    assert_eq!(s.in_flight(CODE), 2);
}

#[test]
fn another_model_waits_while_the_holder_streams_and_is_never_unloaded_mid_request() {
    let mut s = with_loaded(CODE);
    let a = admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    let b = queued(s.arrive(10, SMART, WaitKind::Request, None, 10_000));
    for now in (20..3_000).step_by(100) {
        let actions = s.poll(now);
        assert!(unloads(&actions).is_empty(), "no unload while in flight");
        assert!(granted(&actions).is_empty());
    }
    s.finish(3_000, &a);
    // The residency grace of a request without a lease still holds the model briefly.
    assert!(unloads(&s.poll(3_100)).is_empty());
    let actions = s.poll(3_200);
    assert_eq!(unloads(&actions), vec![CODE.to_owned()]);
    assert!(s.switching());
    // Granted only once the router lists the model gone.
    assert!(granted(&s.poll(3_300)).is_empty());
    s.observe(listing(&[(CODE, RowStatus::Unloaded)]));
    let actions = s.poll(3_400);
    let g = granted(&actions);
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].0, b);
    assert_eq!(s.holder(), Some(SMART));
}

#[test]
fn a_lease_holds_the_model_between_rounds_and_its_release_frees_it_at_once() {
    let mut s = with_loaded(CODE);
    let l = lease(s.arrive(0, CODE, WaitKind::Lease, None, 1000));
    let r1 = admitted(s.arrive(1, CODE, WaitKind::Request, Some(l), 1000));
    let w = queued(s.arrive(2, SMART, WaitKind::Request, None, 10_000));
    s.finish(100, &r1);
    // Between rounds: nothing in flight, but the lease is live.
    assert!(unloads(&s.poll(400)).is_empty());
    let r2 = admitted(s.arrive(450, CODE, WaitKind::Request, Some(l), 1000));
    s.finish(1_200, &r2);
    assert!(unloads(&s.poll(1_300)).is_empty());
    // A round after the fair hold still runs: the lease's turn continues.
    let r3 = admitted(s.arrive(1_350, CODE, WaitKind::Request, Some(l), 1000));
    s.finish(1_400, &r3);
    // A new request without the lease does not: it queues behind the waiting group.
    queued(s.arrive(1_401, CODE, WaitKind::Request, None, 10_000));
    assert!(s.release_lease(l));
    // No residency grace after a lease: the switch starts at once.
    assert_eq!(unloads(&s.poll(1_402)), vec![CODE.to_owned()]);
    s.unload_result(1_403, CODE, UnloadOutcome::NotRunning);
    let g = granted(&s.poll(1_404));
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].0, w);
    assert_eq!(s.holder(), Some(SMART));
}

#[test]
fn a_new_request_for_the_holder_rides_along_only_until_the_fair_hold() {
    let mut s = with_loaded(CODE);
    let a = admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    queued(s.arrive(100, SMART, WaitKind::Request, None, 10_000));
    // Within the fair hold, the busy holder still takes new work.
    admitted(s.arrive(500, CODE, WaitKind::Request, None, 10_000));
    // After it, new holder work queues behind the waiting group.
    let late = queued(s.arrive(1_200, CODE, WaitKind::Request, None, 10_000));
    assert_eq!(s.position(late), Some(2));
    assert_eq!(s.queued_models(), vec![SMART.to_owned(), CODE.to_owned()]);
    let _ = a;
}

#[test]
fn a_client_without_a_lease_keeps_its_model_between_calls_within_the_grace() {
    // The multi-call pattern of a client that takes no lease: call, short gap, next call.
    let mut s = with_loaded(CODE);
    let a = admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    let w = queued(s.arrive(10, SMART, WaitKind::Request, None, 10_000));
    s.finish(20, &a);
    // Its next call within the residency grace rides along: no switch between its calls.
    let b = admitted(s.arrive(150, CODE, WaitKind::Request, None, 10_000));
    assert!(unloads(&s.poll(160)).is_empty());
    s.finish(300, &b);
    let c = admitted(s.arrive(450, CODE, WaitKind::Request, None, 10_000));
    s.finish(500, &c);
    // A call after the grace has run out queues behind the waiting model.
    assert_eq!(unloads(&s.poll(700)), vec![CODE.to_owned()]);
    queued(s.arrive(701, CODE, WaitKind::Request, None, 10_000));
    s.observe(listing(&[(CODE, RowStatus::Unloaded)]));
    assert_eq!(granted(&s.poll(702))[0].0, w);
}

#[test]
fn the_grace_never_extends_the_holder_past_the_fair_hold() {
    let mut s = with_loaded(CODE);
    let first = admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    queued(s.arrive(1, SMART, WaitKind::Request, None, 10_000));
    s.finish(50, &first);
    let mut now = 100;
    // A client calling back to back within the grace keeps the model until the fair hold.
    while now < 1_000 {
        let t = admitted(s.arrive(now, CODE, WaitKind::Request, None, 10_000));
        s.finish(now + 50, &t);
        assert!(unloads(&s.poll(now + 60)).is_empty());
        now += 100;
    }
    let late = queued(s.arrive(1_001, CODE, WaitKind::Request, None, 10_000));
    assert_eq!(s.position(late), Some(2));
    // The switch waits only for the grace of the last call.
    assert!(unloads(&s.poll(1_001)).is_empty());
    assert_eq!(unloads(&s.poll(1_150)), vec![CODE.to_owned()]);
}

#[test]
fn groups_are_served_in_arrival_order_each_in_full() {
    let mut s = with_loaded(CODE);
    let a = admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    let s1 = queued(s.arrive(10, SMART, WaitKind::Request, None, 10_000));
    let s2 = queued(s.arrive(20, SMART, WaitKind::Request, None, 10_000));
    let c1 = queued(s.arrive(1_100, CODE, WaitKind::Request, None, 10_000));
    let s3 = queued(s.arrive(1_200, SMART, WaitKind::Request, None, 10_000));
    s.finish(1_300, &a);
    assert_eq!(unloads(&s.poll(1_600)), vec![CODE.to_owned()]);
    s.observe(listing(&[(CODE, RowStatus::Unloaded)]));
    let g = granted(&s.poll(1_700));
    let keys: Vec<WaiterKey> = g.iter().map(|(k, _)| *k).collect();
    assert_eq!(
        keys,
        vec![s1, s2, s3],
        "the whole route-smart group at once"
    );
    assert_eq!(s.position(c1), Some(1));
}

#[test]
fn a_sleeping_holder_is_unloaded_without_waiting_and_never_loaded_by_the_scheduler() {
    let mut s = Scheduler::new(cfg());
    s.observe(listing(&[(SMART, RowStatus::Sleeping)]));
    assert_eq!(s.holder(), Some(SMART));
    let w = queued(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    let actions = s.poll(1);
    assert_eq!(unloads(&actions), vec![SMART.to_owned()]);
    s.observe(listing(&[(SMART, RowStatus::Unloaded)]));
    let actions = s.poll(2);
    assert_eq!(granted(&actions)[0].0, w);
    // The scheduler has no load action at all: the admitted request loads its model.
    assert!(actions.iter().all(|a| !matches!(a, Action::Unload { .. })));
}

#[test]
fn a_request_for_a_sleeping_holder_runs_at_once() {
    let mut s = Scheduler::new(cfg());
    s.observe(listing(&[(SMART, RowStatus::Sleeping)]));
    admitted(s.arrive(0, SMART, WaitKind::Request, None, 1000));
}

#[test]
fn exempt_models_never_take_or_block_the_slot() {
    let mut s = with_loaded(CODE);
    let a = admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    queued(s.arrive(1, SMART, WaitKind::Request, None, 10_000));
    let e = admitted(s.arrive(2, EMBED, WaitKind::Request, None, 10_000));
    assert_eq!(s.holder(), Some(CODE));
    s.finish(3, &a);
    s.finish(4, &e);
    // A small model detected from the router's arguments is exempt too.
    let mut rows = listing(&[(CODE, RowStatus::Loaded)]);
    rows.rows.push(ModelRow {
        id: "rerank".into(),
        status: RowStatus::Loaded,
        small: true,
        aliases: Vec::new(),
    });
    s.observe(rows);
    assert!(s.exempt("rerank"));
    admitted(s.arrive(5, "rerank", WaitKind::Request, None, 10_000));
}

#[test]
fn a_waiter_is_refused_at_its_deadline() {
    let mut s = with_loaded(CODE);
    admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    let w = queued(s.arrive(0, SMART, WaitKind::Request, None, 500));
    assert!(rejected(&s.poll(499)).is_empty());
    assert_eq!(rejected(&s.poll(500)), vec![(w, Refusal::QueueTimeout)]);
    assert_eq!(s.waiting(), 0);
    assert!(s.queued_models().is_empty());
}

#[test]
fn a_client_wait_limit_is_capped_by_the_configured_maximum() {
    let mut s = with_loaded(CODE);
    admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    let w = queued(s.arrive(0, SMART, WaitKind::Request, None, u64::MAX));
    assert_eq!(rejected(&s.poll(10_000)), vec![(w, Refusal::QueueTimeout)]);
}

#[test]
fn the_queue_is_bounded() {
    let mut s = with_loaded(CODE);
    admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    for i in 0..8 {
        queued(s.arrive(i, SMART, WaitKind::Request, None, 10_000));
    }
    assert_eq!(
        s.arrive(9, SMART, WaitKind::Request, None, 10_000),
        Arrival::Refused(Refusal::QueueFull)
    );
}

#[test]
fn leases_are_bounded() {
    let mut s = Scheduler::new(cfg());
    s.observe(listing(&[]));
    for _ in 0..4 {
        lease(s.arrive(0, CODE, WaitKind::Lease, None, 1000));
    }
    assert_eq!(
        s.arrive(0, CODE, WaitKind::Lease, None, 1000),
        Arrival::Refused(Refusal::QueueFull)
    );
}

#[test]
fn a_cancelled_waiter_leaves_and_an_empty_group_loads_nothing() {
    let mut s = with_loaded(CODE);
    let a = admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    let w = queued(s.arrive(1, SMART, WaitKind::Request, None, 10_000));
    s.cancel(w);
    assert!(s.queued_models().is_empty());
    s.finish(2, &a);
    for now in [10, 500, 5_000] {
        assert!(s.poll(now).is_empty(), "nothing to switch for");
    }
    assert_eq!(s.holder(), Some(CODE));
}

#[test]
fn a_stuck_unload_lets_no_other_model_in_and_recovers_when_the_model_is_gone() {
    let mut s = with_loaded(CODE);
    let w = queued(s.arrive(0, SMART, WaitKind::Request, None, 10_000));
    // Idle holder with no grace (never used through the gate): unloaded at once.
    assert_eq!(unloads(&s.poll(0)), vec![CODE.to_owned()]);
    s.unload_result(1, CODE, UnloadOutcome::Accepted);
    s.observe(listing(&[(CODE, RowStatus::Loaded)]));
    assert!(granted(&s.poll(1_999)).is_empty());
    let actions = s.poll(2_000);
    assert_eq!(rejected(&actions), vec![(w, Refusal::SwitchStuck)]);
    assert!(s.stuck());
    assert_eq!(
        s.arrive(2_001, SMART, WaitKind::Request, None, 10_000),
        Arrival::Refused(Refusal::SwitchStuck)
    );
    s.observe(listing(&[(CODE, RowStatus::Unloaded)]));
    assert!(!s.stuck());
    s.poll(2_100);
    admitted(s.arrive(2_101, SMART, WaitKind::Request, None, 10_000));
}

#[test]
fn an_unreachable_router_refuses_new_work_and_drops_waiters_after_the_grace() {
    let mut s = with_loaded(CODE);
    admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    let w = queued(s.arrive(0, SMART, WaitKind::Request, None, 10_000));
    s.router_unreachable(100);
    assert_eq!(
        s.arrive(101, SMART, WaitKind::Request, None, 10_000),
        Arrival::Refused(Refusal::EngineUnavailable)
    );
    assert!(rejected(&s.poll(399)).is_empty());
    assert_eq!(
        rejected(&s.poll(400)),
        vec![(w, Refusal::EngineUnavailable)]
    );
    s.observe(listing(&[(CODE, RowStatus::Loaded)]));
    assert!(!s.router_down());
}

#[test]
fn a_failed_unload_marks_the_router_unreachable_and_keeps_the_holder() {
    let mut s = with_loaded(CODE);
    queued(s.arrive(0, SMART, WaitKind::Request, None, 10_000));
    assert_eq!(unloads(&s.poll(0)), vec![CODE.to_owned()]);
    s.unload_result(1, CODE, UnloadOutcome::Failed);
    assert!(s.router_down());
    assert!(!s.switching());
    assert_eq!(s.holder(), Some(CODE));
}

#[test]
fn an_explicit_unload_of_a_busy_model_is_refused_and_an_admin_force_is_counted() {
    let mut s = with_loaded(CODE);
    let l = lease(s.arrive(0, CODE, WaitKind::Lease, None, 1000));
    let t = admitted(s.arrive(1, CODE, WaitKind::Request, Some(l), 1000));
    assert_eq!(s.admin_unload(2, CODE, false), Err(Refusal::ModelInUse));
    s.finish(3, &t);
    // Between rounds the lease still protects it.
    assert_eq!(s.admin_unload(4, CODE, false), Err(Refusal::ModelInUse));
    assert_eq!(s.admin_unload(5, CODE, true), Ok(()));
    let actions = s.poll(6);
    assert!(actions.contains(&Action::LeaseEnded {
        lease: l,
        end: LeaseEnd::Forced
    }));
    assert_eq!(
        s.arrive(7, CODE, WaitKind::Request, Some(l), 1000),
        Arrival::Refused(Refusal::LeaseUnknown)
    );
    let status = s.status(8);
    assert_eq!(status.counters.unloads_forced, 1);
    assert_eq!(status.counters.unloads_refused, 2);
}

#[test]
fn an_explicit_unload_of_an_idle_model_holds_new_work_until_it_is_gone() {
    let mut s = with_loaded(CODE);
    assert_eq!(s.admin_unload(0, CODE, false), Ok(()));
    let w = queued(s.arrive(1, CODE, WaitKind::Request, None, 10_000));
    assert_eq!(s.admin_unload(2, SMART, false), Err(Refusal::EngineBusy));
    s.unload_result(3, CODE, UnloadOutcome::Accepted);
    s.observe(listing(&[(CODE, RowStatus::Unloaded)]));
    assert_eq!(granted(&s.poll(4))[0].0, w);
}

#[test]
fn leases_end_when_idle_and_at_their_hard_limit() {
    let mut s = Scheduler::new(cfg());
    s.observe(listing(&[]));
    let idle = lease(s.arrive(0, CODE, WaitKind::Lease, None, 1000));
    let busy = lease(s.arrive(0, CODE, WaitKind::Lease, None, 1000));
    let t = admitted(s.arrive(1, CODE, WaitKind::Request, Some(busy), 1000));
    let actions = s.poll(500);
    assert!(actions.contains(&Action::LeaseEnded {
        lease: idle,
        end: LeaseEnd::Idle
    }));
    // A request in flight keeps its lease from idling, but not past the hard limit.
    assert!(s.poll(4_999).is_empty());
    let actions = s.poll(5_000);
    assert!(actions.contains(&Action::LeaseEnded {
        lease: busy,
        end: LeaseEnd::HardLimit
    }));
    // Its request still counts until it ends.
    assert_eq!(s.in_flight(CODE), 1);
    s.finish(5_001, &t);
    assert_eq!(s.in_flight(CODE), 0);
}

#[test]
fn a_lease_for_another_model_or_an_unknown_lease_is_refused() {
    let mut s = Scheduler::new(cfg());
    s.observe(listing(&[]));
    let l = lease(s.arrive(0, CODE, WaitKind::Lease, None, 1000));
    assert_eq!(
        s.arrive(1, SMART, WaitKind::Request, Some(l), 1000),
        Arrival::Refused(Refusal::LeaseModelMismatch)
    );
    assert_eq!(
        s.arrive(1, CODE, WaitKind::Request, Some(LeaseKey(999)), 1000),
        Arrival::Refused(Refusal::LeaseUnknown)
    );
    assert!(!s.release_lease(LeaseKey(999)));
}

#[test]
fn a_lease_waiter_is_granted_a_lease_with_its_group() {
    let mut s = with_loaded(CODE);
    let a = admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    let w = queued(s.arrive(1, SMART, WaitKind::Lease, None, 10_000));
    s.finish(2, &a);
    assert_eq!(unloads(&s.poll(300)), vec![CODE.to_owned()]);
    s.observe(listing(&[(CODE, RowStatus::Unloaded)]));
    let g = granted(&s.poll(301));
    assert_eq!(g[0].0, w);
    let Grant::Lease(key) = g[0].1 else {
        panic!("a lease grant")
    };
    assert_eq!(s.lease_model(key), Some(SMART));
}

#[test]
fn preset_maintenance_needs_an_idle_engine_unless_forced() {
    let mut s = with_loaded(CODE);
    let t = admitted(s.arrive(0, CODE, WaitKind::Request, None, 1000));
    assert_eq!(s.begin_maintenance(1, false), Err(Refusal::EngineBusy));
    assert_eq!(s.begin_maintenance(1, true), Ok(()));
    // Only one reload at a time.
    assert_eq!(s.begin_maintenance(2, true), Err(Refusal::EngineBusy));
    s.end_maintenance();
    s.finish(3, &t);
    assert_eq!(s.begin_maintenance(4, false), Ok(()));
    s.end_maintenance();
    let l = lease(s.arrive(5, CODE, WaitKind::Lease, None, 1000));
    assert_eq!(s.begin_maintenance(6, false), Err(Refusal::EngineBusy));
    assert!(s.release_lease(l));
}

#[test]
fn nothing_new_is_admitted_while_a_preset_reload_runs() {
    let mut s = with_loaded(CODE);
    assert_eq!(s.begin_maintenance(0, false), Ok(()));
    assert!(s.in_maintenance());
    assert_eq!(s.status(0).switch, "maintenance");
    // Chat work waits for the reload to end; exempt work, which never queues, is refused.
    let w = queued(s.arrive(1, CODE, WaitKind::Request, None, 10_000));
    assert_eq!(
        s.arrive(2, EMBED, WaitKind::Request, None, 10_000),
        Arrival::Refused(Refusal::EngineBusy)
    );
    assert_eq!(s.admin_unload(3, CODE, true), Err(Refusal::EngineBusy));
    for now in [10, 1_000, 5_000] {
        assert!(
            granted(&s.poll(now)).is_empty(),
            "granted during the reload"
        );
    }
    s.end_maintenance();
    assert_eq!(granted(&s.poll(5_001))[0].0, w);
}

#[test]
fn an_unreported_maintenance_ends_by_itself_at_its_cap() {
    let mut s = with_loaded(CODE);
    assert_eq!(s.begin_maintenance(0, false), Ok(()));
    s.poll(599_999);
    assert!(s.in_maintenance());
    s.poll(600_000);
    assert!(!s.in_maintenance());
}

#[test]
fn aliases_name_their_model_everywhere() {
    let mut s = Scheduler::new(cfg());
    let mut rows = listing(&[(CODE, RowStatus::Loaded), (SMART, RowStatus::Unloaded)]);
    rows.rows[0].aliases = vec!["code-alias".into()];
    s.observe(rows);
    assert!(s.knows("code-alias"));
    assert!(!s.knows("no-such-model"));
    assert_eq!(s.canon("code-alias"), CODE);
    // A stream under the alias is the holder's own work: admitted, counted on the id.
    let t = admitted(s.arrive(0, "code-alias", WaitKind::Request, None, 10_000));
    assert_eq!(t.model, CODE);
    assert_eq!(s.in_flight(CODE), 1);
    assert_eq!(s.in_flight("code-alias"), 1);
    // Neither name unloads it while it streams.
    assert_eq!(s.admin_unload(1, CODE, false), Err(Refusal::ModelInUse));
    assert_eq!(
        s.admin_unload(1, "code-alias", false),
        Err(Refusal::ModelInUse)
    );
    // While another model waits, the alias does not queue behind its own model.
    queued(s.arrive(2, SMART, WaitKind::Request, None, 10_000));
    admitted(s.arrive(3, "code-alias", WaitKind::Request, None, 10_000));
    // A lease taken by the alias holds the id; a request by the id uses it.
    let l = lease(s.arrive(4, "code-alias", WaitKind::Lease, None, 1000));
    assert_eq!(s.lease_model(l), Some(CODE));
    admitted(s.arrive(5, CODE, WaitKind::Request, Some(l), 1000));
    assert!(unloads(&s.poll(10)).is_empty());
}

#[test]
fn an_alias_listed_as_exempt_exempts_its_model() {
    let mut s = Scheduler::new(Config {
        exempt: ["embed-alias".to_owned()].into_iter().collect(),
        ..cfg()
    });
    let mut rows = listing(&[(CODE, RowStatus::Loaded), ("embed-id", RowStatus::Loaded)]);
    rows.rows[1].aliases = vec!["embed-alias".into()];
    s.observe(rows);
    assert!(s.exempt("embed-id"));
    admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    admitted(s.arrive(1, "embed-id", WaitKind::Request, None, 10_000));
    assert_eq!(s.holder(), Some(CODE));
}

#[test]
fn a_request_that_cannot_load_never_waits_for_a_switch() {
    let mut s = Scheduler::new(cfg());
    s.observe(listing(&[
        (CODE, RowStatus::Loaded),
        (SMART, RowStatus::Unloaded),
    ]));
    admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    // Not resident and told not to load: passed to the router, nothing queued or unloaded.
    assert_eq!(
        s.arrive_without_autoload(1, SMART, None, 10_000),
        Arrival::Pass
    );
    assert!(s.queued_models().is_empty());
    assert!(unloads(&s.poll(5_000)).is_empty());
    // On the resident model it runs, and is counted.
    let t = admitted(s.arrive_without_autoload(2, CODE, None, 10_000));
    assert_eq!(s.in_flight(CODE), 2);
    s.finish(3, &t);
}

#[test]
fn a_stream_resume_counts_on_its_session_model() {
    let mut s = with_loaded(CODE);
    let resume = s.attach(Some(CODE)).expect("counted on the resident model");
    queued(s.arrive(0, SMART, WaitKind::Request, None, 10_000));
    for now in [10, 500, 2_000] {
        assert!(unloads(&s.poll(now)).is_empty(), "no unload under a resume");
    }
    s.finish(2_100, &resume);
    // Unknown session: counted on the holder. A model that is not resident: not counted.
    let on_holder = s.attach(None).expect("the holder");
    assert_eq!(on_holder.model, CODE);
    assert!(s.attach(Some(SMART)).is_none());
    s.finish(2_200, &on_holder);
    assert_eq!(unloads(&s.poll(2_500)), vec![CODE.to_owned()]);
    // Nothing is counted on a model being unloaded.
    assert!(s.attach(Some(CODE)).is_none());
}

#[test]
fn a_session_whose_client_left_holds_its_model_for_the_session_hold() {
    let mut s = with_loaded(CODE);
    let t = admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    queued(s.arrive(1, SMART, WaitKind::Request, None, 10_000));
    // Its client went away mid-answer: the router may still be generating for a resume.
    s.finish(100, &t);
    s.hold_session(100, "conv-synthetic", CODE);
    assert!(unloads(&s.poll(400)).is_empty(), "past the residency grace");
    assert!(unloads(&s.poll(3_099)).is_empty());
    assert_eq!(unloads(&s.poll(3_100)), vec![CODE.to_owned()]);
}

#[test]
fn one_slow_listing_does_not_mark_the_router_down_and_a_lease_round_still_runs() {
    let mut s = with_loaded(CODE);
    let l = lease(s.arrive(0, CODE, WaitKind::Lease, None, 1000));
    s.listing_failed(1);
    s.listing_failed(2);
    assert!(!s.router_down());
    s.listing_failed(4);
    assert!(s.router_down());
    // A live lease's round still goes out (its own forward fails if the router really is down).
    admitted(s.arrive(5, CODE, WaitKind::Request, Some(l), 1000));
    assert_eq!(
        s.arrive(6, CODE, WaitKind::Request, None, 1000),
        Arrival::Refused(Refusal::EngineUnavailable)
    );
    s.observe(listing(&[(CODE, RowStatus::Loaded)]));
    assert!(!s.router_down());
}

#[test]
fn a_refused_unload_keeps_the_router_up_and_retries_after_a_pause() {
    let mut s = with_loaded(CODE);
    let w = queued(s.arrive(0, SMART, WaitKind::Request, None, 10_000));
    assert_eq!(unloads(&s.poll(0)), vec![CODE.to_owned()]);
    s.unload_result(1, CODE, UnloadOutcome::Refused);
    assert!(!s.router_down());
    assert!(!s.switching());
    assert!(unloads(&s.poll(1_000)).is_empty(), "no retry storm");
    assert_eq!(unloads(&s.poll(2_001)), vec![CODE.to_owned()]);
    s.observe(listing(&[(CODE, RowStatus::Unloaded)]));
    assert_eq!(granted(&s.poll(2_002))[0].0, w);
}

#[test]
fn a_stuck_unload_is_asked_for_again_a_bounded_number_of_times() {
    let mut s = with_loaded(CODE);
    queued(s.arrive(0, SMART, WaitKind::Request, None, 100_000));
    assert_eq!(unloads(&s.poll(0)), vec![CODE.to_owned()]);
    // The first request may never have reached the router (its answer is never reported).
    s.poll(2_000);
    assert!(s.stuck());
    let mut asks = 0;
    let mut now = 2_000;
    for _ in 0..20 {
        now += 2_000;
        asks += unloads(&s.poll(now)).len();
    }
    assert_eq!(asks, 2, "two more asks: three in all");
    assert!(s.stuck());
    // The listing still ends it whenever the model goes.
    s.observe(listing(&[(CODE, RowStatus::Unloaded)]));
    assert!(!s.stuck());
}

#[test]
fn an_explicit_unload_of_an_exempt_or_absent_model_never_blocks_chat_work() {
    let mut s = Scheduler::new(cfg());
    s.observe(listing(&[
        (CODE, RowStatus::Loaded),
        (EMBED, RowStatus::Loaded),
        (SMART, RowStatus::Unloaded),
    ]));
    assert_eq!(s.admin_unload(0, EMBED, false), Ok(()));
    assert!(!s.switching());
    admitted(s.arrive(1, CODE, WaitKind::Request, None, 1000));
    assert_eq!(s.admin_unload(2, SMART, false), Ok(()));
    assert!(!s.switching());
    admitted(s.arrive(3, CODE, WaitKind::Request, None, 1000));
}

#[test]
fn a_listing_read_before_work_or_an_unload_is_stale_for_it() {
    let mut s = Scheduler::new(cfg());
    s.observe(listing(&[
        (CODE, RowStatus::Unloaded),
        (SMART, RowStatus::Unloaded),
    ]));
    // A read starts; meanwhile the first request loads CODE and ends.
    let early = s.listing_requested();
    let t = admitted(s.arrive(0, CODE, WaitKind::Request, None, 1000));
    s.finish(10, &t);
    // Its answer (CODE not yet loaded) arrives after the grace: the holder stays.
    s.observe_at(
        early,
        listing(&[(CODE, RowStatus::Unloaded), (SMART, RowStatus::Unloaded)]),
    );
    s.poll(1_000);
    assert_eq!(s.holder(), Some(CODE));
    // Another model waits; the unload is asked for.
    queued(s.arrive(1_001, SMART, WaitKind::Request, None, 10_000));
    let before_unload = s.listing_requested();
    assert_eq!(unloads(&s.poll(1_002)), vec![CODE.to_owned()]);
    // A read started before the unload cannot show it done.
    s.observe_at(before_unload, listing(&[(CODE, RowStatus::Unloaded)]));
    assert!(s.switching());
    assert!(granted(&s.poll(1_003)).is_empty());
    // A read started after it can; an older answer arriving later changes nothing.
    let after = s.listing_requested();
    s.observe_at(after, listing(&[(CODE, RowStatus::Unloaded)]));
    assert!(!s.switching());
    s.observe_at(early, listing(&[(CODE, RowStatus::Loaded)]));
    assert_eq!(granted(&s.poll(1_004)).len(), 1);
    assert_eq!(s.holder(), Some(SMART));
}

#[test]
fn the_holder_is_adopted_from_the_listing_and_cleared_when_it_goes_away() {
    let mut s = Scheduler::new(cfg());
    s.observe(listing(&[(SMART, RowStatus::Loading)]));
    assert_eq!(s.holder(), Some(SMART));
    s.observe(listing(&[(SMART, RowStatus::Unloaded)]));
    s.poll(1);
    assert_eq!(s.holder(), None);
    admitted(s.arrive(2, CODE, WaitKind::Request, None, 1000));
}

#[test]
fn a_finish_without_admission_is_counted_and_ignored() {
    let mut s = Scheduler::new(cfg());
    s.finish(
        0,
        &Ticket {
            model: CODE.into(),
            lease: None,
            read_only: false,
        },
    );
    assert_eq!(s.status(0).counters.finish_without_admission, 1);
    assert_eq!(s.in_flight(CODE), 0);
}

#[test]
fn refusals_carry_stable_codes_statuses_and_retry_hints() {
    assert_eq!(Refusal::QueueTimeout.code(), "inference_queue_timeout");
    assert_eq!(Refusal::QueueTimeout.http_status(), 503);
    assert!(Refusal::QueueTimeout.retryable());
    assert_eq!(Refusal::ModelInUse.http_status(), 409);
    assert_eq!(Refusal::EngineUnavailable.http_status(), 502);
    assert!(!Refusal::InvalidJson.retryable());
    assert_eq!(Refusal::BodyTooLarge.http_status(), 413);
    assert_eq!(Refusal::AdminRequired.http_status(), 403);
    assert_eq!(Refusal::ModelNotFound.code(), "model_not_found");
    assert_eq!(Refusal::ModelNotFound.http_status(), 404);
    assert!(!Refusal::ModelNotFound.retryable());
    assert_eq!(Refusal::InvalidQuery.http_status(), 400);
}

#[test]
fn config_limits_are_validated() {
    assert!(Config::default().validate().is_ok());
    let zero_wait = Config {
        max_wait_ms: 0,
        ..Config::default()
    };
    assert!(zero_wait.validate().is_err());
    let idle_over_hard = Config {
        lease_idle_ms: Config::default().lease_hard_ms + 1,
        ..Config::default()
    };
    assert!(idle_over_hard.validate().is_err());
    let no_waiters = Config {
        max_waiters: 0,
        ..Config::default()
    };
    assert!(no_waiters.validate().is_err());
    let no_session_hold = Config {
        session_hold_ms: 0,
        ..Config::default()
    };
    assert!(no_session_hold.validate().is_err());
    let mut bad_exempt = Config::default();
    bad_exempt.exempt.insert("bad\nid".into());
    assert!(bad_exempt.validate().is_err());
}

#[test]
fn a_non_loading_request_on_the_holder_is_admitted_while_another_model_waits_and_leaves_no_grace() {
    let mut s = with_loaded(CODE);
    let t = admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    queued(s.arrive(1, SMART, WaitKind::Request, None, 10_000));
    // Past the fair hold: loading work for the holder would queue behind the other model.
    queued(s.arrive(1_500, CODE, WaitKind::Request, None, 10_000));
    let read = admitted(s.arrive_without_autoload(1_500, CODE, None, 10_000));
    assert!(read.read_only);
    assert_eq!(s.in_flight(CODE), 2);
    assert_eq!(s.queued_models(), vec![SMART.to_owned(), CODE.to_owned()]);
    s.finish(1_600, &read);
    s.finish(1_600, &t);
    // Only the generation's residency grace (200 ms) delays the switch, not the read.
    assert!(unloads(&s.poll(1_799)).is_empty());
    assert_eq!(unloads(&s.poll(1_800)), vec![CODE.to_owned()]);
}

#[test]
fn a_non_loading_request_never_queues_or_switches() {
    let mut s = with_loaded(CODE);
    // Not resident: passed on, nothing changes.
    assert_eq!(
        s.arrive_without_autoload(0, SMART, None, 10_000),
        Arrival::Pass
    );
    assert!(s.poll(10).is_empty());
    assert_eq!(s.holder(), Some(CODE));
    // While the holder is being unloaded: refused as busy, never queued.
    queued(s.arrive(20, SMART, WaitKind::Request, None, 10_000));
    assert_eq!(unloads(&s.poll(500)), vec![CODE.to_owned()]);
    assert_eq!(
        s.arrive_without_autoload(600, CODE, None, 10_000),
        Arrival::Refused(Refusal::EngineBusy)
    );
    assert_eq!(s.waiting(), 1);
}

#[test]
fn a_session_hold_ends_when_released_and_is_renewed_while_its_generation_runs() {
    let mut s = with_loaded(CODE);
    let t = admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    queued(s.arrive(1, SMART, WaitKind::Request, None, 10_000));
    s.finish(100, &t);
    s.hold_session(100, "conv-a", CODE);
    assert_eq!(s.held_sessions(100), vec!["conv-a".to_owned()]);
    assert_eq!(s.status(100).session_holds, 1);
    // Renewed while the router still runs it: held past the first session hold (3 s).
    assert!(s.extend_session(3_000, "conv-a"));
    assert!(unloads(&s.poll(3_100)).is_empty());
    assert!(unloads(&s.poll(4_499)).is_empty());
    // Released (stopped or ended): the waiting model is served at once.
    assert!(s.release_session("conv-a"));
    assert!(!s.release_session("conv-a"));
    assert_eq!(unloads(&s.poll(4_500)), vec![CODE.to_owned()]);
    assert!(s.held_sessions(4_500).is_empty());
}

#[test]
fn a_renewed_session_hold_never_outlasts_the_lease_hard_limit() {
    let mut s = with_loaded(CODE);
    let t = admitted(s.arrive(0, CODE, WaitKind::Request, None, 10_000));
    queued(s.arrive(1, SMART, WaitKind::Request, None, 10_000));
    s.finish(0, &t);
    s.hold_session(0, "conv-b", CODE);
    for now in (1_000..5_000).step_by(1_000) {
        assert!(s.extend_session(now, "conv-b"));
        assert!(unloads(&s.poll(now)).is_empty());
    }
    // The cap is the lease hard limit (5 s) after the hold began.
    assert!(unloads(&s.poll(4_999)).is_empty());
    assert_eq!(unloads(&s.poll(5_000)), vec![CODE.to_owned()]);
    assert!(!s.extend_session(5_001, "conv-b"));
}
