//! #2282 native approval HTTP boundary, with synthetic pending-state bridge. No model or chat runs.
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
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-approval-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
type Answer = Arc<Mutex<(u16, String)>>;

struct Node {
    addr: SocketAddr,
    answer: Answer,
    seen: Arc<Mutex<Vec<String>>>,
    resolve: Arc<Mutex<Option<Value>>>,
    revoke: Arc<Mutex<Option<PathBuf>>>,
}

async fn node() -> Node {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let answer: Answer = Arc::new(Mutex::new((
        200,
        json!({"found":true,"kind":"write","generation":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"})
            .to_string(),
    )));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let resolve = Arc::new(Mutex::new(None::<Value>));
    let revoke = Arc::new(Mutex::new(None::<PathBuf>));
    let (a, s, r, d) = (
        Arc::clone(&answer),
        Arc::clone(&seen),
        Arc::clone(&resolve),
        Arc::clone(&revoke),
    );
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            let (a, s, r, d) = (
                Arc::clone(&a),
                Arc::clone(&s),
                Arc::clone(&r),
                Arc::clone(&d),
            );
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let n = c.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if let Some((h, b)) = text.split_once("\r\n\r\n") {
                        let want = h
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if b.len() >= want {
                            break;
                        }
                    }
                }
                s.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).to_ascii_lowercase());
                if String::from_utf8_lossy(&buf).starts_with("POST /internal/tool-approvals/facts ")
                {
                    if let Some(path) = d.lock().unwrap().take() {
                        Connection::open(path)
                            .unwrap()
                            .execute("UPDATE users SET disabled_at=1", [])
                            .unwrap();
                    }
                }
                let (status, text) = a.lock().unwrap().clone();
                let text = if String::from_utf8_lossy(&buf)
                    .starts_with("POST /internal/tool-approvals/resolve ")
                    && status == 200
                    && text.contains("generation")
                {
                    "{\"found\":true,\"accepted\":true}".to_owned()
                } else {
                    text
                };
                let text = if String::from_utf8_lossy(&buf)
                    .starts_with("POST /internal/tool-approvals/resolve ")
                {
                    r.lock()
                        .unwrap()
                        .as_ref()
                        .map(Value::to_string)
                        .unwrap_or(text)
                } else {
                    text
                };
                let out = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
                let _ = c.write_all(out.as_bytes()).await;
            });
        }
    });
    Node {
        addr,
        answer,
        seen,
        resolve,
        revoke,
    }
}

struct Front {
    _dir: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
    node: Node,
}

async fn front(with_node: bool) -> Front {
    let node = node().await;
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
    let upstream = if with_node {
        format!("http://{}", node.addr)
    } else {
        "http://127.0.0.1:9".to_owned()
    };
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
    Front {
        _dir: dir,
        data,
        addr,
        jar,
        csrf,
        user,
        node,
    }
}

