#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use js_json::JValue;
use proptest::prelude::*;
use server_export::markdown::{
    render, Error, MAX_DEPTH, MAX_INPUT_BYTES, MAX_NODES, MAX_RENDER_BYTES,
};

#[test]
fn actual_node_bytes_and_error_classes() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/node-markdown.v1.json")).unwrap();
    assert_eq!(fixtures["core"], "965598462d64dc04e1df55c3bb7a25d4343380e1");
    assert_eq!(
        fixtures["sourceSha256"],
        "5cfbfd9353b3963a903f0da70fb191e50c7af9d6f215742c222143e402d62d7c"
    );
    for case in fixtures["cases"].as_array().unwrap() {
        let input = js_json::parse(case["input"].as_str().unwrap()).unwrap();
        let result = render(input.get("chat"), input.get("history"));
        if case["refusal"] == "UnsupportedDate" {
            assert_eq!(result, Err(Error::UnsupportedDate), "{}", case["id"]);
        } else if let Some(text) = case["node"]["text"].as_str() {
            assert_eq!(result, Ok(text.into()), "{}", case["id"]);
        } else {
            assert_eq!(
                result,
                Err(match case["node"]["error"].as_str().unwrap() {
                    "TypeError" => Error::Type,
                    "RangeError" => Error::Date,
                    other => panic!("unexpected {other}"),
                }),
                "{}",
                case["id"]
            );
        }
    }
}
#[test]
fn preflight_bounds_all_fields_before_formatting() {
    let empty = JValue::Arr(vec![]);
    let oversized = JValue::obj([("reasoning", JValue::Str("x".repeat(MAX_INPUT_BYTES)))]);
    assert_eq!(render(&oversized, &empty), Err(Error::Limit));
    let one_over = JValue::obj([("x", JValue::Str("x".repeat(MAX_INPUT_BYTES)))]);
    assert_eq!(render(&one_over, &empty), Err(Error::Limit));
    let exact = JValue::obj([("x", JValue::Str("x".repeat(MAX_INPUT_BYTES - 1)))]);
    assert_eq!(render(&exact, &empty).unwrap(), "# Untitled chat\n");
    let many = JValue::Arr(vec![JValue::Null; MAX_NODES]);
    assert_eq!(render(&JValue::Null, &many), Err(Error::Limit));
    let exact_nodes = JValue::Arr(vec![JValue::Null; MAX_NODES - 2]);
    assert_eq!(
        render(&JValue::obj::<&str>([]), &exact_nodes),
        Err(Error::Type)
    );
    let mut deep = JValue::Null;
    for _ in 0..MAX_DEPTH {
        deep = JValue::Arr(vec![deep]);
    }
    assert!(render(&JValue::Bool(true), &deep).is_ok()); // nested arrays are skipped non-user roles.
    let too_deep = JValue::Arr(vec![deep]);
    assert_eq!(render(&JValue::Bool(true), &too_deep), Err(Error::Limit));
    let duplicates = JValue::Obj(vec![
        ("title".into(), JValue::Null),
        ("title".into(), JValue::Null),
    ]);
    assert_eq!(render(&duplicates, &empty), Err(Error::UnsupportedInput));
}
#[test]
fn numeric_timeclip_and_nonfinite_values() {
    let empty = JValue::Arr(vec![]);
    for value in [f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            render(&JValue::obj([("updatedAt", JValue::Num(value))]), &empty),
            Err(Error::Date)
        );
    }
    let nan = JValue::obj([
        ("updatedAt", JValue::Num(f64::NAN)),
        ("title", JValue::Num(f64::NAN)),
    ]);
    assert_eq!(render(&nan, &empty).unwrap(), "# Untitled chat\n");
}
#[test]
fn private_reasoning_and_other_roles_never_render() {
    let input = js_json::parse(r#"{"chat":{},"history":[{"role":"user","content":"VISIBLE","reasoning":"PRIVATE_REASONING"},{"role":"system","content":"PRIVATE_SYSTEM"}]}"#).unwrap();
    let text = render(input.get("chat"), input.get("history")).unwrap();
    assert!(!text.contains("PRIVATE_"), "private content leaked: {text}");
    assert_eq!(text, "# Untitled chat\n\n## You\n\nVISIBLE\n");
}
proptest! {
    #[test]
    fn text_policy_preserves_working_content_and_omits_other_fields(title in "[a-zA-Z0-9 _-]{0,64}", text in "[a-zA-Z0-9 _-]{0,512}") {
        let chat = JValue::obj([("title",JValue::Str(title.clone()))]);
        let message = JValue::obj([("role",JValue::from("user")),("content",JValue::Str(format!("\u{feff}{text}\u{feff}"))),("reasoning",JValue::from("PRIVATE_REASONING"))]);
        let ignored = JValue::obj([("role",JValue::from("system")),("content",JValue::from("PRIVATE_SYSTEM")),("toolCalls",JValue::Bool(true))]);
        let history = JValue::Arr(vec![message,ignored]);
        let actual = render(&chat,&history).unwrap();
        let title = if title.is_empty() { "Untitled chat" } else { &title };
        let trimmed = text.trim_matches(' ');
        let body = if trimmed.is_empty() { "_(no text)_" } else { trimmed };
        prop_assert_eq!(&actual,&format!("# {title}\n\n## You\n\n{body}\n"));
        prop_assert!(!actual.contains("PRIVATE_"));
        prop_assert!(actual.len() <= MAX_RENDER_BYTES);
    }
    #[test]
    fn integer_date_matches_utc_roundtrip(ms in -62_167_219_200_000i64..253_402_300_800_000i64) {
        let chat = JValue::obj([("updatedAt",JValue::Num(ms as f64))]);
        let out = render(&chat,&JValue::Arr(vec![])).unwrap();
        if ms == 0 { prop_assert_eq!(out,"# Untitled chat\n"); }
        else {
            let iso = out.strip_prefix("# Untitled chat\n\n_Last updated ").unwrap().strip_suffix("_\n").unwrap();
            prop_assert_eq!(chrono::DateTime::parse_from_rfc3339(iso).unwrap().timestamp_millis(),ms);
        }
    }
}
