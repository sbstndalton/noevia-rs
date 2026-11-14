//! Track R11: PUT /api/models/inference-budget and PUT /api/models/idle-unload through the whole
//! front, with `NOEVIA_RUST_MODEL_WRITES` unset (relayed to a synthetic Node as before) and `rust`
//! (answered by the front: the settings row, the audit record, the model manager's calls). A
//! synthetic Node answers the facts bridge and records what it was sent; a synthetic model manager
//! answers `/api/v1/idle-unload`. QA-named data only; no engine, no model loads.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
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
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ADMIN: &str = "Cookie: cowork_session=admin-token; cowork_csrf=csrf\r\n";
const MEMBER: &str = "Cookie: cowork_session=member-token; cowork_csrf=csrf\r\n";
const SECRET: &str = "synthetic-store-token-0123456789abcdef";
const LOADER_TOKEN: &str = "qa-loader-token";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn put(addr: SocketAddr, path: &str, who: &str, body: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "PUT {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{who}Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), s.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(b).unwrap_or(Value::Null),
    )
}

fn seed(dir: &Path) -> Connection {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, token) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'QA',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
    }
    db
}

type Seen = Arc<Mutex<Vec<String>>>;

/// An HTTP/1.1 stub: `route(request)` picks the reply; every request is kept.
async fn stub(route: impl Fn(&str) -> (u16, String) + Send + Sync + 'static) -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let (kept, route) = (Arc::clone(&seen), Arc::new(route));
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            let (kept, route) = (Arc::clone(&kept), Arc::clone(&route));
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let request = loop {
                    let Ok(n) = c.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let t = String::from_utf8_lossy(&buf).into_owned();
                    if let Some((h, b)) = t.split_once("\r\n\r\n") {
                        let want = h
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if b.len() >= want {
                            break t;
                        }
                    }
                };
                kept.lock().unwrap().push(request.clone());
                let (code, text) = route(&request);
                let out = format!("HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
                let _ = c.write_all(out.as_bytes()).await;
            });
        }
    });
    (port, seen)
}

