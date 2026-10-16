#![cfg(feature = "rust-routing-default")]
//! Real HTTP through the native front (M6, #1305); a dead legacy upstream makes any proxied
//! request answer 502, so a 200/401/503 here can only come from the native handler.
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
use std::{collections::HashMap, path::Path, sync::Arc};
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

/// Status, lower-cased headers and the body of one HTTP/1.1 exchange.
async fn call(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let csrf = if cookie.is_empty() {
        String::new()
    } else {
        format!("X-CSRF-Token: csrf\r\nOrigin: http://{addr}\r\n")
    };
    s.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\n{csrf}Content-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
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
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        h.to_ascii_lowercase(),
        b.to_string(),
    )
}

fn seed_database(dir: &Path) {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, token) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86400000),
        )
        .unwrap();
    }
}

fn tenant_file(dir: &Path, uid: &str, bytes: &[u8]) {
    let t = dir.join("users").join(uid);
    std::fs::create_dir_all(&t).unwrap();
    std::fs::write(t.join("preferences.json"), bytes).unwrap();
}

async fn start(dir: &Path, extra: &[(&str, &str)]) -> (std::net::SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let origin = format!("http://{addr}");
    let mut env: HashMap<&str, String> = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("NOEVIA_RUST_AUTH", "1".into()),
        ("TRUST_PROXY", "true".into()),
        ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "1".into()),
        ("PUBLIC_ORIGIN", origin),
    ]);
    for (k, v) in extra {
        env.insert(k, (*v).to_string());
    }
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let (stop, rx) = oneshot::channel();
    tokio::spawn(serve(
        listener,
        Arc::clone(&app),
        Limits::default(),
        async {
            let _ = rx.await;
        },
    ));
    (addr, stop)
}

#[tokio::test]
async fn native_get_answers_the_signed_in_tenant_and_nothing_else() {
    let d = tempfile::tempdir().unwrap();
    seed_database(d.path());
    tenant_file(d.path(), A, br#"{"defaultRouting":"manual"}"#);
    let (addr, stop) = start(d.path(), &[]).await;
    let admin = "cowork_session=admin-token; cowork_csrf=csrf";
    let member = "cowork_session=member-token; cowork_csrf=csrf";
    let path = "/api/routing-default";

    // Unauthenticated: the native gate refuses (401), it never reaches Node (502).
    assert_eq!(call(addr, "GET", path, "").await.0, 401);
    let (status, headers, body) = call(addr, "GET", path, admin).await;
    assert_eq!((status, body.as_str()), (200, r#"{"routing":"manual"}"#));
    assert!(headers.contains("content-type: application/json"));
    assert!(headers.contains("cache-control: no-store"));
    assert!(headers.contains("x-noevia-api: 1"));
    // Another tenant never sees A's file; a member with no file defaults to auto.
    assert_eq!(
        call(addr, "GET", path, member).await.2,
        r#"{"routing":"auto"}"#
    );

    // Malformed data for B is Node's readJson fallback: auto.
    tenant_file(d.path(), B, b"{");
    assert_eq!(
        call(addr, "GET", path, member).await.2,
        r#"{"routing":"auto"}"#
    );
    // An unsafe file for B fails closed: 503 rather than a guessed answer.
    std::fs::remove_file(d.path().join("users").join(B).join("preferences.json")).unwrap();
    let external = d.path().join("external.json");
    std::fs::write(&external, br#"{"defaultRouting":"manual"}"#).unwrap();
    std::os::unix::fs::symlink(
        &external,
        d.path().join("users").join(B).join("preferences.json"),
    )
    .unwrap();
    assert_eq!(call(addr, "GET", path, member).await.0, 503);

    // Every other method stays Node's: proxied to the dead upstream.
    for method in ["PUT", "POST", "DELETE", "PATCH"] {
        assert_eq!(call(addr, method, path, admin).await.0, 502, "{method}");
    }
    let _ = stop.send(());
}

#[tokio::test]
async fn a_retired_switch_value_never_hands_the_route_back_to_node() {
    // sbstndalton/noevia#1071: NOEVIA_RUST_ROUTING_DEFAULT is retired; "0" changes nothing.
    let d = tempfile::tempdir().unwrap();
    seed_database(d.path());
    tenant_file(d.path(), A, br#"{"defaultRouting":"manual"}"#);
    let (addr, stop) = start(d.path(), &[("NOEVIA_RUST_ROUTING_DEFAULT", "0")]).await;
    let admin = "cowork_session=admin-token; cowork_csrf=csrf";
    assert_eq!(
        call(addr, "GET", "/api/routing-default", admin).await.0,
        200
    );
    let _ = stop.send(());
}
