//! sbstndalton/noevia#2153 (#1071 slice 5a): the chat change journal over the internal project
//! store (src/project_store.rs `/v1/changes/read`, crates/server-projects `journal`): the per-start
//! secret, tenant binding, the cursor and its bounds, and the entries the transcript ops write
//! under the transcript lock, in order, from many concurrent saves. Synthetic, QA-named data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use noevia_server::project_store::{serve, Store, TOKEN_HEADER};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const USER: &str = "11111111-2222-4333-8444-555555555555";
const OTHER: &str = "99999999-2222-4333-8444-555555555555";
const TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";

fn user_dir(root: &Path, user: &str) -> PathBuf {
    root.join("users").join(user)
}

async fn serve_on(root: &Path, token: &str, start: &str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Store {
        data_dir: root.to_path_buf(),
        token: token.into(),
        switch: server_store::RustProjects::owned(),
        start_id: start.into(),
    };
    tokio::spawn(serve(listener, store, std::future::pending()));
    addr
}

async fn start() -> (tempfile::TempDir, SocketAddr) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(user_dir(root.path(), USER)).unwrap();
    std::fs::create_dir_all(user_dir(root.path(), OTHER)).unwrap();
    let addr = serve_on(root.path(), TOKEN, START).await;
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

async fn save(addr: SocketAddr, user: &str, chat: &str, history: Value) -> (u16, Value) {
    let body = json!({ "user": user, "chat": chat, "history": history }).to_string();
    call(addr, "/v1/history/write", Some(TOKEN), &body).await
}

async fn changes(addr: SocketAddr, user: &str, after: Value) -> (u16, Value) {
    let body = json!({ "user": user, "after": after }).to_string();
    call(addr, "/v1/changes/read", Some(TOKEN), &body).await
}

