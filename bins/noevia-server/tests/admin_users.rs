//! sbstndalton/noevia#2190, #2191 round 9 `rust-admin-users`: `DELETE /api/admin/users/{id}` through
//! the whole front, with Node absent (an unreachable upstream: anything proxied is a 502), a real
//! cowork.db and a synthetic Diary sidecar. The broker halves (MCP, ChatGPT) are tested beside the
//! brokers' own fixtures (mcp_credentials.rs); this file covers the gate, the body, the database,
//! the folder and the Diary tenant. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const C: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const ADMIN: &str = "cowork_session=admin-token; cowork_csrf=csrf";
const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";
const OTHER_ADMIN: &str = "cowork_session=other-admin-token; cowork_csrf=csrf";
const TENANT_KEY: &str = "synthetic-tenant-key-0123456789abcdef";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The accounts: A and C administrators, B a member; each with a session, a folder and a file.
fn seed(dir: &Path) -> Connection {
    let db = Connection::open(server_store::db_path(dir)).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, name, role, token) in [
        (A, "Alpha", "admin", "admin-token"),
        (B, "Beta", "member", "member-token"),
        (C, "Gamma", "admin", "other-admin-token"),
    ] {
        db.execute(
            "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?2,lower(?2),?2,?3,'x',?1,1,1)",
            (uid, name, role),
        )
        .unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
        let folder = dir.join("users").join(uid);
        std::fs::create_dir_all(folder.join("project-sources").join("p1")).unwrap();
        std::fs::write(folder.join("projects.json"), r#"{"projects":[]}"#).unwrap();
        std::fs::write(
            folder.join("project-sources").join("p1").join("t.txt"),
            "synthetic text",
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO invitations(token_hash,created_by,role,expires_at,created_at) VALUES('inv',?1,'member',9999999999999,1)",
        [B],
    )
    .unwrap();
    db
}

/// One synthetic Diary sidecar: every request is recorded and answered `{"ok":true}`.
#[derive(Default)]
struct Sidecar {
    seen: Mutex<Vec<String>>,
}

async fn sidecar() -> (SocketAddr, Arc<Sidecar>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(Sidecar::default());
    let keep = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let state = Arc::clone(&keep);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                let n = s.read(&mut buf).await.unwrap_or(0);
                state
                    .seen
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let body = r#"{"ok":true}"#;
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    (addr, state)
}

struct Front {
    addr: SocketAddr,
    _app: Arc<noevia_server::App>,
}

async fn front(dir: &Path, diary: Option<SocketAddr>) -> Front {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut env: HashMap<&str, String> = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
    ]);
    if let Some(d) = diary {
        env.insert("DIARY_BASE_URL", format!("http://{d}"));
        env.insert("DIARY_AUTH_TOKEN", "synthetic-diary-token".into());
        env.insert("DIARY_TENANT_KEY", TENANT_KEY.into());
    }
    let app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    tokio::spawn(serve(
        listener,
        Arc::clone(&app),
        Limits::default(),
        std::future::pending(),
    ));
    Front { addr, _app: app }
}

