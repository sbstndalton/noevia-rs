#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! Rebuilds an index Node wrote from the dump a recorder took (schema SQL in Node's order, rows in
//! id order, vectors in Node's vec0 scan order, hash maps).

use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;

fn sql_value(v: &Value) -> rusqlite::types::Value {
    match v {
        Value::Null => rusqlite::types::Value::Null,
        Value::Number(n) if n.is_i64() => rusqlite::types::Value::Integer(n.as_i64().unwrap()),
        Value::Number(n) => rusqlite::types::Value::Real(n.as_f64().unwrap()),
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        other => panic!("unexpected cell {other}"),
    }
}

fn blob(v: &Value) -> Vec<u8> {
    v.as_array()
        .unwrap()
        .iter()
        .flat_map(|x| (x.as_f64().unwrap() as f32).to_le_bytes())
        .collect()
}

/// Rebuilds a Node-written index from its dump.
pub fn rebuild(path: &Path, dump: &Value) {
    sqlite_vec_loader::register().unwrap();
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    for sql in dump["schema"].as_array().unwrap() {
        conn.execute_batch(sql.as_str().unwrap()).unwrap();
    }
    let cols = [
        "id",
        "file",
        "chunk_no",
        "body",
        "content_hash",
        "version",
        "direct",
        "kind",
        "meta",
    ];
    for row in dump["chunks"].as_array().unwrap() {
        let values: Vec<rusqlite::types::Value> =
            cols.iter().map(|c| sql_value(&row[*c])).collect();
        conn.execute(
            "INSERT INTO chunks (id, file, chunk_no, body, content_hash, version, direct, kind, meta) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params_from_iter(values),
        )
        .unwrap();
    }
    for row in dump["vec_meta"].as_array().unwrap() {
        conn.execute(
            "INSERT INTO vec_meta (key, value) VALUES (?, ?)",
            rusqlite::params![row["key"].as_str().unwrap(), row["value"].as_str()],
        )
        .unwrap();
    }
    for (table, rows) in dump["vectors"].as_object().unwrap() {
        for r in rows.as_array().unwrap() {
            let id = r["chunk_id"].as_i64().unwrap();
            conn.execute(
                &format!("INSERT INTO {table} (chunk_id, embedding) VALUES ({id}, ?)"),
                [blob(&r["embedding"])],
            )
            .unwrap();
        }
    }
    for (table, rows) in dump["maps"].as_object().unwrap() {
        for r in rows.as_array().unwrap() {
            conn.execute(
                &format!("INSERT INTO {table} (chunk_id, content_hash) VALUES (?, ?)"),
                rusqlite::params![
                    r["chunk_id"].as_i64().unwrap(),
                    r["content_hash"].as_str().unwrap()
                ],
            )
            .unwrap();
        }
    }
}
