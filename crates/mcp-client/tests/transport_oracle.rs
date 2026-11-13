//! Replays the transport part of `tests/fixtures/mcp-oracle.v1.json`, recorded from noevia-core's
//! own `mcp.cjs` by `crates/drive-tools/tests/generate_oracles.cjs` with a recording fetch stub:
//! the `tools/list` paging and byte budget, the request bodies, and `positiveEnv`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use mcp_client::*;
use serde_json::{json, Value};

const ORACLE: &str = include_str!("fixtures/mcp-oracle.v1.json");

fn oracle() -> Value {
    serde_json::from_str(ORACLE).unwrap()
}

#[test]
fn list_tools_pages_and_budget_replay_the_node_answers() {
    let cases = oracle()["list_tools"].as_array().unwrap().clone();
    assert!(cases.len() >= 10);
    for case in cases {
        let env = &case["env"];
        let max_pages = positive_env(env["MCP_LIST_TOOLS_MAX_PAGES"].as_str(), LIST_MAX_PAGES);
        let max_bytes = positive_env(env["MCP_LIST_TOOLS_MAX_BYTES"].as_str(), LIST_MAX_BYTES);
        let pages = case["pages"].as_array().unwrap();
        let mut budget = ListBudget::new(max_pages, max_bytes);
        let (mut requests, mut kept, mut cursor) = (Vec::new(), Vec::new(), None::<String>);
        'pages: loop {
            if !budget.page_allowed() {
                break;
            }
            budget.page_asked();
            requests.push(list_tools_request(1, cursor.as_deref())["params"].clone());
            let page = &pages[(requests.len() - 1).min(pages.len() - 1)];
            for tool in page["tools"].as_array().into_iter().flatten() {
                // The UTF-8 length of the tool's JSON.stringify text, as Node measured it.
                let bytes = case["tool_bytes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|p| p[0] == tool["name"])
                    .and_then(|p| p[1].as_u64())
                    .unwrap();
                if budget.admit_tool(usize::try_from(bytes).unwrap()) == Admit::Stop {
                    break 'pages;
                }
                kept.push(tool["name"].clone());
            }
            cursor = next_cursor(page);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(Value::Array(requests), case["requests"], "{env}");
        assert_eq!(Value::Array(kept), case["kept"], "{env}");
    }
}

#[test]
fn request_bodies_replay_the_node_answers() {
    let bodies = oracle()["request_bodies"].as_array().unwrap().clone();
    let without_id = |mut v: Value| {
        let has_id = v.as_object_mut().unwrap().remove("id").is_some();
        v.as_object_mut()
            .unwrap()
            .insert("has_id".into(), json!(has_id));
        v
    };
    let args = [
        Some(json!({"q": 1})),
        Some(Value::Null),
        None,
        Some(json!(0)),
        Some(json!("")),
        Some(json!(false)),
        Some(json!([])),
        Some(json!({})),
    ];
    let mut ours = vec![
        without_id(initialize_request(1)),
        without_id(initialized_notification()),
    ];
    ours.extend(
        args.into_iter()
            .map(|a| without_id(call_tool_request(1, "qa_tool", a))),
    );
    assert_eq!(Value::Array(ours), Value::Array(bodies));
}

#[test]
fn positive_env_replays_the_node_answers() {
    let cases = oracle()["positive_env"].as_array().unwrap().clone();
    assert!(cases.len() >= 20);
    for case in cases {
        let value = case["value"].as_str();
        let want = usize::try_from(case["out"].as_u64().unwrap()).unwrap();
        assert_eq!(positive_env(value, 7), want, "{value:?}");
    }
}
