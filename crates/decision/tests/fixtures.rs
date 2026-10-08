//! Every row of tests/fixtures/decision.v1.json (printed by noevia-core's
//! tools/gen-decision-fixtures.cjs from the JS itself, over the host's own projection;
//! byte-identical to core's copy) through the wasm call's wire format and the library: the same
//! invalidRequest / invalidResult message (a refusal where the JS throws) and the same causeOf code;
//! the strict rows are refused.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use decision::{call, decode, invalid_request};
use prompt_framing::json::{self, Value};

fn fixtures() -> Value {
    json::parse_utf8(include_bytes!("fixtures/decision.v1.json"), 16).expect("fixture JSON")
}

fn rows<'a>(f: &'a Value, key: &str) -> &'a [Value] {
    let Some(Value::Arr(rows)) = f.get(key) else {
        panic!("no {key}")
    };
    rows
}

fn wire_text(row: &Value) -> String {
    String::from_utf16(row.get("wire").and_then(Value::as_str).unwrap()).unwrap()
}

fn input(op: u8, row: &Value) -> Vec<u8> {
    let mut out = vec![op];
    out.extend(wire_text(row).as_bytes());
    out
}

/// The reply the JS's answer `want` corresponds to.
fn expected(key: &str, want: &Value) -> (u32, String) {
    match want {
        Value::Obj(_) if want.get("throws").is_some() => (1, r#"{"error":"throws"}"#.to_owned()),
        Value::Null => (0, format!("{{\"{key}\":null}}")),
        Value::Str(s) => (
            0,
            format!("{{\"{key}\":\"{}\"}}", String::from_utf16(s).unwrap()),
        ),
        _ => panic!("want shape"),
    }
}

fn check(section: &str, op: u8, key: &str, min: usize) -> (usize, usize) {
    let f = fixtures();
    let rows = rows(&f, section);
    assert!(rows.len() >= min, "{section}: {} rows", rows.len());
    let (mut valid, mut throws) = (0, 0);
    for (i, row) in rows.iter().enumerate() {
        let want = row.get("want").unwrap();
        let exp = expected(key, want);
        assert_eq!(
            call(&input(op, row)),
            exp,
            "{section} row {i}: {}",
            wire_text(row)
        );
        valid += usize::from(*want == Value::Null);
        throws += usize::from(exp.0 == 1);
    }
    (valid, throws)
}

#[test]
fn request_rows() {
    let (valid, throws) = check("request", 1, "invalid", 150);
    assert!(
        valid >= 20 && throws == 0,
        "{valid} valid, {throws} throwing"
    );
    // The library agrees with the wire.
    let f = fixtures();
    for row in rows(&f, "request") {
        let tree = json::parse_utf8(wire_text(row).as_bytes(), 16).unwrap();
        let got = invalid_request(&decode(&tree).unwrap()).unwrap();
        let want = row.get("want").unwrap();
        assert_eq!(
            got.map(|m| Value::Str(m.encode_utf16().collect()))
                .unwrap_or(Value::Null),
            *want
        );
    }
}

#[test]
fn result_rows() {
    let (valid, throws) = check("result", 2, "invalid", 900);
    assert!(
        valid >= 30 && throws >= 50,
        "{valid} valid, {throws} throwing"
    );
}

#[test]
fn cause_rows() {
    check("cause", 3, "cause", 30);
}

#[test]
fn strict_rows_are_refused() {
    let f = fixtures();
    let rows = rows(&f, "strict");
    assert!(rows.len() >= 7);
    for (i, row) in rows.iter().enumerate() {
        let Some(Value::Num(op)) = row.get("op") else {
            panic!()
        };
        let (status, reply) = call(&input(*op as u8, row));
        assert_eq!(
            (status, reply.as_str()),
            (1, r#"{"error":"opaque"}"#),
            "strict row {i}"
        );
    }
}
