#![cfg(feature = "rust-routing-mode")]
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
use std::{collections::HashMap, sync::Arc};
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
    if path.starts_with("/api/routing-mode") && !h.starts_with("HTTP/1.1 502") {
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
async fn native_configuration_identity_device_csrf_tenants_dynamic_feature_and_failures() {
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
    db.execute("INSERT INTO device_grants(id,user_id,client_name,created_at,last_used_at,expires_at,ip,user_agent) VALUES('synthetic-grant',?1,'Synthetic',?2,?2,?3,'127.0.0.1','ua')",(A,now,now+86400000)).unwrap();
    db.execute("INSERT INTO device_tokens(token_hash,grant_id,kind,created_at,expires_at) VALUES(?1,'synthetic-grant','access',?2,?3)",(hex("nva_synthetic-device-token"),now,now+86400000)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let origin = format!("http://{addr}");
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR", d.path().display().to_string()),
        ("TRUST_PROXY", "true".into()),
        ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "1".into()),
        ("PUBLIC_ORIGIN", origin.clone()),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let (stop, rx) = oneshot::channel();
    let task = tokio::spawn(serve(
        listener,
        Arc::clone(&app),
        Limits::default(),
        async {
            let _ = rx.await;
        },
    ));
    let csrf = format!("Origin: {origin}\r\nX-CSRF-Token: csrf\r\n");
    let admin = "cowork_session=admin-token; cowork_csrf=csrf";
    let member = "cowork_session=member-token; cowork_csrf=csrf";
    let main = "/api/routing-mode";
    let allowed = "/api/routing-mode/allowed";
    for p in [main, allowed] {
        for method in ["GET", "PUT", "POST", "PATCH", "DELETE", "OPTIONS"] {
            assert_eq!(call(addr, method, p, "", &csrf, "{").await.0, 401);
        }
    }
    assert_eq!(
        call(addr, "PUT", main, admin, "", r#"{"mode":"local"}"#)
            .await
            .0,
        403
    );
    assert_eq!(
        call(
            addr,
            "PUT",
            main,
            admin,
            "Origin: https://evil.example.test\r\nX-CSRF-Token: csrf\r\n",
            r#"{"mode":"local"}"#
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(addr, "GET", main, admin, "", "").await,
        (200, serde_json::json!({"enabled":false}))
    );
    assert_eq!(call(addr, "PUT", allowed, member, &csrf, "{").await.0, 409);
    for p in [main, allowed] {
        for method in ["POST", "PATCH", "DELETE", "OPTIONS"] {
            assert_eq!(call(addr, method, p, admin, &csrf, "{").await.0, 405);
        }
    }
    db.execute(
        "INSERT INTO settings VALUES('feature:routingModes','true')",
        [],
    )
    .unwrap();
    assert_eq!(
        call(addr, "PUT", allowed, member, &csrf, "{").await,
        (400, serde_json::json!({"error":"invalid JSON"}))
    );
    assert_eq!(
        call(addr, "PUT", allowed, member, &csrf, r#"{"allowed":[]}"#)
            .await
            .0,
        403,
        "ROUTING_ASSERT/allowed-admin-required"
    );
    assert_eq!(
        call(
            addr,
            "PUT",
            main,
            member,
            &csrf,
            r#"{"mode":"hybrid","whenSensitive":"local"}"#
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            addr,
            "PUT",
            main,
            admin,
            &csrf,
            r#"{"mode":"cloud","cloud":{"providerId":"p","code":"m"}}"#
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(addr, "GET", main, member, "", "").await.1["storedMode"],
        "hybrid"
    );
    assert_eq!(
        call(addr, "GET", main, admin, "", "").await.1["storedMode"],
        "cloud"
    );
    assert_eq!(
        call(
            addr,
            "PUT",
            allowed,
            admin,
            &csrf,
            r#"{"allowed":["local"]}"#
        )
        .await
        .0,
        200
    );
    let view = call(addr, "GET", main, admin, "", "").await.1;
    assert_eq!(view["mode"], "local");
    assert_eq!(view["storedMode"], "cloud");
    // Queries cannot choose another tenant; normalized Node paths remain native.
    assert_eq!(
        call(addr, "GET", &format!("{main}?userId={B}"), admin, "", "")
            .await
            .1["storedMode"],
        "cloud"
    );
    for path in ["/api/x/../routing-mode", "/api/x/%2e%2e/routing-mode"] {
        assert_eq!(
            call(addr, "GET", path, admin, "", "").await.1["storedMode"],
            "cloud"
        );
    }
    assert_eq!(
        call(addr, "GET", "/api/routing-mode/", admin, "", "")
            .await
            .0,
        502
    );
    assert_eq!(
        call(
            addr,
            "PUT",
            main,
            admin,
            &csrf,
            r#"{"mode":null,"extra":1e999,"\ud800":"\udfff"}"#
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call_bytes(
            addr,
            "PUT",
            main,
            admin,
            &csrf,
            b"{\"mode\":null,\"extra\":\"\xff\"}"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            addr,
            "PUT",
            main,
            admin,
            &csrf,
            &serde_json::json!({"mode":null,"extra":"x".repeat(server_routing_mode::BODY_LIMIT)})
                .to_string()
        )
        .await
        .0,
        400
    );
    let huge = format!(
        "{{\"mode\":null,\"extra\":{}0{}}}",
        "[".repeat(129),
        "]".repeat(129)
    );
    assert_eq!(call(addr, "PUT", main, admin, &csrf, &huge).await.0, 400);
    db.execute_batch("CREATE TRIGGER routing_audit_abort BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic audit refusal'); END;").unwrap();
    assert_eq!(
        call(addr, "PUT", allowed, admin, &csrf, r#"{"allowed":[]}"#).await,
        (500, serde_json::json!({"error":"Internal error"}))
    );
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='routing_modes_allowed'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "[]"
    );
    db.execute_batch("DROP TRIGGER routing_audit_abort")
        .unwrap();
    let details:Vec<(String,String,String)>=db.prepare("SELECT actor_user_id,target_user_id,detail FROM audit_events WHERE action='routing.allowed'").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(
        details,
        vec![(A.into(), A.into(), "{\"allowed\":[\"local\"]}".into())]
    );
    db.execute_batch("UPDATE settings SET value='false' WHERE key='feature:routingModes'")
        .unwrap();
    assert_eq!(
        call(addr, "GET", main, admin, "", "").await.1,
        serde_json::json!({"enabled":false})
    );
    db.execute_batch("UPDATE settings SET value='true' WHERE key='feature:routingModes'; INSERT OR REPLACE INTO settings VALUES('routing_modes_allowed','[\"local\",\"cloud\",\"hybrid\"]');").unwrap();
    let bearer = "Authorization: Bearer nva_synthetic-device-token\r\n";
    assert_eq!(
        call(addr, "PUT", main, "", bearer, r#"{"mode":"local"}"#)
            .await
            .0,
        200
    );
    assert_eq!(
        call(addr, "PUT", allowed, "", bearer, r#"{"allowed":[]}"#)
            .await
            .0,
        403
    );
    assert_eq!(
        call(addr, "GET", main, admin, "", "").await.1["storedMode"],
        "local"
    );
    // Unsafe routing file state refuses every read/write without touching unrelated native auth.
    let file = d.path().join("users").join(A).join("routing-mode.json");
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink("/outside-synthetic-unavailable", &file).unwrap();
    assert_eq!(call(addr, "GET", main, admin, "", "").await.0, 503);
    assert_eq!(
        call(addr, "PUT", main, admin, &csrf, r#"{"mode":"local"}"#)
            .await
            .0,
        503
    );
    assert_eq!(
        call(addr, "GET", "/api/auth/session", admin, "", "")
            .await
            .0,
        200
    );
    db.execute_batch("DROP TRIGGER IF EXISTS routing_audit_abort; UPDATE sessions SET expires_at=1 WHERE user_id='bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb';").unwrap();
    assert_eq!(call(addr, "GET", main, member, "", "").await.0, 401);
    let _ = stop.send(());
    task.await.unwrap();
}
