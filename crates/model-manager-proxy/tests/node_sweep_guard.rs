//! Replays `fixtures/node-sweep-guard.v1.json` (written by `record_sweep_guard.cjs` from core's own
//! benchmark-sweep-guard.cjs) through `model_manager_proxy::sweep_guard`: acquire's failure for
//! every recorded refusal, and every scenario's whole trace (each call, timer, barrier, release,
//! log line and settle, in order) under the recorder's driver loop, copied here.
//!
//! Until the fixture is recorded the replay is skipped (set `SWEEP_GUARD_ORACLE_REQUIRED=1` to make
//! a missing fixture fail); the hand-derived traces below run either way.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;
use model_manager_proxy::sweep_guard::{
    acquire_failure, Answer, Call, Effect, Guard, HoldAnswer, Options, Refusal, Settled, Timer,
    WatchId, LOG_ADOPTED,
};
use std::collections::VecDeque;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/node-sweep-guard.v1.json"
);

enum Pending {
    Progress(Call),
    Hold,
}

/// The recorder's `run(sc)`.
struct Driver {
    guard: Guard,
    trace: Vec<String>,
    pending: VecDeque<Pending>,
    /// Timers in the order they were set, with whether each is still live.
    timers: Vec<(Timer, bool)>,
    /// The lease signals in the order they were made: the watch each belongs to, if any.
    controllers: Vec<Option<WatchId>>,
    harness_watches: Vec<WatchId>,
}

fn num(v: &JValue, key: &str) -> u64 {
    v.get(key)
        .as_num()
        .unwrap_or_else(|| panic!("option {key}")) as u64
}

impl Driver {
    fn new(options: &JValue) -> Self {
        Driver {
            guard: Guard::new(Options {
                poll_ms: num(options, "pollMs"),
                adopt_retry_ms: num(options, "adoptRetryMs"),
                adopt_max_delay_ms: num(options, "adoptMaxDelayMs"),
                barrier: options.get("barrier").truthy(),
            }),
            trace: Vec::new(),
            pending: VecDeque::new(),
            timers: Vec::new(),
            controllers: Vec::new(),
            harness_watches: Vec::new(),
        }
    }

    fn effects(&mut self, out: Vec<Effect>) {
        for e in out {
            match e {
                Effect::Progress(call) => {
                    self.trace.push("progress".into());
                    self.pending.push_back(Pending::Progress(call));
                }
                Effect::Hold => {
                    self.trace.push("hold:sweep:true".into());
                    self.pending.push_back(Pending::Hold);
                }
                Effect::Barrier => self.trace.push("barrier:sweep".into()),
                Effect::Lift => self.trace.push("lift".into()),
                Effect::SetTimer(t, ms) => {
                    self.trace.push(format!("timer:{ms}"));
                    self.timers.push((t, true));
                }
                Effect::ClearTimer(t) => {
                    self.trace.push("clear".into());
                    if let Some(x) = self.timers.iter_mut().find(|x| x.0 == t && x.1) {
                        x.1 = false;
                    }
                }
                Effect::Release(_) | Effect::ReleaseAdopted => self.trace.push("release".into()),
                Effect::Adopted(id) => {
                    *self.controllers.last_mut().expect("a granted hold") = Some(id);
                }
                Effect::Log(m) => self.trace.push(format!("log:{m}")),
                Effect::AdoptSettled(Settled::Resolved(v)) => {
                    self.trace.push(format!("settled:{v}"));
                }
                Effect::AdoptSettled(Settled::Rejected) => self.trace.push("rejected".into()),
            }
        }
    }

    fn watch(&mut self, lost: bool) {
        self.controllers.push(None);
        let (id, out) = self.guard.watch(lost);
        *self.controllers.last_mut().unwrap() = Some(id);
        self.harness_watches.push(id);
        self.effects(out);
    }

    fn apply(&mut self, what: &str) {
        self.trace.push(format!("do:{what}"));
        match what {
            "cancel" => {
                let out = self.guard.cancel_adopt();
                self.effects(out);
            }
            "lose" => {
                if let Some(Some(id)) = self.controllers.last().copied() {
                    let out = self.guard.lease_lost(id);
                    self.effects(out);
                }
            }
            "stop" => {
                if let Some(id) = self.harness_watches.last().copied() {
                    let out = self.guard.stop(id);
                    self.effects(out);
                }
            }
            "watch" => self.watch(false),
            "watch-lost" => self.watch(true),
            "adopt" => {
                if self.guard.adopting() {
                    self.trace.push("adopt:skipped".into());
                } else {
                    let out = self.guard.adopt();
                    self.effects(out);
                }
            }
            other => panic!("unknown event {other}"),
        }
    }
}

fn items(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        _ => panic!("not a list"),
    }
}

