//! sbstndalton/noevia#1264: the internal project store (src/project_store.rs) over real HTTP:
//! the secret, the compare-and-swap, Node's error shapes, the body limit, and many concurrent
//! read-merge-write clients plus native writes with no lost update. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use noevia_server::project_store::{serve, Store, TOKEN_HEADER};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const USER: &str = "11111111-2222-4333-8444-555555555555";
const TOKEN: &str = "synthetic-store-token-0123456789abcdef";

async fn start() -> (tempfile::TempDir, SocketAddr) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("users").join(USER)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Store {
        data_dir: root.path().to_path_buf(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::from_env_value(Some("1")).unwrap(),
    };
    tokio::spawn(serve(listener, store, std::future::pending()));
    (root, addr)
}

async fn call(addr: SocketAddr, path: &str, token: Option<&str>, body: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
        body.len()
    );
    if let Some(t) = token {
        req.push_str(&format!("{TOKEN_HEADER}: {t}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    s.write_all(body.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

async fn read(addr: SocketAddr, file: &str) -> (u16, Value) {
    call(
        addr,
        "/v1/read",
        Some(TOKEN),
        &json!({"user": USER, "file": file}).to_string(),
    )
    .await
}

async fn write(addr: SocketAddr, file: &str, version: &str, text: &str) -> (u16, Value) {
    let body = json!({"user": USER, "file": file, "version": version, "text": text}).to_string();
    call(addr, "/v1/write", Some(TOKEN), &body).await
}

#[tokio::test]
async fn the_secret_is_required_and_nothing_is_touched_without_it() {
    let (root, addr) = start().await;
    let body =
        json!({"user": USER, "file": "projects.json", "version": "", "text": "{\"projects\":[]}"})
            .to_string();
    for token in [None, Some("wrong"), Some(&TOKEN[..TOKEN.len() - 1])] {
        let (status, _) = call(addr, "/v1/write", token, &body).await;
        assert_eq!(status, 401, "{token:?}");
    }
    assert!(!root
        .path()
        .join("users")
        .join(USER)
        .join("projects.json")
        .exists());
}

#[tokio::test]
async fn compare_and_swap_and_nodes_error_shapes() {
    let (root, addr) = start().await;
    assert_eq!(
        read(addr, "projects.json").await,
        (200, json!({"version": "", "text": null}))
    );
    let (status, v1) = write(addr, "projects.json", "", "{\n  \"projects\": []\n}").await;
    assert_eq!(status, 200);
    let v1 = v1["version"].as_str().unwrap().to_string();
    let (status, now) = write(addr, "projects.json", "", "{\"projects\":[1]}").await;
    assert_eq!(status, 409);
    assert_eq!(
        now,
        json!({"version": v1, "text": "{\n  \"projects\": []\n}"})
    );
    assert_eq!(write(addr, "projects.json", &v1, "[]").await.0, 400);
    assert_eq!(write(addr, "cowork.db", "", "[]").await.0, 400);
    assert_eq!(write(addr, "../projects.json", "", "[]").await.0, 400);
    // The lock held past its wait: Node's PROJECTS_BUSY.
    let dir = root.path().join("users").join(USER);
    let held = server_projects::lock::acquire(&dir.join("projects.json")).unwrap();
    let (status, body) = write(addr, "projects.json", &v1, "{\"projects\":[]}").await;
    drop(held);
    assert_eq!(
        (status, body["code"].as_str()),
        (503, Some("PROJECTS_BUSY"))
    );
    // A removed account: 410, and its directory is not recreated.
    std::fs::remove_dir_all(&dir).unwrap();
    let (status, body) = write(addr, "free-chats.json", "", "[]").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (410, Some("account no longer exists"))
    );
    assert!(!dir.exists());
}

#[tokio::test]
async fn a_body_over_the_limit_is_refused() {
    let (_root, addr) = start().await;
    let text = format!(
        "[\"{}\"]",
        "x".repeat(noevia_server::project_store::BODY_LIMIT)
    );
    let (status, _) = write(addr, "free-chats.json", "", &text).await;
    assert_eq!(status, 413);
}

/// N clients doing Node's read-merge-CAS loop (bounded retries) while Rust's native free-chat
/// route writes the same file: every addition from both sides survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_clients_and_native_writes_lose_no_update() {
    let (root, addr) = start().await;
    assert_eq!(write(addr, "free-chats.json", "", "[]").await.0, 200);
    let mut tasks = Vec::new();
    for c in 0..6 {
        tasks.push(tokio::spawn(async move {
            for i in 0..8 {
                let mut done = false;
                for _ in 0..200 {
                    let (_, snap) = read(addr, "free-chats.json").await;
                    let mut list: Vec<Value> =
                        serde_json::from_str(snap["text"].as_str().unwrap()).unwrap();
                    list.push(json!({"id": format!("node-{c}-{i}"), "updatedAt": 1}));
                    let text = serde_json::to_string_pretty(&list).unwrap();
                    match write(
                        addr,
                        "free-chats.json",
                        snap["version"].as_str().unwrap(),
                        &text,
                    )
                    .await
                    .0
                    {
                        200 => {
                            done = true;
                            break;
                        }
                        409 => continue,
                        other => panic!("{other}"),
                    }
                }
                assert!(done, "node-{c}-{i} gave up");
            }
        }));
    }
    let data = root.path().to_path_buf();
    let native = tokio::task::spawn_blocking(move || {
        for i in 0..20 {
            let ctx = server_projects::Ctx {
                data_dir: &data,
                user_id: USER,
                switch: server_store::RustProjects::from_env_value(Some("1")).unwrap(),
                now_ms: 5,
            };
            let body = format!("{{\"chats\":[{{\"id\":\"rust-{i}\",\"updatedAt\":2}}]}}");
            assert_eq!(
                server_projects::free_chats::post(ctx, body.as_bytes(), false, false).status,
                200
            );
        }
    });
    for t in tasks {
        t.await.unwrap();
    }
    native.await.unwrap();
    let (_, snap) = read(addr, "free-chats.json").await;
    let list: Vec<Value> = serde_json::from_str(snap["text"].as_str().unwrap()).unwrap();
    let ids: std::collections::HashSet<_> = list
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 6 * 8 + 20, "{ids:?}");
}
