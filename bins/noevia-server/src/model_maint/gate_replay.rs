//! sbstndalton/noevia#2641: the recordings of Node's maintenance gate (tests/fixtures/maint-gate,
//! from core's own `createMaintenanceGate` on a virtual clock) replayed on a real [`Hold`] of a real
//! [`super::super::Maint`] with the switch on: its lease authority (the grant, `start_release`'s
//! required recovery, the recovery completed), the hold's decisions at Node's virtual times
//! (`tick_at`, `renewed_at`, `assert_covered_at`), its release (`start_release` then `finish`: the
//! settle owed, admission given back, the driver kicked), `Hold::lost_wait` for `aborted`,
//! `Maint::maintenance_held` for `held`, and the settle driver's own attempt and backoff
//! ([`recovery::attempt`], [`Registry::after`], the kick [`drive`](recovery::drive) takes) with the
//! real admission fence and discharge check.
//!
//! Not driven here (the integration tests in tests/lease_coverage.rs cover them over HTTP): the
//! renewal loop's own timing and Node calls (the harness keeps Node's schedule: a tick every `renewMs`
//! from the grant, skipped while one is open, calling the same decisions, and `start_release` at a
//! loss as the loop does), `start_release`'s barrier and DELETE task (Node is not configured here),
//! and the front's settle step against the router (`FrontEnv`; scripted here). `lost_wait`'s own
//! check at the margin point is not in Node's recordings (Node's dispatchSignal aborts only when a
//! tick or assertCovered loses): [`the_in_flight_wait_loses_at_the_margin_point_itself`] tests it.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use super::*;
use crate::model_maint::recovery::{self, Env, Repair};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::AtomicU64;

const FIXTURE: &str = include_str!("../../tests/fixtures/maint-gate/gate.json");
const SOURCES: &str = include_str!("../../tests/fixtures/maint-gate/SOURCES.sha256");
const RECORDER: &str = include_str!("../../tests/fixtures/maint-gate/record-node.cjs");
const RECORDER_PATH: &str = "tests/fixtures/maint-gate/record-node.cjs";

/// Node's `runRecovery` releases a failed attempt's hold before it schedules the backoff, and that
/// release reschedules at 0 ms: Node retries a failed settle or repair at once
/// (sbstndalton/noevia#2660). The driver backs off as Node intends (recorded in "a recovery whose
/// fresh hold is refused"): in these scenarios its attempt times follow that backoff, and everything
/// else matches Node's in order.
const IMMEDIATE_RETRY: &[&str] = &[
    "a recovery that fails, backs off and then succeeds",
    "the recovery backoff doubles to its cap",
    "a repair registered during a lost hold runs after the settle",
];

#[derive(Clone)]
struct Log {
    t: Arc<AtomicU64>,
    events: Arc<Mutex<Vec<Value>>>,
}

impl Log {
    fn push(&self, mut event: Value) {
        if let Some(o) = event.as_object_mut() {
            o.insert("t".into(), json!(self.t.load(Ordering::Acquire)));
        }
        self.events.lock().unwrap().push(event);
    }
}

/// A test Maint with the switch on (no Node, no router: nothing is called over HTTP).
fn maint(options: &Value) -> super::super::Maint {
    let env = [
        ("MODEL_MANAGER_KIND", "llamacpp"),
        ("MODEL_MANAGER_BASE_URL", "http://127.0.0.1:9/v1"),
        ("NOEVIA_RUST_LEASE_COVERAGE", "node"),
    ];
    let get = |k: &str| {
        env.iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| (*v).to_owned())
    };
    let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
    let sizing = Arc::new(crate::model_sizing::Sizing::from_lookup(
        engine.kind(),
        Arc::clone(&engine),
        &get,
    ));
    let mut m = super::super::Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap();
    assert!(m.coverage.node);
    let ms = |k: &str| Duration::from_millis(options[k].as_u64().unwrap());
    m.coverage.margin = ms("coverageMarginMs");
    m.coverage.recoveries = Arc::new(Registry::with_timing(
        ms("recoveryRetryMs"),
        ms("recoveryMaxDelayMs"),
    ));
    m
}

