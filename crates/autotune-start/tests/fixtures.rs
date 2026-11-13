//! Admission cases from `fixtures/autotune-start.v1.json` (hand-derived from noevia-core
//! `llamacpp-full-autotune.cjs` `start`; see the file's `source` field).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use autotune_start::{admit, Mode, Plan, Start};
use serde_json::Value;

fn cases() -> Vec<Value> {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/autotune-start.v1.json"
    ))
    .expect("fixture file");
    let f: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(f["version"], 1);
    f["cases"].as_array().expect("cases array").clone()
}

fn flag(input: &Value, key: &str) -> bool {
    input.get(key).and_then(Value::as_bool).unwrap_or(false)
}

#[test]
fn every_case_matches_the_node_admission() {
    let cases = cases();
    assert_eq!(cases.len(), 28, "fixture case count changed");
    for case in &cases {
        let name = case["name"].as_str().expect("name");
        let input = &case["input"];
        let start = Start {
            confirm_pause: input.get("confirmPause"),
            mode: input.get("mode"),
            prompt_budget_seconds: input.get("promptBudgetSeconds"),
            bulk: flag(input, "bulk"),
            run_active: flag(input, "runActive"),
            system_model: flag(input, "systemModel"),
        };
        let got = admit(&start);
        let want = &case["expect"];
        if let Some(ok) = want.get("ok") {
            let mode = match ok["mode"].as_str() {
                Some("fast") => Mode::Fast,
                Some("long") => Mode::Long,
                other => panic!("{name}: bad expected mode {other:?}"),
            };
            let want_plan = Plan {
                mode,
                prompt_budget_seconds: ok["promptBudgetSeconds"].as_u64().expect("seconds") as u32,
            };
            assert_eq!(got, Ok(want_plan), "{name}");
        } else {
            let refuse = &want["refuse"];
            let status = refuse["status"].as_u64().expect("status") as u16;
            let message = refuse["message"].as_str().expect("message");
            let got = got.map_err(|r| (r.status, r.message.to_owned()));
            assert_eq!(got, Err((status, message.to_owned())), "{name}");
        }
    }
}
