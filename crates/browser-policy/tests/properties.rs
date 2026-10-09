//! Properties of the browser-policy port: no input panics; text the port does not know is never
//! allowed through a decision that reads it; a consequential word never lets a click through; a
//! form button that is not `button`/`reset` always asks; an accepted navigation is http(s), carries
//! no credentials, is not a local name with or without trailing dots and is on the allowlist; a
//! substitution is accepted only when every placeholder names a listed secret bound to the origin's
//! host; and adversarial inputs at the size cap stay linear. A request over the cap is refused
//! before it is parsed and an exhausted budget stops at once (both under 10 ms); a full 4 MiB
//! request that is parsed and then refused takes as long as reading it. The
//! JS side of "never more permissive than the JS" runs against the real JS in noevia-core's
//! differential test (every fixture row and seeded live calls through the switch).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use browser_policy::{
    call, classify, navigation, substitute, Action, Decision, Element, Nav, Page, Work,
    MAX_INPUT_BYTES,
};
use prompt_framing::js::units;
use proptest::prelude::*;
use std::time::{Duration, Instant};

fn work() -> Work {
    Work::new(1 << 32)
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "[a-zA-Z0-9 +._-]{0,12}",
        prop::sample::select(vec![
            "Send",
            "send now",
            "Place order",
            "check out",
            "Delete",
            "Next",
            "Read more",
            "Löschen",
            "Ｐａｙ",
            "ﬁnish",
            "Ω",
            "İ",
            " ",
            "\u{a0}",
            "\u{3000}",
            "Enter",
            "Shift+Enter",
            "Space",
            "Return",
            "+",
            "submit",
            "button",
            "reset",
            "image",
            "input",
            "a",
            "summary",
            "xyz",
            "link",
            "option",
            "🛒",
            "\u{600}",
        ])
        .prop_map(String::from),
    ]
}

fn element() -> impl Strategy<Value = Element> {
    (
        text(),
        text(),
        text(),
        text(),
        text(),
        text(),
        any::<bool>(),
    )
        .prop_map(|(tag, kind, role, name, text, value, in_form)| Element {
            tag: units(&tag),
            kind: units(&kind),
            role: units(&role),
            name: units(&name),
            text: units(&text),
            value: units(&value),
            in_form,
        })
}

