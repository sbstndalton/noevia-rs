//! Native public chat move: actual auth/CSRF/tenant/regate and durable foundation. Synthetic only.
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
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-chat-move-code";
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
    env.insert("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "1".into());
    env.insert("TRUST_PROXY", "true".into());
    assert_eq!(
        server_projects::journal::CHAT_MOVE_JOURNAL_MARKER,
        "noevia-chat-move-journal/1"
    );
    server_projects::journal::set_start_id(START);
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
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic chat-move password","diaryEnabled":false}}"#
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
use std::time::Duration;
async fn call_slow(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
    between: impl FnOnce(),
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\nexpect: 100-continue\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).await.unwrap();
    s.flush().await.unwrap();
    // The interim answer, read byte by byte so nothing of the final reply is consumed. Bounded:
    // a front that never asks for the body fails here instead of hanging the suite.
    let interim = tokio::time::timeout(Duration::from_secs(30), async {
        let mut got = Vec::new();
        let mut byte = [0u8; 1];
        while !got.ends_with(b"\r\n\r\n") {
            assert_eq!(
                s.read(&mut byte).await.unwrap(),
                1,
                "closed before 100 Continue: {got:?}"
            );
            got.push(byte[0]);
        }
        String::from_utf8(got).unwrap()
    })
    .await
    .expect("the front never asked for the body (no 100 Continue)");
    assert!(interim.starts_with("HTTP/1.1 100 "), "{interim}");
    between();
    s.write_all(body.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    let status = text[9..12].parse().unwrap();
    let body = text.split_once("\r\n\r\n").unwrap().1.to_string();
    (status, body)
}

/// One HTTP/1.1 exchange at once: the status, the response head (lower-case names) and the body.
#[cfg(any(feature = "rust-free-chats", feature = "rust-project-chats"))]
fn seed(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("free-chats.json"),
        r#"[{"id":"qa raw!","title":"kept title","updatedAt":1},{"id":"qa-other","updatedAt":2}]"#,
    )
    .unwrap();
    std::fs::write(dir.join("projects.json"),r#"{"projects":[{"id":"qa-p","chats":[]},{"id":"free","chats":[]},{"id":"qa-old","archived":true,"chats":[]}]}"#).unwrap();
    std::fs::write(dir.join("history-qaraw.json"), "transcript sentinel").unwrap();
}
fn lists(dir: &Path) -> (Vec<u8>, Vec<u8>) {
    (
        std::fs::read(dir.join("projects.json")).unwrap(),
        std::fs::read(dir.join("free-chats.json")).unwrap(),
    )
}

#[tokio::test]
async fn public_move_needs_actual_auth_csrf_origin_and_touches_only_its_tenant() {
    let f = front().await;
    seed(&f.user_dir());
    let (member, jar) = insert_member_session(&f);
    let member_dir = f.data.join("users").join(member);
    seed(&member_dir);
    let other = lists(&member_dir);
    let path = "/api/chats/qa%20raw!/move";
    let body = r#"{"projectId":"qa-p","user":"other","start":"forged","generation":"99"}"#;
    assert_eq!(call(f.addr, "POST", path, &[], body).await.0, 401);
    assert_eq!(
        call(
            f.addr,
            "POST",
            path,
            &[("cookie", &f.jar), ("origin", ORIGIN)],
            body
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            f.addr,
            "POST",
            path,
            &[
                ("cookie", &f.jar),
                ("origin", "http://evil.test"),
                ("x-csrf-token", &f.csrf)
            ],
            body
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            f.addr,
            "POST",
            path,
            &[("authorization", "Bearer nva_invalid"), ("origin", ORIGIN)],
            body
        )
        .await
        .0,
        401
    );
    assert!(
        !server_auth::identity::browser_only_path(path, "POST"),
        "move retains device policy"
    );
    let (status, _, answer) = call(f.addr, "POST", path, &headers(&f), body).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&answer).unwrap(),
        serde_json::json!({"ok":true,"from":null,"projectId":"qa-p"})
    );
    assert_eq!(lists(&member_dir), other);
    assert!(!jar.is_empty());
    assert_eq!(
        std::fs::read_to_string(f.user_dir().join("history-qaraw.json")).unwrap(),
        "transcript sentinel"
    );
    let ctx = server_projects::chat_move::grant_context(&f.data, &f.user, "qaraw", START).unwrap();
    assert_eq!(ctx.generation, "1");
    assert_eq!(
        ctx.holder,
        server_projects::chat_move::Holder::Project("qa-p".into())
    );
    let (_, _, answer) = call(f.addr, "POST", path, &headers(&f), r#"{"projectId":null}"#).await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&answer).unwrap(),
        serde_json::json!({"ok":true,"from":"qa-p","projectId":null})
    );
    assert_eq!(
        server_projects::chat_move::grant_context(&f.data, &f.user, "qaraw", START)
            .unwrap()
            .generation,
        "2"
    );
    let entries = server_projects::journal::read(&f.data, &f.user, 0)
        .unwrap()
        .entries;
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e["delivery"] == "live"
        && e["rawChatId"] == "qa raw!"
        && e["chatId"] == "qaraw"
        && e["start"] == START));

    // Native access tokens keep the shared gate policy: no browser CSRF cookie is needed.
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    c.execute(
        "INSERT INTO device_grants(id,user_id,client_name,created_at,last_used_at,expires_at,ip,user_agent) VALUES('synthetic-grant',?1,'Synthetic',?2,?2,?3,'127.0.0.1','ua')",
        (&f.user, now, now + 86_400_000),
    ).unwrap();
    c.execute(
        "INSERT INTO device_tokens(token_hash,grant_id,kind,created_at,expires_at) VALUES(?1,'synthetic-grant','access',?2,?3)",
        (server_auth::js::digest("nva_synthetic-device-token"), now, now + 86_400_000),
    ).unwrap();
    assert_eq!(
        call(
            f.addr,
            "POST",
            path,
            &[("authorization", "Bearer nva_synthetic-device-token")],
            body
        )
        .await
        .0,
        200
    );
    assert_eq!(
        server_projects::chat_move::grant_context(&f.data, &f.user, "qaraw", START)
            .unwrap()
            .generation,
        "3"
    );
    assert_eq!(lists(&member_dir), other);
}
#[tokio::test]
async fn public_move_validates_body_encoding_frame_archived_and_tagged_project_named_free() {
    let f = front().await;
    seed(&f.user_dir());
    let before = lists(&f.user_dir());
    for (path, body, status, error) in [
        ("/api/chats/%E0%A4/move", "{}", 400, "invalid chat id"),
        ("/api/chats/qa%20raw!/move", "{", 400, "invalid JSON"),
        (
            "/api/chats/qa%20raw!/move",
            "null",
            400,
            "projectId (or null) required",
        ),
        (
            "/api/chats/qa%20raw!/move",
            "[]",
            400,
            "projectId (or null) required",
        ),
        (
            "/api/chats/qa%20raw!/move",
            r#"{"projectId":7}"#,
            400,
            "projectId (or null) required",
        ),
        (
            "/api/chats/qa%20raw!/move",
            r#"{"projectId":"qa-p","frame":{"kind":"bogus"}}"#,
            400,
            "invalid frame",
        ),
        (
            "/api/chats/qa%20raw!/move",
            r#"{"projectId":"qa-old"}"#,
            404,
            "no such chat or project",
        ),
        (
            "/api/chats/missing/move",
            r#"{"projectId":null}"#,
            404,
            "no such chat or project",
        ),
    ] {
        let (got, _, answer) = call(f.addr, "POST", path, &headers(&f), body).await;
        assert_eq!(got, status, "{path} {answer}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&answer).unwrap()["error"],
            error
        );
        assert_eq!(lists(&f.user_dir()), before);
    }
    let oversized = format!(
        r#"{{"projectId":null,"padding":"{}"}}"#,
        "x".repeat(1024 * 1024)
    );
    assert_eq!(
        call(
            f.addr,
            "POST",
            "/api/chats/qa%20raw!/move",
            &headers(&f),
            &oversized
        )
        .await
        .0,
        400
    );
    let body = r#"{"projectId":"free","frame":{"kind":"idea","tags":[],"links":[],"confirmed":true,"source":"user"}}"#;
    assert_eq!(
        call(
            f.addr,
            "POST",
            "/api/chats/qa%20raw!/move",
            &headers(&f),
            body
        )
        .await
        .0,
        200
    );
    let first =
        server_projects::chat_move::grant_context(&f.data, &f.user, "qaraw", START).unwrap();
    assert_eq!(
        first.holder,
        server_projects::chat_move::Holder::Project("free".into())
    );
    assert_eq!(
        call(
            f.addr,
            "POST",
            "/api/chats/qa%20raw!/move",
            &headers(&f),
            r#"{"projectId":"free","frame":null}"#
        )
        .await
        .0,
        200
    );
    assert_eq!(
        server_projects::chat_move::grant_context(&f.data, &f.user, "qaraw", START)
            .unwrap()
            .generation,
        first.generation
    );
    for (method, path) in [
        ("GET", "/api/chats/qa/move"),
        ("PUT", "/api/chats/qa/move"),
        ("POST", "/api/chats/qa/move/"),
        ("POST", "/api/chats//move"),
    ] {
        assert!(
            noevia_server::project_routes::route(method, path).is_none(),
            "{method} {path}"
        );
    }
}
#[tokio::test]
async fn full_cap_and_busy_refuse_without_metadata_loss_and_busy_has_retry_header() {
    let f = front().await;
    seed(&f.user_dir());
    let items = (0..1000)
        .map(|i| serde_json::json!({"id":format!("qa-{i}"),"updatedAt":100}))
        .collect::<Vec<_>>();
    std::fs::write(
        f.user_dir().join("projects.json"),
        serde_json::json!({"projects":[{"id":"qa-p","chats":items}]}).to_string(),
    )
    .unwrap();
    let before = lists(&f.user_dir());
    let (status, _, answer) = call(
        f.addr,
        "POST",
        "/api/chats/qa%20raw!/move",
        &headers(&f),
        r#"{"projectId":"qa-p"}"#,
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&answer).unwrap()["code"],
        "CHAT_DESTINATION_FULL"
    );
    assert_eq!(lists(&f.user_dir()), before);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(f.user_dir().join("projects.json.lock"))
        .unwrap();
    file.lock().unwrap();
    let (status, head, answer) = call(
        f.addr,
        "POST",
        "/api/chats/qa%20raw!/move",
        &headers(&f),
        r#"{"projectId":null,"frame":null}"#,
    )
    .await;
    assert_eq!(status, 503, "{answer}");
    assert!(head.contains("retry-after: 1"));
    assert!(head.contains("application/json"));
    assert_eq!(lists(&f.user_dir()), before);
    file.unlock().unwrap();
}
#[tokio::test]
async fn session_or_account_removed_while_reading_body_cannot_move_or_recreate() {
    for remove in [false, true] {
        let f = front().await;
        seed(&f.user_dir());
        let before = lists(&f.user_dir());
        let (status, answer) = call_slow(
            f.addr,
            "POST",
            "/api/chats/qa%20raw!/move",
            &headers(&f),
            r#"{"projectId":"qa-p"}"#,
            || {
                let c = Connection::open(server_store::db_path(&f.data)).unwrap();
                c.execute("DELETE FROM sessions", []).unwrap();
                if remove {
                    std::fs::remove_dir_all(f.user_dir()).unwrap();
                }
            },
        )
        .await;
        assert_eq!(status, if remove { 410 } else { 401 }, "{answer}");
        if remove {
            assert!(!f.user_dir().exists());
        } else {
            assert_eq!(lists(&f.user_dir()), before);
        }
    }
}
