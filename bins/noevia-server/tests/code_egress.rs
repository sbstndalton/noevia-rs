//! Row V over HTTP: the front is the only issuer of the egress proxy's control key and signed task
//! grants. The internal `POST /v1/egress/*` routes answer on the project-store listener behind its
//! secret; a fake proxy serves the real wire (TCP, raw HTTP/1.1) and reads the published key on every
//! request, as the proxy does. Synthetic keys; no real proxy, model or production state.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use egress::control::{derive_epoch_keys, FileKey};
use egress_issuer::testing::{FakeProxy, ProxyTransport, BOOT, NODE_SESSION};
use egress_issuer::transport::{Transport, TransportRequest};
use http_body_util::BodyExt;
use noevia_server::project_store::{self, Store, TOKEN_HEADER};
use noevia_server::{config::Config, App};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;

const TOKEN: &str = "synthetic-egress-issuer-secret-0000000";
const START: &str = "0123456789abcdef0123456789abcdef";

/// Serves the fake proxy on loopback: one request per connection, `connection: close`.
async fn serve_proxy(proxy: Arc<FakeProxy>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let proxy = Arc::clone(&proxy);
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut chunk = [0u8; 4096];
                let end = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&chunk[..n]);
                    if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at;
                    }
                };
                let head = String::from_utf8_lossy(&data[..end]).to_string();
                let mut lines = head.split("\r\n");
                let route = lines.next().unwrap().split(' ').nth(1).unwrap().to_string();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(n, v)| (n.to_string(), v.trim().to_string()))
                    .filter(|(n, _)| n != "host")
                    .collect();
                let length: usize = headers
                    .iter()
                    .find(|(n, _)| n == "content-length")
                    .unwrap()
                    .1
                    .parse()
                    .unwrap();
                let mut body = data[end + 4..].to_vec();
                while body.len() < length {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                let route: &'static str = Box::leak(route.into_boxed_str());
                let reply = ProxyTransport(proxy)
                    .post(TransportRequest {
                        endpoint: String::new(),
                        route,
                        headers,
                        body,
                        timeout_ms: 5000.0,
                    })
                    .await;
                // A transport failure hangs up.
                if let Ok(r) = reply {
                    let mut out = format!("HTTP/1.1 {} X\r\n", r.status);
                    for (n, v) in &r.raw_headers {
                        out.push_str(&format!("{n}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.write_all(&r.body).await;
                }
            });
        }
    });
    port
}

struct Rig {
    _root: tempfile::TempDir,
    data: PathBuf,
    key_file: PathBuf,
    env: HashMap<String, String>,
}

fn rig(proxy_port: Option<u16>) -> Option<Rig> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    if gid == 0 {
        eprintln!("skipped: the 0440 key needs a non-root group");
        return None;
    }
    let root = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(root.path()).unwrap();
    let (data, keydir) = (base.join("data"), base.join("key"));
    // The private issuer state lives in `data`: its chain must be the issuer's own and not writable
    // by group or other.
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(&keydir).unwrap();
    std::os::unix::fs::chown(&keydir, Some(uid), Some(gid)).unwrap();
    std::fs::set_permissions(&keydir, std::fs::Permissions::from_mode(0o750)).unwrap();
    std::fs::write(data.join("secrets.key"), [3u8; 32]).unwrap();
    let key_file = keydir.join("current.key");
    let mut env: HashMap<String, String> = [
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", data.display().to_string()),
        ("PUBLIC_ORIGIN", "http://127.0.0.1:1".to_string()),
        ("NOEVIA_PROJECT_STORE_PORT", "19021".to_string()),
        ("NOEVIA_PROJECT_STORE_TOKEN", TOKEN.to_string()),
        ("NOEVIA_START_ID", START.to_string()),
        ("CODE_EGRESS_PORT", proxy_port.unwrap_or(9).to_string()),
        ("CODE_EGRESS_IMPL", "rust".to_string()),
        ("CODE_EGRESS_RUST_HOST", "127.0.0.1".to_string()),
        (
            "CODE_EGRESS_CONTROL_KEY_FILE",
            key_file.display().to_string(),
        ),
        ("CODE_EGRESS_KEY_UID", uid.to_string()),
        ("CODE_EGRESS_KEY_GID", gid.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    env.insert("HOME".into(), base.display().to_string());
    Some(Rig {
        _root: root,
        data,
        key_file,
        env,
    })
}

/// Where the private issuer state lives by default: its own directory inside the key directory.
fn state_file(r: &Rig) -> PathBuf {
    r.key_file
        .parent()
        .unwrap()
        .join("issuer")
        .join("code-egress-issuer.json")
}

fn app_of(env: &HashMap<String, String>) -> Arc<App> {
    App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap())
}

