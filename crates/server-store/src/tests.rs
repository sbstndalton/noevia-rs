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
fn m2_owns_no_tables() {
    let (dir, _node) = node_db();
    assert!(OWNED_TABLES.is_empty());
    assert!(OWNED_JSON_FILES.is_empty());
    assert!(matches!(
        Writer::open(dir.path()),
        Err(StoreError::NotOwned(_))
    ));
}

#[test]
fn a_writer_writes_only_its_owned_tables() {
    let (dir, node) = node_db();
    let writer = Writer::open_owning(dir.path(), &["audit_events"]).unwrap();
    writer
        .write(|w| {
            w.execute(
                "INSERT INTO audit_events(action,created_at) VALUES('rust.test',1)",
                [],
            )
        })
        .unwrap();
    let n: i64 = node
        .query_row(
            "SELECT count(*) FROM audit_events WHERE action='rust.test'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    for sql in [
        "UPDATE users SET role='admin'",
        "DELETE FROM users",
        "INSERT INTO settings(key,value) VALUES('x','y')",
        "CREATE TABLE evil(x)",
        "ALTER TABLE audit_events ADD COLUMN evil TEXT",
        "DROP TABLE audit_events",
        "CREATE TRIGGER evil AFTER INSERT ON audit_events BEGIN DELETE FROM users; END",
        "ATTACH DATABASE ':memory:' AS evil",
        "PRAGMA journal_mode = DELETE",
    ] {
        let res = writer.write(|w| w.execute(sql, []));
        assert!(res.is_err(), "allowed: {sql}");
    }
    assert!(matches!(
        writer.write(|w| w.execute("UPDATE users SET role='admin'", [])),
        Err(StoreError::NotOwned(_))
    ));
    let role: String = node
        .query_row("SELECT role FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(role, "member");
    // A failed statement rolls the whole transaction back.
    let res = writer.write(|w| {
        w.execute(
            "INSERT INTO audit_events(action,created_at) VALUES('rolled.back',1)",
            [],
        )?;
        w.execute("DELETE FROM users", [])
    });
    assert!(res.is_err());
    let n: i64 = node
        .query_row(
            "SELECT count(*) FROM audit_events WHERE action='rolled.back'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn a_writer_never_creates_the_file() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        Writer::open_owning(dir.path(), &["audit_events"]).err(),
        Some(StoreError::Missing)
    );
    assert!(!db_path(dir.path()).exists());
}
