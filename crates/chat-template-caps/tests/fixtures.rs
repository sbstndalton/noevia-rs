//! Shared fixtures (noevia#1002, #1003): `fixtures/chat-template-caps.v1.json` is byte-identical to
//! noevia-core's `tests/fixtures/chat-template-caps.v1.json` (noevia-core CI compares the two) and
//! is made by noevia-core's `tools/gen-chat-template-caps-fixtures.cjs`. The context cases carry
//! the JS reference's own answer (`chat-context.cjs` providerErrorJs).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_template_caps::{analyze, reply_json, serving_verdict, MAX_TEMPLATE_BYTES};
use provider_error::{classify, is_context_full, Kind, MAX_REASON_CHARS};
use serde_json::Value;

fn fixtures() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/chat-template-caps.v1.json"
    ))
    .expect("fixture file");
    let f: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(f["version"], 1);
    assert_eq!(f["limits"]["maxReasonChars"], MAX_REASON_CHARS);
    assert_eq!(f["limits"]["maxTemplateBytes"], MAX_TEMPLATE_BYTES);
    f
}

#[test]
fn templates() {
    let f = fixtures();
    let cases = f["templates"].as_array().unwrap();
    assert!(cases.len() >= 20);
    for c in cases {
        let got: Value =
            serde_json::from_str(&reply_json(&analyze(c["template"].as_str().unwrap()))).unwrap();
        assert_eq!(got, c["expect"], "{}", c["name"]);
    }
}

#[test]
fn errors() {
    let f = fixtures();
    for c in f["errors"].as_array().unwrap() {
        let got = classify(
            c["status"].as_u64().unwrap() as u32,
            c["body"].as_str().unwrap(),
        );
        assert_eq!(got.kind.as_str(), c["expect"]["kind"], "{}", c["name"]);
        if let Some(reason) = c["expect"].get("reason") {
            assert_eq!(got.reason, reason.as_str().unwrap(), "{}", c["name"]);
        }
        assert!(got.reason.chars().count() <= MAX_REASON_CHARS);
    }
}

#[test]
fn context_matches_js() {
    let f = fixtures();
    for c in f["context"].as_array().unwrap() {
        let text = c["text"].as_str().unwrap();
        assert_eq!(
            is_context_full(text),
            c["jsContextFull"].as_bool().unwrap(),
            "{text:?}"
        );
        assert_eq!(
            classify(400, text).kind == Kind::ContextFull,
            c["jsContextFull"].as_bool().unwrap()
        );
    }
}

#[test]
fn verdicts() {
    let f = fixtures();
    for c in f["verdicts"].as_array().unwrap() {
        let v = serving_verdict(
            c["status"].as_u64().unwrap() as u32,
            c["body"].as_str().unwrap(),
        );
        assert_eq!(v.passed, c["expect"]["passed"], "{}", c["name"]);
        assert_eq!(
            v.kind.map(Kind::as_str),
            c["expect"]["kind"].as_str(),
            "{}",
            c["name"]
        );
    }
}
