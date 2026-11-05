//! Shared synthetic approval front and authenticated private-peer fixtures.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use serde_json::{json, Value};
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const SCHEMA: &str =
    include_str!("../../../../crates/server-store/tests/fixtures/node-schema.sql");
pub const CODE: &str = "synthetic-approval-code";
pub const ORIGIN: &str = "http://127.0.0.1:18021";
pub const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
pub const START: &str = "0123456789abcdef0123456789abcdef";
pub const STRANGER: &str = "99999999-2222-4333-8444-555555555555";
pub const PROTOCOL: &str = "registry-executor-v3";

#[derive(Default)]
pub struct Node {
    pub refuse: std::sync::atomic::AtomicBool,
    pub block: std::sync::atomic::AtomicBool,
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
    pub calls: std::sync::atomic::AtomicUsize,
    pub epoch: std::sync::atomic::AtomicU64,
}
pub async fn node() -> (SocketAddr, Arc<Node>) {
    let state = Arc::new(Node::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let copy = Arc::clone(&state);
    let router = axum::Router::new().route(
        "/internal/approvals/revoke",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, raw: axum::body::Bytes| {
                let state = Arc::clone(&copy);
                async move {
                    let body: Value = serde_json::from_slice(&raw).unwrap();
                    assert_eq!(headers.get("x-noevia-store-token").unwrap(), STORE_TOKEN);
                    assert_eq!(body["protocol"], json!(PROTOCOL));
                    assert!(body["user"].is_string());
                    state
                        .calls
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    state.entered.notify_one();
                    if state.block.load(std::sync::atomic::Ordering::SeqCst) {
                        state.release.notified().await;
                    }
                    let status = if state.refuse.load(std::sync::atomic::Ordering::SeqCst) {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        // The reviewed executor-side contract: bump synchronously before ack.
                        state
                            .epoch
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        axum::http::StatusCode::OK
                    };
                    (
                        status,
                        [("content-type", "application/json")],
                        json!({"acknowledged":true,"protocol":PROTOCOL}).to_string(),
                    )
                }
            },
        ),
    );
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (addr, state)
}
pub struct Front {
    pub node: Arc<Node>,
    pub _dir: tempfile::TempDir,
    pub data: PathBuf,
    pub addr: SocketAddr,
    pub store: SocketAddr,
    pub jar: String,
    pub csrf: String,
    pub user: String,
}

