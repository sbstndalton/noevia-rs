//! Every row of tests/fixtures/project-edit-target.v1.json (printed by noevia-core's
//! tools/gen-project-edit-target-fixtures.cjs from the JS itself; byte-identical to core's copy)
//! through the wasm call's wire format: the exact reply text.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use project_edit_target::call;
use prompt_framing::json::{self, Value};

#[test]
fn rows() {
    let f = json::parse_utf8(include_bytes!("fixtures/project-edit-target.v1.json"), 64)
        .expect("fixture JSON");
    let Some(Value::Arr(rows)) = f.get("rows") else {
        panic!("no rows")
    };
    assert!(rows.len() >= 7000);
    let (mut plans, mut adopts, mut failed) = (0, 0, Vec::new());
    for (i, row) in rows.iter().enumerate() {
        // Lone surrogates cross as JSON escapes, so the wire text is ASCII-safe UTF-16.
        let wire = String::from_utf16_lossy(row.get("wire").unwrap().as_str().unwrap());
        let mut input = vec![1u8];
        input.extend(wire.as_bytes());
        let (status, reply) = call(&input);
        let reply = String::from_utf8(reply).unwrap();
        let want = String::from_utf16_lossy(row.get("want").unwrap().as_str().unwrap());
        if status != 0 || reply != want {
            failed.push(format!(
                "row {i}: {}\n  got  {reply}\n  want {want}",
                &wire[..wire.len().min(300)]
            ));
        }
        if want.starts_with("{\"plan\"") {
            plans += 1;
            if want.ends_with("\"adopt\":true}}") {
                adopts += 1;
            }
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
    assert!(
        plans > 1000 && adopts > 200,
        "{plans} plans, {adopts} adopt"
    );
}