fn router(app: &Arc<App>, data: &Path) -> axum::Router {
    let store = Store {
        data_dir: data.into(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: START.into(),
    };
    project_store::router_with_app(store, None, Some(Arc::clone(app)))
}

async fn call(
    app: &axum::Router,
    op: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(format!("/v1/egress/{op}"))
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header(TOKEN_HEADER, t);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn until_available(router: &axum::Router) {
    for _ in 0..200 {
        let (_, s) = call(router, "status", Some(TOKEN), json!({})).await;
        if s["available"] == true {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the issuer never became available");
}

fn proxy_accepts(file: &Path, session: &str, token: &str) -> bool {
    let key = FileKey::from_hex(std::fs::read_to_string(file).unwrap().trim()).unwrap();
    let keys = derive_epoch_keys(&key, &hex16(BOOT), &hex16(session)).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    egress::verify(&keys.grant, token.as_bytes(), now).is_ok()
}
fn hex16(text: &str) -> [u8; 16] {
    egress_issuer::wire::unhex::<16>(text).unwrap()
}

#[test]
fn the_capability_is_advertised_for_the_release_pairing() {
    assert!(noevia_server::FEATURES.contains(&"rust-code-egress-issuer"));
}

#[tokio::test]
async fn every_route_needs_the_store_secret_and_an_unconfigured_deployment_is_unavailable() {
    let Some(mut r) = rig(None) else { return };
    r.env.remove("CODE_EGRESS_PORT");
    let app = app_of(&r.env);
    assert!(app.code_egress.is_none());
    let router = router(&app, &r.data);
    for op in ["status", "grant", "activity", "revoke", "forget"] {
        for token in [None, Some("wrong-secret-wrong-secret-wrong-secret")] {
            let (status, _) = call(&router, op, token, json!({})).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{op}");
        }
    }
    let (status, body) = call(&router, "status", Some(TOKEN), json!({})).await;
    assert_eq!(
        (status, &body["configured"], &body["available"]),
        (StatusCode::OK, &json!(false), &json!(false))
    );
    let (status, body) = call(
        &router,
        "grant",
        Some(TOKEN),
        json!({ "taskId": "t", "domains": ["example.com"] }),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("EGRESS_UNAVAILABLE"))
    );
    assert!(!r.key_file.exists(), "nothing is published without egress");
}

#[tokio::test]
async fn grants_are_issued_revoked_and_forgotten_through_the_front_only() {
    let Some(r) = rig(Some(1)) else { return };
    let proxy = FakeProxy::new(r.key_file.clone());
    let port = serve_proxy(Arc::clone(&proxy)).await;
    let mut env = r.env.clone();
    env.insert("CODE_EGRESS_PORT".into(), port.to_string());
    let app = app_of(&env);
    let router = router(&app, &r.data);
    // Before it is started nothing is available and nothing is issued.
    let (_, s) = call(&router, "status", Some(TOKEN), json!({})).await;
    assert_eq!(
        (&s["configured"], &s["available"]),
        (&json!(true), &json!(false))
    );
    let (st, b) = call(
        &router,
        "grant",
        Some(TOKEN),
        json!({ "taskId": "task:1", "domains": ["example.com"] }),
    )
    .await;
    assert_eq!(
        (st, b["code"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("EGRESS_UNAVAILABLE"))
    );
    app.code_egress.as_ref().unwrap().start();
    until_available(&router).await;
    // The published key keeps its contract and the private state is 0600.
    let meta = std::fs::metadata(&r.key_file).unwrap();
    assert_eq!((meta.mode() & 0o7777, meta.nlink()), (0o440, 1));
    assert_eq!(
        std::fs::metadata(state_file(&r)).unwrap().mode() & 0o7777,
        0o600
    );
    // A grant: bytes the proxy verifies, domains normalised, no key anywhere in the answer.
    let (st, g) = call(
        &router,
        "grant",
        Some(TOKEN),
        json!({ "taskId": "task:1", "domains": [".EXAMPLE.com.", "api.example.com"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{g}");
    assert_eq!(g["domains"], json!(["example.com", "api.example.com"]));
    let token = g["token"].as_str().unwrap().to_string();
    let session = proxy.session().unwrap();
    assert!(proxy_accepts(&r.key_file, &session, &token));
    let key_hex = std::fs::read_to_string(&r.key_file)
        .unwrap()
        .trim()
        .to_string();
    assert!(!g.to_string().contains(&key_hex));
    let handle = g["handle"].as_str().unwrap().to_string();
    let (st, a) = call(
        &router,
        "activity",
        Some(TOKEN),
        json!({ "handle": handle }),
    )
    .await;
    assert_eq!((st, &a["report"]["final"]), (StatusCode::OK, &json!(false)));
    let (st, v) = call(&router, "revoke", Some(TOKEN), json!({ "handle": handle })).await;
    assert_eq!(
        (st, &v["confirmed"], &v["report"]["final"]),
        (StatusCode::OK, &json!(true), &json!(true))
    );
    let snapshot = v["report"]["snapshotId"].as_str().unwrap().to_string();
    let (st, f) = call(
        &router,
        "forget",
        Some(TOKEN),
        json!({ "handle": handle, "snapshotId": "00".repeat(16) }),
    )
    .await;
    assert_eq!(
        (st, f["reason"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("snapshot"))
    );
    let (st, f) = call(
        &router,
        "forget",
        Some(TOKEN),
        json!({ "handle": handle, "snapshotId": snapshot }),
    )
    .await;
    assert_eq!((st, &f["confirmed"]), (StatusCode::OK, &json!(true)));
    // Bad input is refused, not guessed at.
    for (op, body) in [
        (
            "grant",
            json!({ "taskId": "bad task", "domains": ["example.com"] }),
        ),
        ("grant", json!({ "taskId": "t", "domains": [] })),
        ("grant", json!({ "taskId": "t", "domains": ["not a host"] })),
        ("revoke", json!({ "handle": "nope" })),
    ] {
        let (st, b) = call(&router, op, Some(TOKEN), body.clone()).await;
        assert_eq!(
            (st, b["code"].as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, Some("EGRESS_UNAVAILABLE")),
            "{op} {body}"
        );
    }
    let (st, _) = call(&router, "grant", Some(TOKEN), json!({ "taskId": 7 })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let ready_modes = proxy.state.lock().unwrap().ready_modes.clone();
    assert_eq!(ready_modes, ["initial"]);
}

#[tokio::test]
async fn a_restarted_front_keeps_the_key_and_refuses_the_old_incarnations_handles() {
    let Some(r) = rig(Some(1)) else { return };
    let proxy = FakeProxy::new(r.key_file.clone());
    let port = serve_proxy(Arc::clone(&proxy)).await;
    let mut env = r.env.clone();
    env.insert("CODE_EGRESS_PORT".into(), port.to_string());
    let first = app_of(&env);
    let r1 = router(&first, &r.data);
    first.code_egress.as_ref().unwrap().start();
    until_available(&r1).await;
    let (_, g) = call(
        &r1,
        "grant",
        Some(TOKEN),
        json!({ "taskId": "task:2", "domains": ["example.com"] }),
    )
    .await;
    let (handle, old_token) = (
        g["handle"].as_str().unwrap().to_string(),
        g["token"].as_str().unwrap().to_string(),
    );
    let old_session = proxy.session().unwrap();
    let published = std::fs::read(&r.key_file).unwrap();
    let inode = std::fs::metadata(&r.key_file).unwrap().ino();
    drop((first, r1));
    // The front restarts: same key (no silent rotation), its own session is replaced.
    let second = app_of(&env);
    let r2 = router(&second, &r.data);
    second.code_egress.as_ref().unwrap().start();
    until_available(&r2).await;
    assert_eq!(std::fs::read(&r.key_file).unwrap(), published);
    assert_eq!(std::fs::metadata(&r.key_file).unwrap().ino(), inode);
    assert_eq!(
        proxy.state.lock().unwrap().ready_modes,
        ["initial", "replace"]
    );
    let new_session = proxy.session().unwrap();
    assert_ne!(new_session, old_session);
    assert!(
        !proxy_accepts(&r.key_file, &new_session, &old_token),
        "a grant of the replaced session verifies nowhere"
    );
    // The old handle is unknown to the new incarnation: refused, never re-acquired.
    let (st, b) = call(&r2, "revoke", Some(TOKEN), json!({ "handle": handle })).await;
    assert_eq!(
        (st, b["reason"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("handle"))
    );
    let (st, g2) = call(
        &r2,
        "grant",
        Some(TOKEN),
        json!({ "taskId": "task:2", "domains": ["example.com"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(proxy_accepts(
        &r.key_file,
        &new_session,
        g2["token"].as_str().unwrap()
    ));
}

#[tokio::test]
async fn an_unreachable_proxy_refuses_grants_and_reports_unavailable() {
    let Some(r) = rig(None) else { return }; // port 9 (discard): nothing listens
    let app = app_of(&r.env);
    let router = router(&app, &r.data);
    app.code_egress.as_ref().unwrap().start();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let (_, s) = call(&router, "status", Some(TOKEN), json!({})).await;
    assert_eq!(
        (&s["configured"], &s["available"]),
        (&json!(true), &json!(false))
    );
    let (st, b) = call(
        &router,
        "grant",
        Some(TOKEN),
        json!({ "taskId": "task:3", "domains": ["example.com"] }),
    )
    .await;
    assert_eq!(
        (st, b["code"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("EGRESS_UNAVAILABLE"))
    );
    // The key is published for the proxy to find when it arrives. Publication is asynchronous
    // to the first grant's answer (sbstndalton/noevia#2433), so wait for it, bounded.
    let published = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(m) = std::fs::metadata(&r.key_file) {
                return m;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(published.mode() & 0o7777, 0o440);
}

#[tokio::test]
async fn an_unprepared_key_directory_is_never_created_or_repaired() {
    let Some(r) = rig(None) else { return };
    let keydir = r.key_file.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&keydir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let app = app_of(&r.env);
    let router = router(&app, &r.data);
    app.code_egress.as_ref().unwrap().start();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let (_, s) = call(&router, "status", Some(TOKEN), json!({})).await;
    assert_eq!(s["available"], json!(false));
    assert_eq!(
        std::fs::metadata(&keydir).unwrap().mode() & 0o7777,
        0o777,
        "not repaired"
    );
    assert!(!r.key_file.exists());
    assert!(
        !keydir.join("issuer").exists(),
        "no state directory in an untrusted one"
    );
}

/// The key the retired Node issuer left published (bytes written by the real Node code, see
/// `egress-issuer/tests/fixtures/node-published-key.json`) is served unchanged on a first start with no
/// Rust state, to a proxy that never saw Node's Ready: no rotation, no operator step, Node's file
/// untouched, grants the proxy verifies. (A proxy Node already readied: the next test.)
#[tokio::test]
async fn a_key_node_left_published_is_served_unchanged_on_the_first_start() {
    let Some(r) = rig(Some(1)) else { return };
    let doc: Value = serde_json::from_str(include_str!(
        "../../../crates/egress-issuer/tests/fixtures/node-published-key.json"
    ))
    .unwrap();
    let key_hex = doc["keyHex"].as_str().unwrap().to_string();
    let node_bytes = format!("{key_hex}\n");
    std::fs::write(&r.key_file, &node_bytes).unwrap();
    std::fs::set_permissions(&r.key_file, std::fs::Permissions::from_mode(0o440)).unwrap();
    let before = std::fs::metadata(&r.key_file).unwrap();
    let proxy = FakeProxy::new(r.key_file.clone());
    let port = serve_proxy(Arc::clone(&proxy)).await;
    let mut env = r.env.clone();
    env.insert("CODE_EGRESS_PORT".into(), port.to_string());
    let app = app_of(&env);
    let router = router(&app, &r.data);
    app.code_egress.as_ref().unwrap().start();
    until_available(&router).await;
    let after = std::fs::metadata(&r.key_file).unwrap();
    assert_eq!(std::fs::read_to_string(&r.key_file).unwrap(), node_bytes);
    assert_eq!(
        (
            after.ino(),
            after.mode() & 0o7777,
            after.mtime(),
            after.ctime()
        ),
        (before.ino(), 0o440, before.mtime(), before.ctime())
    );
    assert_eq!(
        std::fs::metadata(state_file(&r)).unwrap().mode() & 0o7777,
        0o600,
        "the state is kept in the issuer's own directory beside the key, not in UI_DATA_DIR"
    );
    assert!(!r.data.join("code-egress-issuer.json").exists());
    let (st, g) = call(
        &router,
        "grant",
        Some(TOKEN),
        json!({ "taskId": "task:1", "domains": ["example.com"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let session = proxy.session().unwrap();
    assert!(proxy_accepts(
        &r.key_file,
        &session,
        g["token"].as_str().unwrap()
    ));
}

/// The live cutover: the retained proxy is already initialized under the key Node published (Node did
/// its Ready under it) and refuses Initial until the key changes. The front adopts that key, has no
/// session to Replace, rotates once at once and is available well inside the 25 s conflict wait. The
/// new key keeps the published contract (0440, one link, same owner, replaced by rename), so the old
/// image can still overwrite it on a rollback.
#[tokio::test]
async fn a_key_node_left_published_under_a_proxy_node_readied_is_rotated_once_at_once() {
    let Some(r) = rig(Some(1)) else { return };
    let doc: Value = serde_json::from_str(include_str!(
        "../../../crates/egress-issuer/tests/fixtures/node-published-key.json"
    ))
    .unwrap();
    let node_bytes = format!("{}\n", doc["keyHex"].as_str().unwrap());
    std::fs::write(&r.key_file, &node_bytes).unwrap();
    std::fs::set_permissions(&r.key_file, std::fs::Permissions::from_mode(0o440)).unwrap();
    let before = std::fs::metadata(&r.key_file).unwrap();
    let proxy = FakeProxy::initialized_by_node(r.key_file.clone());
    let port = serve_proxy(Arc::clone(&proxy)).await;
    let mut env = r.env.clone();
    env.insert("CODE_EGRESS_PORT".into(), port.to_string());
    let app = app_of(&env);
    let router = router(&app, &r.data);
    app.code_egress.as_ref().unwrap().start();
    // At most 10 s (the conflict path needed at least 25 s).
    until_available(&router).await;
    {
        let st = proxy.state.lock().unwrap();
        assert_eq!(st.ready_modes, ["initial", "initial"]);
        assert_eq!((st.key_changes, st.sessions_issued), (1, 1));
        assert_ne!(st.session.as_deref(), Some(NODE_SESSION));
    }
    let after = std::fs::metadata(&r.key_file).unwrap();
    assert_ne!(std::fs::read_to_string(&r.key_file).unwrap(), node_bytes);
    assert_eq!(
        (
            after.mode() & 0o7777,
            after.nlink(),
            after.uid(),
            after.gid()
        ),
        (0o440, 1, before.uid(), before.gid())
    );
    assert_ne!(after.ino(), before.ino());
    assert_eq!(
        std::fs::metadata(state_file(&r)).unwrap().mode() & 0o7777,
        0o600
    );
    let (st, g) = call(
        &router,
        "grant",
        Some(TOKEN),
        json!({ "taskId": "task:1", "domains": ["example.com"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let session = proxy.session().unwrap();
    assert!(proxy_accepts(
        &r.key_file,
        &session,
        g["token"].as_str().unwrap()
    ));
}
