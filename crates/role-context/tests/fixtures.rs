//! Every row of tests/fixtures/role-context.v1.json (printed by noevia-core's
//! tools/gen-role-context-fixtures.cjs from the JS itself; byte-identical to core's copy) through
//! the wasm call's wire format: the exact reply the JS outcome maps to for the ASCII rows, a leak
//! for the Unicode disguises, `ambiguous` for lone surrogates.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::json::{self, Value};
use role_context::call;

fn fixtures() -> Value {
    json::parse_utf8(include_bytes!("fixtures/role-context.v1.json"), 8).expect("fixture JSON")
}

fn rows<'a>(f: &'a Value, key: &str) -> &'a [Value] {
    let Some(Value::Arr(rows)) = f.get(key) else {
        panic!("no {key}")
    };
    rows
}

fn text(row: &Value, key: &str) -> String {
    String::from_utf16(row.get(key).and_then(Value::as_str).unwrap()).unwrap()
}

fn wire(op: u8, row: &Value) -> Vec<u8> {
    let mut input = vec![op];
    input.extend(text(row, "wire").as_bytes());
    input
}

fn op_of(row: &Value) -> u8 {
    match row.get("op") {
        Some(Value::Num(n)) if *n == 1.0 => 1,
        Some(Value::Num(n)) if *n == 2.0 => 2,
        other => panic!("op {other:?}"),
    }
}

#[test]
fn project_and_dossier_rows_reply_exactly() {
    let f = fixtures();
    let mut kinds = [0usize; 3];
    for (key, op) in [("project", 1u8), ("dossier", 2)] {
        let rows = rows(&f, key);
        assert!(rows.len() >= 15, "{key}");
        for row in rows {
            let want = text(row, "reply");
            let (status, got) = call(&wire(op, row));
            assert_eq!(
                (status, got.as_str()),
                (0, want.as_str()),
                "{key} {}",
                text(row, "name")
            );
            let slot = if want.starts_with("{\"leak\"") {
                0
            } else if want.starts_with("{\"refused\"") {
                1
            } else {
                2
            };
            kinds[slot] += 1;
        }
    }
    let [leak, refused, ok] = kinds;
    assert!(leak >= 20 && refused >= 20 && ok >= 80, "{kinds:?}");
}

#[test]
fn unicode_disguises_are_refused_as_leaks() {
    let f = fixtures();
    let rows = rows(&f, "unicodeRefused");
    assert!(rows.len() >= 6);
    for row in rows {
        let (status, got) = call(&wire(op_of(row), row));
        assert_eq!(status, 0, "{}", text(row, "name"));
        assert!(
            got.starts_with("{\"leak\":["),
            "{}: {got}",
            text(row, "name")
        );
    }
}

#[test]
fn strict_rows_are_ambiguous() {
    let f = fixtures();
    let rows = rows(&f, "strict");
    assert!(rows.len() >= 5);
    for row in rows {
        assert_eq!(
            call(&wire(op_of(row), row)),
            (1, text(row, "reply")),
            "{}",
            text(row, "name")
        );
    }
}
