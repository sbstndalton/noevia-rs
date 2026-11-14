//! sbstndalton/noevia#2641: the recordings of Node's maintenance gate (tests/fixtures/maint-gate,
//! from core's own `createMaintenanceGate` on a virtual clock) replayed on this front's lifecycle of a
//! native lease: [`Shared`]'s decisions (`tick_at`, `renewed_at`, `assert_covered_at`, `let_go`) at
//! Node's virtual times, and the settle driver's attempt and backoff ([`recovery::attempt`],
//! [`Registry::after`], the kick [`drive`](recovery::drive) takes) against a scripted environment.
//! The harness keeps Node's schedule: a renewal tick every `renewMs` from the grant (skipped while one
//! is open), and the driver's next attempt when it falls due.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use super::*;
use crate::model_maint::recovery::{self, Env, Repair};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;

const FIXTURE: &str = include_str!("../../tests/fixtures/maint-gate/gate.json");
const SOURCES: &str = include_str!("../../tests/fixtures/maint-gate/SOURCES.sha256");

/// Node's `runRecovery` releases a failed attempt's hold before it schedules the backoff, and that
/// release reschedules at 0 ms: Node retries a failed settle or repair at once. The driver backs off
/// as Node intends (recorded in "a recovery whose fresh hold is refused"): in these scenarios its
/// attempt times follow that backoff, and everything else matches Node's in order.
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

/// The scripted world of one scenario's recovery attempts.
struct Script {
    log: Log,
    leases: Arc<AtomicU64>,
    released: Arc<AtomicBool>,
    grants: Mutex<VecDeque<String>>,
    settles: Mutex<VecDeque<String>>,
}

impl Env for Script {
    type Hold = String;
    type Fence = ();

