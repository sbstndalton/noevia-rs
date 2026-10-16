//! noevia#1318: native read-only `GET /api/v1/settings` against the actual Python answers.
//!
//! `fixtures/settings-python.v1.json` is produced by `tools/gen-settings-oracle.py`, which runs
//! the real services FastAPI app on synthetic stores; CI regenerates it from the pinned services
//! ref and requires identical bytes. Each case's store is rebuilt here from the same declaration.
#![cfg(feature = "native-settings")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{settings::Settings, Container, DockerRead, Redactor, StateData};
use rusqlite::Connection;
use serde_json::Value;
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};
use tower::ServiceExt;

/// sha256 of the services files the fixture was generated from (noevia-services 26b4aa91eb0945602b63ad491668817802574bc4).
const API_SHA: &str = "800edae73e7f7764845510ddf4e108bc2ea0a639f721e06e825a4a4644600dd6";
const DB_SHA: &str = "7ac62912f2295d0c96387f55c21e6f42e5815800c44a3920b7ebc2f3a0217478";
const MAIN_SHA: &str = "6f0af60535cbe505cf301a82ad132ead1378ef9f11deebb1e5dce68c20d1d131";
const TOKEN: &str = "synthetic-model-loader-token-0123456789abcdef";

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

fn state(root: Option<&Path>, legacy_base: &str) -> Arc<StateData> {
    Arc::new(StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis: None,
        #[cfg(feature = "native-prompts")]
        prompts: None,
        settings: root.map(|root| Arc::new(Settings::new(root))),
        #[cfg(feature = "native-model-updates")]
        model_updates: None,
        #[cfg(feature = "native-model-detail")]
        model_detail: None,
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

struct Answer {
    status: u16,
    content_type: Option<String>,
    body: Vec<u8>,
}

async fn send(state: Arc<StateData>, method: &str, path: &str, token: bool) -> Answer {
    let mut request = Request::builder().method(method).uri(path);
    if token {
        request = request.header("x-model-loader-token", TOKEN);
    }
    let response = noevia_model_manager::router(state)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    Answer {
        status: response.status().as_u16(),
        content_type: response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned()),
        body: response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    }
}

fn build(path: &Path, case: &Value) {
    match &case["store"] {
        Value::Null => {}
        Value::Object(raw) => {
            let hex = raw["bytes"].as_str().unwrap();
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            std::fs::write(path, bytes).unwrap();
        }
        Value::Array(statements) => {
            let db = Connection::open(path).unwrap();
            let journal = case["journal"].as_str().unwrap().to_uppercase();
            let _: String = db
                .query_row(&format!("PRAGMA journal_mode={journal}"), [], |r| r.get(0))
                .unwrap();
            for statement in statements {
                db.execute_batch(statement.as_str().unwrap()).unwrap();
            }
        }
        other => panic!("unknown store {other}"),
    }
}

fn hold(path: &Path, lock: &Value) -> Option<Connection> {
    let sql = match lock.as_str()? {
        "exclusive" => "BEGIN EXCLUSIVE",
        "wal-writer" => "BEGIN IMMEDIATE; UPDATE kv SET value='synthetic-uncommitted-token' WHERE key='hf_token'",
        other => panic!("unknown lock {other}"),
    };
    let holder = Connection::open(path).unwrap();
    holder.execute_batch(sql).unwrap();
    Some(holder)
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/settings-python.v1.json")).unwrap()
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
    assert!(cases.len() >= 25);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("model_loader.db");
        build(&db, &case);
        let before = std::fs::read(&db).ok();
        let holder = hold(&db, &case["lock"]);
        let started = Instant::now();
        let answer = send(
            state(Some(dir.path()), "http://127.0.0.1:9"),
            "GET",
            "/api/v1/settings",
            true,
        )
        .await;
        let elapsed = started.elapsed();
        drop(holder);
        assert_eq!(
            answer.status as u64,
            case["status"].as_u64().unwrap(),
            "{id}"
        );
        assert_eq!(
            answer.content_type.as_deref(),
            case["content_type"].as_str(),
            "{id}"
        );
        assert_eq!(
            String::from_utf8(answer.body).unwrap(),
            case["body"].as_str().unwrap(),
            "{id}"
        );
        // Bounded: a held lock answers 500 inside the 5 s request budget, never the 504.
        assert!(elapsed < Duration::from_millis(4900), "{id}: {elapsed:?}");
        // Read-only: never creates the file and never rewrites it.
        if before.is_none() {
            assert!(!db.exists(), "{id}: native read created the store");
        } else if case["lock"].is_null() {
            assert_eq!(std::fs::read(&db).ok(), before, "{id}: store bytes changed");
        }
    }
}

