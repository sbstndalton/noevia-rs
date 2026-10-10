use super::*;
use rusqlite::Connection;

const SCHEMA: &str = include_str!("../tests/fixtures/node-schema.sql");

/// A data dir holding a Node-shaped cowork.db, plus a read-write connection standing in for Node.
fn node_db() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let node = Connection::open(db_path(dir.path())).unwrap();
    node.execute_batch(SCHEMA).unwrap();
    node.execute(
        "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u1','A','a','A','member','x','w',1,1)",
        [],
    )
    .unwrap();
    (dir, node)
}

fn count_users(store: &Store) -> i64 {
    store
        .read(|r| r.row("SELECT count(*) FROM users", [], |row| row.get(0)))
        .unwrap()
        .unwrap()
}

#[test]
fn reads_a_node_database() {
    let (dir, node) = node_db();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(count_users(&store), 1);
    // Node keeps writing (WAL): the next read sees it.
    node.execute(
        "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u2','B','b','B','admin','x','w2',1,1)",
        [],
    )
    .unwrap();
    assert_eq!(count_users(&store), 2);
}

#[test]
fn never_creates_a_database() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(Store::open(dir.path()).err(), Some(StoreError::Missing));
    assert!(!db_path(dir.path()).exists());
    assert_eq!(
        Store::open(&dir.path().join("nope")).err(),
        Some(StoreError::Missing)
    );
    assert!(!dir.path().join("nope").exists());
}

#[test]
fn refuses_a_newer_schema_at_open_and_later() {
    let (dir, node) = node_db();
    node.execute("INSERT INTO schema_migrations VALUES(6,0)", [])
        .unwrap();
    assert_eq!(
        Store::open(dir.path()).err(),
        Some(StoreError::SchemaTooNew { found: 6, known: 5 })
    );
    node.execute("DELETE FROM schema_migrations WHERE version=6", [])
        .unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(count_users(&store), 1);
    // Node upgrades under a running front: a data-only migration (no schema cookie change) and a
    // column migration are both caught before the next read.
    node.execute("INSERT INTO schema_migrations VALUES(6,0)", [])
        .unwrap();
    assert_eq!(
        store.read(|_| Ok(())).err(),
        Some(StoreError::SchemaTooNew { found: 6, known: 5 })
    );
    node.execute("DELETE FROM schema_migrations WHERE version=6", [])
        .unwrap();
    assert_eq!(count_users(&store), 1);
}

#[test]
fn refuses_an_unmigrated_or_incomplete_schema() {
    let (dir, node) = node_db();
    node.execute("DELETE FROM schema_migrations WHERE version=5", [])
        .unwrap();
    assert!(matches!(
        Store::open(dir.path()),
        Err(StoreError::NotReady(_))
    ));
    node.execute("INSERT INTO schema_migrations VALUES(5,0)", [])
        .unwrap();
    let store = Store::open(dir.path()).unwrap();
    // A column Rust reads disappears (a rebuilt table): caught by the schema-cookie recheck.
    node.execute_batch("ALTER TABLE users DROP COLUMN credential_epoch")
        .unwrap();
    match store.read(|_| Ok(())) {
        Err(StoreError::NotReady(why)) => assert!(why.contains("credential_epoch"), "{why}"),
        other => panic!("{other:?}"),
    }
    drop(store);
    assert!(matches!(
        Store::open(dir.path()),
        Err(StoreError::NotReady(_))
    ));
}

#[test]
fn refuses_a_database_node_never_put_in_wal() {
    let dir = tempfile::tempdir().unwrap();
    let node = Connection::open(db_path(dir.path())).unwrap();
    node.execute_batch(&SCHEMA.replace("PRAGMA journal_mode = WAL;", ""))
        .unwrap();
    match Store::open(dir.path()) {
        Err(StoreError::NotReady(why)) => assert!(why.contains("WAL"), "{why}"),
        other => panic!("{:?}", other.err()),
    }
}

#[test]
fn the_reader_refuses_every_write() {
    let (dir, _node) = node_db();
    let store = Store::open(dir.path()).unwrap();
    for sql in [
        "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('x','x','x','x','member','x','x',1,1) RETURNING id",
        "UPDATE users SET role='admin' RETURNING id",
        "DELETE FROM sessions RETURNING id_hash",
        "INSERT INTO audit_events(action,created_at) VALUES('x',1) RETURNING id",
        "CREATE TABLE evil(x)",
        "DROP TABLE users",
        "ALTER TABLE users ADD COLUMN evil TEXT",
        "CREATE INDEX evil ON users(role)",
        "CREATE TEMP TABLE evil(x)",
        "ATTACH DATABASE ':memory:' AS evil",
        "PRAGMA journal_mode = DELETE",
        "PRAGMA query_only = 0",
        "PRAGMA writable_schema = 1",
        "PRAGMA user_version = 9",
        "VACUUM",
        "SAVEPOINT s",
    ] {
        let res = store.read(|r| r.row(sql, [], |row| row.get::<_, rusqlite::types::Value>(0)));
        assert!(res.is_err(), "allowed: {sql}");
    }
    assert_eq!(count_users(&store), 1);
    // Reads, CTEs and functions still work.
    let n: i64 = store
        .read(|r| r.row("WITH t(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM t WHERE x<3) SELECT sum(x) + length(hex(randomblob(1))) FROM t", [], |row| row.get(0)))
        .unwrap()
        .unwrap();
    assert_eq!(n, 8);
}