pub async fn front() -> Front {
    front_with_node(true).await
}
pub async fn front_with_node(reachable: bool) -> Front {
    let (node_addr, node) = node().await;
    front_with_upstream(
        if reachable {
            format!("http://{node_addr}")
        } else {
            "http://127.0.0.1:9".into()
        },
        node,
    )
    .await
}
pub async fn front_with_upstream(upstream: String, node: Arc<Node>) -> Front {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let c = Connection::open(server_store::db_path(&data)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute(
        "INSERT INTO settings VALUES('setup_code_hash', ?1)",
        [server_auth::js::digest(CODE)],
    )
    .unwrap();
    let mut env: HashMap<&str, String> = HashMap::new();
    // Approval decisions stay native; completing revocations additionally require Node acknowledgement.
    env.insert("NOEVIA_LEGACY_UPSTREAM", upstream);
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    env.insert("NOEVIA_START_ID", START.into());
    env.insert("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "true".into());
    env.insert("TRUST_PROXY", "true".into());
    env.insert("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into());
    let mut app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    Arc::get_mut(&mut app).unwrap().storage_child =
        Some(env!("CARGO_BIN_EXE_noevia-server").into());
    let store_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let store = store_listener.local_addr().unwrap();
    tokio::spawn(noevia_server::project_store::serve_with_context(
        store_listener,
        noevia_server::project_store::Store {
            data_dir: data.clone(),
            token: STORE_TOKEN.into(),
            switch: server_store::RustProjects::owned(),
            start_id: START.into(),
        },
        None,
        Some(Arc::clone(&app)),
        std::future::pending(),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    let setup = format!(
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic approval password","diaryEnabled":false}}"#
    );
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("POST /api/setup/complete HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\norigin: {ORIGIN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{setup}", setup.len());
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("HTTP/1.1 201"), "{text}");
    let cookies: Vec<String> = text
        .lines()
        .filter_map(|l| {
            l.strip_prefix("set-cookie: ")
                .map(|v| v.split(';').next().unwrap().to_string())
        })
        .collect();
    let csrf = cookies[1].split_once('=').unwrap().1.to_string();
    let jar = cookies.join("; ");
    let user: String = c
        .query_row("SELECT id FROM users", [], |r| r.get(0))
        .unwrap();
    // Two synthetic free chats and an empty project, for the grant's live context.
    let lists = data.join("users").join(&user);
    std::fs::create_dir_all(&lists).unwrap();
    std::fs::write(
        lists.join("projects.json"),
        r#"{"projects":[{"id":"qa-p","name":"QA","chats":[]}]}"#,
    )
    .unwrap();
    std::fs::write(
        lists.join("free-chats.json"),
        r#"[{"id":"qa-one","title":"QA one"},{"id":"qa-two","title":"QA two"}]"#,
    )
    .unwrap();
    Front {
        node,
        _dir: dir,
        data,
        addr,
        store,
        jar,
        csrf,
        user,
    }
}

pub async fn http(addr: SocketAddr, head: &str, body: &str) -> (u16, Value) {
    let mut c = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{head}connection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    c.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), c.read_to_end(&mut out))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    let body = text.split_once("\r\n\r\n").unwrap().1;
    // A chunked reply (axum) carries one chunk here; take the JSON object out of it.
    let json = body
        .find('{')
        .and_then(|i| body.rfind('}').map(|j| &body[i..=j]))
        .unwrap_or("null");
    (status, serde_json::from_str(json).unwrap_or(Value::Null))
}

impl Front {
    pub async fn call(&self, path: &str, body: &str, auth: bool, csrf: bool) -> (u16, Value) {
        let credentials = if auth {
            format!("cookie: {}\r\n", self.jar)
        } else {
            String::new()
        };
        let token = if csrf {
            format!("x-csrf-token: {}\r\n", self.csrf)
        } else {
            String::new()
        };
        let head = format!(
            "POST {path} HTTP/1.1\r\nhost: front.test\r\norigin: {ORIGIN}\r\n{credentials}{token}"
        );
        http(self.addr, &head, body).await
    }
    pub async fn decide(&self, id: &str, body: &str) -> (u16, Value) {
        self.call(&format!("/api/tool-approvals/{id}"), body, true, true)
            .await
    }
    pub async fn op(&self, op: &str, body: Value) -> (u16, Value) {
        let head = format!(
            "POST /v1/{op} HTTP/1.1\r\nhost: 127.0.0.1\r\nx-noevia-store-token: {STORE_TOKEN}\r\n"
        );
        http(self.store, &head, &body.to_string()).await
    }
    pub async fn open(&self, id: &str, kind: &str, extra: Value) -> (u16, Value) {
        let mut body = json!({"user":self.user,"id":id,"kind":kind});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        self.op("approvals/open", body).await
    }
    pub async fn wait(&self, id: &str) -> (u16, Value) {
        self.op("approvals/wait", json!({"user":self.user,"id":id}))
            .await
    }
    pub async fn binding(&self, chat: &str, scope: &str) -> Value {
        let (status, context) = self
            .op("chat/grant-context", json!({"user":self.user,"chat":chat}))
            .await;
        assert_eq!(status, 200, "{context}");
        json!({"user":self.user,"canonicalChatId":chat,"scope":scope,"context":context})
    }
    pub async fn granted(&self, chat: &str, scope: &str, binding: &Value) -> bool {
        let (status, body) = self
            .op(
                "approvals/granted",
                json!({"user":self.user,"chat":chat,"scope":scope,"binding":binding,"protocol":PROTOCOL}),
            )
            .await;
        assert_eq!(status, 200);
        body["granted"] == json!(true)
    }
}

impl Front {
    /// A chat-wide grant on `chat`, then a grant check's ticket for it.
    pub async fn ticket(&self, id: &str, chat: &str) -> (Value, String) {
        let b = self.binding(chat, "space:free").await;
        let extra = json!({"chat":chat,"scope":"space:free","binding":b});
        assert_eq!(self.open(id, "write", extra).await.0, 200);
        assert_eq!(
            self.decide(id, r#"{"decision":"approve_all"}"#).await.0,
            200
        );
        let (status, body) = self
            .op(
                "approvals/granted",
                json!({"user":self.user,"chat":chat,"scope":"space:free","binding":b,"protocol":PROTOCOL}),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(body["granted"], json!(true), "{body}");
        (b, body["ticket"].as_str().expect("a ticket").to_owned())
    }
    pub async fn redeem(&self, chat: &str, binding: &Value, ticket: &str) -> bool {
        let (status, body) = self
            .op(
                "approvals/redeem",
                json!({"user":self.user,"chat":chat,"scope":"space:free","binding":binding,"ticket":ticket,"protocol":PROTOCOL}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["protocol"], json!(PROTOCOL), "{body}");
        assert_eq!(body["window_ms"], json!(2000), "{body}");
        body["redeemed"] == json!(true)
    }
    pub async fn public(&self, method: &str, path: &str, body: &str) -> u16 {
        let head = format!(
            "{method} {path} HTTP/1.1\r\nhost: front.test\r\norigin: {ORIGIN}\r\ncookie: {}\r\nx-csrf-token: {}\r\n",
            self.jar, self.csrf
        );
        http(self.addr, &head, body).await.0
    }
}
