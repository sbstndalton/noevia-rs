//! Node differential policy oracle. No downloads, model runs or filesystem removals.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use noevia_server::model_maint::{pull, v8_json};

#[test]
fn exact_node_request_and_response_policy() {
    let cases: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/model-pull-node.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let result = pull::prepare(
            case["raw"].as_str().unwrap(),
            case["enabled"].as_bool().unwrap(),
        );
        let actual = match result {
            Err(answer) => {
                serde_json::json!({"status":answer.status,"body":serde_json::from_str::<serde_json::Value>(&answer.body).unwrap()})
            }
            Ok(plan) => {
                let request = plan.request_body();
                let wire = case["upstream"].to_string();
                let upstream = v8_json::parse(&wire.encode_utf16().collect::<Vec<_>>())
                    .ok()
                    .unwrap();
                let answer = plan.finish(
                    case["ok"].as_bool().unwrap(),
                    case["upstreamStatus"].as_u64().unwrap() as u16,
                    upstream,
                );
                serde_json::json!({"status":answer.status,"body":serde_json::from_str::<serde_json::Value>(&answer.body).unwrap(),"request":serde_json::from_str::<serde_json::Value>(&request).unwrap()})
            }
        };
        assert_eq!(actual, case["expected"], "{}", case["name"]);
    }
}

#[test]
fn arbitrary_upstream_errors_remain_lossless_utf16() {
    let plan = pull::prepare(r#"{"checkpoint":"synthetic/repo:Q4"}"#, true)
        .ok()
        .unwrap();
    let upstream = v8_json::parse(&r#"{"error":"\ud800"}"#.encode_utf16().collect::<Vec<_>>())
        .ok()
        .unwrap();
    assert_eq!(
        plan.finish(false, 400, upstream).body,
        r#"{"error":"\ud800"}"#
    );
}
