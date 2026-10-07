//! Property tests and caps for dav-parse (noevia#967).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use dav_parse::{
    decode_uri_component, decode_xml_entities, element_texts, is_forbidden_name_char, list_entries,
    Error, MAX_BODY_BYTES, MAX_RESPONSES, MAX_TARGET_BYTES,
};
use proptest::prelude::*;

const T: &str = "https://dav.example.test/remote.php/dav/files/alice/Notes/";
const DIR: &str = "/remote.php/dav/files/alice/Notes";

fn pct_all(s: &str) -> String {
    s.bytes().map(|b| format!("%{b:02X}")).collect()
}

fn listing(hrefs: &[String]) -> String {
    hrefs
        .iter()
        .map(|h| format!("<d:response><d:href>{h}</d:href></d:response>"))
        .collect()
}

/// Names a file may have: non-empty, no '/', and not a dot segment.
fn child_name() -> impl Strategy<Value = String> {
    "[^/]{1,24}".prop_filter("not listable", |s| dav_parse::is_listable_name(s))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn never_panics_on_arbitrary_input(body in any::<String>(), target in any::<String>()) {
        let _ = list_entries(&body, &target);
        let _ = list_entries(&body, T);
    }

    #[test]
    fn never_panics_on_markup_like_input(body in r#"(<|</|>|d:|response|href|collection|getcontentlength|&|#|;|x|/|%|\.| |[0-9]){0,200}"#) {
        if let Ok(entries) = list_entries(&body, T) {
            for e in entries {
                prop_assert!(!e.name.is_empty() && !e.name.contains('/'));
            }
        }
    }

    #[test]
    fn every_entry_is_a_direct_child(names in proptest::collection::vec(any::<String>(), 0..8)) {
        let hrefs: Vec<String> = names.iter().map(|n| format!("{DIR}/{n}")).collect();
        for e in list_entries(&listing(&hrefs), T).unwrap() {
            prop_assert!(!e.name.is_empty());
            prop_assert!(!e.name.contains('/'));
            prop_assert!(e.name != ".");
        }
    }

    #[test]
    fn percent_encoded_children_list_by_their_name(names in proptest::collection::vec(child_name(), 0..8)) {
        let hrefs: Vec<String> = names.iter().map(|n| format!("{DIR}/{}", pct_all(n))).collect();
        let got: Vec<String> = list_entries(&listing(&hrefs), T).unwrap().into_iter().map(|e| e.name).collect();
        prop_assert_eq!(got, names);
    }

    #[test]
    fn hrefs_under_another_directory_never_list(names in proptest::collection::vec(any::<String>(), 0..8)) {
        let hrefs: Vec<String> = names.iter().map(|n| format!("/remote.php/dav/files/bob/{}", pct_all(n))).collect();
        prop_assert!(list_entries(&listing(&hrefs), T).unwrap().is_empty());
    }

    // noevia#969-#971: whatever the href, a listed name is never a dot segment, never carries a
    // forbidden character, and never comes from another origin.
    #[test]
    fn listed_names_are_never_hostile(
        parts in proptest::collection::vec(proptest::collection::vec(prop_oneof![
            any::<char>().prop_map(|c| c.to_string()),
            Just("..".to_owned()), Just(".".to_owned()), Just("%2e".to_owned()), Just("%2f".to_owned()), Just("%5c".to_owned()),
            Just("%00".to_owned()), Just("%7F".to_owned()), Just("%C2%85".to_owned()), Just("%E2%80%AE".to_owned()),
            Just("%E2%81%A6".to_owned()), Just("%D8%9C".to_owned()), Just("&#x200F;".to_owned()), Just("\\".to_owned()),
            (0x80u32..0xa0).prop_map(|c| char::from_u32(c).unwrap().to_string()),
            (0x2066u32..0x206a).prop_map(|c| char::from_u32(c).unwrap().to_string()),
        ], 1..6), 0..8),
        origins in proptest::collection::vec(prop_oneof![
            Just(""), Just("https://dav.example.test"), Just("https://dav.example.test:443"), Just("https://evil.example.test"),
            Just("http://dav.example.test"), Just("https://dav.example.test:8443"), Just("//evil.example.test"),
            Just("https://dav.example.test@evil.example.test"), Just("https://xn--dv-ilb.example.test"),
        ], 8),
    ) {
        let hrefs: Vec<String> = parts.iter().zip(origins.iter()).map(|(p, o)| format!("{o}{DIR}/{}", p.concat())).collect();
        let base = url::Url::parse(T).unwrap();
        for e in list_entries(&listing(&hrefs), T).unwrap() {
            prop_assert!(e.name != "." && e.name != "..", "{:?}", e.name);
            prop_assert!(!e.name.chars().any(is_forbidden_name_char), "{:?}", e.name);
        }
        // Every href that lists is one whose resolved origin is the target's.
        for h in &hrefs {
            let one = list_entries(&listing(std::slice::from_ref(h)), T).unwrap();
            if !one.is_empty() {
                let r = base.join(dav_parse::js_trim(&dav_parse::decode_xml_entities(h))).unwrap();
                prop_assert_eq!(r.origin(), base.origin(), "{}", h);
            }
        }
    }

    #[test]
    fn foreign_origins_never_list(names in proptest::collection::vec(child_name(), 0..8), host in "[a-z]{1,10}\\.(test|example)", port in proptest::option::of(1u16..)) {
        prop_assume!(host != "dav.example");
        let port = port.map(|p| format!(":{p}")).unwrap_or_default();
        let hrefs: Vec<String> = names.iter().map(|n| format!("https://{host}{port}{DIR}/{}", pct_all(n))).collect();
        prop_assert!(list_entries(&listing(&hrefs), T).unwrap().is_empty());
    }

    #[test]
    fn entity_decoding_never_grows_and_is_single_pass(s in any::<String>()) {
        let once = decode_xml_entities(&s);
        prop_assert!(once.len() <= s.len());
        if !s.contains('&') {
            prop_assert_eq!(&once, &s);
        }
    }

    #[test]
    fn escaped_text_decodes_back(s in any::<String>()) {
        let escaped = s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        prop_assert_eq!(decode_xml_entities(&escaped), s);
    }

    #[test]
    fn numeric_references_decode_scalars(c in any::<char>()) {
        prop_assume!(c != '\0');
        prop_assert_eq!(decode_xml_entities(&format!("&#{};", c as u32)), c.to_string());
        prop_assert_eq!(decode_xml_entities(&format!("&#x{:X};", c as u32)), c.to_string());
    }

    #[test]
    fn uri_decoding_round_trips(s in any::<String>()) {
        prop_assert_eq!(decode_uri_component(&pct_all(&s)), Some(s));
    }

    #[test]
    fn element_texts_finds_every_closed_element(texts in proptest::collection::vec("[^<]{0,16}", 0..10)) {
        let body: String = texts.iter().map(|t| format!("<p:x>{t}</q:x>")).collect();
        prop_assert_eq!(element_texts(&body, "x", usize::MAX), texts.iter().map(String::as_str).collect::<Vec<_>>());
    }
}

