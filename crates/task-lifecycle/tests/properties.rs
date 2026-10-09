//! Properties of the port: no input panics; the fold is deterministic and replays in chunks; a
//! journal without a lifecycle authority event never reaches reviewing, changes_requested or
//! merged (the #522 honesty constraint); an authoritative journal reaches merged only through a
//! recorded reviewing → merged stage and reviewing only with a report hash; every answered move is
//! in the table; large and deep requests stay bounded.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::json::{self, Value};
use proptest::prelude::*;
use std::time::{Duration, Instant};
use task_lifecycle::{
    assert_stage_move, call, can_transition, derive, fold, transition, State, Throw, STATES,
};

const HASH: &str = "abababababababababababababababababababababababababababababababab";

fn state_name() -> impl Strategy<Value = String> {
    prop_oneof![
        8 => (0usize..7).prop_map(|i| STATES[i].name().to_owned()),
        1 => Just("shipped".to_owned()),
        1 => Just("Planned".to_owned()),
    ]
}

/// One synthetic journal event as JSON text.
fn event(authority: bool) -> BoxedStrategy<String> {
    let plain = prop_oneof![
        Just(r#"{"type":"job.created","data":{"kind":"code"}}"#.to_owned()),
        Just(r#"{"type":"job.started"}"#.to_owned()),
        Just(
            r#"{"type":"job.completed","data":{"result":{"merged":true,"review":true}}}"#
                .to_owned()
        ),
        Just(r#"{"type":"job.failed","data":{"error":"x"}}"#.to_owned()),
        Just(r#"{"type":"job.cancelled"}"#.to_owned()),
        Just(r#"{"type":"job.interrupted"}"#.to_owned()),
        Just(
            r#"{"type":"approval.requested","data":{"review":true,"merged":true,"to":"merged"}}"#
                .to_owned()
        ),
        Just(r#"{"type":"approval.decided","data":{"review":true}}"#.to_owned()),
        Just(r#"{"type":"review.completed","data":{"verdict":"approve"}}"#.to_owned()),
        state_name().prop_map(|s| format!(r#"{{"type":"progress","data":{{"stage":"{s}"}}}}"#)),
        Just("null".to_owned()),
        Just("[]".to_owned()),
        Just(r#"{"type":7}"#.to_owned()),
        Just(r#"{"type":"step.started","data":"str"}"#.to_owned()),
    ];
    if !authority {
        return plain.boxed();
    }
    let stage = (
        state_name(),
        state_name(),
        prop_oneof![
            Just(String::new()),
            Just(format!(r#","reportHash":"{HASH}""#)),
            Just(r#","reportHash":0"#.to_owned()),
            Just(r#","reportHash":"x""#.to_owned()),
        ],
    )
        .prop_map(|(f, t, h)| {
            format!(r#"{{"type":"task.stage","data":{{"from":"{f}","to":"{t}","revision":0{h}}}}}"#)
        });
    prop_oneof![
        2 => plain,
        3 => stage,
        1 => Just(r#"{"type":"task.revision","data":{"n":1}}"#.to_owned()),
    ]
    .boxed()
}

fn journal(authority: bool) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(event(authority), 0..24)
}

fn value(events: &[String]) -> Value {
    json::parse_utf8(format!("[{}]", events.join(",")).as_bytes(), 3).unwrap()
}

fn st(s: State) -> Value {
    Value::Str(s.name().encode_utf16().collect())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn random_bytes_never_panic(input in prop::collection::vec(any::<u8>(), 0..512)) {
        let (status, reply) = call(&input);
        prop_assert!(status <= 1);
        prop_assert!(json::parse_utf8(reply.as_bytes(), 4).is_some());
    }

    #[test]
    fn random_json_requests_never_panic(op in 0u8..7, body in "[\\[\\]{}\",:0-9a-z. ]{0,200}") {
        let mut input = vec![op];
        input.extend(body.as_bytes());
        let (status, reply) = call(&input);
        prop_assert!(status <= 1);
        prop_assert!(json::parse_utf8(reply.as_bytes(), 4).is_some());
    }

    #[test]
    fn without_authority_never_reviewing_or_merged(events in journal(false)) {
        let v = value(&events);
        if let Ok(Ok(s)) = derive(&v) {
            prop_assert!(!matches!(s, State::Reviewing | State::ChangesRequested | State::Merged), "{s:?}");
        }
    }

    #[test]
    fn fold_replays_in_chunks(events in journal(true), k in 0usize..24, auth in any::<bool>()) {
        let k = k.min(events.len());
        let whole = fold(&value(&events), &st(State::Planned), auth);
        prop_assert_eq!(whole, fold(&value(&events), &st(State::Planned), auth));
        if let Ok(Ok(mid)) = fold(&value(&events[..k]), &st(State::Planned), auth) {
            prop_assert_eq!(whole, fold(&value(&events[k..]), &st(mid), auth));
        }
    }

    #[test]
    fn authoritative_merge_needs_review(events in journal(true)) {
        let v = value(&events);
        match derive(&v) {
            Ok(Ok(State::Merged)) => prop_assert!(events.iter().any(|e| e.contains(r#""from":"reviewing","to":"merged""#))),
            Ok(Ok(State::Reviewing)) => prop_assert!(events.iter().any(|e| e.contains(HASH))),
            _ => {}
        }
    }

    #[test]
    fn answered_moves_are_in_the_table(a in 0usize..7, b in 0usize..7) {
        let (from, to) = (STATES[a], STATES[b]);
        prop_assert_eq!(transition(from, to).is_ok(), can_transition(from, to));
        if let Ok(s) = assert_stage_move(from, to) {
            prop_assert_eq!(s, to);
            prop_assert!(can_transition(from, to));
            prop_assert!(to != State::Merged || matches!(from, State::Reviewing | State::Merged));
        }
        if from == State::Merged && to != State::Merged {
            prop_assert_eq!(transition(from, to), Err(Throw::Illegal));
        }
    }
}

#[test]
fn large_and_deep_requests_stay_bounded() {
    // ~7.5 MiB, 150,000 events (just under the cap): linear.
    let one = r#"{"type":"progress","data":{"stage":"implementing"}},"#;
    let mut body = String::from("[[{\"type\":\"job.started\"},");
    body.push_str(&one.repeat(150_000));
    body.push_str("{\"type\":\"job.completed\"}]]");
    let mut input = vec![5u8];
    input.extend(body.as_bytes());
    let t = Instant::now();
    assert_eq!(call(&input), (0, r#"{"state":"verifying"}"#.to_owned()));
    assert!(t.elapsed() < Duration::from_secs(20));
    // Over the cap: refused before parsing.
    let mut big = vec![5u8];
    big.extend(std::iter::repeat_n(b' ', task_lifecycle::MAX_INPUT_BYTES));
    assert_eq!(call(&big).0, 1);
    // 100,000-deep nesting inside an event's data never panics.
    let deep = format!(
        r#"[[{{"type":"job.started","data":{}{}}}]]"#,
        "[".repeat(100_000),
        "]".repeat(100_000)
    );
    let mut input = vec![5u8];
    input.extend(deep.as_bytes());
    assert_eq!(call(&input), (0, r#"{"state":"implementing"}"#.to_owned()));
    let deep_hash = format!(
        r#"[[{{"type":"task.stage","data":{{"from":"planned","to":"implementing"}}}},{{"type":"task.stage","data":{{"from":"implementing","to":"reviewing","reportHash":{}{}}}}}]]"#,
        "[".repeat(100_000),
        "]".repeat(100_000)
    );
    let mut input = vec![5u8];
    input.extend(deep_hash.as_bytes());
    assert_eq!(call(&input), (0, r#"{"throws":"report_hash"}"#.to_owned()));
}
