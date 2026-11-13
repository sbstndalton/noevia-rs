//! Differential test against the Node oracle: `tests/fixtures/mcp-oracle.v1.json` was recorded by
//! `handoff/foundation/R5/scripts/record-oracles.cjs` calling noevia-core's mcp.cjs
//! `resultToText` and `withBuiltInHeaders` on synthetic inputs. The fixture's `structured_json`
//! is the JS `JSON.stringify` of `structuredContent`, which is what the caller passes in here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use mcp_client::*;
use serde_json::Value;

const ORACLE: &str = include_str!("fixtures/mcp-oracle.v1.json");

/// Maps one JSON part the way `resultToText` reads it: `c.type === 'text' && typeof c.text ===
/// 'string'` is text, any other truthy type is named, everything else is skipped.
fn part(c: &Value) -> Content {
    let Some(obj) = c.as_object() else {
        return Content::Untyped;
    };
    let typ = obj.get("type").cloned().unwrap_or(Value::Null);
    if !js_truthy(&typ) {
        return Content::Untyped;
    }
    match (typ.as_str(), obj.get("text").and_then(Value::as_str)) {
        (Some("text"), Some(text)) => Content::Text(text.to_owned()),
        (Some(name), _) => Content::Typed(name.to_owned()),
        (None, _) => Content::Typed(typ.to_string()),
    }
}

fn result_from(input: &Value, structured: Option<&str>) -> ToolResult {
    let Some(obj) = input.as_object() else {
        return ToolResult::default();
    };
    let content = obj
        .get("content")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(part).collect())
        .unwrap_or_default();
    ToolResult {
        content,
        structured_json: structured.map(str::to_owned),
        is_error: obj.get("isError").is_some_and(js_truthy),
    }
}

fn pairs(v: &Value) -> Vec<(String, String)> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|p| {
                    let k = p[0].as_str().unwrap().to_owned();
                    let v = p[1].as_str().unwrap().to_owned();
                    (k, v)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn result_text_replays_the_node_answers() {
    let oracle: Value = serde_json::from_str(ORACLE).unwrap();
    let cases = oracle["result_to_text"].as_array().unwrap();
    assert!(cases.len() >= 10);
    for case in cases {
        let input = &case["input"];
        let structured = case["structured_json"].as_str();
        let got = if input.is_object() {
            result_to_text(&result_from(input, structured))
        } else {
            // `if (!result) return ''`, and a non-object reads no content.
            String::new()
        };
        assert_eq!(got, case["expect"].as_str().unwrap(), "input {input}");
    }
}

#[test]
fn built_in_headers_replay_the_node_answers() {
    let oracle: Value = serde_json::from_str(ORACLE).unwrap();
    assert_eq!(
        oracle["protocol_version"].as_str().unwrap(),
        PROTOCOL_VERSION
    );
    let cases = oracle["built_in_headers"].as_array().unwrap();
    assert!(cases.len() >= 4);
    for case in cases {
        let extra = pairs(&case["extra"]);
        let built_owned = pairs(&case["builtIn"]);
        let built: Vec<(&str, &str)> = built_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let got = built_in_headers(&extra, &built);
        assert_eq!(got, pairs(&case["expect"]), "extra {}", case["extra"]);
    }
}
