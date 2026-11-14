//! Differential test: the Rust retrieval trace must reproduce the frozen Python module's output on
//! the synthetic cases in `retrieval-trace-fixtures.json` (made by `generate_fixtures.py`).

use diary_core::retrieval_trace::{attach, EntryRef, RetrievalTrace};
use serde_json::{json, Map, Value};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixtures() -> Result<Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_str(include_str!(
        "retrieval-trace-fixtures.json"
    ))?)
}

fn object(value: &Value) -> Result<Map<String, Value>, String> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| format!("expected an object, got {value}"))
}

#[test]
fn clock_time_matches_python_ref() -> TestResult {
    let fx = fixtures()?;
    // The frozen diary/agent/retrieval_trace.py the generator refuses any other source than.
    assert_eq!(
        fx.get("source_sha256").and_then(Value::as_str),
        Some("ec4172f5135e70754aceab4a218e57e5730509dfd741851e53870a06bd1394d4")
    );
    let cases = fx.get("clock").and_then(Value::as_array).ok_or("clock")?;
    assert!(cases.len() >= 20);
    for case in cases {
        let row = if let Some(raw) = case.get("raw_header") {
            json!({"id": 2, "day": "2026-08-02", "header": raw})
        } else if let Some(raw_row) = case.get("raw_row") {
            raw_row.clone()
        } else {
            json!({"id": 1, "day": "2026-08-01", "header": case.get("header")})
        };
        let expected = object(&case["expected"])?;
        let actual = EntryRef::from_row(&row).to_json();
        if case
            .get("known_deviation")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            // Python `\d` accepts Unicode digits, and its `\w` is narrower than the port's word
            // test after the minutes; both are documented in `clock_time`.
            assert_eq!(actual.get("time"), Some(&Value::Null), "{row}");
            assert_ne!(actual, expected, "{row}");
        } else {
            assert_eq!(actual, expected, "{row}");
        }
    }
    Ok(())
}

#[test]
fn traces_match_python_module_output() -> TestResult {
    let fx = fixtures()?;
    let traces = fx.get("traces").and_then(Value::as_array).ok_or("traces")?;
    assert_eq!(traces.len(), 9);
    for case in traces {
        let name = case["name"].as_str().unwrap_or_default();
        let mut trace = RetrievalTrace::new();
        for op in case["ops"].as_array().ok_or("ops")? {
            apply(&mut trace, op).map_err(|e| format!("{name}: {e}"))?;
        }
        assert_eq!(trace.to_json(), case["data"], "trace {name}");

        let mut result = object(&json!({"answer": "synthetic"}))?;
        attach(&mut result, Some(&trace));
        assert_eq!(Value::Object(result), case["attached"], "attach {name}");
    }
    Ok(())
}

fn apply(trace: &mut RetrievalTrace, op: &Value) -> Result<(), String> {
    match op["op"].as_str().unwrap_or_default() {
        "status" => trace.set_status(op["value"].as_str().unwrap_or_default()),
        "params" => trace.set_params(object(&op["value"])?),
        "budget" => trace.set_budget(object(&op["value"])?),
        "candidate" => {
            let index = trace.add_candidate(
                op["rank"].as_u64().unwrap_or_default(),
                &op["row"],
                op.get("vector_rank").and_then(Value::as_u64),
            );
            trace.set_candidate_outcome(
                index,
                op["loaded"].as_bool().unwrap_or_default(),
                op["dropped"].as_str(),
            );
        }
        "score_gap" => {
            let removed: Vec<(u64, Value)> = op["removed"]
                .as_array()
                .ok_or("removed")?
                .iter()
                .map(|pair| (pair[0].as_u64().unwrap_or_default(), pair[1].clone()))
                .collect();
            let diagnostics = op.get("diagnostics").and_then(Value::as_object).cloned();
            trace.set_score_gap(
                op["gap"].as_f64(),
                op["applied"].as_bool().unwrap_or_default(),
                op["best"].clone(),
                &removed,
                diagnostics,
            );
        }
        "loaded" => trace.add_loaded(
            &op["row"],
            op["tokens"].as_u64().unwrap_or_default(),
            op["chars"].as_u64().unwrap_or_default(),
        ),
        "finish" => trace.finish(op["used_chars"].as_u64().unwrap_or_default()),
        "direct_fallback" => trace.set_direct_fallback(
            op["found"].as_bool().unwrap_or_default(),
            op["tokens"].as_u64().unwrap_or_default(),
        ),
        "fail" => trace.fail(),
        other => return Err(format!("unknown op {other}")),
    }
    Ok(())
}
