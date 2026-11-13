//! Differential test: the follow-up resolver must reproduce the frozen Python module on the
//! synthetic cases in `follow-up-fixtures.json` (made by `generate_follow_up_fixtures.py`).

use diary_core::follow_up::{
    casefold, conversation_note, elliptical_name, is_category_c, mentions, resolve, safe_name,
    today_note, Resolution,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixtures() -> Result<Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_str(include_str!(
        "follow-up-fixtures.json"
    ))?)
}

fn array<'a>(fixtures: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    fixtures
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing {key}"))
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string {key}"))
}

fn resolution(value: &Value) -> Result<Resolution, String> {
    Ok(Resolution {
        standalone: text(value, "standalone")?.to_owned(),
        earlier_name: text(value, "earlier_name")?.to_owned(),
        asked_name: text(value, "asked_name")?.to_owned(),
        near: value
            .get("near")
            .and_then(Value::as_bool)
            .ok_or("missing near")?,
    })
}

#[test]
fn fixtures_record_the_pinned_python_and_unicode() -> TestResult {
    let fx = fixtures()?;
    assert_eq!(text(&fx, "python_version")?, "3.12.15");
    assert_eq!(text(&fx, "unidata_version")?, "15.0.0");
    // Every code point matching the pattern's literal letters under re.I is the ASCII pair.
    let probe = fx.get("ci_literal_matches").ok_or("probe")?;
    for (letter, matched) in probe.as_object().ok_or("probe object")? {
        let upper = letter.to_uppercase();
        let want: Vec<Value> = if upper == *letter {
            vec![]
        } else {
            let mut cps: Vec<u32> = letter.chars().chain(upper.chars()).map(u32::from).collect();
            cps.sort_unstable();
            cps.into_iter().map(Value::from).collect()
        };
        assert_eq!(matched.as_array().ok_or("probe list")?, &want, "{letter}");
    }
    Ok(())
}

#[test]
fn elliptical_pattern_matches_python_regex() -> TestResult {
    let fx = fixtures()?;
    let cases = array(&fx, "elliptical")?;
    assert!(cases.len() >= 10000);
    let mut matched = 0;
    for case in cases {
        let input = text(case, "text")?;
        let expected = case.get("name").and_then(Value::as_str);
        assert_eq!(elliptical_name(input), expected, "input {input:?}");
        matched += usize::from(expected.is_some());
    }
    assert!(matched >= 2000, "only {matched} matching cases");
    Ok(())
}

#[test]
fn resolve_matches_python() -> TestResult {
    let fx = fixtures()?;
    let cases = array(&fx, "resolve")?;
    assert!(cases.len() >= 100);
    let mut resolved = 0;
    for case in cases {
        let name = text(case, "name")?;
        let turns: Vec<Value> = case
            .get("turns")
            .and_then(Value::as_array)
            .cloned()
            .ok_or("turns")?;
        let actual = resolve(text(case, "message")?, &turns)?;
        let expected = case.get("expected").ok_or("expected")?;
        if expected.is_null() {
            assert_eq!(actual, None, "case {name}");
        } else {
            resolved += 1;
            assert_eq!(actual, Some(resolution(expected)?), "case {name}");
        }
    }
    assert!(resolved >= 50);
    Ok(())
}

#[test]
fn notes_match_python_for_both_exact_loaded_values() -> TestResult {
    let fx = fixtures()?;
    let cases = array(&fx, "notes")?;
    assert!(cases.len() >= 60);
    for case in cases {
        let name = text(case, "name")?;
        let res = resolution(case.get("resolution").ok_or("resolution")?)?;
        assert_eq!(
            conversation_note(&res, false),
            text(case, "exact_loaded_false")?,
            "{name}"
        );
        // Python's default is exact_loaded=False.
        assert_eq!(
            conversation_note(&res, false),
            text(case, "default_arg")?,
            "{name}"
        );
        assert_eq!(
            conversation_note(&res, true),
            text(case, "exact_loaded_true")?,
            "{name}"
        );
        assert_eq!(today_note(&res), text(case, "today")?, "{name}");
    }
    Ok(())
}

#[test]
fn safe_name_matches_python() -> TestResult {
    let fx = fixtures()?;
    let cases = array(&fx, "safe_name")?;
    assert!(cases.len() >= 40);
    for case in cases {
        assert_eq!(
            safe_name(text(case, "input")?),
            text(case, "expected")?,
            "{}",
            text(case, "name")?
        );
    }
    Ok(())
}

#[test]
fn mentions_matches_python() -> TestResult {
    let fx = fixtures()?;
    let cases = array(&fx, "mentions")?;
    assert!(cases.len() >= 35);
    for case in cases {
        let expected = case
            .get("expected")
            .and_then(Value::as_bool)
            .ok_or("expected")?;
        assert_eq!(
            mentions(text(case, "text")?, text(case, "needle")?),
            expected,
            "{}",
            text(case, "name")?
        );
    }
    Ok(())
}

/// Digest over `<hex code point>:<result>\n` for every Unicode scalar, as the generator computes it.
fn digest(function: impl Fn(char) -> String) -> String {
    let mut hash = Sha256::new();
    for scalar in (0..=0x10_ffff_u32).filter_map(char::from_u32) {
        hash.update(format!("{:x}:{}\n", u32::from(scalar), function(scalar)).as_bytes());
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn every_scalar_matches_python_for_safe_name_and_casefold() -> TestResult {
    let fx = fixtures()?;
    let digests = fx.get("single_char_digests").ok_or("digests")?;
    assert_eq!(
        digest(|c| safe_name(&c.to_string())),
        text(digests, "safe_name")?
    );
    assert_eq!(
        digest(|c| casefold(&c.to_string())),
        text(digests, "casefold")?
    );
    // The category table itself: Python's category is C* exactly for these.
    assert!(is_category_c('\u{0}') && is_category_c('\u{ad}') && is_category_c('\u{378}'));
    assert!(is_category_c('\u{e000}') && is_category_c('\u{10ffff}'));
    assert!(!is_category_c('a') && !is_category_c('\u{a0}') && !is_category_c('\u{2028}'));
    Ok(())
}
