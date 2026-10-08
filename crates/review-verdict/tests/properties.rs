//! Invariants over generated values: never a panic, and every verdict the port accepts is one the
//! schema allows (only its fields, cleaned and bounded text, consistent), whatever the input.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use proptest::prelude::*;
use review_verdict::{bound_review_event, call, read_verdict, Event, Js};

fn u(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn text() -> impl Strategy<Value = Vec<u16>> {
    prop::collection::vec(
        prop_oneof![
            Just(0x20u16),
            Just(0x09),
            Just(0x00),
            Just(0x1b),
            Just(0x7f),
            Just(0x202e),
            Just(0x2066),
            Just(0xfeff),
            Just(0x3000),
            Just(0xd83d),
            Just(0xde00),
            Just(0xd800),
            Just(0x61),
            Just(0x7a),
        ],
        0..20,
    )
}

fn leaf() -> impl Strategy<Value = Js> {
    prop_oneof![
        Just(Js::Undefined),
        Just(Js::Null),
        any::<bool>().prop_map(Js::Bool),
        prop_oneof![
            Just(-0.0f64),
            Just(f64::NAN),
            Just(3.0),
            Just(1e300),
            Just(-1.0),
            Just(2.5)
        ]
        .prop_map(Js::Num),
        text().prop_map(Js::Str),
        prop_oneof![
            Just("approve"),
            Just("request_changes"),
            Just("blocker"),
            Just("note"),
            Just("abcdef0"),
            Just("review.completed")
        ]
        .prop_map(|s| Js::Str(u(s))),
        Just(Js::Func),
        Just(Js::Opaque),
    ]
}

fn value() -> impl Strategy<Value = Js> {
    let key = prop_oneof![
        Just("verdict"),
        Just("summary"),
        Just("findings"),
        Just("severity"),
        Just("file"),
        Just("message"),
        Just("grant"),
        Just("baseSha"),
        Just("files"),
        Just("corrected"),
        Just("code"),
        Just("reason"),
    ];
    leaf().prop_recursive(3, 48, 14, move |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..14).prop_map(Js::Arr),
            prop::collection::btree_map(key.clone(), inner, 0..6)
                .prop_map(|m| Js::Obj(m.into_iter().map(|(k, v)| (u(k), v)).collect())),
        ]
    })
}

fn clean_ok(s: &[u16], max: usize) -> bool {
    let dropped =
        |c: u16| matches!(c, 0x00..=0x08 | 0x0b..=0x1f | 0x7f | 0x202a..=0x202e | 0x2066..=0x2069);
    !s.is_empty()
        && s.len() <= 2 * max
        && !s.iter().any(|&c| dropped(c))
        && !prompt_framing::js::is_js_space(s[0])
        && !prompt_framing::js::is_js_space(s[s.len() - 1])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn accepted_verdicts_are_schema_verdicts(raw in value()) {
        if let Ok(Ok(v)) = read_verdict(&raw) {
            prop_assert!(clean_ok(&v.summary, 600));
            prop_assert!(v.findings.len() <= 12);
            prop_assert!(v.verdict != "request_changes" || !v.findings.is_empty());
            prop_assert!(v.verdict != "approve" || v.findings.iter().all(|f| f.severity != "blocker"));
            for f in &v.findings {
                prop_assert!(clean_ok(&f.message, 600));
                if let Some(file) = &f.file { prop_assert!(clean_ok(file, 240)); }
            }
        }
    }

    #[test]
    fn events_never_panic(kind in leaf(), data in value()) {
        if let Ok(Event::Requested { files: Some(n), .. }) = bound_review_event(&kind, &data) {
            prop_assert!((0.0..=100_000.0).contains(&n));
        }
    }

    #[test]
    fn call_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        let (status, reply) = call(&bytes);
        prop_assert!(status <= 1);
        prop_assert!(reply.starts_with('{'), "reply {}", reply);
    }
}
