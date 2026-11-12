//! Golden tests for the AG-UI replay translation (sbstndalton/noevia#2446, slice 5): each
//! `tests/fixtures/events/<case>.input.json` (`{after, history}`, the stored messages of a
//! coworker's chat) must translate to exactly the JSON lines of `<case>.events.jsonl`. Set
//! `NOEVIA_BLESS=1` to write the expected files, then read the diff.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use js_json::{wtf16, JValue};
use server_specialists::events::{frame, translate, UnknownAfter};
use std::path::PathBuf;

const THREAD: &str = "11111111-1111-4111-8111-111111111111";

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/events")
}

fn history(text: &str) -> Vec<JValue> {
    match wtf16::parse(text).unwrap() {
        JValue::Arr(items) => items,
        other => panic!("not a list: {other:?}"),
    }
}

fn lines(history: &[JValue], after: Option<&str>) -> String {
    translate(THREAD, history, after)
        .unwrap()
        .iter()
        .map(|e| serde_json::to_string(e).unwrap() + "\n")
        .collect()
}

fn check(case: &str) {
    let text = std::fs::read_to_string(dir().join(format!("{case}.input.json"))).unwrap();
    let input = wtf16::parse(&text).unwrap();
    let after = input.get("after").as_str().map(str::to_owned);
    let JValue::Arr(messages) = input.get("history").clone() else {
        panic!("{case}: no history");
    };
    let got = lines(&messages, after.as_deref());
    let file = dir().join(format!("{case}.events.jsonl"));
    if std::env::var_os("NOEVIA_BLESS").is_some() {
        std::fs::write(&file, &got).unwrap();
    }
    assert_eq!(got, std::fs::read_to_string(&file).unwrap(), "{case}");
}

#[test]
fn plain_reply() {
    check("plain_reply");
}

#[test]
fn reply_with_reasoning() {
    check("reply_with_reasoning");
}

#[test]
fn tool_call_with_result() {
    check("tool_call_with_result");
}

#[test]
fn declined_call() {
    check("declined_call");
}

#[test]
fn page_save() {
    check("page_save");
}

#[test]
fn interrupted_reply() {
    check("interrupted_reply");
}

#[test]
fn two_turns_with_after() {
    check("two_turns_after");
}

#[test]
fn after_the_last_message_is_an_empty_replay_and_an_unknown_one_is_refused() {
    let h = history(
        r#"[{"id":"a","role":"user","content":"QA q"},{"id":"b","role":"assistant","content":"QA a"}]"#,
    );
    assert_eq!(translate(THREAD, &h, Some("b")).unwrap(), vec![]);
    // A trailing user message gets a run of its own.
    let t = lines(
        &history(r#"[{"id":"a","role":"user","content":"QA q"}]"#),
        None,
    );
    assert!(t.starts_with(r#"{"type":"RUN_STARTED""#) && t.contains(r#""awaitingReply":true"#));
    assert_eq!(translate(THREAD, &h, Some("zz")), Err(UnknownAfter));
    assert_eq!(translate(THREAD, &[], None).unwrap(), vec![]);
}

#[test]
fn ids_are_stored_ids_or_positions_and_every_event_is_one_sse_frame() {
    let h = history(r#"[{"role":"user","content":"QA q"},{"role":"assistant","content":"QA a"}]"#);
    let events = translate(THREAD, &h, None).unwrap();
    let text: String = events.iter().map(|e| frame(e).unwrap()).collect();
    assert!(text.contains(r#""messageId":"message-0""#));
    assert!(text.contains(r#""runId":"message-1""#));
    assert_eq!(text.matches("\n\n").count(), events.len());
    assert!(text
        .lines()
        .filter(|l| !l.is_empty())
        .all(|l| l.starts_with("data: {")));
    // A cursor can name a message that was saved without an id.
    assert_eq!(translate(THREAD, &h, Some("message-0")).unwrap().len(), 5);
}

#[test]
fn a_pending_call_a_stopped_reply_a_pause_and_a_failure_say_so() {
    let h = history(
        r#"[
        {"id":"p","role":"assistant","content":"","toolCalls":[{"name":"w","args":"{}","status":"pending"},{"name":"x","args":"","status":"done"}]},
        {"id":"s","role":"assistant","content":"QA part","stopped":true},
        {"id":"q","role":"assistant","content":"","paused":{"reason":"supervision","applied":2}},
        {"id":"f","role":"assistant","content":"QA kept text","error":true,"failure":"QA failed"},
        {"id":"o","role":"assistant","content":"QA old failure sentence","error":true}
    ]"#,
    );
    let got = lines(&h, None);
    assert!(got.contains(r#""reason":"approval_pending""#));
    // A call with no stored status but a "done" and no result still answers, with empty content.
    assert!(got.contains(r#""content":"""#));
    assert!(got.contains(r#""result":{"stopped":true}"#));
    assert!(got.contains(r#""result":{"paused":{"applied":2,"reason":"supervision"}}"#));
    assert!(got.contains(r#""message":"QA failed","code":"failed""#));
    assert!(got.contains(r#""message":"QA old failure sentence","code":"failed""#));
    // The older failure sentence is not message text.
    assert!(!got.contains(r#""delta":"QA old failure sentence""#));
}

#[test]
fn every_fixture_follows_the_run_lifecycle() {
    // The protocol's verifier: the first event is RUN_STARTED, a run opens only when none is
    // open, and a run ends with RUN_FINISHED or RUN_ERROR.
    for entry in std::fs::read_dir(dir()).unwrap() {
        let path = entry.unwrap().path();
        if !path.to_string_lossy().ends_with(".events.jsonl") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let mut open = false;
        for (n, line) in text.lines().enumerate() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            match v.get("type").and_then(serde_json::Value::as_str).unwrap() {
                "RUN_STARTED" => {
                    assert!(!open, "{path:?}");
                    open = true;
                }
                "RUN_FINISHED" | "RUN_ERROR" => {
                    assert!(open, "{path:?}");
                    open = false;
                }
                _ => assert!(open, "{path:?} line {n}"),
            }
        }
        assert!(!open, "{path:?}");
    }
}

#[test]
fn a_stopped_pause_keeps_both_marks() {
    let h = history(
        r#"[{"id":"s","role":"assistant","content":"QA","stopped":true,"paused":{"reason":"stopped","applied":1}}]"#,
    );
    assert!(lines(&h, None)
        .contains(r#""result":{"paused":{"applied":1,"reason":"stopped"},"stopped":true}"#));
}

#[test]
fn only_a_real_page_save_result_makes_the_custom_event() {
    let h = history(
        r#"[{"id":"a","role":"assistant","content":"","toolCalls":[
        {"name":"space_page_save","args":"{}","status":"done","result":"ERROR: no default Space"},
        {"name":"other_tool","args":"{}","status":"done","result":"{\"pageId\":\"p\",\"spaceId\":\"s\",\"title\":\"t\",\"link\":\"/l\"}"}
    ]}]"#,
    );
    assert!(!lines(&h, None).contains("CUSTOM"));
}