fn unknown_text(s: &[u16]) -> bool {
    char::decode_utf16(s.iter().copied())
        .any(|r| r.map_or(true, |c| !browser_policy::is_known(u32::from(c))))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn random_bytes_never_panic(op in 0u8..6, body in prop::collection::vec(any::<u8>(), 0..200)) {
        let mut input = vec![op];
        input.extend(body);
        let (status, reply) = call(&input);
        prop_assert!(status <= 1);
        prop_assert!(!reply.is_empty());
    }

    #[test]
    fn random_json_never_panics(op in 1u8..5, s in "[\\[\\]{}\",:a-z0-9 ]{0,80}") {
        let mut input = vec![op];
        input.extend(s.as_bytes());
        let _ = call(&input);
    }

    #[test]
    fn clicks_and_presses_are_never_laxer_than_their_facts(el in element(), key in text(), press in any::<bool>()) {
        let action = Action { kind: units(if press { "press" } else { "click" }), key: units(&key), ..Action::default() };
        let v = classify(&action, &el, &Page::default(), &mut work()).unwrap();
        // A form button that is not type=button/reset submits (when the decision is the click's).
        let folded_kind = String::from_utf16_lossy(&el.kind).trim().to_ascii_lowercase();
        let folded_tag = String::from_utf16_lossy(&el.tag).trim().to_ascii_lowercase();
        if !press && el.in_form && folded_tag == "button" && !unknown_text(&el.kind) && !unknown_text(&el.tag)
            && folded_kind != "button" && folded_kind != "reset" {
            prop_assert_eq!(v.status, Some(Decision::NeedsApproval));
        }
        // Unknown label text never yields a clean allow from a click that read it.
        if !press && [&el.name, &el.text, &el.value].iter().any(|t| unknown_text(t)) {
            prop_assert_ne!(v.status, Some(Decision::Allow));
        }
        // Adding a consequential word never turns a click into an allow.
        if !press {
            let mut louder = el.clone();
            louder.text.extend(units(" send"));
            let w = classify(&action, &louder, &Page::default(), &mut work()).unwrap();
            prop_assert_ne!(w.status, Some(Decision::Allow));
        }
        // An unknown status carries no reason.
        if v.status.is_none() {
            prop_assert!(v.reason.is_empty());
        }
    }

    #[test]
    fn accepted_navigation_is_on_the_list(
        scheme in prop::sample::select(vec!["http", "https", "HTTPS", "ftp", "javascript", "ws"]),
        user in prop::sample::select(vec!["", "u@", "u:p@", ":p@", "@"]),
        host in prop::sample::select(vec!["example.com", "a.example.com", "example.com.", "example.com..",
            "notexample.com", "localhost", "localhost.", "corp.internal", "corp.internal.", "nas.local..",
            "127.0.0.1", "[::1]", "10.1.2.3", "EXAMPLE.COM", "example.com.evil.net", "x.localhost.", "0x7f.1"]),
        port in prop::sample::select(vec!["", ":443", ":8080", ":0"]),
        path in "[a-z/%?#.]{0,10}",
        domains in prop::collection::vec(prop::sample::select(vec!["example.com", "*.example.com", ".example.com.",
            "corp.internal", "localhost", "local", "com", "", "*."]), 0..3),
    ) {
        let url = format!("{scheme}://{user}{host}{port}/{path}");
        let domains: Vec<Vec<u16>> = domains.iter().map(|d| units(d)).collect();
        if let Nav::Ok(origin) = navigation(&units(&url), &domains, &mut work()).unwrap() {
            let parsed = url::Url::parse(&url).unwrap();
            prop_assert!(matches!(parsed.scheme(), "http" | "https"));
            prop_assert!(parsed.username().is_empty() && parsed.password().is_none_or(str::is_empty));
            let h = parsed.host_str().unwrap().trim_end_matches('.').to_string();
            prop_assert!(!(h == "localhost" || h.ends_with(".localhost") || h.ends_with(".internal") || h.ends_with(".local")));
            prop_assert!(!matches!(parsed.host(), Some(url::Host::Ipv4(_) | url::Host::Ipv6(_))) || !ssrf_like_private(&h));
            let h1 = parsed.host_str().unwrap().strip_suffix('.').unwrap_or(parsed.host_str().unwrap()).to_string();
            let on_list = domains.iter().any(|d| {
                let d = String::from_utf16(d).unwrap().to_ascii_lowercase();
                let d = d.strip_prefix("*.").or_else(|| d.strip_prefix('.')).unwrap_or(&d).to_string();
                let d = d.strip_suffix('.').unwrap_or(&d).to_string();
                !d.is_empty() && (h1 == d || h1.ends_with(&format!(".{d}")))
            });
            prop_assert!(on_list, "{} {:?}", url, domains);
            prop_assert_eq!(String::from_utf16(&origin).unwrap(), parsed.origin().ascii_serialization());
        }
    }

    #[test]
    fn substitution_needs_every_name_bound(
        names in prop::collection::vec(prop::sample::select(vec!["a", "b", "gh", "x.y", "missing", "A-1"]), 0..5),
        glue in prop::sample::select(vec!["", " ", "&", "{", "}", "{{secret:", "x"]),
        origin in prop::sample::select(vec!["https://github.com", "https://api.github.com", "https://example.com", "about:blank", "nope"]),
    ) {
        let secrets = vec![
            (units("a"), vec![units("github.com")]),
            (units("b"), vec![units("example.com")]),
            (units("gh"), vec![units("*.github.com")]),
            (units("x.y"), vec![]),
        ];
        let text: String = names.iter().map(|n| format!("{{{{secret:{n}}}}}{glue}")).collect();
        if let Ok(used) = substitute(&units(&text), &secrets, &units(origin), &mut work()).unwrap() {
            let host = url::Url::parse(origin).unwrap().host_str().unwrap_or("").to_string();
            for n in &used {
                let (_, d) = secrets.iter().find(|(k, _)| k == n).expect("a listed secret");
                let bound = d.iter().any(|d| {
                    let d = String::from_utf16(d).unwrap();
                    let d = d.strip_prefix("*.").unwrap_or(&d).to_string();
                    host == d || host.ends_with(&(String::from(".") + &d))
                });
                prop_assert!(bound);
            }
        }
    }
}

/// The literals the property above uses are all private (ssrf-policy decides the real ones).
fn ssrf_like_private(h: &str) -> bool {
    ["127.0.0.1", "::1", "[::1]", "10.1.2.3"].contains(&h)
}

