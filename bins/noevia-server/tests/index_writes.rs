//! Council p6: `POST /v1/index/{file,tail,delete}` on the project-store listener, the native writer
//! of a project's index behind `NOEVIA_RUST_INDEX_WRITES` (native_index_writes.rs over the
//! `project-index` crate, whose own suites hold the Node parity fixtures). A plain TCP stub is the
//! embedding service. Synthetic QA data only; no model.
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
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const TOKEN: &str = "synthetic-index-writes-secret-000000000";
const START: &str = "0123456789abcdef0123456789abcdef";
const USER: &str = "0123abcd-0000-4000-8000-0000000000b4";
const PROJECT: &str = "qa-proj-index";
const DIM: usize = 4;

/// An embedding service answering every `POST /v1/embeddings` with one `DIM`-wide vector per input;
/// counts the requests.
async fn embedding_stub() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let count = Arc::clone(&count);
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
                let text = String::from_utf8_lossy(&buf).into_owned();
                let (_, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                count.fetch_add(1, Ordering::SeqCst);
                let inputs = serde_json::from_str::<Value>(body).unwrap()["input"]
                    .as_array()
                    .map_or(0, Vec::len);
                let data: Vec<Value> = (0..inputs)
                    .map(|i| {
                        let mut v = vec![0.0f64; DIM];
                        v[i % DIM] = 1.0;
                        json!({ "index": i, "embedding": v })
                    })
                    .collect();
                let reply = json!({ "data": data }).to_string();
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
    router: axum::Router,
    embeds: Arc<AtomicUsize>,
}

impl Rig {
    fn data(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }
    fn user_dir(&self) -> PathBuf {
        server_projects::store::user_dir(&self.data(), USER).unwrap()
    }
    fn live(&self) -> PathBuf {
        self.user_dir().join("rag").join(format!("{PROJECT}.db"))
    }
    fn shadow(&self) -> PathBuf {
        self.user_dir()
            .join(noevia_server::native_index_writes::SHADOW_DIR)
            .join(format!("{PROJECT}.db"))
    }
}

