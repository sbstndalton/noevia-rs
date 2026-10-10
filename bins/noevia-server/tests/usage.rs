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
) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\n{extra}\r\n").as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        s.read_to_end(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    let status = h.split_whitespace().nth(1).unwrap().parse().unwrap();
    (
        status,
        serde_json::from_str(b).unwrap_or(serde_json::Value::Null),
    )
}
#[tokio::test]
async fn usage_auth_admin_tenant_methods_and_omission() {
    let d = tempfile::tempdir().unwrap();
    let db = Connection::open(d.path().join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, t) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)",(uid,role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(t), uid, hex("csrf"), now, now + 86400000),
        )
        .unwrap();
        let dir = d.path().join("users").join(uid);
        std::fs::create_dir_all(&dir).unwrap();
        let day = chrono_day();
        std::fs::write(
            dir.join("usage.json"),
            serde_json::json!({"days":{day:{"input":if uid==A{11}else{22},"replies":0.5}}})
                .to_string(),
        )
        .unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let origin = format!("http://{addr}");
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR", d.path().display().to_string()),
        ("NOEVIA_RUST_AUTH", "1".into()),
        ("NOEVIA_RUST_USAGE", "1".into()),
        ("PUBLIC_ORIGIN", origin.clone()),
        ("TZ", "UTC".into()),
        ("LANG", "en_US.UTF-8".into()),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(serve(listener, app, Limits::default(), async {
        let _ = rx.await;
    }));
    let admin = "cowork_session=admin-token; cowork_csrf=csrf";
    let member = "cowork_session=member-token; cowork_csrf=csrf";
    assert_eq!(call(addr, "GET", "/api/usage", "", "").await.0, 401);
    assert_eq!(
        call(addr, "GET", "/api/usage", "cowork_session=wrong", "")
            .await
            .0,
        401
    );
    let (s, a) = call(
        addr,
        "GET",
        "/api/usage?userId=ignored",
        admin,
        &format!("X-User-Id: {B}\r\n"),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(a["allTime"]["input"], 11.0);
    let (s, b) = call(addr, "GET", "/api/usage", member, "").await;
    assert_eq!(s, 200);
    assert_eq!(b["allTime"]["input"], 22.0);
    assert_eq!(
        call(addr, "GET", "/api/usage/aggregate", member, "").await,
        (403, serde_json::json!({"error":"administrator required"}))
    );
    let (s, a) = call(addr, "GET", "/api/usage/aggregate", admin, "").await;
    assert_eq!(s, 200);
    assert_eq!(a["allTime"]["input"], 33.0);
    assert_eq!(a["aggregate"]["accounts"], 2);
    assert_eq!(call(addr, "HEAD", "/api/usage", admin, "").await.0, 405);
    assert_eq!(call(addr, "POST", "/api/usage", admin, "").await.0, 403);
    assert_eq!(
        call(
            addr,
            "POST",
            "/api/usage",
            admin,
            &format!("Origin: {origin}\r\nX-CSRF-Token: csrf\r\n")
        )
        .await
        .0,
        405
    );
    db.execute("UPDATE sessions SET expires_at=0 WHERE user_id=?1", [B])
        .unwrap();
    assert_eq!(call(addr, "GET", "/api/usage", member, "").await.0, 401);
    db.execute("DELETE FROM sessions WHERE user_id=?1", [A])
        .unwrap();
    assert_eq!(call(addr, "GET", "/api/usage", admin, "").await.0, 401);
    tx.send(()).unwrap();
    task.await.unwrap();
    // Omission control: removing switch ownership forwards to the dead legacy server, never 200.
    let mut env = env;
    env.insert("NOEVIA_RUST_USAGE", "0".into());
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
    assert_ne!(call(addr, "GET", "/api/usage", member, "").await.0, 200);
    tx.send(()).unwrap();
    task.await.unwrap();
}
fn chrono_day() -> String {
    use server_usage::summary::{summarize, Clock, Usage};
    let v = summarize(
        &Usage::default(),
        &Clock::new("UTC", "en-US").unwrap(),
        noevia_server::identity::now_ms(),
        1,
    )
    .unwrap();
    v["days"][0]["day"].as_str().unwrap().to_string()
}