#[test]
fn body_cap() {
    let body = "a".repeat(MAX_BODY_BYTES + 1);
    assert!(matches!(
        list_entries(&body, T),
        Err(Error::BodyTooLarge { .. })
    ));
    assert!(list_entries(&body[..MAX_BODY_BYTES], T).is_ok());
}

#[test]
fn target_cap() {
    let target = format!("https://dav.example.test/{}", "a".repeat(MAX_TARGET_BYTES));
    assert_eq!(
        list_entries("", &target).unwrap_err().code(),
        "target_too_long"
    );
}

#[test]
fn response_cap() {
    let at_cap = "<response></response>".repeat(MAX_RESPONSES);
    assert!(list_entries(&at_cap, T).unwrap().is_empty());
    let over = format!("{at_cap}<response></response>");
    assert_eq!(
        list_entries(&over, T).unwrap_err(),
        Error::TooManyResponses { max: MAX_RESPONSES }
    );
}

#[test]
fn invalid_targets_are_typed() {
    for t in ["", "not a url", "https://h/%zz/", "https://h/%C3%28/"] {
        assert_eq!(
            list_entries("", t).unwrap_err(),
            Error::InvalidTarget,
            "{t}"
        );
    }
}

#[test]
fn hostile_unclosed_tags_stay_linear() {
    let body = "<d:response><d:href>".repeat(400_000); // 8 MB
    let start = std::time::Instant::now();
    assert!(list_entries(&body, T).unwrap().is_empty());
    assert!(start.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn entity_bombs_do_not_expand() {
    let body = format!(
        "<!DOCTYPE x [<!ENTITY a \"{}\">]><d:response><d:href>{DIR}/&a;&a;</d:href></d:response>",
        "&b;".repeat(1000)
    );
    let entries = list_entries(&body, T).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "&a;&a;");
}
