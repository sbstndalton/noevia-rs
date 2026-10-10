#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use server_sampling_settings::{get, put, KEY};
use server_store::{rusqlite::Connection, RustAuth, RustSamplingSettings, Store, Writer};
fn fixture() -> (tempfile::TempDir, RustAuth, RustSamplingSettings) {
    let d = tempfile::tempdir().unwrap();
    let db = Connection::open(d.path().join("cowork.db")).unwrap();
    let schema = include_str!("../../server-store/tests/fixtures/node-schema.sql")
        .replace("value TEXT NOT NULL", "value");
    db.execute_batch(&schema).unwrap();
    db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('a','a','a','Synthetic','admin','x','a',1,1)",[]).unwrap();
    let auth = RustAuth::from_env_value(Some("1")).unwrap();
    let sampling = RustSamplingSettings::from_env_value(Some("1"), auth).unwrap();
    (d, auth, sampling)
}
#[test]
fn raw_sqlite_values_only_exact_text_false_disables() {
    let (d, _, _) = fixture();
    let db = Connection::open(d.path().join("cowork.db")).unwrap();
    let s = Store::open(d.path()).unwrap();
    assert_eq!(s.read(|r| get(r, false)).unwrap()["enabled"], true);
    for (sql, want) in [
        ("NULL", true),
        ("'false'", false),
        ("'FALSE'", true),
        ("'true'", true),
        ("0", true),
        ("1", true),
        ("x'66616c7365'", true),
    ] {
        db.execute_batch(&format!(
            "INSERT OR REPLACE INTO settings VALUES('{KEY}',{sql})"
        ))
        .unwrap();
        let v = s.read(|r| get(r, true)).unwrap();
        assert_eq!(v["enabled"], want, "{sql}");
        assert_eq!(v["admin"], true);
        assert_eq!(v["version"], 1);
    }
}
#[test]
fn distinct_key_capability_audit_and_failure_rollback() {
    let (d, auth, sampling) = fixture();
    let w = Writer::open(d.path(), auth).unwrap();
    assert!(RustSamplingSettings::from_env_value(None, auth).is_none());
    assert!(w.write(|tx| tx.set_setting(KEY, "false")).is_err());
    assert!(w
        .write(|tx| tx.execute(
            "INSERT INTO settings VALUES('auto_sampling_presets_enabled','false')",
            []
        ))
        .is_err());
    assert!(w
        .write(|tx| tx.execute("UPDATE settings SET value='false'", []))
        .is_err());
    assert_eq!(put(&w, sampling, "a", false, 12).unwrap()["enabled"], false);
    let db = Connection::open(d.path().join("cowork.db")).unwrap();
    let row: (String, String, String, i64) = db
        .query_row(
            "SELECT actor_user_id,action,detail,created_at FROM audit_events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        (
            "a".into(),
            "sampling.autoPresets".into(),
            "{\"enabled\":false}".into(),
            12
        )
    );
    let target: Option<String> = db
        .query_row("SELECT target_user_id FROM audit_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(target, None);
    db.execute_batch("CREATE TRIGGER reject_audit BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic failure'); END;").unwrap();
    assert!(put(&w, sampling, "a", true, 13).is_err());
    let value: String = db
        .query_row("SELECT value FROM settings WHERE key=?1", [KEY], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(value, "false");
    let count: i64 = db
        .query_row("SELECT count(*) FROM audit_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
    // Closing the special grant still refuses arbitrary settings SQL afterward.
    assert!(w
        .write(|tx| tx.execute("DELETE FROM settings", []))
        .is_err());
}
