//! #2282 and port contract 2026-11-04 S9: the public approval decision answered from the front's
//! own approval registry, and the store ops Node's turn asks through. A real front and a real
//! store listener, synthetic data only; no Node, no chat, no model.
#![allow(
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

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-approval-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const STRANGER: &str = "99999999-2222-4333-8444-555555555555";
const PROTOCOL: &str = "registry-executor-v3";

#[derive(Default)]
struct Node {
    refuse: std::sync::atomic::AtomicBool,
    block: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    calls: std::sync::atomic::AtomicUsize,
    epoch: std::sync::atomic::AtomicU64,
}
async fn node() -> (SocketAddr, Arc<Node>) {
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
struct Front {
    node: Arc<Node>,
    _dir: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    store: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
}

async fn front() -> Front {
    front_with_node(true).await
}
async fn front_with_node(reachable: bool) -> Front {
    let (node_addr, node) = node().await;
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
    env.insert(
        "NOEVIA_LEGACY_UPSTREAM",
        if reachable {
            format!("http://{node_addr}")
        } else {
            "http://127.0.0.1:9".to_owned()
        },
    );
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

async fn http(addr: SocketAddr, head: &str, body: &str) -> (u16, Value) {
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
    async fn call(&self, path: &str, body: &str, auth: bool, csrf: bool) -> (u16, Value) {
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
    async fn decide(&self, id: &str, body: &str) -> (u16, Value) {
        self.call(&format!("/api/tool-approvals/{id}"), body, true, true)
            .await
    }
    async fn op(&self, op: &str, body: Value) -> (u16, Value) {
        let head = format!(
            "POST /v1/{op} HTTP/1.1\r\nhost: 127.0.0.1\r\nx-noevia-store-token: {STORE_TOKEN}\r\n"
        );
        http(self.store, &head, &body.to_string()).await
    }
    async fn open(&self, id: &str, kind: &str, extra: Value) -> (u16, Value) {
        let mut body = json!({"user":self.user,"id":id,"kind":kind});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        self.op("approvals/open", body).await
    }
    async fn wait(&self, id: &str) -> (u16, Value) {
        self.op("approvals/wait", json!({"user":self.user,"id":id}))
            .await
    }
    async fn binding(&self, chat: &str, scope: &str) -> Value {
        let (status, context) = self
            .op("chat/grant-context", json!({"user":self.user,"chat":chat}))
            .await;
        assert_eq!(status, 200, "{context}");
        json!({"user":self.user,"canonicalChatId":chat,"scope":scope,"context":context})
    }
    async fn granted(&self, chat: &str, scope: &str, binding: &Value) -> bool {
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

#[tokio::test]
async fn identity_and_body_policy_precede_pending_state_and_route_decisions_preserve_remember() {
    let f = front().await;
    assert_eq!(f.open("ap-a", "write", json!({})).await.0, 200);
    let post = r#"{"decision":"approve"}"#;
    assert_eq!(
        f.call("/api/tool-approvals/ap-a", post, false, false)
            .await
            .0,
        401
    );
    assert_eq!(
        f.call("/api/tool-approvals/ap-a", post, true, false)
            .await
            .0,
        403
    );
    for body in ["null", "[]", "1", "true", "{}{"] {
        assert_eq!(f.decide("ap-a", body).await.0, 400, "{body}");
    }
    assert_eq!(f.decide("%ZZ", post).await.0, 400);
    assert_eq!(
        f.decide("ap-a", r#"{"decision":"cloud"}"#).await,
        (
            400,
            json!({"error":"decision must be 'approve', 'deny' or 'approve_all'"})
        ),
        "a routing choice on a write card"
    );
    // None of that consumed the question.
    assert_eq!(f.decide("ap-a", post).await, (200, json!({"ok":true})));
    assert_eq!(f.wait("ap-a").await, (200, json!({"outcome":"approve"})));
    assert_eq!(f.decide("ap-a", post).await.0, 404, "single use");
    // A routing question (#778): its own choices, remember strictly boolean.
    assert_eq!(
        f.open("route-1", "route", json!({"chat":"qa-one"})).await.0,
        200
    );
    assert_eq!(
        f.decide("route-1", post).await,
        (400, json!({"error":"decision must be 'cloud' or 'local'"}))
    );
    assert_eq!(
        f.decide("route-1", r#"{"decision":"local","remember":true}"#)
            .await,
        (200, json!({"ok":true}))
    );
    assert_eq!(
        f.wait("route-1").await,
        (200, json!({"outcome":"local","remember":true}))
    );
    assert_eq!(f.open("route-2", "route", json!({})).await.0, 200);
    assert_eq!(
        f.decide("route-2", r#"{"decision":["cloud"],"remember":1}"#)
            .await
            .0,
        200
    );
    assert_eq!(
        f.wait("route-2").await,
        (200, json!({"outcome":"cloud","remember":false}))
    );
}

#[tokio::test]
async fn missing_other_user_and_disabled_account_never_return_success() {
    let f = front().await;
    assert_eq!(
        f.decide("nobody", r#"{"decision":"bogus"}"#).await,
        (404, json!({"error":"no such pending approval"}))
    );
    // Another account's question: invisible, even to the coercion (owner check comes first).
    let open = json!({"user":STRANGER,"id":"theirs","kind":"write"});
    assert_eq!(f.op("approvals/open", open).await.0, 200);
    assert_eq!(f.decide("theirs", r#"{"decision":"approve"}"#).await.0, 404);
    assert_eq!(
        f.decide("theirs", r#"{"decision":{"toString":null}}"#)
            .await
            .0,
        404
    );
    assert_eq!(
        f.op("approvals/cancel", json!({"user":f.user,"id":"theirs"}))
            .await,
        (200, json!({"cancelled":false}))
    );
    assert_eq!(
        f.op("approvals/wait", json!({"user":f.user,"id":"theirs"}))
            .await
            .0,
        404
    );
    // The owner's own: JavaScript coercion errors survive, after the lookup.
    assert_eq!(f.open("mine", "write", json!({})).await.0, 200);
    assert_eq!(
        f.decide("mine", r#"{"decision":{"toString":null}}"#)
            .await
            .0,
        500
    );
    // A disabled account answers nothing and consumes nothing.
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute("UPDATE users SET disabled_at=1 WHERE id=?1", [&f.user])
        .unwrap();
    assert_eq!(f.decide("mine", r#"{"decision":"approve"}"#).await.0, 401);
    // The question is still open (disabling ended the session, so ask the store directly).
    assert_eq!(
        f.op("approvals/cancel", json!({"user":f.user,"id":"mine"}))
            .await,
        (200, json!({"cancelled":true}))
    );
}

#[tokio::test]
async fn a_held_account_lock_is_a_generic_503_and_consumes_nothing() {
    let f = front().await;
    assert_eq!(f.open("busy", "write", json!({})).await.0, 200);
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (status, body) = f.decide("busy", r#"{"decision":"approve"}"#).await;
    c.execute_batch("ROLLBACK").unwrap();
    assert_eq!(status, 503, "{body}");
    assert!(!body.to_string().contains("SQLITE"));
    assert_eq!(f.decide("busy", r#"{"decision":"approve"}"#).await.0, 200);
}

/// #679: the person left while the card was open. Node drops its wait; the question is withdrawn,
/// so a later answer finds nothing, is never recorded as declined and grants nothing.
#[tokio::test]
async fn a_dropped_wait_stops_the_question_and_a_late_answer_finds_nothing() {
    let f = front().await;
    let b = f.binding("qa-one", "space:free").await;
    let extra = json!({"chat":"qa-one","scope":"space:free","binding":b});
    assert_eq!(f.open("ap-left", "write", extra).await.0, 200);
    let mut wait = TcpStream::connect(f.store).await.unwrap();
    let body = json!({"user":f.user,"id":"ap-left"}).to_string();
    wait.write_all(format!("POST /v1/approvals/wait HTTP/1.1\r\nhost: 127.0.0.1\r\nx-noevia-store-token: {STORE_TOKEN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    drop(wait);
    let mut gone = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        // `kind` is read without consuming: a 404 on a choice the card never takes.
        if f.decide("ap-left", r#"{"decision":"cloud"}"#).await.0 == 404 {
            gone = true;
            break;
        }
    }
    assert!(gone, "withdrawn within 1 s of the dropped wait");
    assert_eq!(
        f.decide("ap-left", r#"{"decision":"approve_all"}"#).await.0,
        404
    );
    assert!(!f.granted("qa-one", "space:free", &b).await);
}

#[tokio::test]
async fn approve_all_grants_its_chat_scope_and_binding_and_a_move_or_delete_revokes_it() {
    let f = front().await;
    let b = f.binding("qa-one", "space:free").await;
    assert!(!f.granted("qa-one", "space:free", &b).await);
    let extra = json!({"chat":"qa-one","scope":"space:free","binding":b});
    assert_eq!(f.open("ap-all", "write", extra.clone()).await.0, 200);
    assert_eq!(
        f.decide("ap-all", r#"{"decision":"approve_all"}"#).await.0,
        200
    );
    assert_eq!(f.wait("ap-all").await, (200, json!({"outcome":"approve"})));
    assert!(f.granted("qa-one", "space:free", &b).await);
    assert!(!f.granted("qa-one", "project:qa-p", &b).await, "#917 scope");
    assert!(!f.granted("qa-one", "space:free", &Value::Null).await);
    let two = f.binding("qa-two", "space:free").await;
    assert!(
        !f.granted("qa-two", "space:free", &two).await,
        "one chat only"
    );
    // The store's move (Node's turn or the public route): the generation moves, the grant goes.
    let (status, _) = f
        .op(
            "chat/move",
            json!({"user":f.user,"chat":"qa-one","projectId":"qa-p"}),
        )
        .await;
    assert_eq!(status, 200);
    assert!(!f.granted("qa-one", "space:free", &b).await);
    let moved = f.binding("qa-one", "project:qa-p").await;
    assert!(!f.granted("qa-one", "project:qa-p", &moved).await);
    // revokeChatGrant.
    let extra = json!({"chat":"qa-two","scope":"space:free","binding":two});
    assert_eq!(f.open("ap-two", "write", extra.clone()).await.0, 200);
    assert_eq!(
        f.decide("ap-two", r#"{"decision":"approve_all"}"#).await.0,
        200
    );
    assert!(f.granted("qa-two", "space:free", &two).await);
    assert_eq!(
        f.op("approvals/revoke", json!({"user":f.user,"chat":"qa-two"}))
            .await,
        (200, json!({"revoked":true}))
    );
    assert!(!f.granted("qa-two", "space:free", &two).await);
    // The public delete drops it too.
    assert_eq!(f.open("ap-three", "write", extra).await.0, 200);
    assert_eq!(
        f.decide("ap-three", r#"{"decision":"approve_all"}"#)
            .await
            .0,
        200
    );
    assert!(f.granted("qa-two", "space:free", &two).await);
    let head = format!(
        "DELETE /api/freechats/qa-two HTTP/1.1\r\nhost: front.test\r\norigin: {ORIGIN}\r\ncookie: {}\r\nx-csrf-token: {}\r\n",
        f.jar, f.csrf
    );
    let (status, body) = http(f.addr, &head, "").await;
    assert_eq!(status, 200, "{body}");
    assert!(!f.granted("qa-two", "space:free", &two).await);
}

#[tokio::test]
async fn store_ops_need_the_secret_and_well_formed_questions() {
    let f = front().await;
    let head = "POST /v1/approvals/open HTTP/1.1\r\nhost: 127.0.0.1\r\n";
    let body = json!({"user":f.user,"id":"x","kind":"write"}).to_string();
    assert_eq!(http(f.store, head, &body).await.0, 401);
    let bad = format!("{head}x-noevia-store-token: not-the-secret-0123456789abcdef\r\n");
    assert_eq!(http(f.store, &bad, &body).await.0, 401);
    assert_eq!(f.open("x", "other", json!({})).await.0, 400);
    assert_eq!(f.open("", "write", json!({})).await.0, 400);
    assert_eq!(f.open("x", "write", json!({"chat":1})).await.0, 400);
    assert_eq!(
        f.op("approvals/open", json!({"id":"x","kind":"write"}))
            .await
            .0,
        400
    );
    assert_eq!(f.open("x", "write", json!({})).await.0, 200);
    assert_eq!(
        f.open("x", "route", json!({})).await,
        (
            409,
            json!({"error":"approval already open","code":"APPROVAL_EXISTS"})
        )
    );
    assert_eq!(f.wait("never-opened").await.0, 404);
    assert_eq!(
        f.op("approvals/cancel", json!({"user":f.user,"id":"x"}))
            .await,
        (200, json!({"cancelled":true}))
    );
    assert_eq!(f.decide("x", r#"{"decision":"approve"}"#).await.0, 404);
}

impl Front {
    /// A chat-wide grant on `chat`, then a grant check's ticket for it.
    async fn ticket(&self, id: &str, chat: &str) -> (Value, String) {
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
    async fn redeem(&self, chat: &str, binding: &Value, ticket: &str) -> bool {
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
    async fn public(&self, method: &str, path: &str, body: &str) -> u16 {
        let head = format!(
            "{method} {path} HTTP/1.1\r\nhost: front.test\r\norigin: {ORIGIN}\r\ncookie: {}\r\nx-csrf-token: {}\r\n",
            self.jar, self.csrf
        );
        http(self.addr, &head, body).await.0
    }
}

/// Re-review 1ed3a14: a granted:true answer already produced authorises nothing; the write's redeem,
/// ordered at the front against every revocation, does. A public move after the answer refuses it.
#[tokio::test]
async fn a_front_move_after_the_grant_answer_refuses_its_redeem() {
    let f = front().await;
    let (b, t) = f.ticket("ap-1", "qa-one").await;
    assert_eq!(
        f.public("POST", "/api/chats/qa-one/move", r#"{"projectId":"qa-p"}"#)
            .await,
        200
    );
    assert!(!f.redeem("qa-one", &b, &t).await);
    // Control: with nothing in between, a ticket redeems once.
    let (b, t) = f.ticket("ap-2", "qa-two").await;
    assert!(f.redeem("qa-two", &b, &t).await);
    assert!(!f.redeem("qa-two", &b, &t).await, "a replay is refused");
}

#[tokio::test]
async fn a_front_delete_after_the_grant_answer_refuses_its_redeem() {
    let f = front().await;
    let (b, t) = f.ticket("ap-1", "qa-two").await;
    assert_eq!(f.public("DELETE", "/api/freechats/qa-two", "").await, 200);
    assert!(!f.redeem("qa-two", &b, &t).await);
}

/// A move that fails (a 400, nothing changed, the live context the same) still revokes, and is
/// ordered against the redeem like any other revocation.
#[tokio::test]
async fn a_failed_move_revokes_and_refuses_the_redeem() {
    let f = front().await;
    let (b, t) = f.ticket("ap-1", "qa-one").await;
    assert_eq!(f.public("POST", "/api/chats/qa-one/move", "{}").await, 400);
    assert_eq!(
        f.binding("qa-one", "space:free").await,
        b,
        "nothing changed"
    );
    assert!(!f.redeem("qa-one", &b, &t).await);
    // Node's own store move, failing too, the same.
    let (b, t) = f.ticket("ap-2", "qa-two").await;
    let (status, _) = f
        .op("chat/move", json!({"user":f.user,"chat":"qa-two"}))
        .await;
    assert_eq!(status, 400);
    assert!(!f.redeem("qa-two", &b, &t).await);
    // And Node's store delete.
    let (b, t) = f.ticket("ap-3", "qa-one").await;
    let (status, _) = f
        .op(
            "chat/delete",
            json!({"user":f.user,"chat":"qa-one","kind":"free"}),
        )
        .await;
    assert_eq!(status, 200);
    assert!(!f.redeem("qa-one", &b, &t).await);
}

#[tokio::test]
async fn redeem_needs_a_ticket() {
    let f = front().await;
    let (b, _) = f.ticket("ap-1", "qa-one").await;
    let (status, _) = f
        .op(
            "approvals/redeem",
            json!({"user":f.user,"chat":"qa-one","scope":"space:free","binding":b,"protocol":PROTOCOL}),
        )
        .await;
    assert_eq!(status, 400);
}

/// Re-review 689d17a P1-2: a grant check that does not name the redeem protocol (a Core from before
/// tickets, which would read `{granted:true, ticket}` as plain approval) is answered "no grant".
#[tokio::test]
async fn a_grant_check_without_the_redeem_protocol_gets_no_grant() {
    let f = front().await;
    let (b, _) = f.ticket("ap-1", "qa-one").await;
    for protocol in [
        Value::Null,
        json!("rust-approval-registry"),
        json!("registry-redeem-v2"),
        json!(1),
    ] {
        let mut body = json!({"user":f.user,"chat":"qa-one","scope":"space:free","binding":b});
        if !protocol.is_null() {
            body["protocol"] = protocol;
        }
        let (status, answer) = f.op("approvals/granted", body).await;
        assert_eq!(status, 200);
        assert_eq!(answer["granted"], json!(false), "{answer}");
        assert!(answer.get("ticket").is_none(), "{answer}");
        assert_eq!(answer["code"], json!("APPROVAL_PROTOCOL"));
    }
    assert!(
        f.granted("qa-one", "space:free", &b).await,
        "the grant stands"
    );
}

/// Revocation success requires Node's epoch acknowledgement, in either redeem/revoke order.
async fn check_revoke_order(redeem_first: bool) {
    use std::sync::atomic::Ordering::SeqCst;
    {
        let f = front().await;
        let (b, ticket) = f.ticket("ap-order", "qa-one").await;
        if redeem_first {
            assert!(f.redeem("qa-one", &b, &ticket).await);
        }
        f.node.block.store(true, SeqCst);
        let revoking = f.op("approvals/revoke", json!({"user":f.user,"chat":"qa-one"}));
        tokio::pin!(revoking);
        tokio::select! { _ = &mut revoking => panic!("revoke returned before Node acknowledgement"), () = f.node.entered.notified() => {} }
        assert!(
            !f.redeem("qa-one", &b, &ticket).await,
            "registry revocation already recorded"
        );
        f.node.release.notify_one();
        assert_eq!(revoking.await.0, 200);
    }
}
#[tokio::test]
async fn redeem_before_revoke_still_requires_node_acknowledgement() {
    check_revoke_order(true).await;
}
#[tokio::test]
async fn revoke_before_redeem_requires_node_acknowledgement_and_refuses_ticket() {
    check_revoke_order(false).await;
}
#[tokio::test]
async fn unreachable_node_never_reports_success_and_retry_can_confirm() {
    use std::sync::atomic::Ordering::SeqCst;
    let f = front_with_node(false).await;
    let (b, ticket) = f.ticket("ap-down", "qa-one").await;
    assert_eq!(
        f.op("approvals/revoke", json!({"user":f.user,"chat":"qa-one"}))
            .await
            .0,
        503
    );
    assert!(
        !f.redeem("qa-one", &b, &ticket).await,
        "revocation is recorded despite failed ack"
    );
    for (method, path) in [
        ("POST", "/api/chats/qa-one/move"),
        ("DELETE", "/api/freechats/qa-one"),
        ("DELETE", "/api/projects/qa-p"),
    ] {
        assert_eq!(f.public(method, path, "{}").await, 503, "{path}");
    }
    assert_eq!(
        f.op(
            "chat/delete",
            json!({"user":f.user,"chat":"qa-one","kind":"free"})
        )
        .await
        .0,
        503
    );
    let f = front().await;
    f.node.refuse.store(true, SeqCst);
    assert_eq!(
        f.op("approvals/revoke", json!({"user":f.user,"chat":"qa-one"}))
            .await
            .0,
        503
    );
    f.node.refuse.store(false, SeqCst);
    assert_eq!(
        f.op("approvals/revoke", json!({"user":f.user,"chat":"qa-one"}))
            .await
            .0,
        200
    );
}

#[tokio::test]
async fn missing_epoch_acknowledgement_times_out_fail_closed() {
    use std::sync::atomic::Ordering::SeqCst;
    let f = front().await;
    f.node.block.store(true, SeqCst);
    let started = std::time::Instant::now();
    let (status, _) = f
        .op("approvals/revoke", json!({"user":f.user,"chat":"qa-one"}))
        .await;
    assert_eq!(status, 503);
    assert!(started.elapsed() < Duration::from_secs(4));
}

// Direct store mutation represents a changed live context discovered by the next check, without
// an explicit HTTP revoke path that would already have notified the executor.
fn change_live_context(f: &Front) {
    server_projects::chat_move::move_chat(
        &f.data,
        &f.user,
        "qa-one",
        server_projects::chat_move::Holder::Project("qa-p".into()),
        None,
        START,
    )
    .unwrap();
}
async fn context_check_payload(f: &Front, op: &str, b: &Value) -> Value {
    let mut body =
        json!({"user":f.user,"chat":"qa-one","scope":"space:free","binding":b,"protocol":PROTOCOL});
    if op == "redeem" {
        let (status, grant) = f.op("approvals/granted", body.clone()).await;
        assert_eq!(status, 200);
        body["ticket"] = grant["ticket"].clone();
        assert!(body["ticket"].is_string());
    }
    body
}
async fn check_context_invalidation_with_paused_executor(op: &str) {
    use std::sync::atomic::Ordering::SeqCst;
    let f = front().await;
    let (b, ticket_a) = f.ticket("ap-context", "qa-one").await;
    let payload = context_check_payload(&f, op, &b).await;
    assert!(f.redeem("qa-one", &b, &ticket_a).await);
    let captured = f.node.epoch.load(SeqCst);
    // Ticket A is already redeemed and parked inside credential setup. Only the final outbound
    // epoch check remains; the earlier registry redeem is not repeated by this executor.
    let credentials = Arc::new(tokio::sync::Semaphore::new(0));
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let parked = Arc::new(tokio::sync::Notify::new());
    let running = {
        let parked = Arc::clone(&parked);
        let (credentials, node, writes) = (
            Arc::clone(&credentials),
            Arc::clone(&f.node),
            Arc::clone(&writes),
        );
        tokio::spawn(async move {
            parked.notify_one();
            let _credential = credentials.acquire().await.unwrap();
            if node.epoch.load(SeqCst) == captured {
                writes.fetch_add(1, SeqCst); // synthetic transport's outbound write call
            }
        })
    };
    parked.notified().await;
    change_live_context(&f);
    let (status, answer) = f.op(&format!("approvals/{op}"), payload).await;
    assert_eq!(status, 200);
    assert_eq!(
        answer[if op == "granted" {
            "granted"
        } else {
            "redeemed"
        }],
        json!(false)
    );
    credentials.add_permits(1);
    running.await.unwrap();
    assert_eq!(
        writes.load(SeqCst),
        0,
        "context-invalidating success must fence ticket A's outbound write"
    );
    assert_eq!(
        f.node.calls.load(SeqCst),
        1,
        "context invalidation requires the private acknowledgement"
    );
}
#[tokio::test]
async fn context_invalidating_grant_check_fences_an_executor_paused_in_credentials() {
    check_context_invalidation_with_paused_executor("granted").await;
}
#[tokio::test]
async fn context_invalidating_redeem_fences_an_executor_paused_in_credentials() {
    check_context_invalidation_with_paused_executor("redeem").await;
}
#[tokio::test]
async fn context_invalidation_cannot_complete_before_the_epoch_acknowledgement() {
    use std::sync::atomic::Ordering::SeqCst;
    let f = front().await;
    let (b, _) = f.ticket("ap-context-wait", "qa-one").await;
    let payload = context_check_payload(&f, "granted", &b).await;
    change_live_context(&f);
    f.node.block.store(true, SeqCst);
    let checking = f.op("approvals/granted", payload);
    tokio::pin!(checking);
    tokio::select! {
        answer = &mut checking => panic!("context invalidation completed without Node acknowledgement: {answer:?}"),
        () = f.node.entered.notified() => {}
    }
    assert_eq!(f.node.epoch.load(SeqCst), 0);
    f.node.release.notify_one();
    assert_eq!(checking.await, (200, json!({"granted":false})));
    assert_eq!(f.node.epoch.load(SeqCst), 1);
}
#[tokio::test]
async fn failed_context_ack_remains_pending_after_the_grant_and_ticket_are_gone() {
    use std::sync::atomic::Ordering::SeqCst;
    for op in ["granted", "redeem"] {
        let f = front().await;
        let (b, _) = f.ticket("ap-context-retry", "qa-one").await;
        let payload = context_check_payload(&f, op, &b).await;
        change_live_context(&f);
        f.node.refuse.store(true, SeqCst);
        for _ in 0..2 {
            assert_eq!(
                f.op(&format!("approvals/{op}"), payload.clone()).await.0,
                503
            );
        }
        f.node.refuse.store(false, SeqCst);
        assert_eq!(
            f.op(&format!("approvals/{op}"), payload.clone()).await.0,
            200
        );
        assert_eq!(f.node.calls.load(SeqCst), 3);
        assert_eq!(f.op(&format!("approvals/{op}"), payload).await.0, 200);
        assert_eq!(
            f.node.calls.load(SeqCst),
            3,
            "confirmed invalidation is retired"
        );
    }
}
#[tokio::test]
async fn context_invalidation_with_an_unreachable_executor_never_reports_success() {
    for op in ["granted", "redeem"] {
        let f = front_with_node(false).await;
        let (b, _) = f.ticket("ap-context-down", "qa-one").await;
        let payload = context_check_payload(&f, op, &b).await;
        change_live_context(&f);
        assert_eq!(
            f.op(&format!("approvals/{op}"), payload.clone()).await.0,
            503
        );
        assert_eq!(f.op(&format!("approvals/{op}"), payload).await.0, 503);
    }
}
