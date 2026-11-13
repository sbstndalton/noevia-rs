//! Differential test: replays every scenario of `retrieval-fixtures.json` (recorded from the
//! frozen Python retriever by `generate_fixtures.py`) on the Rust index and compares each step's
//! result, the embedding calls it made, and full dumps of the index file.

#![allow(clippy::indexing_slicing)] // serde_json `Value[...]` on fixture data: a missing key reads as Null and fails the comparison.

use std::cell::RefCell;
use std::collections::HashMap;

use diary_retrieval::index::{Embedder, Kind, Options, ReindexPlan, Retriever};
use diary_retrieval::{slots, NoGuard, Target};
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Same as `fake_vector` in the generator.
fn fake_vector(profile: &str, text: &str, dim: usize, scale: f64) -> Vec<f64> {
    if dim == 0 {
        return Vec::new();
    }
    let mut v = vec![0.0; dim];
    let lower = text.to_lowercase();
    let mut token = String::new();
    let flush = |token: &mut String, v: &mut Vec<f64>| {
        if !token.is_empty() {
            let h = Sha256::digest(format!("{profile}|{token}").as_bytes());
            let (b0, b1) = (
                h.first().copied().unwrap_or(0),
                h.get(1).copied().unwrap_or(0),
            );
            if let Some(slot) = v.get_mut(usize::from(b0) % dim) {
                *slot += 1.0 + f64::from(b1 % 4) * 0.5;
            }
            token.clear();
        }
    };
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            token.push(c);
        } else {
            flush(&mut token, &mut v);
        }
    }
    flush(&mut token, &mut v);
    if let Some(last) = v.last_mut() {
        *last += 0.25;
    }
    v.into_iter().map(|x| x * scale).collect()
}

struct FakeState {
    target: Target,
    fail_marker: Option<String>,
    dim: usize,
    scale: f64,
    calls: Vec<Value>,
}

struct Fake(RefCell<FakeState>);

impl Embedder for Fake {
    fn active(&self) -> Target {
        self.0.borrow().target.clone()
    }

    fn embed(&self, target: &Target, texts: &[&str], kind: Kind) -> Result<Vec<Vec<f64>>, String> {
        let mut state = self.0.borrow_mut();
        let mut out = Vec::new();
        for text in texts {
            let h = hex(&Sha256::digest(text.as_bytes()));
            state
                .calls
                .push(json!([kind.as_str(), h.get(..12).unwrap_or_default()]));
            if let Some(marker) = &state.fail_marker {
                if text.contains(marker.as_str()) {
                    return Err("synthetic embed failure".into());
                }
            }
            out.push(fake_vector(&target.profile, text, state.dim, state.scale));
        }
        Ok(out)
    }
}

fn target(v: &Value) -> TestResult<Target> {
    Ok(Target {
        base_url: v.get("base_url").and_then(Value::as_str).map(str::to_owned),
        model: v
            .get("model")
            .and_then(Value::as_str)
            .ok_or("model")?
            .to_owned(),
        profile: v
            .get("profile")
            .and_then(Value::as_str)
            .ok_or("profile")?
            .to_owned(),
    })
}

fn sql_value(v: rusqlite::types::Value) -> Value {
    match v {
        rusqlite::types::Value::Null => Value::Null,
        rusqlite::types::Value::Integer(i) => json!(i),
        rusqlite::types::Value::Real(f) => json!(f),
        rusqlite::types::Value::Text(s) => json!(s),
        rusqlite::types::Value::Blob(b) => json!(hex(&b)),
    }
}

fn table_name_ok(name: &str) -> bool {
    let hex16 =
        |s: &str| s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    matches!(
        name,
        "chunks" | "vec_meta" | "dirty_documents" | "vec_items"
    ) || name.strip_prefix("vec_").is_some_and(hex16)
        || name.strip_prefix("vecmap_").is_some_and(hex16)
}

