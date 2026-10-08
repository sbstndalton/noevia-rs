//! Every row of tests/fixtures/code-net-guard.v1.json (printed by noevia-core's
//! tools/gen-code-net-guard-fixtures.cjs from the JS itself; byte-identical to core's copy) through
//! the wasm call's wire format and the library: the same spec reading, the same kept lookup answers,
//! the same refusals; the stricter rows refuse where the JS does not; the strict rows are refused as
//! ambiguous.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use code_net_guard::{call, parse_spec, refuses, Refusal};
use prompt_framing::json::{self, Value};

fn fixtures() -> Value {
    json::parse_utf8(include_bytes!("fixtures/code-net-guard.v1.json"), 16).expect("fixture JSON")
}

fn rows<'a>(f: &'a Value, key: &str) -> &'a [Value] {
    let Some(Value::Arr(rows)) = f.get(key) else {
        panic!("no {key}")
    };
    rows
}

fn wire(op: u8, row: &Value) -> Vec<u8> {
    let w = String::from_utf16(row.get("wire").and_then(Value::as_str).unwrap()).unwrap();
    let mut input = vec![op];
    input.extend(w.as_bytes());
    input
}

fn reply(op: u8, row: &Value, i: usize) -> Value {
    let (status, reply) = call(&wire(op, row));
    assert_eq!(status, 0, "row {i}: {reply}");
    json::parse_utf8(reply.as_bytes(), 16).unwrap()
}

fn strs(v: &Value) -> Vec<&[u16]> {
    let Value::Arr(xs) = v else { panic!() };
    xs.iter().map(|x| x.as_str().unwrap()).collect()
}

#[test]
fn spec_rows() {
    let f = fixtures();
    let rows = rows(&f, "spec");
    assert!(rows.len() >= 60);
    let mut malformed = 0;
    for (i, row) in rows.iter().enumerate() {
        let want = row.get("want").unwrap();
        assert_eq!(&reply(1, row, i), want, "row {i}");
        malformed += usize::from(want.get("malformed").is_some());
        // The library agrees with the wire.
        assert!(parse_spec(row.get("spec").unwrap().as_str().unwrap()).is_ok());
    }
    assert!(malformed >= 20 && malformed < rows.len() - 20);
}

#[test]
fn answers_and_refuses_rows() {
    let f = fixtures();
    for (i, row) in rows(&f, "answers").iter().enumerate() {
        assert_eq!(&reply(2, row, i), row.get("want").unwrap(), "row {i}");
    }
    let (mut yes, mut no) = (0, 0);
    for (i, row) in rows(&f, "refuses").iter().enumerate() {
        let want = row.get("want") == Some(&Value::Bool(true));
        assert_eq!(
            reply(3, row, i).get("refuses"),
            Some(&Value::Bool(want)),
            "row {i}"
        );
        let local = row.get("local").unwrap().as_str();
        assert_eq!(
            refuses(&strs(row.get("addresses").unwrap()), local),
            Ok(want),
            "row {i}"
        );
        if want {
            yes += 1
        } else {
            no += 1
        }
    }
    assert!(yes >= 8 && no >= 8, "{yes} refused, {no} served");
}

#[test]
fn stricter_rows_refuse_where_the_js_serves() {
    let f = fixtures();
    let rows = rows(&f, "stricter");
    assert!(rows.len() >= 5);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.get("js"), Some(&Value::Bool(false)));
        assert_eq!(
            reply(3, row, i).get("refuses"),
            Some(&Value::Bool(true)),
            "row {i}"
        );
    }
}

#[test]
fn strict_rows_are_refused() {
    let f = fixtures();
    let amb = (1, Refusal::Ambiguous.json().to_owned());
    for (key, op, min) in [
        ("strictSpec", 1, 20),
        ("strictAnswers", 2, 3),
        ("strictRefuses", 3, 3),
    ] {
        let rows = rows(&f, key);
        assert!(rows.len() >= min, "{key}");
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(call(&wire(op, row)), amb, "{key} row {i}");
        }
    }
}
