//! Real HTTP through the native front for the configuration-only model reads
//! (sbstndalton/noevia#2187): `GET /api/models/capabilities` and `GET /api/models/hardware`. The
//! legacy upstream is dead, so any proxied request answers 502 with the proxy's own body: a
//! 200/401/404 here can only come from the native handler, and the other methods and model routes
//! prove they still go to Node. Synthetic accounts only.
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
const DEVICE: &str = "nva_synthetic-device-token-0001";
const CAPABILITIES: &str = "/api/models/capabilities";
const HARDWARE: &str = "/api/models/hardware";

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
    headers: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{headers}Content-Length: 0\r\n\r\n"
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

fn cookie(c: &str) -> String {
    format!("Cookie: {c}\r\n")
}

/// An admin and a member with sessions, and a device token on the admin's account.
fn seed_database(dir: &Path) -> Connection {
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
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO device_grants VALUES('g-1',?1,'Mac',?2,?2,?3,'ip','ua')",
        (A, now, now + 86_400_000),
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_tokens VALUES(?1,'g-1','access',?2,?3,NULL,NULL)",
        (hex(DEVICE), now, now + 3_600_000),
    )
    .unwrap();
    db
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
        env.insert(k, (*v).to_owned());
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

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn capabilities_answer_nodes_body_per_role_and_signed_out_gets_401() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let (addr, stop) = start(
        d.path(),
        &[("MODEL_LOADER_URL", "http://model-loader.invalid:1")],
    )
    .await;
    let (s, h, b) = call(addr, "GET", CAPABILITIES, "").await;
    assert_eq!(s, 401, "{b}");
    assert!(h.contains("www-authenticate: bearer realm=\"cowork\""));
    // The corpus answer (000136): no model manager, an administrator.
    let (s, h, b) = call(addr, "GET", CAPABILITIES, &cookie(ADMIN)).await;
    assert_eq!(s, 200);
    assert_eq!(
        b,
        r#"{"kind":"none","enabled":false,"admin":true,"routing":false,"load":false,"unload":false,"download":false,"deleteCached":false,"runtimeOptions":false,"hardware":false,"presets":false,"modelManagement":true}"#
    );
    for header in [
        "content-type: application/json",
        "cache-control: no-store",
        "x-noevia-api: 1",
        "x-content-type-options: nosniff",
    ] {
        assert!(h.contains(header), "{header}");
    }
    // A member: not an admin, and no model management.
    let (s, _, b) = call(addr, "GET", CAPABILITIES, &cookie(MEMBER)).await;
    assert_eq!(s, 200);
    let v = json(&b);
    assert_eq!(
        (v["admin"].clone(), v["modelManagement"].clone()),
        (false.into(), false.into())
    );
    // A device token acts as a member even on an admin's account.
    let (s, _, b) = call(
        addr,
        "GET",
        CAPABILITIES,
        &format!("Authorization: Bearer {DEVICE}\r\n"),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(json(&b)["admin"], false);
    // An unknown session is refused like Node's gate.
    let (s, _, _) = call(
        addr,
        "GET",
        CAPABILITIES,
        &cookie("cowork_session=nope; cowork_csrf=csrf"),
    )
    .await;
    assert_eq!(s, 401);
    let _ = stop.send(());
}

#[tokio::test]
async fn native_engine_capabilities_and_hardware() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let (addr, stop) = start(
        d.path(),
        &[
            ("MODEL_MANAGER_KIND", "llamacpp"),
            ("MODEL_MANAGER_BASE_URL", "http://127.0.0.1:9/v1"),
            ("LLAMACPP_PRESET_PATH", "/synthetic/models.ini"),
        ],
    )
    .await;
    let (s, _, b) = call(addr, "GET", CAPABILITIES, &cookie(ADMIN)).await;
    assert_eq!(s, 200);
    assert_eq!(
        b,
        r#"{"kind":"llamacpp","enabled":true,"admin":true,"routing":true,"load":true,"unload":true,"download":true,"deleteCached":true,"runtimeOptions":false,"hardware":false,"presets":true,"autotune":true,"modelManagement":false}"#
    );
    // #629: the llama.cpp adapter cannot discover hardware; a capability answer, never a 502 (the
    // engine at 127.0.0.1:9 is not even asked).
    for c in [ADMIN, MEMBER] {
        let (s, h, b) = call(addr, "GET", HARDWARE, &cookie(c)).await;
        assert_eq!((s, b.as_str()), (200, r#"{"supported":false}"#));
        assert!(h.contains("cache-control: no-store"));
    }
    let (s, _, _) = call(addr, "GET", HARDWARE, "").await;
    assert_eq!(s, 401);
    let _ = stop.send(());
}

#[tokio::test]
async fn hardware_without_a_model_manager_is_nodes_404() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let (addr, stop) = start(d.path(), &[]).await;
    let (s, _, b) = call(addr, "GET", HARDWARE, &cookie(MEMBER)).await;
    assert_eq!(
        (s, b.as_str()),
        (
            404,
            r#"{"error":"Model manager is disabled. Enter a memory plan manually."}"#
        )
    );
    let (s, _, b) = call(
        addr,
        "GET",
        HARDWARE,
        &format!("Authorization: Bearer {DEVICE}\r\n"),
    )
    .await;
    assert_eq!(s, 404, "{b}");
    let _ = stop.send(());
}

#[tokio::test]
async fn other_methods_trailing_slashes_and_the_other_model_routes_go_to_node() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let (addr, stop) = start(d.path(), &[]).await;
    let csrf = format!(
        "{}X-CSRF-Token: csrf\r\nOrigin: http://{addr}\r\n",
        cookie(ADMIN)
    );
    for (m, p) in [
        ("HEAD", CAPABILITIES),
        ("POST", CAPABILITIES),
        ("POST", HARDWARE),
        ("PUT", HARDWARE),
        ("GET", "/api/models/capabilities/"),
        ("GET", "/api/models/hardware/"),
        ("GET", "/api/models/installed"),
        ("GET", "/api/models/evidence?model=m"),
        ("GET", "/api/models/estimate?model=m"),
        ("GET", "/api/models/inference-budget"),
        ("GET", "/api/models/autotune"),
        ("GET", "/api/models/calibration"),
        ("GET", "/api/models/machines"),
        ("GET", "/api/model-manager/models"),
    ] {
        let (s, _, b) = call(addr, m, p, &csrf).await;
        assert_eq!(s, 502, "{m} {p}: {b}");
    }
    let _ = stop.send(());
}
