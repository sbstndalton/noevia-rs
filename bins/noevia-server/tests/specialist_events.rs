//! Coworkers and Spaces slice 5 through the real front (sbstndalton/noevia#2446, capability
//! `rust-specialists`): `GET /api/specialists/{id}/events`, the conversation as AG-UI events in
//! the replay form. The legacy upstream is dead, so a request that fell through to Node would
//! answer 502. Synthetic accounts and QA-named data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    dead_code
)]
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use serde_json::{json, Value};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const COOKIE_A: &str = "cowork_session=session-a; cowork_csrf=csrf";
const COOKIE_B: &str = "cowork_session=session-b; cowork_csrf=csrf";
const MISSING: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn seed(dir: &std::path::Path) {
    let db = Connection::open(server_store::db_path(dir)).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, name, token) in [(A, "ada", "session-a"), (B, "bob", "session-b")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?2,?2,'Synthetic','member','x',?1,1,1)", (uid, name)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("users").join(uid)).unwrap();
    }
}

struct Rig {
    dir: tempfile::TempDir,
    addr: std::net::SocketAddr,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", dir.path().display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    Rig { dir, addr }
}

struct Answer {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
    json: Value,
}

#[derive(Clone, Copy, PartialEq)]
enum Csrf {
    Valid,
    /// A browser page on another site.
    ForeignOrigin,
    WrongToken,
    NoToken,
}

async fn send(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    body: Option<&Value>,
    csrf: Csrf,
) -> Answer {
    let text = body.map(Value::to_string).unwrap_or_default();
    let guard = if method == "GET" {
        String::new()
    } else {
        match csrf {
            Csrf::Valid => format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n"),
            Csrf::ForeignOrigin => {
                "Origin: http://evil.example\r\nX-CSRF-Token: csrf\r\n".to_owned()
            }
            Csrf::NoToken => format!("Origin: http://{addr}\r\n"),
            Csrf::WrongToken => format!("Origin: http://{addr}\r\nX-CSRF-Token: nope\r\n"),
        }
    };
    let cookie = if cookie.is_empty() {
        String::new()
    } else {
        format!("Cookie: {cookie}\r\n")
    };
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{cookie}Content-Type: application/json\r\nContent-Length: {}\r\n{guard}\r\n{text}",
            text.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(40), s.read_to_end(&mut bytes)).await;
    let raw = String::from_utf8(bytes).unwrap();
    let (h, b) = raw.split_once("\r\n\r\n").unwrap();
    let mut lines = h.lines();
    let status = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_lowercase(), v.to_owned()))
        .collect();
    Answer {
        status,
        headers,
        body: b.to_owned(),
        json: serde_json::from_str(b).unwrap_or(Value::Null),
    }
}

impl Rig {
    async fn go(&self, method: &str, path: &str, cookie: &str, body: Option<Value>) -> Answer {
        send(self.addr, method, path, cookie, body.as_ref(), Csrf::Valid).await
    }
    async fn a(&self, method: &str, path: &str, body: Option<Value>) -> Answer {
        self.go(method, path, COOKIE_A, body).await
    }
    async fn space(&self, name: &str) -> String {
        let r = self
            .a("POST", "/api/spaces", Some(json!({"name": name})))
            .await;
        assert_eq!(r.status, 201, "{}", r.json);
        r.json["space"]["id"].as_str().unwrap().to_owned()
    }
    fn user_dir(&self, user: &str) -> std::path::PathBuf {
        self.dir.path().join("users").join(user)
    }
}

fn code(a: &Answer) -> &str {
    a.json["code"].as_str().unwrap_or("")
}

/// The data of every frame of a stream body, in order, as JSON.
fn frames(a: &Answer) -> Vec<Value> {
    assert!(
        a.body.is_empty() || a.body.ends_with("\n\n"),
        "{:?}",
        a.body
    );
    a.body
        .split("\n\n")
        .filter(|f| !f.is_empty())
        .map(|f| {
            let data = f.strip_prefix("data: ").expect("a data frame");
            assert!(!data.contains('\n'), "one line per frame");
            serde_json::from_str(data).unwrap()
        })
        .collect()
}

fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

impl Rig {
    async fn coworker(&self, cookie: &str, name: &str) -> Value {
        let r = self
            .go(
                "POST",
                "/api/specialists",
                cookie,
                Some(json!({"name": name, "brief": "QA brief"})),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.json);
        r.json["specialist"].clone()
    }

    async fn save(&self, cookie: &str, chat: &str, history: Value) {
        let r = self
            .go(
                "POST",
                &format!("/api/chats/{chat}/history"),
                cookie,
                Some(json!({ "history": history })),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.json);
    }
}

fn conversation() -> Value {
    json!([
        {"id": "qa-u1", "role": "user", "content": "QA first"},
        {"id": "qa-a1", "role": "assistant", "content": "QA one"},
        {"id": "qa-u2", "role": "user", "content": "QA second"},
        {"id": "qa-a2", "role": "assistant", "content": "QA two", "toolCalls": [
            {"name": "space_page_save", "args": "{}", "status": "done",
             "result": "{\"pageId\":\"p1\",\"spaceId\":\"s1\",\"title\":\"QA\",\"link\":\"/spaces/s1/p1\"}"}
        ]}
    ])
}

#[tokio::test]
async fn signed_out_is_401_and_the_gate_comes_before_the_id() {
    let r = rig().await;
    for path in [
        format!("/api/specialists/{MISSING}/events"),
        "/api/specialists/not-an-id/events".to_owned(),
    ] {
        let a = r.go("GET", &path, "", None).await;
        assert_eq!(a.status, 401, "{path}");
    }
}

#[tokio::test]
async fn an_unknown_malformed_or_other_accounts_id_is_404() {
    let r = rig().await;
    let mine = r.coworker(COOKIE_A, "QA Scribe").await;
    let id = mine["id"].as_str().unwrap();
    r.save(COOKIE_A, mine["chatId"].as_str().unwrap(), conversation())
        .await;
    for path in [
        format!("/api/specialists/{MISSING}/events"),
        "/api/specialists/not-an-id/events".to_owned(),
        // Account B asks for account A's coworker.
        format!("/api/specialists/{id}/events"),
    ] {
        let cookie = if path.ends_with(&format!("{id}/events")) {
            COOKIE_B
        } else {
            COOKIE_A
        };
        let a = r.go("GET", &path, cookie, None).await;
        assert_eq!((a.status, code(&a)), (404, "not_found"), "{path}");
        assert!(!a.body.contains("QA first"), "{path}");
    }
    assert_eq!(
        r.go(
            "GET",
            &format!("/api/specialists/{id}/events"),
            COOKIE_A,
            None
        )
        .await
        .status,
        200
    );
}

#[tokio::test]
async fn other_methods_are_405_with_allow_and_a_trailing_slash_is_404() {
    let r = rig().await;
    let id = r.coworker(COOKIE_A, "QA Scribe").await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("/api/specialists/{id}/events");
    for m in ["POST", "PUT", "PATCH", "DELETE"] {
        let a = r.go(m, &path, COOKIE_A, Some(json!({}))).await;
        assert_eq!((a.status, code(&a)), (405, "method_not_allowed"), "{m}");
        assert_eq!(a.headers.get("allow").map(String::as_str), Some("GET"));
    }
    let a = r.go("GET", &format!("{path}/"), COOKIE_A, None).await;
    assert_eq!(a.status, 404);
}

#[tokio::test]
async fn an_empty_conversation_is_an_empty_well_formed_stream() {
    let r = rig().await;
    let id = r.coworker(COOKIE_A, "QA Scribe").await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let a = r
        .go(
            "GET",
            &format!("/api/specialists/{id}/events"),
            COOKIE_A,
            None,
        )
        .await;
    assert_eq!(a.status, 200);
    assert_eq!(
        a.headers.get("content-type").map(String::as_str),
        Some("text/event-stream")
    );
    assert_eq!(a.body, "");
}

