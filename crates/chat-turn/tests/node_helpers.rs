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
fn clean_search_args_as_node_with_the_named_differences() {
    let (mut unsupported, mut throws) = (0, 0);
    for case in section("cleanSearchArgs") {
        let (label, name, raw) = (
            s(get(case, "label")),
            s(get(case, "name")),
            s(get(case, "raw")),
        );
        let got = helpers::clean_search_args(name, raw);
        match case.get("difference").as_str() {
            Some("unsupported") => {
                // Named difference 1: js_json refuses these arguments; Rust keeps them, Node rewrote them.
                assert_eq!(got, raw, "{label}");
                assert_ne!(
                    s(get(case, "out")),
                    raw,
                    "{label}: Node no longer rewrites it"
                );
                unsupported += 1;
            }
            Some("throws") => {
                // Named difference 2: Node's JSON.stringify ran out of stack and threw; Rust keeps them.
                assert_eq!(s(get(case, "throws")), "RangeError", "{label}");
                assert!(matches!(case.get("out"), JValue::Undefined), "{label}");
                assert_eq!(got, raw, "{label}");
                throws += 1;
            }
            _ => assert_eq!(got, s(get(case, "out")), "{label} {name:?} {raw:?}"),
        }
    }
    assert_eq!((unsupported, throws), (2, 1));
}

/// A recorded value with every string as its UTF-16 code units.
fn as_units(v: &JValue) -> JValue {
    match v {
        JValue::Str(t) => JValue::Arr(
            helpers::units(t)
                .into_iter()
                .map(|u| JValue::Num(f64::from(u)))
                .collect(),
        ),
        JValue::Arr(items) => JValue::Arr(items.iter().map(as_units).collect()),
        other => other.clone(),
    }
}

/// Inputs holding U+10F800..U+10FFFF: in js_json::wtf16 form (the caller rule) those characters
/// are lone surrogates, so the Rust result is Node's result for the lone-surrogate string.
#[test]
fn wtf16_form_of_the_reserved_range_is_read_as_node_reads_lone_surrogates() {
    let mut differs = 0;
    for case in section("outOfDomain") {
        let (input, mapped, real) = (
            s(get(case, "input")),
            get(case, "mapped"),
            get(case, "real"),
        );
        assert!(input.chars().any(|c| wtf16::lone_unit(c).is_some()));
        differs += usize::from(&as_units(mapped) != real);
        match s(get(case, "fn")) {
            "cleanSearchQuery" => assert_eq!(helpers::clean_search_query(input), s(mapped)),
            "cleanSearchArgs" => {
                assert_eq!(helpers::clean_search_args("web_search", input), s(mapped))
            }
            "announcesToolAction" => {
                assert_eq!(
                    Some(helpers::announces_tool_action(input)),
                    mapped.as_bool()
                )
            }
            "sourceLinks" => assert_eq!(helpers::source_links(input), strings(mapped)),
            other => panic!("unknown {other}"),
        }
    }
    assert!(differs > 0);
}

/// Seeded random token soup recorded from Node, through every helper.
#[test]
fn random_text_as_node() {
    let notes = strings(get(get(&RECORDED, "toolRouteNotes"), "notes"));
    let cases = section("random");
    assert!(cases.len() >= 500);
    let (gdrive, web) = (["gdrive".to_string()], ["web-search".to_string()]);
    for case in cases {
        let text = s(get(case, "text"));
        assert_eq!(
            helpers::clean_search_query(text),
            s(get(case, "query")),
            "{text:?}"
        );
        assert_eq!(
            Some(helpers::announces_tool_action(text)),
            get(case, "announces").as_bool(),
            "{text:?}"
        );
        assert_eq!(
            helpers::source_links(text),
            strings(get(case, "links")),
            "{text:?}"
        );
        // JSON.stringify({ query: text, n: 1 }), lone surrogates escaped as Node writes them.
        let args = JValue::Obj(vec![
            ("query".into(), JValue::Str(text.into())),
            ("n".into(), JValue::Num(1.0)),
        ]);
        let raw = wtf16::stringify(&args).unwrap();
        assert_eq!(
            helpers::clean_search_args("web_search", &raw),
            s(get(case, "args")),
            "{text:?}"
        );
        let input = RouteInput {
            message: text,
            offered: &gdrive,
            on_server: &web,
            automatic: get(case, "automatic").as_bool().unwrap(),
            gate_none: true,
            automatic_web: get(case, "automaticWeb").as_bool().unwrap(),
            ..RouteInput::default()
        };
        let want = &notes[get(case, "notes").as_num().unwrap() as usize];
        assert_eq!(&helpers::tool_route_notes(&input), want, "{text:?}");
    }
}

/// Inputs V8 cleans in linear (repeated wrappers) or bounded time (a long `CITE_TAIL` scan):
/// the recorded result, well under two seconds each even in a debug build.
#[test]
fn hostile_queries_as_node_in_bounded_time() {
    for case in section("hostile") {
        let [input, want] = arr(case) else {
            panic!("case")
        };
        let started = std::time::Instant::now();
        assert_eq!(helpers::clean_search_query(s(input)), s(want));
        let took = started.elapsed();
        assert!(
            took.as_secs_f64() < 2.0,
            "{} units took {took:?}",
            s(input).len()
        );
    }
}

/// Recursion depth follows the pattern, not the text: megabyte inputs on a 256 KiB stack.
#[test]
fn recursion_does_not_grow_with_the_text() {
    let run = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let big = "Search the web: I will read https://qa.invalid/x. ".repeat(20_000);
            let spaced = format!("cite sources for {}x", "a ".repeat(200_000));
            for text in [&big, &spaced, &" ".repeat(1_000_000)] {
                let _ = helpers::clean_search_query(text);
                let _ = helpers::announces_tool_action(text);
                let _ = helpers::source_links(text);
                let _ = helpers::tool_route_notes(&RouteInput {
                    message: text,
                    ..RouteInput::default()
                });
            }
        });
    run.unwrap().join().unwrap();
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

/// Inputs no recording covers (long, repetitive, deeply nested, lone-surrogate-heavy): nothing
/// panics. Results are checked against Node in `random_text_as_node`.
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
        let _ = helpers::clean_search_query(&text);
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
