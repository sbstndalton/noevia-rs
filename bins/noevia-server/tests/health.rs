//! `GET /api/health` through the real front (sbstndalton/noevia#2190 close-out, capability
//! `rust-health`): the gate, the default provider's probe and the Diary sidecar's, the retrieval
//! snapshot Node publishes, the answer's shape, the bounds, and a client that goes away. Synthetic
//! stand-ins on loopback only (a provider, a Diary sidecar); the legacy upstream is dead, so a
//! request that fell through to Node would answer 502. No model, no production service.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    dead_code
)]
use http_body_util::Full;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const COOKIE_A: &str = "cowork_session=session-a; cowork_csrf=csrf";
const COOKIE_B: &str = "cowork_session=session-b; cowork_csrf=csrf";
const BEARER: &str = "synthetic-diary-bearer";
const TENANT_KEY: &str = "synthetic-diary-tenant-key";
const NOW_FIELD: i64 = 0;

/// One test at a time: the default provider is read from the process environment.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    target: String,
    headers: HashMap<String, String>,
}

#[derive(Clone)]
struct Canned {
    status: u16,
    delay: Duration,
    location: Option<String>,
    body: Vec<u8>,
}

impl Canned {
    fn ok() -> Self {
        Canned {
            status: 200,
            delay: Duration::ZERO,
            location: None,
            body: br#"{"data":[]}"#.to_vec(),
        }
    }
    fn status(status: u16) -> Self {
        Canned {
            status,
            ..Canned::ok()
        }
    }
    fn slow(delay: Duration) -> Self {
        Canned {
            delay,
            ..Canned::ok()
        }
    }
}

struct Stand {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    canned: Arc<Mutex<Canned>>,
}

impl Stand {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
    fn set(&self, c: Canned) {
        *self.canned.lock().unwrap() = c;
    }
}

