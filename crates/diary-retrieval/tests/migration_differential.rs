//! Differential test: every scenario of `migration-fixtures.json` (recorded from the frozen Python
//! `EmbeddingMigration` by `generate_migration_fixtures.py`) replayed on [`Migration`] over the
//! same synthetic service index and tenant files: each step's result, the pacing sleeps, the
//! timers, every embedding call, the saved configuration file and full index dumps must match.
#![allow(clippy::indexing_slicing, clippy::panic)] // serde_json `Value[...]` on fixture data

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use diary_retrieval::embedding::config::EmbeddingConfig;
use diary_retrieval::index::{Embedder, Kind, Options as IndexOptions, Retriever};
use diary_retrieval::migration::{Embed, FileGuard, Migration, MigrationError, Options};
use diary_retrieval::{NoGuard, Target};
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn fake_vector(profile: &str, text: &str, dim: usize) -> Vec<f64> {
    let mut v = vec![0.0; dim];
    let lower = text.to_lowercase();
    for token in lower
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|t| !t.is_empty())
    {
        let h = Sha256::digest(format!("{profile}|{token}").as_bytes());
        if let Some(slot) = v.get_mut(usize::from(h[0]) % dim) {
            *slot += 1.0 + f64::from(h[1] % 4) * 0.5;
        }
    }
    if let Some(last) = v.last_mut() {
        *last += 0.25;
    }
    v
}

#[derive(Default)]
struct FakeState {
    refuse: Option<String>,
    fail_all: bool,
    calls: Vec<Value>,
    /// change_text_on: (marker, the index files, done)
    change_text: Option<(String, Vec<PathBuf>, bool)>,
}

#[derive(Default)]
struct Fake(Mutex<FakeState>);

impl Fake {
    fn vectors(&self, target: &Target, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
        let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let hashes: Vec<String> = texts
            .iter()
            .map(|t| hex(&Sha256::digest(t.as_bytes()))[..10].to_owned())
            .collect();
        s.calls
            .push(json!([target.model, target.base_url, "document", hashes]));
        if let Some((marker, files, done)) = s.change_text.as_mut() {
            if let Some(t) = texts
                .iter()
                .find(|t| t.contains(marker.as_str()))
                .filter(|_| !*done)
            {
                *done = true;
                for f in files.iter() {
                    if let Ok(c) = Connection::open(f) {
                        let _ = c.execute(
                            "UPDATE chunks SET content_hash = 'qa-changed' WHERE body = ?",
                            [t],
                        );
                    }
                }
            }
        }
        if s.fail_all
            || s.refuse
                .as_ref()
                .is_some_and(|m| texts.iter().any(|t| t.contains(m.as_str())))
        {
            return Err("synthetic embed refusal".into());
        }
        let dim = if target.profile == "embeddinggemma-2" {
            8
        } else {
            16
        };
        Ok(texts
            .iter()
            .map(|t| {
                let v = fake_vector(&target.profile, t, dim);
                if target
                    .base_url
                    .as_deref()
                    .is_some_and(|u| u.contains("perturbed"))
                {
                    v.iter()
                        .enumerate()
                        .map(|(i, x)| x + if i % 2 == 1 { 1.5 } else { 0.0 })
                        .collect()
                } else {
                    v
                }
            })
            .collect())
    }
}

impl Embed for Fake {
    fn embed(&self, target: &Target, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
        self.vectors(target, texts)
    }
}

/// The index builder's embedder: the active target is the seeded one.
struct Builder<'a>(&'a Fake);

impl Embedder for Builder<'_> {
    fn active(&self) -> Target {
        Target::new(None, "qa-embed-a", "nomic-v1")
    }
    fn embed(&self, target: &Target, texts: &[&str], _kind: Kind) -> Result<Vec<Vec<f64>>, String> {
        let owned: Vec<String> = texts.iter().map(|t| (*t).to_owned()).collect();
        self.0.vectors(target, &owned)
    }
}

fn day(header: &str, subs: &[(String, String)]) -> String {
    let mut lines = vec![header.to_owned(), String::new()];
    for (h, me) in subs {
        lines.extend([
            format!("### {h}"),
            String::new(),
            format!("**Me:** {me}"),
            String::new(),
        ]);
    }
    lines.join("\n")
}

fn corpus(n: usize, tag: &str) -> String {
    let mut parts = vec!["# QA".to_owned(), String::new()];
    for d in 0..n {
        let subs: Vec<(String, String)> = (0..3)
            .map(|k| {
                let refuse = if (d + k) % 7 == 3 { "QAREFUSE" } else { "" };
                (
                    format!("{}:00 — {tag} {d}-{k}", 8 + k),
                    format!("Synthetic {tag} entry {d} {k} about kites tides lanterns {refuse}"),
                )
            })
            .collect();
        parts.push(day(&format!("## Monday, March {}, 2031", d + 1), &subs));
    }
    parts.join("\n")
}

