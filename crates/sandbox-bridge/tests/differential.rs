//! Differential fixtures (noevia#999): every case in `fixtures/sandbox-bridge.v1.json` was
//! evaluated by noevia-core's JS references (`code-sandbox/pi-acp-bridge.cjs` lines/toolCallFor,
//! `code-sandbox/supervisor.cjs` parseStartJs/containedJs); the Rust port must agree on all of
//! them. The file is byte-identical to noevia-core's `tests/fixtures/sandbox-bridge.v1.json`
//! (noevia-core CI compares the two). Outputs are compared as the bytes `JSON.stringify` would put
//! on the wire.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use sandbox_bridge::contain::contained;
use sandbox_bridge::frame::{Framer, Pushed};
use sandbox_bridge::json::{js_stringify, parse};
use sandbox_bridge::start::start_json;
use sandbox_bridge::tool_call::{tool_call_json, Error};
use serde_json::Value;

fn fixtures() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sandbox-bridge.v1.json"
    ))
    .expect("fixture file");
    let f: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(f["version"], 1);
    f
}

fn cases<'a>(f: &'a Value, key: &str, min: usize) -> &'a Vec<Value> {
    let c = f[key].as_array().expect("cases");
    assert!(c.len() >= min, "{key} fixtures shrank to {}", c.len());
    c
}

fn report(what: &str, total: usize, bad: &[String]) {
    assert!(
        bad.is_empty(),
        "{what}: {} of {total} fixtures disagree:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

fn canonical(json: &str) -> String {
    js_stringify(&parse(json).expect("Rust emitted invalid JSON")).expect("stringify")
}

#[test]
fn framing_agrees_with_js() {
    let f = fixtures();
    let cases = cases(&f, "frame", 300);
    let mut bad = Vec::new();
    for case in cases {
        let mut framer = Framer::new(case["limit"].as_u64().unwrap() as usize);
        let chunks = case["chunks"].as_array().unwrap();
        let expect = case["expect"].as_array().unwrap();
        assert_eq!(chunks.len(), expect.len());
        for (i, (chunk, want)) in chunks.iter().zip(expect).enumerate() {
            let got = match framer.push(chunk.as_str().unwrap()) {
                Pushed::Lines(lines) => serde_json::json!({ "lines": lines }),
                Pushed::Overflow(size) => serde_json::json!({ "overflow": size }),
            };
            if got != *want {
                bad.push(format!(
                    "{} chunk {i}: expected {want} got {got}",
                    case["name"]
                ));
            }
        }
    }
    report("frame", cases.len(), &bad);
}

#[test]
fn tool_call_agrees_with_js() {
    let f = fixtures();
    let cases = cases(&f, "toolCall", 400);
    let mut bad = Vec::new();
    for case in cases {
        let payload = case["payload"].as_str().unwrap();
        let got = match tool_call_json(payload) {
            Ok(json) => serde_json::json!({ "json": canonical(&json) }),
            Err(Error::TypeError) => serde_json::json!({ "error": "type_error" }),
            Err(e) => serde_json::json!({ "error": e.code() }),
        };
        if got != case["expect"] {
            bad.push(format!(
                "{}: expected {} got {got}",
                case["name"], case["expect"]
            ));
        }
    }
    report("toolCall", cases.len(), &bad);
}

#[test]
fn start_agrees_with_js() {
    let f = fixtures();
    let cases = cases(&f, "start", 200);
    let mut bad = Vec::new();
    for case in cases {
        let got = canonical(&start_json(case["line"].as_str().unwrap()));
        if got != case["expect"].as_str().unwrap() {
            bad.push(format!(
                "{}: expected {} got {got}",
                case["name"], case["expect"]
            ));
        }
    }
    report("start", cases.len(), &bad);
}

#[test]
fn containment_agrees_with_js() {
    let f = fixtures();
    let cases = cases(&f, "contained", 500);
    let mut bad = Vec::new();
    for case in cases {
        let (ok, rel) = contained(
            case["root"].as_str().unwrap(),
            case["resolved"].as_str().unwrap(),
        );
        let got = serde_json::json!({ "contained": ok, "rel": rel });
        if got != case["expect"] {
            bad.push(format!(
                "{}: expected {} got {got}",
                case["name"], case["expect"]
            ));
        }
    }
    report("contained", cases.len(), &bad);
}
