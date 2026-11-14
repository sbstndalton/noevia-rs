#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

//! Replays the steps `generate_index_fixtures.cjs` recorded from core's rag.cjs (index, tail append,
//! delete, busy, embedding failures, retarget) with this crate, and checks after every step that the
//! result, the per-file state sizes, the embed requests and the whole index file are Node's.

use project_index::chunk::{chunk_text, hash, units};
use project_index::embed::{embed_framed, Vector};
use project_index::index::{self, Index, TailChunk, TailGates, TailRequest};
use project_index::slots::Target;
use project_index::state::{FileKey, FileState, Outcome};
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn chunk_text_matches_node() {
    let f = fixture("chunk-text.json");
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 10);
    for case in cases {
        let want: Vec<Vec<u16>> = case["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| match c.as_str() {
                Some(text) => units(text),
                None => c.as_array().unwrap().iter().map(|u| u.as_u64().unwrap() as u16).collect(),
            })
            .collect();
        let got = chunk_text(&units(case["text"].as_str().unwrap()));
        assert_eq!(got, want, "case {}", case["name"]);
    }
}

const WORDS: [&str; 8] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"];

/// The recorder's `vectorFor` of a chunk's unframed text.
fn vector_for(text: &str, dim: usize) -> Vec<f64> {
    let mut v = vec![0f64; dim];
    let lower = text.to_lowercase();
    for w in lower.split(|c: char| !c.is_ascii_lowercase()).filter(|w| !w.is_empty()) {
        if let Some(i) = WORDS.iter().position(|x| *x == w) {
            if i < dim - 1 {
                v[i] += 1.0;
            }
        }
    }
    let h = Sha256::digest(text.as_bytes());
    v[dim - 1] += f64::from(h[0]) / 2550.0;
    v.into_iter().map(|x| f64::from(x as f32)).collect()
}

fn target(v: &Value) -> Target {
    Target { profile: v["profile"].as_str().unwrap().into(), model: v["model"].as_str().unwrap().into() }
}

