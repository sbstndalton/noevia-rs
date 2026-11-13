//! The turn record replayed against Node: `fixtures/node-turn-record.v1.json` (noevia-core's
//! `createTurnRecord`, `bounded()` and `finish({ keep })` over synthetic QA event sequences,
//! written by `record_entries.cjs`) and every whole turn of `node-turns.v1.json`. Every result is
//! compared as the bytes `JSON.stringify` wrote: key order, number forms and lone surrogates.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod common;

use std::collections::BTreeSet;

use chat_turn::record::{bounded, partial_file, TurnRecord, PARTIAL_CAP_UNITS, TOOL_RESULT_LIMIT};
use js_json::{wtf16, JValue};
use serde_json::Value;

const RECORDED: &str = include_str!("fixtures/node-turn-record.v1.json");

fn recorded() -> Value {
    serde_json::from_str(RECORDED).unwrap()
}

/// A recorded string, its `{ "$str": [segment, ...] }` form written out.
fn text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_owned();
    }
    let mut out = String::new();
    for segment in v["$str"].as_array().unwrap() {
        match segment {
            Value::String(s) => out.push_str(s),
            run => out.push_str(
                &run[0]
                    .as_str()
                    .unwrap()
                    .repeat(run[1].as_u64().unwrap() as usize),
            ),
        }
    }
    out
}

/// JSON text as Rust reads it (lone surrogate escapes kept).
fn parse(text: &str) -> JValue {
    wtf16::parse(text).unwrap()
}

/// `JSON.stringify(v)`, `"null"` for nothing.
fn js(v: Option<&JValue>) -> String {
    v.map_or_else(|| "null".to_owned(), |v| wtf16::stringify(v).unwrap())
}

fn assert_bytes(name: &str, what: &str, got: &str, want: &str) {
    let cut = |s: &str| s.chars().take(1500).collect::<String>();
    assert!(
        got == want,
        "{name}: {what}\n got: {}\nwant: {}",
        cut(got),
        cut(want)
    );
}

/// A record fed `events` with a clock answering `clock` in order, the last value repeating.
fn replay<'a>(clock: Vec<i64>, events: impl IntoIterator<Item = &'a JValue>) -> TurnRecord {
    let last = *clock.last().unwrap();
    let mut ticks = clock.into_iter();
    let mut record = TurnRecord::new(move || ticks.next().unwrap_or(last));
    for ev in events {
        record.observe(ev);
    }
    record
}

/// The entry chat.cjs keeps when the client left: `partialEntry() || assistantEntry()`.
fn kept(record: &TurnRecord) -> Option<JValue> {
    record.partial_entry().or_else(|| record.assistant_entry())
}

fn case<'a>(f: &'a Value, name: &str) -> &'a Value {
    f["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap()
}

#[test]
fn the_recording_names_its_sources() {
    let f = recorded();
    assert_eq!(f["version"], 1);
    for source in [
        "server/chat-interrupted-turn.cjs",
        "server/chat-partial.cjs",
    ] {
        assert_eq!(
            f["sources"][source].as_str().map(str::len),
            Some(64),
            "{source}"
        );
    }
    assert_eq!(f["partialCapChars"], PARTIAL_CAP_UNITS);
    let names: Vec<&str> = f["cases"]
        .as_array()
        .unwrap()
        .iter()
        .chain(f["boundedCases"].as_array().unwrap())
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names.iter().collect::<BTreeSet<_>>().len(),
        names.len(),
        "unique names"
    );
    assert!(names.len() > 320, "{} cases", names.len());
}

