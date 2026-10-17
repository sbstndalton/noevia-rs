#![cfg(feature = "rust-stats")]
//! Real HTTP through the native front (M6, #1650) in front of a synthetic llama.cpp router on
//! loopback; a dead legacy upstream makes any proxied request answer 502, so a 200/401/403 here
//! can only come from the native handler. The router counts its `/models` reads, which is how the
//! shared answer (one engine read per 2 s, dropped by a gated model or role write) is observed.
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
use std::sync::atomic::{AtomicUsize, Ordering};
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
const LISTING: &str = r#"{"data":[{"id":"chat-a","status":{"value":"loaded"}},{"id":"idle","status":{"value":"unloaded"}}]}"#;
const METRICS: &str = "llamacpp:prompt_tokens_total 120\nllamacpp:tokens_predicted_total 80\nllamacpp:predicted_tokens_seconds 12.5\nllamacpp:spec_decode_num_draft_tokens_total 40\nllamacpp:spec_decode_num_accepted_tokens_total 25\n";
const BODY: &str = r#"{"up":true,"telemetryScope":"loaded model processes; counters reset on unload","mtp":[{"model":"chat-a","drafted":40,"accepted":25,"rate":0.625,"source":"backend total"}],"tokensPerSecond":null,"timeToFirstToken":null,"inputTokens":null,"outputTokens":null,"inputTokensTotal":120,"outputTokensTotal":80,"requestCount":null,"cpuPercent":null,"gpuPercent":null,"vramGb":null,"memoryGb":null}"#;

/// BODY is what core's own handler sent for this engine (crates/server-stats oracle.cjs, case
/// llamacpp-front-test, at contracts/http/core.ref).
#[test]
fn the_expected_body_is_nodes() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../crates/server-stats/tests/fixtures/node-stats.v1.json"
    ))
    .unwrap();
    let case = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "llamacpp-front-test")
        .unwrap();
    assert_eq!(case["expected"]["status"], 200);
    assert_eq!(case["expected"]["body"], BODY);
    assert_eq!(case["routes"]["/models"]["text"], LISTING);
    assert_eq!(
        case["routes"]["/metrics?model=chat-a&autoload=false"]["text"],
        METRICS
    );
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

