#![cfg(feature = "rust-chatgpt-status")]
//! Real HTTP through the native front (M6, sbstndalton/noevia#1373). The legacy upstream is dead,
//! so any proxied request answers 502 with the proxy's own body: a 200/401/404 here can only come
//! from the native handler, and the siblings and other methods prove they still go to Node.
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
use std::time::Duration;
use std::{collections::HashMap, path::Path, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ADMIN: &str = "cowork_session=admin-token; cowork_csrf=csrf";
const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";
const KEY: [u8; 32] = [7; 32];
const DDL: &str = "CREATE TABLE chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY, data_enc TEXT NOT NULL,\n    state TEXT NOT NULL DEFAULT 'connected', updated_at INTEGER NOT NULL)";

/// DDL is what core itself ran (crates/server-chatgpt-status oracle.cjs, at its oracle pin).
#[test]
fn the_table_is_nodes() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../crates/server-chatgpt-status/tests/fixtures/node-chatgpt-status.v1.json"
    ))
    .unwrap();
    assert_eq!(fixture["chatgptDdl"], DDL);
}

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
    csrf: Option<&str>,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let csrf = csrf
        .map(|t| format!("X-CSRF-Token: {t}\r\nOrigin: http://{addr}\r\n"))
        .unwrap_or_default();
    s.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\n{csrf}Content-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), s.read_to_end(&mut bytes))
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

fn seed_database(dir: &Path) -> Connection {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    db.execute_batch(DDL).unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, token) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86400000),
        )
        .unwrap();
    }
    db
}

fn seal(plain: &str, user: &str) -> String {
    secret_envelope::seal(&KEY, &[5; 12], plain.as_bytes(), Some(user.as_bytes())).unwrap()
}

async fn start(
    dir: &Path,
    extra: &[(&str, String)],
) -> (std::net::SocketAddr, oneshot::Sender<()>) {
    let (addr, stop, _) = start_app(dir, extra).await;
    (addr, stop)
}

async fn start_app(
    dir: &Path,
    extra: &[(&str, String)],
) -> (std::net::SocketAddr, oneshot::Sender<()>, Arc<App>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let origin = format!("http://{addr}");
    let mut env: HashMap<&str, String> = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("TRUST_PROXY", "true".into()),
        ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "1".into()),
        ("PUBLIC_ORIGIN", origin),
    ]);
    for (k, v) in extra {
        env.insert(k, v.clone());
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
    (addr, stop, app)
}

/// Nothing to switch on any more (sbstndalton/noevia#1071): the front always owns the route.
const ON: [(&str, &str); 0] = [];
fn on() -> Vec<(&'static str, String)> {
    ON.iter().map(|(k, v)| (*k, v.to_string())).collect()
}
const PATH: &str = "/api/providers/chatgpt";
const OFF: &str = r#"{"error":"Sign in with ChatGPT is turned off on this server."}"#;
const DISCONNECTED: &str =
    r#"{"state":"disconnected","providerId":"chatgpt-oauth","external":true}"#;

