//! Replays `fixtures/node-helpers.v1.json` (written by `record_helpers.cjs` from noevia-core's
//! search-query, announced-action, final-answer and tool-route-notes files) against
//! `chat_turn::helpers`, with exact equality. Strings are read with `js_json::wtf16`, so the lone
//! surrogates Node recorded reach the Rust functions as they reached Node's.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use chat_turn::helpers::{self, js_regex, RouteInput};
use js_json::{wtf16, JValue};
use std::sync::LazyLock;

const FIXTURE: &str = include_str!("fixtures/node-helpers.v1.json");

static RECORDED: LazyLock<JValue> = LazyLock::new(|| wtf16::parse(FIXTURE).expect("fixture"));

fn get<'a>(v: &'a JValue, key: &str) -> &'a JValue {
    let out = v.get(key);
    assert!(
        !matches!(out, JValue::Undefined),
        "no {key} in the recording"
    );
    out
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        other => panic!("not an array: {other:?}"),
    }
}

fn s(v: &JValue) -> &str {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v:?}"))
}

fn strings(v: &JValue) -> Vec<String> {
    arr(v).iter().map(|x| s(x).to_string()).collect()
}

fn section(name: &str) -> &'static [JValue] {
    arr(get(&RECORDED, name))
}

#[test]
fn records_the_four_core_files() {
    let JValue::Obj(sources) = get(&RECORDED, "sources") else {
        panic!("sources")
    };
    let names: Vec<&str> = sources.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "server/search-query.cjs",
            "server/announced-action.cjs",
            "server/final-answer.cjs",
            "server/tool-route-notes.cjs"
        ]
    );
    for (_, sha) in sources {
        assert_eq!(s(sha).len(), 64);
    }
    assert!(helpers::patterns_compile());
}

#[test]
fn constants_match_node() {
    let c = get(&RECORDED, "constants");
    assert_eq!(s(get(c, "ANNOUNCED_NOTE")), helpers::ANNOUNCED_NOTE);
    assert_eq!(s(get(c, "FINAL_ANSWER_NOTE")), helpers::FINAL_ANSWER_NOTE);
    assert_eq!(
        get(c, "TAIL_CHARS").as_num(),
        Some(helpers::TAIL_CHARS as f64)
    );
    assert_eq!(
        get(c, "LAST_TOOL_ROUND").as_num(),
        Some(f64::from(helpers::LAST_TOOL_ROUND))
    );
    assert_eq!(
        get(c, "MAX_SOURCES").as_num(),
        Some(helpers::MAX_SOURCES as f64)
    );
}

#[test]
fn js_space_is_nodes_space_on_every_unit() {
    let ranges: Vec<(u32, u32)> = section("space")
        .iter()
        .map(|r| {
            let r = arr(r);
            (r[0].as_num().unwrap() as u32, r[1].as_num().unwrap() as u32)
        })
        .collect();
    for u in 0..=u16::MAX {
        let node = ranges
            .iter()
            .any(|&(a, b)| a <= u32::from(u) && u32::from(u) <= b);
        assert_eq!(js_regex::is_js_space(u), node, "unit {u:#06x}");
    }
}

#[test]
fn clean_search_query_as_node() {
    let cases = section("cleanSearchQuery");
    assert!(cases.len() > 1000);
    for case in cases {
        let [input, want] = arr(case) else {
            panic!("case")
        };
        assert_eq!(
            helpers::clean_search_query(s(input)),
            s(want),
            "query {input:?}"
        );
    }
}

#[test]
fn clean_search_args_as_node_with_the_named_difference() {
    let cases = section("cleanSearchArgs");
    let mut differences = 0;
    for case in cases {
        let (label, name, raw, want) = (
            s(get(case, "label")),
            s(get(case, "name")),
            s(get(case, "raw")),
            s(get(case, "out")),
        );
        let got = helpers::clean_search_args(name, raw);
        if case.get("difference").as_str() == Some("unsupported") {
            // Named difference: js_json refuses these arguments; Rust keeps them, Node rewrote them.
            assert_eq!(got, raw, "{label}");
            assert_ne!(
                want, raw,
                "{label}: Node no longer rewrites it, drop the difference"
            );
            differences += 1;
        } else {
            assert_eq!(got, want, "{label} {name:?} {raw:?}");
        }
    }
    assert_eq!(differences, 2);
}

