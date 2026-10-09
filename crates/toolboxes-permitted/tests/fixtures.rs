//! Every row of tests/fixtures/toolboxes-permitted.v1.json (printed by noevia-core's
//! tools/gen-toolboxes-permitted-fixtures.cjs from the JS itself; byte-identical to core's copy)
//! through the wasm call's wire format: the exact reply text. There are no `strict` rows: the port
//! is never stricter than the JS here (the host still only ever offers less with it).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::json::{self, Value};
use toolboxes_permitted::call;

fn text(v: &Value) -> String {
    String::from_utf16(v.as_str().unwrap()).unwrap()
}

#[test]
fn rows() {
    let f = json::parse_utf8(include_bytes!("fixtures/toolboxes-permitted.v1.json"), 64)
        .expect("fixture JSON");
    let Some(Value::Arr(rows)) = f.get("rows") else {
        panic!("no rows")
    };
    assert!(rows.len() >= 1500);
    let (mut strict, mut failed) = (0, Vec::new());
    for (i, row) in rows.iter().enumerate() {
        let Some(Value::Num(op)) = row.get("op") else {
            panic!("row {i}: no op")
        };
        let wire = text(row.get("wire").unwrap());
        let mut input = vec![*op as u8];
        input.extend(wire.as_bytes());
        let (status, reply) = call(&input);
        let reply = String::from_utf8(reply).unwrap();
        let want = text(row.get("want").unwrap());
        if status != 0 || reply != want {
            failed.push(format!(
                "row {i} op {op}: {wire}\n  got  {reply}\n  want {want}"
            ));
        }
        if row.get("strict").is_some() {
            strict += 1;
        }
    }
    assert!(
        failed.is_empty(),
        "{} rows differ:\n{}",
        failed.len(),
        failed
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(strict, 0, "the port is never stricter here");
}
