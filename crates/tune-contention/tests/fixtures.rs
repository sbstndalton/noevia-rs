//! Shared fixtures (noevia#1062): `fixtures/tune-contention.v1.json` is byte-identical to
//! noevia-core's `tests/fixtures/tune-contention.v1.json` (noevia-core CI compares the two) and is
//! made by noevia-core's `tools/gen-tune-contention-fixtures.cjs`, whose expectations come from an
//! independent JavaScript reference of this crate's specification. Each case goes through the
//! JSON entry point, as dav-parse.wasm's `tune_contention` sees it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use serde_json::Value;
use tune_contention::{
    decide_json, MAX_ID_BYTES, MAX_INPUT_BYTES, MAX_QUIET_MS, MAX_ROWS, MAX_STATUS_BYTES,
    MAX_WAIT_MS,
};

fn fixtures() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tune-contention.v1.json"
    ))
    .expect("fixture file");
    let f: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(f["version"], 1);
    let l = &f["limits"];
    assert_eq!(l["maxInputBytes"], MAX_INPUT_BYTES);
    assert_eq!(l["maxRows"], MAX_ROWS);
    assert_eq!(l["maxIdBytes"], MAX_ID_BYTES);
    assert_eq!(l["maxStatusBytes"], MAX_STATUS_BYTES);
    assert_eq!(l["maxWaitMs"], MAX_WAIT_MS);
    assert_eq!(l["maxQuietMs"], MAX_QUIET_MS);
    f
}

#[test]
fn cases() {
    let f = fixtures();
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 250);
    let mut reasons = std::collections::BTreeMap::new();
    for c in cases {
        let (status, reply) = decide_json(&c["input"].to_string());
        assert_eq!(status, 0, "{}", c["name"]);
        let got: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(got, c["expect"], "{}", c["name"]);
        *reasons
            .entry(got["reason"].as_str().unwrap().to_owned())
            .or_insert(0) += 1;
    }
    for r in [
        "clear",
        "timed_out",
        "loading",
        "other",
        "busy",
        "settling",
        "idle",
        "idle_unknown",
    ] {
        assert!(
            reasons.get(r).copied().unwrap_or(0) >= 3,
            "{r}: {reasons:?}"
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
        let (status, reply) = decide_json(&text);
        assert_eq!(status, 1, "{}", e["name"]);
        let got: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(got, e["expect"], "{}", e["name"]);
    }
}