#[test]
fn the_switch_is_exactly_one() {
    assert!(RustAuth::from_env_value(Some("1")).is_some());
    for off in [
        None,
        Some(""),
        Some("0"),
        Some("true"),
        Some(" 1"),
        Some("1 "),
        Some("yes"),
    ] {
        assert!(RustAuth::from_env_value(off).is_none(), "{off:?}");
    }
    assert!(OWNED_TABLES.iter().any(|(t, _)| *t == "sessions"));
    assert!(!OWNED_TABLES.iter().any(|(t, _)| *t == "settings"));
    assert!(!OWNED_TABLES
        .iter()
        .any(|(t, _)| *t == "storage_connections"));
}

fn count(node: &Connection, sql: &str) -> i64 {
    node.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn a_writer_writes_only_what_it_owns() {
    let (dir, node) = node_db();
    let writer = Writer::open(dir.path(), RustAuth::for_tests()).unwrap();
    writer
        .write(|w| {
            w.execute(
                "INSERT INTO audit_events(action,created_at) VALUES('rust.test',1)",
                [],
            )?;
            w.execute(
                "INSERT INTO sessions VALUES('h','u1','c',1,1,2,'ua','127.0.0.1')",
                [],
            )?;
            w.execute("UPDATE sessions SET last_seen_at=5 WHERE id_hash='h'", [])?;
            w.execute("UPDATE users SET display_name='B' WHERE id='u1'", [])?;
            w.execute(
                "INSERT INTO user_features(user_id,diary_enabled,updated_at) VALUES('u1',1,1) ON CONFLICT(user_id) DO UPDATE SET diary_enabled=excluded.diary_enabled",
                [],
            )?;
            w.execute("DELETE FROM sessions WHERE id_hash='h'", [])
        })
        .unwrap();
    assert_eq!(
        count(
            &node,
            "SELECT count(*) FROM audit_events WHERE action='rust.test'"
        ),
        1
    );
    assert_eq!(count(&node, "SELECT count(*) FROM sessions"), 0);
    for sql in [
        // Not granted by the table's access.
        "DELETE FROM users",
        "UPDATE audit_events SET action='x'",
        "DELETE FROM audit_events",
        "DELETE FROM user_features",
        // Not owned at all.
        "INSERT INTO storage_connections(user_id,kind,updated_at) VALUES('u1','local',1)",
        "UPDATE schema_migrations SET applied_at=1",
        // settings only through the key-checked helpers.
        "INSERT INTO settings(key,value) VALUES('x','y')",
        "INSERT OR REPLACE INTO settings(key,value) VALUES('setup_code_hash','y')",
        "DELETE FROM settings WHERE key='setup_code_hash'",
        "UPDATE settings SET value='y' WHERE key='public_origin'",
        // Never DDL, ATTACH or pragmas with values.
        "CREATE TABLE evil(x)",
        "ALTER TABLE audit_events ADD COLUMN evil TEXT",
        "DROP TABLE audit_events",
        "CREATE TRIGGER evil AFTER INSERT ON audit_events BEGIN DELETE FROM users; END",
        "CREATE TEMP TABLE evil(x)",
        "ATTACH DATABASE ':memory:' AS evil",
        "PRAGMA journal_mode = DELETE",
        "PRAGMA foreign_keys = OFF",
    ] {
        let res = writer.write(|w| w.execute(sql, []));
        assert!(res.is_err(), "allowed: {sql}");
    }
    assert!(matches!(
        writer.write(|w| w.execute("DELETE FROM users", [])),
        Err(StoreError::NotOwned(_))
    ));
    assert_eq!(count(&node, "SELECT count(*) FROM users"), 1);
    // A failed statement rolls the whole transaction back.
    let res = writer.write(|w| {
        w.execute(
            "INSERT INTO audit_events(action,created_at) VALUES('rolled.back',1)",
            [],
        )?;
        w.execute("DELETE FROM users", [])
    });
    assert!(res.is_err());
    assert_eq!(
        count(
            &node,
            "SELECT count(*) FROM audit_events WHERE action='rolled.back'"
        ),
        0
    );
}

#[test]
fn settings_are_written_per_owned_key_only() {
    let (dir, node) = node_db();
    node.execute("INSERT INTO settings VALUES('feature:x','true')", [])
        .unwrap();
    let writer = Writer::open(dir.path(), RustAuth::for_tests()).unwrap();
    writer
        .write(|w| {
            assert_eq!(w.set_setting("public_origin", "https://a.test")?, 1);
            assert_eq!(w.insert_setting_if_absent("passkey_decoy_key", "k1")?, 1);
            assert_eq!(w.insert_setting_if_absent("passkey_decoy_key", "k2")?, 0);
            w.set_setting("setup_code_hash", "h")?;
            assert_eq!(w.delete_setting_if("setup_code_hash", "other")?, 0);
            assert_eq!(w.delete_setting_if("setup_code_hash", "h")?, 1);
            Ok(())
        })
        .unwrap();
    let get = |k: &str| -> Option<String> {
        node.query_row("SELECT value FROM settings WHERE key=?1", [k], |r| r.get(0))
            .ok()
    };
    assert_eq!(get("public_origin").as_deref(), Some("https://a.test"));
    assert_eq!(get("passkey_decoy_key").as_deref(), Some("k1"));
    assert_eq!(get("setup_code_hash"), None);
    for key in [
        "feature:x",
        "instance_id",
        "previous_origins",
        "reasoning_effort_default",
        "",
    ] {
        let res = writer.write(|w| w.set_setting(key, "evil"));
        assert!(matches!(res, Err(StoreError::NotOwned(_))), "{key}");
        let res = writer.write(|w| w.delete_setting_if(key, "true"));
        assert!(matches!(res, Err(StoreError::NotOwned(_))), "{key}");
    }
    assert_eq!(get("feature:x").as_deref(), Some("true"));
    // The window closes again: a raw statement right after a helper is refused.
    let res = writer.write(|w| {
        w.set_setting("public_origin", "https://b.test")?;
        w.execute(
            "INSERT OR REPLACE INTO settings(key,value) VALUES('feature:x','false')",
            [],
        )
    });
    assert!(res.is_err());
    assert_eq!(get("feature:x").as_deref(), Some("true"));
    assert_eq!(get("public_origin").as_deref(), Some("https://a.test"));
}

#[test]
fn foreign_keys_hold_for_rust_rows() {
    let (dir, node) = node_db();
    node.execute_batch(
        "INSERT INTO device_grants VALUES('g1','u1','Mac',1,1,9,'ip','ua');
         INSERT INTO device_tokens VALUES('t1','g1','access',1,9,NULL,NULL);
         INSERT INTO device_tokens VALUES('t2','g1','refresh',1,9,NULL,NULL);",
    )
    .unwrap();
    let writer = Writer::open(dir.path(), RustAuth::for_tests()).unwrap();
    // A grant's tokens go with it (ON DELETE CASCADE needs foreign_keys = ON, as Node sets).
    writer
        .write(|w| w.execute("DELETE FROM device_grants WHERE id='g1'", []))
        .unwrap();
    assert_eq!(count(&node, "SELECT count(*) FROM device_tokens"), 0);
    // A session for an account that does not exist is refused, as in Node.
    let res = writer.write(|w| {
        w.execute(
            "INSERT INTO sessions VALUES('h','nobody','c',1,1,2,'ua','ip')",
            [],
        )
    });
    assert!(res.is_err());
}

#[test]
fn a_cascade_reaches_only_tables_the_writer_owns() {
    // SQLite runs the authorizer on the programs it compiles for foreign-key actions, so an owned
    // parent whose rows cascade into a table Rust does not own cannot be deleted from.
    let (dir, node) = node_db();
    node.execute_batch(
        "CREATE TABLE parent(id TEXT PRIMARY KEY);
         CREATE TABLE child(id TEXT, parent TEXT REFERENCES parent(id) ON DELETE CASCADE);
         INSERT INTO parent VALUES('p'); INSERT INTO child VALUES('c','p');",
    )
    .unwrap();
    static OWN_PARENT: &[(&str, Access)] = &[("parent", Access::Full)];
    let writer = Writer::open_owning(dir.path(), OWN_PARENT).unwrap();
    let res = writer.write(|w| w.execute("DELETE FROM parent", []));
    assert!(
        matches!(res, Err(StoreError::NotOwned(_))),
        "the cascade into an unowned table ran: {res:?}"
    );
    assert_eq!(count(&node, "SELECT count(*) FROM child"), 1);
    static OWN_BOTH: &[(&str, Access)] = &[("parent", Access::Full), ("child", Access::Full)];
    let writer = Writer::open_owning(dir.path(), OWN_BOTH).unwrap();
    writer
        .write(|w| w.execute("DELETE FROM parent", []))
        .unwrap();
    assert_eq!(count(&node, "SELECT count(*) FROM child"), 0);
}

#[test]
fn the_writer_reads_its_own_commits() {
    let (dir, _node) = node_db();
    let writer = Writer::open(dir.path(), RustAuth::for_tests()).unwrap();
    writer
        .write(|w| {
            w.execute(
                "INSERT INTO sessions VALUES('h','u1','c',1,1,2,'ua','ip')",
                [],
            )
        })
        .unwrap();
    let n: Option<i64> = writer
        .read(|r| r.row("SELECT count(*) FROM sessions", [], |row| row.get(0)))
        .unwrap();
    assert_eq!(n, Some(1));
}

#[test]
fn a_writer_never_creates_the_file() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        Writer::open(dir.path(), RustAuth::for_tests()).err(),
        Some(StoreError::Missing)
    );
    assert!(!db_path(dir.path()).exists());
}
