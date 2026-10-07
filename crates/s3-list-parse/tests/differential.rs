//! Differential fixtures (noevia#976): every case in `fixtures/s3-list.v1.json` was evaluated by
//! noevia-core's JS reference (`server/s3-listing.cjs`, `s3PageRecordsJs`); the Rust port must
//! agree on all of them. The file is byte-identical to noevia-core's
//! `tests/fixtures/s3-list.v1.json` (noevia-core CI compares the two).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use s3_list_parse::{parse_page, reply_json};
use serde_json::{json, Value};

fn fixtures() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/s3-list.v1.json"
    ))
    .expect("fixture file");
    serde_json::from_str(&text).expect("fixture JSON")
}

fn expected(case: &Value) -> Value {
    let e = &case["expect"];
    json!({ "entries": e["records"], "truncated": e["truncated"], "next": e["next"] })
}

#[test]
fn agrees_with_js_reference_on_every_fixture() {
    let f = fixtures();
    assert_eq!(f["version"], 1);
    let cases = f["cases"].as_array().expect("cases");
    assert!(
        cases.len() >= 500,
        "fixture table shrank to {}",
        cases.len()
    );
    let mut disagreements = Vec::new();
    for case in cases {
        let body = case["body"].as_str().expect("body");
        let prefix = case["prefix"].as_str().expect("prefix");
        let got: Value =
            serde_json::from_str(&reply_json(&parse_page(body, prefix))).expect("valid JSON");
        if got != expected(case) {
            disagreements.push(format!(
                "{}: expected {} got {}",
                case["name"],
                expected(case),
                got
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
