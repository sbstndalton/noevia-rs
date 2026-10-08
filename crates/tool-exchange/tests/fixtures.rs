//! Every row of tests/fixtures/tool-exchange.v1.json (printed by noevia-core's
//! tools/gen-tool-exchange-fixtures.cjs from the JS itself; byte-identical to core's copy) through
//! the library and the wasm call's wire format: the same answer or the same dedupe key, unit for
//! unit, and the same failed-call text.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::json::{self, Value};
use tool_exchange::{call, call_error, check, Check};

fn fixtures() -> Value {
    json::parse_utf8(include_bytes!("fixtures/tool-exchange.v1.json"), 16).expect("fixture JSON")
}

fn le(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

fn framed(units: &[u16]) -> Vec<u8> {
    let mut out = (units.len() as u32).to_le_bytes().to_vec();
    out.extend(le(units));
    out
}

fn reply(tag: u8, units: &[u16]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(le(units));
    out
}

fn s(v: Option<&Value>) -> Vec<u16> {
    v.and_then(Value::as_str).expect("string").to_vec()
}

#[test]
fn check_rows() {
    let f = fixtures();
    let Some(Value::Arr(rows)) = f.get("check") else {
        panic!()
    };
    assert!(rows.len() > 150);
    let (mut keys, mut answers) = (0, 0);
    for (i, row) in rows.iter().enumerate() {
        let aborted = row.get("aborted") == Some(&Value::Bool(true));
        let allowed = row.get("allowed") == Some(&Value::Bool(true));
        let name = s(row.get("name"));
        let args = match row.get("args") {
            Some(Value::Null) => None,
            v => Some(s(v)),
        };
        let want = row.get("want").unwrap();
        let got = check(aborted, allowed, &name, args.as_deref()).unwrap();
        let (tag, text) = match (&got, want.get("key"), want.get("answer")) {
            (Check::Run(k), Some(Value::Str(w)), None) => {
                keys += 1;
                assert_eq!(k, w, "row {i}: key");
                (0, k)
            }
            (Check::Answer(a), None, Some(Value::Str(w))) => {
                answers += 1;
                assert_eq!(a, w, "row {i}: answer");
                (1, a)
            }
            _ => panic!("row {i}: {got:?} vs the JS's {want:?}"),
        };
        let mut input = vec![1, u8::from(aborted), u8::from(allowed)];
        input.extend(framed(&name));
        match &args {
            None => input.push(0),
            Some(a) => {
                input.push(1);
                input.extend(le(a));
            }
        }
        assert_eq!(call(&input), (0, reply(tag, text)), "row {i}: wire");
    }
    assert!(keys > 40 && answers > 80);
}

#[test]
fn error_rows() {
    let f = fixtures();
    let Some(Value::Arr(rows)) = f.get("error") else {
        panic!()
    };
    assert!(rows.len() > 50);
    for (i, row) in rows.iter().enumerate() {
        let (name, message, want) = (
            s(row.get("name")),
            s(row.get("message")),
            s(row.get("want")),
        );
        assert_eq!(call_error(&name, &message).unwrap(), want, "row {i}");
        let mut input = vec![2];
        input.extend(framed(&name));
        input.extend(framed(&message));
        assert_eq!(call(&input), (0, reply(1, &want)), "row {i}: wire");
    }
}