fn rows(conn: &Connection, sql: &str) -> TestResult<Vec<Value>> {
    let mut stmt = conn.prepare(sql)?;
    let n = stmt.column_count();
    let out = stmt
        .query_map([], |r| {
            let mut row = Vec::with_capacity(n);
            for i in 0..n {
                row.push(sql_value(r.get(i)?));
            }
            Ok(Value::Array(row))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

fn dump(conn: &Connection) -> TestResult<Value> {
    let mut tables: Vec<String> =
        rows(conn, "SELECT name FROM sqlite_master WHERE type = 'table'")?
            .into_iter()
            .filter_map(|r| r.get(0).and_then(Value::as_str).map(str::to_owned))
            .filter(|n| table_name_ok(n))
            .collect();
    tables.sort();
    let mut vectors = Map::new();
    for name in &tables {
        if name.starts_with("vecmap_") {
            vectors.insert(
                name.clone(),
                json!(rows(
                    conn,
                    &format!("SELECT chunk_id, content_hash FROM {name} ORDER BY chunk_id")
                )?),
            );
        } else if name.starts_with("vec_") && name != "vec_meta" {
            vectors.insert(
                name.clone(),
                json!(rows(
                    conn,
                    &format!("SELECT chunk_id, embedding FROM {name} ORDER BY chunk_id")
                )?),
            );
        }
    }
    let dirty: Vec<Value> = rows(
        conn,
        "SELECT document FROM dirty_documents ORDER BY document",
    )?
    .into_iter()
    .filter_map(|r| r.get(0).cloned())
    .collect();
    Ok(json!({
        "tables": tables,
        "chunks": rows(conn, "SELECT id, xid, day, file, header, body, content_hash FROM chunks ORDER BY id")?,
        "vec_meta": rows(conn, "SELECT key, value FROM vec_meta ORDER BY key")?,
        "dirty": dirty,
        "vectors": vectors,
    }))
}

fn legacy_seed(path: &std::path::Path, op: &Value) -> TestResult {
    sqlite_vec_loader::register()?;
    let conn = Connection::open(path)?;
    conn.execute_batch(diary_retrieval::index::SCHEMA)?;
    let dim = op["dim"].as_u64().ok_or("dim")? as usize;
    let profile = op["profile"].as_str().ok_or("profile")?;
    let file = op["file"].as_str().ok_or("file")?;
    conn.execute_batch(&format!("CREATE VIRTUAL TABLE vec_items USING vec0(chunk_id INTEGER PRIMARY KEY, embedding float[{dim}])"))?;
    conn.execute(
        "INSERT INTO vec_meta (key, value) VALUES ('dim', ?)",
        [dim.to_string()],
    )?;
    for c in diary_corpus_parse::parse_chunks(op["text"].as_str()) {
        conn.execute(
            "INSERT INTO chunks (day, file, header, body, content_hash) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![c.day, file, c.header, c.text, c.content_hash],
        )?;
        let id = conn.last_insert_rowid();
        let blob = slots::serialize_f32(&fake_vector(profile, &c.text, dim, 1.0));
        conn.execute(
            "INSERT INTO vec_items (chunk_id, embedding) VALUES (?, ?)",
            rusqlite::params![id, blob],
        )?;
    }
    Ok(())
}

fn plan_json(plan: &ReindexPlan) -> Value {
    let mut hashes: Vec<&String> = plan.embeddings.keys().collect();
    hashes.sort();
    json!({
        "pending": plan.pending,
        "embedded_hashes": hashes,
        "rows": plan.rows.iter().map(|r| json!([r.day, r.header, r.text, r.content_hash])).collect::<Vec<_>>(),
    })
}

fn run_scenario(name: &str, steps: &[Value]) -> TestResult {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("index.db");
    let mut r: Option<Retriever<Fake, NoGuard>> = None;
    let mut plans: HashMap<String, ReindexPlan> = HashMap::new();
    for (i, step) in steps.iter().enumerate() {
        let op = step["op"].as_str().ok_or("op")?;
        if let Some(r) = &r {
            r.embedder().0.borrow_mut().calls.clear();
        }
        let result: Value = match op {
            "legacy_seed" => {
                legacy_seed(&db, step)?;
                Value::Null
            }
            "open" | "open_novec" | "open_guarded" => {
                drop(r.take());
                let fake = Fake(RefCell::new(FakeState {
                    target: target(&step["target"])?,
                    fail_marker: None,
                    dim: 16,
                    scale: 1.0,
                    calls: Vec::new(),
                }));
                let opened = if op == "open" {
                    Retriever::open(&db, fake, NoGuard, Options::default())?
                } else if op == "open_guarded" {
                    let options = Options {
                        adopt_on_read: false,
                        ..Options::default()
                    };
                    Retriever::open(&db, fake, NoGuard, options)?
                } else {
                    let conn = Connection::open(&db)?;
                    Retriever::with_connection(conn, false, fake, NoGuard, Options::default())?
                };
                let out = json!({"vec_available": opened.vec_available()});
                r = Some(opened);
                out
            }
            _ => {
                let r = r.as_mut().ok_or("not open")?;
                match op {
                    "set_active" => {
                        r.embedder().0.borrow_mut().target = target(&step["target"])?;
                        Value::Null
                    }
                    "fail_on" => {
                        r.embedder().0.borrow_mut().fail_marker =
                            step["marker"].as_str().map(str::to_owned);
                        Value::Null
                    }
                    "set_dim" => {
                        r.embedder().0.borrow_mut().dim =
                            step["dim"].as_u64().ok_or("dim")? as usize;
                        Value::Null
                    }
                    "reindex" => match r
                        .reindex_file(step["file"].as_str().ok_or("file")?, step["text"].as_str())
                    {
                        Ok(embedded) => {
                            json!({"embedded": embedded, "pending": r.pending_embeddings()})
                        }
                        Err(e) => json!({"error": python_error(&e)}),
                    },
                    "set_scale" => {
                        r.embedder().0.borrow_mut().scale =
                            step["scale"].as_f64().ok_or("scale")?;
                        Value::Null
                    }
                    "sql" => {
                        for statement in step["statements"].as_array().ok_or("statements")? {
                            r.connection()
                                .execute_batch(statement.as_str().ok_or("statement")?)?;
                        }
                        Value::Null
                    }
                    "sql_meta" => {
                        r.connection().execute(
                            "INSERT OR REPLACE INTO vec_meta (key, value) VALUES (?, ?)",
                            [step["key"].as_str(), step["value"].as_str()],
                        )?;
                        Value::Null
                    }
                    "slots_probe" => {
                        let t = target(&step["target"])?;
                        let conn = r.connection();
                        let slot_json = |s: &slots::Slot| json!({"id": s.id, "dim": s.dim, "table": s.table, "map": s.map});
                        json!({
                            "needs": slots::needs_legacy_adoption(conn, Some(&t))?,
                            "find": slots::find_slot(conn, Some(&t))?.as_ref().map(slot_json),
                            "list": slots::list_slots(conn)?.iter().map(slot_json).collect::<Vec<_>>(),
                        })
                    }
                    "prepare" => {
                        let plan = r.prepare_reindex(
                            step["file"].as_str().ok_or("file")?,
                            step["text"].as_str(),
                        )?;
                        let out = plan_json(&plan);
                        plans.insert(step["as"].as_str().ok_or("as")?.to_owned(), plan);
                        out
                    }
                    "apply" => {
                        let plan = plans
                            .get(step["plan"].as_str().ok_or("plan")?)
                            .ok_or("no plan")?
                            .clone();
                        let embedded = r.apply_reindex(&plan)?;
                        json!({"embedded": embedded, "pending": r.pending_embeddings()})
                    }
                    "search" => {
                        let with_ids = step["with_ids"].as_bool().ok_or("with_ids")?;
                        let (hits, status) = match r.search(
                            step["query"].as_str().ok_or("query")?,
                            step["top_k"].as_i64().ok_or("top_k")?,
                            step["min_score"].as_f64().ok_or("min_score")?,
                            with_ids,
                        ) {
                            Ok(found) => found,
                            Err(e) => {
                                check(name, i, op, &json!({"error": python_error(&e)}), step, r)?;
                                continue;
                            }
                        };
                        let hits: Vec<Value> = hits
                            .iter()
                            .map(|h| {
                                let mut o = json!({"day": h.day, "header": h.header, "text": h.text, "score": h.score});
                                if let Some(id) = h.id {
                                    o["id"] = json!(id);
                                }
                                o
                            })
                            .collect();
                        json!({"hits": hits, "status": status.map(|s| s.as_str())})
                    }
                    "dirty" => {
                        r.connection().execute(
                            "INSERT OR IGNORE INTO dirty_documents (document) VALUES (?)",
                            [step["document"].as_str()],
                        )?;
                        Value::Null
                    }
                    "undirty" => {
                        r.connection().execute(
                            "DELETE FROM dirty_documents WHERE document = ?",
                            [step["document"].as_str()],
                        )?;
                        Value::Null
                    }
                    "sql_delete_chunk" => {
                        r.connection().execute(
                            "DELETE FROM chunks WHERE id = ?",
                            [step["id"].as_i64().ok_or("id")?],
                        )?;
                        Value::Null
                    }
                    "stats" => {
                        let s = r.stats()?;
                        json!({"chunks": s.chunks, "vec_available": s.vec_available, "dim": s.dim, "model": s.model, "profile": s.profile})
                    }
                    "coverage" => {
                        let active = r.embedder().active();
                        let slot = slots::find_slot(r.connection(), Some(&active))?;
                        let c = slots::coverage(r.connection(), slot.as_ref())?;
                        json!({"chunks": c.chunks, "vectors": c.vectors})
                    }
                    "missing" => {
                        let active = r.embedder().active();
                        let slot = slots::find_slot(r.connection(), Some(&active))?;
                        let exclude: Vec<i64> = step["exclude"]
                            .as_array()
                            .ok_or("exclude")?
                            .iter()
                            .filter_map(Value::as_i64)
                            .collect();
                        let m = slots::missing_chunks(
                            r.connection(),
                            slot.as_ref(),
                            step["limit"].as_i64().ok_or("limit")?,
                            &exclude,
                        )?;
                        json!(m
                            .iter()
                            .map(|m| json!([m.id, m.body, m.content_hash]))
                            .collect::<Vec<_>>())
                    }
                    "drop_slot" => {
                        let tx = r.connection().unchecked_transaction()?;
                        let dropped = slots::drop_slot(&tx, &target(&step["target"])?)?;
                        tx.commit()?;
                        json!(dropped)
                    }
                    "dump" => dump(r.connection())?,
                    other => return Err(format!("unknown op {other}").into()),
                }
            }
        };
        let expected = python_true_is_one(step["expected"].clone());
        assert_eq!(result, expected, "{name} step {i} ({op}): result differs");
        let calls = match &r {
            Some(r) => Value::Array(r.embedder().0.borrow().calls.clone()),
            None => json!([]),
        };
        assert_eq!(
            calls, step["embed_calls"],
            "{name} step {i} ({op}): embedding calls differ"
        );
    }
    Ok(())
}

fn check(
    name: &str,
    i: usize,
    op: &str,
    result: &Value,
    step: &Value,
    r: &Retriever<Fake, NoGuard>,
) -> TestResult {
    assert_eq!(
        *result, step["expected"],
        "{name} step {i} ({op}): result differs"
    );
    let calls = Value::Array(r.embedder().0.borrow().calls.clone());
    assert_eq!(
        calls, step["embed_calls"],
        "{name} step {i} ({op}): embedding calls differ"
    );
    Ok(())
}

/// The Python exception class each Rust error stands for.
fn python_error(e: &diary_retrieval::Error) -> &'static str {
    match e {
        diary_retrieval::Error::BadWidth(_) => "ValueError",
        diary_retrieval::Error::Value(_) => "OverflowError",
        diary_retrieval::Error::Sqlite(_) => "OperationalError",
        diary_retrieval::Error::Busy(_) => "WriterBusy",
    }
}

/// A slot record's `"dim": true` is Python's int 1 (`isinstance(True, int)`); the Rust slot holds 1.
fn python_true_is_one(mut v: Value) -> Value {
    let Some(probe) = v.as_object_mut() else {
        return v;
    };
    let fix = |item: &mut Value| {
        if let Some(o) = item.as_object_mut() {
            if o.get("dim") == Some(&Value::Bool(true)) {
                o.insert("dim".into(), json!(1));
            }
        }
    };
    if let Some(find) = probe.get_mut("find") {
        fix(find);
    }
    if let Some(list) = probe.get_mut("list").and_then(Value::as_array_mut) {
        list.iter_mut().for_each(fix);
    }
    v
}

#[test]
fn every_scenario_matches_python() -> TestResult {
    let fx: Value = serde_json::from_str(include_str!("retrieval-fixtures.json"))?;
    let scenarios = fx["scenarios"].as_object().ok_or("scenarios")?;
    assert!(scenarios.len() >= 5);
    for (name, steps) in scenarios {
        run_scenario(name, steps.as_array().ok_or("steps")?)?;
    }
    Ok(())
}

#[test]
fn slot_ids_match_python() -> TestResult {
    let fx: Value = serde_json::from_str(include_str!("retrieval-fixtures.json"))?;
    for (model, profile) in [
        ("qa-embed-a", "nomic-v1"),
        ("qa-embed-b", "embeddinggemma-2"),
        ("qa-embed-c-modèl", "nomic-v1"),
    ] {
        let t = Target::new(None, model, profile);
        assert_eq!(json!(t.slot_id()), fx["slot_ids"][model], "{model}");
    }
    Ok(())
}
