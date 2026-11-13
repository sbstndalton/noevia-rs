//! Rust port track R4: `POST /v1/retrieval/search` on the project-store listener, which core
//! `rag.cjs` `searchProject` asks through `rust-retrieval.cjs` when `NOEVIA_RUST_RETRIEVAL` says so.
//! The index is one Node wrote (crates/project-retrieval F2-F6, recorded search S1 "ties between the
//! two duplicate files"); a plain TCP stub is the embedding service and answers the query vector
//! Node bound. Synthetic QA data only; no model.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_server::project_store::{self, Store, TOKEN_HEADER};
use noevia_server::{config::Config, App};
use serde_json::{json, Value};
use server_store::rusqlite::{self, Connection};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const FIXTURE: &str =
    include_str!("../../../crates/project-retrieval/tests/fixtures/F2-F6-search.json");
const TOKEN: &str = "synthetic-retrieval-store-secret-00000";
const START: &str = "0123456789abcdef0123456789abcdef";
const USER: &str = "0123abcd-0000-4000-8000-0000000000a2";

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

/// Rebuilds a Node-written index from its dump.
fn rebuild(path: &Path, dump: &Value) {
    embedding_worker::sqlite_vec_loader::register().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let c = Connection::open(path).unwrap();
    for sql in dump["schema"].as_array().unwrap() {
        c.execute_batch(sql.as_str().unwrap()).unwrap();
    }
    for r in dump["chunks"].as_array().unwrap() {
        c.execute(
            "INSERT INTO chunks (id, file, chunk_no, body, content_hash, version, direct) VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![r["id"].as_i64(), r["file"].as_str(), r["chunk_no"].as_i64(), r["body"].as_str(), r["content_hash"].as_str(), r["version"].as_str(), r["direct"].as_i64()],
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

/// An embedding service answering every `POST /v1/embeddings` with `vector`; counts the requests.
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
    _dir: tempfile::TempDir,
    router: axum::Router,
    embeds: Arc<AtomicUsize>,
    want: Vec<Value>,
}

async fn rig(mode: &str, extra: &[(&str, &str)]) -> Rig {
    let r = recorded();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    Connection::open(server_store::db_path(&data))
        .unwrap()
        .execute_batch(SCHEMA)
        .unwrap();
    rebuild(
        &data
            .join("users")
            .join(USER)
            .join("rag")
            .join("qa-proj-1.db"),
        &r["index"],
    );
    let vector: Vec<f64> = r["search"]["query_vector"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap())
        .collect();
    let (base, embeds) = embedding_stub(vector).await;
    std::fs::write(
        data.join("embedding-config.json"),
        json!({"version": 1, "active": {"model": "qa-embed-map", "profile": "embeddinggemma-2", "baseUrl": base}, "next": null, "previous": null}).to_string(),
    )
    .unwrap();
    let mut env: HashMap<String, String> = [
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9"),
        ("UI_DATA_DIR", data.to_str().unwrap()),
        ("PUBLIC_ORIGIN", "http://127.0.0.1:1"),
        ("NOEVIA_PROJECT_STORE_PORT", "19021"),
        ("NOEVIA_PROJECT_STORE_TOKEN", TOKEN),
        ("NOEVIA_START_ID", START),
        ("NOEVIA_RUST_RETRIEVAL", mode),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    for (k, v) in extra {
        env.insert((*k).to_owned(), (*v).to_owned());
    }
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let store = Store {
        data_dir: data.clone(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: START.into(),
    };
    Rig {
        _dir: dir,
        router: project_store::router_with_app(store, None, Some(app)),
        embeds,
        want: r["search"]["hits"].as_array().unwrap().clone(),
    }
}

async fn ask(r: &Rig, token: Option<&str>, body: &str) -> (u16, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/retrieval/search")
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header(TOKEN_HEADER, t);
    }
    let res = r
        .router
        .clone()
        .oneshot(req.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn query(versions: Value) -> String {
    json!({ "user": USER, "projectId": "qa-proj-1", "query": "echo echo echo", "versions": versions })
        .to_string()
}

#[tokio::test]
async fn answers_nodes_hits_behind_the_secret() {
    assert!(noevia_server::FEATURES.contains(&"rust-retrieval-v1"));
    let r = rig("on", &[]).await;
    assert_eq!(ask(&r, None, &query(json!({}))).await.0, 401);
    assert_eq!(ask(&r, Some("wrong"), &query(json!({}))).await.0, 401);
    let (status, body) = ask(&r, Some(TOKEN), &query(json!({}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["hits"].as_array().unwrap(),
        &r.want,
        "Node's six hits, file, body and score"
    );
    assert_eq!(r.embeds.load(Ordering::SeqCst), 1);
    // Node's wanted versions filter as Node's do: the first file is being re-indexed, the second deleted.
    let (_, body) = ask(
        &r,
        Some(TOKEN),
        &query(json!({ "qa-dup-1.md": "qa-new-version", "qa-dup-2.md": null })),
    )
    .await;
    let files: Vec<&str> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["file"].as_str().unwrap())
        .collect();
    assert!(
        !files.is_empty() && files.iter().all(|f| *f == "qa-long-a.md"),
        "the other files' chunks above the floor remain: {body}"
    );
    let (_, body) = ask(
        &r,
        Some(TOKEN),
        &query(json!({ "qa-dup-1.md": "qa-new-version" })),
    )
    .await;
    let files: Vec<&str> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["file"].as_str().unwrap())
        .collect();
    assert!(
        files.len() == 6 && files[..3] == ["qa-dup-2.md"; 3] && !files.contains(&"qa-dup-1.md"),
        "{body}"
    );
}

#[tokio::test]
async fn refuses_bad_bodies_and_defers_what_is_nodes() {
    let r = rig("on", &[]).await;
    for bad in [
        "{",
        r#"{"user":null}"#,
        r#"{"projectId":"qa-proj-1"}"#,
        r#"{"projectId":"qa-proj-1","query":"q","versions":[]}"#,
        r#"{"projectId":"qa-proj-1","query":"q","versions":{"a":1}}"#,
    ] {
        assert_eq!(ask(&r, Some(TOKEN), bad).await.0, 400, "{bad}");
    }
    // The shared index (no account) is Node's.
    let (status, body) = ask(
        &r,
        Some(TOKEN),
        r#"{"user":null,"projectId":"qa-proj-1","query":"q","versions":{}}"#,
    )
    .await;
    assert_eq!((status, body), (200, json!({ "defer": "shared index" })));
    // No index for this project: no hits, nothing embedded.
    let (_, body) = ask(
        &r,
        Some(TOKEN),
        &json!({ "user": USER, "projectId": "qa-proj-none", "query": "q", "versions": {} })
            .to_string(),
    )
    .await;
    assert_eq!(body, json!({ "hits": [] }));
    assert_eq!(r.embeds.load(Ordering::SeqCst), 0);
    // The reranker on: Node's search, which reranks.
    let rr = rig(
        "on",
        &[
            ("NOEVIA_FEATURE_RAG_RERANK", "1"),
            ("RERANK_BASE_URL", "http://qa-rerank:8080"),
        ],
    )
    .await;
    assert_eq!(
        ask(&rr, Some(TOKEN), &query(json!({}))).await.1,
        json!({ "defer": "reranker on" })
    );
    assert_eq!(rr.embeds.load(Ordering::SeqCst), 0);
    // Switched off: every question is Node's.
    let off = rig("off", &[]).await;
    assert_eq!(
        ask(&off, Some(TOKEN), &query(json!({}))).await.1,
        json!({ "defer": "off" })
    );
    assert_eq!(off.embeds.load(Ordering::SeqCst), 0);
}
