//! Every row of tests/fixtures/provider-egress.v1.json (printed by noevia-core's
//! tools/gen-provider-egress-fixtures.cjs from the JS itself; byte-identical to core's copy)
//! through the wasm call's wire format: the exact reply text. Rows marked `strict` hold the port's
//! deliberately stricter answer (see the crate docs); the generator checks they never let out more.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::json::{self, Value};
use provider_egress::call;

fn text(v: &Value) -> String {
    String::from_utf16(v.as_str().unwrap()).unwrap()
}

#[test]
fn rows() {
    let f = json::parse_utf8(include_bytes!("fixtures/provider-egress.v1.json"), 64)
        .expect("fixture JSON");
    let Some(Value::Arr(rows)) = f.get("rows") else {
        panic!("no rows")
    };
    assert!(rows.len() >= 4000);
    let mut strict = 0;
    for (i, row) in rows.iter().enumerate() {
        let Some(Value::Num(op)) = row.get("op") else {
            panic!("row {i}: no op")
        };
        let mut input = vec![*op as u8];
        input.extend(text(row.get("wire").unwrap()).as_bytes());
        let (status, reply) = call(&input);
        let reply = String::from_utf8(reply).unwrap();
        assert_eq!(status, 0, "row {i}: {reply}");
        assert_eq!(reply, text(row.get("want").unwrap()), "row {i}");
        if row.get("strict").is_some() {
            strict += 1;
        }
    }
    assert!(strict > 100, "{strict} strict rows");
}
