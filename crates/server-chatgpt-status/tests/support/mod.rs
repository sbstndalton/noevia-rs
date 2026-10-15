//! Synthetic cowork.db and key files for the status tests: core's auth schema
//! (server-store tests/fixtures/node-schema.sql), the chatgpt_oauth_tokens DDL Node itself ran
//! (recorded by tests/oracle.cjs), and fixed synthetic keys. Nothing here is a real credential.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use server_auth::secrets::SecretKeys;
use server_store::rusqlite::Connection;
use std::path::Path;
use std::sync::Arc;

pub const FIXTURE: &str = include_str!("../fixtures/node-chatgpt-status.v1.json");

pub fn fixture() -> serde_json::Value {
    // CI passes the freshly generated oracle output; locally the committed copy.
    let text = std::env::var("NOEVIA_CHATGPT_STATUS_FIXTURE")
        .map(|p| std::fs::read_to_string(p).unwrap())
        .unwrap_or_else(|_| FIXTURE.to_string());
    serde_json::from_str(&text).unwrap()
}

pub fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// A data dir with cowork.db: Node's auth schema plus `ddl` (None: no ChatGPT table).
pub fn data_dir(ddl: Option<&str>) -> (tempfile::TempDir, Connection) {
    let d = tempfile::tempdir().unwrap();
    let db = Connection::open(d.path().join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    if let Some(ddl) = ddl {
        db.execute_batch(ddl).unwrap();
    }
    (d, db)
}

pub fn node_ddl() -> String {
    fixture()["chatgptDdl"].as_str().unwrap().to_string()
}

pub fn write_key(dir: &Path, name: &str, key: &[u8]) {
    std::fs::write(dir.join(name), key).unwrap();
}

pub fn keys(dir: &Path, previous_env: Option<&str>) -> Arc<SecretKeys> {
    Arc::new(server_auth::secrets::load_existing(dir, previous_env).unwrap())
}

pub fn seal(key: &[u8], plain: &[u8], aad: Option<&str>) -> String {
    secret_envelope::seal(key, &[5u8; 12], plain, aad.map(str::as_bytes)).unwrap()
}

pub fn set_flag(db: &Connection, value: &str) {
    db.execute(
        "INSERT OR REPLACE INTO settings(key,value) VALUES('feature:chatgptOAuth',?1)",
        [value],
    )
    .unwrap();
}

pub fn put_row(
    db: &Connection,
    user: &str,
    state: &str,
    data_enc: &dyn server_store::rusqlite::ToSql,
) {
    db.execute(
        "INSERT INTO chatgpt_oauth_tokens(user_id,data_enc,state,updated_at) VALUES(?1,?2,?3,1)",
        server_store::rusqlite::params![user, data_enc, state],
    )
    .unwrap();
}

/// Every row of both tables the route could read, to show it wrote nothing.
pub fn dump(db: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    for sql in [
        "SELECT user_id || '|' || quote(data_enc) || '|' || quote(state) || '|' || updated_at FROM chatgpt_oauth_tokens ORDER BY user_id",
        "SELECT key || '|' || quote(value) FROM settings ORDER BY key",
    ] {
        if let Ok(mut stmt) = db.prepare(sql) {
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            out.extend(rows.map(Result::unwrap));
        }
    }
    out
}
