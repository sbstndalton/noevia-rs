//! Properties of the tool-gate port: no input panics; a rule or a Stage 2 answer never picks a tool
//! that is not offered or not read-only; a prefetch's arguments always fit the tool (every required
//! name is the one argument written, and the tool lists it when it lists any); a URL is prefetched
//! only when it is ASCII, http(s), credential-free, not an `xn--`, local or private name (with or
//! without trailing dots); a search is prefetched only by the rule stage and only for a short
//! single-line message; Stage 2 options never offer a write; and messages at the size cap stay
//! linear (or are refused, which the host treats as "none"). The JS side of "never more permissive
//! than the JS" runs against the real JS in noevia-core's differential test (every fixture row and
//! seeded live calls through the switch).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::js::units;
use proptest::prelude::*;
use std::time::{Duration, Instant};
use tool_gate::{
    call, options, public_url, rule, stage2, Args, Decision, Key, Offered, Patterns, Read,
    Selected, Tool, Work, MAX_INPUT_BYTES,
};

fn work() -> Work {
    Work::new(1 << 34)
}

const NAMES: [&str; 12] = [
    "tavily_extract",
    "web_fetch",
    "fetch_url",
    "tavily_search",
    "web_search",
    "diary_read_month",
    "diary_read_today",
    "diary_list_months",
    "nc_webdav_search_files",
    "project_search",
    "write_file",
    "calc",
];

fn tool() -> impl Strategy<Value = Tool> {
    (
        prop::sample::select(NAMES.to_vec()),
        any::<bool>(),
        0u8..32,
        0u8..32,
        any::<bool>(),
        prop::collection::vec(
            prop::option::of(prop::sample::select(vec![
                "url", "urls", "query", "q", "month", "lang", "x",
            ])),
            0..3,
        ),
        "[a-zA-Z ]{0,30}",
    )
        .prop_map(
            |(name, read_only, own, truthy, any_props, required, d)| Tool {
                name: units(name),
                read_only,
                description: units(&d),
                own,
                truthy: truthy & own,
                any_props: any_props || own != 0,
                required: required.into_iter().map(|r| r.map(units)).collect(),
            },
        )
}

fn message() -> impl Strategy<Value = String> {
    let parts = prop::sample::select(vec![
        "search",
        "latest news",
        "look up",
        "weather",
        "my diary",
        "yesterday I",
        "3 March",
        "march 2024",
        "2026-09-01",
        "today",
        "my files",
        "folder",
        "https://example.com/a.",
        "http://nas.local../",
        "http://10.0.0.1/",
        "https://xn--exmple-cua.com/",
        "https://exämple.com/",
        "http://user@example.com/",
        "please",
        "can you",
        "\n",
        "```",
        ">",
        " ",
        "\u{a0}",
        "?!",
        "x",
    ]);
    prop::collection::vec(parts, 0..12).prop_map(|v| v.join(" "))
}

fn boxes() -> tool_gate::Boxes {
    let b = |k: &str, names: &[&str]| (units(k), names.iter().map(|n| Some(units(n))).collect());
    vec![
        b(
            "url",
            &["tavily_extract", "web_fetch", "fetch_url", "browse"],
        ),
        b(
            "search",
            &["tavily_search", "web_search", "wikipedia_search"],
        ),
        b(
            "diary",
            &["diary_read_month", "diary_read_today", "diary_list_months"],
        ),
        b("drive", &["nc_webdav_search_files", "project_search"]),
        b("extra", &["calc", "write_file"]),
    ]
}

