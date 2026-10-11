#![cfg(feature = "rust-reasoning-settings")]
//! Real HTTP through the native front; dead legacy upstream makes fallback fail.
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
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
async fn call(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    extra: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    call_bytes(addr, method, path, cookie, extra, body.as_bytes()).await
}
async fn call_bytes(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    extra: &str,
    body: &[u8],
) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\nContent-Length: {}\r\n{extra}\r\n",body.len()).as_bytes()).await.unwrap();
    // Admin/member refusal may close without consuming a large request body. A write/reset
    // alone never passes: the complete expected HTTP response is still parsed and asserted.
    let write = s.write_all(body).await;
    if let Err(err) = write {
        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
        ));
    }
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        s.read_to_end(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap_or_else(|err| {
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
        0
    });
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    if path.starts_with("/api/reasoning-settings") && !h.starts_with("HTTP/1.1 502") {
        let lower = h.to_ascii_lowercase();
        assert!(lower.contains("content-type: application/json"));
        assert!(lower.contains("cache-control: no-store"));
    }
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(b).unwrap_or(serde_json::Value::Null),
    )
}
#[tokio::test]
async fn native_put_identity_csrf_admin_audit_caps_and_selector_omission() {
    let d = tempfile::tempdir().unwrap();
    let db = Connection::open(d.path().join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, token) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)",(uid,role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86400000),
        )
        .unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let origin = format!("http://{addr}");
    let mut env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR", d.path().display().to_string()),
        ("NOEVIA_RUST_AUTH", "1".into()),
        ("NOEVIA_RUST_REASONING_SETTINGS", "1".into()),
        ("PUBLIC_ORIGIN", origin.clone()),
    ]);
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(serve(
        listener,
        App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap()),
        Limits::default(),
        async {
            let _ = rx.await;
        },
    ));
    let p = "/api/reasoning-settings";
    let admin = "cowork_session=admin-token; cowork_csrf=csrf";
    let member = "cowork_session=member-token; cowork_csrf=csrf";
    let trusted = format!("Origin: {origin}\r\nX-CSRF-Token: csrf\r\n");
    assert_eq!(
        call(addr, "PUT", p, "", &trusted, r#"{"default":"high"}"#)
            .await
            .0,
        401
    );
    assert_eq!(
        call(addr, "PUT", p, admin, "", r#"{"default":"high"}"#)
            .await
            .0,
        403
    );
    assert_eq!(
        call(
            addr,
            "PUT",
            p,
            admin,
            "Origin: http://untrusted.invalid\r\nX-CSRF-Token: csrf\r\n",
            r#"{"default":"high"}"#
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(addr, "PUT", p, member, &trusted, r#"{"default":"high"}"#).await,
        (403, serde_json::json!({"error":"Administrator required"}))
    );
    assert_eq!(
        call(
            addr,
            "PUT",
            p,
            member,
            &format!("{trusted}X-User-Id: {A}\r\n"),
            r#"{"default":"high","userId":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"}"#
        )
        .await
        .0,
        403
    );
    for raw in [
        "[]",
        "0",
        "false",
        r#"{"default":"max"}"#,
        r#"{"default":true}"#,
    ] {
        assert_eq!(
            call(addr, "PUT", p, admin, &trusted, raw).await,
            (
                400,
                serde_json::json!({"error":"default must be default, low or high"})
            )
        );
    }
    for raw in ["", "{", r#"{"default":"low",}"#] {
        assert_eq!(
            call(addr, "PUT", p, admin, &trusted, raw).await,
            (400, serde_json::json!({"error":"invalid JSON"}))
        );
    }
    assert_eq!(
        call(addr, "PUT", p, admin, &trusted, "null").await,
        (500, serde_json::json!({"error":"Internal error"}))
    );
    for effort in ["default", "low", "high"] {
        let raw = serde_json::json!({"default":effort,"extra":1,"userId":B}).to_string();
        assert_eq!(
            call(addr, "PUT", p, admin, &trusted, &raw).await,
            (200, serde_json::json!({"default":effort}))
        );
    }
    let rows:Vec<(String,Option<String>,String,String)>=db.prepare("SELECT actor_user_id,target_user_id,action,detail FROM audit_events WHERE action='reasoning.default' ORDER BY id").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(rows.len(), 3);
    for (row, effort) in rows.iter().zip(["default", "low", "high"]) {
        assert_eq!(
            *row,
            (
                A.into(),
                None,
                "reasoning.default".into(),
                serde_json::json!({"effort":effort}).to_string()
            )
        );
    }
    db.execute_batch("CREATE TRIGGER reject_audit BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic audit refusal'); END;").unwrap();
    assert_eq!(
        call(addr, "PUT", p, admin, &trusted, r#"{"default":"low"}"#).await,
        (500, serde_json::json!({"error":"Internal error"}))
    );
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='reasoning_effort_default'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "high"
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM audit_events WHERE action='reasoning.default'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        3
    );
    db.execute_batch("DROP TRIGGER reject_audit").unwrap();
    for path in [
        "/api/x/../reasoning-settings",
        "/api/x/%2e%2e/reasoning-settings",
    ] {
        assert_eq!(
            call(addr, "PUT", path, admin, &trusted, r#"{"default":"high"}"#)
                .await
                .0,
            200
        );
    }
    let over = format!(
        r#"{{"default":"low","extra":"{}"}}"#,
        "x".repeat(server_reasoning_settings::BODY_LIMIT)
    );
    assert_eq!(call(addr, "PUT", p, admin, &trusted, &over).await.0, 400);
    for method in ["GET", "HEAD", "POST", "PATCH", "DELETE", "OPTIONS"] {
        assert_eq!(call(addr, method, p, admin, &trusted, "").await.0, 502);
    }
    db.execute("UPDATE sessions SET expires_at=0 WHERE user_id=?1", [B])
        .unwrap();
    assert_eq!(
        call(addr, "PUT", p, member, &trusted, r#"{"default":"high"}"#)
            .await
            .0,
        401
    );
    tx.send(()).unwrap();
    task.await.unwrap();
    env.insert("NOEVIA_RUST_REASONING_SETTINGS", "0".into());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(serve(
        listener,
        App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap()),
        Limits::default(),
        async {
            let _ = rx.await;
        },
    ));
    assert_eq!(
        call(addr, "GET", "/api/auth/session", admin, "", "")
            .await
            .0,
        200
    );
    assert_eq!(
        call(addr, "PUT", p, admin, &trusted, r#"{"default":"low"}"#)
            .await
            .0,
        502
    );
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='reasoning_effort_default'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "high"
    );
    tx.send(()).unwrap();
    task.await.unwrap();
}
