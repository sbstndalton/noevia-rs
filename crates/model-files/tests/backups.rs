//! Shared fixtures (noevia#1021): `fixtures/model-backups.v1.json`, made by
//! `tools/gen-model-backups.py` from an independent Python reference of the specification and
//! copied byte-for-byte into noevia-services (model-manager/tests/fixtures/, where the same table
//! runs through the built `model-files backups`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use model_files::backups::{
    plan_json, KEEP_REVISIONS, KEEP_ROTATING, MAX_EXISTING, MAX_INPUT_BYTES, MAX_KEEP,
    MAX_NAME_BYTES,
};
use serde_json::Value;

#[test]
fn cases() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/model-backups.v1.json"
    ))
    .unwrap();
    let f: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(f["version"], 1);
    let l = &f["limits"];
    assert_eq!(l["keepRotating"], KEEP_ROTATING);
    assert_eq!(l["keepRevisions"], KEEP_REVISIONS);
    assert_eq!(l["maxKeep"], MAX_KEEP);
    assert_eq!(l["maxExisting"], MAX_EXISTING);
    assert_eq!(l["maxNameBytes"], MAX_NAME_BYTES);
    assert_eq!(l["maxInputBytes"], MAX_INPUT_BYTES);
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 180);
    for c in cases {
        let input = match c["input"].as_str() {
            Some(s) => s.to_owned(),
            None => format!(
                "{}{}",
                c["inputBase"].as_str().unwrap(),
                " ".repeat(c["pad"].as_u64().unwrap() as usize)
            ),
        };
        let got: Value = match plan_json(input.as_bytes()) {
            Ok(json) => serde_json::from_str(&json).unwrap(),
            Err(e) => serde_json::json!({ "error": e.code() }),
        };
        assert_eq!(got, c["expect"], "{}", c["name"]);
    }
}
