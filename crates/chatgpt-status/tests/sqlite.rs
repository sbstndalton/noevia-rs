#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use chatgpt_status::{Error, SqliteService};
use rusqlite::Connection;
use std::{
    os::unix::fs::symlink,
    time::{Duration, Instant},
};
struct Fixture {
    dir: tempfile::TempDir,
    db: Connection,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secrets.key"), [7; 32]).unwrap();
        let db = Connection::open(dir.path().join("cowork.db")).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY,applied_at INTEGER NOT NULL); INSERT INTO schema_migrations VALUES(5,0); CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY,data_enc TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'connected',updated_at INTEGER NOT NULL);").unwrap();
        Self { dir, db }
    }
    fn service(&self, env: Option<&str>) -> Result<SqliteService, Error> {
        SqliteService::open(&self.dir.path().canonicalize().unwrap(), None, env)
    }
    fn read(&self, env: Option<&str>, owner: &str) -> Result<(u16, String), Error> {
        self.service(env)?.submit(owner, Instant::now())?.finish()
    }
    fn row(&self, owner: &str, payload: &str, state: &str) {
        self.db
            .execute(
                "INSERT OR REPLACE INTO chatgpt_oauth_tokens VALUES(?1,?2,?3,0)",
                rusqlite::params![owner, payload, state],
            )
            .unwrap();
    }
}
#[test]
fn actual_schema_and_product_false_before_oauth() {
    let f = Fixture::new();
    f.db.execute_batch("DROP TABLE chatgpt_oauth_tokens")
        .unwrap();
    assert_eq!(
        f.read(None, "u1").map(|a| a.0),
        Ok(404),
        "ROUTING_ASSERT/sqlite/false_before_oauth"
    );
    assert_eq!(f.read(Some("true"), "u1"), Err(Error::Unavailable));
}
#[test]
fn persisted_typed_and_env_override() {
    let f = Fixture::new();
    for v in ["false", "TRUE", "true\0", "", "junk"] {
        f.db.execute(
            "INSERT OR REPLACE INTO settings VALUES('feature:chatgptOAuth',?1)",
            [v],
        )
        .unwrap();
        assert_eq!(f.read(None, "u").unwrap().0, 404);
        assert_eq!(f.read(Some(" yes "), "u").unwrap().0, 200);
    }
    f.db.execute(
        "INSERT OR REPLACE INTO settings VALUES('feature:chatgptOAuth','true')",
        [],
    )
    .unwrap();
    assert_eq!(f.read(None, "u").unwrap().0, 200);
    assert_eq!(f.read(Some("OFF"), "u").unwrap().0, 404);
    f.db.execute(
        "INSERT OR REPLACE INTO settings VALUES('feature:chatgptOAuth',x'74727565')",
        [],
    )
    .unwrap();
    assert_eq!(f.read(None, "u").unwrap().0, 404);
    assert!(f.service(Some("invalid")).is_err());
}
#[test]
fn tenant_and_runtime_caps() {
    let f = Fixture::new();
    f.row(
        "u1",
        r#"{"email":"x@y","plan":{"token-shaped":"arbitrary"}}"#,
        "connected",
    );
    let a = f.read(Some("true"), "u1").unwrap();
    assert!(a.1.contains("arbitrary"));
    assert_eq!(
        f.read(Some("true"), "u2")
            .map(|a| a.1.contains("disconnected")),
        Ok(true),
        "ROUTING_ASSERT/sqlite/tenant"
    );
    for (field, value) in [("state", "x".repeat(65)), ("data_enc", "x".repeat(262145))] {
        f.db.execute(
            &format!("UPDATE chatgpt_oauth_tokens SET {field}=?1"),
            [value],
        )
        .unwrap();
        assert_eq!(
            f.read(Some("true"), "u1"),
            Err(Error::Unavailable),
            "ROUTING_ASSERT/sqlite/precopy"
        );
        f.row("u1", "{}", "connected");
    }
    for field in ["state", "data_enc"] {
        f.db.execute(
            &format!("UPDATE chatgpt_oauth_tokens SET {field}=x'7b7d'"),
            [],
        )
        .unwrap();
        assert_eq!(f.read(Some("true"), "u1"), Err(Error::Unavailable));
        f.row("u1", "{}", "connected");
    }
    assert!(f.read(Some("true"), &"u".repeat(1025)).is_err());
    f.row("u1", r#"{"plan":1}"#, "x".repeat(64).as_str());
    assert!(f.read(Some("true"), "u1").unwrap().1.contains("reconnect"));
}
#[test]
fn missing_and_incompatible_schema_and_null_pk() {
    for ddl in ["DROP TABLE settings", "ALTER TABLE settings ADD COLUMN extra TEXT", "DROP TABLE chatgpt_oauth_tokens; CREATE TABLE chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY COLLATE NOCASE,data_enc TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'connected',updated_at INTEGER NOT NULL)","DROP TABLE chatgpt_oauth_tokens; CREATE VIEW chatgpt_oauth_tokens AS SELECT 'u' user_id,'{}' data_enc,'connected' state,0 updated_at", "UPDATE schema_migrations SET version=6", "INSERT INTO chatgpt_oauth_tokens VALUES(NULL,'{}','connected',0)"] { let f=Fixture::new(); f.db.execute_batch(ddl).unwrap(); assert_eq!(f.read(Some("true"),"u"),Err(Error::Unavailable),"ROUTING_ASSERT/sqlite/schema"); }
}
#[test]
fn snapshots_refresh_without_mutating() {
    let f = Fixture::new();
    let s = f.service(None).unwrap();
    assert_eq!(
        s.submit("u", Instant::now()).unwrap().finish().unwrap().0,
        404
    );
    f.db.execute(
        "INSERT INTO settings VALUES('feature:chatgptOAuth','true')",
        [],
    )
    .unwrap();
    assert_eq!(
        s.submit("u", Instant::now()).unwrap().finish().unwrap().0,
        200
    );
    f.row("u", "{}", "reconnect");
    assert!(s
        .submit("u", Instant::now())
        .unwrap()
        .finish()
        .unwrap()
        .1
        .contains("reconnect"));
    f.db.execute_batch("ALTER TABLE chatgpt_oauth_tokens ADD COLUMN x TEXT")
        .unwrap();
    assert_eq!(
        s.submit("u", Instant::now()).unwrap().finish(),
        Err(Error::Unavailable)
    );
}
#[test]
fn admission_cancel_shutdown_and_reap() {
    let f = Fixture::new();
    let s = f.service(Some("true")).unwrap();
    assert!(s
        .submit("u", Instant::now() - Duration::from_secs(3))
        .is_err());
    let j = s.submit("u", Instant::now()).unwrap();
    assert!(s.submit("u", Instant::now()).is_err());
    j.cancel();
    assert!(j.finish().is_err());
    let j = s.submit("u", Instant::now()).unwrap();
    let _ = j.finish();
    for _ in 0..12 {
        s.submit("u", Instant::now()).unwrap().finish().unwrap();
    }
    let j = s.submit("u", Instant::now()).unwrap();
    s.shutdown().unwrap();
    let _ = j.finish();
    assert!(s.submit("u", Instant::now()).is_err());
}
#[test]
fn root_database_companion_negatives() {
    let f = Fixture::new();
    let s = f.service(Some("true")).unwrap();
    std::fs::rename(f.dir.path().join("cowork.db"), f.dir.path().join("old.db")).unwrap();
    symlink("old.db", f.dir.path().join("cowork.db")).unwrap();
    assert!(s.submit("u", Instant::now()).unwrap().finish().is_err());
    assert!(f.service(None).is_err());
}
#[test]
fn actual_real_node_sqlite_cases() {
    let packet: serde_json::Value =
        serde_json::from_str(include_str!("node-sqlite.v1.json")).unwrap();
    assert_eq!(packet["core"], "010735c7d86d30269ab3f4a351b2135faea1c8e7");
    assert_eq!(packet["node"], "22.23.3");
    let cases = packet["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 14);
    for c in cases {
        let f = Fixture::new();
        if let Some(p) = c["previous"].as_u64() {
            std::fs::write(f.dir.path().join("secrets.key.previous"), [p as u8; 32]).unwrap();
        }
        if let Some(v) = c["setting"].as_str() {
            f.db.execute(
                "INSERT INTO settings VALUES('feature:chatgptOAuth',?1)",
                [v],
            )
            .unwrap();
        }
        if let Some(enc) = c["ciphertext"].as_str() {
            f.row(
                c["owner"].as_str().unwrap(),
                enc,
                c["state"].as_str().unwrap(),
            );
        }
        let actual = f
            .read(c["env"].as_str(), c["owner"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            serde_json::json!({"code":actual.0,"body":actual.1}),
            c["answer"],
            "ROUTING_ASSERT/sqlite/actual_node {}",
            c["id"]
        );
    }
}
#[test]
fn exact_caps_nul_utf8_and_key_snapshot() {
    let f = Fixture::new();
    f.row("u", "{}", "connected\0");
    assert!(f.read(Some("true"), "u").unwrap().1.contains("reconnect"));
    f.row("u", "{}", &"😀".repeat(17));
    assert_eq!(f.read(Some("true"), "u"), Err(Error::Unavailable));
    f.row("u", &"x".repeat(262144), "connected");
    assert_eq!(f.read(Some("true"), "u"), Err(Error::Limit));
    assert_eq!(f.read(Some("true"), &"u".repeat(1024)).unwrap().0, 200);
    let enc = secret_envelope::seal(&[7; 32], &[3; 12], b"{}", Some(b"u")).unwrap();
    f.row("u", &enc, "connected");
    let s = f.service(Some("true")).unwrap();
    std::fs::write(f.dir.path().join("secrets.key"), [9; 32]).unwrap();
    assert!(s
        .submit("u", Instant::now())
        .unwrap()
        .finish()
        .unwrap()
        .1
        .contains("connected"));
    drop(s);
    assert!(f.read(Some("true"), "u").unwrap().1.contains("reconnect"));
}
#[test]
fn metadata_object_bounds_and_companion_negatives() {
    let f = Fixture::new();
    for i in 0..257 {
        f.db.execute_batch(&format!("CREATE TABLE extra{i}(x)"))
            .unwrap();
    }
    assert_eq!(f.read(Some("true"), "u"), Err(Error::Unavailable)); // Feature-off schema reads stay restricted to base tables.
    assert_eq!(f.read(Some("false"), "u").unwrap().0, 404);
    for name in ["cowork.db-wal", "cowork.db-shm"] {
        let f = Fixture::new();
        std::fs::rename(
            f.dir.path().join(name),
            f.dir.path().join("companion-original"),
        )
        .unwrap();
        symlink("companion-original", f.dir.path().join(name)).unwrap();
        assert!(f.service(None).is_err());
    }
    let f = Fixture::new();
    f.db.execute("INSERT INTO settings VALUES(NULL,'true')", [])
        .unwrap();
    assert_eq!(f.read(None, "u"), Err(Error::Unavailable));
}
proptest::proptest! { #![proptest_config(proptest::test_runner::Config::with_cases(24))] #[test] fn tenant_property(owner in "[a-z]{1,8}", other in "[a-z]{1,8}") {proptest::prop_assume!(owner != other);let f=Fixture::new();f.row(&owner,r#"{"plan":"tenant-only"}"#,"connected");let answer=f.read(Some("true"),&other).unwrap();proptest::prop_assert!(answer.1.contains("disconnected"));proptest::prop_assert!(!answer.1.contains("tenant-only"));} }
#[test]
fn busy_refuses_immediately_without_application_writes() {
    let f = Fixture::new();
    let s = f.service(Some("true")).unwrap();
    let before: Vec<(String, String)> =
        f.db.prepare("SELECT key,value FROM settings")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
    f.db.execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE; INSERT INTO settings VALUES('locked','false')").unwrap();
    let start = Instant::now();
    assert!(s.submit("u", Instant::now()).unwrap().finish().is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    f.db.execute_batch("ROLLBACK").unwrap();
    let after: Vec<(String, String)> =
        f.db.prepare("SELECT key,value FROM settings")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
    assert_eq!(before, after);
}
#[test]
fn independent_cancellation_and_late_signal_do_not_reuse_connections() {
    let f = Fixture::new();
    let s = f.service(Some("true")).unwrap();
    let j = s.submit("cancel-owner", Instant::now()).unwrap();
    let signal = j.cancellation();
    signal.cancel();
    let _ = j.finish();
    signal.cancel();
    let j = s.submit("cancel-owner", Instant::now()).unwrap();
    assert_eq!(j.finish().unwrap().0, 200);
    signal.cancel();
    assert_eq!(
        s.submit("cancel-owner", Instant::now())
            .unwrap()
            .finish()
            .unwrap()
            .0,
        200
    );
}
