//! sbstndalton/noevia#1264 slice 1: `GET`/`POST /api/chats/{chatId}/context` through the whole
//! front. With NOEVIA_RUST_PROJECT_WRITES=1 the front answers them and writes projects.json
//! itself; without it (live today) they go to Node, which is not running here, so nothing is
//! written. Synthetic data only.
#![cfg(feature = "rust-project-writes")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-project-writes-code";
const ORIGIN: &str = "http://127.0.0.1:18021";

struct Front {
    _data: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
}

impl Front {
    fn user_dir(&self) -> PathBuf {
        self.data.join("users").join(&self.user)
    }
}

/// One HTTP/1.1 exchange at once: the status, the response head (lower-case names) and the body.
async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    let status = text[9..12].parse().unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    (status, head.to_ascii_lowercase(), body.to_string())
}

async fn front(writes: bool) -> Front {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let c = Connection::open(server_store::db_path(&data)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute(
        "INSERT INTO settings VALUES('setup_code_hash', ?1)",
        [server_auth::js::digest(CODE)],
    )
    .unwrap();
    // Node answers nothing here: every route under test is the front's.
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into());
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    if writes {
        env.insert("NOEVIA_RUST_PROJECT_WRITES", "1".into());
    }
    let app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    // Sign the owner in (setup), as the corpus does.
    let setup = format!(
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic project-writes password","diaryEnabled":false}}"#
    );
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST /api/setup/complete HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\norigin: {ORIGIN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{setup}",
        setup.len()
    );
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
    assert_eq!(cookies.len(), 2, "{text}");
    let csrf = cookies[1].split_once('=').unwrap().1.to_string();
    let jar = cookies.join("; ");
    let user: String = c
        .query_row("SELECT id FROM users", [], |r| r.get(0))
        .unwrap();
    let f = Front {
        _data: dir,
        data,
        addr,
        jar,
        csrf,
        user,
    };
    // Node's workspace load made the directory on the user's first request.
    std::fs::create_dir_all(f.user_dir()).unwrap();
    f
}

fn headers(f: &Front) -> Vec<(&str, &str)> {
    vec![
        ("cookie", f.jar.as_str()),
        ("x-csrf-token", f.csrf.as_str()),
        ("origin", ORIGIN),
    ]
}

fn projects_json(f: &Front) -> Option<String> {
    std::fs::read_to_string(f.user_dir().join("projects.json")).ok()
}

#[tokio::test]
async fn the_front_answers_and_writes_the_context_project_under_the_switch() {
    let f = front(true).await;
    let (status, _, body) = call(f.addr, "GET", "/api/chats/c1/context", &headers(&f), "").await;
    assert_eq!((status, body.as_str()), (200, r#"{"project":null}"#));
    assert_eq!(
        projects_json(&f),
        None,
        "a GET of a missing project writes nothing"
    );
    let (status, _, body) = call(f.addr, "POST", "/api/chats/c1/context", &headers(&f), "").await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.starts_with(
            r#"{"project":{"id":"cowork-chat-context-c1","name":"Chat attachments c1","#
        ),
        "{body}"
    );
    let stored = projects_json(&f).unwrap();
    assert!(
        stored.contains(r#""id": "cowork-chat-context-c1""#),
        "{stored}"
    );
    // Created once: a second POST answers the stored project and leaves the file as it is.
    let (status, _, again) = call(f.addr, "POST", "/api/chats/c1/context", &headers(&f), "").await;
    assert_eq!((status, again.as_str()), (200, body.as_str()));
    assert_eq!(projects_json(&f).unwrap(), stored);
    let (status, _, body) = call(
        f.addr,
        "POST",
        "/api/chats/bad%E0%A4%A/context",
        &headers(&f),
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (400, r#"{"error":"Invalid chat identifier"}"#)
    );
    // A write without the CSRF token is refused by the gate, before the store.
    let (status, _, _) = call(
        f.addr,
        "POST",
        "/api/chats/c2/context",
        &[("cookie", f.jar.as_str()), ("origin", ORIGIN)],
        "",
    )
    .await;
    assert_eq!(status, 403);
    assert!(!projects_json(&f)
        .unwrap()
        .contains("cowork-chat-context-c2"));
}

#[tokio::test]
async fn without_the_switch_the_context_routes_stay_nodes() {
    let f = front(false).await;
    for m in ["GET", "POST"] {
        let (status, _, body) = call(f.addr, m, "/api/chats/c1/context", &headers(&f), "").await;
        // Proxied to Node, which is not running: never the front's own 200.
        assert_ne!(status, 200, "{m}: {body}");
        assert!(!body.contains("cowork-chat-context-c1"), "{m}: {body}");
    }
    assert_eq!(projects_json(&f), None);
}
