#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use proptest::prelude::*;

fn json(text: &str) -> JsonValue {
    crate::prompts::parse_json(text).unwrap()
}

#[test]
fn route_int_accepts_python_integers_and_refuses_the_rest() {
    for (text, expected) in [
        ("0", Ok(0)),
        ("null", Ok(0)),
        ("false", Ok(0)),
        ("\"\"", Ok(0)),
        ("[]", Ok(0)),
        ("3", Ok(3)),
        ("3.0", Ok(3)),
        ("-2.0", Ok(-2)),
        ("\" 4 \"", Ok(4)),
        ("\"+5\"", Ok(5)),
        ("\"-7\"", Ok(-7)),
        ("\"1_0\"", Ok(10)),
        ("1e300", Err(NotInt::Big)),
        ("100000000000000000000", Err(NotInt::Big)),
        ("\"99999999999999999999\"", Err(NotInt::Big)),
        ("true", Err(NotInt::Other)),
        ("2.9", Err(NotInt::Other)),
        ("[1]", Err(NotInt::Other)),
        ("{\"a\":1}", Err(NotInt::Other)),
        ("\"abc\"", Err(NotInt::Other)),
        ("\"1__0\"", Err(NotInt::Other)),
        ("\"_1\"", Err(NotInt::Other)),
        ("\"1_\"", Err(NotInt::Other)),
        ("\"3.0\"", Err(NotInt::Other)),
        ("\" \"", Err(NotInt::Other)),
        ("\"+\"", Err(NotInt::Other)),
    ] {
        assert_eq!(route_int(Some(&json(text))), expected, "{text}");
    }
    assert_eq!(route_int(None), Ok(0));
}

#[test]
fn validation_order_matches_python() {
    let v = |text: &str| validate(&json(text));
    assert_eq!(
        v(r#"{"rating":"x","alias":""}"#).unwrap_err(),
        "rating must be a number"
    );
    assert_eq!(
        v(r#"{"rating":3,"runId":"x","alias":""}"#).unwrap_err(),
        "runId must be a number"
    );
    assert_eq!(
        v(r#"{"rating":1e300,"alias":""}"#).unwrap_err(),
        "no model given"
    );
    assert_eq!(
        v(r#"{"rating":1e300,"alias":"m","category":"x"}"#).unwrap_err(),
        "unknown category 'x'"
    );
    assert_eq!(
        v(r#"{"rating":1e300,"alias":"m","category":"coding"}"#).unwrap_err(),
        "rating must be 1-5"
    );
    let ok =
        v(r#"{"rating":"2","alias":" m ","category":"vision","note":" n ","runId":"0"}"#).unwrap();
    assert_eq!(
        ok,
        Rating {
            alias: "m".into(),
            category: "vision".into(),
            rating: 2,
            note: "n".into(),
            run_id: None
        }
    );
}

#[test]
fn delete_query_last_value_and_missing_list() {
    assert_eq!(
        delete_query("alias=a&alias=b+c&category=x").unwrap(),
        ("b c".into(), "x".into())
    );
    assert_eq!(
        delete_query("alias=&category=").unwrap(),
        (String::new(), String::new())
    );
    assert_eq!(
        String::from_utf8(delete_query("category=x").unwrap_err()).unwrap(),
        r#"{"detail":[{"type":"missing","loc":["query","alias"],"msg":"Field required","input":null}]}"#
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // Hostile bodies never panic, and anything accepted is within the closed vocabulary.
    #[test]
    fn hostile_bodies_are_total(
        alias in "\\PC{0,40}",
        category in "\\PC{0,12}",
        rating in "\\PC{0,12}",
        note in "\\PC{0,700}",
    ) {
        let body = serde_json::json!({"alias": alias, "category": category, "rating": rating, "note": note});
        if let Ok(r) = validate(&json(&body.to_string())) {
            prop_assert!(CATEGORIES.contains(&r.category.as_str()));
            prop_assert!((1..=5).contains(&r.rating));
            prop_assert!(!r.alias.is_empty());
            prop_assert!(r.note.chars().count() <= NOTE_CHARS);
        }
    }

    // Hostile query strings never panic.
    #[test]
    fn hostile_queries_are_total(query in "\\PC{0,200}") {
        let _ = delete_query(&query);
    }
}