#[tokio::test]
async fn gate_flag_tenant_and_node_answers_through_the_front() {
    let d = tempfile::tempdir().unwrap();
    let db = seed_database(d.path());
    std::fs::write(d.path().join("secrets.key"), KEY).unwrap();
    let (addr, stop, app) = start_app(d.path(), &on()).await;
    assert!(
        app.chatgpt_status.as_ref().unwrap().keys_loaded(),
        "keys snapshotted at startup"
    );
    // Signed out: Node's 401, before anything else.
    let (s, h, b) = call(addr, "GET", PATH, "", None).await;
    assert_eq!(s, 401, "{b}");
    assert!(h.contains("www-authenticate: bearer realm=\"cowork\""));
    // Flag off (no settings row): Node's 404 with Node's headers.
    let (s, h, b) = call(addr, "GET", PATH, MEMBER, None).await;
    assert_eq!((s, b.as_str()), (404, OFF));
    assert!(h.contains("content-type: application/json"));
    assert!(h.contains("cache-control: no-store"));
    assert!(h.contains("x-noevia-api: 1"));
    db.execute(
        "INSERT INTO settings(key,value) VALUES('feature:chatgptOAuth','true')",
        [],
    )
    .unwrap();
    let (s, _, b) = call(addr, "GET", PATH, MEMBER, None).await;
    assert_eq!((s, b.as_str()), (200, DISCONNECTED));
    // A's row is never B's answer; A's is A's.
    db.execute("INSERT INTO chatgpt_oauth_tokens VALUES(?1,?2,'connected',1)", (A, seal(r#"{"accessToken":"AT-LEAK","email":"admin@example.test","plan":{"type":"plus","access_token":"PLAN-LEAK"}}"#, A))).unwrap();
    let (s, _, b) = call(addr, "GET", PATH, MEMBER, None).await;
    assert_eq!((s, b.as_str()), (200, DISCONNECTED));
    let (s, _, b) = call(addr, "GET", PATH, ADMIN, None).await;
    assert_eq!(s, 200);
    assert_eq!(
        b,
        r#"{"state":"connected","account":{"email":"a…@example.test","plan":null},"providerId":"chatgpt-oauth","external":true}"#
    );
    assert!(!b.contains("LEAK"));
    // A's ciphertext copied into B's row does not open for B.
    db.execute("INSERT INTO chatgpt_oauth_tokens SELECT ?1, data_enc, state, updated_at FROM chatgpt_oauth_tokens WHERE user_id=?2", (B, A)).unwrap();
    let (s, _, b) = call(addr, "GET", PATH, MEMBER, None).await;
    assert_eq!(
        (s, b.as_str()),
        (
            200,
            r#"{"state":"reconnect","providerId":"chatgpt-oauth","external":true}"#
        )
    );
    // The query is ignored, as Node routes on the pathname; the WHATWG-normalized path too.
    let (s, _, _) = call(
        addr,
        "GET",
        "/api/providers/chatgpt?user=aaaaaaaa",
        MEMBER,
        None,
    )
    .await;
    assert_eq!(s, 200);
    let (s, _, _) = call(addr, "GET", "/api/providers/x/../chatgpt", MEMBER, None).await;
    assert_eq!(s, 200);
    // Everything else is Node's (dead upstream: 502).
    for (m, p) in [
        ("POST", PATH),
        ("DELETE", PATH),
        ("HEAD", PATH),
        ("GET", "/api/providers/chatgpt/"),
        // The broker's siblings (sbstndalton/noevia#2190 round 5) keep their other methods on Node.
        ("POST", "/api/providers/chatgpt/models"),
        ("GET", "/api/providers/chatgpt/device"),
        ("GET", "/api/providers/chatgpt/device/poll"),
    ] {
        let (s, _, _) = call(addr, m, p, MEMBER, Some("csrf")).await;
        assert_eq!(s, 502, "{m} {p}");
    }
    // A missing table is a sanitized 502, never created.
    db.execute_batch("DROP TABLE chatgpt_oauth_tokens").unwrap();
    let (s, _, b) = call(addr, "GET", PATH, MEMBER, None).await;
    assert_eq!(
        (s, b.as_str()),
        (502, r#"{"error":"ChatGPT sign-in failed."}"#)
    );
    let n: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='chatgpt_oauth_tokens'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
    let _ = stop.send(());
}

#[tokio::test]
async fn a_retired_switch_value_keeps_it_native_and_a_late_key_is_loaded_once() {
    let d = tempfile::tempdir().unwrap();
    let db = seed_database(d.path());
    db.execute(
        "INSERT INTO settings(key,value) VALUES('feature:chatgptOAuth','true')",
        [],
    )
    .unwrap();
    // sbstndalton/noevia#1071: the old switch is retired; even "0" leaves the route native (the
    // proxy to the dead upstream would be a 502).
    let (addr, stop) = start(d.path(), &[("NOEVIA_RUST_CHATGPT_STATUS", "0".into())]).await;
    let (s, _, b) = call(addr, "GET", PATH, MEMBER, None).await;
    assert_eq!(s, 200, "native without a switch: {b}");
    let _ = stop.send(());
    // No key at startup (Node has not created it yet): rows that need it are 502 until it exists.
    db.execute(
        "INSERT INTO chatgpt_oauth_tokens VALUES(?1,?2,'connected',1)",
        (B, seal(r#"{"email":"m@example.test","plan":"plus"}"#, B)),
    )
    .unwrap();
    let (addr, stop, app) = start_app(d.path(), &on()).await;
    assert!(!app.chatgpt_status.as_ref().unwrap().keys_loaded());
    let (s, _, _) = call(addr, "GET", PATH, MEMBER, None).await;
    assert_eq!(s, 502);
    std::fs::write(d.path().join("secrets.key"), KEY).unwrap();
    let (s, _, b) = call(addr, "GET", PATH, MEMBER, None).await;
    assert_eq!(s, 200, "{b}");
    assert!(b.contains("m…@example.test"));
    // Snapshotted: replacing the key on disk changes nothing until a restart.
    std::fs::write(d.path().join("secrets.key"), [9u8; 32]).unwrap();
    let (s, _, b) = call(addr, "GET", PATH, MEMBER, None).await;
    assert_eq!(s, 200);
    assert!(b.contains("m…@example.test"));
    let _ = stop.send(());
    let _ = Duration::from_secs(0);
}
