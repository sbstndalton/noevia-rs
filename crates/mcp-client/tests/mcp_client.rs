//! Tests for the pure MCP client helpers. Expected values are read from server/mcp.cjs (the
//! function named in each comment), not from running it: no Node oracle has been recorded for
//! this crate yet (handoff/foundation/R5.md, step 4). Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use mcp_client::*;
use serde_json::{json, Value};

fn pair(k: &str, v: &str) -> (String, String) {
    (k.to_owned(), v.to_owned())
}

#[test]
fn constants_match_mcp_cjs() {
    assert_eq!(PROTOCOL_VERSION, "2025-06-18");
    assert_eq!((CLIENT_NAME, CLIENT_VERSION), ("noevia", "1"));
    assert_eq!(MAX_RESPONSE_BYTES, 8 * 1024 * 1024);
    assert_eq!(
        (RPC_TIMEOUT_MS, LIST_TIMEOUT_MS, CALL_TIMEOUT_MS),
        (30_000, 60_000, 60_000)
    );
    assert_eq!(
        (DISCONNECT_TIMEOUT_MS, ABORTED_DISCONNECT_MS),
        (5_000, 1_000)
    );
    assert_eq!((LIST_MAX_PAGES, LIST_MAX_BYTES), (20, 16 * 1024 * 1024));
}

#[test]
fn positive_env_reads_like_positive_env_in_js() {
    assert_eq!(positive_env(None, 20), 20);
    assert_eq!(positive_env(Some(""), 20), 20);
    assert_eq!(positive_env(Some("   "), 20), 20);
    assert_eq!(positive_env(Some("0"), 20), 20);
    assert_eq!(positive_env(Some("-3"), 20), 20);
    assert_eq!(positive_env(Some("NaN"), 20), 20);
    assert_eq!(positive_env(Some("Infinity"), 20), 20);
    assert_eq!(positive_env(Some("inf"), 20), 20);
    assert_eq!(positive_env(Some("abc"), 20), 20);
    assert_eq!(positive_env(Some("7"), 20), 7);
    assert_eq!(positive_env(Some(" 7 "), 20), 7);
    assert_eq!(positive_env(Some("2.9"), 20), 2);
    assert_eq!(positive_env(Some("1e3"), 20), 1000);
    assert_eq!(positive_env(Some("0.5"), 20), 0);
}

#[test]
fn reserved_headers_are_dropped_whatever_their_case_and_built_ins_follow() {
    let extra = vec![
        pair("Authorization", "Bearer x"),
        pair("MCP-Session-Id", "forged"),
        pair("accept", "text/html"),
        pair("Content-Type", "text/plain"),
        pair("mcp-protocol-version", "1999-01-01"),
        pair("X-Custom", "1"),
    ];
    let built = [
        ("MCP-Protocol-Version", PROTOCOL_VERSION.to_owned()),
        ("mcp-session-id", "real".to_owned()),
    ];
    let out = built_in_headers(&extra, &built);
    assert_eq!(
        out,
        vec![
            pair("Authorization", "Bearer x"),
            pair("X-Custom", "1"),
            pair("MCP-Protocol-Version", "2025-06-18"),
            pair("mcp-session-id", "real"),
        ]
    );
}

#[test]
fn initialize_request_has_the_protocol_and_client_info() {
    let v = initialize_request(3);
    assert_eq!(v["id"], json!(3));
    assert_eq!(v["method"], json!("initialize"));
    assert_eq!(v["params"]["protocolVersion"], json!("2025-06-18"));
    assert_eq!(
        v["params"]["clientInfo"],
        json!({"name": "noevia", "version": "1"})
    );
    assert_eq!(v["params"]["capabilities"], json!({}));
}

#[test]
fn initialized_notification_has_no_id() {
    let v = initialized_notification();
    assert_eq!(
        v,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    );
    assert!(v.get("id").is_none());
}

#[test]
fn list_request_carries_the_cursor_only_when_there_is_one() {
    assert_eq!(list_tools_request(4, None)["params"], json!({}));
    assert_eq!(list_tools_request(4, Some(""))["params"], json!({}));
    assert_eq!(
        list_tools_request(5, Some("p2"))["params"],
        json!({"cursor": "p2"})
    );
}

#[test]
fn call_request_turns_absent_or_falsy_arguments_into_an_empty_object() {
    for args in [
        None,
        Some(Value::Null),
        Some(json!(false)),
        Some(json!(0)),
        Some(json!("")),
    ] {
        assert_eq!(
            call_tool_request(9, "t", args)["params"]["arguments"],
            json!({}),
            "falsy arguments must become {{}}"
        );
    }
    assert_eq!(
        call_tool_request(9, "t", Some(json!({"q": "x"})))["params"],
        json!({"name": "t", "arguments": {"q": "x"}})
    );
    // Truthy non-objects are passed through as the JS does (`args || {}`).
    assert_eq!(
        call_tool_request(9, "t", Some(json!([1])))["params"]["arguments"],
        json!([1])
    );
}

