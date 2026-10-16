//! noevia#1318: native `PUT` / `DELETE /api/v1/badges` against the actual Python answers.
//!
//! `fixtures/badges-python.v1.json` is produced by `tools/gen-badges-oracle.py`, which runs the
//! real services FastAPI app step by step on synthetic stores with a fixed clock; CI regenerates
//! it from the pinned services ref and requires identical bytes. Each case's store is rebuilt
//! here and its steps replayed against the native router. The pinned services ref carries the
//! noevia#1905 fix, so every case, `fixed-1905` included, must match (noevia#1910).
#![cfg(feature = "native-badges")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{badges::Badges, Container, DockerRead, Redactor, StateData};
use rusqlite::Connection;
use serde_json::Value;
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tower::ServiceExt;

/// sha256 of the services files the fixture was generated from (noevia-services 641c9c3,
/// services#64 with the noevia#1905 fix).
const API_SHA: &str = "c8cf58e15afb97cc0008c9b11c28b4f4a853a3e59ce8cbbd6ba5336c65cee035";
const DB_SHA: &str = "7ac62912f2295d0c96387f55c21e6f42e5815800c44a3920b7ebc2f3a0217478";
const MAIN_SHA: &str = "597baa16769063bb66bde46f38c4404443f0ef486b92f6548c0e5e37e8c7a2f5";
const TOKEN: &str = "synthetic-model-loader-token-0123456789abcdef";
const PATH: &str = "/api/v1/badges";
/// `db.init()`'s badge table, verbatim.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS model_badge (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    alias      TEXT NOT NULL,
    category   TEXT NOT NULL,
    rating     INTEGER NOT NULL,
    note       TEXT NOT NULL DEFAULT '',
    run_id     INTEGER,
    created_at REAL NOT NULL,
    UNIQUE(alias, category)
);
CREATE INDEX IF NOT EXISTS model_badge_alias ON model_badge(alias);";

type Job<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;
struct NoDocker;
impl DockerRead for NoDocker {
    fn containers(&self) -> Job<'_, Vec<Container>> {
        Box::pin(async { Err("not used") })
    }
    fn logs<'a>(&'a self, _: &'a str, _: usize) -> Job<'a, Vec<u8>> {
        Box::pin(async { Err("not used") })
    }
}

fn state(badges: Option<Badges>, legacy_base: &str) -> Arc<StateData> {
    Arc::new(StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis: None,
        prompts: None,
        #[cfg(feature = "native-settings")]
        settings: None,
        #[cfg(feature = "native-model-updates")]
        model_updates: None,
        #[cfg(feature = "native-model-detail")]
        model_detail: None,
        #[cfg(feature = "native-download-targets")]
        download_targets: None,
        badges: badges.map(Arc::new),
        #[cfg(feature = "native-bench-runs")]
        bench_runs: None,
        #[cfg(feature = "native-section-writes")]
        section_writes: None,
        #[cfg(feature = "native-settings-write")]
        settings_write: None,
        #[cfg(feature = "native-backend-control")]
        backends: None,
        sections: None,
        embedding: None,
        host: None,
        token: TOKEN.into(),
        configured: vec![],
        docker: Arc::new(NoDocker),
        redactor: Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(4),
        legacy: noevia_model_manager::legacy_client().unwrap(),
        legacy_base: legacy_base.into(),
        idle_unload: None,
    })
}

/// The generator's clock: 1700000000.25 + 1.5 per call, counted per case.
fn clocked(root: &Path) -> Badges {
    let calls = Arc::new(AtomicU32::new(0));
    Badges::with_clock(
        root,
        Arc::new(move || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            1_700_000_000.25 + 1.5 * f64::from(n)
        }),
    )
}

#[derive(Debug, PartialEq)]
struct Answer {
    status: u16,
    content_type: Option<String>,
    body: String,
}

async fn send(
    state: Arc<StateData>,
    method: &str,
    uri: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-model-loader-token", TOKEN);
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    let response = noevia_model_manager::router(state)
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    Answer {
        status: response.status().as_u16(),
        content_type: response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned()),
        body: String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap(),
    }
}