#[test]
fn every_recorded_record_replays() {
    let f = recorded();
    for c in f["cases"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let clock = c["clock"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_i64().unwrap())
            .collect();
        let now = c["now"].as_i64().unwrap();
        let events: Vec<JValue> = c["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| parse(&text(e)))
            .collect();
        let record = replay(clock, &events);
        let assistant = record.assistant_entry();
        let partial = record.partial_entry();
        assert_bytes(
            name,
            "assistantEntry",
            &js(assistant.as_ref()),
            &text(&c["assistantEntry"]),
        );
        assert_bytes(
            name,
            "partialEntry",
            &js(partial.as_ref()),
            &text(&c["partialEntry"]),
        );
        let keep = partial.or(assistant);
        let b = keep.as_ref().map(|k| bounded(k, now));
        let file = keep.as_ref().map(|k| partial_file(k, now));
        assert_bytes(name, "bounded", &js(b.as_ref()), &text(&c["bounded"]));
        assert_bytes(
            name,
            "partialFile",
            &js(file.as_ref()),
            &text(&c["partialFile"]),
        );
    }
}

#[test]
fn every_recorded_bounded_entry_replays() {
    let f = recorded();
    for c in f["boundedCases"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let now = c["now"].as_i64().unwrap();
        let entry = parse(&text(&c["entry"]));
        assert_bytes(
            name,
            "bounded",
            &js(Some(&bounded(&entry, now))),
            &text(&c["bounded"]),
        );
        assert_bytes(
            name,
            "partialFile",
            &js(Some(&partial_file(&entry, now))),
            &text(&c["partialFile"]),
        );
    }
}

/// A cut inside a pair keeps the lone half JS keeps: the tool result's high half at its end, the
/// newest units of content and reasoning their low half at the start.
#[test]
fn cuts_inside_a_pair_keep_the_lone_half() {
    let f = recorded();
    let lone_low = wtf16::lone_char(0xde00).unwrap();
    let lone_high = wtf16::lone_char(0xd83d).unwrap();
    let file_of = |name: &str| {
        let c = case(&f, name);
        let events: Vec<JValue> = c["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| parse(&text(e)))
            .collect();
        let file = partial_file(&kept(&replay(vec![0], &events)).unwrap(), 0);
        assert!(
            text(&c["partialFile"]).contains("\\ude00")
                || text(&c["partialFile"]).contains("\\ud83d")
        );
        file
    };
    let file = file_of("reasoning_cap_splits_pair");
    let reasoning = file.get("reasoning").as_str().unwrap();
    assert_eq!(wtf16::len(reasoning), PARTIAL_CAP_UNITS);
    assert_eq!(reasoning.chars().next(), Some(lone_low));
    assert_eq!(
        file.get("truncated"),
        &JValue::Undefined,
        "reasoning never marks truncated"
    );

    let file = file_of("content_cap_splits_pair");
    let content = file.get("content").as_str().unwrap();
    assert_eq!(wtf16::len(content), PARTIAL_CAP_UNITS);
    assert_eq!(content.chars().next(), Some(lone_low));

    let file = file_of("result_cap_splits_pair");
    let JValue::Arr(calls) = file.get("toolCalls") else {
        panic!("no calls")
    };
    let result = calls[0].get("result").as_str().unwrap();
    assert_eq!(wtf16::len(result), TOOL_RESULT_LIMIT);
    assert_eq!(result.chars().next_back(), Some(lone_high));
}

/// The events observe() threw on in Node are meta events whose route String() refuses; the port
/// ignores them (the replay above shows the record is the same).
#[test]
fn only_unconvertible_routes_throw_in_node() {
    let f = recorded();
    let mut threw = 0;
    for c in f["cases"].as_array().unwrap() {
        for i in c["threw"].as_array().into_iter().flatten() {
            let ev = parse(&text(&c["events"][i.as_u64().unwrap() as usize]));
            assert_eq!(ev.get("type").as_str(), Some("meta"), "{}", c["name"]);
            threw += 1;
        }
    }
    assert!(threw >= 2);
}

/// The whole turns recorded through handleChat: createTurnRecord fed with each turn's events.
#[test]
fn every_whole_turn_record_replays() {
    let turns = wtf16::parse(common::FIXTURE).unwrap();
    assert_eq!(
        turns.get("fixedNow").as_str(),
        Some("2026-11-13T12:00:00.000Z")
    );
    let now = 1_794_571_200_000_i64;
    let JValue::Arr(cases) = turns.get("cases") else {
        panic!("no cases")
    };
    let (mut appended, mut files) = (0, 0);
    for c in cases {
        let name = c.get("name").as_str().unwrap();
        let JValue::Arr(events) = c.get("events") else {
            panic!("{name}: no events")
        };
        let record = replay(vec![now], events);
        let want = |key: &str| js(Some(c.get("record").get(key)));
        assert_bytes(
            name,
            "record.assistantEntry",
            &js(record.assistant_entry().as_ref()),
            &want("assistantEntry"),
        );
        assert_bytes(
            name,
            "record.partialEntry",
            &js(record.partial_entry().as_ref()),
            &want("partialEntry"),
        );
        let keep = kept(&record);
        // A turn whose client left: the appended entry is `partialEntry() || assistantEntry()`.
        let JValue::Arr(stores) = c.get("stores") else {
            panic!("{name}: no stores")
        };
        // A client that left while a card was open or a tool ran: the turn still records the call's
        // result it no longer sends, so the stream alone does not rebuild the appended entry (the
        // native replay, native_turns.rs, checks those appends byte for byte).
        let left_mid_call = c.get("input").get("leaveOnCard") == &JValue::Bool(true)
            || c.get("input").get("leaveInTool").as_str().is_some();
        for store in stores
            .iter()
            .filter(|_| !left_mid_call)
            .filter(|s| s.get("op").as_str() == Some("transcripts.appendIfLast"))
        {
            assert_bytes(
                name,
                "appended entry",
                &js(keep.as_ref()),
                &js(Some(store.get("entry"))),
            );
            appended += 1;
        }
        // The store refused: the partial file is the kept entry, unmarked.
        let JValue::Obj(written) = c.get("files") else {
            panic!("{name}: no files")
        };
        for (path, file) in written
            .iter()
            .filter(|(p, _)| p.starts_with("chat-partials/"))
        {
            let ours = keep.as_ref().map(|k| partial_file(k, now));
            assert_bytes(name, path, &js(ours.as_ref()), &js(Some(file)));
            files += 1;
        }
    }
    assert_eq!(
        (appended, files),
        (3, 1),
        "three turns whose client left, one kept as a file"
    );
}
