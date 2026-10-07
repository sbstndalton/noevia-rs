//! Differential fixtures (noevia#978): every case in `fixtures/storage-path.v1.json` was evaluated
//! by noevia-core's JS reference (`server/storage-path.cjs`); the Rust port must agree on all of
//! them. The file is byte-identical to noevia-core's `tests/fixtures/storage-path.v1.json`
//! (noevia-core CI compares the two).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use serde_json::Value;
use storage_path::{reply_json, Op};

#[test]
fn agrees_with_js_reference_on_every_fixture() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/storage-path.v1.json"
    ))
    .expect("fixture file");
    let f: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(f["version"], 1);
    let cases = f["cases"].as_array().expect("cases");
    assert!(
        cases.len() >= 500,
        "fixture table shrank to {}",
        cases.len()
    );
    let mut disagreements = Vec::new();
    for case in cases {
        let op = match case["op"].as_str().expect("op") {
            "safeRelativePath" => Op::SafeRelativePath,
            "cleanRoot" => Op::CleanRoot,
            "joinRoot" => Op::JoinRoot,
            "isPlainFilename" => Op::IsPlainFilename,
            other => panic!("unknown op {other}"),
        };
        let a = case["a"].as_str().expect("a");
        let b = case["b"].as_str().unwrap_or("");
        let (_, reply) = reply_json(op, a, b);
        let got: Value = serde_json::from_str(&reply).expect("valid JSON");
        if got != case["expect"] {
            disagreements.push(format!(
                "{}: expected {} got {}",
                case["name"], case["expect"], got
            ));
        }
    }
    assert!(
        disagreements.is_empty(),
        "{} of {} fixtures disagree:\n{}",
        disagreements.len(),
        cases.len(),
        disagreements.join("\n")
    );
}