fn connect(path: &Path) -> rusqlite::Result<Connection> {
    let _ = sqlite_vec_loader::register();
    Connection::open(path)
}

fn rows(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(sql)?;
    let n = stmt.column_count();
    let out = stmt
        .query_map([], |r| {
            let mut row = Vec::new();
            for i in 0..n {
                row.push(match r.get::<_, rusqlite::types::Value>(i)? {
                    rusqlite::types::Value::Null => Value::Null,
                    rusqlite::types::Value::Integer(x) => json!(x),
                    rusqlite::types::Value::Real(x) => json!(x),
                    rusqlite::types::Value::Text(x) => json!(x),
                    rusqlite::types::Value::Blob(x) => json!(hex(&x)),
                });
            }
            Ok(Value::Array(row))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

fn dump(conn: &Connection) -> rusqlite::Result<Value> {
    let ok = |n: &str| {
        let h = |s: &str| {
            s.len() == 16
                && s.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        };
        matches!(n, "chunks" | "vec_meta" | "dirty_documents" | "vec_items")
            || n.strip_prefix("vec_").is_some_and(h)
            || n.strip_prefix("vecmap_").is_some_and(h)
    };
    let mut tables: Vec<String> =
        rows(conn, "SELECT name FROM sqlite_master WHERE type = 'table'")?
            .into_iter()
            .filter_map(|r| r[0].as_str().map(str::to_owned))
            .filter(|n| ok(n))
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
    Ok(json!({
        "tables": tables,
        "chunks": rows(conn, "SELECT id, day, file, header, content_hash FROM chunks ORDER BY id")?,
        "vec_meta": rows(conn, "SELECT key, value FROM vec_meta ORDER BY key")?,
        "vectors": vectors,
    }))
}

/// The guard of the scenario: none, but it can change the configuration once just before a
/// write runs (the configuration changing while the write waited for its guard).
struct Guard<'a> {
    config: &'a Mutex<EmbeddingConfig>,
    bump: Mutex<Option<Value>>,
}

impl FileGuard for Guard<'_> {
    fn write(
        &self,
        _path: &Path,
        write: &mut dyn FnMut() -> diary_retrieval::Result<()>,
    ) -> diary_retrieval::Result<()> {
        if let Some(target) = self.bump.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = self
                .config
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .set_next(&target);
        }
        write()
    }
}

fn mask(mut status: Value) -> Value {
    if let Some(lp) = status.get_mut("last_pass").and_then(Value::as_object_mut) {
        lp.remove("at");
    }
    status
}

fn error_json(e: MigrationError) -> Value {
    match e {
        MigrationError::Config(c, coverage) => {
            json!({"error": c.code, "status": c.status, "message": c.message, "coverage": coverage})
        }
        MigrationError::Change(c) => json!({"error": "change", "message": c.to_string()}),
        MigrationError::Index(e) => json!({"error": "index", "message": e.to_string()}),
    }
}

