#![cfg(feature = "rust-routing-default")]
//! Track R12: `PUT /api/routing-default` through the real HTTP front with NOEVIA_RUST_SETTINGS_ROUTES=1,
//! replayed on the cases recorded from core's own models.cjs route
//! (crates/server-routing-default/tests/fixtures/node-routing-default-put.v1.json): the reply, preferences.json
//! byte for byte and projects.json as Node saved it. A dead legacy upstream makes any proxied request answer
//! 502, so with the switch unset the PUT must reach it.
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
use serde_json::Value;
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::Path, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

const FIXTURE: &str = include_str!(
    "../../../crates/server-routing-default/tests/fixtures/node-routing-default-put.v1.json"
);
const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";
const SWITCH: (&str, &str) = ("NOEVIA_RUST_SETTINGS_ROUTES", "1");

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn decode_hex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

fn bytes_of(v: &Value) -> Option<Vec<u8>> {
    if v.is_null() {
        return None;
    }
    if let Some(n) = v["repeatSpacesTo"].as_u64() {
        let mut b = v["text"].as_str().unwrap().as_bytes().to_vec();
        b.resize(usize::try_from(n).unwrap(), b' ');
        return Some(b);
    }
    Some(match v["text"].as_str() {
        Some(t) => t.as_bytes().to_vec(),
        None => decode_hex(v["hex"].as_str().unwrap()),
    })
}

/// Status and body of one HTTP/1.1 PUT.
async fn put(addr: std::net::SocketAddr, cookie: &str, csrf: bool, body: &[u8]) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let csrf = if csrf {
        format!("X-CSRF-Token: csrf\r\nOrigin: http://{addr}\r\n")
    } else {
        String::new()
    };
    let head = format!(
        "PUT /api/routing-default HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\n{csrf}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    // A refused request may close before the whole body is sent.
    let _ = s.write_all(body).await;
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        s.read_to_end(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        b.to_string(),
    )
}

fn seed_database(dir: &Path, user: &str) {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic QA',?2,'x',?1,1,1)", (user, "member")).unwrap();
    db.execute(
        "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
        (hex("member-token"), user, hex("csrf"), now, now + 86400000),
    )
    .unwrap();
}

async fn start(dir: &Path, extra: &[(&str, &str)]) -> (std::net::SocketAddr, oneshot::Sender<()>) {
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

/// `bytes` with every `"updatedAt": <t>` stamped during the request (`before..=after`) written as `now`.
fn at_recorded_time(bytes: &[u8], before: i64, after: i64, now: i64) -> Vec<u8> {
    const KEY: &str = "\"updatedAt\": ";
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find(KEY) {
        let (head, tail) = rest.split_at(at + KEY.len());
        out.push_str(head);
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        let (number, tail) = tail.split_at(digits);
        match number.parse::<i64>() {
            Ok(t) if (before..=after).contains(&t) => out.push_str(&now.to_string()),
            _ => out.push_str(number),
        }
        rest = tail;
    }
    out.push_str(rest);
    out.into_bytes()
}

#[tokio::test]
async fn recorded_node_cases_through_the_front() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let user = fixture["user"].as_str().unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let expected = &case["expected"];
        let d = tempfile::tempdir().unwrap();
        seed_database(d.path(), user);
        let tenant = d.path().join("users").join(user);
        std::fs::create_dir_all(&tenant).unwrap();
        let prefs = bytes_of(&case["preferences"]);
        if let Some(p) = &prefs {
            std::fs::write(tenant.join("preferences.json"), p).unwrap();
        }
        let projects = bytes_of(&case["projects"]);
        if let Some(p) = &projects {
            std::fs::write(tenant.join("projects.json"), p).unwrap();
        }
        let (addr, stop) = start(d.path(), &[SWITCH]).await;
        let before = noevia_server::identity::now_ms();
        let (status, reply) = put(addr, MEMBER, true, &bytes_of(&case["body"]).unwrap()).await;
        let after = noevia_server::identity::now_ms();
        let _ = stop.send(());
        let prefs_after = std::fs::read(tenant.join("preferences.json")).ok();
        // The front stamps `updatedAt` with its clock, Node's recording with the fixed `now`.
        let projects_after = std::fs::read(tenant.join("projects.json"))
            .ok()
            .map(|b| at_recorded_time(&b, before, after, fixture["now"].as_i64().unwrap()));
        match case["difference"].as_str() {
            Some("unholdable-body") => {
                assert_eq!(
                    (status, reply.as_str()),
                    (400, r#"{"error":"invalid JSON"}"#),
                    "{name}"
                );
                assert_eq!(prefs_after, prefs, "{name}");
                continue;
            }
            Some("unholdable-preferences") => {
                assert_eq!(status, 503, "{name}: fails closed");
                assert_eq!(prefs_after, prefs, "{name}: left as stored");
                assert_eq!(projects_after, projects, "{name}");
                continue;
            }
            Some(other) => panic!("{name}: unknown difference {other}"),
            None => {}
        }
        assert_eq!(
            u64::from(status),
            expected["status"].as_u64().unwrap(),
            "{name}: {reply}"
        );
        assert_eq!(reply, expected["reply"].as_str().unwrap(), "{name}");
        let want_prefs = expected["preferences"].as_str().map(decode_hex);
        assert_eq!(prefs_after, want_prefs, "{name}: preferences.json");
        let want_projects = match expected["projectsSaved"].as_str() {
            Some(text) => Some(text.as_bytes().to_vec()),
            None => projects,
        };
        assert_eq!(projects_after, want_projects, "{name}: projects.json");
    }
}

