//! sbstndalton/noevia#1425: a project write whose account is removed while its body is still
//! arriving (the gate ran before the body was read) writes nothing and never recreates the
//! removed user's directory. The front re-runs the gate before the write, and the store never
//! makes the user's directory. Synthetic data only. Also the chat lists' other save failures
//! (#1406/#1419): a busy store is 503 with Retry-After, a null body 400 `chats array required`.
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
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-removed-account-code";
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
    fn revoke_sessions(&self) {
        let c = Connection::open(server_store::db_path(&self.data)).unwrap();
        c.execute("DELETE FROM sessions", []).unwrap();
    }
}

/// One HTTP/1.1 exchange over a raw socket, with `between` run once the front has gated the
/// request: the headers go out with `Expect: 100-continue` and no body, and the front answers
/// `100 Continue` only when it starts reading the body, which it does after the gate
/// (project_routes.rs `serve`). Then `between`, then the body; the status and body text.
/// sbstndalton/noevia#1942: this used to send half the body and sleep 300 ms, assuming the gate
/// had run by then. On a loaded runner it sometimes had not, so `between` revoked the session
/// before the gate and the front rightly answered 401 instead of the 410 under test.
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

/// sbstndalton/noevia#1406/#1419: a chat-list save that finds the store busy answers as core's
/// saveFailureReply: 503 PROJECTS_BUSY with Retry-After: 1 and Node's headers, never a 400.
#[cfg(any(feature = "rust-free-chats", feature = "rust-project-chats"))]
fn assert_busy_reply((status, head, body): &(u16, String, String)) {
    assert_eq!(*status, 503, "{head}");
    assert_eq!(
        body,
        r#"{"error":"The project store is busy. Try again shortly.","code":"PROJECTS_BUSY"}"#
    );
    for line in [
        "retry-after: 1",
        "content-type: application/json",
        "cache-control: no-store",
        "x-noevia-api: 1",
        "strict-transport-security: max-age=15552000",
    ] {
        assert!(head.lines().any(|l| l == line), "{line} missing in {head}");
    }
}

