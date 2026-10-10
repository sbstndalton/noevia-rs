use proptest::prelude::*;
use server_sampling_settings::{
    input::{parse, Error},
    BODY_LIMIT,
};
#[test]
fn shapes_duplicates_unicode_and_ignored_numbers() {
    for raw in [
        "null",
        "[]",
        "[true]",
        "0",
        "1e999",
        "false",
        "\"text\"",
        "{}",
        "{\"enabled\":null}",
    ] {
        assert_eq!(parse(raw.as_bytes()), Err(Error::Enabled), "{raw}");
    }
    for raw in ["", "{", "{\"enabled\":true,}", "{\"enabled\":true}x"] {
        assert_eq!(parse(raw.as_bytes()), Err(Error::Json), "{raw}");
    }
    for raw in [
        r#"{"enabled":false,"enabled":true}"#,
        r#"{"enabled":false,"\u0065nabled":true}"#,
        r#"{"enabled":true,"extra":1e999}"#,
        r#"{"enabled":true,"\uD800":"\uDFFF"}"#,
    ] {
        assert_eq!(parse(raw.as_bytes()), Ok(true), "{raw}");
    }
    assert_eq!(
        parse(br#"{"enabled":true,"enabled":null}"#),
        Err(Error::Enabled)
    );
    assert_eq!(parse(b"{\"enabled\":true,\"extra\":\"\xff\"}"), Ok(true));
    assert_eq!(parse(&vec![b' '; BODY_LIMIT + 1]), Err(Error::Json));
    let deep = format!("{}{}{}", "[".repeat(129), "0", "]".repeat(129));
    assert_eq!(parse(deep.as_bytes()), Err(Error::Json));
}
proptest! {
 #[test]
 fn last_duplicate_boolean_wins(a in any::<bool>(),b in any::<bool>(),extra in "[a-zA-Z0-9]{0,1000}") {
  let raw=format!(r#"{{"enabled":{a},"extra":"{extra}","\u0065nabled":{b}}}"#);
  prop_assert_eq!(parse(raw.as_bytes()),Ok(b));
 }
}