/// The recorder's driver loop over one scenario.
fn run(sc: &JValue) -> Vec<String> {
    let mut d = Driver::new(sc.get("options"));
    let events = items(sc.get("events"));
    let progress = items(sc.get("progress"));
    let holds = items(sc.get("holds"));
    let max_steps = num(sc, "maxSteps");
    let (mut pi, mut hi) = (0usize, 0usize);
    d.apply(sc.get("start").as_str().expect("start"));
    let mut end = "steps";
    for step in 0..max_steps {
        for e in events {
            if e.get("at").as_num() == Some(step as f64) {
                d.apply(e.get("do").as_str().expect("event"));
            }
        }
        if let Some(p) = d.pending.pop_front() {
            match p {
                Pending::Progress(call) => {
                    let Some(a) = progress.get(pi) else {
                        end = "out";
                        break;
                    };
                    pi += 1;
                    d.trace.push("answer".into());
                    let answer = if a.get("throws").truthy() {
                        Answer::Threw
                    } else {
                        Answer::Value(a.get("value").clone())
                    };
                    let out = d.guard.progress_answered(call, &answer);
                    d.effects(out);
                }
                Pending::Hold => {
                    let Some(h) = holds.get(hi) else {
                        end = "out";
                        break;
                    };
                    hi += 1;
                    d.trace.push("answer".into());
                    let answer = if h.get("grant").truthy() {
                        // makeRelease(lost): a fresh lease signal.
                        d.controllers.push(None);
                        HoldAnswer::Granted {
                            lost: h.get("lost") == &JValue::Bool(true),
                        }
                    } else {
                        HoldAnswer::Refused
                    };
                    let out = d.guard.hold_answered(answer);
                    d.effects(out);
                }
            }
        } else if let Some(i) = d.timers.iter().position(|x| x.1) {
            d.timers[i].1 = false;
            let t = d.timers[i].0;
            d.trace.push("fire".into());
            let out = d.guard.timer_fired(t);
            d.effects(out);
        } else if events
            .iter()
            .any(|e| e.get("at").as_num().is_some_and(|at| at > step as f64))
        {
            d.trace.push("idle".into());
        } else {
            end = "quiet";
            break;
        }
    }
    d.trace.push(format!("end:{end}"));
    d.trace.push(format!("active:{}", d.guard.active()));
    d.trace
}

fn scenario(json: &str) -> JValue {
    js_json::parse(json).expect("scenario")
}

const BASE: &str =
    r#""options":{"pollMs":1000,"adoptRetryMs":100,"adoptMaxDelayMs":150,"barrier":true}"#;
const RUNNING_1: &str =
    r#"{"value":{"ok":true,"body":{"job":{"active":true,"unit":"models","total":1}}}}"#;

fn expect(sc: &JValue, want: &[&str]) {
    let got = run(sc);
    let adopted = format!("log:{LOG_ADOPTED}");
    let want: Vec<String> = want
        .iter()
        .map(|w| {
            if *w == "ADOPTED" {
                adopted.clone()
            } else {
                (*w).to_string()
            }
        })
        .collect();
    assert_eq!(got, want);
}

#[test]
fn hand_adopt_backoff_then_hold() {
    let sc = scenario(&format!(
        r#"{{{BASE},"start":"adopt","events":[],"progress":[{{"throws":true}},{{"value":{{"ok":false,"status":503}}}},{{"value":{{"ok":true,"body":{{"job":{{"active":true,"unit":"models","total":2}}}}}}}}],"holds":[{{"grant":true}}],"maxSteps":12}}"#
    ));
    expect(
        &sc,
        &[
            "do:adopt",
            "barrier:sweep",
            "progress",
            "answer",
            "timer:100",
            "fire",
            "progress",
            "answer",
            "timer:150",
            "fire",
            "progress",
            "answer",
            "hold:sweep:true",
            "answer",
            "lift",
            "ADOPTED",
            "timer:1000",
            "settled:true",
            "fire",
            "progress",
            "end:out",
            "active:true",
        ],
    );
}

#[test]
fn hand_watch_outage_then_terminal() {
    let sc = scenario(&format!(
        r#"{{{BASE},"start":"watch","events":[],"progress":[{RUNNING_1},{{"value":null}},{{"value":null}},{{"value":null}},{{"value":{{"ok":true,"body":{{"job":{{"active":false}}}}}}}}],"holds":[],"maxSteps":20}}"#
    ));
    let mut want = vec!["do:watch", "timer:1000"];
    for _ in 0..4 {
        want.extend(["fire", "progress", "answer", "timer:1000"]);
    }
    want.extend([
        "fire",
        "progress",
        "answer",
        "release",
        "end:quiet",
        "active:false",
    ]);
    expect(&sc, &want);
}