/// `base`: the active target's own address (`None`: the shared inference engine's).
async fn rig(mode: &str, disabled: bool, own_address: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let db = Connection::open(server_store::db_path(&data)).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    db.execute(
        "INSERT INTO users (id, username, username_norm, display_name, role, password_hash, webauthn_user_id, disabled_at, created_at, updated_at) VALUES (?1, 'qa-user', 'qa-user', 'QA User', 'member', 'x', 'qa-webauthn-b4', ?2, 1, 1)",
        server_store::rusqlite::params![USER, if disabled { Some(1i64) } else { None }],
    )
    .unwrap();
    let (base, embeds) = embedding_stub().await;
    let mut active = json!({"model": "qa-embed", "profile": project_index::slots::LEGACY_PROFILE});
    if own_address {
        active["baseUrl"] = json!(base);
    }
    std::fs::write(
        data.join("embedding-config.json"),
        json!({"version": 1, "active": active, "next": null, "previous": null}).to_string(),
    )
    .unwrap();
    let env: HashMap<String, String> = [
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9"),
        ("UI_DATA_DIR", data.to_str().unwrap()),
        ("PUBLIC_ORIGIN", "http://127.0.0.1:1"),
        ("NOEVIA_PROJECT_STORE_PORT", "19022"),
        ("NOEVIA_PROJECT_STORE_TOKEN", TOKEN),
        ("NOEVIA_START_ID", START),
        ("NOEVIA_RUST_INDEX_WRITES", mode),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let store = Store {
        data_dir: data.clone(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: START.into(),
    };
    Rig {
        dir,
        router: project_store::router_with_app(store, None, Some(app)),
        embeds,
    }
}

async fn post(r: &Rig, op: &str, token: &str, body: &Value) -> (u16, Value) {
    let res = r
        .router
        .clone()
        .oneshot(
            Request::post(format!("/v1/index/{op}"))
                .header(TOKEN_HEADER, token)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// A synthetic document of several chunk windows.
fn document() -> String {
    (0..60)
        .map(|i| format!("QA paragraph {i}: the synthetic index-writes fixture line number {i}."))
        .collect::<Vec<_>>()
        .join("\n")
}

fn file_body(name: &str, text: &str) -> Value {
    json!({ "projectId": PROJECT, "fileName": name, "text": text, "userId": USER })
}

fn chunks(path: &Path, file: &str) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM chunks WHERE file = ?1", [file], |r| {
            r.get(0)
        })
        .unwrap()
}

#[test]
fn the_store_advertises_the_index_writes() {
    assert!(noevia_server::FEATURES.contains(&"rust-index-writes-v1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_defers_every_write_and_writes_nothing() {
    for mode in ["", "off"] {
        let r = rig(mode, false, true).await;
        for (op, body) in [
            ("file", file_body("qa.md", &document())),
            (
                "tail",
                json!({"projectId": PROJECT, "fileName": "qa.md", "headText": "h", "tail": [], "userId": USER}),
            ),
            (
                "delete",
                json!({"projectId": PROJECT, "fileName": "qa.md", "userId": USER}),
            ),
        ] {
            let (status, answer) = post(&r, op, TOKEN, &body).await;
            assert_eq!((status, answer), (200, json!({ "defer": "off" })), "{op}");
        }
        assert!(!r.live().exists() && !r.shadow().exists());
        assert_eq!(r.embeds.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_secret_is_refused() {
    let r = rig("on", false, true).await;
    let (status, _) = post(&r, "file", "not-the-secret", &file_body("qa.md", "x")).await;
    assert_eq!(status, 401);
    assert!(!r.live().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_indexes_tails_and_deletes_the_live_index() {
    let r = rig("on", false, true).await;
    let text = document();
    let (status, out) = post(&r, "file", TOKEN, &file_body("qa.md", &text)).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["ok"], json!(true), "{out}");
    let stored = out["stored"].as_i64().unwrap();
    assert!(stored > 1, "a multi-window document: {out}");
    assert_eq!(out["embedded"].as_i64(), Some(stored), "{out}");
    assert_eq!(chunks(&r.live(), "qa.md"), stored);
    assert!(r.embeds.load(Ordering::SeqCst) >= 1);
    assert!(!r.shadow().exists(), "on never writes the shadow copy");

    // The same text again: the file's text rows are replaced, not doubled (rag.cjs re-embeds them).
    let (_, again) = post(&r, "file", TOKEN, &file_body("qa.md", &text)).await;
    assert_eq!(again["ok"], json!(true), "{again}");
    assert_eq!(chunks(&r.live(), "qa.md"), stored);

    // A short file is injected directly: one row, no embedding.
    let before = r.embeds.load(Ordering::SeqCst);
    let (_, short) = post(&r, "file", TOKEN, &file_body("qa-short.md", "QA short note.")).await;
    assert_eq!(
        short,
        json!({ "ok": true, "stored": 1, "embedded": 0, "direct": true })
    );
    assert_eq!(chunks(&r.live(), "qa-short.md"), 1);
    assert_eq!(r.embeds.load(Ordering::SeqCst), before);

    // A tail append answers an outcome (its parity is the crate's), never a deferral.
    let (_, tail) = post(
        &r,
        "tail",
        TOKEN,
        &json!({"projectId": PROJECT, "fileName": "qa.md", "headText": text,
                "tail": [{"body": "QA tail page two.", "page": 2}], "userId": USER}),
    )
    .await;
    assert!(tail.get("ok").is_some_and(Value::is_boolean), "{tail}");
    assert!(tail.get("defer").is_none(), "{tail}");

    let (_, deleted) = post(
        &r,
        "delete",
        TOKEN,
        &json!({"projectId": PROJECT, "fileName": "qa.md", "userId": USER}),
    )
    .await;
    assert_eq!(deleted, json!({ "ok": true }));
    assert_eq!(chunks(&r.live(), "qa.md"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shadow_writes_only_the_side_copy() {
    let r = rig("shadow", false, true).await;
    let (_, out) = post(&r, "file", TOKEN, &file_body("qa.md", &document())).await;
    assert_eq!(out["ok"], json!(true), "{out}");
    assert!(r.shadow().is_file());
    assert!(!r.live().exists(), "the live index is Node's while shadowing");
    assert_eq!(
        chunks(&r.shadow(), "qa.md"),
        out["stored"].as_i64().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tombstoned_project_or_a_disabled_account_gets_no_index() {
    let r = rig("on", false, true).await;
    std::fs::create_dir_all(r.user_dir()).unwrap();
    Connection::open(server_projects::outbox::path(&r.user_dir()))
        .unwrap()
        .execute_batch(&format!(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             CREATE TABLE tombstones (project TEXT PRIMARY KEY, generation INTEGER NOT NULL, at INTEGER NOT NULL);
             INSERT INTO meta VALUES ('generation', 2);
             INSERT INTO tombstones VALUES ('{PROJECT}', 2, 1);"
        ))
        .unwrap();
    // Node's run is superseded before it opens the index (`notDeleted`, `userActive`).
    let (_, out) = post(&r, "file", TOKEN, &file_body("qa.md", &document())).await;
    assert_eq!(out["superseded"], json!(true), "{out}");
    assert!(!r.live().exists());
    assert_eq!(r.embeds.load(Ordering::SeqCst), 0);

    let r = rig("on", true, true).await;
    let (_, out) = post(&r, "file", TOKEN, &file_body("qa.md", &document())).await;
    assert_eq!(out["superseded"], json!(true), "{out}");
    assert!(!r.live().exists());
    assert_eq!(r.embeds.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_keeps_the_shared_index_and_the_shared_engine() {
    let r = rig("on", false, true).await;
    for user in [json!(null), json!("")] {
        let body = json!({ "projectId": PROJECT, "fileName": "qa.md", "text": "x", "userId": user });
        let (_, out) = post(&r, "file", TOKEN, &body).await;
        assert_eq!(out, json!({ "defer": "shared index" }));
    }
    let r = rig("on", false, false).await;
    let (_, out) = post(&r, "file", TOKEN, &file_body("qa.md", "x")).await;
    assert_eq!(out, json!({ "defer": "shared engine target" }));
    assert!(!r.live().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_writes_are_refused_with_nothing_written() {
    let r = rig("on", false, true).await;
    for body in [
        json!({ "projectId": "../escape", "fileName": "qa.md", "text": "x", "userId": USER }),
        json!({ "projectId": PROJECT, "fileName": "qa.md", "text": 7, "userId": USER }),
        json!({ "projectId": PROJECT, "fileName": "qa.md", "text": "x", "userId": 7 }),
    ] {
        let (status, out) = post(&r, "file", TOKEN, &body).await;
        assert_eq!(
            (status, out),
            (200, json!({ "ok": false, "reason": "bad-request" })),
            "{body}"
        );
    }
    let (status, _) = r
        .router
        .clone()
        .oneshot(
            Request::post("/v1/index/file")
                .header(TOKEN_HEADER, TOKEN)
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .map(|res| (res.status().as_u16(), ()))
        .unwrap();
    assert_eq!(status, 400);
    assert!(!r.live().exists());
    assert_eq!(r.embeds.load(Ordering::SeqCst), 0);
}
