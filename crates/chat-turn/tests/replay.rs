//! Replays synthetic upstream streams through the relay and compares the events with fixtures.
//!
//! The fixtures under `fixtures/` are written by hand from noevia-core's `chat.cjs` (the source
//! lines are cited in `src/relay.rs`). They are not recordings of a Node run. The recorded turns
//! (`fixtures/node-turns.v1.json`, `node_turns.rs`) are the contract; these go when the relay is
//! rebuilt on them (track R8 step 4).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_turn::partial::FIELD_CAP_CHARS;
use chat_turn::tools::RESULT_CAP_CHARS;
use chat_turn::{relay, Event, Relayed, ToolCall, ToolDispatch, ToolOutcome, Upstream};
use serde_json::{json, Value};

/// Answers the k-th tool call with the k-th scripted outcome; past the script, an empty result.
struct Scripted {
    outcomes: Vec<Result<String, String>>,
}

impl ToolDispatch for Scripted {
    fn dispatch(&mut self, call: &ToolCall) -> ToolOutcome {
        match self.outcomes.get(call.index).cloned() {
            Some(Ok(text)) => ToolOutcome::Result(text),
            Some(Err(text)) => ToolOutcome::NotRun(text),
            None => ToolOutcome::Result(String::new()),
        }
    }
}

fn upstream_item(v: &Value) -> Upstream {
    if let Some(t) = v.get("delta").and_then(Value::as_str) {
        return Upstream::Delta(t.to_string());
    }
    if let Some(t) = v.get("reasoning").and_then(Value::as_str) {
        return Upstream::Reasoning(t.to_string());
    }
    if let Some(t) = v.get("fail").and_then(Value::as_str) {
        return Upstream::Fail(t.to_string());
    }
    if v.get("length").is_some() {
        return Upstream::Length;
    }
    let tool = &v["tool"];
    Upstream::Tool {
        name: tool["name"].as_str().unwrap_or_default().to_string(),
        args: tool["args"].clone(),
    }
}

/// The tool outcomes a fixture scripts, in call order: `not_run` text, or the `result`.
fn scripted_outcomes(v: &Value) -> Vec<Result<String, String>> {
    v["upstream"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("tool").is_some())
                .map(
                    |item| match item["tool"].get("not_run").and_then(Value::as_str) {
                        Some(text) => Err(text.to_string()),
                        None => Ok(item["tool"]["result"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string()),
                    },
                )
                .collect()
        })
        .unwrap_or_default()
}

fn run(
    model: &str,
    items: Vec<Upstream>,
    outcomes: Vec<Result<String, String>>,
    ttft: Option<f64>,
) -> Relayed {
    relay(model, items, &mut Scripted { outcomes }, ttft).expect("the relay keeps the event order")
}

/// Runs one fixture; returns its name, the events the relay produced and the expected events.
fn run_fixture(text: &str) -> (String, Vec<Value>, Vec<Value>) {
    let v: Value = serde_json::from_str(text).expect("fixtures are valid JSON");
    let items: Vec<Upstream> = v["upstream"]
        .as_array()
        .map(|a| a.iter().map(upstream_item).collect())
        .unwrap_or_default();
    let relayed = run(
        v["model"].as_str().unwrap_or_default(),
        items,
        scripted_outcomes(&v),
        v["ttft"].as_f64(),
    );
    let got = relayed.turn.events().iter().map(Event::to_json).collect();
    let expected = v["expected"].as_array().cloned().unwrap_or_default();
    (
        v["name"].as_str().unwrap_or_default().to_string(),
        got,
        expected,
    )
}

#[test]
fn every_fixture_matches_its_expected_events() {
    let fixtures = [
        include_str!("fixtures/plain_reply.json"),
        include_str!("fixtures/reasoning_only.json"),
        include_str!("fixtures/tool_round.json"),
        include_str!("fixtures/tool_not_run.json"),
        include_str!("fixtures/length_stop.json"),
        include_str!("fixtures/provider_failure.json"),
        include_str!("fixtures/empty_reply.json"),
    ];
    for text in fixtures {
        let (name, got, expected) = run_fixture(text);
        assert_eq!(got, expected, "fixture: {name}");
    }
}

#[test]
fn every_fixture_ends_with_one_terminal_and_the_turn_finishes() {
    let fixtures = [
        include_str!("fixtures/plain_reply.json"),
        include_str!("fixtures/provider_failure.json"),
    ];
    for text in fixtures {
        let v: Value = serde_json::from_str(text).unwrap();
        let items: Vec<Upstream> = v["upstream"]
            .as_array()
            .unwrap()
            .iter()
            .map(upstream_item)
            .collect();
        let relayed = run("m", items, Vec::new(), None);
        assert!(relayed.turn.finish().is_ok());
    }
    let (_, got, _) = run_fixture(include_str!("fixtures/provider_failure.json"));
    assert_eq!(got.last().map(|e| e["type"].clone()), Some(json!("error")));
}

#[test]
fn a_very_long_reply_is_kept_from_the_end_and_still_streamed_whole() {
    let big = "a".repeat(FIELD_CAP_CHARS + 1000);
    let items = vec![
        Upstream::Delta(big.clone()),
        Upstream::Delta("tail".to_string()),
    ];
    let relayed = run("m", items, Vec::new(), Some(0.1));
    assert!(relayed.partial.truncated);
    assert!(relayed.partial.content.ends_with("tail"));
    assert_eq!(relayed.partial.content.chars().count(), FIELD_CAP_CHARS);
    let streamed: usize = relayed
        .turn
        .events()
        .iter()
        .filter_map(|e| match e {
            Event::Delta { text } => Some(text.len()),
            _ => None,
        })
        .sum();
    assert_eq!(streamed, big.len() + "tail".len());
}

#[test]
fn a_huge_multibyte_tool_result_is_cut_without_panicking() {
    let result = "é😀".repeat(5000);
    let items = vec![Upstream::Tool {
        name: "t".into(),
        args: Value::Null,
    }];
    let relayed = run("m", items, vec![Ok(result)], None);
    let shown = relayed.turn.events().iter().find_map(|e| match e {
        Event::ToolResult { text, .. } => Some(text.chars().count()),
        _ => None,
    });
    assert_eq!(shown, Some(RESULT_CAP_CHARS));
}