/// A stand-in HTTP server that records each request and answers what it was told to.
async fn stand(canned: Canned) -> Stand {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
    let canned = Arc::new(Mutex::new(canned));
    let (log, answer) = (Arc::clone(&seen), Arc::clone(&canned));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (log, answer) = (Arc::clone(&log), Arc::clone(&answer));
            tokio::spawn(async move {
                let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let (log, answer) = (Arc::clone(&log), Arc::clone(&answer));
                    async move {
                        log.lock().unwrap().push(Seen {
                            method: req.method().to_string(),
                            target: req.uri().path_and_query().unwrap().as_str().to_owned(),
                            headers: req
                                .headers()
                                .iter()
                                .map(|(k, v)| {
                                    (k.as_str().to_owned(), v.to_str().unwrap().to_owned())
                                })
                                .collect(),
                        });
                        let c = answer.lock().unwrap().clone();
                        tokio::time::sleep(c.delay).await;
                        let mut b = hyper::Response::builder()
                            .status(c.status)
                            .header("content-type", "application/json");
                        if let Some(l) = &c.location {
                            b = b.header("location", l);
                        }
                        Ok::<_, Infallible>(
                            b.body(Full::new(bytes::Bytes::from(c.body.clone())))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Stand { base, seen, canned }
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

async fn front(dir: &std::path::Path, diary_base: &str) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        ("DIARY_BASE_URL", diary_base.to_string()),
        ("DIARY_AUTH_TOKEN", BEARER.to_string()),
        ("DIARY_TENANT_KEY", TENANT_KEY.to_string()),
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

fn set_provider(base: &str, key: Option<&str>) {
    std::env::set_var("INFERENCE_BASE_URL", base);
    match key {
        Some(k) => std::env::set_var("INFERENCE_API_KEY", k),
        None => std::env::remove_var("INFERENCE_API_KEY"),
    }
    std::env::remove_var("LEMONADE_BASE_URL");
    std::env::remove_var("LEMONADE_API_KEY");
}

/// What Node's publisher writes (core published-snapshot.cjs), at `written_at`.
fn publish(dir: &std::path::Path, rag: bool, word: &str, written_at: i64, every_ms: i64) {
    std::fs::write(
        dir.join("rag-status.json"),
        format!(
            r#"{{"version":1,"kind":"rag-status","writtenAt":{written_at},"everyMs":{every_ms},"ragAvailable":{rag},"retrieval":"{word}"}}"#
        ),
    )
    .unwrap();
}

fn fresh(dir: &std::path::Path, rag: bool, word: &str) {
    publish(dir, rag, word, noevia_server::identity::now_ms(), 10_000);
}

fn data() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    dir
}

const SHAPE_UP: &str = r#"{"inferenceUp":true,"lemonadeUp":true,"diaryUp":null,"ragAvailable":true,"retrieval":"available"}"#;

#[tokio::test]
async fn the_answer_is_nodes_for_a_signed_in_person_and_the_gate_is_nodes() {
    let _env = ENV.lock().await;
    let dir = data();
    let provider = stand(Canned::ok()).await;
    set_provider(&format!("{}/v1/", provider.base), Some("sk-synthetic-key"));
    fresh(dir.path(), true, "available");
    let addr = front(dir.path(), "http://127.0.0.1:9").await;

    // Not signed in: the gate's 401, and nothing is probed.
    let r = call(addr, "GET", "/api/health", "").await;
    assert_eq!(r.status, 401);
    assert_eq!(r.text, r#"{"error":"unauthorized"}"#);
    assert_eq!(r.headers["www-authenticate"], "Bearer realm=\"cowork\"");
    assert!(
        provider.seen().is_empty(),
        "an unauthenticated call probes nothing"
    );
    let r = call(addr, "GET", "/api/health", "cowork_session=nope").await;
    assert_eq!(r.status, 401);
    assert!(provider.seen().is_empty());

    // Signed in: core's body, its key order and its headers.
    let r = call(addr, "GET", "/api/health", COOKIE_B).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.text, SHAPE_UP);
    assert_eq!(r.headers["content-type"], "application/json");
    assert_eq!(r.headers["cache-control"], "no-store");
    assert_eq!(r.headers["x-noevia-api"], "1");
    assert_eq!(r.headers["x-content-type-options"], "nosniff");
    // The provider saw one `GET /v1/models` with core's headers (the base's trailing slash and `/v1` are dropped).
    let seen = provider.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].target.as_str()),
        ("GET", "/v1/models")
    );
    assert_eq!(seen[0].headers["authorization"], "Bearer sk-synthetic-key");
    assert_eq!(seen[0].headers["content-type"], "application/json");
}

#[tokio::test]
async fn a_key_of_local_or_no_key_sends_no_bearer() {
    let _env = ENV.lock().await;
    let dir = data();
    let provider = stand(Canned::ok()).await;
    fresh(dir.path(), true, "available");
    let addr = front(dir.path(), "http://127.0.0.1:9").await;
    for key in [Some("local"), None] {
        set_provider(&provider.base, key);
        assert_eq!(
            call(addr, "GET", "/api/health", COOKIE_B).await.text,
            SHAPE_UP
        );
    }
    assert!(provider
        .seen()
        .iter()
        .all(|s| !s.headers.contains_key("authorization")));
}

#[tokio::test]
async fn the_provider_is_up_only_on_a_2xx_within_five_seconds() {
    let _env = ENV.lock().await;
    let dir = data();
    fresh(dir.path(), true, "available");
    let provider = stand(Canned::ok()).await;
    set_provider(&provider.base, None);
    let addr = front(dir.path(), "http://127.0.0.1:9").await;
    let down = r#"{"inferenceUp":false,"lemonadeUp":false,"diaryUp":null,"ragAvailable":true,"retrieval":"available"}"#;
    for status in [500, 404, 401, 503] {
        provider.set(Canned::status(status));
        assert_eq!(
            call(addr, "GET", "/api/health", COOKIE_B).await.text,
            down,
            "{status}"
        );
    }
    // A redirect is a failure (core: `redirect: 'error'`) and is not followed.
    let elsewhere = stand(Canned::ok()).await;
    provider.set(Canned {
        location: Some(format!("{}/v1/models", elsewhere.base)),
        ..Canned::status(302)
    });
    assert_eq!(call(addr, "GET", "/api/health", COOKIE_B).await.text, down);
    assert!(elsewhere.seen().is_empty(), "the redirect was not followed");
    provider.set(Canned::status(204));
    assert_eq!(
        call(addr, "GET", "/api/health", COOKIE_B).await.text,
        SHAPE_UP
    );
    // Nothing listens: down, quickly.
    set_provider("http://127.0.0.1:9", None);
    let started = Instant::now();
    assert_eq!(call(addr, "GET", "/api/health", COOKIE_B).await.text, down);
    assert!(started.elapsed() < Duration::from_secs(4));
    // A reply that takes longer than the five seconds is down at five seconds, and the answer does not wait for it.
    let slow = stand(Canned::slow(Duration::from_secs(9))).await;
    set_provider(&slow.base, None);
    let started = Instant::now();
    assert_eq!(call(addr, "GET", "/api/health", COOKIE_B).await.text, down);
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(4) && took < Duration::from_secs(7),
        "{took:?}"
    );
    // A reply over core's 8 MiB cap is not "up" either.
    let big = stand(Canned {
        body: vec![b' '; 8 * 1024 * 1024 + 1],
        ..Canned::ok()
    })
    .await;
    set_provider(&big.base, None);
    assert_eq!(call(addr, "GET", "/api/health", COOKIE_B).await.text, down);
}

