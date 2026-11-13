//! The turn record replayed against Node: `fixtures/node-turn-record.v1.json` (noevia-core's
//! `createTurnRecord`, `bounded()` and `finish({ keep })` over synthetic QA event sequences,
//! written by `record_entries.cjs`) and every whole turn of `node-turns.v1.json`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod common;

use std::collections::BTreeSet;

use chat_turn::record::{bounded, partial_file, TurnRecord, PARTIAL_CAP_UNITS};
use common::{cases, js_len, same_json};
use serde_json::{Map, Value};

const RECORDED: &str = include_str!("fixtures/node-turn-record.v1.json");

/// The fixture with its `{ "$str": [[codePoint, count], ...] }` runs written out.
fn recorded() -> Value {
    decode(serde_json::from_str(RECORDED).unwrap())
}

fn decode(v: Value) -> Value {
    match v {
        Value::Array(items) => Value::Array(items.into_iter().map(decode).collect()),
        Value::Object(m) => {
            if m.len() == 1 {
                if let Some(Value::Array(runs)) = m.get("$str") {
                    let mut s = String::new();
                    for run in runs {
                        let ch = char::from_u32(run[0].as_u64().unwrap() as u32).unwrap();
                        for _ in 0..run[1].as_u64().unwrap() {
                            s.push(ch);
                        }
                    }
                    return Value::String(s);
                }
            }
            Value::Object(m.into_iter().map(|(k, v)| (k, decode(v))).collect())
        }
        other => other,
    }
}

/// `null` for `None`, as the recording writes Node's `null`.
fn or_null(v: Option<Value>) -> Value {
    v.unwrap_or(Value::Null)
}

fn assert_same(name: &str, what: &str, got: &Value, want: &Value) {
    assert!(
        same_json(got, want),
        "{name}: {what}\n got: {}\nwant: {}",
        short(got),
        short(want)
    );
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 2000 {
        format!(
            "{}… ({} bytes)",
            s.chars().take(2000).collect::<String>(),
            s.len()
        )
    } else {
        s
    }
}

/// A record fed `events` with a clock answering `clock` in order, the last value repeating.
fn replay(clock: Vec<i64>, events: &[Value]) -> TurnRecord {
    let last = *clock.last().unwrap();
    let mut ticks = clock.into_iter();
    let mut record = TurnRecord::new(move || ticks.next().unwrap_or(last));
    for ev in events {
        record.observe(ev);
    }
    record
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
    assert!(names.len() > 300, "{} cases", names.len());
}

#[test]
fn every_recorded_record_replays() {
    let f = recorded();
    let mut checked = 0;
    for c in f["cases"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let clock: Vec<i64> = c["clock"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_i64().unwrap())
            .collect();
        let now = c["now"].as_i64().unwrap();
        let events = c["events"].as_array().unwrap();
        let record = replay(clock, events);
        let assistant = or_null(record.assistant_entry());
        let partial = or_null(record.partial_entry());
        assert_same(name, "assistantEntry", &assistant, &c["assistantEntry"]);
        assert_same(name, "partialEntry", &partial, &c["partialEntry"]);
        // chat.cjs keeps `partialEntry() || assistantEntry()`.
        let keep = if partial.is_null() {
            &assistant
        } else {
            &partial
        };
        let (b, file) = if keep.is_null() {
            (Value::Null, Value::Null)
        } else {
            (bounded(keep, now), partial_file(keep, now))
        };
        assert_same(name, "bounded", &b, &c["bounded"]);
        assert_same(name, "partialFile", &file, &c["partialFile"]);
        checked += 1;
    }
    assert!(checked > 300);
}

#[test]
fn every_recorded_bounded_entry_replays() {
    let f = recorded();
    for c in f["boundedCases"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let now = c["now"].as_i64().unwrap();
        assert_same(name, "bounded", &bounded(&c["entry"], now), &c["bounded"]);
        assert_same(
            name,
            "partialFile",
            &partial_file(&c["entry"], now),
            &c["partialFile"],
        );
    }
}

/// The named difference: where Node's cut kept a lone surrogate, the recording holds Node's value
/// without it, and the port's value is that one unit shorter than the cap.
#[test]
fn a_cut_inside_a_pair_is_the_only_lone_surrogate_difference() {
    let f = recorded();
    let mut seen = BTreeSet::new();
    for c in f["cases"].as_array().unwrap() {
        let Some(paths) = c["loneDropped"].as_array() else {
            continue;
        };
        for path in paths {
            let path = path.as_str().unwrap();
            let mut at = c;
            for part in path.split('.') {
                at = match part.parse::<usize>() {
                    Ok(i) => &at[i],
                    Err(_) => &at[part],
                };
            }
            let kept = js_len(at.as_str().unwrap());
            let field = path.rsplit('.').next().unwrap();
            let cap = if field == "result" {
                4000
            } else {
                PARTIAL_CAP_UNITS
            };
            assert_eq!(kept, cap - 1, "{}: {path}", c["name"]);
            seen.insert(field.to_owned());
        }
    }
    assert_eq!(
        seen,
        BTreeSet::from(["content".to_owned(), "result".to_owned()]),
        "both cuts are recorded inside a pair"
    );
    // The events observe() threw on in Node are meta events whose route String() refuses.
    for c in f["cases"].as_array().unwrap() {
        for i in c["threw"].as_array().into_iter().flatten() {
            let ev = &c["events"][i.as_u64().unwrap() as usize];
            assert_eq!(ev["type"], "meta", "{}", c["name"]);
        }
    }
}

/// The whole turns recorded through handleChat: createTurnRecord fed with each turn's events.
#[test]
fn every_whole_turn_record_replays() {
    let fixed = common::fixture()["fixedNow"].as_str().unwrap().to_owned();
    assert_eq!(fixed, "2026-11-13T12:00:00.000Z");
    let now = 1_794_571_200_000_i64;
    for c in cases() {
        let name = c["name"].as_str().unwrap();
        let record = replay(vec![now], c["events"].as_array().unwrap());
        let assistant = or_null(record.assistant_entry());
        let partial = or_null(record.partial_entry());
        assert_same(
            name,
            "record.assistantEntry",
            &assistant,
            &c["record"]["assistantEntry"],
        );
        assert_same(
            name,
            "record.partialEntry",
            &partial,
            &c["record"]["partialEntry"],
        );
        let keep = if partial.is_null() {
            &assistant
        } else {
            &partial
        };
        // A turn whose client left: the appended entry is `partialEntry() || assistantEntry()`.
        let appended: Vec<&Value> = c["stores"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["op"] == "transcripts.appendIfLast")
            .collect();
        for store in &appended {
            assert_same(name, "appended entry", keep, &store["entry"]);
        }
        // The store refused: the partial file is the kept entry, unmarked.
        let files: &Map<String, Value> = c["files"].as_object().unwrap();
        for (path, file) in files
            .iter()
            .filter(|(p, _)| p.starts_with("chat-partials/"))
        {
            assert_same(name, path, &partial_file(keep, now), file);
        }
    }
    // The turns whose client left: two appended, one the store refused and kept as a file.
    let left = |c: &Value| {
        c["stores"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["op"] == "transcripts.appendIfLast")
    };
    assert_eq!(cases().iter().filter(|c| left(c)).count(), 3);
    let files = common::case("stopped_store_refuses")["files"].clone();
    assert!(files
        .as_object()
        .unwrap()
        .keys()
        .any(|p| p.starts_with("chat-partials/")));
}