/// The scripted world of one scenario's recovery attempts, on the real authority and admission.
struct Script {
    log: Log,
    authority: lease::Authority,
    local: Arc<AtomicBool>,
    leases: Arc<AtomicU64>,
    grants: Mutex<VecDeque<String>>,
    settles: Mutex<VecDeque<String>>,
}

impl Env for Script {
    type Hold = (String, lease::Grant, String);
    type Fence = recovery::Fence;

    fn reserve(&self) -> Option<recovery::Fence> {
        // Node's `if(maintenance)`: the work's hold still holds this process's admission.
        recovery::Fence::admit(&self.local)
    }
    fn discharged(&self) -> bool {
        !self.authority.recovery_pending()
    }
    async fn hold(&self) -> Result<Self::Hold, String> {
        let n = self.leases.fetch_add(1, Ordering::AcqRel) + 1;
        if self.grants.lock().unwrap().pop_front().as_deref() == Some("refuse") {
            self.log
                .push(json!({"call": "acquire", "refused": true, "recovery": true}));
            return Err("The maintenance lease authority is not answering, so nothing was changed. Try again shortly.".into());
        }
        let grant = self
            .authority
            .try_recovery(None, "qa recovery")
            .map_err(|_| "Inference is busy.".to_owned())?;
        let ticket = self.authority.recovery_ticket().unwrap_or_default();
        let token = format!("qa-lease-{n}");
        self.log
            .push(json!({"call": "acquire", "token": token, "recovery": true}));
        Ok((token, grant, ticket))
    }
    async fn settle(&self, _: &Self::Hold) -> Result<(), String> {
        let outcome = self
            .settles
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "ok".into());
        self.log.push(json!({ "settle": outcome }));
        if outcome == "ok" {
            Ok(())
        } else {
            Err("The model server did not answer.".into())
        }
    }
    fn covered(&self, (_, g, _): &Self::Hold) -> Result<(), String> {
        if self.authority.covers(&g.id, g.generation, Duration::ZERO) {
            Ok(())
        } else {
            Err(coverage_message(ENDED))
        }
    }
    async fn complete(&self, (_, _, ticket): &Self::Hold) -> Result<(), String> {
        if self.authority.complete_recovery(ticket) {
            Ok(())
        } else {
            Err("The model service did not confirm the recovery.".into())
        }
    }
    async fn release(&self, (token, grant, _): Self::Hold) {
        self.authority.release(&grant.id, Some(grant.generation));
        self.log.push(json!({"call": "release", "token": token}));
    }
    fn log(&self, line: String) {
        self.log.push(json!({ "log": line }));
    }
}

fn strings(v: &Value) -> VecDeque<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|x| x.as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn ms(v: &Value) -> u64 {
    v.as_u64().unwrap()
}

/// Whether `hold.lost_wait()` has resolved (one poll; Node's `signal.aborted`).
fn aborted(hold: &Hold) -> bool {
    let wait = std::pin::pin!(hold.lost_wait());
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    wait.poll(&mut cx).is_ready()
}

