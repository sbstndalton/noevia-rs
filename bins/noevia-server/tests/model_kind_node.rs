//! sbstndalton/noevia#2584: the front's model_kind.rs answers as core's chat-model-kind.cjs
//! `isChatGenerationModel` and model-system.cjs `isSystemModel` (roles file read beside
//! models.ini) on every recorded case. tests/fixtures/model-kind-node.json is written by
//! `node tools/gen-model-kind-fixtures.cjs <noevia-core> <out>` (synthetic names only).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use noevia_server::model_kind;
use serde_json::Value;

fn bytes_of(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn every_case_answers_as_core() {
    let f: Value = serde_json::from_str(include_str!("fixtures/model-kind-node.json")).unwrap();
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() > 400, "{}", cases.len());
    let mut failures = Vec::new();
    for c in cases {
        let dir = tempfile::tempdir().unwrap();
        if let Some(hex) = c["roles"].as_str() {
            std::fs::write(dir.path().join("models.roles.json"), bytes_of(hex)).unwrap();
        }
        let roles = model_kind::roles_beside(Some(&dir.path().join("models.ini")));
        let name = c["name"].as_str().unwrap();
        if let Some(want) = c["system"].as_bool() {
            let got = match c["path"].as_str() {
                Some(p) => model_kind::is_system_model_at(&roles, name, Some(p)),
                None => model_kind::is_system_model(&roles, name),
            };
            if got != want {
                failures.push(format!("system {c}: got {got}"));
            }
        }
        if let Some(want) = c["chat"].as_bool() {
            let labels: Vec<String> = c["labels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l.as_str().unwrap().to_string())
                .collect();
            let got = model_kind::is_chat_generation_model(&roles, name, Some(&labels));
            if got != want {
                failures.push(format!("chat {c}: got {got}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