fn run_scenario(name: &str, steps: &[Value], tenants: &[Value], options: &Value) -> TestResult {
    let dir = tempfile::tempdir()?;
    let tmp = dir.path();
    let service_db = tmp.join("index.db");
    let t0 = tenants[0].as_str().ok_or("tenant")?;
    let t1 = tenants[1].as_str().ok_or("tenant")?;
    let files: Vec<(PathBuf, String)> = vec![
        (service_db.clone(), corpus(4, "service")),
        (
            tmp.join("users").join(t0).join("index.db"),
            corpus(3, "tenantA"),
        ),
        (
            tmp.join("users").join(t1).join("managed-index.db"),
            corpus(2, "tenantB"),
        ),
    ];
    std::fs::create_dir_all(
        tmp.join("users")
            .join("00000000-0000-4000-8000-00000000a903"),
    )?;
    let fake = Fake::default();
    for (path, text) in &files {
        let mut r = Retriever::open(path, Builder(&fake), NoGuard, IndexOptions::default())?;
        r.reindex_file("2031-03.md", Some(text))?;
    }
    fake.0
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .calls
        .clear();
    let cfg = json!({"llm": {"embed_model": "qa-embed-a"}, "retrieval": {"db_path": service_db.to_string_lossy()}});
    let state_file = tmp.join("embedding-config.json");
    let config = Mutex::new(EmbeddingConfig::new(&cfg, Some(&state_file))?);
    let clock = Arc::new(Mutex::new(1000.0f64));
    let sleeps: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let c = clock.clone();
    let sl = sleeps.clone();
    let guard = Guard {
        config: &config,
        bump: Mutex::new(None),
    };
    let duty = match &options["duty"] {
        Value::String(s) if s == "nan" => f64::NAN,
        v => v.as_f64().unwrap_or(0.5),
    };
    let skip = options["skip"]
        .as_u64()
        .and_then(|i| tenants.get(i as usize))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let m = Migration::new(
        &config,
        &fake,
        &guard,
        &service_db,
        Options {
            duty: Some(duty),
            batch: Some(options["batch"].as_f64().unwrap_or(2.0)),
            retry_s: options["retry_s"].as_f64().unwrap_or(60.0),
            cooldown_s: 600.0,
            max_cooldown_s: 86400.0,
            active_budget: 6,
        },
        Box::new(move |u| skip.as_deref() == Some(u)),
        Box::new(move || {
            let mut t = c.lock().unwrap_or_else(|p| p.into_inner());
            *t += 0.5;
            *t
        }),
        Box::new(move |s| sl.lock().unwrap_or_else(|p| p.into_inner()).push(s)),
    );
    let timers = RefCell::new(Vec::<Value>::new());
    for (i, step) in steps.iter().enumerate() {
        let op = step["op"].as_str().ok_or("op")?;
        sleeps.lock().unwrap_or_else(|p| p.into_inner()).clear();
        timers.borrow_mut().clear();
        fake.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .calls
            .clear();
        let result: Value = match op {
            "status" => m.status(false).map(mask).unwrap_or_else(error_json),
            "recheck_fired" => {
                m.recheck_fired();
                Value::Null
            }
            "retry_fired" => {
                m.retry_fired();
                Value::Null
            }
            "bump_in_guard" => {
                *guard.bump.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(step["target"].clone());
                Value::Null
            }
            "change_text_on" => {
                let all: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();
                fake.0.lock().unwrap_or_else(|p| p.into_inner()).change_text = Some((
                    step["marker"].as_str().ok_or("marker")?.to_owned(),
                    all,
                    false,
                ));
                Value::Null
            }
            "run" => {
                let t = m.run_once(&|| 0.0);
                if let Some(s) = t.retry {
                    timers.borrow_mut().push(json!(["retry", s]));
                }
                if let Some(s) = t.recheck {
                    timers.borrow_mut().push(json!(["recheck", s]));
                }
                let mut lp = m
                    .last_pass
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                    .unwrap_or(Value::Null);
                if let Some(o) = lp.as_object_mut() {
                    o.remove("at");
                }
                lp
            }
            "set_next" => m
                .set_next(&step["target"])
                .map(mask)
                .unwrap_or_else(error_json),
            "switch" => m
                .switch(step["force"].as_bool().unwrap_or(false))
                .map(mask)
                .unwrap_or_else(error_json),
            "rollback" => m.rollback().map(mask).unwrap_or_else(error_json),
            "finish" => m.finish().map(mask).unwrap_or_else(error_json),
            "clear_next" => m.clear_next().map(mask).unwrap_or_else(error_json),
            "retry_missing" => {
                m.clear_refusals();
                m.status(false).map(mask).unwrap_or_else(error_json)
            }
            "refuse" => {
                fake.0.lock().unwrap_or_else(|p| p.into_inner()).refuse =
                    step["marker"].as_str().map(str::to_owned);
                Value::Null
            }
            "fail_all" => {
                fake.0.lock().unwrap_or_else(|p| p.into_inner()).fail_all =
                    step["on"].as_bool().unwrap_or(false);
                Value::Null
            }
            "advance" => {
                *clock.lock().unwrap_or_else(|p| p.into_inner()) +=
                    step["seconds"].as_f64().ok_or("seconds")?;
                Value::Null
            }
            "reset_active_vectors" => {
                for (path, _) in &files {
                    connect(path)?.execute_batch("DELETE FROM vec_items")?;
                }
                Value::Null
            }
            "break_tenant" => {
                let p = &files[1].0;
                std::fs::remove_file(p)?;
                Connection::open(p)?.execute_batch("CREATE TABLE unrelated (x)")?;
                Value::Null
            }
            "dump" => {
                let mut out = Map::new();
                for (path, _) in &files {
                    let conn = connect(path)?;
                    let has = conn
                        .query_row(
                            "SELECT 1 FROM sqlite_master WHERE name='chunks'",
                            [],
                            |_| Ok(()),
                        )
                        .is_ok();
                    let rel = path.strip_prefix(tmp)?.to_string_lossy().into_owned();
                    out.insert(rel, if has { dump(&conn)? } else { Value::Null });
                }
                Value::Object(out)
            }
            other => return Err(format!("unknown op {other}").into()),
        };
        let mut want = step["expected"].clone();
        // The pass error names only the Python class here (messages are the driver's own).
        if op == "run" {
            if let Some(e) = want.get_mut("error").filter(|e| e.is_string()) {
                *e = json!(e.as_str().and_then(|s| s.split(':').next()).unwrap_or(""));
            }
        }
        assert_eq!(result, want, "{name} step {i} ({op}): result");
        assert_eq!(
            json!(*sleeps.lock().unwrap_or_else(|p| p.into_inner())),
            step["sleeps"],
            "{name} step {i} ({op}): sleeps"
        );
        assert_eq!(
            json!(*timers.borrow()),
            step["timers"],
            "{name} step {i} ({op}): timers"
        );
        assert_eq!(
            json!(fake.0.lock().unwrap_or_else(|p| p.into_inner()).calls),
            step["embed_calls"],
            "{name} step {i} ({op}): embed calls"
        );
        let state = std::fs::read_to_string(&state_file).ok();
        assert_eq!(
            json!(state),
            step["state_file"],
            "{name} step {i} ({op}): saved configuration"
        );
    }
    Ok(())
}

