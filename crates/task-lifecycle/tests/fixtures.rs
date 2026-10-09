//! Every row of tests/fixtures/task-lifecycle.v1.json (printed by noevia-core's
//! tools/gen-task-lifecycle-fixtures.cjs from the JS itself; byte-identical to core's copy) through
//! the wasm call's wire format: the same table answers, stage moves and folds, the same throw codes;
//! the strict rows are refused as ambiguous.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::json::{self, Value};
use task_lifecycle::call;

fn fixtures() -> Value {
    json::parse_utf8(include_bytes!("fixtures/task-lifecycle.v1.json"), 64).expect("fixture JSON")
}

fn rows<'a>(f: &'a Value, key: &str) -> &'a [Value] {
    let Some(Value::Arr(rows)) = f.get(key) else {
        panic!("no {key}")
    };
    rows
}

fn ask(op: u8, row: &Value) -> (u32, Value) {
    let w = String::from_utf16(row.get("wire").and_then(Value::as_str).unwrap()).unwrap();
    let mut input = vec![op];
    input.extend(w.as_bytes());
    let (status, reply) = call(&input);
    (status, json::parse_utf8(reply.as_bytes(), 4).unwrap())
}

fn answered(op: u8, row: &Value, want: &Value, what: &str) {
    let (status, reply) = ask(op, row);
    assert_eq!(status, 0, "{what}: {reply:?}");
    assert_eq!(&reply, want, "{what}");
}

#[test]
fn pair_rows() {
    let f = fixtures();
    let rows = rows(&f, "pairs");
    assert!(rows.len() >= 200);
    for (i, row) in rows.iter().enumerate() {
        let want = row.get("want").unwrap();
        answered(
            1,
            row,
            want.get("canTransition").unwrap(),
            &format!("pair {i} canTransition"),
        );
        answered(
            2,
            row,
            want.get("transition").unwrap(),
            &format!("pair {i} transition"),
        );
        answered(
            3,
            row,
            want.get("stageMove").unwrap(),
            &format!("pair {i} stageMove"),
        );
    }
}

#[test]
fn fold_and_derive_rows() {
    let f = fixtures();
    let (folds, derive) = (rows(&f, "folds"), rows(&f, "derive"));
    assert!(folds.len() >= 250 && derive.len() >= 340);
    let mut thrown = 0;
    for (i, row) in folds.iter().enumerate() {
        answered(4, row, row.get("want").unwrap(), &format!("fold {i}"));
    }
    for (i, row) in derive.iter().enumerate() {
        let want = row.get("want").unwrap();
        thrown += usize::from(want.get("throws").is_some());
        answered(5, row, want, &format!("derive {i}"));
    }
    // Both outcomes are well represented.
    assert!(thrown >= 100 && thrown + 100 <= derive.len(), "{thrown}");
}

#[test]
fn strict_rows_refused() {
    let f = fixtures();
    let rows = rows(&f, "strict");
    assert!(rows.len() >= 5);
    let ambiguous = json::parse_utf8(br#"{"error":"ambiguous"}"#, 4).unwrap();
    for (i, row) in rows.iter().enumerate() {
        let (status, reply) = ask(5, row);
        assert_eq!(status, 1, "strict {i}");
        assert_eq!(
            &reply,
            row.get("want").map(|_| &ambiguous).unwrap(),
            "strict {i}"
        );
    }
}