/// A synthetic llama.cpp router: `/models` and `/metrics?model=chat-a&autoload=false`, each reply
/// held back by `delay`; counts `/models` reads.
async fn router(delay: Duration) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let listings = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&listings);
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let count = Arc::clone(&count);
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 2048];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&head).to_string();
                let target = text.split(' ').nth(1).unwrap_or("").to_string();
                let body = match target.as_str() {
                    "/models" => {
                        count.fetch_add(1, Ordering::SeqCst);
                        LISTING
                    }
                    "/metrics?model=chat-a&autoload=false" => METRICS,
                    _ => "",
                };
                tokio::time::sleep(delay).await;
                let status = if body.is_empty() {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    (port, listings)
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

/// A loopback port that refuses every connection and that no other test can be handed.
///
/// Binding a listener and dropping it frees the port at once, so another test running in parallel
/// in this binary (a router, a front listener) can be given the same port by `bind("127.0.0.1:0")`
/// before the "dead" engine is polled; the poll then reaches that test's router, answers `up:true`
/// and adds a `/models` read to its count (#2016). A socket that is bound but never listening
/// keeps the port allocated for as long as it lives, and a connect to it is refused.
struct DeadPort {
    _socket: tokio::net::TcpSocket,
    port: u16,
}

impl DeadPort {
    fn new() -> Self {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = socket.local_addr().unwrap().port();
        Self {
            _socket: socket,
            port,
        }
    }

    fn port(&self) -> u16 {
        self.port
    }
}

fn switched_on(port: u16) -> Vec<(&'static str, String)> {
    vec![
        ("MODEL_MANAGER_KIND", "llamacpp".into()),
        (
            "MODEL_MANAGER_BASE_URL",
            format!("http://127.0.0.1:{port}/v1"),
        ),
    ]
}

#[tokio::test]
async fn signed_in_members_and_admins_get_nodes_bytes_signed_out_get_401() {
    let d = tempfile::tempdir().unwrap();
    seed_database(d.path());
    let (engine, listings) = router(Duration::ZERO).await;
    let (addr, stop) = start(d.path(), &switched_on(engine)).await;

    let (status, headers, body) = call(addr, "GET", "/api/stats", "", None).await;
    assert_eq!(
        (status, body.as_str()),
        (401, r#"{"error":"unauthorized"}"#)
    );
    assert!(headers.contains("www-authenticate: bearer realm=\"cowork\""));
    assert_eq!(
        listings.load(Ordering::SeqCst),
        0,
        "a signed-out poll reads nothing"
    );

    let (status, headers, body) = call(addr, "GET", "/api/stats", ADMIN, None).await;
    assert_eq!((status, body.as_str()), (200, BODY));
    for h in [
        "content-type: application/json",
        "cache-control: no-store",
        "x-noevia-api: 1",
        "x-content-type-options: nosniff",
        "strict-transport-security: max-age=15552000",
    ] {
        assert!(headers.contains(h), "{h}");
    }
    // A member gets the same engine-wide answer, from the same shared read.
    let (status, _, body) = call(addr, "GET", "/api/stats", MEMBER, None).await;
    assert_eq!((status, body.as_str()), (200, BODY));
    assert_eq!(listings.load(Ordering::SeqCst), 1);

    // Every other method, and the trailing-slash path Node does not match, stay Node's (502 here).
    for (m, p) in [
        ("HEAD", "/api/stats"),
        ("POST", "/api/stats"),
        ("PUT", "/api/stats"),
        ("DELETE", "/api/stats"),
        ("GET", "/api/stats/"),
    ] {
        assert_eq!(
            call(addr, m, p, ADMIN, Some("csrf")).await.0,
            502,
            "{m} {p}"
        );
    }
    let _ = stop.send(());
}

#[tokio::test]
async fn concurrent_polls_share_one_engine_read_kept_two_seconds() {
    let d = tempfile::tempdir().unwrap();
    seed_database(d.path());
    let (engine, listings) = router(Duration::from_millis(300)).await;
    let (addr, stop) = start(d.path(), &switched_on(engine)).await;
    let polls: Vec<_> = (0..6)
        .map(|i| {
            tokio::spawn(async move {
                call(
                    addr,
                    "GET",
                    "/api/stats",
                    if i % 2 == 0 { ADMIN } else { MEMBER },
                    None,
                )
                .await
            })
        })
        .collect();
    for p in polls {
        let (status, _, body) = p.await.unwrap();
        assert_eq!((status, body.as_str()), (200, BODY));
    }
    assert_eq!(
        listings.load(Ordering::SeqCst),
        1,
        "concurrent pages share one read"
    );
    assert_eq!(call(addr, "GET", "/api/stats", ADMIN, None).await.0, 200);
    assert_eq!(
        listings.load(Ordering::SeqCst),
        1,
        "a poll inside the TTL is served the shared answer"
    );
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(call(addr, "GET", "/api/stats", ADMIN, None).await.0, 200);
    assert_eq!(
        listings.load(Ordering::SeqCst),
        2,
        "after the TTL the next poll reads again"
    );
    let _ = stop.send(());
}

#[tokio::test]
async fn a_gated_model_or_role_write_drops_the_shared_answer_nothing_else_does() {
    let d = tempfile::tempdir().unwrap();
    seed_database(d.path());
    let (engine, listings) = router(Duration::ZERO).await;
    let (addr, stop, app) = start_app(d.path(), &switched_on(engine)).await;
    let stats = app.stats.as_ref().map(Arc::clone).unwrap();
    let poll = || async { call(addr, "GET", "/api/stats", MEMBER, None).await.0 };
    assert_eq!(poll().await, 200);
    assert_eq!(listings.load(Ordering::SeqCst), 1);
    // Refused by Node's gate (no session; a bad CSRF token), a read, or another path: kept.
    // Judged by the drop count, not by engine reads: on a slow runner the 2 s TTL can run out
    // inside this loop and the next poll reads again without anything being dropped (#1970).
    for (method, path, cookie, csrf) in [
        ("POST", "/api/auto-roles", "", None),
        ("POST", "/api/models/load", ADMIN, Some("wrong")),
        ("PUT", "/api/auto-roles", MEMBER, None),
        ("GET", "/api/models/downloads", ADMIN, None),
        ("POST", "/api/modelsx", ADMIN, Some("csrf")),
        ("POST", "/api/stats", ADMIN, Some("csrf")),
    ] {
        let _ = call(addr, method, path, cookie, csrf).await;
        assert_eq!(poll().await, 200);
        assert_eq!(stats.drops(), 0, "{method} {path} {cookie:?} {csrf:?}");
    }
    // A member's role save, an admin's model write: dropped, the next poll reads the engine again
    // (the writes themselves go to Node: 502 behind this dead upstream).
    let mut expected = listings.load(Ordering::SeqCst);
    let mut dropped = 0;
    for (method, path, cookie) in [
        ("PUT", "/api/auto-roles", MEMBER),
        ("POST", "/api/models/load", ADMIN),
        ("DELETE", "/api/model-manager/sections/x", ADMIN),
        ("POST", "/api/models/../models/unload", ADMIN),
    ] {
        assert_eq!(call(addr, method, path, cookie, Some("csrf")).await.0, 502);
        dropped += 1;
        assert_eq!(stats.drops(), dropped, "{method} {path}");
        assert_eq!(poll().await, 200);
        expected += 1;
        assert_eq!(listings.load(Ordering::SeqCst), expected, "{method} {path}");
    }
    let _ = stop.send(());
}

#[tokio::test]
async fn signed_out_polls_get_401_even_when_every_gate_permit_is_taken() {
    let d = tempfile::tempdir().unwrap();
    seed_database(d.path());
    let (engine, listings) = router(Duration::ZERO).await;
    let (addr, stop, app) = start_app(d.path(), &switched_on(engine)).await;
    let stats = app.stats.as_ref().map(Arc::clone).unwrap();
    let held = stats.hold_every_permit();
    // No credential at all is Node's 401 before a permit is taken (#1654), not "busy".
    for cookie in ["", "cowork_csrf=x; theme=dark", "cowork_session="] {
        let (status, headers, body) = call(addr, "GET", "/api/stats", cookie, None).await;
        assert_eq!(
            (status, body.as_str()),
            (401, r#"{"error":"unauthorized"}"#),
            "{cookie:?}"
        );
        assert!(headers.contains("www-authenticate: bearer realm=\"cowork\""));
    }
    // A credential still needs the gate, so with every permit taken it is told to retry.
    assert_eq!(call(addr, "GET", "/api/stats", MEMBER, None).await.0, 503);
    assert_eq!(listings.load(Ordering::SeqCst), 0);
    drop(held);
    assert_eq!(call(addr, "GET", "/api/stats", MEMBER, None).await.0, 200);
    assert_eq!(listings.load(Ordering::SeqCst), 1);
    let _ = stop.send(());
}

#[test]
fn an_invalid_auth_config_refuses_startup_instead_of_serving_stats() {
    // sbstndalton/noevia#1071: GET /api/stats is always the front's, so an auth
    // environment it cannot check refuses startup (Node refuses to start on it too).
    let d = tempfile::tempdir().unwrap();
    let env: HashMap<&str, String> = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR", d.path().display().to_string()),
        ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "maybe".into()),
    ]);
    let err = Config::from_lookup(|k| env.get(k).cloned()).unwrap_err();
    assert!(err.contains("valid native auth"), "{err}");
}