#[test]
fn every_scenario_matches_python() -> TestResult {
    let fx: Value = serde_json::from_str(include_str!("migration-fixtures.json"))?;
    let tenants = fx["tenants"].as_array().ok_or("tenants")?;
    let scenarios = fx["scenarios"].as_object().ok_or("scenarios")?;
    assert!(scenarios.len() >= 10);
    for (name, steps) in scenarios {
        run_scenario(
            name,
            steps.as_array().ok_or("steps")?,
            tenants,
            &fx["options"][name],
        )?;
    }
    Ok(())
}

#[test]
fn a_migration_can_be_shared_between_threads() {
    fn shared<T: Sync>() {}
    shared::<Migration<'static>>();
}

/// Known difference (a Python defect): after one refusal, an endpoint that starts answering vectors
/// of another width writes nothing; Python's fill would read the same rows again for ever. Here the
/// fill reports stuck and the pass ends.
#[test]
fn a_fill_that_writes_nothing_and_refuses_nothing_new_ends() -> TestResult {
    struct Shifting(Mutex<usize>);
    impl Embed for Shifting {
        fn embed(&self, target: &Target, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
            if texts.iter().any(|t| t.contains("QAREFUSE")) {
                return Err("refused".into());
            }
            let mut calls = self.0.lock().unwrap_or_else(|p| p.into_inner());
            *calls += 1;
            let dim = if *calls == 1 { 8 } else { 4 };
            Ok(texts
                .iter()
                .map(|t| fake_vector(&target.profile, t, dim))
                .collect())
        }
    }
    let dir = tempfile::tempdir()?;
    let service_db = dir.path().join("index.db");
    let fake = Fake::default();
    let mut r = Retriever::open(
        &service_db,
        Builder(&fake),
        NoGuard,
        IndexOptions::default(),
    )?;
    // Day 0 sub 3 and day 1 sub 2 carry the refusal marker: the first batch of two holds one.
    r.reindex_file("2031-03.md", Some(&corpus(4, "service")))?;
    drop(r);
    let cfg = json!({"llm": {"embed_model": "qa-embed-a"}, "retrieval": {"db_path": service_db.to_string_lossy()}});
    let config = Mutex::new(EmbeddingConfig::new(&cfg, None)?);
    let embed = Shifting(Mutex::new(0));
    let guard = Guard {
        config: &config,
        bump: Mutex::new(None),
    };
    let m = Migration::new(
        &config,
        &embed,
        &guard,
        &service_db,
        Options {
            batch: Some(16.0),
            ..Options::default()
        },
        Box::new(|_| false),
        Box::new(|| 0.0),
        Box::new(|_| {}),
    );
    m.set_next(&json!({"model": "qa-embed-b", "profile": "embeddinggemma-2"}))
        .map_err(|e| format!("{e:?}"))?;
    let _ = m.run_once(&|| 0.0);
    let last = m
        .last_pass
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .ok_or("no pass")?;
    assert_eq!(last["failed"], json!(1), "{last}");
    // One batch, its one-by-one retry, and one more round that writes nothing: then stuck.
    assert!(
        *embed.0.lock().unwrap_or_else(|p| p.into_inner()) <= 40,
        "bounded number of embedding calls"
    );
    Ok(())
}
