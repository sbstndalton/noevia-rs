//! Every row of tests/fixtures/code-actions.v1.json (printed by noevia-core's
//! tools/gen-code-actions-fixtures.cjs from the JS itself; byte-identical to core's copy) through the
//! wasm call's wire format: the same reply text as JSON.stringify of the JS answer, or the refusal
//! the crate documents for a strict row.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use code_actions::call;
use prompt_framing::json::{self, Value};

fn fixtures() -> Value {
    json::parse_utf8(include_bytes!("fixtures/code-actions.v1.json"), 64).expect("fixture JSON")
}

fn text(v: &Value) -> String {
    String::from_utf16(v.as_str().unwrap()).unwrap()
}

fn check(section: &str, op: u8, min: usize) {
    let f = fixtures();
    let Some(Value::Arr(rows)) = f.get(section) else {
        panic!("no {section}")
    };
    assert!(rows.len() >= min, "{section}: {} rows", rows.len());
    let mut strict = 0;
    for (i, row) in rows.iter().enumerate() {
        let mut input = vec![op];
        input.extend(text(row.get("wire").unwrap()).as_bytes());
        let (status, reply) = call(&input);
        let reply = String::from_utf8(reply).unwrap();
        if let Some(want) = row.get("want") {
            assert_eq!(status, 0, "{section} {i}: {reply}");
            assert_eq!(reply, text(want), "{section} {i}");
        } else {
            strict += 1;
            let refused = text(row.get("refused").unwrap());
            assert_eq!(status, 1, "{section} {i}: {reply}");
            assert_eq!(
                reply,
                format!("{{\"error\":\"{refused}\"}}"),
                "{section} {i}"
            );
        }
    }
    eprintln!("{section}: {} rows, {strict} strict", rows.len());
}

#[test]
fn classify_rows() {
    check("classify", 1, 1000);
}

#[test]
fn decide_rows() {
    check("decide", 2, 3000);
}

#[test]
fn pick_rows() {
    check("pick", 3, 150);
}
