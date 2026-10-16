#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::cfg::*;
use super::*;
use proptest::prelude::*;

#[test]
fn splitlines_matches_python() {
    assert_eq!(
        splitlines_keep("a\r\nb\rc\x0bd\u{2028}e"),
        ["a\r\n", "b\r", "c\x0b", "d\u{2028}", "e"]
    );
    assert_eq!(splitlines("a\r\nb\n"), ["a", "b"]);
    assert!(splitlines_keep("").is_empty());
}

#[test]
fn section_names_reject_a_trailing_newline() {
    // noevia#1934: Python's `$` accepted one trailing newline, which wrote an unparseable file.
    assert!(!valid_section_name("foo\n"));
    assert!(!valid_section_name("\n"));
    assert!(!valid_section_name("foo\n\n"));
    assert!(!valid_section_name(""));
    assert!(valid_section_name("foo-1.2_b+c"));
}

#[test]
fn headers_and_duplicates() {
    assert_eq!(header_match("  [a] ; c"), Some((3, 4)));
    assert_eq!(header_match("[a] junk"), None);
    assert_eq!(header_match("[]"), None);
    assert_eq!(duplicate_sections("[a]\n[b]\n[a]\n[a]\n[b]\n"), ["a", "b"]);
}

#[test]
fn configparser_reads() {
    let p = parse("v = 1\n[a]\nk = v\n  more\n\n  after blank\n# c\nk2: x = y\n[DEFAULT]\nd = 1\n")
        .unwrap();
    assert_eq!(p.preamble, "v = 1\n");
    assert_eq!(
        p.section("a").unwrap().get("k"),
        Some("v\nmore\n\nafter blank")
    );
    assert_eq!(p.section("a").unwrap().get("k2"), Some("x = y"));
    assert_eq!(p.items("a").unwrap().0[0], ("d".to_owned(), "1".to_owned()));
    assert_eq!(
        parse("[a]\nnot an option\n").unwrap_err(),
        ParseError::Invalid
    );
    assert_eq!(parse("[a]\n= v\n").unwrap_err(), ParseError::Invalid);
    assert_eq!(parse("[\nk = v\n").unwrap_err(), ParseError::Invalid);
    assert!(matches!(parse("[a]\n[a]\n"), Err(ParseError::Duplicate(_))));
}

#[test]
fn cache_ram_rules() {
    assert_eq!(clamp_cache_ram(" 007 ", 100), "7");
    assert_eq!(clamp_cache_ram("-1", 100), "100");
    assert_eq!(clamp_cache_ram("101", 100), "100");
    assert_eq!(clamp_cache_ram("lots", 100), "lots");
    assert_eq!(
        clamp_cache_ram("99999999999999999999999999999999999999999", 5),
        "5"
    );
}

#[test]
fn route_shapes() {
    use crate::{section_write_route, SectionWrite};
    use axum::http::Method;
    assert_eq!(
        section_write_route(&Method::PUT, "/api/v1/models-ini"),
        Some(SectionWrite::ModelsIni)
    );
    assert_eq!(
        section_write_route(&Method::PUT, "/api/v1/sections/a%20b"),
        Some(SectionWrite::Save("a b".into()))
    );
    assert_eq!(
        section_write_route(&Method::DELETE, "/api/v1/sections/%zz"),
        Some(SectionWrite::Delete("%zz".into()))
    );
    assert_eq!(
        section_write_route(&Method::POST, "/api/v1/sections/a/rename"),
        Some(SectionWrite::Rename("a".into()))
    );
    for (m, p) in [
        (Method::GET, "/api/v1/sections/a"),
        (Method::PUT, "/api/v1/sections/"),
        (Method::PUT, "/api/v1/sections/a/"),
        (Method::PUT, "/api/v1/sections/a%2Fb"),
        (Method::POST, "/api/v1/sections/a/other"),
        (Method::POST, "/api/v1/models-ini"),
        (Method::PUT, "/api/v1/models-ini/"),
    ] {
        assert_eq!(section_write_route(&m, p), None, "{m} {p}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // Hostile models.ini text never panics the reader or the renderers, and whatever the
    // preserving renderer returns reads back to exactly the parser it was given.
    #[test]
    fn hostile_ini_is_total(text in "[\\[\\]a-c=:;# \\t\\r\\n\\x0b\u{2028}]{0,120}", key in "[a-c]{1,3}") {
        if let Ok(mut cp) = parse(&text) {
            let _ = render_full(&cp);
            if let Some((name, _)) = cp.sections.first().cloned() {
                if let Some(s) = cp.section_mut(&name) {
                    s.set(&key, "v");
                }
                if let Some(out) = render_preserving(&text, &cp) {
                    let back = parse(&out).unwrap();
                    prop_assert!(back.same_sections(&cp.sections));
                }
                let _ = rename_header(&text, &cp, &name, "renamed");
            }
        }
    }
}