    fn reserve(&self) -> Option<()> {
        // Node's `if(maintenance)`: the work's hold still holds this process's admission.
        self.released.load(Ordering::Acquire).then_some(())
    }
    fn discharged(&self) -> bool {
        false
    }
    async fn hold(&self) -> Result<String, String> {
        let n = self.leases.fetch_add(1, Ordering::AcqRel) + 1;
        if self.grants.lock().unwrap().pop_front().as_deref() == Some("refuse") {
            self.log
                .push(json!({"call": "acquire", "refused": true, "recovery": true}));
            return Err("The maintenance lease authority is not answering, so nothing was changed. Try again shortly.".into());
        }
        let token = format!("qa-lease-{n}");
        self.log
            .push(json!({"call": "acquire", "token": token, "recovery": true}));
        Ok(token)
    }
    async fn settle(&self, _: &String) -> Result<(), String> {
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
    fn covered(&self, _: &String) -> Result<(), String> {
        Ok(())
    }
    async fn complete(&self, _: &String) -> Result<(), String> {
        Ok(())
    }
    async fn release(&self, token: String) {
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

/// One scenario on the front's lifecycle: its steps and events, shaped as the recorder writes them.
async fn replay(sc: &Value, options: &Value) -> (Vec<Value>, Vec<Value>) {
    let input = &sc["input"];
    let grant = &input["grant"];
    let (ttl, renew, hard) = (
        ms(&grant["ttlMs"]),
        ms(&grant["renewMs"]),
        ms(&grant["hardMs"]),
    );
    let base = Instant::now();
    let at = |t: u64| base + Duration::from_millis(t);
    let registry = Arc::new(Registry::with_timing(
        Duration::from_millis(ms(&options["recoveryRetryMs"])),
        Duration::from_millis(ms(&options["recoveryMaxDelayMs"])),
    ));
    let mut shared = Shared::new(
        Some("qa-lease-1".into()),
        at(36_000_000),
        Some(Duration::from_millis(ttl)),
        Some(at(hard)),
        None,
    );
    {
        let s = Arc::get_mut(&mut shared).unwrap();
        s.node = true;
        s.margin = Duration::from_millis(ms(&options["coverageMarginMs"]));
        s.settle = Some(Arc::clone(&registry));
        *s.confirmed.lock().unwrap() = base;
    }
    let log = Log {
        t: Arc::new(AtomicU64::new(0)),
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let released = Arc::new(AtomicBool::new(false));
    let leases = Arc::new(AtomicU64::new(0));
    let script = Script {
        log: log.clone(),
        leases: Arc::clone(&leases),
        released: Arc::clone(&released),
        grants: Mutex::new(strings(&input["grants"])),
        settles: Mutex::new(strings(&input["settle"])),
    };
    // The work's own acquire was the first.
    script.grants.lock().unwrap().pop_front();
    let mut renewals = strings_or_values(&input["renewals"]);
    let renew_default = input["renewDefault"].clone();
    let (mut next_tick, mut open, mut due) = (Some(renew), None::<Instant>, None::<u64>);
    let (mut delay, mut was_lost) = (Duration::ZERO, false);
    let mut steps = Vec::new();
    let set_t = |t: u64| log.t.store(t, Ordering::Release);
    // Node's lose(): its token goes back at once, then the line.
    let note_loss = |was_lost: &mut bool| {
        if shared.lost.load(Ordering::Acquire) && !*was_lost {
            *was_lost = true;
            log.push(json!({"call": "release", "token": "qa-lease-1"}));
            log.push(json!({ "log": loss_line(shared.lost_why().unwrap()) }));
        }
    };
    for step in input["steps"].as_array().unwrap() {
        let to = ms(&step["at"]);
        // Advance: the timers due by `to`, in time order (the renewal interval first on a tie).
        loop {
            let tick = next_tick.filter(|t| *t <= to);
            let run = due.filter(|t| *t <= to);
            match (tick, run) {
                (Some(t), r) if r.is_none_or(|r| t <= r) => {
                    set_t(t);
                    let now = at(t);
                    if open.is_none() && shared.tick_at(now) {
                        log.push(json!({"call": "renew", "token": "qa-lease-1"}));
                        let answer = renewals
                            .pop_front()
                            .unwrap_or_else(|| renew_default.clone());
                        if answer == json!("defer") {
                            open = Some(now);
                        } else {
                            shared.renewed_at(now, now, answer.as_bool());
                        }
                    }
                    note_loss(&mut was_lost);
                    let stopped = shared.lost.load(Ordering::Acquire)
                        || shared.released.load(Ordering::Acquire);
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
                leases.store(1, Ordering::Release);
                log.push(json!({"call": "acquire", "token": "qa-lease-1", "recovery": false}));
            }
            "assert" => {
                let step_ms = ms(&step["stepMs"]);
                seen.insert("stepMs".into(), json!(step_ms));
                let result = match shared.assert_covered_at(now, Duration::from_millis(step_ms)) {
                    Ok(()) => json!({"ok": true}),
                    Err(why) => json!({"ok": false, "message": coverage_message(why),
                        "code": "maintenanceCoverageLost", "status": 503}),
                };
                note_loss(&mut was_lost);
                seen.insert("result".into(), result);
            }
            "release" => {
                if !shared.lost.load(Ordering::Acquire) {
                    log.push(json!({"call": "release", "token": "qa-lease-1"}));
                }
                shared.let_go();
                released.store(true, Ordering::Release);
                shared.kick_owed();
                next_tick = None;
            }
            "resolve" => {
                if let Some(asked) = open.take() {
                    shared.renewed_at(asked, now, step["value"].as_bool());
                }
                note_loss(&mut was_lost);
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
                if released.load(Ordering::Acquire) {
                    registry.kick();
                }
            }
            _ => {}
        }
        // The driver takes a kick when no attempt is scheduled.
        if due.is_none() && registry.take_kick() && registry.pending() {
            due = Some(to);
        }
        let lost = shared.lost.load(Ordering::Acquire);
        seen.insert(
            "held".into(),
            json!(!released.load(Ordering::Acquire) || registry.pending()),
        );
        seen.insert("recoveryPending".into(), json!(registry.pending()));
        seen.insert("aborted".into(), json!(lost));
        seen.insert(
            "abortReason".into(),
            shared
                .lost_why()
                .filter(|_| lost)
                .map_or(Value::Null, |w| json!(coverage_message(w))),
        );
        steps.push(Value::Object(seen));
    }
    let events = log.events.lock().unwrap().clone();
    (steps, events)
}

fn strings_or_values(v: &Value) -> VecDeque<Value> {
    v.as_array().cloned().unwrap_or_default().into()
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
    let listed: Vec<(String, String)> = SOURCES
        .lines()
        .map(|l| {
            let (sha, file) = l.split_once("  ").unwrap();
            (file.to_owned(), sha.to_owned())
        })
        .collect();
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