#[tokio::test]
async fn the_stored_conversation_is_replayed_as_frames_in_order() {
    let r = rig().await;
    let s = r.coworker(COOKIE_A, "QA Scribe").await;
    let id = s["id"].as_str().unwrap();
    r.save(COOKIE_A, s["chatId"].as_str().unwrap(), conversation())
        .await;
    let a = r
        .go(
            "GET",
            &format!("/api/specialists/{id}/events"),
            COOKIE_A,
            None,
        )
        .await;
    assert_eq!(a.status, 200);
    let h = |k: &str| a.headers.get(k).map(String::as_str);
    assert_eq!(h("content-type"), Some("text/event-stream"));
    assert_eq!(h("cache-control"), Some("no-cache, no-transform"));
    assert_eq!(h("x-accel-buffering"), Some("no"));
    assert_eq!(h("x-content-type-options"), Some("nosniff"));
    let ev = frames(&a);
    assert_eq!(
        types(&ev),
        [
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "RUN_STARTED",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "RUN_FINISHED",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "RUN_STARTED",
            "TOOL_CALL_START",
            "TOOL_CALL_ARGS",
            "TOOL_CALL_END",
            "TOOL_CALL_RESULT",
            "CUSTOM",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "RUN_FINISHED",
        ]
    );
    assert_eq!(ev[3]["threadId"], id);
    assert_eq!(ev[3]["runId"], "qa-a1");
    let saved = ev.iter().find(|e| e["type"] == "CUSTOM").unwrap();
    assert_eq!(
        saved["value"],
        json!({"pageId": "p1", "spaceId": "s1", "title": "QA", "link": "/spaces/s1/p1"})
    );
}

#[tokio::test]
async fn after_replays_only_what_follows_and_an_unknown_cursor_is_400() {
    let r = rig().await;
    let s = r.coworker(COOKIE_A, "QA Scribe").await;
    let id = s["id"].as_str().unwrap();
    r.save(COOKIE_A, s["chatId"].as_str().unwrap(), conversation())
        .await;
    let path = format!("/api/specialists/{id}/events");
    let a = r
        .go("GET", &format!("{path}?after=qa-a1"), COOKIE_A, None)
        .await;
    assert_eq!(a.status, 200);
    let ev = frames(&a);
    assert_eq!(ev[0]["messageId"], "qa-u2");
    assert!(!a.body.contains("qa-a1"));
    let last = r
        .go("GET", &format!("{path}?after=qa-a2"), COOKIE_A, None)
        .await;
    assert_eq!((last.status, last.body.as_str()), (200, ""));
    let bad = r
        .go("GET", &format!("{path}?after=nope"), COOKIE_A, None)
        .await;
    assert_eq!((bad.status, code(&bad)), (400, "invalid_request"));
}

#[tokio::test]
async fn concurrent_stream_reads_and_a_write_do_not_wait_on_one_another() {
    // Two reads of one stream and a write to the account interleave without waiting on one another.
    let r = rig().await;
    let s = r.coworker(COOKIE_A, "QA Scribe").await;
    r.save(COOKIE_A, s["chatId"].as_str().unwrap(), conversation())
        .await;
    let path = format!("/api/specialists/{}/events", s["id"].as_str().unwrap());
    let (a, b, c) = tokio::join!(
        r.go("GET", &path, COOKIE_A, None),
        r.go("GET", &path, COOKIE_A, None),
        r.go(
            "POST",
            "/api/spaces",
            COOKIE_A,
            Some(json!({"name": "QA Space"}))
        ),
    );
    assert_eq!((a.status, b.status, c.status), (200, 200, 201));
    assert_eq!(a.body, b.body);
}
