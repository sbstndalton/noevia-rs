//! Shared fixtures (noevia#1003): `fixtures/autotune-plan.v1.json` is byte-identical to
//! noevia-core's `tests/fixtures/autotune-plan.v1.json` (noevia-core CI compares the two) and is
//! made by noevia-core's `tools/gen-autotune-plan-fixtures.cjs`, whose expectations come from an
//! independent JavaScript reference of this planner (BigInt arithmetic). Each run is replayed step
//! by step through the JSON entry point, as dav-parse.wasm's `autotune_plan` sees it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use autotune_plan::{plan_json, MAX_INPUT_BYTES, MAX_LADDER, MAX_PROBES, MAX_RESULTS, MAX_VERIFY};
use serde_json::{json, Value};

fn fixtures() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/autotune-plan.v1.json"
    ))
    .expect("fixture file");
    let f: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(f["version"], 1);
    let l = &f["limits"];
    assert_eq!(l["maxInputBytes"], MAX_INPUT_BYTES);
    assert_eq!(l["maxLadder"], MAX_LADDER);
    assert_eq!(l["maxResults"], MAX_RESULTS);
    assert_eq!(l["maxProbes"], MAX_PROBES);
    assert_eq!(l["maxVerify"], MAX_VERIFY);
    f
}

fn result_of(expect: &Value, outcome: &Value) -> Value {
    match expect["step"].as_str().unwrap() {
        s @ ("probe" | "verify") => {
            json!({"step": s, "ctx": expect["ctx"], "kv": expect["kv"], "outcome": outcome})
        }
        "phase" => json!({"step": "phase", "id": expect["id"], "outcome": outcome}),
        "serving" => json!({"step": "serving", "outcome": outcome}),
        s => panic!("no result for {s}"),
    }
}

#[test]
fn runs() {
    let f = fixtures();
    let runs = f["runs"].as_array().unwrap();
    assert!(runs.len() >= 40);
    let mut steps = 0;
    for run in runs {
        let model = &f["models"][run["model"].as_str().unwrap()];
        let mut results: Vec<Value> = Vec::new();
        for (n, t) in run["trace"].as_array().unwrap().iter().enumerate() {
            let input = json!({
                "facts": model["facts"],
                "ladder": model["ladder"],
                "memory": f["memory"][run["memory"].as_str().unwrap()],
                "kv": f["kv"][run["kv"].as_str().unwrap()],
                "results": results,
            });
            let (status, reply) = plan_json(&input.to_string());
            assert_eq!(status, 0, "{} step {}", run["name"], n + 1);
            let got: Value = serde_json::from_str(&reply).unwrap();
            assert_eq!(got, t["expect"], "{} step {}", run["name"], n + 1);
            steps += 1;
            if let Some(outcome) = t.get("outcome") {
                results.push(result_of(&t["expect"], outcome));
            }
        }
    }
    assert!(steps >= 300);
}

#[test]
fn errors() {
    let f = fixtures();
    for c in f["errors"].as_array().unwrap() {
        let text = format!(
            "{}{}",
            c["text"].as_str().unwrap(),
            " ".repeat(c["pad"].as_u64().unwrap() as usize)
        );
        let (status, reply) = plan_json(&text);
        assert_eq!(status, 1, "{}", c["name"]);
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap(),
            c["expect"],
            "{}",
            c["name"]
        );
    }
}
