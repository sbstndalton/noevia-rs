//! Every row of tests/fixtures/project-file-names.v1.json (printed by noevia-core's
//! tools/gen-project-file-names-fixtures.cjs from the JS itself; byte-identical to core's copy)
//! through the wasm call's wire format: the same file index, reason, candidates or refusal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use project_file_names::call;
use prompt_framing::json::{self, Value};

fn text(v: &Value) -> String {
    String::from_utf16(v.as_str().unwrap()).unwrap()
}

#[test]
fn rows() {
    let f = json::parse_utf8(include_bytes!("fixtures/project-file-names.v1.json"), 64)
        .expect("fixture JSON");
    let Some(Value::Arr(rows)) = f.get("rows") else {
        panic!("no rows")
    };
    assert!(rows.len() >= 450);
    for (i, row) in rows.iter().enumerate() {
        let mut input = vec![1u8];
        input.extend(text(row.get("wire").unwrap()).as_bytes());
        let (status, reply) = call(&input);
        let reply = String::from_utf8(reply).unwrap();
        match row.get("want") {
            Some(want) => {
                assert_eq!(status, 0, "row {i}: {reply}");
                assert_eq!(reply, text(want), "row {i}");
            }
            None => {
                assert_eq!(status, 1, "row {i}: {reply}");
                assert_eq!(reply, r#"{"error":"ambiguous"}"#, "row {i}");
            }
        }
    }
}
