//! sbstndalton/noevia#1071 slice 6: a chat's delete. `DELETE /api/freechats/{id}` and
//! `DELETE /api/projects/{p}/chats/{c}` through the whole front (the request gate, CSRF, the user it
//! returns), and the internal store's `POST /v1/chat/delete` (its secret, its tenant binding), both
//! answered by crates/server-projects `chat_delete`. Node is not running here. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::config::Config;
use noevia_server::project_store::{self, Store, TOKEN_HEADER};
use noevia_server::serve::{serve, Limits};
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-chat-delete-code";
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
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic chat-delete password","diaryEnabled":false}}"#
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

const TOKEN: &str = "synthetic-store-token-0123456789abcdef";

/// A free chat and a project chat, each with its transcript, meter, brain note and partial.
fn seed(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("free-chats.json"),
        r#"[{"id":"qa-free","title":"QA free"},{"id":"qa-keep","title":"QA keep"}]"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("projects.json"),
        r#"{"projects":[{"id":"qa-p","name":"QA","chats":[{"id":"qa-proj"}]}]}"#,
    )
    .unwrap();
    for id in ["qa-free", "qa-keep", "qa-proj"] {
        for f in files(dir, id) {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, r#"{"history":[]}"#).unwrap();
        }
    }
}

fn files(dir: &Path, id: &str) -> Vec<PathBuf> {
    vec![
        server_projects::history::history_file(dir, id),
        server_projects::context_window::state_file(dir, id),
        server_projects::chat_delete::brain_file(dir, id),
        server_projects::partial::file_for(dir, id),
    ]
}

fn gone(dir: &Path, id: &str) -> bool {
    files(dir, id).iter().all(|f| !f.exists())
}

fn kept(dir: &Path, id: &str) -> bool {
    files(dir, id).iter().all(|f| f.exists())
}

