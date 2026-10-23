//! sbstndalton/noevia#1071 slice 5: `POST /api/chats/{chatId}/history` through the whole front
//! (the request gate, CSRF, the user it returns, the body limit), answered by crates/server-projects
//! `history::post` over `transcript`. Node is not running here. Synthetic data only.
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
const CODE: &str = "synthetic-history-save-code";
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

async fn front() -> Front {
    let f = front_new_account().await;
    std::fs::create_dir_all(f.user_dir()).unwrap();
    f
}

/// A signed-in account that has made no request yet: its directory does not exist.
async fn front_new_account() -> Front {
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
    env.insert("NOEVIA_START_ID", START.into());
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
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic history-save password","diaryEnabled":false}}"#
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
    assert!(
        !data.join("users").exists(),
        "setup makes no user directory"
    );
    Front {
        _data: dir,
        data,
        addr,
        jar,
        csrf,
        user,
    }
}

fn headers(f: &Front) -> Vec<(&str, &str)> {
    vec![
        ("cookie", f.jar.as_str()),
        ("x-csrf-token", f.csrf.as_str()),
        ("origin", ORIGIN),
    ]
}

fn insert_member_session(f: &Front) -> (String, String) {
    let user = "22222222-2222-4222-8222-222222222222";
    let token = "synthetic-member-session-token";
    let csrf = "synthetic-member-csrf-token";
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute(
        "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,'member','member','Synthetic member','member','x',?1,1,1)",
        [user],
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    c.execute(
        "INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES(?1,?2,?3,?4,?4,?5,'synthetic','127.0.0.1')",
        (server_auth::js::digest(token), user, server_auth::js::digest(csrf), now, now + 86_400_000),
    )
    .unwrap();
    (
        user.to_string(),
        format!("cowork_session={token}; cowork_csrf={csrf}"),
    )
}

const START: &str = "0123456789abcdef0123456789abcdef";

fn revision(history: &str) -> String {
    server_projects::history::revision_of(&js_json::wtf16::parse(history).unwrap()).unwrap()
}

fn transcript(f: &Front, user_dir: &std::path::Path, id: &str) -> Option<String> {
    let _ = f;
    std::fs::read_to_string(user_dir.join(format!("history-{id}.json"))).ok()
}

#[tokio::test]
async fn a_save_needs_a_session_and_csrf_and_writes_only_the_callers_transcript() {
    let f = front().await;
    let body = r#"{"history":[{"role":"user","content":"qa hello"}]}"#;
    let path = "/api/chats/c-qa/history";
    // No session; a session without CSRF; a foreign origin: refused, nothing written.
    let (status, _, _) = call(f.addr, "POST", path, &[("origin", ORIGIN)], body).await;
    assert_eq!(status, 401);
    let (status, _, _) = call(
        f.addr,
        "POST",
        path,
        &[("cookie", f.jar.as_str()), ("origin", ORIGIN)],
        body,
    )
    .await;
    assert_eq!(status, 403);
    let (status, _, _) = call(
        f.addr,
        "POST",
        path,
        &[
            ("cookie", f.jar.as_str()),
            ("x-csrf-token", f.csrf.as_str()),
            ("origin", "http://evil.test"),
        ],
        body,
    )
    .await;
    assert_eq!(status, 403);
    assert_eq!(transcript(&f, &f.user_dir(), "c-qa"), None);
    let (status, head, answer) = call(f.addr, "POST", path, &headers(&f), body).await;
    assert_eq!(status, 200, "{answer}");
    assert!(head.contains("cache-control: no-store"), "{head}");
    assert_eq!(
        answer,
        format!(
            r#"{{"ok":true,"revision":"{}"}}"#,
            revision(r#"[{"role":"user","content":"qa hello"}]"#)
        )
    );
    assert!(transcript(&f, &f.user_dir(), "c-qa")
        .unwrap()
        .contains("qa hello"));
    // Another account's save of the same chat id lands in its own directory only.
    let (member, jar) = insert_member_session(&f);
    let member_dir = f.data.join("users").join(&member);
    let csrf = jar.split("cowork_csrf=").nth(1).unwrap().to_string();
    let (status, _, _) = call(
        f.addr,
        "POST",
        path,
        &[
            ("cookie", jar.as_str()),
            ("x-csrf-token", csrf.as_str()),
            ("origin", ORIGIN),
        ],
        r#"{"history":["qa member"]}"#,
    )
    .await;
    assert_eq!(status, 200);
    assert!(transcript(&f, &member_dir, "c-qa")
        .unwrap()
        .contains("qa member"));
    assert!(transcript(&f, &f.user_dir(), "c-qa")
        .unwrap()
        .contains("qa hello"));
    // ... and its GET sees only its own.
    let (_, _, got) = call(
        f.addr,
        "GET",
        path,
        &[("cookie", jar.as_str()), ("origin", ORIGIN)],
        "",
    )
    .await;
    assert!(
        got.contains("qa member") && !got.contains("qa hello"),
        "{got}"
    );
}

#[tokio::test]
async fn conflicts_tombstones_and_limits_answer_as_node() {
    let f = front().await;
    let path = "/api/chats/c1/history";
    let (status, _, _) = call(f.addr, "POST", path, &headers(&f), r#"{"history":[1]}"#).await;
    assert_eq!(status, 200);
    let stale = revision("[]");
    let (status, _, answer) = call(
        f.addr,
        "POST",
        path,
        &headers(&f),
        &format!(r#"{{"history":[2],"baseRevision":"{stale}"}}"#),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(
        answer,
        format!(
            r#"{{"error":"This chat changed on another device.","history":[1],"revision":"{}"}}"#,
            revision("[1]")
        )
    );
    std::fs::write(f.user_dir().join("deleted-chats.json"), r#"["c1"]"#).unwrap();
    let (status, _, answer) = call(f.addr, "POST", path, &headers(&f), r#"{"history":[3]}"#).await;
    assert_eq!(
        (status, answer.as_str()),
        (410, r#"{"error":"This chat was deleted."}"#)
    );
    let big = format!(
        r#"{{"history":["{}"]}}"#,
        "q".repeat(server_projects::history::BODY_LIMIT)
    );
    let (status, _, answer) =
        call(f.addr, "POST", "/api/chats/c2/history", &headers(&f), &big).await;
    assert_eq!(status, 413, "{answer}");
    assert_eq!(
        answer,
        r#"{"error":"This chat is too large to save; start a new chat to keep going."}"#
    );
    assert_eq!(transcript(&f, &f.user_dir(), "c2"), None);
    let (status, _, answer) = call(
        f.addr,
        "POST",
        "/api/chats/%E0%A4/history",
        &headers(&f),
        "{}",
    )
    .await;
    assert_eq!(
        (status, answer.as_str()),
        (400, r#"{"error":"invalid chat id"}"#)
    );
}

#[tokio::test]
async fn a_new_accounts_first_save_makes_its_directory() {
    let f = front_new_account().await;
    let (status, _, _) = call(
        f.addr,
        "POST",
        "/api/chats/c1/history",
        &headers(&f),
        r#"{"history":["qa first"]}"#,
    )
    .await;
    assert_eq!(status, 200);
    assert!(transcript(&f, &f.user_dir(), "c1")
        .unwrap()
        .contains("qa first"));
}

#[tokio::test]
async fn a_save_keeps_this_starts_running_partial() {
    let f = front().await;
    let partial = server_projects::partial::file_for(&f.user_dir(), "c1");
    std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
    std::fs::write(
        &partial,
        format!(r#"{{"content":"qa","running":{{"start":"{START}"}}}}"#),
    )
    .unwrap();
    let (status, _, _) = call(
        f.addr,
        "POST",
        "/api/chats/c1/history",
        &headers(&f),
        r#"{"history":[]}"#,
    )
    .await;
    assert_eq!(status, 200);
    assert!(partial.exists());
    std::fs::write(
        &partial,
        r#"{"content":"qa","running":{"start":"ffffffffffffffffffffffffffffffff"}}"#,
    )
    .unwrap();
    let (status, _, _) = call(
        f.addr,
        "POST",
        "/api/chats/c1/history",
        &headers(&f),
        r#"{"history":[]}"#,
    )
    .await;
    assert_eq!(status, 200);
    assert!(!partial.exists());
}
