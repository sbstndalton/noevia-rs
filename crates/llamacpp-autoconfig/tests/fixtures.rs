//! Every row of tests/fixtures/llamacpp-autoconfig.v1.json (printed by noevia-core's
//! tools/gen-llamacpp-autoconfig-fixtures.cjs from the JS itself; byte-identical to core's copy)
//! through the wasm call's wire format: the exact reply (JSON.stringify of the JS answer) for
//! suggest, estimateInputs, estimateFootprint and the helpers, `input` where the JS throws and
//! `ambiguous` for the stricter rows.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use llamacpp_autoconfig::call;
use prompt_framing::json::{self, Value};

fn fixtures() -> Value {
    json::parse_utf8(include_bytes!("fixtures/llamacpp-autoconfig.v1.json"), 16)
        .expect("fixture JSON")
}

fn rows<'a>(f: &'a Value, key: &str) -> &'a [Value] {
    let Some(Value::Arr(rows)) = f.get(key) else {
        panic!("no {key}")
    };
    rows
}

fn s(v: &Value, key: &str) -> String {
    String::from_utf16(v.get(key).and_then(Value::as_str).unwrap()).unwrap()
}

fn op(v: &Value) -> u8 {
    let Some(Value::Num(n)) = v.get("op") else {
        panic!("no op")
    };
    *n as u8
}

fn run(op: u8, wire: &str) -> (u32, String) {
    let mut input = vec![op];
    input.extend(wire.as_bytes());
    call(&input)
}

#[test]
fn answers_are_byte_identical() {
    let f = fixtures();
    for (key, code, min) in [
        ("suggest", 1u8, 500),
        ("inputs", 2, 150),
        ("footprint", 3, 400),
    ] {
        let rows = rows(&f, key);
        assert!(rows.len() >= min, "{key}");
        for (i, row) in rows.iter().enumerate() {
            let (status, reply) = run(code, &s(row, "wire"));
            assert_eq!(status, 0, "{key} row {i}: {reply}");
            assert_eq!(reply, s(row, "reply"), "{key} row {i}: {}", s(row, "wire"));
        }
    }
    // The suggestion table holds every kind of answer.
    let replies: Vec<String> = rows(&f, "suggest").iter().map(|r| s(r, "reply")).collect();
    let values = replies
        .iter()
        .filter(|r| r.starts_with(r#"{"values""#))
        .count();
    assert!(values >= 100, "{values}");
    for needle in [
        "No inference memory budget",
        "chat models only",
        "Cannot size the KV cache",
        "mixture-of-experts",
        "below the smallest supported",
        "before any context",
        "draft-mtp",
        r#""spec-type":"none""#,
        r#""batch-size""#,
        "trained maximum",
        "an unknown number of",
    ] {
        assert!(replies.iter().any(|r| r.contains(needle)), "{needle}");
    }
}

#[test]
fn helper_rows_are_byte_identical() {
    let f = fixtures();
    let rows = rows(&f, "helpers");
    assert!(rows.len() >= 60);
    for (i, row) in rows.iter().enumerate() {
        let (status, reply) = run(op(row), &s(row, "wire"));
        assert_eq!(status, 0, "helper row {i}: {reply}");
        assert_eq!(reply, s(row, "reply"), "helper row {i}: {}", s(row, "wire"));
    }
}

#[test]
fn throws_and_strict_rows_are_refused() {
    let f = fixtures();
    for (key, want) in [
        ("throws", r#"{"error":"input"}"#),
        ("strict", r#"{"error":"ambiguous"}"#),
    ] {
        let rows = rows(&f, key);
        assert!(rows.len() >= 8, "{key}");
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(
                run(op(row), &s(row, "wire")),
                (1, want.to_owned()),
                "{key} row {i}: {}",
                s(row, "wire")
            );
        }
    }
}