#[test]
fn announces_tool_action_as_node() {
    let cases = section("announcesToolAction");
    let mut yes = 0;
    for case in cases {
        let [input, want] = arr(case) else {
            panic!("case")
        };
        let want = want.as_bool().unwrap();
        yes += usize::from(want);
        assert_eq!(
            helpers::announces_tool_action(s(input)),
            want,
            "reply {input:?}"
        );
    }
    assert!(yes > 100 && yes < cases.len() - 100);
}

#[test]
fn final_answer_helpers_as_node() {
    for case in section("needsFinalPass") {
        let [round, calls, content, want] = arr(case) else {
            panic!("case")
        };
        let got = helpers::needs_final_pass(
            round.as_num().unwrap() as u32,
            calls.as_num().unwrap() as usize,
            content.as_bool().unwrap(),
        );
        assert_eq!(Some(got), want.as_bool(), "{case:?}");
    }
    for case in section("sourceLinks") {
        let [input, want] = arr(case) else {
            panic!("case")
        };
        assert_eq!(
            helpers::source_links(s(input)),
            strings(want),
            "text {input:?}"
        );
    }
    for case in section("fallbackText") {
        let [sources, want] = arr(case) else {
            panic!("case")
        };
        assert_eq!(helpers::fallback_text(&strings(sources)), s(want));
    }
}

#[test]
fn tool_route_notes_as_node() {
    let recorded = get(&RECORDED, "toolRouteNotes");
    let notes = strings(get(recorded, "notes"));
    for case in arr(get(recorded, "cases")) {
        let (offered, on_server) = (
            strings(get(case, "offered")),
            strings(get(case, "onServer")),
        );
        let defaults = RouteInput::default();
        let flag = |key: &str, default: bool| case.get(key).as_bool().unwrap_or(default);
        let input = RouteInput {
            message: s(get(case, "message")),
            offered: &offered,
            on_server: &on_server,
            automatic: flag("automatic", defaults.automatic),
            local_model: flag("localModel", defaults.local_model),
            gate_none: flag("gateNone", defaults.gate_none),
            automatic_web: get(case, "automaticWeb").as_bool().unwrap(),
        };
        let want = &notes[get(case, "out").as_num().unwrap() as usize];
        assert_eq!(&helpers::tool_route_notes(&input), want, "{case:?}");
    }
}

/// Inputs no recording covers: long, repetitive, lone-surrogate-heavy and pseudo-random text.
/// Nothing may panic, recursion must not grow with the text, and results keep their invariants.
#[test]
fn odd_inputs_do_not_panic() {
    let mut seed = 0x5eed_u32;
    let mut next = || {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        seed >> 8
    };
    let alphabet: Vec<char> = "aIiLlet'’ ms\u{a0}\u{feff}\u{200b}.!?:;,-\"cite sources for https://qa.invalid/ 😀\u{10F800}\u{10FBFF}{}[]\\query"
        .chars()
        .collect();
    for _ in 0..400 {
        let len = (next() % 400) as usize;
        let text: String = (0..len)
            .map(|_| alphabet[next() as usize % alphabet.len()])
            .collect();
        let cleaned = helpers::clean_search_query(&text);
        assert!(cleaned == text || js_json::wtf16::len(&cleaned) >= 3);
        let _ = helpers::announces_tool_action(&text);
        assert!(helpers::source_links(&text).len() <= helpers::MAX_SOURCES);
        let _ = helpers::clean_search_args("web_search", &text);
        let args = format!("{{\"query\":\"{}\"}}", text.replace(['"', '\\'], ""));
        let _ = helpers::clean_search_args("web_search", &args);
        let _ = helpers::tool_route_notes(&RouteInput {
            message: &text,
            ..RouteInput::default()
        });
    }
    let long = format!(
        "Search the web: {} cite sources for {}x",
        " ".repeat(3000),
        "a ".repeat(3000)
    );
    let _ = helpers::clean_search_query(&long);
    let spaces = format!("x{}y", "\u{3000}".repeat(100_000));
    assert_eq!(helpers::clean_search_query(&spaces), spaces);
    assert!(!helpers::announces_tool_action(
        &"I will read ".repeat(50_000).replace("read", "readx")
    ));
    assert_eq!(
        helpers::source_links(&format!("https://{}", "a".repeat(100_000))).len(),
        0
    );
    let deep = format!(
        "{{\"query\":\"Search the web: QA deep\",\"x\":{}1{}}}",
        "[".repeat(100_000),
        "]".repeat(100_000)
    );
    assert_eq!(helpers::clean_search_args("search", &deep), deep);
}