#[tokio::test]
async fn rollback_journal_store_is_not_converted_to_wal() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("model_loader.db");
    let c = Connection::open(&db).unwrap();
    c.execute_batch(
        "CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO kv VALUES('hf_token', 'synthetic-rollback-0000');",
    )
    .unwrap();
    drop(c);
    let answer = send(
        state(Some(dir.path()), "http://127.0.0.1:9"),
        "GET",
        "/api/v1/settings",
        true,
    )
    .await;
    assert_eq!(
        answer.body,
        "{\"hasToken\":true,\"tokenHint\":\"…0000\"}".as_bytes()
    );
    let mode: String = Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
}

#[tokio::test]
async fn oversized_value_is_refused_not_read() {
    // Deliberate bound (Python would read it): over VALUE_CAP answers Python's 500 shape.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("model_loader.db");
    let c = Connection::open(&db).unwrap();
    c.execute_batch("CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
        .unwrap();
    let cap = usize::try_from(noevia_model_manager::settings::VALUE_CAP).unwrap();
    for (len, ok) in [(cap, true), (cap + 1, false)] {
        c.execute(
            "INSERT OR REPLACE INTO kv VALUES('hf_token', ?1)",
            ["x".repeat(len)],
        )
        .unwrap();
        let answer = send(
            state(Some(dir.path()), "http://127.0.0.1:9"),
            "GET",
            "/api/v1/settings",
            true,
        )
        .await;
        if ok {
            assert_eq!(answer.status, 200);
            assert_eq!(
                answer.body,
                "{\"hasToken\":true,\"tokenHint\":\"…xxxx\"}".as_bytes()
            );
        } else {
            assert_eq!(answer.status, 500);
            assert_eq!(answer.body, b"Internal Server Error");
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
async fn put_stays_with_the_python_writer() {
    let dir = tempfile::tempdir().unwrap();
    let (base, seen) = legacy().await;
    let answer = send(
        state(Some(dir.path()), &base),
        "PUT",
        "/api/v1/settings",
        true,
    )
    .await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body, br#"{"legacy":true}"#);
    assert_eq!(seen.await.unwrap(), "PUT /api/v1/settings HTTP/1.1");
}

#[tokio::test]
async fn python_mode_forwards_get_unchanged() {
    let (base, seen) = legacy().await;
    let answer = send(state(None, &base), "GET", "/api/v1/settings", true).await;
    assert_eq!(answer.body, br#"{"legacy":true}"#);
    assert_eq!(seen.await.unwrap(), "GET /api/v1/settings HTTP/1.1");
}

#[tokio::test]
async fn native_route_keeps_auth_and_non_get_policy() {
    let dir = tempfile::tempdir().unwrap();
    let unauthorized = send(
        state(Some(dir.path()), "http://127.0.0.1:9"),
        "GET",
        "/api/v1/settings",
        false,
    )
    .await;
    assert_eq!(unauthorized.status, 401);
    assert_eq!(unauthorized.body, br#"{"error":"unauthorized"}"#);
    for method in ["POST", "DELETE", "HEAD"] {
        let answer = send(
            state(Some(dir.path()), "http://127.0.0.1:9"),
            method,
            "/api/v1/settings",
            true,
        )
        .await;
        assert_eq!(answer.status, 405, "{method}");
    }
    let slash = send(
        state(Some(dir.path()), "http://127.0.0.1:9"),
        "GET",
        "/api/v1/settings/",
        true,
    )
    .await;
    assert_eq!(slash.status, 404);
}
