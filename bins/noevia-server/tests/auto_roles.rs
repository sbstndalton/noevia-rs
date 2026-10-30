//! `GET /api/auto-roles` through the real front (sbstndalton/noevia#2190 close-out, capability
//! `rust-auto-roles`): the gate, the account's auto-roles.json as it is on disk, the snapshot Node
//! publishes (served catalogue, roles on other machines, their models), and what stays Node's (the
//! PUT). The legacy upstream is dead, so a request that fell through to Node would answer 502.
//! Synthetic accounts and model names only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    dead_code
)]
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const COOKIE_A: &str = "cowork_session=session-a; cowork_csrf=csrf";
const COOKIE_B: &str = "cowork_session=session-b; cowork_csrf=csrf";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn seed(dir: &std::path::Path) {
    let db = Connection::open(server_store::db_path(dir)).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, name, token, diary) in [
        (A, "ada", "session-a", true),
        (B, "bob", "session-b", false),
    ] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?2,?2,'Synthetic','member','x',?1,1,1)", (uid, name)).unwrap();
        if diary {
            db.execute(
                "INSERT INTO user_features(user_id,diary_enabled,updated_at) VALUES(?1,1,1)",
                [uid],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("users").join(uid)).unwrap();
    }
}

async fn front(dir: &std::path::Path) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    addr
}

struct Answer {
    status: u16,
    headers: HashMap<String, String>,
    text: String,
}

async fn call(addr: std::net::SocketAddr, method: &str, path: &str, cookie: &str) -> Answer {
    let csrf = if method == "GET" {
        String::new()
    } else {
        format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n")
    };
    let cookie = if cookie.is_empty() {
        String::new()
    } else {
        format!("Cookie: {cookie}\r\n")
    };
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{cookie}Content-Length: 0\r\n{csrf}\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(40), s.read_to_end(&mut bytes)).await;
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    let mut lines = h.lines();
    let status = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_lowercase(), v.to_owned()))
        .collect();
    Answer {
        status,
        headers,
        text: b.to_owned(),
    }
}

const A_DIR_FILE: &str = "auto-roles.json";

fn roles_file(dir: &std::path::Path, user: &str, text: &str) {
    std::fs::write(dir.join("users").join(user).join(A_DIR_FILE), text).unwrap();
}

/// What Node's publisher writes (core published-snapshot.cjs / rust-auto-roles.cjs), at `written_at`.
fn publish(dir: &std::path::Path, state: &str, written_at: i64, every_ms: i64) {
    std::fs::write(
        dir.join("auto-roles-snapshot.json"),
        format!(
            r#"{{"version":1,"kind":"auto-roles","writtenAt":{written_at},"everyMs":{every_ms},{state}}}"#
        ),
    )
    .unwrap();
}

fn fresh(dir: &std::path::Path, state: &str) {
    publish(dir, state, noevia_server::identity::now_ms(), 15_000);
}

fn data() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    dir
}

const CATALOGUE: &str = r#""catalogue":[{"name":"alpha","missingFile":false},{"name":"beta","missingFile":false},{"name":"ghost","missingFile":true}],"machineRoles":[],"machineModels":[]"#;