async fn front() -> Front {
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
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic removed password","diaryEnabled":false}}"#
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

fn write_headers(f: &Front) -> Vec<(&str, &str)> {
    vec![
        ("cookie", f.jar.as_str()),
        ("x-csrf-token", f.csrf.as_str()),
        ("origin", ORIGIN),
        ("content-type", "application/json"),
    ]
}

fn assert_gone(dir: &Path) {
    assert!(!dir.exists(), "{} was recreated", dir.display());
}

const UPLOAD: &str = r#"{"name":"pixel.png","mime":"image/png","dataBase64":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="}"#;

fn seed_project(f: &Front) {
    std::fs::write(
        f.user_dir().join("projects.json"),
        r#"{"projects":[{"id":"proj-synthetic","name":"P","chats":[]}]}"#,
    )
    .unwrap();
}

/// The account is removed (its sessions and its directory, as core's admin delete does) while an
/// image upload's body is in flight: Node's 410, nothing recreated.
#[tokio::test]
async fn an_upload_for_an_account_removed_mid_body_is_gone() {
    let f = front().await;
    seed_project(&f);
    let dir = f.user_dir();
    let (status, body) = call_slow(
        f.addr,
        "POST",
        "/api/projects/proj-synthetic/assets",
        &write_headers(&f),
        UPLOAD,
        || {
            f.revoke_sessions();
            std::fs::remove_dir_all(&dir).unwrap();
        },
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (410, r#"{"error":"account no longer exists"}"#)
    );
    assert_gone(&dir);
}

/// Only the directory is gone (the session still passes): the store refuses, nothing recreated.
#[tokio::test]
async fn an_upload_whose_user_directory_vanished_recreates_nothing() {
    let f = front().await;
    seed_project(&f);
    let dir = f.user_dir();
    let (status, _) = call_slow(
        f.addr,
        "POST",
        "/api/projects/proj-synthetic/assets",
        &write_headers(&f),
        UPLOAD,
        || std::fs::remove_dir_all(&dir).unwrap(),
    )
    .await;
    // projects.json went with the directory: Node's 404 for the project.
    assert_eq!(status, 404);
    assert_gone(&dir);
}

/// A session revoked mid-body (the directory stays): the gate's own 401, nothing written.
#[tokio::test]
async fn an_upload_from_a_session_revoked_mid_body_is_refused() {
    let f = front().await;
    seed_project(&f);
    let before = std::fs::read_to_string(f.user_dir().join("projects.json")).unwrap();
    let (status, body) = call_slow(
        f.addr,
        "POST",
        "/api/projects/proj-synthetic/assets",
        &write_headers(&f),
        UPLOAD,
        || f.revoke_sessions(),
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (401, r#"{"error":"unauthorized"}"#)
    );
    assert_eq!(
        std::fs::read_to_string(f.user_dir().join("projects.json")).unwrap(),
        before
    );
    assert!(!f.user_dir().join("project-assets").exists());
}

#[cfg(feature = "rust-free-chats")]
mod free_chats {
    use super::*;

    const SAVE: &str =
        r#"{"chats":[{"id":"free-synthetic","title":"Synthetic","updatedAt":5,"preview":"x"}]}"#;

    #[tokio::test]
    async fn a_free_chat_save_for_an_account_removed_mid_body_is_gone() {
        let f = front().await;
        let dir = f.user_dir();
        let (status, body) = call_slow(
            f.addr,
            "POST",
            "/api/freechats",
            &write_headers(&f),
            SAVE,
            || {
                f.revoke_sessions();
                std::fs::remove_dir_all(&dir).unwrap();
            },
        )
        .await;
        assert_eq!(
            (status, body.as_str()),
            (410, r#"{"error":"account no longer exists"}"#)
        );
        assert_gone(&dir);
    }

    #[tokio::test]
    async fn a_free_chat_save_whose_user_directory_vanished_is_gone() {
        let f = front().await;
        let dir = f.user_dir();
        let (status, body) = call_slow(
            f.addr,
            "POST",
            "/api/freechats",
            &write_headers(&f),
            SAVE,
            || std::fs::remove_dir_all(&dir).unwrap(),
        )
        .await;
        assert_eq!(
            (status, body.as_str()),
            (410, r#"{"error":"account no longer exists"}"#)
        );
        assert_gone(&dir);
    }

    #[tokio::test]
    async fn a_free_chat_save_on_a_busy_store_is_a_503_with_retry_after() {
        let f = front().await;
        let file = f.user_dir().join("free-chats.json");
        let held = server_projects::lock::acquire(&file).unwrap();
        let reply = call(f.addr, "POST", "/api/freechats", &write_headers(&f), SAVE).await;
        drop(held);
        assert_busy_reply(&reply);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn a_free_chat_save_still_works_for_a_live_account() {
        let f = front().await;
        let (status, body) = call_slow(
            f.addr,
            "POST",
            "/api/freechats",
            &write_headers(&f),
            SAVE,
            || {},
        )
        .await;
        assert_eq!((status, body.as_str()), (200, r#"{"ok":true}"#));
        assert!(
            std::fs::read_to_string(f.user_dir().join("free-chats.json"))
                .unwrap()
                .contains("free-synthetic")
        );
    }
}

#[cfg(feature = "rust-project-chats")]
mod project_chats {
    use super::*;

    #[tokio::test]
    async fn a_chat_list_save_on_a_busy_store_is_a_503_with_retry_after() {
        let f = front().await;
        seed_project(&f);
        let file = f.user_dir().join("projects.json");
        let before = std::fs::read_to_string(&file).unwrap();
        let held = server_projects::lock::acquire(&file).unwrap();
        let reply = call(
            f.addr,
            "POST",
            "/api/projects/proj-synthetic/chats",
            &write_headers(&f),
            r#"{"chats":[{"id":"chat-synthetic","title":"T","updatedAt":5}]}"#,
        )
        .await;
        drop(held);
        assert_busy_reply(&reply);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    #[tokio::test]
    async fn a_null_chat_list_body_is_chats_array_required() {
        let f = front().await;
        seed_project(&f);
        let (status, _, body) = call(
            f.addr,
            "POST",
            "/api/projects/proj-synthetic/chats",
            &write_headers(&f),
            "null",
        )
        .await;
        assert_eq!(
            (status, body.as_str()),
            (400, r#"{"error":"chats array required"}"#)
        );
    }

    #[tokio::test]
    async fn a_chat_list_save_for_an_account_removed_mid_body_is_gone() {
        let f = front().await;
        seed_project(&f);
        let dir = f.user_dir();
        let (status, body) = call_slow(
            f.addr,
            "POST",
            "/api/projects/proj-synthetic/chats",
            &write_headers(&f),
            r#"{"chats":[{"id":"chat-synthetic","title":"T","updatedAt":5}]}"#,
            || {
                f.revoke_sessions();
                std::fs::remove_dir_all(&dir).unwrap();
            },
        )
        .await;
        assert_eq!(
            (status, body.as_str()),
            (410, r#"{"error":"account no longer exists"}"#)
        );
        assert_gone(&dir);
    }
}
