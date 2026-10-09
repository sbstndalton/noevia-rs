//! Every row of tests/fixtures/completeness-report.v1.json (printed by noevia-core's
//! tools/gen-completeness-report-fixtures.cjs from the JS itself; byte-identical to core's copy)
//! through the wasm call's wire format: the exact reply (canonical report JSON and its sha256),
//! the same reportHash refusal for over-deep reports, `input` where the JS throws and `ambiguous`
//! for the stricter rows.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use completeness_report::{build, call, Status};
use prompt_framing::json::{self, Value};

fn fixtures() -> Value {
    json::parse_utf8(include_bytes!("fixtures/completeness-report.v1.json"), 16)
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

fn run(row: &Value) -> (u32, String) {
    let mut input = vec![1u8];
    input.extend(s(row, "wire").as_bytes());
    call(&input)
}

#[test]
fn report_rows_are_byte_identical() {
    let f = fixtures();
    let rows = rows(&f, "reports");
    assert!(rows.len() >= 400);
    let (mut pass, mut fail, mut unknown) = (0, 0, 0);
    for (i, row) in rows.iter().enumerate() {
        let (status, reply) = run(row);
        assert_eq!(status, 0, "row {i}: {reply}");
        assert_eq!(reply, s(row, "reply"), "row {i}");
        // The library agrees with the wire, and canEnterReviewing is "overall pass".
        let wire = json::parse_utf8(s(row, "wire").as_bytes(), 80).unwrap();
        let Value::Arr(parts) = wire else { panic!() };
        let r = build(&parts[0], &parts[1]).unwrap();
        assert_eq!(
            r.can_enter_reviewing(),
            r.overall == Status::Pass,
            "row {i}"
        );
        match r.overall {
            Status::Pass => pass += 1,
            Status::Fail => fail += 1,
            Status::Unknown => unknown += 1,
        }
    }
    assert!(
        pass >= 40 && fail >= 40 && unknown >= 40,
        "{pass} {fail} {unknown}"
    );
}

#[test]
fn deep_rows_refuse_to_hash() {
    let f = fixtures();
    let rows = rows(&f, "deep");
    assert!(rows.len() >= 5);
    for (i, row) in rows.iter().enumerate() {
        let (status, reply) = run(row);
        assert_eq!(status, 0, "row {i}: {reply}");
        assert_eq!(reply, s(row, "reply"), "row {i}");
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
            assert_eq!(run(row), (1, want.to_owned()), "{key} row {i}");
        }
    }
}
