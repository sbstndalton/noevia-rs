//! Differential fixtures (noevia#977): every case in `fixtures/upload-sniff.v1.json` was evaluated
//! by noevia-core's JS reference (`server/upload-sniff.cjs`); the Rust port must agree on all of
//! them, including the exact decoded text. The file is byte-identical to noevia-core's
//! `tests/fixtures/upload-sniff.v1.json` (noevia-core CI compares the two).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use serde_json::{json, Value};

fn fixtures() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/upload-sniff.v1.json"
    ))
    .expect("fixture file");
    let f: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(f["version"], 1);
    assert_eq!(f["cap"], upload_sniff::CAP);
    f
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

fn report(kind: &str, total: usize, bad: Vec<String>) {
    assert!(
        bad.is_empty(),
        "{} of {total} {kind} fixtures disagree:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[test]
fn validate_agrees_with_js_reference() {
    let f = fixtures();
    let cases = f["validate"].as_array().expect("validate");
    assert!(
        cases.len() >= 1500,
        "validate table shrank to {}",
        cases.len()
    );
    let mut bad = Vec::new();
    for c in cases {
        let name = c["name"].as_str().expect("name");
        let len = c["len"].as_u64().expect("len");
        let head = unhex(c["head"].as_str().expect("head"));
        let got: Value =
            serde_json::from_str(&upload_sniff::validate_json(name, len, &head)).expect("JSON");
        if got["value"] != c["expect"] {
            bad.push(format!(
                "{name:?} len {len}: expected {} got {}",
                c["expect"], got
            ));
        }
    }
    report("validate", cases.len(), bad);
}

#[test]
fn classify_agrees_with_js_reference() {
    let f = fixtures();
    let cases = f["classify"].as_array().expect("classify");
    assert!(
        cases.len() >= 1500,
        "classify table shrank to {}",
        cases.len()
    );
    let mut bad = Vec::new();
    for c in cases {
        let name = c["name"].as_str().expect("name");
        if upload_sniff::classify(name) != c["expect"] {
            bad.push(format!("{name:?}: expected {}", c["expect"]));
        }
    }
    report("classify", cases.len(), bad);
}

#[test]
fn decode_agrees_with_js_reference_byte_for_byte() {
    let f = fixtures();
    let cases = f["decode"].as_array().expect("decode");
    assert!(
        cases.len() >= 3000,
        "decode table shrank to {}",
        cases.len()
    );
    let mut bad = Vec::new();
    for c in cases {
        let bytes = unhex(c["bytes"].as_str().expect("bytes"));
        let got = match upload_sniff::decode_text(&bytes).expect("in cap") {
            None => Value::Null,
            Some((text, enc)) => json!({ "encoding": enc.name(), "text": text }),
        };
        if got != c["expect"] {
            bad.push(format!(
                "{}: expected {} got {}",
                c["name"], c["expect"], got
            ));
        }
        // The wire form carries the same text after its tag byte.
        let wire = upload_sniff::decode_reply(&bytes).expect("in cap");
        match &c["expect"] {
            Value::Null => assert_eq!(wire, vec![0], "{}", c["name"]),
            e => assert_eq!(
                std::str::from_utf8(&wire[1..]).expect("utf-8"),
                e["text"].as_str().expect("text"),
                "{}",
                c["name"]
            ),
        }
    }
    report("decode", cases.len(), bad);
}