fn chats(v: &Value) -> Vec<(u64, String)> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["seq"].as_u64().unwrap(),
                e["chatId"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_secret_is_required() {
    let (_root, addr) = start().await;
    save(addr, USER, "qa-c1", json!([1])).await;
    let body = json!({ "user": USER, "after": 0 }).to_string();
    for token in [None, Some("wrong"), Some(&TOKEN[..TOKEN.len() - 1])] {
        let (status, reply) = call(addr, "/v1/changes/read", token, &body).await;
        assert_eq!(status, 401, "{token:?}");
        assert!(reply.get("entries").is_none());
    }
    // A previous start's secret is refused by the new start's store.
    let root2 = tempfile::tempdir().unwrap();
    let fresh = serve_on(root2.path(), "another-start-token-0123456789abcdef", START).await;
    assert_eq!(
        call(fresh, "/v1/changes/read", Some(TOKEN), &body).await.0,
        401
    );
}

#[tokio::test]
async fn a_read_is_bound_to_the_user_it_names() {
    let (root, addr) = start().await;
    assert_eq!(save(addr, USER, "qa-mine", json!([1])).await.0, 200);
    assert_eq!(save(addr, OTHER, "qa-theirs", json!([2])).await.0, 200);
    let (_, mine) = changes(addr, USER, json!(0)).await;
    let (_, theirs) = changes(addr, OTHER, json!(0)).await;
    assert_eq!(chats(&mine), vec![(1, "qa-mine".into())]);
    assert_eq!(chats(&theirs), vec![(1, "qa-theirs".into())]);
    // A user id that is not one, or names a path, never reads anything; a removed account is gone.
    for bad in ["../../etc", "11111111-2222-4333-8444-55555555555/", ""] {
        assert_eq!(changes(addr, bad, json!(0)).await.0, 400, "{bad}");
    }
    let gone = "aaaaaaaa-2222-4333-8444-555555555555";
    assert_eq!(changes(addr, gone, json!(0)).await.0, 410);
    assert!(!user_dir(root.path(), gone).exists());
    let (status, _) = call(addr, "/v1/changes/read", Some(TOKEN), "{}").await;
    assert_eq!(status, 400);
    for bad in [json!(-1), json!("3"), json!(1.5)] {
        assert_eq!(changes(addr, USER, bad.clone()).await.0, 400, "{bad}");
    }
}

#[tokio::test]
async fn the_cursor_bounds_and_start_tell_a_reader_where_it_is() {
    let (_root, addr) = start().await;
    let (status, empty) = changes(addr, USER, Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(
        (
            &empty["entries"],
            &empty["oldestSeq"],
            &empty["latestSeq"],
            &empty["start"],
            &empty["more"]
        ),
        (
            &json!([]),
            &Value::Null,
            &Value::Null,
            &json!(START),
            &json!(false)
        )
    );
    for (i, chat) in ["qa-a", "qa-b", "qa-a"].iter().enumerate() {
        let (status, saved) = save(addr, USER, chat, json!([i])).await;
        assert_eq!(status, 200, "{saved}");
    }
    let (_, all) = changes(addr, USER, json!(0)).await;
    assert_eq!(
        chats(&all),
        vec![(1, "qa-a".into()), (2, "qa-b".into()), (3, "qa-a".into())]
    );
    assert_eq!(
        (all["oldestSeq"].as_u64(), all["latestSeq"].as_u64()),
        (Some(1), Some(3))
    );
    let (_, saved) = save(addr, USER, "qa-c", json!(["x"])).await;
    let (_, after) = changes(addr, USER, json!(3)).await;
    assert_eq!(chats(&after), vec![(4, "qa-c".into())]);
    assert_eq!(after["entries"][0]["revision"], saved["revision"]);
    assert_eq!(after["entries"][0]["op"], "write");
    // A conflict writes nothing, so journals nothing.
    let body = json!({ "user": USER, "chat": "qa-c", "history": [1], "baseRevision": "stale" });
    assert_eq!(
        call(addr, "/v1/history/write", Some(TOKEN), &body.to_string())
            .await
            .0,
        409
    );
    assert!(changes(addr, USER, json!(4)).await.1["entries"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_saves_are_journalled_in_order_with_no_gap() {
    let (_root, addr) = start().await;
    let tasks: Vec<_> = (0..24)
        .map(|i| {
            tokio::spawn(async move {
                save(addr, USER, &format!("qa-{}", i % 6), json!([i]))
                    .await
                    .0
            })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    let (_, all) = changes(addr, USER, json!(0)).await;
    let seqs: Vec<u64> = chats(&all).iter().map(|e| e.0).collect();
    assert_eq!(seqs, (1..=24).collect::<Vec<u64>>());
}

fn origins(v: &Value) -> Vec<(String, String)> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["op"].as_str().unwrap().to_string(),
                e["origin"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn each_op_journals_the_origin_its_caller_named_and_absent_is_other() {
    let (_root, addr) = start().await;
    let post = |path: &'static str, body: Value| async move {
        call(addr, path, Some(TOKEN), &body.to_string()).await
    };
    let q = json!([{ "role": "user", "content": "qa question" }]);
    assert_eq!(
        post(
            "/v1/history/write",
            json!({ "user": USER, "chat": "qa-o", "history": q, "origin": "history" })
        )
        .await
        .0,
        200
    );
    let (s, b) = post(
        "/v1/history/append-if-last",
        json!({ "user": USER, "chat": "qa-o", "message": "qa question",
                "entry": { "role": "assistant", "content": "qa partial" }, "origin": "turn" }),
    )
    .await;
    assert_eq!((s, b["outcome"].as_str()), (200, Some("saved")));
    assert_eq!(
        post("/v1/history/write", json!({ "user": USER, "chat": "qa-i", "history": [1], "ifAbsent": true, "origin": "import" })).await.0,
        200
    );
    let (s, b) = post(
        "/v1/history/remove-if",
        json!({ "user": USER, "chat": "qa-i", "history": [1], "origin": "remove" }),
    )
    .await;
    assert_eq!((s, b["removed"].as_bool()), (200, Some(true)));
    // No origin, a null one, and an explicit "other" are all `other`; unknown fields are ignored.
    assert_eq!(save(addr, USER, "qa-x", json!([2])).await.0, 200);
    assert_eq!(
        post("/v1/history/write", json!({ "user": USER, "chat": "qa-x", "history": [3], "origin": null, "futureField": 1 })).await.0,
        200
    );
    assert_eq!(
        post(
            "/v1/history/write",
            json!({ "user": USER, "chat": "qa-x", "history": [4], "origin": "other" })
        )
        .await
        .0,
        200
    );
    let (s, all) = changes(addr, USER, json!(0)).await;
    assert_eq!(s, 200);
    let want: Vec<(String, String)> = [
        ("write", "history"),
        ("append", "turn"),
        ("write", "import"),
        ("remove", "remove"),
        ("write", "other"),
        ("write", "other"),
        ("write", "other"),
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    assert_eq!(origins(&all), want);
}

#[tokio::test]
async fn an_unknown_origin_is_refused_and_changes_nothing() {
    let (root, addr) = start().await;
    let bad = [
        json!("History"),
        json!("brain"),
        json!(""),
        json!(1),
        json!(true),
        json!(["history"]),
    ];
    for origin in bad {
        for (path, body) in [
            (
                "/v1/history/write",
                json!({ "user": USER, "chat": "qa-b", "history": [1], "origin": origin }),
            ),
            (
                "/v1/history/append-if-last",
                json!({ "user": USER, "chat": "qa-b", "message": "m",
                "entry": { "role": "assistant" }, "origin": origin }),
            ),
            (
                "/v1/history/remove-if",
                json!({ "user": USER, "chat": "qa-b", "history": [1], "origin": origin }),
            ),
        ] {
            let (s, b) = call(addr, path, Some(TOKEN), &body.to_string()).await;
            assert_eq!(s, 400, "{path} {origin}");
            assert!(b["error"].as_str().unwrap().contains("origin"), "{b}");
        }
    }
    assert!(!user_dir(root.path(), USER)
        .join("history-qa-b.json")
        .exists());
    let (_, all) = changes(addr, USER, json!(0)).await;
    assert!(all["entries"].as_array().unwrap().is_empty());
}