#[tokio::test]
async fn the_gate_is_nodes_and_a_member_reads_their_own_roles() {
    let dir = data();
    roles_file(dir.path(), A, r#"{"fast":"alpha","smart":"beta"}"#);
    roles_file(dir.path(), B, r#"{"fast":"beta","smart":"beta"}"#);
    fresh(dir.path(), CATALOGUE);
    let addr = front(dir.path()).await;
    let r = call(addr, "GET", "/api/auto-roles", "").await;
    assert_eq!(
        (r.status, r.text.as_str()),
        (401, r#"{"error":"unauthorized"}"#)
    );
    let r = call(addr, "GET", "/api/auto-roles", COOKIE_A).await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.text,
        r#"{"configured":true,"roles":{"fast":"alpha","smart":"beta"},"missing":[]}"#
    );
    assert_eq!(r.headers["content-type"], "application/json");
    assert_eq!(r.headers["cache-control"], "no-store");
    assert_eq!(r.headers["x-noevia-api"], "1");
    // Each account reads its own file.
    let r = call(addr, "GET", "/api/auto-roles", COOKIE_B).await;
    assert_eq!(
        r.text,
        r#"{"configured":true,"roles":{"fast":"beta","smart":"beta"},"missing":[]}"#
    );
}

#[tokio::test]
async fn a_role_the_engine_no_longer_serves_is_missing_in_cores_order() {
    let dir = data();
    // The file's key order is the answer's `roles`; `missing` follows core's role order; a model
    // whose file is gone counts as missing; an empty role is not a role.
    roles_file(
        dir.path(),
        A,
        r#"{"creative":"nowhere","smart":"ghost","fast":"alpha","code":"","vision":"alpha"}"#,
    );
    fresh(dir.path(), CATALOGUE);
    let addr = front(dir.path()).await;
    let r = call(addr, "GET", "/api/auto-roles", COOKIE_A).await;
    assert_eq!(
        r.text,
        r#"{"configured":true,"roles":{"creative":"nowhere","smart":"ghost","fast":"alpha","code":"","vision":"alpha"},"missing":[{"role":"smart","model":"ghost"},{"role":"creative","model":"nowhere"}]}"#
    );
}

#[tokio::test]
async fn roles_on_other_machines_are_not_checked_here_and_their_models_are_listed() {
    let dir = data();
    roles_file(
        dir.path(),
        A,
        r#"{"fast":"remote-one","smart":"gone-here","vision":"remote-two"}"#,
    );
    fresh(
        dir.path(),
        r#""catalogue":[{"name":"alpha","missingFile":false}],"machineRoles":["fast","vision"],"machineModels":[{"model":"remote-one","machine":{"id":"m1","name":"Box"},"roles":["fast"]},{"model":"remote-two","machine":{"id":"m2","name":"Other"},"roles":["vision"]}]"#,
    );
    let addr = front(dir.path()).await;
    let r = call(addr, "GET", "/api/auto-roles", COOKIE_A).await;
    assert_eq!(
        r.text,
        r#"{"configured":true,"roles":{"fast":"remote-one","smart":"gone-here","vision":"remote-two"},"missing":[{"role":"smart","model":"gone-here"}],"machineModels":[{"model":"remote-one","machine":{"id":"m1","name":"Box"},"roles":["fast"]},{"model":"remote-two","machine":{"id":"m2","name":"Other"},"roles":["vision"]}]}"#
    );
}

#[tokio::test]
async fn an_unusable_snapshot_reads_as_an_engine_that_cannot_be_reached() {
    let dir = data();
    roles_file(dir.path(), A, r#"{"fast":"anything","smart":"anything"}"#);
    let addr = front(dir.path()).await;
    let silent =
        r#"{"configured":true,"roles":{"fast":"anything","smart":"anything"},"missing":[]}"#;
    let file = dir.path().join("auto-roles-snapshot.json");
    // Missing: nothing is reported missing and no machine models are listed.
    assert_eq!(
        call(addr, "GET", "/api/auto-roles", COOKIE_A).await.text,
        silent
    );
    // The first request after a start waits for Node (two seconds): the clock is read after it.
    let now = noevia_server::identity::now_ms();
    // The same models, published, are missing.
    let state = r#""catalogue":[{"name":"other","missingFile":false}],"machineRoles":[],"machineModels":[{"model":"m","machine":{"id":"i","name":"n"},"roles":["fast"]}]"#;
    let loud = r#"{"configured":true,"roles":{"fast":"anything","smart":"anything"},"missing":[{"role":"fast","model":"anything"},{"role":"smart","model":"anything"}],"machineModels":[{"model":"m","machine":{"id":"i","name":"n"},"roles":["fast"]}]}"#;
    fresh(dir.path(), state);
    assert_eq!(
        call(addr, "GET", "/api/auto-roles", COOKIE_A).await.text,
        loud
    );
    // Older than two intervals (30 s here), from the future, of another kind or version, or damaged: silent again.
    publish(dir.path(), state, now - 29_000, 15_000);
    assert_eq!(
        call(addr, "GET", "/api/auto-roles", COOKIE_A).await.text,
        loud
    );
    publish(dir.path(), state, now - 40_000, 15_000);
    assert_eq!(
        call(addr, "GET", "/api/auto-roles", COOKIE_A).await.text,
        silent
    );
    publish(dir.path(), state, now + 60_000, 15_000);
    assert_eq!(
        call(addr, "GET", "/api/auto-roles", COOKIE_A).await.text,
        silent
    );
    for text in [
        format!(r#"{{"version":1,"kind":"rag-status","writtenAt":{now},"everyMs":15000,{state}}}"#),
        format!(r#"{{"version":3,"kind":"auto-roles","writtenAt":{now},"everyMs":15000,{state}}}"#),
        format!(
            r#"{{"version":1,"kind":"auto-roles","writtenAt":{now},"everyMs":15000,"catalogue":"all","machineRoles":[],"machineModels":[]}}"#
        ),
        format!(
            r#"{{"version":1,"kind":"auto-roles","writtenAt":{now},"everyMs":15000,"catalogue":[{{"name":5}}],"machineRoles":[],"machineModels":[]}}"#
        ),
        format!(
            r#"{{"version":1,"kind":"auto-roles","writtenAt":{now},"everyMs":15000,"catalogue":[],"machineRoles":[1],"machineModels":[]}}"#
        ),
        format!(
            r#"{{"version":1,"kind":"auto-roles","writtenAt":{now},"everyMs":15000,"catalogue":[]}}"#
        ),
        r#"{"version":1,"kind":"auto-roles""#.to_owned(),
        String::new(),
    ] {
        std::fs::write(&file, &text).unwrap();
        assert_eq!(
            call(addr, "GET", "/api/auto-roles", COOKIE_A).await.text,
            silent,
            "{text}"
        );
    }
    // An unknown catalogue (null) says nothing is missing, as core does for an engine it cannot read.
    fresh(
        dir.path(),
        r#""catalogue":null,"machineRoles":[],"machineModels":[]"#,
    );
    assert_eq!(
        call(addr, "GET", "/api/auto-roles", COOKIE_A).await.text,
        silent
    );
}

#[tokio::test]
async fn a_missing_or_damaged_roles_file_is_not_configured_and_each_read_is_fresh() {
    let dir = data();
    fresh(dir.path(), CATALOGUE);
    let addr = front(dir.path()).await;
    let none = r#"{"configured":false,"roles":null,"missing":[]}"#;
    assert_eq!(
        call(addr, "GET", "/api/auto-roles", COOKIE_A).await.text,
        none
    );
    for text in ["", "{not json", "null", "0", "false", "\"\""] {
        roles_file(dir.path(), A, text);
        assert_eq!(
            call(addr, "GET", "/api/auto-roles", COOKIE_A).await.text,
            none,
            "{text:?}"
        );
    }
    roles_file(dir.path(), A, r#"{"fast":"alpha","smart":"nope"}"#);
    assert!(call(addr, "GET", "/api/auto-roles", COOKIE_A)
        .await
        .text
        .contains(r#""model":"nope""#));
    roles_file(dir.path(), A, r#"{"fast":"alpha","smart":"beta"}"#);
    assert!(call(addr, "GET", "/api/auto-roles", COOKIE_A)
        .await
        .text
        .contains(r#""missing":[]"#));
}

#[tokio::test]
async fn the_first_request_of_an_account_makes_its_directory_and_only_the_get_is_the_fronts() {
    let dir = data();
    std::fs::remove_dir(dir.path().join("users").join(B)).unwrap();
    fresh(dir.path(), CATALOGUE);
    let addr = front(dir.path()).await;
    let r = call(addr, "GET", "/api/auto-roles", COOKIE_B).await;
    assert_eq!(r.text, r#"{"configured":false,"roles":null,"missing":[]}"#);
    assert!(dir.path().join("users").join(B).is_dir());
    // The save (the PUT, which warms the roles in the engine), and every other method, are Node's: dead here.
    for method in ["PUT", "POST", "DELETE"] {
        assert_eq!(
            call(addr, method, "/api/auto-roles", COOKIE_A).await.status,
            502,
            "{method}"
        );
    }
    assert_eq!(
        call(addr, "GET", "/api/auto-roles/", COOKIE_A).await.status,
        502
    );
}

fn marker(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("snapshot-demand")
}

#[tokio::test]
async fn a_signed_in_read_asks_node_to_publish_and_waits_for_it_only_after_idle() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = data();
    roles_file(dir.path(), A, r#"{"fast":"anything"}"#);
    let addr = front(dir.path()).await;
    let silent = r#"{"configured":true,"roles":{"fast":"anything"},"missing":[]}"#;
    let loud = r#"{"configured":true,"roles":{"fast":"anything"},"missing":[{"role":"fast","model":"anything"}],"machineModels":[{"model":"m","machine":{"id":"i","name":"n"},"roles":["fast"]}]}"#;
    let state = r#""catalogue":[{"name":"other","missingFile":false}],"machineRoles":[],"machineModels":[{"model":"m","machine":{"id":"i","name":"n"},"roles":["fast"]}]"#;
    // Signed out: no marker.
    assert_eq!(call(addr, "GET", "/api/auto-roles", "").await.status, 401);
    assert!(!marker(dir.path()).exists());
    // Idle and nothing published: silent after the wait (no machineModels), still a 200.
    let started = std::time::Instant::now();
    let r = call(addr, "GET", "/api/auto-roles", COOKIE_A).await;
    assert_eq!((r.status, r.text.as_str()), (200, silent));
    assert!(
        started.elapsed() >= Duration::from_millis(1900),
        "{:?}",
        started.elapsed()
    );
    let mode = std::fs::metadata(marker(dir.path()))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // The marker is live and Node is not publishing: answered at once.
    let started = std::time::Instant::now();
    assert_eq!(
        call(addr, "GET", "/api/auto-roles", COOKIE_A).await.text,
        silent
    );
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "{:?}",
        started.elapsed()
    );
    // Idle again, and Node wakes inside the wait: the answer carries what it published.
    std::fs::OpenOptions::new()
        .write(true)
        .open(marker(dir.path()))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(90))
        .unwrap();
    let writer = {
        let path = dir.path().to_path_buf();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            fresh(&path, state);
        })
    };
    let started = std::time::Instant::now();
    let r = call(addr, "GET", "/api/auto-roles", COOKIE_A).await;
    writer.await.unwrap();
    assert_eq!(r.text, loud);
    assert!(
        started.elapsed() >= Duration::from_millis(450),
        "{:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_millis(1900),
        "{:?}",
        started.elapsed()
    );
}