#[test]
fn next_cursor_only_for_a_non_empty_string() {
    assert_eq!(
        next_cursor(&json!({"nextCursor": "abc"})),
        Some("abc".into())
    );
    assert_eq!(next_cursor(&json!({"nextCursor": ""})), None);
    assert_eq!(next_cursor(&json!({"nextCursor": 5})), None);
    assert_eq!(next_cursor(&json!({})), None);
    assert_eq!(next_cursor(&Value::Null), None);
}

#[test]
fn page_budget_allows_twenty_pages_then_stops() {
    let mut b = ListBudget::new(LIST_MAX_PAGES, LIST_MAX_BYTES);
    for _ in 0..LIST_MAX_PAGES {
        assert!(b.page_allowed());
        b.page_asked();
    }
    assert!(!b.page_allowed());
}

#[test]
fn byte_budget_stops_at_the_tool_that_would_exceed_it_and_does_not_count_it() {
    let mut b = ListBudget::new(LIST_MAX_PAGES, 10);
    assert_eq!(b.admit_tool(4), Admit::Keep);
    assert_eq!(b.admit_tool(6), Admit::Keep); // exactly at the budget
    assert_eq!(b.admit_tool(1), Admit::Stop);
    assert_eq!(b.admit_tool(0), Admit::Keep); // not counted: the listing already stopped in JS
}

#[test]
fn byte_budget_saturates_instead_of_wrapping() {
    // Without saturation, usize::MAX - 1 + 2 would wrap to 0 and the second tool would be kept.
    let mut b = ListBudget::new(1, usize::MAX - 1);
    assert_eq!(b.admit_tool(usize::MAX - 1), Admit::Keep);
    assert_eq!(b.admit_tool(2), Admit::Stop);
}

fn text(s: &str) -> Content {
    Content::Text(s.to_owned())
}

#[test]
fn text_parts_join_with_newlines_and_are_trimmed_as_js_trim() {
    let r = ToolResult {
        content: vec![
            text("\u{feff} first "),
            Content::Untyped,
            text("second\u{a0}"),
        ],
        ..ToolResult::default()
    };
    assert_eq!(result_to_text(&r), "first \nsecond");
    // U+0085 is not JS whitespace, so it stays.
    let kept = ToolResult {
        content: vec![text("\u{85}x")],
        ..ToolResult::default()
    };
    assert_eq!(result_to_text(&kept), "\u{85}x");
}

#[test]
fn other_typed_parts_are_named_not_shown() {
    let r = ToolResult {
        content: vec![
            text("see the chart"),
            Content::Typed("image".into()),
            Content::Typed("resource_link".into()),
        ],
        ..ToolResult::default()
    };
    assert_eq!(
        result_to_text(&r),
        "see the chart\n[image content omitted]\n[resource_link content omitted]"
    );
}

#[test]
fn structured_content_is_the_fallback_only_when_there_is_no_text() {
    let fallback = ToolResult {
        content: vec![Content::Untyped],
        structured_json: Some(r#"{"b":2,"a":1}"#.into()),
        is_error: false,
    };
    // The caller's JSON.stringify text is used as given: insertion order, not sorted.
    assert_eq!(result_to_text(&fallback), r#"{"b":2,"a":1}"#);
    let with_text = ToolResult {
        content: vec![text("words")],
        structured_json: Some("{}".into()),
        is_error: false,
    };
    assert_eq!(result_to_text(&with_text), "words");
}

#[test]
fn an_error_result_is_text_with_a_prefix() {
    let r = ToolResult {
        content: vec![text("  not found  ")],
        is_error: true,
        ..ToolResult::default()
    };
    assert_eq!(result_to_text(&r), "ERROR from tool: not found");
    let empty = ToolResult {
        is_error: true,
        ..ToolResult::default()
    };
    assert_eq!(result_to_text(&empty), "ERROR from tool: (no detail)");
}

#[test]
fn an_empty_result_is_empty_text() {
    assert_eq!(result_to_text(&ToolResult::default()), "");
}

#[test]
fn hostile_result_sizes_do_not_panic() {
    let big = "🙂".repeat(1_000);
    let r = ToolResult {
        content: (0..1_000).map(|_| text(&big)).collect(),
        ..ToolResult::default()
    };
    let out = result_to_text(&r);
    assert!(out.len() > 1_000_000);
    let many = ToolResult {
        content: vec![Content::Untyped; 10_000],
        ..ToolResult::default()
    };
    assert_eq!(result_to_text(&many), "");
}