#[tokio::test]
async fn the_diary_is_probed_only_for_an_account_with_the_add_on() {
    let _env = ENV.lock().await;
    let dir = data();
    fresh(dir.path(), false, "unavailable");
    let provider = stand(Canned::ok()).await;
    set_provider(&provider.base, None);
    let diary = stand(Canned::ok()).await;
    let addr = front(dir.path(), &diary.base).await;

    // Bob has no add-on: null, and the sidecar is never asked.
    let r = call(addr, "GET", "/api/health", COOKIE_B).await;
    assert_eq!(
        r.text,
        r#"{"inferenceUp":true,"lemonadeUp":true,"diaryUp":null,"ragAvailable":false,"retrieval":"unavailable"}"#
    );
    assert!(diary.seen().is_empty());

    // Ada has: true on a 2xx, with the bearer and her tenant headers.
    let r = call(addr, "GET", "/api/health", COOKIE_A).await;
    assert_eq!(
        r.text,
        r#"{"inferenceUp":true,"lemonadeUp":true,"diaryUp":true,"ragAvailable":false,"retrieval":"unavailable"}"#
    );
    let seen = diary.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].target.as_str()),
        ("GET", "/api/health")
    );
    assert_eq!(seen[0].headers["authorization"], format!("Bearer {BEARER}"));
    assert_eq!(seen[0].headers["x-cowork-user-id"], A);

    // Each probe settles on its own: the sidecar down does not hide the provider, nor the reverse.
    diary.set(Canned::status(500));
    let r = call(addr, "GET", "/api/health", COOKIE_A).await;
    assert!(
        r.text.contains(r#""inferenceUp":true"#) && r.text.contains(r#""diaryUp":false"#),
        "{}",
        r.text
    );
    diary.set(Canned::ok());
    provider.set(Canned::status(500));
    let r = call(addr, "GET", "/api/health", COOKIE_A).await;
    assert!(
        r.text.contains(r#""inferenceUp":false"#) && r.text.contains(r#""diaryUp":true"#),
        "{}",
        r.text
    );
}

#[tokio::test]
async fn the_two_probes_run_together_and_a_slow_one_does_not_hide_the_other() {
    let _env = ENV.lock().await;
    let dir = data();
    fresh(dir.path(), true, "available");
    let provider = stand(Canned::slow(Duration::from_millis(1500))).await;
    set_provider(&provider.base, None);
    let diary = stand(Canned::slow(Duration::from_millis(1500))).await;
    let addr = front(dir.path(), &diary.base).await;
    let started = Instant::now();
    let r = call(addr, "GET", "/api/health", COOKIE_A).await;
    let took = started.elapsed();
    assert_eq!(
        r.text,
        r#"{"inferenceUp":true,"lemonadeUp":true,"diaryUp":true,"ragAvailable":true,"retrieval":"available"}"#
    );
    assert!(
        took < Duration::from_millis(2800),
        "the probes ran one after the other: {took:?}"
    );
}

#[tokio::test]
async fn retrieval_is_what_node_published_and_nothing_old_or_unprovable() {
    let _env = ENV.lock().await;
    let dir = data();
    let provider = stand(Canned::ok()).await;
    set_provider(&provider.base, None);
    let addr = front(dir.path(), "http://127.0.0.1:9").await;
    let retrieval = |text: &str| -> (String, String) {
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        (v["ragAvailable"].to_string(), v["retrieval"].to_string())
    };
    let ask = || async { retrieval(&call(addr, "GET", "/api/health", COOKIE_B).await.text) };
    let unavailable = ("false".to_owned(), "\"unavailable\"".to_owned());

    // No file: unavailable.
    assert_eq!(ask().await, unavailable);
    // Each of core's four words is passed on, and ragAvailable with it.
    for (rag, word) in [
        (true, "available"),
        (true, "degraded"),
        (true, "off"),
        (false, "unavailable"),
    ] {
        fresh(dir.path(), rag, word);
        assert_eq!(
            ask().await,
            (rag.to_string(), format!("\"{word}\"")),
            "{word}"
        );
    }
    // A word core never wrote, a missing field, a wrong kind or version, damage, a symlink: unavailable.
    let now = noevia_server::identity::now_ms();
    let file = dir.path().join("rag-status.json");
    for text in [
        format!(
            r#"{{"version":1,"kind":"rag-status","writtenAt":{now},"everyMs":10000,"ragAvailable":true,"retrieval":"Available"}}"#
        ),
        format!(
            r#"{{"version":1,"kind":"rag-status","writtenAt":{now},"everyMs":10000,"ragAvailable":true}}"#
        ),
        format!(
            r#"{{"version":1,"kind":"rag-status","writtenAt":{now},"everyMs":10000,"retrieval":"available"}}"#
        ),
        format!(
            r#"{{"version":1,"kind":"auto-roles","writtenAt":{now},"everyMs":10000,"ragAvailable":true,"retrieval":"available"}}"#
        ),
        format!(
            r#"{{"version":2,"kind":"rag-status","writtenAt":{now},"everyMs":10000,"ragAvailable":true,"retrieval":"available"}}"#
        ),
        r#"{"version":1,"kind":"rag-status""#.to_owned(),
        String::new(),
    ] {
        std::fs::write(&file, &text).unwrap();
        assert_eq!(ask().await, unavailable, "{text}");
    }
    std::fs::remove_file(&file).unwrap();
    fresh(dir.path(), true, "available");
    let real = dir.path().join("real.json");
    std::fs::rename(&file, &real).unwrap();
    std::os::unix::fs::symlink(&real, &file).unwrap();
    assert_eq!(ask().await, unavailable, "a symlink is not Node's file");
    std::fs::remove_file(&file).unwrap();
    // Stale: more than two intervals old is unavailable, two is not; a file from the future is not current.
    publish(dir.path(), true, "available", now - 19_000, 10_000);
    assert_eq!(ask().await.1, "\"available\"");
    publish(dir.path(), true, "available", now - 25_000, 10_000);
    assert_eq!(ask().await, unavailable);
    publish(dir.path(), true, "available", now + 60_000, 10_000);
    assert_eq!(ask().await, unavailable);
    // The front reads it on every request: a new write shows at once.
    fresh(dir.path(), true, "degraded");
    assert_eq!(ask().await.1, "\"degraded\"");
}

#[tokio::test]
async fn only_the_get_is_the_fronts() {
    let _env = ENV.lock().await;
    let dir = data();
    let provider = stand(Canned::ok()).await;
    set_provider(&provider.base, None);
    let addr = front(dir.path(), "http://127.0.0.1:9").await;
    for method in ["POST", "PUT", "DELETE"] {
        let r = call(addr, method, "/api/health", COOKIE_B).await;
        assert_eq!(r.status, 502, "{method} went to the (dead) Node");
    }
    assert!(provider.seen().is_empty());
    // The table's trailing-slash tolerance is not Node's route.
    let r = call(addr, "GET", "/api/health/", COOKIE_B).await;
    assert_eq!(r.status, 502);
    // /api/ready stays the unauthenticated process check.
    assert_eq!(call(addr, "GET", "/api/ready", "").await.status, 503);
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn a_client_that_leaves_strands_no_probe() {
    use std::sync::atomic::Ordering;
    let _env = ENV.lock().await;
    let dir = data();
    fresh(dir.path(), true, "available");
    let provider = stand(Canned::slow(Duration::from_millis(1200))).await;
    set_provider(&provider.base, None);
    let diary = stand(Canned::slow(Duration::from_millis(1200))).await;
    let addr = front(dir.path(), &diary.base).await;
    assert_eq!(
        noevia_server::health_routes::PROBES_RUNNING.load(Ordering::SeqCst),
        0
    );
    // Ask, then hang up before the answer.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("GET /api/health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {COOKIE_A}\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        noevia_server::health_routes::PROBES_RUNNING.load(Ordering::SeqCst) >= 1,
        "the probes were in flight when the client left"
    );
    drop(s);
    // Both probes finish on their own deadline; nothing is left behind, and the next call is whole.
    let mut waited = 0;
    while noevia_server::health_routes::PROBES_RUNNING.load(Ordering::SeqCst) != 0 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited += 100;
        assert!(waited < 7000, "a probe outlived its bound");
    }
    assert_eq!(provider.seen().len(), 1);
    assert_eq!(diary.seen().len(), 1);
    let r = call(addr, "GET", "/api/health", COOKIE_A).await;
    assert!(r.text.contains(r#""diaryUp":true"#), "{}", r.text);
}