async fn step(state: Arc<StateData>, step: &Value) -> Answer {
    if step["method"] == "PUT" {
        send(
            state,
            "PUT",
            PATH,
            step["content_type"].as_str(),
            step["body"].as_str().unwrap().as_bytes().to_vec(),
        )
        .await
    } else {
        let query = step["query"].as_str().unwrap();
        let uri = if query.is_empty() {
            PATH.to_owned()
        } else {
            format!("{PATH}?{query}")
        };
        send(state, "DELETE", &uri, None, Vec::new()).await
    }
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/badges-python.v1.json")).unwrap()
}

fn build(root: &Path, case: &Value) -> Option<Connection> {
    let path = root.join("model_loader.db");
    match case["store"].as_str().unwrap() {
        "missing" => None,
        "no-table" => {
            let c = Connection::open(&path).unwrap();
            let _: String = c
                .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
                .unwrap();
            c.execute_batch("CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                .unwrap();
            None
        }
        store => {
            let c = Connection::open(&path).unwrap();
            let _: String = c
                .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
                .unwrap();
            c.execute_batch(SCHEMA).unwrap();
            for statement in case["seed"].as_array().unwrap() {
                c.execute_batch(statement.as_str().unwrap()).unwrap();
            }
            (store == "locked").then(|| {
                let holder = Connection::open(&path).unwrap();
                holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
                holder
            })
        }
    }
}

#[test]
fn fixture_provenance_is_the_pinned_services_source() {
    let oracle = &fixture()["oracle"];
    assert_eq!(oracle["api.py"], API_SHA);
    assert_eq!(oracle["db.py"], DB_SHA);
    assert_eq!(oracle["main.py"], MAIN_SHA);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_python_differential() {
    let cases = fixture()["cases"].as_array().unwrap().clone();
    assert!(cases.len() >= 12);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let holder = build(dir.path(), &case);
        let state = state(Some(clocked(dir.path())), "http://127.0.0.1:9");
        let expected: Vec<Answer> = case["answers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| Answer {
                status: u16::try_from(a["status"].as_u64().unwrap()).unwrap(),
                content_type: a["content_type"].as_str().map(str::to_owned),
                body: a["body"].as_str().unwrap().to_owned(),
            })
            .collect();
        let steps = case["steps"].as_array().unwrap();
        assert_eq!(steps.len(), expected.len(), "{id}");
        for (i, (s, want)) in steps.iter().zip(expected).enumerate() {
            let started = Instant::now();
            let got = step(state.clone(), s).await;
            if want.status == 422 && got.status == 422 {
                // FastAPI's validation bodies: same JSON, but the shared prompt-body validator
                // (serde_json maps) writes their keys sorted.
                let parse = |b: &str| serde_json::from_str::<Value>(b).unwrap();
                assert_eq!(parse(&got.body), parse(&want.body), "{id} step {i}");
                assert_eq!(got.content_type, want.content_type, "{id} step {i}");
            } else {
                assert_eq!(got, want, "{id} step {i}");
            }
            // A held lock answers 500 inside the 5 s request budget, never the 504.
            assert!(
                started.elapsed() < Duration::from_millis(4900),
                "{id} step {i}"
            );
        }
        drop(holder);
        if case["store"] == "missing" {
            assert!(
                !dir.path().join("model_loader.db").exists(),
                "{id}: created the store"
            );
        }
    }
}

/// A one-shot fake for the Python child: records the request line and answers fixed JSON.
async fn legacy() -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0);
            seen.extend_from_slice(&buf[..n]);
        }
        let body = br#"{"legacy":true}"#;
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(body).await.unwrap();
        String::from_utf8_lossy(&seen)
            .lines()
            .next()
            .unwrap()
            .to_owned()
    });
    (base, task)
}

