//! Read-only health detail never creates a database or modifies an existing one.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use noevia_diary::health::tenant_detail;
use sha2::{Digest, Sha256};

#[test]
fn absent_index_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.db");
    assert!(tenant_detail(
        &path,
        r#"{"model":"synthetic","profile":"plain","vec_available":false}"#
    )
    .is_none());
    assert!(!path.exists());
}

#[test]
fn health_counts_slots_and_extension_absence_are_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE journal(applied INTEGER, last_error TEXT);
        INSERT INTO journal VALUES (0,NULL),(0,'retry'),(1,'quarantined:bad'),(1,NULL);
        CREATE TABLE chunks(id INTEGER); INSERT INTO chunks VALUES(1),(2),(3);
        CREATE TABLE vec_meta(key TEXT, value TEXT);",
    )
    .unwrap();
    let id = format!("{:x}", Sha256::digest(b"plain\0synthetic"));
    conn.execute(
        "INSERT INTO vec_meta VALUES (?,?)",
        [
            format!("slot:{}", &id[..16]),
            r#"{"table":"vec_items","dim":7,"map":null}"#.to_string(),
        ],
    )
    .unwrap();
    drop(conn);
    let before = std::fs::read(&path).unwrap();
    for available in [true, false] {
        let facts =
            serde_json::json!({"model":"synthetic","profile":"plain","vec_available":available});
        let stats =
            serde_json::to_value(tenant_detail(&path, &facts.to_string()).unwrap()).unwrap();
        assert_eq!(
            stats,
            serde_json::json!({"journal_pending":2,"journal_quarantined":1,
            "retrieval":{"chunks":3,"vec_available":available,"dim":if available {Some(7)} else {None},
                "model":"synthetic","profile":"plain"}})
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