fn check_decision(d: &Decision, tools: &[Tool], message: &str, stage_rule: bool) {
    let name = match d {
        Decision::Prefetch { tool, .. } | Decision::Require { tool } => tool,
    };
    // The Map keeps the last value for a repeated name.
    let t = tools
        .iter()
        .rev()
        .find(|t| &t.name == name)
        .expect("offered");
    assert!(t.read_only, "a write was picked");
    let Decision::Prefetch { args, .. } = d else {
        return;
    };
    match args {
        Args::Empty => assert!(t.required.is_empty()),
        Args::One(k, v) => {
            assert!(t.required.iter().all(|r| r.as_deref()
                == Some(
                    &units(match k {
                        Key::Urls => "urls",
                        Key::Url => "url",
                        Key::Query => "query",
                        Key::Q => "q",
                        Key::Month => "month",
                    })[..]
                )));
            match k {
                Key::Urls | Key::Url => assert!(public_url(v, &mut work()).unwrap()),
                Key::Query | Key::Q => {
                    assert!(stage_rule, "a search prefetch from Stage 2");
                    assert!(message.trim().chars().count() <= 120 && !message.contains('\n'));
                }
                Key::Month => {}
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1500, ..ProptestConfig::default() })]

    #[test]
    fn random_bytes_never_panic(op in 0u8..8, body in prop::collection::vec(any::<u8>(), 0..300)) {
        let mut input = vec![op];
        input.extend(body);
        let (status, reply) = call(&input);
        prop_assert!(status <= 1);
        prop_assert!(!reply.is_empty());
    }

    #[test]
    fn rules_pick_offered_read_only_tools_with_fitting_args(
        tools in prop::collection::vec(tool(), 0..8), m in message(), now in -8.7e15f64..8.7e15,
    ) {
        let pats = Patterns::new();
        let offered = Offered::new(&tools, &mut work()).unwrap();
        if let Some((_, d)) = rule(&pats, &units(&m), &offered, &boxes(), now, &mut work()).unwrap() {
            check_decision(&d, &tools, &m, true);
        }
    }

    #[test]
    fn answers_pick_offered_read_only_tools(
        tools in prop::collection::vec(tool(), 0..8), m in message(), pick in 0usize..14,
        c in prop::option::of(0.0f64..1.0), min in 0.0f64..1.0,
    ) {
        let pats = Patterns::new();
        let offered = Offered::new(&tools, &mut work()).unwrap();
        let selected = NAMES.get(pick).map_or(Selected::Nothing, |n| Selected::Name(units(n)));
        let read = stage2(&pats, &units(&m), &offered, &boxes(), 0.0, &selected,
            (f64::NAN, f64::NAN, c.unwrap_or(f64::NAN)), min, &mut work()).unwrap();
        if let Read::Decision(d) = read {
            check_decision(&d, &tools, &m, false);
            prop_assert!(c.unwrap_or(0.0) >= min);
        }
    }

    #[test]
    fn options_never_offer_a_write(tools in prop::collection::vec(tool(), 0..30), limited in any::<bool>(),
        max_options in 0.0f64..30.0, max_choice in 0.0f64..3000.0) {
        let offered = Offered::new(&tools, &mut work()).unwrap();
        let limits = limited.then_some(tool_gate::Limits { max_options, max_label: 120.0, max_choice });
        let (_, opts) = options(&offered, None, None, limits, &boxes(), &mut work()).unwrap();
        for o in &opts {
            if o.id == units("none") { continue; }
            let t = tools.iter().rev().find(|t| t.name == o.id).unwrap();
            prop_assert!(t.read_only);
        }
        prop_assert!(opts.len() <= 25);
    }

    #[test]
    fn public_urls_are_plain_public_names(host in "[a-z0-9.-]{0,20}", dots in 0usize..3,
        suffix in prop::sample::select(vec!["", ".com", ".local", ".internal", ".lan", ".home.arpa", ".corp", ".intranet", ".localhost"])) {
        let raw = format!("http://{host}{suffix}{}/", ".".repeat(dots));
        if public_url(&units(&raw), &mut work()).unwrap() {
            let h = url::Url::parse(&raw).unwrap().host_str().unwrap().trim_end_matches('.').to_string();
            prop_assert!(h.contains('.'));
            for s in ["local", "internal", "lan", "home.arpa", "corp", "intranet", "localhost"] {
                prop_assert!(h != s && !h.ends_with(&format!(".{s}")), "{raw}");
            }
            prop_assert!(!h.split('.').any(|l| l.starts_with("xn--")));
        }
    }
}

fn timed(input: &[u8]) -> (u32, Duration) {
    let t = Instant::now();
    let (status, _) = call(input);
    (status, t.elapsed())
}

fn request(message: &str) -> Vec<u8> {
    let tool = r#"{"name":"web_fetch","readOnly":true,"description":"","own":["url"],"truthy":["url"],"anyProps":true,"required":["url"]}"#;
    let mut out = vec![1u8];
    out.extend(
        format!(
            r#"[{},[{tool}],0,[["url",["web_fetch"]],["search",["web_fetch"]]]]"#,
            serde_like(message)
        )
        .as_bytes(),
    );
    out
}

fn serde_like(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[test]
fn messages_at_the_cap_stay_linear() {
    // Adversarial shapes for each pattern: the trailing-punctuation strip (quadratic in the JS),
    // long whitespace runs after a keyword, many near-miss dates, keyword soup.
    let n = MAX_INPUT_BYTES - 400;
    let cases = [
        format!("see http://a.com/{}x", ".,".repeat(n / 2 - 20)),
        format!("look{}x", " ".repeat(n - 10)),
        "1 ".repeat(n / 2 - 4),
        "march ".repeat(n / 6 - 2),
        "2026-1".repeat(n / 6 - 2),
        "search the web for ".repeat(n / 19 - 2),
        format!("1st{}z", "\u{a0}".repeat((n - 10) / 2)),
    ];
    for m in &cases {
        let input = request(m);
        assert!(input.len() <= MAX_INPUT_BYTES, "{}", input.len());
        let (status, took) = timed(&input);
        // Shared CI runners: a loose bound; the point is linear (seconds), not quadratic (hours).
        assert!(
            took < Duration::from_secs(20),
            "{took:?} for a {}-byte message",
            m.len()
        );
        assert!(status <= 1);
    }
    // Over the cap: refused before parsing.
    let big = vec![1u8; MAX_INPUT_BYTES + 1];
    let (status, took) = timed(&big);
    assert_eq!(status, 1);
    assert!(took < Duration::from_millis(500));
}

#[test]
fn the_quadratic_js_case_is_linear_here() {
    // tool-gate.cjs: `.replace(/[.,;:!?]+$/, '')` on the matched URL backtracks quadratically
    // (80,000 units take seconds in V8). Here it is one backwards scan.
    let m = format!("see http://a.com/{}x", ".,".repeat(200_000));
    let input = request(&m);
    let (status, took) = timed(&input);
    assert_eq!(status, 0);
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn a_million_unit_word_soup_is_answered_not_refused() {
    // "a " * 500,000 with the default boxes: every unit is a start position for several patterns.
    let tool = r#"{"name":"project_search","readOnly":true,"description":"","own":["query"],"truthy":["query"],"anyProps":true,"required":[]}"#;
    let boxes = r#"[["url",["tavily_extract","web_fetch","fetch_url","browse"]],["search",["tavily_search","web_search","wikipedia_search"]],["diary",["diary_read_month","diary_read_today","diary_list_months"]],["drive",["nc_webdav_search_files","drive_search_files","nc_webdav_find_by_name","nc_webdav_list_directory","project_search"]]]"#;
    let mut input = vec![1u8];
    input.extend(format!(r#"["{}",[{tool}],0,{boxes}]"#, "a ".repeat(500_000)).as_bytes());
    let (status, reply) = call(&input);
    assert_eq!(status, 0, "{}", String::from_utf8_lossy(&reply));
}