#[tokio::test]
async fn a_delete_needs_a_session_and_csrf_and_touches_only_the_callers_chats() {
    let f = front().await;
    seed(&f.user_dir());
    let (member, jar) = insert_member_session(&f);
    let member_dir = f.data.join("users").join(&member);
    seed(&member_dir);
    for path in ["/api/freechats/qa-free", "/api/projects/qa-p/chats/qa-proj"] {
        let (status, _, _) = call(f.addr, "DELETE", path, &[("origin", ORIGIN)], "").await;
        assert_eq!(status, 401, "{path}");
        let (status, _, _) = call(
            f.addr,
            "DELETE",
            path,
            &[("cookie", f.jar.as_str()), ("origin", ORIGIN)],
            "",
        )
        .await;
        assert_eq!(status, 403, "{path}");
        let (status, _, _) = call(
            f.addr,
            "DELETE",
            path,
            &[
                ("cookie", f.jar.as_str()),
                ("x-csrf-token", f.csrf.as_str()),
                ("origin", "http://evil.test"),
            ],
            "",
        )
        .await;
        assert_eq!(status, 403, "{path}");
    }
    assert!(kept(&f.user_dir(), "qa-free") && kept(&f.user_dir(), "qa-proj"));
    let (status, head, answer) =
        call(f.addr, "DELETE", "/api/freechats/qa-free", &headers(&f), "").await;
    assert_eq!((status, answer.as_str()), (200, r#"{"ok":true}"#));
    assert!(head.contains("cache-control: no-store"), "{head}");
    let (status, _, answer) = call(
        f.addr,
        "DELETE",
        "/api/projects/qa-p/chats/qa-proj",
        &headers(&f),
        "",
    )
    .await;
    assert_eq!((status, answer.as_str()), (200, r#"{"ok":true}"#));
    assert!(gone(&f.user_dir(), "qa-free") && gone(&f.user_dir(), "qa-proj"));
    assert!(kept(&f.user_dir(), "qa-keep"));
    let tomb = std::fs::read_to_string(f.user_dir().join("deleted-chats.json")).unwrap();
    assert_eq!(tomb, r#"["qa-free","qa-proj"]"#);
    // Again: Node's 404, nothing more changed.
    let (status, _, answer) =
        call(f.addr, "DELETE", "/api/freechats/qa-free", &headers(&f), "").await;
    assert_eq!(
        (status, answer.as_str()),
        (404, r#"{"error":"no such chat"}"#)
    );
    let (status, _, _) = call(
        f.addr,
        "DELETE",
        "/api/freechats/%E0%A4%A",
        &headers(&f),
        "",
    )
    .await;
    assert_eq!(status, 400);
    // The other account kept all of its own, the same ids included.
    assert!(kept(&member_dir, "qa-free") && kept(&member_dir, "qa-proj"));
    assert!(!member_dir.join("deleted-chats.json").exists());
    let csrf = jar.split("cowork_csrf=").nth(1).unwrap().to_string();
    let (status, _, _) = call(
        f.addr,
        "DELETE",
        "/api/freechats/qa-free",
        &[
            ("cookie", jar.as_str()),
            ("x-csrf-token", csrf.as_str()),
            ("origin", ORIGIN),
        ],
        "",
    )
    .await;
    assert_eq!(status, 200);
    assert!(gone(&member_dir, "qa-free") && kept(&member_dir, "qa-proj"));
}

async fn store_on(root: &Path) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Store {
        data_dir: root.to_path_buf(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: START.into(),
    };
    tokio::spawn(project_store::serve(
        listener,
        store,
        std::future::pending(),
    ));
    addr
}

async fn store_call(addr: SocketAddr, token: Option<&str>, body: &str) -> (u16, String) {
    let mut h = vec![("content-type", "application/json")];
    if let Some(t) = token {
        h.push((TOKEN_HEADER, t));
    }
    let (status, _, body) = call(addr, "POST", "/v1/chat/delete", &h, body).await;
    (status, body)
}

const QA_USER: &str = "11111111-2222-4333-8444-555555555555";
const QA_OTHER: &str = "99999999-2222-4333-8444-555555555555";

#[tokio::test]
async fn the_store_op_needs_the_secret_and_binds_the_user() {
    let root = tempfile::tempdir().unwrap();
    let mine = root.path().join("users").join(QA_USER);
    let theirs = root.path().join("users").join(QA_OTHER);
    seed(&mine);
    seed(&theirs);
    let addr = store_on(root.path()).await;
    let ask = format!(r#"{{"user":"{QA_USER}","chat":"qa-free","kind":"free"}}"#);
    for token in [None, Some("synthetic-store-token-of-a-previous-start-0000")] {
        let (status, _) = store_call(addr, token, &ask).await;
        assert_eq!(status, 401);
    }
    assert!(kept(&mine, "qa-free"));
    let (status, body) = store_call(addr, Some(TOKEN), &ask).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"deleted":true,"leftovers":0,"swept":false}"#);
    assert!(gone(&mine, "qa-free") && kept(&theirs, "qa-free"));
    let (_, body) = store_call(addr, Some(TOKEN), &ask).await;
    assert_eq!(body, r#"{"deleted":false,"leftovers":0,"swept":true}"#);
    // Recovery retries repeat the idempotent removal notice, even with no files left.
    let changes = std::fs::read_to_string(mine.join("chat-changes.jsonl")).unwrap();
    let entries: Vec<serde_json::Value> = changes
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(entries.len(), 2, "{changes}");
    for (i, entry) in entries.iter().enumerate() {
        assert_eq!(entry["seq"], i + 1);
        assert_eq!(entry["chatId"], "qa-free");
        assert_eq!(entry["op"], "remove");
        assert_eq!(entry["origin"], "remove");
    }
    assert!(!theirs.join("chat-changes.jsonl").exists());
    // A project's chat; a deleted project's chat (unlisted) only while no list holds it.
    let (status, body) = store_call(
        addr,
        Some(TOKEN),
        &format!(r#"{{"user":"{QA_USER}","chat":"qa-keep","kind":"unlisted"}}"#),
    )
    .await;
    assert_eq!((status, body.contains("CHAT_LISTED")), (409, true));
    assert!(kept(&mine, "qa-keep"));
    let (status, _) = store_call(
        addr,
        Some(TOKEN),
        &format!(r#"{{"user":"{QA_USER}","chat":"qa-proj","kind":"project","projectId":"qa-p","origin":"other"}}"#),
    )
    .await;
    assert_eq!(status, 200);
    assert!(gone(&mine, "qa-proj"));
    // Bad requests change nothing.
    for bad in [
        format!(r#"{{"user":"{QA_USER}","chat":"qa-keep","kind":"project"}}"#),
        format!(r#"{{"user":"{QA_USER}","chat":"qa-keep","kind":"free","projectId":"qa-p"}}"#),
        format!(r#"{{"user":"{QA_USER}","chat":"qa-keep"}}"#),
        format!(r#"{{"user":"{QA_USER}","chat":"qa-keep","kind":"free","origin":"nope"}}"#),
        format!(r#"{{"user":"{QA_USER}","chat":"","kind":"free"}}"#),
        r#"{"chat":"qa-keep","kind":"free"}"#.to_string(),
        r#"{"user":"../x","chat":"qa-keep","kind":"free"}"#.to_string(),
        "not json".to_string(),
    ] {
        let (status, _) = store_call(addr, Some(TOKEN), &bad).await;
        assert_eq!(status, 400, "{bad}");
    }
    assert!(kept(&mine, "qa-keep"));
    // A removed account: 410, its directory not recreated.
    let gone_user = "aaaaaaaa-2222-4333-8444-555555555555";
    let (status, _) = store_call(
        addr,
        Some(TOKEN),
        &format!(r#"{{"user":"{gone_user}","chat":"qa-free","kind":"free"}}"#),
    )
    .await;
    assert_eq!(status, 410);
    assert!(!root.path().join("users").join(gone_user).exists());
}