async fn delete(
    f: &Front,
    id: &str,
    cookie: &str,
    csrf: bool,
    body: &str,
) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    let extra = if csrf {
        format!("Origin: http://{}\r\nX-CSRF-Token: csrf\r\n", f.addr)
    } else {
        String::new()
    };
    s.write_all(
        format!(
            "DELETE /api/admin/users/{id} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n{body}",
            f.addr,
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (
        status,
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
}

fn users(db: &Connection) -> Vec<String> {
    let mut st = db.prepare("SELECT id FROM users ORDER BY id").unwrap();
    st.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn folder(dir: &Path, id: &str) -> PathBuf {
    dir.join("users").join(id)
}

#[tokio::test]
async fn the_gate_and_the_body_come_before_anything_is_deleted() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let f = front(d.path(), None).await;
    // Signed out, a member, and an administrator without the CSRF token.
    assert_eq!(
        delete(&f, B, "", false, r#"{"username":"Beta"}"#).await.0,
        401
    );
    let (s, body) = delete(&f, B, MEMBER, true, r#"{"username":"Beta"}"#).await;
    assert_eq!(
        (s, body["error"].as_str()),
        (403, Some("administrator required"))
    );
    assert_eq!(
        delete(&f, B, ADMIN, false, r#"{"username":"Beta"}"#)
            .await
            .0,
        403
    );
    // Core's body errors, in its order: a bad escape, bad JSON, a body that is not an object.
    let (s, body) = delete(&f, "%E0%A4%A", ADMIN, true, "{}").await;
    assert_eq!((s, body["error"].as_str()), (400, Some("URI malformed")));
    let (s, body) = delete(&f, B, ADMIN, true, "not json").await;
    assert_eq!((s, body["error"].as_str()), (400, Some("invalid JSON")));
    for not_object in ["null", "[]", "5", r#""Beta""#] {
        let (s, body) = delete(&f, B, ADMIN, true, not_object).await;
        assert_eq!(
            (s, body["error"].as_str()),
            (400, Some("request body must be a JSON object")),
            "{not_object}"
        );
    }
    let big = format!(r#"{{"username":"{}"}}"#, "x".repeat(1024 * 1024));
    let (s, body) = delete(&f, B, ADMIN, true, &big).await;
    assert_eq!(
        (s, body["error"].as_str()),
        (400, Some("Request exceeds size limit"))
    );
    // A name that is not the account's (or no name), and an account that is not there: { ok: false }.
    for (id, typed) in [
        (B, r#"{"username":"beta"}"#),
        (B, r#"{"username":"Beta "}"#),
        (B, r#"{}"#),
        (B, ""),
        (B, r#"{"username":5}"#),
        (
            "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
            r#"{"username":"Beta"}"#,
        ),
    ] {
        let (s, body) = delete(&f, id, ADMIN, true, typed).await;
        assert_eq!(
            (s, body["ok"].as_bool()),
            (400, Some(false)),
            "{id} {typed}"
        );
    }
    assert_eq!(users(&db), vec![A, B, C]);
    assert!(folder(d.path(), B).join("projects.json").exists());
    let audits: i64 = db
        .query_row(
            "SELECT count(*) FROM audit_events WHERE action='user.delete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audits, 0, "a refusal leaves no trace");
}

#[tokio::test]
async fn an_account_goes_with_its_rows_its_folder_and_its_diary_tenant() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let (diary, seen) = sidecar().await;
    let f = front(d.path(), Some(diary)).await;
    let (s, body) = delete(&f, B, ADMIN, true, r#"{"username":"Beta"}"#).await;
    assert_eq!((s, body["ok"].as_bool()), (200, Some(true)), "{body}");
    assert_eq!(users(&db), vec![A, C]);
    let n = |sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        n(&format!(
            "SELECT count(*) FROM sessions WHERE user_id='{B}'"
        )),
        0
    );
    assert_eq!(
        n("SELECT count(*) FROM sessions"),
        2,
        "the others stay signed in"
    );
    assert_eq!(
        n("SELECT count(*) FROM invitations"),
        0,
        "what it issued is revoked"
    );
    let (action, actor, target): (String, String, String) = db
        .query_row(
            "SELECT action, actor_user_id, target_user_id FROM audit_events WHERE action='user.delete'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (action.as_str(), actor.as_str(), target.as_str()),
        ("user.delete", A, B)
    );
    assert!(!folder(d.path(), B).exists(), "the whole folder is gone");
    assert!(
        folder(d.path(), C).join("projects.json").exists(),
        "another account's is not"
    );
    // The deleted account is signed out at once.
    let (s, _) = delete(&f, C, MEMBER, true, r#"{"username":"Gamma"}"#).await;
    assert_eq!(s, 401);
    // The Diary tenant: one DELETE as that tenant, bearer + id + assertion, and no storage.
    let calls = seen.seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let call = calls[0].to_ascii_lowercase();
    assert!(
        call.starts_with("delete /api/internal/tenant http/1.1"),
        "{call}"
    );
    assert!(call.contains("authorization: bearer synthetic-diary-token"));
    assert!(call.contains(&format!("x-cowork-user-id: {B}")));
    assert!(
        !call.contains("x-cowork-storage"),
        "tenant deletion carries no storage: {call}"
    );
    assert!(!call.contains("x-cowork-legacy-owner"));
    let assertion = calls[0]
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .starts_with("x-cowork-tenant-assertion:")
                .then(|| l.split_once(':').unwrap().1.trim().to_string())
        })
        .expect("a signed tenant assertion");
    let parts: Vec<&str> = assertion.split('.').collect();
    assert_eq!(parts[0], "v2");
    let signed = tenant_assertion::sign(
        &tenant_assertion::Request {
            key: TENANT_KEY,
            user_id: B,
            assertion: "",
            method: "DELETE",
            path: "/api/internal/tenant",
            query: b"",
            body_hash: &hex(""),
            storage: "",
            legacy_owner: "",
            blocked: "",
            now: 0.0,
        },
        parts[1].parse().unwrap(),
        parts[2],
    )
    .unwrap();
    assert_eq!(
        signed, assertion,
        "signed over the tenant, the target and an empty body"
    );
}

#[tokio::test]
async fn the_last_active_administrator_stays_and_a_sidecar_that_is_down_does_not_undo_a_deletion() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    // A sidecar nobody listens on: the deletion still completes.
    let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
    let f = front(d.path(), Some(dead)).await;
    // Two administrators: C may go (a disabled one first, then the active one is the last).
    db.execute("UPDATE users SET disabled_at=7 WHERE id=?1", [C])
        .unwrap();
    let (s, body) = delete(&f, A, ADMIN, true, r#"{"username":"Alpha"}"#).await;
    assert_eq!(
        (s, body["error"].as_str()),
        (400, Some("cannot delete the last active administrator"))
    );
    assert_eq!(users(&db), vec![A, B, C]);
    assert!(folder(d.path(), A).exists());
    let (s, body) = delete(&f, C, ADMIN, true, r#"{"username":"Gamma"}"#).await;
    assert_eq!((s, body["ok"].as_bool()), (200, Some(true)), "{body}");
    assert_eq!(users(&db), vec![A, B]);
    // Now A is the only administrator: it cannot delete itself either.
    let (s, body) = delete(&f, A, ADMIN, true, r#"{"username":"Alpha"}"#).await;
    assert_eq!(
        (s, body["error"].as_str()),
        (400, Some("cannot delete the last active administrator"))
    );
    assert_eq!(users(&db), vec![A, B]);
}

#[tokio::test]
async fn an_administrator_may_delete_itself_when_another_is_active_and_is_signed_out() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let f = front(d.path(), None).await;
    let (s, body) = delete(&f, A, ADMIN, true, r#"{"username":"Alpha"}"#).await;
    assert_eq!((s, body["ok"].as_bool()), (200, Some(true)), "{body}");
    assert_eq!(users(&db), vec![B, C]);
    assert!(!folder(d.path(), A).exists());
    let (s, _) = delete(&f, B, ADMIN, true, r#"{"username":"Beta"}"#).await;
    assert_eq!(s, 401, "the deleted administrator's session is gone");
}

#[tokio::test]
async fn a_client_that_goes_away_after_sending_the_request_cannot_leave_an_account_half_deleted() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    // The sidecar holds its answer: the cleanup is still waiting on it when the client has gone.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let diary = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let seen_one = Arc::new(Mutex::new(false));
    let seen = Arc::clone(&seen_one);
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let _ = s.read(&mut buf).await;
        *seen.lock().unwrap() = true;
        let _ = rx.await;
        let _ = s
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")
            .await;
    });
    let f = front(d.path(), Some(diary)).await;
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    let body = r#"{"username":"Beta"}"#;
    s.write_all(
        format!(
            "DELETE /api/admin/users/{B} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nCookie: {ADMIN}\r\nOrigin: http://{}\r\nX-CSRF-Token: csrf\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            f.addr, f.addr, body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    // Wait until the sidecar has the tenant call, then hang up before any answer.
    for _ in 0..200 {
        if *seen_one.lock().unwrap() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(*seen_one.lock().unwrap(), "the cleanup reached the sidecar");
    drop(s);
    let _ = tx.send(());
    // The account is gone whole: the row and the folder, though nobody is waiting for the answer.
    for _ in 0..200 {
        if !folder(d.path(), B).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(users(&db), vec![A, C]);
    assert!(!folder(d.path(), B).exists());
}

#[tokio::test]
async fn an_administrator_disabled_between_the_gate_and_the_transaction_deletes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let f = front(d.path(), None).await;
    // Gamma passes both gates; then another administrator disables Gamma, before the delete's transaction
    // opens. The transaction checks the acting administrator again. (The hook is keyed by the actor, so the
    // other tests, whose actor is Alpha, are not touched.)
    let path = server_store::db_path(d.path());
    noevia_server::admin_user_routes::hooks::on_before_write(C, move || {
        let other = Connection::open(&path).unwrap();
        other
            .execute("UPDATE users SET disabled_at=7 WHERE id=?1", [C])
            .unwrap();
    });
    let (s, body) = delete(&f, B, OTHER_ADMIN, true, r#"{"username":"Beta"}"#).await;
    assert_eq!(
        (s, body["error"].as_str()),
        (403, Some("administrator required"))
    );
    assert_eq!(users(&db), vec![A, B, C], "nothing was deleted");
    assert!(folder(d.path(), B).join("projects.json").exists());
}