/// A synthetic Node: the budget facts (the figure the test says it enforces) and a relay mark for
/// anything else.
async fn node(enforced: f64) -> (u16, Seen) {
    stub(move |req| {
        if req.starts_with("POST /internal/models/facts ") {
            return (200, json!({"budget": {"budgetGib": enforced}}).to_string());
        }
        (202, r#"{"relayed":true}"#.to_owned())
    })
    .await
}

/// A synthetic model manager's `/api/v1/idle-unload`.
async fn loader() -> (u16, Seen) {
    stub(|req| {
        if req.starts_with("GET /api/v1/idle-unload ") {
            return (200, r#"{"seconds":1800,"source":"default"}"#.to_owned());
        }
        if req.starts_with("PUT /api/v1/idle-unload ") {
            return (
                200,
                r#"{"seconds":600,"source":"setting","fallbackSeconds":1800,"fallbackSource":"default","preset":"written"}"#.to_owned(),
            );
        }
        (404, "{}".to_owned())
    })
    .await
}

async fn front(dir: &Path, node: u16, extra: &[(&str, String)]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut env: HashMap<String, String> = [
        ("NOEVIA_LEGACY_UPSTREAM", format!("http://127.0.0.1:{node}")),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    for (k, v) in extra {
        env.insert((*k).to_owned(), v.clone());
    }
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    addr
}

fn on() -> (&'static str, String) {
    ("NOEVIA_RUST_MODEL_WRITES", "rust".to_owned())
}

fn row(db: &Connection) -> Option<String> {
    db.query_row(
        "SELECT value FROM settings WHERE key='inference:memory-budget'",
        [],
        |r| r.get(0),
    )
    .ok()
}

fn audits(db: &Connection, action: &str) -> Vec<(String, String)> {
    let mut q = db
        .prepare("SELECT actor_user_id, detail FROM audit_events WHERE action=?1 ORDER BY id")
        .unwrap();
    q.query_map([action], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_unset_both_puts_stay_node_s() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let (node_port, seen) = node(16.0).await;
    let addr = front(dir.path(), node_port, &[]).await;
    for path in ["/api/models/inference-budget", "/api/models/idle-unload"] {
        let (status, body) = put(addr, path, ADMIN, r#"{"budgetGib":2,"seconds":600}"#).await;
        assert_eq!((status, body), (202, json!({"relayed": true})), "{path}");
    }
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(row(&db), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_front_saves_the_budget_row_and_its_audit() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let (node_port, seen) = node(2.0).await;
    let addr = front(dir.path(), node_port, &[on()]).await;

    // A member: the models write prelude's 403, nothing relayed or written.
    let (status, body) = put(
        addr,
        "/api/models/inference-budget",
        MEMBER,
        r#"{"budgetGib":2}"#,
    )
    .await;
    assert_eq!(
        (status, body),
        (403, json!({"error": "administrator required"}))
    );
    // Out of range: the 400 with the range, nothing written.
    let (status, body) = put(
        addr,
        "/api/models/inference-budget",
        ADMIN,
        r#"{"budgetGib":0}"#,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["errorId"], "invalidBudget");
    assert_eq!(row(&db), None);

    // A save: the row, the audit record and the answer, with this host's range.
    let host = model_writes::budget::host_ram_gib(
        std::fs::read_to_string("/proc/meminfo")
            .as_deref()
            .map_err(|_| ()),
    );
    let range = model_writes::budget::range(host);
    let (status, body) = put(
        addr,
        "/api/models/inference-budget",
        ADMIN,
        r#"{"budgetGib":2}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // Key order is the crate's replay's to check (serde_json sorts keys here).
    assert_eq!(body.as_object().unwrap().len(), 7, "{body}");
    assert_eq!(
        (
            &body["budgetGib"],
            &body["source"],
            &body["defaultGib"],
            &body["minGib"],
            &body["limited"]
        ),
        (
            &json!(2),
            &json!("admin"),
            &json!(16),
            &json!(2),
            &json!(false)
        )
    );
    assert_eq!(body["maxGib"].as_f64(), Some(range.max_gib));
    assert_eq!(body["hostRamGib"].as_f64(), range.host_ram_gib);
    assert_eq!(row(&db).as_deref(), Some(r#"{"budgetGib":2}"#));
    assert_eq!(
        audits(&db, "inference.budget"),
        vec![(A.to_owned(), r#"{"budgetGib":2}"#.to_owned())]
    );
    // Core was asked to drop its shared model reads before each write (as its prelude does) and,
    // after the save, which figure it enforces (it said 2; asked in the background); nothing relayed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !seen
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.starts_with("POST /internal/models/facts "))
    {
        assert!(tokio::time::Instant::now() < deadline, "core was never asked its figure");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let asked = seen.lock().unwrap().clone();
    let kind = |p: &str| asked.iter().filter(|r| r.starts_with(p)).count();
    assert_eq!(kind("POST /internal/auto-roles/invalidate "), 3, "{asked:?}");
    assert_eq!(kind("POST /internal/models/facts "), 1, "{asked:?}");
    assert!(asked.iter().any(|r| r.contains(r#""want":"budget""#)));
    assert_eq!(asked.len(), 4, "{asked:?}");

    // A body JSON.parse reads but the port cannot hold goes to core unchanged: the same bytes, its
    // length and the session.
    let (status, body) = put(
        addr,
        "/api/models/inference-budget",
        ADMIN,
        r#"{"budgetGib":1e400}"#,
    )
    .await;
    assert_eq!((status, body), (202, json!({"relayed": true})));
    let relayed = seen.lock().unwrap().last().cloned().unwrap();
    assert!(relayed.starts_with("PUT /api/models/inference-budget "), "{relayed}");
    let lower = relayed.to_ascii_lowercase();
    assert!(lower.contains("content-length: 19\r\n"), "{relayed}");
    assert!(lower.contains("cowork_session=admin-token"), "{relayed}");
    assert!(relayed.ends_with(r#"{"budgetGib":1e400}"#), "{relayed}");

    // Over 1 MiB: core's own 400, answered here, nothing relayed.
    let before = seen.lock().unwrap().len();
    let big = format!(r#"{{"budgetGib":2,"pad":"{}"}}"#, "x".repeat(1024 * 1024));
    let (status, body) = put(addr, "/api/models/inference-budget", ADMIN, &big).await;
    assert_eq!((status, body), (400, json!({"error": "invalid JSON"})));
    let after: Vec<String> = seen.lock().unwrap()[before..].to_vec();
    assert!(after.iter().all(|r| r.starts_with("POST /internal/auto-roles/invalidate ")), "{after:?}");

    // Without the CSRF header: the session check refuses before anything is written.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let raw = format!("PUT /api/models/inference-budget HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{ADMIN}Content-Type: application/json\r\nContent-Length: 15\r\n\r\n{{\"budgetGib\":3}}");
    s.write_all(raw.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    s.read_to_end(&mut bytes).await.unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.starts_with("HTTP/1.1 403"), "{text}");
    assert_eq!(row(&db).as_deref(), Some(r#"{"budgetGib":2}"#));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_idle_unload_time_without_a_model_manager_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let (node_port, seen) = node(16.0).await;
    let addr = front(dir.path(), node_port, &[on()]).await;
    let (status, body) = put(
        addr,
        "/api/models/idle-unload",
        MEMBER,
        r#"{"seconds":600}"#,
    )
    .await;
    assert_eq!(
        (status, body),
        (403, json!({"error": "administrator required"}))
    );
    let (status, body) = put(addr, "/api/models/idle-unload", ADMIN, r#"{"seconds":600}"#).await;
    assert_eq!(
        (status, body),
        (
            404,
            json!({"error": "The idle unload setting is unavailable", "errorId": "idleUnloadUnavailable"})
        )
    );
    let asked = seen.lock().unwrap().clone();
    assert!(asked.iter().all(|r| r.starts_with("POST /internal/auto-roles/invalidate ")), "{asked:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_idle_unload_time_is_written_through_the_model_manager() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let (node_port, node_seen) = node(16.0).await;
    let (loader_port, loader_seen) = loader().await;
    let addr = front(
        dir.path(),
        node_port,
        &[
            on(),
            (
                "MODEL_LOADER_URL",
                format!("http://127.0.0.1:{loader_port}/"),
            ),
            ("MODEL_LOADER_TOKEN", LOADER_TOKEN.to_owned()),
        ],
    )
    .await;
    // An invalid time is refused before the model manager is asked.
    let (status, body) = put(addr, "/api/models/idle-unload", ADMIN, r#"{"seconds":299}"#).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["errorId"], "idleUnloadInvalid");
    assert!(loader_seen.lock().unwrap().is_empty());

    // No llama.cpp manager here: no held writer, so the write, the audit, then no reload.
    let (status, body) = put(addr, "/api/models/idle-unload", ADMIN, r#"{"seconds":600}"#).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({"seconds": 600, "source": "setting", "fallbackSeconds": 1800, "fallbackSource": "default",
            "defaultSeconds": 1800, "minSeconds": 300, "maxSeconds": 86400, "apply": {"state": "pending"}, "applied": false})
    );
    let calls = loader_seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0].starts_with("GET /api/v1/idle-unload "));
    assert!(calls[1].starts_with("PUT /api/v1/idle-unload "));
    for c in &calls {
        assert!(c
            .to_ascii_lowercase()
            .contains(&format!("x-model-loader-token: {LOADER_TOKEN}")));
    }
    assert!(calls[1].ends_with(r#"{"seconds":600}"#));
    assert_eq!(
        audits(&db, "models.idle_unload"),
        vec![(
            A.to_owned(),
            r#"{"seconds":600,"source":"setting","previous":1800}"#.to_owned()
        )]
    );
    let asked = node_seen.lock().unwrap().clone();
    assert!(asked.iter().all(|r| r.starts_with("POST /internal/auto-roles/invalidate ")), "nothing relayed: {asked:?}");
}