impl Front {
    async fn call(&self, path: &str, body: &str, auth: bool, csrf: bool) -> (u16, Value) {
        let mut c = TcpStream::connect(self.addr).await.unwrap();
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
        let req = format!("POST {path} HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\norigin: {ORIGIN}\r\n{credentials}{token}content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",body.len());
        c.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(15), c.read_to_end(&mut out))
            .await
            .unwrap()
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        let status = text.split(' ').nth(1).unwrap().parse().unwrap();
        let body = text.split_once("\r\n\r\n").unwrap().1;
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }
}
#[tokio::test]
async fn identity_and_body_policy_precede_pending_state_and_route_decisions_preserve_remember() {
    let f = front(true).await;
    let post = r#"{"decision":"approve"}"#;
    assert_eq!(
        f.call("/api/tool-approvals/a", post, false, false).await.0,
        401
    );
    assert_eq!(
        f.call("/api/tool-approvals/a", post, true, false).await.0,
        403
    );
    for body in ["null", "[]", "1", "true", "{}{"] {
        assert_eq!(
            f.call("/api/tool-approvals/a", body, true, true).await.0,
            400
        );
    }
    assert_eq!(
        f.call("/api/tool-approvals/%ZZ", post, true, true).await.0,
        400
    );
    assert!(f.node.seen.lock().unwrap().is_empty());
    assert_eq!(
        f.call("/api/tool-approvals/a", post, true, true).await,
        (200, json!({"ok":true}))
    );
    let seen = f.node.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].starts_with("post /internal/tool-approvals/facts "));
    assert!(seen[1].starts_with("post /internal/tool-approvals/resolve "));
    assert!(seen[1].contains(&format!("\"user\":\"{}\"", f.user)));
    assert!(seen[1].contains(&format!("x-noevia-store-token: {STORE_TOKEN}")));
    assert!(!seen[1].contains("x-noevia-client-ip:"));
    *f.node.answer.lock().unwrap() = (
        200,
        json!({"found":true,"kind":"route","generation":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"})
            .to_string(),
    );
    assert_eq!(
        f.call("/api/tool-approvals/a", post, true, true).await,
        (400, json!({"error":"decision must be 'cloud' or 'local'"}))
    );
    assert_eq!(
        f.node.seen.lock().unwrap().len(),
        3,
        "invalid choices never consume"
    );
    assert_eq!(
        f.call(
            "/api/tool-approvals/a",
            r#"{"decision":"local","remember":true}"#,
            true,
            true
        )
        .await
        .0,
        200
    );
    assert!(f
        .node
        .seen
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains("\"remember\":true"));
}
#[tokio::test]
async fn missing_other_user_stale_or_dead_state_never_returns_success() {
    let f = front(true).await;
    *f.node.answer.lock().unwrap() = (200, json!({"found":false}).to_string());
    assert_eq!(
        f.call(
            "/api/tool-approvals/foreign",
            r#"{"decision":"bogus"}"#,
            true,
            true
        )
        .await,
        (404, json!({"error":"no such pending approval"}))
    );
    assert_eq!(f.node.seen.lock().unwrap().len(), 1);
    assert_eq!(
        f.call(
            "/api/tool-approvals/foreign",
            r#"{"decision":{"toString":null}}"#,
            true,
            true
        )
        .await
        .0,
        404,
        "owner check precedes coercion"
    );
    *f.node.answer.lock().unwrap() = (503, "{}".into());
    assert_eq!(
        f.call(
            "/api/tool-approvals/a",
            r#"{"decision":"approve"}"#,
            true,
            true
        )
        .await
        .0,
        503
    );
    *f.node.answer.lock().unwrap() = (
        200,
        json!({"found":true,"kind":"write","generation":""}).to_string(),
    );
    assert_eq!(
        f.call(
            "/api/tool-approvals/a",
            r#"{"decision":"approve"}"#,
            true,
            true
        )
        .await
        .0,
        503
    );
    *f.node.answer.lock().unwrap() = (
        200,
        json!({"found":true,"kind":"write","generation":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"})
            .to_string(),
    );
    *f.node.resolve.lock().unwrap() = Some(json!({"found":false}));
    assert_eq!(
        f.call(
            "/api/tool-approvals/a",
            r#"{"decision":"approve"}"#,
            true,
            true
        )
        .await
        .0,
        404,
        "expired or replaced between facts and resolution"
    );
    *f.node.resolve.lock().unwrap() = Some(json!({"found":true,"accepted":false}));
    assert_eq!(
        f.call(
            "/api/tool-approvals/a",
            r#"{"decision":"approve_all"}"#,
            true,
            true
        )
        .await,
        (
            400,
            json!({"error":"decision must be 'approve', 'deny' or 'approve_all'"})
        ),
        "existing scope refusal cannot become success"
    );
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute("UPDATE users SET disabled_at=1 WHERE id=?1", [&f.user])
        .unwrap();
    let before = f.node.seen.lock().unwrap().len();
    assert_eq!(
        f.call(
            "/api/tool-approvals/a",
            r#"{"decision":"approve"}"#,
            true,
            true
        )
        .await
        .0,
        401
    );
    assert_eq!(f.node.seen.lock().unwrap().len(), before);
    let dead = front(false).await;
    assert_eq!(
        dead.call(
            "/api/tool-approvals/a",
            r#"{"decision":"deny"}"#,
            true,
            true
        )
        .await
        .0,
        503
    );
}

#[tokio::test]
async fn an_account_disabled_during_facts_is_rejected_before_resolution() {
    let f = front(true).await;
    *f.node.revoke.lock().unwrap() = Some(server_store::db_path(&f.data));
    assert_eq!(
        f.call(
            "/api/tool-approvals/a",
            r#"{"decision":"approve"}"#,
            true,
            true
        )
        .await
        .0,
        401
    );
    let seen = f.node.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].starts_with("post /internal/tool-approvals/facts "));
}
