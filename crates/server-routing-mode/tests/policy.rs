#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use proptest::prelude::*;
use server_routing_mode::{self as rm, input::Json};
#[test]
fn node_json_and_precedence_controls() {
    for raw in [
        r#"{"mode":"local","unknown":1e999}"#,
        r#"{"mode":"local","\ud800":"\udfff"}"#,
        r#"{"mode":null,"mode":"local"}"#,
        r#"{"mode":null,"\u006dode":"local"}"#,
    ] {
        assert_eq!(
            rm::validate(&Json::parse(raw.as_bytes()).unwrap(), &rm::all_modes())
                .unwrap()
                .mode
                .as_deref(),
            Some("local")
        );
    }
    assert_eq!(
        rm::validate(
            &Json::parse(br#"{"mode":"hybrid","whenSensitive":false}"#).unwrap(),
            &[]
        )
        .unwrap_err()
        .status,
        403
    );
    assert_eq!(
        rm::validate(
            &Json::parse(br#"{"mode":"local","cloud":{"fast":" \t "}}"#).unwrap(),
            &rm::all_modes()
        )
        .unwrap_err()
        .message,
        "cloud.fast must be a model id"
    );
    for whitespace in [
        '\u{feff}', '\u{a0}', '\u{1680}', '\u{2007}', '\u{2028}', '\u{202f}', '\u{205f}',
        '\u{3000}',
    ] {
        let body=serde_json::json!({"mode":"cloud","cloud":{"providerId":"synthetic","code":format!("{whitespace}m+1{whitespace}")}}).to_string();
        assert_eq!(
            rm::validate(&Json::parse(body.as_bytes()).unwrap(), &rm::all_modes())
                .unwrap()
                .models[2],
            "m+1"
        );
    }
    for whitespace in ['\u{85}', '\u{180e}'] {
        assert_eq!(
            rm::js_trim(&format!("{whitespace}x{whitespace}")),
            format!("{whitespace}x{whitespace}")
        );
    }
    let input = Json::parse(br#"{"mode":"cloud","cloud":{"providerId":"p","fast":"m"}}"#).unwrap();
    let s = rm::validate(&input, &rm::all_modes()).unwrap();
    assert_eq!(s.view(vec!["hybrid".into()], false)["mode"], "local");
    assert_eq!(s.view(vec!["hybrid".into()], false)["storedMode"], "cloud");
    assert!(Json::parse(format!("{}0{}", "[".repeat(129), "]".repeat(129)).as_bytes()).is_err());
    assert!(Json::parse(&vec![b' '; rm::BODY_LIMIT + 1]).is_err());
    assert_eq!(
        rm::normalize(None).view(vec![], true)["mode"],
        serde_json::Value::Null
    );
    for raw in [None, Some(""), Some(" \u{feff}")] {
        assert_eq!(rm::feature_env(raw).unwrap(), None);
    }
    for raw in ["1", " TRUE ", "ON", "yes"] {
        assert_eq!(rm::feature_env(Some(raw)).unwrap(), Some(true));
    }
    for raw in ["0", "false", "OFF", " No "] {
        assert_eq!(rm::feature_env(Some(raw)).unwrap(), Some(false));
    }
    assert!(rm::feature_env(Some("enabled")).is_err());
}
proptest! {
    #[test]
    fn accepted_settings_roundtrip_and_disallowed_always_local(mode in prop::sample::select(vec!["local","cloud","hybrid"]),model in "[a-zA-Z0-9._:/@+-]{1,200}",provider in "[a-zA-Z0-9_-]{1,80}", sensitive in any::<bool>()) {
        let input=serde_json::json!({"mode":mode,"whenSensitive":if sensitive {"local"}else{"ask"},"cloud":{"providerId":provider,"fast":model}}).to_string();
        let s=rm::validate(&Json::parse(input.as_bytes()).unwrap(),&rm::all_modes()).unwrap();
        prop_assert_eq!(rm::normalize(Some(&Json::parse(s.bytes().as_bytes()).unwrap())),s.clone());
        prop_assert_eq!(&s.view(vec![],false)["mode"],"local");
        prop_assert_eq!(&s.view(vec![],false)["storedMode"],mode);
    }
    #[test]
    fn allowed_canonicalizes_only_recognized_modes(values in prop::collection::vec(prop::sample::select(vec!["local","cloud","hybrid","other"]),0..100)) {
        let text=serde_json::json!(values).to_string();
        let normalized=rm::allowed_text(&text);
        prop_assert_eq!(&normalized,&rm::MODES.into_iter().filter(|m|values.contains(m)).map(str::to_owned).collect::<Vec<_>>());
    }
}
#[test]
fn dedicated_sql_capability_and_audit_partial_commit() {
    use server_store::{rusqlite::Connection, RustAuth, RustRoutingMode, Store, Writer};
    let d = tempfile::tempdir().unwrap();
    let db = Connection::open(d.path().join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('synthetic','s','s','Synthetic','admin','x','w',1,1)",[]).unwrap();
    let auth = RustAuth::from_env_value(Some("1")).unwrap();
    let cap = RustRoutingMode::from_env_value(Some("1"), auth).unwrap();
    let writer = Writer::open(d.path(), auth).unwrap();
    assert!(writer.write(|tx| tx.set_setting(rm::KEY, "[]")).is_err());
    assert!(writer
        .write(|tx| tx.execute(
            "INSERT OR REPLACE INTO settings VALUES('routing_modes_allowed','[]')",
            []
        ))
        .is_err());
    assert!(writer
        .write(|tx| tx.set_setting(rm::FEATURE_KEY, "true"))
        .is_err());
    rm::put_allowed(&writer, cap, "synthetic", vec!["local".into()], 1).unwrap();
    db.execute_batch("CREATE TRIGGER routing_audit_abort BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic refusal'); END;").unwrap();
    assert!(rm::put_allowed(&writer, cap, "synthetic", vec![], 2).is_err());
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='routing_modes_allowed'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "[]"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM audit_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let store = Store::open(d.path()).unwrap();
    for value in ["true", "false", "TRUE", "1", "bad"] {
        db.execute(
            "INSERT OR REPLACE INTO settings VALUES('feature:routingModes',?1)",
            [value],
        )
        .unwrap();
        assert_eq!(
            store.read(|r| rm::enabled(r, None)).unwrap(),
            value == "true"
        );
        assert!(store.read(|r| rm::enabled(r, Some(true))).unwrap());
    }
}
