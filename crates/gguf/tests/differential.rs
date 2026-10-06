//! Differential corpus: every fixture in tests/fixtures was summarised by noevia's reference
//! gguf_meta.py (tools/gen-fixtures.py). The Rust summary must match it field for field.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use gguf::{read_raw, read_raw_bytes, summarize};
use serde_json::Value as J;

/// Ok(summary JSON text) or Err((kind, message)), kind being "GgufMetaError" for a parse
/// failure and "Summary" for a summarize() failure.
fn run(path: &std::path::Path, bytes: &[u8]) -> Result<String, (&'static str, String)> {
    let raw = read_raw(path).map_err(|e| ("GgufMetaError", e.0))?;
    let raw_b = read_raw_bytes(bytes).map_err(|e| ("GgufMetaError", e.0))?;
    // Debug text so NaN values compare equal to themselves.
    let dbg = |r: &gguf::Raw| -> std::collections::BTreeMap<String, String> {
        r.iter()
            .map(|(k, v)| (k.clone(), format!("{v:?}")))
            .collect()
    };
    assert_eq!(dbg(&raw), dbg(&raw_b), "file and in-memory parse differ");
    summarize(&raw)
        .map(|j| j.to_string())
        .map_err(|e| ("Summary", e.0))
}

#[test]
fn every_fixture_matches_the_python_reference() {
    let fixtures = common::fixtures();
    assert!(fixtures.len() >= 60, "fixture corpus missing");
    let mut failures = Vec::new();
    for (name, path, bytes) in &fixtures {
        let expected_text = std::fs::read_to_string(path.with_extension("json")).unwrap();
        let expected: J = serde_json::from_str(&expected_text).unwrap();
        match (run(path, bytes), expected.get("__error__")) {
            (Ok(text), None) => {
                let actual: J = serde_json::from_str(&text)
                    .unwrap_or_else(|e| panic!("{name}: invalid JSON {e}: {text}"));
                if actual != expected {
                    failures.push(format!("{name}:\n  rust:   {actual}\n  python: {expected}"));
                }
            }
            (Err((kind, msg)), Some(err)) => {
                let want_kind = err["kind"].as_str().unwrap();
                let parse_err = want_kind == "GgufMetaError";
                if parse_err != (kind == "GgufMetaError") {
                    failures.push(format!("{name}: error kind {kind} vs python {want_kind}"));
                } else if parse_err && err["message"].as_str() != Some(msg.as_str()) {
                    failures.push(format!("{name}: message {msg:?} vs python {err}"));
                }
            }
            (Ok(text), Some(err)) => {
                failures.push(format!("{name}: rust ok ({text}) but python raised {err}"))
            }
            (Err(e), None) => failures.push(format!("{name}: rust raised {e:?}, python ok")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn huge_integers_match_python_digit_for_digit() {
    // serde_json compares integers past u64 as floats, so check the exact digits here.
    let dir = common::fixtures_dir();
    let expected = std::fs::read_to_string(dir.join("floats_to_ints.json")).unwrap();
    let bytes = std::fs::read(dir.join("floats_to_ints.gguf")).unwrap();
    let actual = summarize(&read_raw_bytes(&bytes).unwrap())
        .unwrap()
        .to_string();
    for key in ["context_length", "block_count"] {
        let line = expected
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("\"{key}\":")))
            .unwrap();
        let digits = line.split(':').nth(1).unwrap().trim().trim_end_matches(',');
        assert!(digits.len() > 30, "{key}: {digits}");
        assert!(
            actual.contains(&format!("\"{key}\":{digits}")),
            "{key}: want {digits} in {actual}"
        );
    }
}