fn dump(path: &Path) -> Value {
    if !path.exists() {
        return Value::Null;
    }
    let db = Connection::open(path).unwrap();
    let rows = |sql: &str| -> Vec<Value> {
        let mut st = db.prepare(sql).unwrap();
        let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
        st.query_map([], |r| {
            let mut o = serde_json::Map::new();
            for (i, n) in names.iter().enumerate() {
                let v = match r.get_ref(i).unwrap() {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => json!(n),
                    rusqlite::types::ValueRef::Real(f) => json!(f),
                    rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                    rusqlite::types::ValueRef::Blob(b) => {
                        Value::Array(b.chunks(4).map(|c| json!(f64::from(f32::from_le_bytes(c.try_into().unwrap())))).collect())
                    }
                };
                o.insert(n.clone(), v);
            }
            Ok(Value::Object(o))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };
    let master = rows("SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid");
    let virtuals: Vec<String> = master
        .iter()
        .filter(|m| m["sql"].as_str().unwrap().to_uppercase().contains("CREATE VIRTUAL TABLE"))
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect();
    let mut vectors = serde_json::Map::new();
    for v in &virtuals {
        vectors.insert(v.clone(), Value::Array(rows(&format!("SELECT chunk_id, embedding FROM {v} ORDER BY chunk_id"))));
    }
    let mut maps = serde_json::Map::new();
    for m in &master {
        let name = m["name"].as_str().unwrap();
        if m["type"] == "table" && name.starts_with("vecmap_") {
            maps.insert(name.into(), Value::Array(rows(&format!("SELECT chunk_id, content_hash FROM {name} ORDER BY chunk_id"))));
        }
    }
    let columns: Vec<Value> = rows("PRAGMA table_info(chunks)").into_iter().map(|c| c["name"].clone()).collect();
    json!({
        "columns": columns,
        "chunks": rows("SELECT id, file, chunk_no, body, content_hash, version, direct, kind, meta FROM chunks ORDER BY id"),
        "vec_meta": rows("SELECT key, value FROM vec_meta ORDER BY key"),
        "vectors": vectors,
        "maps": maps,
    })
}

/// Numbers compared by value (Node prints 1 where Rust holds 1.0).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        // serde_json's default float parse may be one f64 ulp off; the stored vectors are f32.
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            x == y || (x as f32) == (y as f32)
        }
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

struct Replay {
    dir: PathBuf,
    target: RefCell<Target>,
    state: RefCell<FileState>,
    busy: Cell<bool>,
    failing: Cell<bool>,
    short: Cell<bool>,
    calls: RefCell<Vec<usize>>,
    dim: usize,
}

const USER: &str = "qa-user-p1";

impl Replay {
    fn path(&self, project: &str) -> PathBuf {
        self.dir.join("users").join(USER).join("rag").join(format!("{project}.db"))
    }

    fn open(&self, project: &str) -> Result<Option<Index>, String> {
        index::open_index(&self.path(project), &self.target.borrow(), &|| false)
    }

    fn embed(&self, _t: &Target, texts: &[String]) -> Result<Vec<Vector>, String> {
        let mut once = |input: &[String]| -> Result<Vec<Value>, String> {
            self.calls.borrow_mut().push(input.len());
            if self.failing.get() {
                return Err("embeddings 500: down".into());
            }
            if self.short.get() && input.len() > 1 {
                return Ok(vec![]);
            }
            let mut data: Vec<Value> =
                input.iter().enumerate().map(|(i, t)| json!({"index": i, "embedding": vector_for(t, self.dim)})).collect();
            data.reverse();
            Ok(data)
        };
        embed_framed(&mut once, texts)
    }

    fn step(&self, step: &Value) -> Value {
        let project = step["projectId"].as_str().unwrap_or_default();
        let file = step["file"].as_str().unwrap_or_default();
        let key = FileKey::new(project, file, Some(USER));
        let mut embed = |t: &Target, texts: &[String]| self.embed(t, texts);
        match step["step"].as_str().unwrap() {
            "index" => {
                let text = step["text"].as_str().unwrap();
                let version = hash(text);
                let g = self.state.borrow_mut().request_index(&key, &version);
                let mut current = || self.state.borrow().current(&key, Some(g));
                let out = index::index_file_now(&mut || self.open(project), file, &units(text), &version, &mut current, &mut embed);
                let outcome = if out.ok {
                    Outcome::Ok
                } else if out.reason.as_deref() == Some("failed") {
                    Outcome::Failed
                } else {
                    Outcome::Unavailable
                };
                self.state.borrow_mut().drained(&key, outcome, true);
                out.to_json()
            }
            "tail" => {
                let tail: Vec<TailChunk> = step["tail"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| TailChunk { body: t["body"].as_str().unwrap().into(), page: t["page"].as_i64() })
                    .collect();
                let o = &step["opts"];
                let req = TailRequest {
                    file,
                    head_text: step["headText"].as_str().unwrap(),
                    tail: &tail,
                    limit: o["limit"].as_u64().unwrap() as usize,
                    from_page: o["fromPage"].as_i64(),
                    to_page: o["toPage"].as_i64(),
                    final_window: o["final"].as_bool().unwrap(),
                };
                let g = self.state.borrow().generation(&key);
                let wanted = self.state.borrow().wanted(&key).map(|w| w.map(str::to_string));
                let mut current = || self.state.borrow().current(&key, g);
                let busy = || self.busy.get();
                let mut gates = TailGates {
                    is_current: &mut current,
                    busy: &busy,
                    index_exists: self.path(project).exists(),
                    wanted: wanted.as_ref().map(|w| w.as_deref()),
                    embedding_off: false,
                };
                index::append_tail_now(&mut || self.open(project), &req, &mut gates, &mut embed).unwrap().to_json()
            }
            "delete" => {
                self.state.borrow_mut().request_delete(&key);
                let dropped = index::delete_file(&mut || self.open(project), file, step["keepMedia"].as_bool().unwrap()).unwrap();
                self.state.borrow_mut().after_delete(&key, dropped, true, false);
                Value::Null
            }
            other => panic!("step {other}"),
        }
    }
}

#[test]
fn index_writes_match_node() {
    sqlite_vec_loader::register().unwrap();
    let f = fixture("index-steps.json");
    let dim = f["dim"].as_u64().unwrap() as usize;
    let mut checked = 0;
    for sc in f["scenarios"].as_array().unwrap() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Replay {
            dir: tmp.path().to_path_buf(),
            target: RefCell::new(target(&sc["target"])),
            state: RefCell::new(FileState::new()),
            busy: Cell::new(false),
            failing: Cell::new(false),
            short: Cell::new(false),
            calls: RefCell::new(Vec::new()),
            dim,
        };
        for (i, step) in sc["steps"].as_array().unwrap().iter().enumerate() {
            let at = format!("{} step {i} ({})", sc["name"], step["step"]);
            match step["step"].as_str().unwrap() {
                "busy" => r.busy.set(step["on"].as_bool().unwrap()),
                "fail" => r.failing.set(step["on"].as_bool().unwrap()),
                "short" => r.short.set(step["on"].as_bool().unwrap()),
                "retarget" => *r.target.borrow_mut() = target(&step["target"]),
                _ => {
                    let got = r.step(step);
                    assert!(same(&got, &step["result"]), "{at}: result\n rust {got}\n node {}", step["result"]);
                    let calls: Vec<usize> = r.calls.borrow_mut().drain(..).collect();
                    let want: Vec<usize> = step["calls"].as_array().unwrap().iter().map(|c| c.as_u64().unwrap() as usize).collect();
                    assert_eq!(calls, want, "{at}: embed requests");
                    let (g, v) = r.state.borrow().tracked();
                    assert_eq!(json!({"generations": g, "versions": v}), step["tracked"], "{at}: tracked state");
                    let got = dump(&r.path(step["projectId"].as_str().unwrap()));
                    let want = &step["index"];
                    for part in ["columns", "chunks", "vec_meta", "vectors", "maps"] {
                        if got.is_null() || want.is_null() {
                            assert_eq!(got.is_null(), want.is_null(), "{at}: index exists");
                            break;
                        }
                        assert!(same(&got[part], &want[part]), "{at}: index {part}\n rust {}\n node {}", got[part], want[part]);
                    }
                    checked += 1;
                }
            }
        }
    }
    assert!(checked >= 30, "only {checked} steps checked");
}