#[test]
fn hand_cancel_during_a_wait() {
    let sc = scenario(
        r#"{"options":{"pollMs":1000,"adoptRetryMs":10,"adoptMaxDelayMs":150,"barrier":true},"start":"adopt","events":[{"at":1,"do":"cancel"}],"progress":[{"value":null},{"value":null}],"holds":[],"maxSteps":6}"#,
    );
    expect(
        &sc,
        &[
            "do:adopt",
            "barrier:sweep",
            "progress",
            "answer",
            "timer:10",
            "do:cancel",
            "clear",
            "settled:false",
            "end:quiet",
            "active:false",
        ],
    );
}

#[test]
fn hand_a_watch_started_meanwhile_wins() {
    let sc = scenario(&format!(
        r#"{{{BASE},"start":"adopt","events":[{{"at":1,"do":"watch"}}],"progress":[{RUNNING_1},{RUNNING_1}],"holds":[{{"grant":true}}],"maxSteps":8}}"#
    ));
    expect(
        &sc,
        &[
            "do:adopt",
            "barrier:sweep",
            "progress",
            "answer",
            "hold:sweep:true",
            "do:watch",
            "timer:1000",
            "answer",
            "lift",
            "release",
            "settled:false",
            "fire",
            "progress",
            "answer",
            "timer:1000",
            "fire",
            "progress",
            "end:out",
            "active:true",
        ],
    );
}

#[test]
fn hand_lease_lost_logs_once_and_keeps_the_hold() {
    let lost = format!("log:{}", model_manager_proxy::sweep_guard::LOG_LEASE_LOST);
    let sc = scenario(&format!(
        r#"{{{BASE},"start":"watch","events":[{{"at":0,"do":"lose"}},{{"at":3,"do":"lose"}}],"progress":[{RUNNING_1},{{"value":{{"ok":true,"body":{{"job":{{"active":false}}}}}}}}],"holds":[],"maxSteps":12}}"#
    ));
    let got = run(&sc);
    assert_eq!(
        got,
        vec![
            "do:watch",
            "timer:1000",
            "do:lose",
            lost.as_str(),
            "fire",
            "progress",
            "answer",
            "timer:1000",
            "fire",
            "progress",
            "do:lose",
            "answer",
            "release",
            "end:quiet",
            "active:false",
        ]
    );
}

fn load() -> Option<JValue> {
    match std::fs::read_to_string(FIXTURE) {
        Ok(text) => Some(js_json::parse(&text).expect("the fixture parses")),
        Err(_) if std::env::var("SWEEP_GUARD_ORACLE_REQUIRED").as_deref() == Ok("1") => {
            panic!("{FIXTURE} is missing: record it with record_sweep_guard.cjs")
        }
        Err(_) => {
            eprintln!("skipped: {FIXTURE} not recorded yet");
            None
        }
    }
}

#[test]
fn node_acquire_failures() {
    let Some(f) = load() else { return };
    let cases = items(f.get("acquire"));
    assert!(!cases.is_empty());
    for case in cases {
        let err = case.get("err");
        let got = acquire_failure(&Refusal {
            status: err.get("status").clone(),
            code: err.get("code").clone(),
            from_gate: err.get("fromGate").clone(),
            chat_active: err.get("chatActive").clone(),
        });
        let want = case.get("failure");
        let label = js_json::stringify(case).unwrap_or_default();
        assert_eq!(
            want.get("status").as_num(),
            Some(f64::from(got.status)),
            "{label}"
        );
        assert_eq!(want.get("code").as_str(), got.code, "{label}");
        assert_eq!(
            want.get("publicMessage").as_str(),
            Some(got.public_message),
            "{label}"
        );
        assert_eq!(
            want.get("message").as_str(),
            Some(got.public_message),
            "{label}"
        );
        assert_eq!(
            want.get("waitable").as_bool(),
            Some(got.waitable),
            "{label}"
        );
        assert_eq!(
            want.get("unavailable").as_bool(),
            Some(got.unavailable),
            "{label}"
        );
    }
}

#[test]
fn node_scenarios() {
    let Some(f) = load() else { return };
    let scenarios = items(f.get("scenarios"));
    assert!(scenarios.len() > 400);
    let mut failed = Vec::new();
    for sc in scenarios {
        let want: Vec<String> = items(sc.get("trace"))
            .iter()
            .map(|t| t.as_str().expect("trace line").to_string())
            .collect();
        let got = run(sc);
        if got != want {
            failed.push(format!(
                "{}:\n  node: {want:?}\n  rust: {got:?}",
                sc.get("name").as_str().unwrap_or("?")
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} differ:\n{}",
        failed.len(),
        scenarios.len(),
        failed.join("\n")
    );
}