#[tokio::test]
async fn model_writes_past_the_invalidation_bound_skip_the_drop_instead_of_queueing() {
    let d = tempfile::tempdir().unwrap();
    seed_database(d.path());
    let (engine, listings) = router(Duration::ZERO).await;
    let (addr, stop, app) = start_app(d.path(), &switched_on(engine)).await;
    let stats = app.stats.as_ref().map(Arc::clone).unwrap();
    assert_eq!(call(addr, "GET", "/api/stats", ADMIN, None).await.0, 200);
    assert_eq!(listings.load(Ordering::SeqCst), 1);
    // Every invalidation permit taken: an admin's model write is still proxied (502 here) but
    // the drop is skipped, so the next poll inside the TTL keeps the shared answer.
    let held = stats.hold_every_permit();
    assert_eq!(
        call(addr, "POST", "/api/models/load", ADMIN, Some("csrf"))
            .await
            .0,
        502
    );
    drop(held);
    // The drop count, not engine reads, so a runner slower than the TTL cannot blur it (#1970).
    assert_eq!(stats.drops(), 0);
    assert_eq!(call(addr, "GET", "/api/stats", ADMIN, None).await.0, 200);
    let reads = listings.load(Ordering::SeqCst);
    // With permits free the same write drops it again.
    assert_eq!(
        call(addr, "POST", "/api/models/load", ADMIN, Some("csrf"))
            .await
            .0,
        502
    );
    assert_eq!(stats.drops(), 1);
    assert_eq!(call(addr, "GET", "/api/stats", ADMIN, None).await.0, 200);
    assert_eq!(listings.load(Ordering::SeqCst), reads + 1);
    let _ = stop.send(());
}

#[tokio::test]
async fn a_dead_engine_is_up_false_and_a_retired_switch_value_keeps_the_route_native() {
    let d = tempfile::tempdir().unwrap();
    seed_database(d.path());
    // Held to the end of the test: see `DeadPort`.
    let dead = DeadPort::new();
    let (addr, stop) = start(d.path(), &switched_on(dead.port())).await;
    let (status, _, body) = call(addr, "GET", "/api/stats", ADMIN, None).await;
    assert_eq!(status, 200);
    assert!(
        body.starts_with(r#"{"up":false,"telemetryScope":null,"mtp":[]"#),
        "{body}"
    );
    let _ = stop.send(());

    // sbstndalton/noevia#1071: NOEVIA_RUST_STATS is retired; "0" still leaves GET native, and
    // the role write stays Node's (the dead upstream's 502).
    let (engine, listings) = router(Duration::ZERO).await;
    let mut off = switched_on(engine);
    off.push(("NOEVIA_RUST_STATS", "0".into()));
    let (addr, stop) = start(d.path(), &off).await;
    assert_eq!(call(addr, "GET", "/api/stats", ADMIN, None).await.0, 200);
    assert_eq!(
        call(addr, "PUT", "/api/auto-roles", ADMIN, Some("csrf"))
            .await
            .0,
        502
    );
    assert_eq!(listings.load(Ordering::SeqCst), 1);
    let _ = stop.send(());
}

#[tokio::test]
async fn a_dead_port_refuses_connections_and_cannot_be_taken_by_another_listener() {
    let dead = DeadPort::new();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], dead.port()));
    let refused = TcpStream::connect(addr).await.unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionRefused);
    // Neither an explicit bind of the port nor a burst of OS-assigned ones ever lands on it.
    assert!(TcpListener::bind(addr).await.is_err());
    let mut taken = Vec::new();
    for _ in 0..256 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert_ne!(l.local_addr().unwrap().port(), dead.port());
        taken.push(l);
    }
}
