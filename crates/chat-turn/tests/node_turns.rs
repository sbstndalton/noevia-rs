//! The recorded Node turns: whole event streams, engine requests and store writes of noevia-core's
//! `server/chat.cjs` `handleChat`, driven by `record_turns.cjs` with a scripted engine and synthetic
//! stores. These tests pin what the recording says about the stream as a whole; the replays of the
//! native turn's pieces (context, relay, tools, saving) are checked against the same cases.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod common;

use std::collections::BTreeSet;

use chat_turn::sink::{check_stream, OrderError};
use common::{case, cases, client_left, fixture, js_len, request_body};
use serde_json::Value;

fn kinds(case: &Value) -> Vec<String> {
    case["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

fn streamed(case: &Value) -> bool {
    case["http"].is_object()
}

#[test]
fn the_fixture_names_its_node_sources_and_the_matrix() {
    let f = fixture();
    assert_eq!(f["version"], 1);
    for source in [
        "server/chat.cjs",
        "server/chat-context.cjs",
        "server/chat-partial.cjs",
        "server/chat-interrupted-turn.cjs",
        "server/chat-title.cjs",
        "server/dav-parse.lock",
    ] {
        assert_eq!(
            f["sources"][source].as_str().map(str::len),
            Some(64),
            "{source}"
        );
    }
    let names: Vec<String> = cases()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    let unique: BTreeSet<&String> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "case names are unique");
    // The matrix the track asks for: plain, reasoning, stopped mid-stream, engine failure, length
    // cut, title, a tool call approved and declined, context over budget.
    for name in [
        "plain",
        "reasoning",
        "stopped_mid_stream",
        "engine_http_500",
        "engine_stream_error",
        "length_cut",
        "title",
        "tool_approved",
        "tool_declined",
        "context_compacted",
        "context_too_full",
    ] {
        assert!(names.iter().any(|n| n == name), "{name} is recorded");
    }
    // Every request's tool reference resolves to a recorded list.
    for c in cases() {
        for r in c["requests"].as_array().unwrap() {
            let body = request_body(&f, r);
            if r["body"]["tools"].is_null() {
                assert!(body["tools"].is_null(), "{}", c["name"]);
            } else {
                assert!(r["body"]["tools"]["$tools"].is_string(), "{}", c["name"]);
                assert!(
                    !body["tools"].as_array().unwrap().is_empty(),
                    "{}",
                    c["name"]
                );
            }
        }
    }
}

#[test]
fn every_recorded_stream_keeps_the_order_rule() {
    for c in cases().iter().filter(|c| streamed(c)) {
        let events = c["events"].as_array().unwrap();
        check_stream(events, client_left(c))
            .unwrap_or_else(|e| panic!("{}: {e:?} in {:?}", c["name"], kinds(c)));
    }
}

#[test]
fn the_order_rule_refuses_what_node_never_sends() {
    let c = case("plain");
    let mut events = c["events"].as_array().unwrap().clone();
    let done = events.pop().unwrap();
    // A stream cut after `telemetry` complete, before `done`.
    assert_eq!(
        check_stream(&events, false),
        Err(OrderError::AfterComplete("end of stream".into()))
    );
    events.push(done.clone());
    events.push(done);
    assert_eq!(
        check_stream(&events, false),
        Err(OrderError::AfterDone("done".into()))
    );
}

#[test]
fn refusals_answer_before_the_stream_and_ask_nothing() {
    for c in cases().iter().filter(|c| !streamed(c)) {
        assert!(
            c["refusal"]["status"].as_u64().unwrap() >= 400,
            "{}",
            c["name"]
        );
        assert!(c["events"].as_array().unwrap().is_empty(), "{}", c["name"]);
        assert!(
            c["requests"].as_array().unwrap().is_empty(),
            "{}",
            c["name"]
        );
        assert!(c["stores"].as_array().unwrap().is_empty(), "{}", c["name"]);
    }
    for c in cases().iter().filter(|c| streamed(c)) {
        assert_eq!(c["http"]["status"], 200);
        assert_eq!(c["http"]["headers"]["Content-Type"], "text/event-stream");
        assert_eq!(
            c["http"]["headers"]["Cache-Control"],
            "no-cache, no-transform"
        );
        assert_eq!(c["http"]["headers"]["X-Accel-Buffering"], "no");
        assert!(c["refusal"].is_null());
    }
}

#[test]
fn what_follows_an_error_depends_on_where_it_came_from() {
    // A round that failed before or while its stream was read breaks the round loop: only the
    // reply's closing events follow. A failed non-streaming retry of an empty stream does not break
    // it, and the final-answer round streams after the error. A mid-stream error frame ends the
    // stream at once, as do a failed preparation and a refused tool round.
    // `gate_other_client_error`: a 4xx on the tool gate's forced round that is not about tool_choice (R15).
    const BREAKS: [&str; 4] = [
        "engine_http_500",
        "engine_stalls",
        "engine_unreachable",
        "gate_other_client_error",
    ];
    const FALLBACK: &str = "fallback_fails_then_final_pass";
    // A stream broken after reasoning only (here the forced answer after a thinking cut): the note follows.
    const NOTED: &str = "thinking_cut_forced_fails";
    let mut seen = BTreeSet::new();
    for c in cases().iter().filter(|c| streamed(c)) {
        let name = c["name"].as_str().unwrap();
        let k = kinds(c);
        let Some(at) = k.iter().position(|t| t == "error") else {
            continue;
        };
        let rest = &k[at + 1..];
        if rest.is_empty() {
            continue;
        }
        seen.insert(name.to_string());
        if BREAKS.contains(&name) {
            assert_eq!(rest, ["telemetry", "done"], "{name}");
        } else if name == NOTED {
            assert_eq!(rest, ["delta", "telemetry", "done"], "{name}");
        } else if name == FALLBACK {
            assert_eq!(&rest[rest.len() - 2..], ["telemetry", "done"], "{name}");
            assert!(
                rest.iter().any(|t| t == "delta"),
                "{name}: the final pass answers"
            );
            assert!(!rest.iter().any(|t| t == "tool_pending"), "{name}");
        } else {
            panic!("{name}: events after an error with no known source: {rest:?}");
        }
    }
    let mut expected: BTreeSet<String> = BREAKS.iter().map(|n| n.to_string()).collect();
    expected.insert(FALLBACK.to_string());
    expected.insert(NOTED.to_string());
    assert_eq!(seen, expected);
    assert_eq!(kinds(&case("engine_stream_error")).last().unwrap(), "error");
    assert_eq!(kinds(&case("context_too_full")).last().unwrap(), "error");
}

#[test]
fn every_tool_chip_gets_its_result_unless_the_client_left() {
    for c in cases().iter().filter(|c| streamed(c) && !client_left(c)) {
        let events = c["events"].as_array().unwrap();
        let shown: BTreeSet<u64> = events
            .iter()
            .filter(|e| e["type"] == "tool")
            .map(|e| e["index"].as_u64().unwrap())
            .collect();
        let answered: BTreeSet<u64> = events
            .iter()
            .filter(|e| e["type"] == "tool_result")
            .map(|e| e["index"].as_u64().unwrap())
            .collect();
        assert_eq!(shown, answered, "{}", c["name"]);
        for e in events.iter().filter(|e| e["type"] == "tool_result") {
            assert!(js_len(e["text"].as_str().unwrap()) <= 300, "{}", c["name"]);
        }
    }
}

#[test]
fn done_names_the_model_meta_named() {
    for c in cases().iter().filter(|c| streamed(c)) {
        let events = c["events"].as_array().unwrap();
        let model = &events.iter().find(|e| e["type"] == "meta").unwrap()["model"];
        for e in events
            .iter()
            .filter(|e| e["type"] == "done" || e["type"] == "telemetry")
        {
            assert_eq!(&e["model"], model, "{}", c["name"]);
        }
    }
}

#[test]
fn a_reply_the_client_left_is_kept_through_the_store_only() {
    for name in [
        "stopped_mid_stream",
        "stopped_before_output",
        "stopped_store_refuses",
    ] {
        let c = case(name);
        let appends: Vec<&Value> = c["stores"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["op"] == "transcripts.appendIfLast")
            .collect();
        assert_eq!(appends.len(), 1, "{name}");
        assert_eq!(appends[0]["chatId"], "qa-chat-1");
        assert_eq!(appends[0]["message"], "QA hello");
        assert_eq!(appends[0]["options"]["origin"], "turn");
        // The entry appended is the one the client would have kept (createTurnRecord, recorded).
        let kept = &appends[0]["entry"];
        let partial = &c["record"]["partialEntry"];
        let assistant = &c["record"]["assistantEntry"];
        assert!(kept == partial || kept == assistant, "{name}");
    }
    // The store refused: the partial file keeps the reply, without the running marker.
    let refused = case("stopped_store_refuses");
    let partials: Vec<(&String, &Value)> = refused["files"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(k, _)| k.starts_with("chat-partials/"))
        .collect();
    assert_eq!(partials.len(), 1);
    assert_eq!(partials[0].1["interrupted"], true);
    assert!(partials[0].1.get("running").is_none());
    assert_eq!(partials[0].1["content"], "Kept only as a partial");
    // Every other turn leaves no partial behind.
    for c in cases()
        .iter()
        .filter(|c| c["name"] != "stopped_store_refuses")
    {
        let any = c["files"]
            .as_object()
            .unwrap()
            .keys()
            .any(|k| k.starts_with("chat-partials/"));
        assert!(!any, "{}", c["name"]);
    }
}

#[test]
fn the_title_comes_after_done_from_one_non_streaming_request() {
    let c = case("title");
    let k = kinds(&c);
    assert_eq!(&k[k.len() - 2..], ["done", "title"]);
    let requests = c["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["stream"], false);
    let title = c["events"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(title["chatId"], "qa-chat-1");
    let saves: Vec<&Value> = c["stores"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["op"] == "chatTitles.save")
        .collect();
    assert_eq!(saves.len(), 1);
    assert_eq!(saves[0]["title"], title["title"]);
    // Renamed chat, failed title request: no title event, no save.
    for name in ["title_renamed", "title_failed"] {
        let c = case(name);
        assert_eq!(kinds(&c).last().unwrap(), "done", "{name}");
        assert!(!c["stores"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["op"] == "chatTitles.save"));
    }
}