/// One scenario on the front's lifecycle: its steps and events, shaped as the recorder writes them.
async fn replay(sc: &Value, options: &Value) -> (Vec<Value>, Vec<Value>) {
    let input = &sc["input"];
    let grant_in = &input["grant"];
    let (ttl, renew, hard) = (
        ms(&grant_in["ttlMs"]),
        ms(&grant_in["renewMs"]),
        ms(&grant_in["hardMs"]),
    );
    let m = maint(options);
    let registry = Arc::clone(&m.coverage.recoveries);
    let base = Instant::now();
    let at = |t: u64| base + Duration::from_millis(t);
    let log = Log {
        t: Arc::new(AtomicU64::new(0)),
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let leases = Arc::new(AtomicU64::new(0));
    let script = Script {
        log: log.clone(),
        authority: m.authority.clone(),
        local: Arc::clone(&m.local),
        leases: Arc::clone(&leases),
        grants: Mutex::new(strings(&input["grants"])),
        settles: Mutex::new(strings(&input["settle"])),
    };
    // The work's own acquire was the first.
    script.grants.lock().unwrap().pop_front();
    let mut renewals: VecDeque<Value> = input["renewals"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into();
    let renew_default = input["renewDefault"].clone();
    let (mut next_tick, mut open, mut due) = (Some(renew), None::<Instant>, None::<u64>);
    let (mut delay, mut was_lost, mut lease_out) = (Duration::ZERO, false, false);
    let mut work: Option<Hold> = None;
    let mut steps = Vec::new();
    let set_t = |t: u64| log.t.store(t, Ordering::Release);
    // Node's lose(): its token goes back at once (the front's renewer breaks to start_release, which
    // gives the authority's lease back), then the line.
    let note_loss = |hold: &Hold, was_lost: &mut bool, lease_out: &mut bool| {
        if hold.shared.lost.load(Ordering::Acquire) && !*was_lost {
            *was_lost = true;
            if let Some(node) = hold.node.as_ref() {
                hold.shared.start_release(Arc::clone(node));
            }
            if *lease_out && !m.authority.held() {
                *lease_out = false;
                log.push(json!({"call": "release", "token": "qa-lease-1"}));
            }
            log.push(json!({ "log": loss_line(hold.shared.lost_why().unwrap()) }));
        }
    };
    for step in input["steps"].as_array().unwrap() {
        let to = ms(&step["at"]);
        // Advance: the timers due by `to`, in time order (the renewal interval first on a tie).
        loop {
            let tick = next_tick.filter(|t| *t <= to && work.is_some());
            let run = due.filter(|t| *t <= to);
            match (tick, run) {
                (Some(t), r) if r.is_none_or(|r| t <= r) => {
                    set_t(t);
                    let hold = work.as_ref().unwrap();
                    let now = at(t);
                    if open.is_none() && hold.shared.tick_at(now) {
                        log.push(json!({"call": "renew", "token": "qa-lease-1"}));
                        let answer = renewals
                            .pop_front()
                            .unwrap_or_else(|| renew_default.clone());
                        if answer == json!("defer") {
                            open = Some(now);
                        } else {
                            hold.shared.renewed_at(now, now, answer.as_bool());
                        }
                    }
                    note_loss(hold, &mut was_lost, &mut lease_out);
                    let stopped = hold.shared.lost.load(Ordering::Acquire)
                        || hold.shared.released.load(Ordering::Acquire);
                    next_tick = (!stopped).then_some(t + renew);
                }
                (_, Some(t)) => {
                    set_t(t);
                    let outcome = recovery::attempt(&registry, &script).await;
                    registry.take_kick();
                    let (next, line) = registry.after(&mut delay, &outcome);
                    if let Some(line) = line {
                        script.log(line);
                    }
                    due = next.map(|d| t + u64::try_from(d.as_millis()).unwrap());
                }
                _ => break,
            }
        }
        set_t(to);
        let now = at(to);
        let mut seen = serde_json::Map::new();
        seen.insert("t".into(), json!(to));
        seen.insert("do".into(), step["do"].clone());
        match step["do"].as_str().unwrap() {
            "hold" => {
                // As `Maint::acquire`: the authority's grant, this process's admission, Node's latch
                // (granted at `base`, renewed by the harness on Node's schedule).
                let own = m
                    .authority
                    .try_acquire(lease::Origin::Front, None, "qa work")
                    .unwrap();
                m.local.store(true, Ordering::Release);
                leases.store(1, Ordering::Release);
                lease_out = true;
                log.push(json!({"call": "acquire", "token": "qa-lease-1", "recovery": false}));
                work = Some(Hold::leased(
                    Arc::clone(&m.local),
                    Arc::clone(&m.node),
                    LeaseGrant {
                        id: "fedcba9876543210fedcba9876543210".into(),
                        generation: 1,
                        ttl: Duration::from_millis(ttl),
                        // The harness ticks; the hold's own renewer never falls due here.
                        renew_every: Duration::from_secs(36_000),
                        hard: Duration::from_millis(hard),
                        asked: base,
                    },
                    Some((m.authority.clone(), own)),
                    "qa work",
                    at(36_000_000),
                    Some(&m.coverage),
                ));
            }
            "assert" => {
                let hold = work.as_ref().unwrap();
                let step_ms = ms(&step["stepMs"]);
                seen.insert("stepMs".into(), json!(step_ms));
                let result = match hold
                    .shared
                    .assert_covered_at(now, Duration::from_millis(step_ms))
                {
                    Ok(()) => json!({"ok": true}),
                    Err(why) => json!({"ok": false, "message": coverage_message(why),
                        "code": "maintenanceCoverageLost", "status": 503}),
                };
                note_loss(hold, &mut was_lost, &mut lease_out);
                seen.insert("result".into(), result);
            }
            "release" => {
                // `Hold::release` under the switch, without its wait for Node's acknowledgement.
                let hold = work.as_mut().unwrap();
                if let Some(node) = hold.node.as_ref() {
                    hold.shared.start_release(Arc::clone(node));
                }
                hold.finish();
                if lease_out && !m.authority.held() {
                    lease_out = false;
                    log.push(json!({"call": "release", "token": "qa-lease-1"}));
                }
                next_tick = None;
            }
            "resolve" => {
                let hold = work.as_ref().unwrap();
                if let Some(asked) = open.take() {
                    hold.shared.renewed_at(asked, now, step["value"].as_bool());
                }
                note_loss(hold, &mut was_lost, &mut lease_out);
            }
            "require" => {
                let key = step["key"].as_str().unwrap().to_owned();
                seen.insert("key".into(), json!(key));
                let outcomes = Arc::new(Mutex::new(strings(&step["repair"])));
                let (l, k) = (log.clone(), key.clone());
                let repair: Repair = Arc::new(move || {
                    let outcome = outcomes
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| "ok".into());
                    l.push(json!({"repair": k, "outcome": outcome}));
                    Box::pin(async move {
                        if outcome == "ok" {
                            Ok(())
                        } else {
                            Err("The qa repair did not finish.".into())
                        }
                    })
                });
                registry.require(&key, Some(repair));
                // Node's public requireRecovery schedules only while nothing holds.
                if !m.local_held() {
                    registry.kick();
                }
            }
            _ => {}
        }
        // The driver takes a kick when no attempt is scheduled.
        if due.is_none() && registry.take_kick() && registry.pending() {
            due = Some(to);
        }
        let hold = work.as_ref().unwrap();
        let lost = aborted(hold);
        seen.insert("held".into(), json!(m.maintenance_held()));
        seen.insert("recoveryPending".into(), json!(registry.pending()));
        seen.insert("aborted".into(), json!(lost));
        seen.insert(
            "abortReason".into(),
            if lost {
                json!(hold.coverage_error())
            } else {
                Value::Null
            },
        );
        steps.push(Value::Object(seen));
    }
    let events = log.events.lock().unwrap().clone();
    (steps, events)
}

fn without_t(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .map(|e| {
            let mut e = e.clone();
            e.as_object_mut().unwrap().remove("t");
            e
        })
        .collect()
}

#[tokio::test]
async fn replays_nodes_gate_recordings() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(fixture["version"], json!(1));
    assert_eq!(fixture["coreCommit"].as_str().map(str::len), Some(40));
    // The recorder that wrote the fixture is the one committed beside it.
    let recorder: String = Sha256::digest(RECORDER.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        fixture["recorderSha256"],
        json!(recorder),
        "record-node.cjs changed since the recording"
    );
    let mut listed: Vec<(String, String)> = SOURCES
        .lines()
        .map(|l| {
            let (sha, file) = l.split_once("  ").unwrap();
            (file.to_owned(), sha.to_owned())
        })
        .collect();
    assert_eq!(listed.pop(), Some((RECORDER_PATH.to_owned(), recorder)));
    let recorded: Vec<(String, String)> = fixture["sourceSha256"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(f, s)| (f.clone(), s.as_str().unwrap().to_owned()))
        .collect();
    assert_eq!(listed, recorded, "SOURCES.sha256 and the fixture disagree");
    let options = &fixture["options"];
    let scenarios = fixture["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 13);
    for sc in scenarios {
        let name = sc["name"].as_str().unwrap();
        let (steps, events) = replay(sc, options).await;
        let (node_steps, node_events) = (
            sc["steps"].as_array().unwrap(),
            sc["events"].as_array().unwrap(),
        );
        if !IMMEDIATE_RETRY.contains(&name) {
            assert_eq!(&steps, node_steps, "{name}: steps");
            assert_eq!(&events, node_events, "{name}: events");
            continue;
        }
        // The same events in the same order; the attempts at the release, then 5 s doubling.
        assert_eq!(without_t(&events), without_t(node_events), "{name}: events");
        let release = node_steps
            .iter()
            .find(|s| s["do"] == json!("release"))
            .unwrap()["t"]
            .as_u64()
            .unwrap();
        let attempts: Vec<u64> = events
            .iter()
            .filter(|e| e["call"] == json!("acquire") && e["recovery"] == json!(true))
            .map(|e| e["t"].as_u64().unwrap())
            .collect();
        let mut expected = vec![release];
        let mut wait = 5_000;
        while expected.len() < attempts.len() {
            expected.push(expected.last().unwrap() + wait);
            wait = (wait * 2).min(60_000);
        }
        assert_eq!(attempts, expected, "{name}: the backoff Node intends");
        let last = *expected.last().unwrap();
        for (ours, node) in steps.iter().zip(node_steps) {
            let t = node["t"].as_u64().unwrap();
            if t <= release && node["do"] != json!("check") || t >= last {
                assert_eq!(ours, node, "{name}: step at {t}");
            } else {
                // Between the first attempt and the last, the recovery is still owed here.
                assert_eq!(ours["recoveryPending"], json!(true), "{name}: step at {t}");
                assert_eq!(ours["held"], json!(true), "{name}: step at {t}");
            }
        }
    }
}

/// A named difference from Node (README, notes): a call in flight under a hold nobody renews ends at
/// the margin point itself (`lost_wait` checks coverage there), not at the next renewal tick or
/// coverage check as Node's dispatchSignal does; until then the hold is not lost.
#[tokio::test]
async fn the_in_flight_wait_loses_at_the_margin_point_itself() {
    let m = maint(
        &json!({"coverageMarginMs": 200, "recoveryRetryMs": 5000, "recoveryMaxDelayMs": 60000}),
    );
    let own = m
        .authority
        .try_acquire(lease::Origin::Front, None, "qa work")
        .unwrap();
    let asked = Instant::now();
    let hold = Hold::leased(
        Arc::clone(&m.local),
        Arc::clone(&m.node),
        LeaseGrant {
            id: "fedcba9876543210fedcba9876543210".into(),
            generation: 1,
            ttl: Duration::from_millis(600),
            renew_every: Duration::from_secs(3600),
            hard: Duration::from_secs(3600),
            asked,
        },
        Some((m.authority.clone(), own)),
        "qa work",
        asked + Duration::from_secs(3600),
        Some(&m.coverage),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !hold.shared.lost.load(Ordering::Acquire),
        "covered before the margin point"
    );
    tokio::time::timeout(Duration::from_secs(2), hold.lost_wait())
        .await
        .unwrap();
    let at = asked.elapsed();
    assert!(
        at >= Duration::from_millis(400) && at < Duration::from_millis(600),
        "{at:?}"
    );
    assert_eq!(hold.coverage_error(), coverage_message(NOT_RENEWED));
}
