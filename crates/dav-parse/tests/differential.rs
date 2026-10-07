//! Differential fixtures (noevia#967): every case in `fixtures/dav-listing.v1.json` was evaluated
//! by noevia-core's JS reference (`server/dav-listing.cjs`, `listingRecordsJs`); the Rust port must
//! agree on all of them. The file is byte-identical to noevia-core's
//! `tests/fixtures/dav-listing.v1.json` (noevia-core CI compares the two).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use dav_parse::{list_entries, reply_json};
use serde_json::{json, Value};

fn fixtures() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/dav-listing.v1.json"
    ))
    .expect("fixture file");
    serde_json::from_str(&text).expect("fixture JSON")
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
        let target = case["target"].as_str().expect("target");
        let got: Value = match list_entries(body, target) {
            Ok(entries) => json!({ "entries": entries.iter().map(|e| json!({
                "name": e.name, "isDir": e.is_dir, "size": e.size,
            })).collect::<Vec<_>>() }),
            Err(e) => json!({ "error": e.code() }),
        };
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

#[test]
fn reply_json_round_trips_through_a_json_reader() {
    for case in fixtures()["cases"].as_array().expect("cases") {
        let result = list_entries(
            case["body"].as_str().expect("body"),
            case["target"].as_str().expect("target"),
        );
        let parsed: Value = serde_json::from_str(&reply_json(&result)).expect("valid JSON");
        assert_eq!(parsed, case["expect"], "{}", case["name"]);
    }
}
