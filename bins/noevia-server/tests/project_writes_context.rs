//! sbstndalton/noevia#1264 slice 1: `GET`/`POST /api/chats/{chatId}/context` through the whole
//! front. Ownership is permanent, including absent or hostile retired deployment keys.
//! Node is not running here. Synthetic data only.
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
    let f = front_new_account(writes).await;
    // Node's workspace load made the directory on the user's first request.
    std::fs::create_dir_all(f.user_dir()).unwrap();
    f
}

/// A signed-in account that has made no request yet: its directory does not exist.
async fn front_new_account(writes: bool) -> Front {
    front_with_retired_value(writes.then_some("1")).await
}

async fn front_with_retired_value(value: Option<&str>) -> Front {
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
    if let Some(value) = value {
        env.insert("NOEVIA_RUST_PROJECT_WRITES", value.into());
        env.insert("NOEVIA_RUST_PROJECT_WRITES_CONFIRMED", value.into());
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

fn projects_json(f: &Front) -> Option<String> {
    std::fs::read_to_string(f.user_dir().join("projects.json")).ok()
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

fn insert_second_admin_session(f: &Front) -> (String, String, String) {
    let user = "33333333-2222-4333-8444-555555555555";
    let token = "synthetic-second-admin-session-token";
    let csrf = "synthetic-second-admin-csrf-token";
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute(
        "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,'second-admin','second-admin','Synthetic second admin','admin','x',?1,1,1)",
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
        csrf.to_string(),
    )
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
    let returned: serde_json::Value = serde_json::from_str(&body).unwrap();
    let saved: serde_json::Value = serde_json::from_str(&stored).unwrap();
    for project in [&returned["project"], &saved["projects"][0]] {
        assert_eq!(project["routing"], "auto");
        assert_eq!(project["toolsMode"], "auto");
        assert_eq!(project["toolboxes"], serde_json::json!(["core"]));
    }
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
async fn the_same_chat_id_is_isolated_between_accounts_in_one_data_root() {
    let f = front(true).await;
    let (status, _, body) = call(
        f.addr,
        "POST",
        "/api/chats/shared/context",
        &headers(&f),
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let owner_before = projects_json(&f).unwrap();

    let (second_user, second_cookie, second_csrf) = insert_second_admin_session(&f);
    let other_headers = [
        ("cookie", second_cookie.as_str()),
        ("x-csrf-token", second_csrf.as_str()),
        ("origin", ORIGIN),
    ];
    let second_dir = f.data.join("users").join(&second_user);
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/chats/shared/context",
        &other_headers,
        "",
    )
    .await;
    assert_eq!((status, body.as_str()), (200, r#"{"project":null}"#));
    assert_eq!(
        std::fs::read_to_string(f.user_dir().join("projects.json")).unwrap(),
        owner_before
    );

    let (status, _, body) = call(
        f.addr,
        "POST",
        "/api/chats/shared/context",
        &other_headers,
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(created["project"]["id"], "cowork-chat-context-shared");
    let second_projects: serde_json::Value =
        serde_json::from_slice(&std::fs::read(second_dir.join("projects.json")).unwrap()).unwrap();
    assert_eq!(second_projects["projects"].as_array().unwrap().len(), 1);
    assert_eq!(
        second_projects["projects"][0]["id"],
        "cowork-chat-context-shared"
    );
    assert_eq!(
        std::fs::read_to_string(f.user_dir().join("projects.json")).unwrap(),
        owner_before
    );
}

#[tokio::test]
async fn a_tombstone_blocks_only_creation_and_get_never_recreates_context() {
    let f = front(true).await;
    let dir = f.user_dir();
    let empty = "{\n  \"projects\": []\n}\n";
    std::fs::write(dir.join("projects.json"), empty).unwrap();
    std::fs::write(dir.join("deleted-chats.json"), r#"["gone"]"#).unwrap();

    let (status, _, body) = call(f.addr, "POST", "/api/chats/gone/context", &headers(&f), "").await;
    assert_eq!(
        (status, body.as_str()),
        (410, r#"{"error":"This chat was deleted."}"#)
    );
    let (status, _, body) = call(f.addr, "GET", "/api/chats/gone/context", &headers(&f), "").await;
    assert_eq!((status, body.as_str()), (200, r#"{"project":null}"#));
    assert_eq!(
        std::fs::read_to_string(dir.join("projects.json")).unwrap(),
        empty
    );

    let existing = r#"{"projects":[{"id":"cowork-chat-context-gone","name":"Existing tombstoned context","modes":["chat"],"routing":"auto","toolboxes":["core"],"chats":[],"instructionSkills":{}}]}"#;
    std::fs::write(dir.join("projects.json"), existing).unwrap();
    let (status, _, body) = call(f.addr, "POST", "/api/chats/gone/context", &headers(&f), "").await;
    assert_eq!(status, 200, "{body}");
    let returned: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(returned["project"]["id"], "cowork-chat-context-gone");
    assert_eq!(returned["project"]["name"], "Existing tombstoned context");
    assert_eq!(
        std::fs::read_to_string(dir.join("projects.json")).unwrap(),
        existing
    );
}

#[tokio::test]
async fn without_the_retired_switch_the_context_routes_are_native() {
    let f = front(false).await;
    let (status, _, body) = call(f.addr, "POST", "/api/chats/c1/context", &headers(&f), "").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("cowork-chat-context-c1"), "{body}");
    let (status, _, body) = call(f.addr, "GET", "/api/chats/c1/context", &headers(&f), "").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("cowork-chat-context-c1"), "{body}");
    assert!(projects_json(&f).is_some());
}

fn mode_of(p: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// sbstndalton/noevia#2026: core's workspace load makes `users/<id>` (mode 0700) on a signed-in
/// user's first request, so the first context POST of a new account is Node's 200, not a 410.
#[tokio::test]
async fn a_new_accounts_first_context_post_creates_its_directory() {
    let f = front_new_account(true).await;
    let (status, _, body) = call(f.addr, "POST", "/api/chats/c1/context", &headers(&f), "").await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.starts_with(
            r#"{"project":{"id":"cowork-chat-context-c1","name":"Chat attachments c1","#
        ),
        "{body}"
    );
    assert!(f.user_dir().is_dir());
    assert_eq!(mode_of(&f.user_dir()) & 0o077, 0, "never wider than 0700");
    assert!(projects_json(&f)
        .unwrap()
        .contains(r#""id": "cowork-chat-context-c1""#));
    // The account's own directory only.
    let users: Vec<_> = std::fs::read_dir(f.data.join("users"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(users, vec![f.user.clone()]);
}

#[tokio::test]
async fn a_new_accounts_first_context_get_answers_null_and_makes_the_directory() {
    let f = front_new_account(true).await;
    let (status, _, body) = call(f.addr, "GET", "/api/chats/c1/context", &headers(&f), "").await;
    assert_eq!((status, body.as_str()), (200, r#"{"project":null}"#));
    assert!(f.user_dir().is_dir());
    assert_eq!(
        projects_json(&f),
        None,
        "a GET of a missing project writes nothing"
    );
}

/// Node's workspace load runs before the route reads the chat id, so a malformed id from a signed-in
/// new account is the 400 and the directory exists after it, as it does on Node.
#[tokio::test]
async fn a_malformed_id_from_a_new_account_is_400_and_creates_only_its_own_directory() {
    let f = front_new_account(true).await;
    for (m, p) in [
        ("POST", "/api/chats/bad%E0%A4%A/context"),
        ("GET", "/api/chats/a.b/context"),
        // Traversal in the id is an invalid chat id; it never reaches a path.
        ("POST", "/api/chats/..%2F..%2Fx/context"),
    ] {
        let (status, _, body) = call(f.addr, m, p, &headers(&f), "").await;
        assert_eq!(
            (status, body.as_str()),
            (400, r#"{"error":"Invalid chat identifier"}"#),
            "{m} {p}"
        );
    }
    assert!(f.user_dir().is_dir());
    assert_eq!(projects_json(&f), None);
    let entries: Vec<_> = std::fs::read_dir(&f.data)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(!entries.iter().any(|n| n == "x"), "{entries:?}");
}

/// No session, a session without the CSRF token, and a cross-origin write each stop at the gate:
/// no directory is made for them.
#[tokio::test]
async fn a_request_the_gate_refuses_makes_no_directory() {
    let f = front_new_account(true).await;
    for m in ["GET", "POST"] {
        let (status, _, _) = call(
            f.addr,
            m,
            "/api/chats/c1/context",
            &[("origin", ORIGIN)],
            "",
        )
        .await;
        assert_eq!(status, 401, "{m}: no session");
    }
    let (status, _, _) = call(
        f.addr,
        "POST",
        "/api/chats/c1/context",
        &[("cookie", f.jar.as_str()), ("origin", ORIGIN)],
        "",
    )
    .await;
    assert_eq!(status, 403, "no CSRF token");
    let (status, _, _) = call(
        f.addr,
        "POST",
        "/api/chats/c1/context",
        &[
            ("cookie", f.jar.as_str()),
            ("x-csrf-token", f.csrf.as_str()),
            ("origin", "http://evil.invalid"),
        ],
        "",
    )
    .await;
    assert_eq!(status, 403, "foreign origin");
    assert!(
        !f.data.join("users").exists(),
        "nothing made for a refused request"
    );
    assert_eq!(projects_json(&f), None);
}

/// A removed account (its session gone with it) is not brought back by a late request.
#[tokio::test]
async fn a_removed_accounts_directory_is_not_recreated() {
    let f = front_new_account(true).await;
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute("DELETE FROM sessions", []).unwrap();
    let (status, _, _) = call(f.addr, "POST", "/api/chats/c1/context", &headers(&f), "").await;
    assert_eq!(status, 401);
    assert!(!f.data.join("users").exists());
}

/// The account is removed after the first gate and before the directory is made (Node refuses it
/// in process once `removed` holds the id): the answer is the removed account's 410 and no
/// directory is left behind, not an orphan `users/<id>` the removal had already swept.
#[cfg(debug_assertions)]
#[tokio::test]
async fn an_account_removed_between_the_gate_and_the_directory_leaves_nothing() {
    let f = front_new_account(true).await;
    let (data, user) = (f.data.clone(), f.user.clone());
    noevia_server::project_routes::hooks::on_before_ensure(&f.user, move || {
        // core deleteUser, then workspaceStore.remove (the directory never existed here).
        let c = Connection::open(server_store::db_path(&data)).unwrap();
        c.execute("DELETE FROM sessions WHERE user_id = ?1", [&user])
            .unwrap();
        c.execute("DELETE FROM users WHERE id = ?1", [&user])
            .unwrap();
        let _ = std::fs::remove_dir_all(data.join("users").join(&user));
    });
    for (m, p) in [("POST", "/api/chats/c1/context")] {
        let (status, _, body) = call(f.addr, m, p, &headers(&f), "").await;
        assert_eq!(
            (status, body.as_str()),
            (410, r#"{"error":"account no longer exists"}"#),
            "{m} {p}"
        );
    }
    assert!(
        !f.user_dir().exists(),
        "the removed account's directory stays gone"
    );
    assert_eq!(projects_json(&f), None);
}

/// A directory that was already there is not this request's to take away, whatever the gate says
/// on the second pass.
#[cfg(debug_assertions)]
#[tokio::test]
async fn an_existing_directory_is_never_removed_by_the_second_gate() {
    let f = front(true).await;
    let (data, user) = (f.data.clone(), f.user.clone());
    noevia_server::project_routes::hooks::on_before_ensure(&f.user, move || {
        let c = Connection::open(server_store::db_path(&data)).unwrap();
        c.execute("DELETE FROM sessions WHERE user_id = ?1", [&user])
            .unwrap();
    });
    let (status, _, _) = call(f.addr, "POST", "/api/chats/c1/context", &headers(&f), "").await;
    assert_eq!(status, 401);
    assert!(f.user_dir().is_dir());
}

/// The first signed-in administrator claims the root before the context route proceeds.
#[tokio::test]
async fn the_first_admin_claims_legacy_root_before_serving_context() {
    let f = front_new_account(true).await;
    std::fs::write(f.data.join("projects.json"), r#"{"projects":[]}"#).unwrap();
    let (status, _, body) = call(f.addr, "GET", "/api/chats/c1/context", &headers(&f), "").await;
    assert_eq!((status, body.as_str()), (200, r#"{"project":null}"#));
    let migration: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.user_dir().join("migration.json")).unwrap())
            .unwrap();
    assert_eq!(migration["source"], "legacy-root");
    let owner: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.data.join("legacy-owner.json")).unwrap()).unwrap();
    assert_eq!(owner["userId"], f.user);
    assert!(
        f.data.join("projects.json").is_file(),
        "source retained for recovery"
    );
    let (status, _, body) = call(f.addr, "POST", "/api/chats/c1/context", &headers(&f), "").await;
    assert_eq!(status, 200, "{body}");

    let g = front_new_account(true).await;
    std::fs::write(g.data.join("projects.json"), r#"{"projects":[]}"#).unwrap();
    std::fs::write(
        g.data.join("legacy-owner.json"),
        r#"{"userId":"99999999-2222-4333-8444-555555555555"}"#,
    )
    .unwrap();
    let (status, _, body) = call(g.addr, "POST", "/api/chats/c1/context", &headers(&g), "").await;
    assert_eq!(
        status, 200,
        "mismatched ownership allows an empty account directory: {body}"
    );
    assert!(g.user_dir().is_dir());
    assert!(!g.user_dir().join("migration.json").exists());
    assert!(g.data.join("projects.json").is_file());
    let owner: serde_json::Value =
        serde_json::from_slice(&std::fs::read(g.data.join("legacy-owner.json")).unwrap()).unwrap();
    assert_eq!(owner["userId"], "99999999-2222-4333-8444-555555555555");
}

/// A member cannot take the first-user legacy claim; only an administrator may do so.
#[tokio::test]
async fn a_non_admin_cannot_claim_a_pending_legacy_root() {
    let f = front_new_account(true).await;
    std::fs::write(f.data.join("projects.json"), r#"{"projects":[]}"#).unwrap();
    let (_, jar) = insert_member_session(&f);
    let headers = [("cookie", jar.as_str()), ("origin", ORIGIN)];
    let (status, _, body) = call(f.addr, "GET", "/api/chats/c1/context", &headers, "").await;
    assert_eq!(status, 503, "non-admin cannot run claim: {body}");
    assert!(!f.data.join("users").exists());
    assert!(!f.data.join("legacy-owner.json").exists());
    assert!(f.data.join("projects.json").is_file());
}

/// Node's deleteUser owns the auth writer first, so a concurrent native claim waits and then
/// revalidates the deleted session before touching legacy files or creating a user directory.
#[cfg(debug_assertions)]
#[tokio::test]
async fn a_concurrent_account_delete_wins_before_the_legacy_claim() {
    let f = front_new_account(true).await;
    std::fs::write(f.data.join("projects.json"), r#"{"projects":[]}"#).unwrap();
    let (data, user) = (f.data.clone(), f.user.clone());
    let worker = std::sync::Arc::new(std::sync::Mutex::new(None));
    let hook_worker = std::sync::Arc::clone(&worker);
    noevia_server::project_routes::hooks::on_before_ensure(&f.user, move || {
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let data = data.clone();
        let user = user.clone();
        let deleter = std::thread::spawn(move || {
            let c = Connection::open(server_store::db_path(&data)).unwrap();
            c.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(150));
            c.execute("DELETE FROM sessions WHERE user_id = ?1", [&user])
                .unwrap();
            c.execute("DELETE FROM users WHERE id = ?1", [&user])
                .unwrap();
            c.execute_batch("COMMIT").unwrap();
            let _ = std::fs::remove_dir_all(data.join("users").join(&user));
        });
        locked_rx.recv().unwrap();
        *hook_worker.lock().unwrap() = Some(deleter);
    });
    let (status, _, _) = call(f.addr, "GET", "/api/chats/c1/context", &headers(&f), "").await;
    if let Some(deleter) = worker.lock().unwrap().take() {
        deleter.join().unwrap();
    }
    assert_eq!(status, 503);
    assert!(!f.data.join("users").exists());
    assert!(!f.data.join("legacy-owner.json").exists());
    assert!(f.data.join("projects.json").is_file());
}

#[tokio::test]
async fn hostile_retired_values_cannot_disable_native_context() {
    for value in ["", "0", "true", " 1", "hostile"] {
        let f = front_with_retired_value(Some(value)).await;
        let (status, _, body) =
            call(f.addr, "POST", "/api/chats/c1/context", &headers(&f), "").await;
        assert_eq!(status, 200, "{value:?}: {body}");
        assert!(body.contains("cowork-chat-context-c1"), "{value:?}: {body}");
        let (status, _, body) =
            call(f.addr, "GET", "/api/chats/c1/context", &headers(&f), "").await;
        assert_eq!(status, 200, "{value:?}: {body}");
        assert!(body.contains("cowork-chat-context-c1"), "{value:?}: {body}");
        assert!(projects_json(&f).is_some());
    }
}