#[tokio::test]
async fn python_mode_forwards_both_methods_unchanged() {
    for method in ["PUT", "DELETE"] {
        let (base, seen) = legacy().await;
        let answer = send(state(None, &base), method, PATH, None, Vec::new()).await;
        assert_eq!(answer.body, r#"{"legacy":true}"#);
        assert_eq!(
            seen.await.unwrap(),
            format!("{method} /api/v1/badges HTTP/1.1")
        );
    }
}

#[tokio::test]
async fn benchmark_run_reads_stay_with_python() {
    // GET /benchmark/runs/{id} also lists badges; it is not part of this slice.
    let dir = tempfile::tempdir().unwrap();
    let (base, seen) = legacy().await;
    let answer = send(
        state(Some(clocked(dir.path())), &base),
        "GET",
        "/api/v1/benchmark/runs/1",
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(answer.body, r#"{"legacy":true}"#);
    assert_eq!(seen.await.unwrap(), "GET /api/v1/benchmark/runs/1 HTTP/1.1");
}

#[tokio::test]
async fn native_route_keeps_auth_and_method_policy() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(Some(clocked(dir.path())), "http://127.0.0.1:9");
    let response = noevia_model_manager::router(state.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(PATH)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    for method in ["GET", "POST"] {
        let answer = send(state.clone(), method, PATH, None, Vec::new()).await;
        assert_eq!(answer.status, 405, "{method}");
    }
}

#[tokio::test]
async fn oversized_body_is_refused_before_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(Some(clocked(dir.path())), "http://127.0.0.1:9");
    let mut body = br#"{"alias":"m","category":"coding","rating":3,"note":""#.to_vec();
    body.extend(std::iter::repeat_n(b'x', 1024 * 1024));
    body.extend_from_slice(br#""}"#);
    let answer = send(state, "PUT", PATH, Some("application/json"), body).await;
    assert_eq!(answer.status, 413);
    assert!(!dir.path().join("model_loader.db").exists());
}

/// noevia#1909: a write whose request was dropped (the front's 504) still finishes before a later
/// write starts, and the later write answers inside its own 5 s budget and is what stays stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_write_cannot_land_after_a_newer_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model_loader.db");
    let c = Connection::open(&path).unwrap();
    let _: String = c
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    c.execute_batch(SCHEMA).unwrap();
    // A slow first job: another connection holds the write lock for 2 s.
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(2000));
        holder.execute_batch("ROLLBACK").unwrap();
    });
    let state = state(Some(clocked(dir.path())), "http://127.0.0.1:9");
    let put = |rating: u8| {
        let state = state.clone();
        async move {
            send(
                state,
                "PUT",
                PATH,
                Some("application/json"),
                format!(r#"{{"alias":"m","category":"coding","rating":{rating}}}"#).into_bytes(),
            )
            .await
        }
    };
    // The first request is abandoned while its write waits on the store.
    let first = tokio::time::timeout(Duration::from_millis(200), put(2)).await;
    assert!(first.is_err());
    let started = Instant::now();
    let second = put(4).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(second.status, 200, "{second:?}");
    release.join().unwrap();
    let rating: i64 = c
        .query_row("SELECT rating FROM model_badge WHERE alias='m'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rating, 4);
}

/// noevia#1909: a write still waiting at the request's own deadline answers inside the budget
/// (503 busy or the store's 500), never the front's 504, and never writes later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lock_wait_is_bounded_by_the_request_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model_loader.db");
    let c = Connection::open(&path).unwrap();
    let _: String = c
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    c.execute_batch(SCHEMA).unwrap();
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let state = state(Some(clocked(dir.path())), "http://127.0.0.1:9");
    let body = |r: u8| format!(r#"{{"alias":"m","category":"coding","rating":{r}}}"#).into_bytes();
    let (a, b) = tokio::join!(
        send(
            state.clone(),
            "PUT",
            PATH,
            Some("application/json"),
            body(2)
        ),
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let started = Instant::now();
            let b = send(
                state.clone(),
                "PUT",
                PATH,
                Some("application/json"),
                body(4),
            )
            .await;
            (b, started.elapsed())
        }
    );
    let (b, waited) = b;
    assert_eq!(a.status, 500, "{a:?}");
    // B either never gets the lock (503 busy) or gets it with no time left for the store (500).
    assert!(
        (b.status == 503 && b.body == r#"{"detail":"service is busy"}"#) || b.status == 500,
        "{b:?}"
    );
    assert!(waited < Duration::from_secs(5), "{waited:?}");
    holder.execute_batch("ROLLBACK").unwrap();
    let rows: i64 = c
        .query_row("SELECT COUNT(*) FROM model_badge", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}
