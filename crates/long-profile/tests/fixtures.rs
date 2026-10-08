//! Shared fixtures (noevia#1079): `fixtures/long-profile.v1.json` is byte-identical to
//! noevia-core's `tests/fixtures/long-profile.v1.json` (noevia-core CI compares the two) and is
//! made by noevia-core's `tools/gen-long-profile-fixtures.cjs`, whose expectations come from an
//! independent JavaScript reference of this crate's specification. Each case goes through the
//! JSON entry point, as dav-parse.wasm's `long_profile` sees it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use long_profile::{
    run_json, MAX_FILE_BYTES, MAX_ID_BYTES, MAX_INPUT_BYTES, MAX_PATH_BYTES, MAX_ROWS,
    MAX_ROW_ID_BYTES,
};
use serde_json::Value;

fn fixtures() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/long-profile.v1.json"
    ))
    .expect("fixture file");
    let f: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(f["version"], 1);
    let l = &f["limits"];
    assert_eq!(l["maxInputBytes"], MAX_INPUT_BYTES);
    assert_eq!(l["maxFileBytes"], MAX_FILE_BYTES);
    assert_eq!(l["maxIdBytes"], MAX_ID_BYTES);
    assert_eq!(l["maxRowIdBytes"], MAX_ROW_ID_BYTES);
    assert_eq!(l["maxPathBytes"], MAX_PATH_BYTES);
    assert_eq!(l["maxRows"], MAX_ROWS);
    f
}

#[test]
fn cases() {
    let f = fixtures();
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 90);
    let mut seen = std::collections::BTreeMap::new();
    for c in cases {
        let (status, reply) = run_json(&c["input"].to_string());
        assert_eq!(status, 0, "{}", c["name"]);
        let got: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(got, c["expect"], "{}", c["name"]);
        if let Some(r) = got["reason"].as_str() {
            *seen.entry(r.to_owned()).or_insert(0) += 1;
        }
    }
    for r in [
        "invalid_id",
        "is_long",
        "ambiguous",
        "no_base",
        "no_model",
        "bad_path",
        "high",
        "low",
        "no_long",
    ] {
        assert!(seen.get(r).copied().unwrap_or(0) >= 1, "{r}: {seen:?}");
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
        let (status, reply) = run_json(&text);
        assert_eq!(status, 1, "{}", e["name"]);
        let got: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(got, e["expect"], "{}", e["name"]);
    }
}

/// The base section `[a]` (model = /f) followed by `[b]` holding one comment of `k` bytes; the
/// long section adds `\n[a-long]\nmodel = /f\n` (21 bytes) to its 22 + k bytes.
fn padded(k: usize) -> String {
    format!("[a]\nmodel = /f\n[b]\n; {}\n", "p".repeat(k))
}

fn section_request(text: &str) -> String {
    serde_json::json!({"op": "section", "text": text, "base": "a", "model": null, "mmproj": null})
        .to_string()
}

#[test]
fn the_reply_may_be_exactly_the_file_limit() {
    let (status, reply) = run_json(&section_request(&padded(MAX_FILE_BYTES - 43)));
    assert_eq!(status, 0);
    let got: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(got["ok"], true);
    assert_eq!(got["text"].as_str().unwrap().len(), MAX_FILE_BYTES);
    let (_, over) = run_json(&section_request(&padded(MAX_FILE_BYTES - 42)));
    assert_eq!(over, r#"{"ok":false,"reason":"too_large"}"#);
}

#[test]
fn a_text_over_the_file_limit_is_refused() {
    let (status, reply) = run_json(&section_request(&"x".repeat(MAX_FILE_BYTES + 1)));
    assert_eq!((status, reply.as_str()), (1, r#"{"error":"too_large"}"#));
}
