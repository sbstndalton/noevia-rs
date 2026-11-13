//! Differential test: `completed_exchanges` must reproduce the frozen Python function on the
//! synthetic conversations in `completed-exchanges-fixtures.json` (made by
//! `generate_context_fixtures.py`).

use diary_core::context::completed_exchanges;
use serde_json::Value;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn completed_exchanges_matches_python_function() -> TestResult {
    let fixtures: Value = serde_json::from_str(include_str!("completed-exchanges-fixtures.json"))?;
    let cases = fixtures
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("cases")?;
    assert_eq!(cases.len(), 7);
    for case in cases {
        let name = case["name"].as_str().unwrap_or_default();
        // Python `turns or []` also covers None; a non-list input is iterated by key in Python and
        // every key is a string, so nothing survives. The Rust API takes a slice, so those map to
        // an empty slice.
        let empty = Vec::new();
        let turns = case["turns"].as_array().unwrap_or(&empty);
        let actual = Value::Array(completed_exchanges(turns));
        assert_eq!(actual, case["expected"], "case {name}");
    }
    Ok(())
}

#[test]
fn kept_pairs_keep_the_reply_role_and_unstripped_question_text() -> TestResult {
    let turns: Vec<Value> = serde_json::from_str(
        r#"[
            {"role": "user", "content": "  padded question  "},
            {"role": "companion", "content": "answer"},
            {"role": "user", "content": "\u001c"},
            {"role": "assistant", "content": "orphan after control-only question"}
        ]"#,
    )?;
    let kept = completed_exchanges(&turns);
    assert_eq!(
        kept,
        vec![
            serde_json::json!({"role": "user", "content": "  padded question  "}),
            serde_json::json!({"role": "companion", "content": "answer"}),
        ]
    );
    Ok(())
}
