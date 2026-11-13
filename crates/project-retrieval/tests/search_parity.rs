#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! F2-F6: searches recorded from noevia-core `server/rag.cjs` over indexes Node wrote with
//! better-sqlite3 + sqlite-vec 0.1.9 (`generate_search_fixtures.cjs`). Each index is rebuilt here
//! from its dump (Node's own schema SQL, rows in id order, vectors in Node's scan order) and every
//! recorded search must give Node's hits, in Node's order, with Node's scores.

use project_retrieval::search::{open_index, search, Target, Versions, TOP_K};
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;

fn fixture() -> Value {
    let path = format!(
        "{}/tests/fixtures/F2-F6-search.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

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
fn rebuild(path: &Path, dump: &Value) {
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

fn versions(v: &Value) -> Versions {
    v.as_object()
        .unwrap()
        .iter()
        .map(|(file, want)| (file.clone(), want.as_str().map(str::to_owned)))
        .collect()
}

/// The main file and the committed WAL frames (a missing `-wal` is an empty one). The `-shm`
/// index a reader may create is not content.
fn digest(path: &Path) -> (Vec<u8>, Vec<u8>) {
    let wal = path.with_file_name(format!(
        "{}-wal",
        path.file_name().unwrap().to_str().unwrap()
    ));
    (
        std::fs::read(path).unwrap(),
        std::fs::read(wal).unwrap_or_default(),
    )
}

#[test]
fn every_recorded_search_matches_node() {
    let f = fixture();
    assert_eq!(f["constants"]["TOP_K"], TOP_K);
    let mut checked = 0;
    for s in f["scenarios"].as_array().unwrap() {
        let name = s["name"].as_str().unwrap();
        for q in s["searches"].as_array().unwrap() {
            let note = q["note"].as_str().unwrap();
            let want: Vec<(String, String, f64)> = q["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| {
                    (
                        h["file"].as_str().unwrap().to_owned(),
                        h["body"].as_str().unwrap().to_owned(),
                        h["score"].as_f64().unwrap(),
                    )
                })
                .collect();
            let dir = tempfile::tempdir().unwrap();
            let file = dir
                .path()
                .join(format!("{}.db", q["projectId"].as_str().unwrap()));
            if let Some(key) = q["index"].as_str() {
                rebuild(&file, &f["snapshots"][key]);
            }
            let Some(conn) = open_index(&file).unwrap() else {
                assert!(want.is_empty(), "{name}/{note}: no index but Node had hits");
                assert_eq!(q["created"], Value::Bool(false), "{name}/{note}");
                assert!(!file.exists(), "{name}/{note}: a read created the index");
                checked += 1;
                continue;
            };
            let target = Target {
                profile: q["target"]["profile"].as_str().unwrap(),
                model: q["target"]["model"].as_str().unwrap(),
            };
            let query: Vec<f32> = q["query_vector"]
                .as_array()
                .map(|a| a.iter().map(|x| x.as_f64().unwrap() as f32).collect())
                .unwrap_or_default();
            let before = digest(&file);
            let got = search(&conn, target, &query, TOP_K, &versions(&q["versions"])).unwrap();
            drop(conn);
            assert_eq!(
                digest(&file),
                before,
                "{name}/{note}: the search wrote to the index"
            );
            let got: Vec<(String, String, f64)> =
                got.into_iter().map(|h| (h.file, h.body, h.score)).collect();
            if note.starts_with("difference: tie-order") {
                // Named difference: Node returns exact ties in the vec0 scan order, the native search
                // in chunk id order. Exact ties here are the twin files' identical chunks (same text,
                // same vector), adjacent in Node's list: re-ordered by chunk id they are ours.
                let snapshot = &f["snapshots"][q["index"].as_str().unwrap()];
                let id = |file: &str, body: &str| {
                    snapshot["chunks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|c| c["file"] == file && c["body"] == body)
                        .and_then(|c| c["id"].as_i64())
                        .unwrap()
                };
                let mut native = want.clone();
                for run in native.chunk_by_mut(|a, b| a.1 == b.1 && a.2 == b.2) {
                    run.sort_by_key(|h| id(&h.0, &h.1));
                }
                assert_ne!(
                    native, want,
                    "{name}/{note}: the case no longer shows the difference"
                );
                assert_eq!(got, native, "{name}/{note}");
            } else if note.starts_with("difference: unclaimed-legacy") {
                // Named difference: Node claims the pre-#1636 table on open and searches it; a native
                // read claims nothing, so it has no slot yet.
                assert!(
                    !want.is_empty(),
                    "{name}/{note}: the case no longer shows the difference"
                );
                assert!(got.is_empty(), "{name}/{note}");
            } else {
                assert_eq!(got, want, "{name}/{note}");
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 21);
}

fn recorded(f: &Value, scenario: &str, note: &str) -> Value {
    f["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == scenario)
        .unwrap()["searches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["note"] == note)
        .unwrap()
        .clone()
}

fn query_of(q: &Value) -> Vec<f32> {
    q["query_vector"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap() as f32)
        .collect()
}

fn triples(hits: Vec<project_retrieval::hits::Hit>) -> Vec<(String, String, f64)> {
    hits.into_iter()
        .map(|h| (h.file, h.body, h.score))
        .collect()
}

fn want_of(q: &Value) -> Vec<(String, String, f64)> {
    q["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["file"].as_str().unwrap().to_owned(),
                h["body"].as_str().unwrap().to_owned(),
                h["score"].as_f64().unwrap(),
            )
        })
        .collect()
}

fn target_of(q: &Value) -> Target<'_> {
    Target {
        profile: q["target"]["profile"].as_str().unwrap(),
        model: q["target"]["model"].as_str().unwrap(),
    }
}

/// An index from before #122/#1636/#1726 has no `version`, `direct`, `kind` or `meta` column.
/// Node adds them (NULL) on open and then searches; read-only, a missing column reads as NULL:
/// the recorded S4 search over the same rows gives the same hits, and a wanted version equal to
/// the chunks' real version filters them all, because an unversioned chunk is never current.
#[test]
fn an_index_without_the_later_columns_reads_them_as_null() {
    let f = fixture();
    let q = recorded(&f, "S4-legacy-slot", "legacy slot");
    let mut dump = f["snapshots"][q["index"].as_str().unwrap()].clone();
    let version = dump["chunks"][0]["version"].as_str().unwrap().to_owned();
    let file_name = dump["chunks"][0]["file"].as_str().unwrap().to_owned();
    for sql in dump["schema"].as_array_mut().unwrap() {
        if sql.as_str().unwrap().starts_with("CREATE TABLE chunks") {
            *sql = Value::String("CREATE TABLE chunks (id INTEGER PRIMARY KEY, file TEXT NOT NULL, chunk_no INTEGER NOT NULL, body TEXT NOT NULL, content_hash TEXT NOT NULL, updated_at TEXT NOT NULL DEFAULT (datetime('now')))".into());
        }
    }
    let old = dump["chunks"].as_array().unwrap().clone();
    dump["chunks"] = Value::Array(Vec::new());
    let current = |dir: &Path, with_columns: bool| {
        let file = dir.join(if with_columns { "new.db" } else { "old.db" });
        let mut d = dump.clone();
        if with_columns {
            d = f["snapshots"][q["index"].as_str().unwrap()].clone();
        }
        rebuild(&file, &d);
        if !with_columns {
            let conn = Connection::open(&file).unwrap();
            for c in &old {
                conn.execute(
                    "INSERT INTO chunks (id, file, chunk_no, body, content_hash) VALUES (?, ?, ?, ?, ?)",
                    rusqlite::params![
                        c["id"].as_i64().unwrap(),
                        c["file"].as_str().unwrap(),
                        c["chunk_no"].as_i64().unwrap(),
                        c["body"].as_str().unwrap(),
                        c["content_hash"].as_str().unwrap()
                    ],
                )
                .unwrap();
            }
        }
        open_index(&file).unwrap().unwrap()
    };
    let dir = tempfile::tempdir().unwrap();
    let (old_conn, new_conn) = (current(dir.path(), false), current(dir.path(), true));
    let query = query_of(&q);
    let none = Versions::new();
    assert_eq!(
        triples(search(&old_conn, target_of(&q), &query, TOP_K, &none).unwrap()),
        want_of(&q)
    );
    let pending: Versions = [(file_name.clone(), Some(version))].into();
    let of_file = |conn: &Connection| {
        search(conn, target_of(&q), &query, TOP_K, &pending)
            .unwrap()
            .into_iter()
            .filter(|h| h.file == file_name)
            .count()
    };
    assert!(of_file(&new_conn) > 0, "the chunks' own version is current");
    assert_eq!(
        of_file(&old_conn),
        0,
        "an unversioned chunk is never current"
    );
}

/// A WAL index whose last writer died with committed frames still in `-wal`: the native search is
/// then the only (and last) connection, and must not checkpoint them into the main file on close.
#[test]
fn closing_after_a_search_does_not_checkpoint_into_the_index() {
    let f = fixture();
    let q = recorded(&f, "S1-map-slot", "mixed files");
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("live.db");
    rebuild(&live, &f["snapshots"][q["index"].as_str().unwrap()]);
    let writer = Connection::open(&live).unwrap();
    writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    writer
        .execute(
            "INSERT INTO vec_meta (key, value) VALUES ('qa-pending', 'x')",
            [],
        )
        .unwrap();
    // A copy of the files while the writer still holds its frames: what a crash leaves behind.
    let crashed = dir.path().join("crashed.db");
    for ext in ["", "-wal"] {
        std::fs::copy(
            format!("{}{ext}", live.display()),
            format!("{}{ext}", crashed.display()),
        )
        .unwrap();
    }
    drop(writer);
    let wal = std::fs::metadata(format!("{}-wal", crashed.display()))
        .unwrap()
        .len();
    assert!(wal > 0, "the copy holds WAL frames");
    let main_before = std::fs::read(&crashed).unwrap();
    let conn = open_index(&crashed).unwrap().unwrap();
    let got = search(&conn, target_of(&q), &query_of(&q), TOP_K, &Versions::new()).unwrap();
    assert_eq!(triples(got), want_of(&q));
    drop(conn);
    assert_eq!(
        std::fs::read(&crashed).unwrap(),
        main_before,
        "the close checkpointed into the index"
    );
}
