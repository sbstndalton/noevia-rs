//! Shared fixtures (noevia#1004): `fixtures/load-verdict.v1.json` is byte-identical to
//! noevia-core's `tests/fixtures/load-verdict.v1.json` (noevia-core CI compares the two) and is
//! made by noevia-core's `tools/gen-load-verdict-fixtures.cjs`, whose expectations come from an
//! independent JavaScript reference of this crate's specification. Each case goes through the
//! JSON entry point, as dav-parse.wasm's `load_verdict` sees it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use load_verdict::{verdict_json, MAX_INPUT_BYTES, MAX_TEXT_BYTES, MIN_ADVICE_PERMILLE};
use serde_json::Value;

fn fixtures() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/load-verdict.v1.json"
    ))
    .expect("fixture file");
    let f: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(f["version"], 1);
    let l = &f["limits"];
    assert_eq!(l["maxInputBytes"], MAX_INPUT_BYTES);
    assert_eq!(l["maxTextBytes"], MAX_TEXT_BYTES);
    assert_eq!(l["minAdvicePermille"], MIN_ADVICE_PERMILLE);
    f
}

#[test]
fn cases() {
    let f = fixtures();
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 250);
    let mut sources = std::collections::BTreeMap::new();
    for c in cases {
        let (status, reply) = verdict_json(&c["input"].to_string());
        assert_eq!(status, 0, "{}", c["name"]);
        let got: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(got, c["expect"], "{}", c["name"]);
        *sources
            .entry(got["source"].as_str().unwrap().to_owned())
            .or_insert(0) += 1;
    }
    // Every path is exercised.
    for s in ["measured", "rule", "advisor", "fallback"] {
        assert!(
            sources.get(s).copied().unwrap_or(0) >= 5,
            "{s}: {sources:?}"
        );
    }
}

#[test]
fn errors() {
    let f = fixtures();
    for e in f["errors"].as_array().unwrap() {
        let text = format!(
            "{}{}",
            e["text"].as_str().unwrap(),
            " ".repeat(e["pad"].as_u64().unwrap() as usize)
        );
        let (status, reply) = verdict_json(&text);
        assert_eq!(status, 1, "{}", e["name"]);
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap(),
            e["expect"],
            "{}",
            e["name"]
        );
    }
}
