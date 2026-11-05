//! The chat mirror and brain schedule (capability `rust-chat-mirror-v1`, src/chat_mirror.rs) end to
//! end: the real internal store writes transcripts and their journal, the consumer reads it in
//! process, and the work executors reach a stub of Node's two private endpoints over the loopback
//! bridge with the per-start secret. Synthetic, QA-named data only.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::body::Bytes;
use axum::http::{HeaderMap, Uri};
use axum::routing::any;
use axum::Router;
use noevia_server::chat_mirror::{ChatMirror, Options};
use noevia_server::project_store::{serve_with_context, Store, TOKEN_HEADER};
use noevia_server::research_routes::{Research, Settings};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const USER: &str = "11111111-2222-4333-8444-555555555555";
const TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

struct Node {
    addr: SocketAddr,
    calls: Calls,
    fail_syncs: Arc<AtomicUsize>,
    brain_reason: Arc<Mutex<String>>,
}

impl Node {
    fn syncs(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == "/internal/chat-mirror/sync")
            .map(|(_, b)| b.clone())
            .collect()
    }
    fn brains(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == "/internal/chat-mirror/brain")
            .map(|(_, b)| b.clone())
            .collect()
    }
}

/// A stub of Node's two private endpoints, behind the per-start secret.
async fn node() -> Node {
    let calls: Calls = Arc::default();
    let fail_syncs = Arc::new(AtomicUsize::new(0));
    let brain_reason = Arc::new(Mutex::new("built".to_string()));
    let (c, f, b) = (calls.clone(), fail_syncs.clone(), brain_reason.clone());
    let app = Router::new().fallback(any(move |uri: Uri, headers: HeaderMap, body: Bytes| {
        let (c, f, b) = (c.clone(), f.clone(), b.clone());
        async move {
            let ok = headers
                .get(TOKEN_HEADER)
                .is_some_and(|v| v.as_bytes() == TOKEN.as_bytes());
            if !ok {
                return (axum::http::StatusCode::NOT_FOUND, "{}".to_string());
            }
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            c.lock().unwrap().push((uri.path().to_string(), body));
            match uri.path() {
                "/internal/chat-mirror/sync" => {
                    if f.load(Ordering::SeqCst) > 0 {
                        f.fetch_sub(1, Ordering::SeqCst);
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            "{}".to_string(),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        json!({"outcome": "done"}).to_string(),
                    )
                }
                "/internal/chat-mirror/brain" => (
                    axum::http::StatusCode::OK,
                    json!({"reason": b.lock().unwrap().clone()}).to_string(),
                ),
                _ => (axum::http::StatusCode::NOT_FOUND, "{}".to_string()),
            }
        }
    }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Node {
        addr,
        calls,
        fail_syncs,
        brain_reason,
    }
}

#[test]
fn the_capability_and_the_scheduler_marker_are_compiled_in() {
    assert!(noevia_server::FEATURES.contains(&"rust-chat-mirror-v1"));
    assert_eq!(chat_mirror_sched::MARKER, "noevia-chat-mirror-scheduler/1");
}

fn quick() -> Options {
    Options {
        mirror: chat_mirror_sched::mirror::Timings {
            delay: Duration::from_millis(30),
            max_wait: Duration::from_millis(150),
            retry: Duration::from_millis(60),
            max_retry: Duration::from_millis(240),
        },
        brain: chat_mirror_sched::brain::Timings {
            idle: Duration::from_millis(150),
            retry: Duration::from_millis(300),
        },
        interval: Duration::from_millis(25),
        active: Duration::from_secs(60),
    }
}

fn research(node: &Node) -> Arc<Research> {
    Arc::new(
        Research::new(Settings {
            upstream: format!("http://{}", node.addr),
            token: Some(TOKEN.to_string()),
            ..Settings::default()
        })
        .unwrap(),
    )
}

async fn store(root: &Path) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Store {
        data_dir: root.to_path_buf(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: START.into(),
    };
    tokio::spawn(serve_with_context(
        listener,
        store,
        None,
        None,
        std::future::pending(),
    ));
    addr
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

async fn save(addr: SocketAddr, chat: &str, history: Value) {
    let body =
        json!({"user": USER, "chat": chat, "history": history, "origin": "history"}).to_string();
    let (status, _) = call(addr, "/v1/history/write", Some(TOKEN), &body).await;
    assert_eq!(status, 200);
}

async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn data_dir() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("users").join(USER)).unwrap();
    root
}

fn chats_of(call: &Value) -> Vec<String> {
    let mut v: Vec<String> = call["chats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn a_history_save_reaches_the_mirror_and_after_its_idle_time_the_brain_then_the_mirror_again()
{
    let root = data_dir();
    let store_addr = store(root.path()).await;
    let node = node().await;
    let mirror =
        ChatMirror::start_with(research(&node), Some(root.path().to_path_buf()), quick()).unwrap();

    save(
        store_addr,
        "qa-c1",
        json!([{"role": "user", "content": "hi"}]),
    )
    .await;
    // The first read of a user is a rescan: the mirror only, one pass over the chat, no brain.
    mirror.touch(USER);
    until("the rescan's mirror pass", || !node.syncs().is_empty()).await;
    assert_eq!(chats_of(&node.syncs()[0]), vec!["qa-c1"]);
    assert_eq!(node.syncs()[0]["user"], USER);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        node.brains().is_empty(),
        "no brain build on restart (owner decision)"
    );
    let before = node.syncs().len();

    // A history save after that: the mirror within the debounce, the brain after the idle time.
    save(
        store_addr,
        "qa-c1",
        json!([{"role": "user", "content": "hi"}, {"role": "assistant", "content": "hello"}]),
    )
    .await;
    until("the save's mirror pass", || node.syncs().len() > before).await;
    assert_eq!(chats_of(&node.syncs()[before]), vec!["qa-c1"]);
    until("the brain build", || !node.brains().is_empty()).await;
    assert_eq!(node.brains()[0], json!({"user": USER, "chatId": "qa-c1"}));
    // The brain reported built: the mirror is nudged for that chat once more.
    let after_save = node.syncs().len();
    until("the mirror pass after the brain", || {
        node.syncs().len() > after_save
    })
    .await;
}

#[tokio::test]
async fn a_failed_pass_keeps_its_chats_and_is_retried_on_the_backoff() {
    let root = data_dir();
    let store_addr = store(root.path()).await;
    let node = node().await;
    *node.brain_reason.lock().unwrap() = "current".into();
    let mirror =
        ChatMirror::start_with(research(&node), Some(root.path().to_path_buf()), quick()).unwrap();
    mirror.touch(USER);
    until("the rescan pass", || !node.syncs().is_empty()).await;
    node.fail_syncs.store(2, Ordering::SeqCst);
    let before = node.syncs().len();
    save(
        store_addr,
        "qa-c2",
        json!([{"role": "user", "content": "x"}]),
    )
    .await;
    until("two failures then a success", || {
        node.syncs().len() >= before + 3
    })
    .await;
    let tail: Vec<Vec<String>> = node.syncs()[before..before + 3]
        .iter()
        .map(chats_of)
        .collect();
    assert_eq!(
        tail,
        vec![vec!["qa-c2"], vec!["qa-c2"], vec!["qa-c2"]],
        "the dirty chat travels with every retry"
    );
}

#[tokio::test]
async fn an_unreachable_node_is_a_failed_pass_never_a_success() {
    let root = data_dir();
    let store_addr = store(root.path()).await;
    // A bridge to a port nothing listens on.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = dead.local_addr().unwrap();
    drop(dead);
    let stub = Node {
        addr,
        calls: Arc::default(),
        fail_syncs: Arc::default(),
        brain_reason: Arc::default(),
    };
    let mirror =
        ChatMirror::start_with(research(&stub), Some(root.path().to_path_buf()), quick()).unwrap();
    save(
        store_addr,
        "qa-c3",
        json!([{"role": "user", "content": "x"}]),
    )
    .await;
    mirror.touch(USER);
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Now Node comes up on the same address: the retained dirty chat is delivered.
    let listener = TcpListener::bind(addr).await.unwrap();
    let calls: Calls = Arc::default();
    let c = calls.clone();
    let app = Router::new().fallback(any(move |uri: Uri, body: Bytes| {
        let c = c.clone();
        async move {
            c.lock().unwrap().push((
                uri.path().to_string(),
                serde_json::from_slice(&body).unwrap_or(Value::Null),
            ));
            (
                axum::http::StatusCode::OK,
                json!({"outcome": "done"}).to_string(),
            )
        }
    }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    until("delivery after Node came up", || {
        !calls.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(chats_of(&calls.lock().unwrap()[0].1), vec!["qa-c3"]);
}

#[tokio::test]
async fn nodes_triggers_reach_the_schedule_over_the_store_behind_the_secret() {
    let root = data_dir();
    let node = node().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // The same synthetic database the other front tests build (the App opens it for sign-in).
    let db = server_store::rusqlite::Connection::open(server_store::db_path(root.path())).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    drop(db);
    let vars = std::collections::HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", root.path().display().to_string()),
        ("PUBLIC_ORIGIN", "https://mirror.example.test".to_string()),
        ("WEBAUTHN_RP_ID", "example.test".to_string()),
        ("NOEVIA_START_ID", START.to_string()),
    ]);
    let config = noevia_server::config::Config::from_lookup(|k| vars.get(k).cloned()).unwrap();
    let app = noevia_server::App::new(config);
    let store = Store {
        data_dir: root.path().to_path_buf(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: START.into(),
    };
    tokio::spawn(serve_with_context(
        listener,
        store,
        None,
        Some(app.clone()),
        std::future::pending(),
    ));

    let schedule = |body: &str, token: Option<&'static str>| {
        let body = body.to_string();
        async move { call(addr, "/v1/chat-mirror/schedule", token, &body).await.0 }
    };
    assert_eq!(schedule(r#"{"user":"u"}"#, None).await, 401);
    assert_eq!(schedule(r#"{"user":"u"}"#, Some("wrong")).await, 401);
    assert_eq!(
        schedule(r#"{"user":"u"}"#, Some(TOKEN)).await,
        503,
        "not running yet: refuse, never pretend"
    );

    let mirror =
        ChatMirror::start_with(research(&node), Some(root.path().to_path_buf()), quick()).unwrap();
    assert!(app.chat_mirror.set(mirror).is_ok());
    assert_eq!(schedule("not json", Some(TOKEN)).await, 400);
    assert_eq!(schedule(r#"{"chatId":"c"}"#, Some(TOKEN)).await, 400);
    assert_eq!(schedule(r#"{"user":""}"#, Some(TOKEN)).await, 400);
    assert_eq!(
        schedule(r#"{"user":"u","chatId":5}"#, Some(TOKEN)).await,
        400
    );
    assert_eq!(node.syncs().len(), 0, "a refused call schedules nothing");

    assert_eq!(
        schedule(
            r#"{"user":"u","chatId":"qa-x","immediate":true}"#,
            Some(TOKEN)
        )
        .await,
        200
    );
    until("an immediate pass", || !node.syncs().is_empty()).await;
    assert_eq!(node.syncs()[0], json!({"user": "u", "chats": ["qa-x"]}));
    assert_eq!(
        schedule(
            r#"{"user":"u","chatId":null,"immediate":true}"#,
            Some(TOKEN)
        )
        .await,
        200
    );
    until("an all-chats pass", || node.syncs().len() == 2).await;
    assert_eq!(node.syncs()[1], json!({"user": "u", "chats": []}));
    let (status, _) = call(
        addr,
        "/v1/chat-mirror/touch",
        Some(TOKEN),
        r#"{"user":"not-a-user-id"}"#,
    )
    .await;
    assert_eq!(
        status, 200,
        "a touch of a non-account id is taken and reads nothing"
    );
}