fn timed(input: &[u8]) -> (u32, Duration) {
    // Best of three, so a scheduler hiccup does not fail the bound.
    (0..3)
        .map(|_| {
            let t = Instant::now();
            let (status, _) = call(input);
            (status, t.elapsed())
        })
        .min_by_key(|&(_, d)| d)
        .unwrap()
}

fn request(op: u8, json: &str) -> Vec<u8> {
    let mut v = vec![op];
    v.extend(json.as_bytes());
    v
}

fn el_json(label: &str) -> String {
    format!(
        r#"{{"tag":"a","type":"","role":"","name":"","text":{label:?},"value":"","inForm":false}}"#
    )
}

#[test]
fn adversarial_inputs_at_the_cap_stay_linear() {
    let n = MAX_INPUT_BYTES / 2 - 1024;
    let page = r#"{"origin":"","allowedDomains":[]}"#;
    let action = r#"{"type":"click","url":null,"method":null,"key":null}"#;
    let cases = [
        // Long labels: one token, many tokens, near-miss phrases, folded text.
        request(1, &format!("[{action},{},{page}]", el_json(&"a".repeat(n)))),
        request(
            1,
            &format!("[{action},{},{page}]", el_json(&"check ".repeat(n / 6))),
        ),
        request(
            1,
            &format!(
                "[{action},{},{page}]",
                el_json(&"place  order ".repeat(n / 13))
            ),
        ),
        request(
            1,
            &format!("[{action},{},{page}]", el_json(&"Löschen ".repeat(n / 9))),
        ),
        request(
            1,
            &format!(
                r#"[{{"type":"press","url":null,"method":null,"key":{:?}}},{},{page}]"#,
                "+".repeat(n),
                el_json("")
            ),
        ),
        // Navigation: a long host against many domains; dots.
        request(
            2,
            &format!(
                r#"["https://{}example.com/",[{}]]"#,
                "a.".repeat(n / 4),
                vec![r#""example.org""#; n / 32].join(",")
            ),
        ),
        request(
            2,
            &format!(
                r#"["https://example.com{}/",["example.com"]]"#,
                ".".repeat(n)
            ),
        ),
        // Placeholders: many near-misses, many hits, many secrets.
        request(
            3,
            &format!(
                r#"[{:?},[],"https://example.com"]"#,
                "{{secret:".repeat(n / 10)
            ),
        ),
        request(
            3,
            &format!(
                r#"[{:?},[["a",["example.com"]]],"https://example.com"]"#,
                "{{secret:a}}".repeat(n / 13)
            ),
        ),
        request(
            3,
            &format!(
                r#"["{{{{secret:z}}}}",[{}],"https://example.com"]"#,
                vec![r#"["y",["example.com"]]"#; n / 24].join(",")
            ),
        ),
        request(4, &format!("[[{:?}]]", "Ｓ\u{a0}".repeat(n / 8))),
    ];
    for (k, input) in cases.iter().enumerate() {
        assert!(
            input.len() <= MAX_INPUT_BYTES,
            "case {k}: {} bytes",
            input.len()
        );
        let (status, d) = timed(input);
        assert!(status <= 1, "case {k}");
        // Linear at ~2 MiB; generous for a loaded CI runner.
        assert!(d < Duration::from_millis(1500), "case {k}: {d:?}");
    }
}

#[test]
fn refusals_are_fast() {
    // Over the cap: refused before parsing.
    let big = vec![b'['; MAX_INPUT_BYTES + 1];
    let (status, d) = timed(&big);
    assert_eq!(status, 1);
    assert!(d < Duration::from_millis(10), "{d:?}");
    // Out of work: refused at the first charge past the budget, not after the work.
    let label = units(&"send ".repeat(200_000));
    let el = Element {
        text: label,
        ..Element::default()
    };
    let t = Instant::now();
    let r = classify(
        &Action {
            kind: units("click"),
            ..Action::default()
        },
        &el,
        &Page::default(),
        &mut Work::new(1000),
    );
    assert!(r.is_err());
    assert!(t.elapsed() < Duration::from_millis(10), "{:?}", t.elapsed());
    // Malformed JSON is refused while reading it.
    let (status, d) = timed(&request(1, &format!("[{}", "[".repeat(1_000_000))));
    assert_eq!(status, 1);
    assert!(d < Duration::from_millis(200), "{d:?}");
}
