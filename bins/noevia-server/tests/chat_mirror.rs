//! The chat mirror and brain schedule (capability `rust-chat-mirror-v1`, src/chat_mirror.rs) end to
//! end: the real internal store writes transcripts and their journal, the consumer reads it in
//! process, and the schedule hands its work to a recording executor (the native executors are
//! tested in chat_mirror_work.rs and crates/chat-mirror-exec). Synthetic, QA-named data only.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_mirror_sched::{BoxFut, BrainExecutor, MirrorExecutor, MirrorOutcome};
use noevia_server::chat_mirror::{ChatMirror, Options};
use noevia_server::project_store::{serve_with_context, Store, TOKEN_HEADER};
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

/// Records what the schedule asks of its executors, in the shape of the old bridge calls.
#[derive(Clone)]
struct Node {
    calls: Calls,
    fail_syncs: Arc<AtomicUsize>,
    brain_reason: Arc<Mutex<String>>,
    down: Arc<std::sync::atomic::AtomicBool>,
}

impl Node {
    fn syncs(&self) -> Vec<Value> {
        self.of("sync")
    }
    fn brains(&self) -> Vec<Value> {
        self.of("brain")
    }
    fn of(&self, kind: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == kind)
            .map(|(_, b)| b.clone())
            .collect()
    }
}

impl MirrorExecutor for Node {
    fn sync(&self, user: &str, dirty: Vec<String>) -> BoxFut<MirrorOutcome> {
        let me = self.clone();
        let body = json!({"user": user, "chats": dirty});
        Box::pin(async move {
            if me.down.load(Ordering::SeqCst) {
                return MirrorOutcome::Failed;
            }
            me.calls.lock().unwrap().push(("sync".into(), body));
            if me.fail_syncs.load(Ordering::SeqCst) > 0 {
                me.fail_syncs.fetch_sub(1, Ordering::SeqCst);
                return MirrorOutcome::Failed;
            }
            MirrorOutcome::Done
        })
    }
}

impl BrainExecutor for Node {
    fn run(&self, user: &str, chat: &str) -> BoxFut<String> {
        let me = self.clone();
        let body = json!({"user": user, "chatId": chat});
        Box::pin(async move {
            me.calls.lock().unwrap().push(("brain".into(), body));
            let reason = me.brain_reason.lock().unwrap().clone();
            reason
        })
    }
}

async fn node() -> Node {
    Node {
        calls: Arc::default(),
        fail_syncs: Arc::new(AtomicUsize::new(0)),
        brain_reason: Arc::new(Mutex::new("built".to_string())),
        down: Arc::default(),
    }
}

#[test]
fn the_capability_and_the_scheduler_marker_are_compiled_in() {
    assert!(noevia_server::FEATURES.contains(&"rust-chat-mirror-v1"));
    assert!(noevia_server::FEATURES.contains(&"rust-chat-mirror-exec-v1"));
    assert_eq!(chat_mirror_exec::MARKER, "noevia-chat-mirror-exec/1");
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

fn research(node: &Node) -> Arc<Node> {
    Arc::new(node.clone())
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
async fn a_failing_executor_is_a_failed_pass_never_a_success() {
    let root = data_dir();
    let store_addr = store(root.path()).await;
    let stub = node().await;
    stub.down.store(true, Ordering::SeqCst);
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
    assert!(stub.syncs().is_empty());
    // The work succeeds again: the retained dirty chat is delivered.
    stub.down.store(false, Ordering::SeqCst);
    until("delivery after recovery", || !stub.syncs().is_empty()).await;
    assert_eq!(chats_of(&stub.syncs()[0]), vec!["qa-c3"]);
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
