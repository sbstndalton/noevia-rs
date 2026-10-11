#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use proptest::prelude::*;
use server_reasoning_settings::{
    input::{parse, Error},
    put, BODY_LIMIT,
};
use server_store::{
    rusqlite::Connection, RustAuth, RustReasoningSettings, RustSamplingSettings, Writer,
};
#[test]
fn shapes_strings_duplicates_utf8_and_bounds() {
    assert_eq!(parse(b"null"), Err(Error::Null));
    for s in [
        "[]",
        "0",
        "1e999",
        "false",
        r#""low""#,
        "{}",
        r#"{"default":null}"#,
        r#"{"default":"HIGH"}"#,
        r#"{"default":{}}"#,
    ] {
        assert_eq!(parse(s.as_bytes()), Err(Error::Default), "{s}");
    }
    for s in ["", "{", r#"{"default":"low",}"#, r#"{"default":"low"}x"#] {
        assert_eq!(parse(s.as_bytes()), Err(Error::Json), "{s}");
    }
    for s in [
        r#"{"default":"low","default":"high"}"#,
        r#"{"default":"low","\u0064efault":"h\u0069gh"}"#,
        r#"{"default":"high","extra":1e999}"#,
        r#"{"default":"high","\uD800":"\uDFFF"}"#,
    ] {
        assert_eq!(parse(s.as_bytes()), Ok("high"), "{s}");
    }
    assert_eq!(
        parse(br#"{"default":"high","default":false}"#),
        Err(Error::Default)
    );
    assert_eq!(
        parse(b"{\"default\":\"high\",\"extra\":\"\xff\"}"),
        Ok("high")
    );
    assert_eq!(parse(&vec![b' '; BODY_LIMIT + 1]), Err(Error::Json));
}
proptest! {
    #[test]
    fn last_default_wins(a in prop::sample::select(vec!["default","low","high"]),b in prop::sample::select(vec!["default","low","high"]),extra in "[A-Za-z0-9]{0,1000}") {
        let raw=format!(r#"{{"default":"{a}","extra":"{extra}","\u0064efault":"{b}"}}"#);
        prop_assert_eq!(parse(raw.as_bytes()),Ok(b));
    }
}
#[test]
fn key_grant_is_exact_and_setting_plus_audit_roll_back() {
    let d = tempfile::tempdir().unwrap();
    let db = Connection::open(d.path().join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    db.execute(
        "INSERT INTO settings VALUES('reasoning_effort_default','low')",
        [],
    )
    .unwrap();
    let auth = RustAuth::from_env_value(Some("1")).unwrap();
    let capability = RustReasoningSettings::from_env_value(Some("1"), auth).unwrap();
    let writer = Writer::open(d.path(), auth).unwrap();
    for sql in [
        "INSERT OR REPLACE INTO settings VALUES('reasoning_effort_default','high')",
        "UPDATE settings SET value='high' WHERE key='reasoning_effort_default'",
        "DELETE FROM settings WHERE key='reasoning_effort_default'",
    ] {
        assert!(writer.write(|tx| tx.execute(sql, [])).is_err());
    }
    assert!(writer
        .write(|tx| tx.set_setting("reasoning_effort_default", "high"))
        .is_err());
    let sampling = RustSamplingSettings::from_env_value(Some("1"), auth).unwrap();
    writer
        .write(|tx| tx.set_sampling_enabled(sampling, false))
        .unwrap();
    assert_eq!(
        put(&writer, capability, "synthetic-admin", "high", 42).unwrap(),
        serde_json::json!({"default":"high"})
    );
    let row: (String, Option<String>, String, String, i64) = db
        .query_row(
            "SELECT actor_user_id,target_user_id,action,detail,created_at FROM audit_events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        (
            "synthetic-admin".into(),
            None,
            "reasoning.default".into(),
            "{\"effort\":\"high\"}".into(),
            42
        )
    );
    db.execute_batch("CREATE TRIGGER reject_audit BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic audit abort'); END;").unwrap();
    assert!(put(&writer, capability, "synthetic-admin", "low", 43).is_err());
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='reasoning_effort_default'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "high"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM audit_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(writer
        .write(|tx| tx.execute(
            "INSERT OR REPLACE INTO settings VALUES('reasoning_effort_default','default')",
            []
        ))
        .is_err());
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='auto_sampling_presets_enabled'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "false"
    );
}
#[test]
fn persisted_settings_triggers_cannot_widen_any_fixed_key_grant() {
    for key in [
        "reasoning_effort_default",
        "auto_sampling_presets_enabled",
        "public_origin",
    ] {
        let d = tempfile::tempdir().unwrap();
        let db = Connection::open(d.path().join("cowork.db")).unwrap();
        db.execute_batch(include_str!(
            "../../server-store/tests/fixtures/node-schema.sql"
        ))
        .unwrap();
        db.execute_batch(&format!("CREATE TRIGGER cross_key AFTER INSERT ON settings WHEN NEW.key='{key}' BEGIN INSERT OR REPLACE INTO settings VALUES('unrelated_setting','synthetic-write'); END;")).unwrap();
        let auth = RustAuth::from_env_value(Some("1")).unwrap();
        let capability = RustReasoningSettings::from_env_value(Some("1"), auth).unwrap();
        let sampling = RustSamplingSettings::from_env_value(Some("1"), auth).unwrap();
        let writer = Writer::open(d.path(), auth).unwrap();
        let write = || match key {
            "reasoning_effort_default" => {
                put(&writer, capability, "synthetic-admin", "high", 42).map(|_| ())
            }
            "auto_sampling_presets_enabled" => writer
                .write(|tx| tx.set_sampling_enabled(sampling, true))
                .map(|_| ()),
            _ => writer
                .write(|tx| tx.set_setting("public_origin", "http://synthetic.invalid"))
                .map(|_| ()),
        };
        assert!(write().is_err(), "{key}");
        assert_eq!(
            db.query_row("SELECT count(*) FROM settings", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0,
            "{key}"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM audit_events", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "{key}"
        );
        assert!(writer
            .write(|tx| tx.execute("INSERT INTO settings VALUES('unrelated_setting','raw')", []))
            .is_err());
        db.execute_batch("DROP TRIGGER cross_key").unwrap();
        write().unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM settings", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "{key}"
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM settings WHERE key='unrelated_setting'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0,
            "{key}"
        );
        assert!(writer
            .write(|tx| tx.execute("INSERT INTO settings VALUES('unrelated_setting','raw')", []))
            .is_err());
    }
}
