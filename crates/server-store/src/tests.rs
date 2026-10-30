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
fn the_ownership_is_permanent() {
    assert_eq!(RustAuth::owned(), RustAuth::for_tests());
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
            assert_eq!(
                w.set_setting("previous_origins", r#"["https://old.test"]"#)?,
                1
            );
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
    assert_eq!(
        get("previous_origins").as_deref(),
        Some(r#"["https://old.test"]"#)
    );
    assert_eq!(
        writer
            .write(|w| w.delete_setting_if("previous_origins", "[]"))
            .unwrap(),
        0
    );
    assert_eq!(
        writer
            .write(|w| w.delete_setting_if("previous_origins", r#"["https://old.test"]"#))
            .unwrap(),
        1
    );
    assert_eq!(get("previous_origins"), None);
    for key in ["feature:x", "instance_id", "reasoning_effort_default", ""] {
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
    // noevia#1378: a cascade between two tables the writer owns is refused at open unless it is
    // one of Node's known foreign keys, so nothing can be deleted through it.
    static OWN_BOTH: &[(&str, Access)] = &[("parent", Access::Full), ("child", Access::Full)];
    assert!(matches!(
        Writer::open_owning(dir.path(), OWN_BOTH).err(),
        Some(StoreError::NotReady(m)) if m.contains("child.parent -> parent(id)")
    ));
    assert_eq!(count(&node, "SELECT count(*) FROM child"), 1);
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

// noevia#1378: a cascade or trigger inside a table Rust writes could reach another user's rows.
fn writer_refusal(dir: &std::path::Path) -> Option<String> {
    match Writer::open(dir, RustAuth::for_tests()) {
        Ok(_) => None,
        Err(StoreError::NotReady(m)) => Some(m),
        Err(e) => panic!("unexpected {e:?}"),
    }
}

#[test]
fn writer_opens_on_nodes_foreign_keys_including_the_tables_the_fixture_lacks() {
    let (dir, node) = node_db();
    node.execute_batch(
        "CREATE TABLE tool_policies(user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, tool TEXT NOT NULL, mode TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY(user_id, tool));
         CREATE TABLE dav_settings(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE, scope TEXT NOT NULL, acknowledged_at INTEGER, updated_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS diary_connectors(id TEXT PRIMARY KEY,user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,name TEXT NOT NULL,token_hash TEXT NOT NULL,created_at INTEGER NOT NULL);",
    )
    .unwrap();
    assert_eq!(writer_refusal(dir.path()), None);
}

#[test]
fn writer_refuses_a_self_referencing_session_cascade_and_never_deletes_across_users() {
    let (dir, node) = node_db();
    node.execute_batch(
        "ALTER TABLE sessions ADD COLUMN parent_session TEXT REFERENCES sessions(id_hash) ON DELETE CASCADE;",
    )
    .unwrap();
    let m = writer_refusal(dir.path()).expect("refused");
    assert!(
        m.contains("sessions.parent_session -> sessions(id_hash)"),
        "{m}"
    );
    // A read-only Store is unaffected: it cannot set anything off.
    assert!(Store::open(dir.path()).is_ok());
}

#[test]
fn writer_refuses_unknown_actions_into_tables_it_writes() {
    for ddl in [
        "ALTER TABLE passkeys ADD COLUMN sid TEXT REFERENCES sessions(id_hash) ON DELETE SET NULL",
        "ALTER TABLE challenges ADD COLUMN uid TEXT REFERENCES users(id) ON DELETE CASCADE",
        "ALTER TABLE device_tokens ADD COLUMN uid TEXT REFERENCES users(id) ON UPDATE CASCADE",
    ] {
        let (dir, node) = node_db();
        node.execute_batch(ddl).unwrap();
        assert!(writer_refusal(dir.path()).is_some(), "{ddl}");
    }
    // Into a table Rust never writes, the authorizer refuses the action itself; plain references
    // anywhere are fine.
    let (dir, node) = node_db();
    node.execute_batch(
        "CREATE TABLE a(id TEXT PRIMARY KEY); CREATE TABLE b(aid TEXT REFERENCES a(id));
         CREATE TABLE notes(sid TEXT REFERENCES sessions(id_hash) ON DELETE CASCADE);",
    )
    .unwrap();
    assert_eq!(writer_refusal(dir.path()), None);
}

#[test]
fn a_trigger_can_abort_a_writers_statement_but_never_write_through_it() {
    let (dir, node) = node_db();
    node.execute_batch(
        "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u2','B','b','B','member','x','w2',1,1);
         INSERT INTO sessions VALUES('mine','u1','c',1,1,9,'ua','ip'),('theirs','u2','c',1,1,9,'ua','ip');
         CREATE TRIGGER wipe AFTER DELETE ON sessions BEGIN DELETE FROM sessions; END;",
    )
    .unwrap();
    let writer = Writer::open(dir.path(), RustAuth::for_tests()).unwrap();
    // Signing out one session would delete every user's sessions through the trigger: refused whole.
    let res = writer.write(|w| w.execute("DELETE FROM sessions WHERE id_hash='mine'", []));
    assert!(matches!(res, Err(StoreError::NotOwned(_))), "{res:?}");
    assert_eq!(count(&node, "SELECT count(*) FROM sessions"), 2);
    // Added while the writer is open, on another owned table: refused the same way.
    node.execute_batch(
        "DROP TRIGGER wipe; CREATE TRIGGER grant AFTER INSERT ON challenges BEGIN UPDATE users SET role='admin'; END;",
    )
    .unwrap();
    let res = writer.write(|w| w.execute("INSERT INTO challenges VALUES('h','u1','k','c',9)", []));
    assert!(matches!(res, Err(StoreError::NotOwned(_))), "{res:?}");
    assert_eq!(
        count(&node, "SELECT count(*) FROM users WHERE role='admin'"),
        0
    );
    // A trigger that only raises still aborts, as the audit-failure tests rely on.
    node.execute_batch(
        "DROP TRIGGER grant; CREATE TRIGGER abort_audit BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic'); END;",
    )
    .unwrap();
    let res = writer.write(|w| {
        w.execute(
            "INSERT INTO audit_events(action, created_at) VALUES('x', 1)",
            [],
        )
    });
    assert!(res.is_err());
    assert!(!matches!(res, Err(StoreError::NotOwned(_))), "{res:?}");
    // The plain delete works once the writing trigger is gone.
    writer
        .write(|w| w.execute("DELETE FROM sessions WHERE id_hash='mine'", []))
        .unwrap();
    assert_eq!(count(&node, "SELECT count(*) FROM sessions"), 1);
}

#[test]
fn feature_capability_grants_only_canonical_catalog_keys_and_exact_epoch() {
    let (dir, node) = node_db();
    let writer = Writer::open(dir.path(), RustAuth::owned()).unwrap();
    writer
        .write(|tx| {
            for feature in server_features::Feature::ALL {
                assert_eq!(
                    tx.set_feature_enabled(RustFeatureSettings::owned(), feature, true)?,
                    1
                );
            }
            tx.set_feature_runtime_incarnation(
                RustFeatureSettings::owned(),
                "1234567890abcdef1234567890abcdef",
            )?;
            Ok(())
        })
        .unwrap();
    for feature in server_features::Feature::ALL {
        let value: String = node
            .query_row(
                "SELECT value FROM settings WHERE key=?1",
                [server_features::spec(feature).setting],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(value, "true");
        assert!(writer
            .write(|tx| tx.set_setting(server_features::spec(feature).setting, "false"))
            .is_err());
    }
    assert!(writer
        .write(|tx| tx.set_setting(FEATURE_RUNTIME_INCARNATION_KEY, "forged"))
        .is_err());
    assert!(writer
        .write(|tx| tx.execute(
            "UPDATE settings SET value='false' WHERE key='feature:plannerReview'",
            []
        ))
        .is_err());
    for invalid in [
        "",
        "00000000000000000000000000000000",
        "1234567890ABCDEF1234567890ABCDEF",
        "1234567890abcdef1234567890abcde/",
    ] {
        assert!(writer
            .write(|tx| tx.set_feature_runtime_incarnation(RustFeatureSettings::owned(), invalid))
            .is_err());
    }
    let epoch: String = node
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            [FEATURE_RUNTIME_INCARNATION_KEY],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(epoch, "1234567890abcdef1234567890abcdef");
    let count: i64 = node
        .query_row("SELECT count(*) FROM settings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 24);
}

#[test]
fn storage_connections_take_only_the_save_upsert() {
    // sbstndalton/noevia#2190: a completed Nextcloud sign-in saves the row through Node's exact
    // upsert; no other statement (a raw insert, an update, a delete) reaches the table.
    let (dir, node) = node_db();
    let w = Writer::open(dir.path(), RustAuth::owned()).unwrap();
    let raw = w.write(|tx| {
        tx.execute(
            "INSERT INTO storage_connections(user_id,kind) VALUES('u1','webdav')",
            [],
        )
    });
    assert!(matches!(raw, Err(StoreError::NotOwned(_))), "{raw:?}");
    let save = |kind: &str, at: i64| {
        w.write(|tx| {
            tx.save_storage_connection(
                RustStorageIntegrations::owned(),
                "u1",
                kind,
                "https://cloud.invalid/remote.php/dav/files/ada",
                "",
                "us-east-1",
                "ada",
                "enc:v2:synthetic",
                "Cowork/Diary",
                at,
            )
        })
    };
    assert_eq!(save("nextcloud", 5).unwrap(), 1);
    assert_eq!(
        save("nextcloud", 6).unwrap(),
        1,
        "the conflict path updates the row"
    );
    let got: (String, String, i64) = node
        .query_row(
            "SELECT kind,secret,updated_at FROM storage_connections WHERE user_id='u1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(got, ("nextcloud".into(), "enc:v2:synthetic".into(), 6));
    // The window closes after the statement: no update or delete afterwards.
    for sql in [
        "UPDATE storage_connections SET kind='local'",
        "DELETE FROM storage_connections",
    ] {
        let r = w.write(|tx| tx.execute(sql, []));
        assert!(matches!(r, Err(StoreError::NotOwned(_))), "{sql}: {r:?}");
    }
    assert_eq!(count(&node, "SELECT count(*) FROM storage_connections"), 1);
}

#[test]
fn chatgpt_tokens_take_only_the_broker_statements() {
    // sbstndalton/noevia#2190 round 5: the token broker's four statements, nothing else.
    let (dir, node) = node_db();
    node.execute_batch(
        "CREATE TABLE IF NOT EXISTS chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY, data_enc TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'connected', updated_at INTEGER NOT NULL)",
    )
    .unwrap();
    let w = Writer::open(dir.path(), RustAuth::owned()).unwrap();
    for sql in [
        "INSERT INTO chatgpt_oauth_tokens(user_id,data_enc,state,updated_at) VALUES('u1','x','connected',1)",
        "UPDATE chatgpt_oauth_tokens SET state='connected'",
        "DELETE FROM chatgpt_oauth_tokens",
    ] {
        let raw = w.write(|tx| tx.execute(sql, []));
        assert!(matches!(raw, Err(StoreError::NotOwned(_))), "{sql}: {raw:?}");
    }
    let cap = RustChatgptOAuth::owned();
    let read = || -> Option<(String, String, i64)> {
        node.query_row(
            "SELECT data_enc,state,updated_at FROM chatgpt_oauth_tokens WHERE user_id='u1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()
    };
    assert_eq!(
        w.write(|tx| tx.save_chatgpt_tokens(cap, "u1", "a", "connected", 5))
            .unwrap(),
        1
    );
    assert_eq!(
        w.write(|tx| tx.save_chatgpt_tokens(cap, "u1", "b", "connected", 6))
            .unwrap(),
        1
    );
    assert_eq!(read(), Some(("b".into(), "connected".into(), 6)));
    // The refresh's compare-and-set: a stale start value changes nothing.
    assert_eq!(
        w.write(|tx| tx.swap_chatgpt_tokens(cap, "u1", "c", 7, "a"))
            .unwrap(),
        0
    );
    assert_eq!(
        w.write(|tx| tx.swap_chatgpt_tokens(cap, "u1", "c", 7, "b"))
            .unwrap(),
        1
    );
    // Reconnect only for the sign-in that failed.
    assert_eq!(
        w.write(|tx| tx.mark_chatgpt_reconnect(cap, "u1", Some("b"), 8))
            .unwrap(),
        0
    );
    assert_eq!(
        w.write(|tx| tx.mark_chatgpt_reconnect(cap, "u1", Some("c"), 8))
            .unwrap(),
        1
    );
    assert_eq!(read(), Some(("c".into(), "reconnect".into(), 8)));
    // A reconnect-needed row is not refreshed into connected.
    assert_eq!(
        w.write(|tx| tx.swap_chatgpt_tokens(cap, "u1", "d", 9, "c"))
            .unwrap(),
        0
    );
    assert_eq!(
        w.write(|tx| tx.mark_chatgpt_reconnect(cap, "u1", None, 9))
            .unwrap(),
        1
    );
    assert_eq!(
        w.write(|tx| tx.delete_chatgpt_tokens(cap, "u1")).unwrap(),
        1
    );
    assert_eq!(read(), None);
    // The start sweep removes the rows of accounts that no longer exist, and only those.
    node.execute_batch(
        "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('kept','kept','kept','K','member','x','kept',1,1)",
    )
    .unwrap();
    for u in ["kept", "gone-1", "gone-2"] {
        w.write(|tx| tx.save_chatgpt_tokens(cap, u, "x", "connected", 1))
            .unwrap();
    }
    assert_eq!(
        w.write(|tx| tx.delete_orphan_chatgpt_tokens(cap)).unwrap(),
        2
    );
    let left: Vec<String> = node
        .prepare("SELECT user_id FROM chatgpt_oauth_tokens")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(left, vec!["kept".to_string()]);
    assert_eq!(
        w.write(|tx| tx.delete_orphan_chatgpt_tokens(cap)).unwrap(),
        0
    );
    // The window closed again: a raw statement after the methods is still refused.
    let raw = w.write(|tx| tx.execute("DELETE FROM chatgpt_oauth_tokens", []));
    assert!(matches!(raw, Err(StoreError::NotOwned(_))), "{raw:?}");
}
