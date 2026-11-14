//! Council p9 (retrieval): `native_files_context::answer`, the native `rag.cjs` `filesContext` for
//! the chat preparation, over an index Node wrote (crates/project-retrieval F2-F6, recorded search
//! "ties between the two duplicate files", the same one tests/retrieval_store.rs serves) with a
//! plain TCP stub as the embedding service answering the query vector Node bound. The excerpts the
//! context places must be Node's recorded hits, in order, with their scores. Synthetic QA data only;
//! no model.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use chat_prepare::retrieval::{Answer, Request, Source};
use js_json::JValue;
use noevia_server::native_files_context::{answer, Native};
use noevia_server::{config::Config, App};
use serde_json::{json, Value};
use server_store::rusqlite::{self, Connection};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const FIXTURE: &str =
    include_str!("../../../crates/project-retrieval/tests/fixtures/F2-F6-search.json");
const TOKEN: &str = "synthetic-files-context-secret-000000";
const START: &str = "0123456789abcdef0123456789abcdef";
const USER: &str = "0123abcd-0000-4000-8000-0000000000a2";
const PROJECT: &str = "qa-proj-1";

fn recorded() -> Value {
    let f: Value = serde_json::from_str(FIXTURE).unwrap();
    let q = f["scenarios"][0]["searches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["note"] == "ties between the two duplicate files")
        .unwrap()
        .clone();
    json!({ "search": q, "index": f["snapshots"][q["index"].as_str().unwrap()] })
}

/// Rebuilds a Node-written index from its dump (as tests/retrieval_store.rs does).
fn rebuild(path: &Path, dump: &Value) {
    embedding_worker::sqlite_vec_loader::register().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let c = Connection::open(path).unwrap();
    for sql in dump["schema"].as_array().unwrap() {
        c.execute_batch(sql.as_str().unwrap()).unwrap();
    }
    for r in dump["chunks"].as_array().unwrap() {
        c.execute(
            "INSERT INTO chunks (id, file, chunk_no, body, content_hash, version, direct, kind, meta) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![r["id"].as_i64(), r["file"].as_str(), r["chunk_no"].as_i64(), r["body"].as_str(), r["content_hash"].as_str(), r["version"].as_str(), r["direct"].as_i64(), r["kind"].as_str(), r["meta"].as_str()],
        )
        .unwrap();
    }
    for r in dump["vec_meta"].as_array().unwrap() {
        c.execute(
            "INSERT INTO vec_meta (key, value) VALUES (?, ?)",
            [r["key"].as_str(), r["value"].as_str()],
        )
        .unwrap();
    }
    for (table, rows) in dump["vectors"].as_object().unwrap() {
        for r in rows.as_array().unwrap() {
            let blob: Vec<u8> = r["embedding"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|x| (x.as_f64().unwrap() as f32).to_le_bytes())
                .collect();
            let id = r["chunk_id"].as_i64().unwrap();
            c.execute(
                &format!("INSERT INTO {table} (chunk_id, embedding) VALUES ({id}, ?)"),
                [blob],
            )
            .unwrap();
        }
    }
    for (table, rows) in dump["maps"].as_object().unwrap() {
        for r in rows.as_array().unwrap() {
            c.execute(
                &format!("INSERT INTO {table} (chunk_id, content_hash) VALUES (?, ?)"),
                rusqlite::params![r["chunk_id"].as_i64(), r["content_hash"].as_str()],
            )
            .unwrap();
        }
    }
}

/// An embedding service answering every request with `vector`; counts the requests.
async fn embedding_stub(vector: Vec<f64>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let (vector, count) = (vector.clone(), Arc::clone(&count));
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if body.len() >= len {
                            break;
                        }
                    }
                }
                count.fetch_add(1, Ordering::SeqCst);
                let reply = json!({ "data": [{ "index": 0, "embedding": vector }] }).to_string();
                let out = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = s.write_all(out.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

struct Rig {
    dir: tempfile::TempDir,
    app: Arc<App>,
    embeds: Arc<AtomicUsize>,
    /// Node's recorded hits: `[{file, body, score}]`.
    want: Vec<Value>,
    query: String,
}

/// The account (`disabled`: its `disabled_at` set), the Node-written index, the embedding
/// configuration naming the stub, and an `App` with `NOEVIA_RUST_RETRIEVAL=mode`.
async fn rig(mode: &str, disabled: bool) -> Rig {
    let r = recorded();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let db = Connection::open(server_store::db_path(&data)).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    db.execute(
        "INSERT INTO users (id, username, username_norm, display_name, role, password_hash, webauthn_user_id, disabled_at, created_at, updated_at) VALUES (?1, 'qa-user', 'qa-user', 'QA user', 'member', 'qa-hash', 'qa-webauthn', ?2, 1, 1)",
        rusqlite::params![USER, disabled.then_some(1_i64)],
    )
    .unwrap();
    drop(db);
    rebuild(
        &data
            .join("users")
            .join(USER)
            .join("rag")
            .join(format!("{PROJECT}.db")),
        &r["index"],
    );
    let vector: Vec<f64> = r["search"]["query_vector"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap())
        .collect();
    let (base, embeds) = embedding_stub(vector).await;
    // The recorded target (its profile and model), at the stub's address.
    let target = &r["search"]["target"];
    std::fs::write(
        data.join("embedding-config.json"),
        json!({"version": 1, "active": {"model": target["model"], "profile": target["profile"], "baseUrl": base}, "next": null, "previous": null}).to_string(),
    )
    .unwrap();
    let env: HashMap<String, String> = [
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9"),
        ("UI_DATA_DIR", data.to_str().unwrap()),
        ("PUBLIC_ORIGIN", "http://127.0.0.1:1"),
        ("NOEVIA_PROJECT_STORE_PORT", "19022"),
        ("NOEVIA_PROJECT_STORE_TOKEN", TOKEN),
        ("NOEVIA_START_ID", START),
        ("NOEVIA_RUST_RETRIEVAL", mode),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    Rig {
        dir,
        app,
        embeds,
        want: r["search"]["hits"].as_array().unwrap().clone(),
        query: r["search"]["query"].as_str().unwrap().to_owned(),
    }
}

/// The recorded hits' files, each with a text over `DIRECT_INJECT_MAX` (so `filesContext` searches).
fn large_files(want: &[Value]) -> Vec<Source> {
    let mut names: Vec<&str> = Vec::new();
    for h in want {
        let n = h["file"].as_str().unwrap();
        if !names.contains(&n) {
            names.push(n);
        }
    }
    names
        .into_iter()
        .map(|n| {
            Source::plain(JValue::obj([
                ("name", JValue::Str(n.to_owned())),
                ("content", JValue::Str("qa filler line. ".repeat(200))),
            ]))
        })
        .collect()
}

fn request<'a>(project: &'a JValue, files: Vec<Source>, query: &'a str) -> Request<'a> {
    Request {
        project_id: project,
        readable: (0..files.len()).collect(),
        files,
        query,
        user: USER,
        attached: false,
        images: false,
    }
}

fn frame(kind: &str, label: &str, text: &str) -> String {
    let u = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    String::from_utf16(&prompt_framing::framing::frame_untrusted(
        &u(kind),
        &u(label),
        &u(text),
    ))
    .unwrap()
}

fn answered(native: Native) -> (String, Value, Value) {
    let v = |j: &JValue| serde_json::from_str::<Value>(&js_json::stringify(j).unwrap()).unwrap();
    match native {
        Native::Answer(Answer::Answered {
            block: Some(block),
            reported: Some((placed, injected)),
        }) => (block, v(&placed), v(&injected)),
        other => panic!("not answered: {other:?}"),
    }
}

#[tokio::test]
async fn on_places_nodes_recorded_hits_as_excerpts() {
    let r = rig("on", false).await;
    let project = JValue::Str(PROJECT.into());
    let req = request(&project, large_files(&r.want), &r.query);
    let (block, placed, injected) = answered(answer(Arc::clone(&r.app), &req).await);
    assert_eq!(r.embeds.load(Ordering::SeqCst), 1, "one query embed");
    let expected: Vec<Value> = r
        .want
        .iter()
        .map(|h| json!({ "file": h["file"], "body": h["body"], "score": h["score"], "kind": "excerpt" }))
        .collect();
    assert!(!expected.is_empty());
    assert_eq!(placed, Value::Array(expected.clone()));
    assert_eq!(injected, Value::Array(expected));
    for h in &r.want {
        let f = frame(
            "excerpt",
            h["file"].as_str().unwrap(),
            h["body"].as_str().unwrap(),
        );
        assert!(block.contains(&f), "missing excerpt of {}", h["file"]);
    }
    // Excerpts placed: no large file's head.
    assert!(!block.contains("qa filler line."));
}

#[tokio::test]
async fn shadow_answers_too_and_off_defers() {
    let r = rig("shadow", false).await;
    let project = JValue::Str(PROJECT.into());
    let req = request(&project, large_files(&r.want), &r.query);
    let (_, placed, _) = answered(answer(Arc::clone(&r.app), &req).await);
    assert_eq!(placed.as_array().unwrap().len(), r.want.len());

    let off = rig("off", false).await;
    let req = request(&project, large_files(&off.want), &off.query);
    assert_eq!(
        answer(Arc::clone(&off.app), &req).await,
        Native::Defer("off")
    );
    assert_eq!(off.embeds.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_tombstoned_project_or_a_disabled_account_gets_the_heads_without_an_embed() {
    let project = JValue::Str(PROJECT.into());
    let r = rig("on", false).await;
    let user_dir = r.dir.path().join("users").join(USER);
    server_projects::outbox::tombstone(&user_dir, PROJECT, None, 1).unwrap();
    let req = request(&project, large_files(&r.want), &r.query);
    let (block, placed, injected) = answered(answer(Arc::clone(&r.app), &req).await);
    assert_eq!(r.embeds.load(Ordering::SeqCst), 0);
    assert!(block.contains("qa filler line."));
    // Heads are injected; the question names no file, so none is a reply source.
    assert_eq!(placed, json!([]));
    assert!(injected
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["kind"] == "file"));

    let d = rig("on", true).await;
    let req = request(&project, large_files(&d.want), &d.query);
    let (block, _, _) = answered(answer(Arc::clone(&d.app), &req).await);
    assert_eq!(d.embeds.load(Ordering::SeqCst), 0);
    assert!(block.contains("qa filler line."));
}

#[tokio::test]
async fn small_files_only_never_embed() {
    let r = rig("on", false).await;
    let project = JValue::Str(PROJECT.into());
    let files = vec![Source::plain(JValue::obj([
        ("name", JValue::Str("qa-small.md".into())),
        ("content", JValue::Str("a short note".into())),
    ]))];
    let req = request(&project, files, "what is in qa-small.md?");
    let (block, placed, _) = answered(answer(Arc::clone(&r.app), &req).await);
    assert_eq!(r.embeds.load(Ordering::SeqCst), 0);
    assert!(block.contains(&frame("file", "qa-small.md", "a short note")));
    assert_eq!(
        placed,
        json!([{ "file": "qa-small.md", "body": "a short note", "kind": "file" }])
    );
}