#[tokio::test]
async fn switch_unset_leaves_the_put_to_node_and_the_gate_comes_first() {
    let user = "aaaaaaaa-1111-4aaa-8aaa-aaaaaaaaaaaa";
    let d = tempfile::tempdir().unwrap();
    seed_database(d.path(), user);
    let body = br#"{"routing":"manual","applyToExisting":true}"#;
    // Unset (and any value but 1): the dead upstream answers, nothing is written here.
    for extra in [&[][..], &[("NOEVIA_RUST_SETTINGS_ROUTES", "true")][..]] {
        let (addr, stop) = start(d.path(), extra).await;
        assert_eq!(put(addr, MEMBER, true, body).await.0, 502);
        let _ = stop.send(());
    }
    assert!(!d
        .path()
        .join("users")
        .join(user)
        .join("preferences.json")
        .exists());
    let (addr, stop) = start(d.path(), &[SWITCH]).await;
    // Signed out: 401; no CSRF token: refused by the gate; neither writes.
    assert_eq!(put(addr, "", false, body).await.0, 401);
    assert_eq!(put(addr, MEMBER, false, body).await.0, 403);
    assert!(!d
        .path()
        .join("users")
        .join(user)
        .join("preferences.json")
        .exists());
    // A member may set their own default; the account's directory is made as Node's load makes it.
    assert_eq!(
        put(addr, MEMBER, true, body).await,
        (200, r#"{"routing":"manual","updated":0}"#.to_string())
    );
    assert_eq!(
        std::fs::read_to_string(d.path().join("users").join(user).join("preferences.json"))
            .unwrap(),
        "{\n  \"defaultRouting\": \"manual\"\n}"
    );
    let _ = stop.send(());
}

/// The account is removed after the first gate and before its directory is made: the removed
/// account's 410, and neither the directory nor a preferences.json comes back (#1425, #2026).
#[cfg(debug_assertions)]
#[tokio::test]
async fn an_account_removed_after_the_gate_gets_nothing_back() {
    // Its own user id: the hook is per user and process-wide.
    let user = "cccccccc-3333-4ccc-8ccc-cccccccccccc";
    let d = tempfile::tempdir().unwrap();
    seed_database(d.path(), user);
    let data = d.path().to_path_buf();
    noevia_server::project_routes::hooks::on_before_ensure(user, move || {
        let c = Connection::open(data.join("cowork.db")).unwrap();
        c.execute("DELETE FROM sessions WHERE user_id = ?1", [user])
            .unwrap();
        c.execute("DELETE FROM users WHERE id = ?1", [user])
            .unwrap();
        let _ = std::fs::remove_dir_all(data.join("users").join(user));
    });
    let (addr, stop) = start(d.path(), &[SWITCH]).await;
    let (status, body) = put(addr, MEMBER, true, br#"{"routing":"manual"}"#).await;
    let _ = stop.send(());
    assert_eq!(
        (status, body.as_str()),
        (410, r#"{"error":"account no longer exists"}"#)
    );
    assert!(!d.path().join("users").join(user).exists());
}
