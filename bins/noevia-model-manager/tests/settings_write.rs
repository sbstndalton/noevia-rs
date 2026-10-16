//! noevia#1318: native `PUT /api/v1/settings` against the actual Python answers and stored tokens.
//!
//! `fixtures/settings-write-python.v1.json` comes from `tools/gen-settings-write-oracle.py`: the real
//! services app, step by step, on stores built by Python's own `db.init()` (its full schema, WAL,
//! all connections closed), with the Hugging Face check replaced by a fixed synthetic one. Each
//! step's token-test answer from the child's private call is served by a fake child here.
#![cfg(feature = "native-settings-write")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{
    settings::Settings, settings_write::SettingsWrite, Container, DockerRead, Redactor, StateData,
};
use rusqlite::Connection;
use serde_json::Value;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

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

fn state(root: &std::path::Path, legacy_base: &str) -> Arc<StateData> {
    Arc::new(StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis: None,
        prompts: None,
        settings: Some(Arc::new(Settings::new(root))),
        settings_write: Some(Arc::new(SettingsWrite::new(root))),
        #[cfg(feature = "native-benchmark-overview")]
        benchmark_overview: None,
        #[cfg(feature = "native-backend-control")]
        backends: None,
        #[cfg(feature = "native-model-updates")]
        model_updates: None,
        #[cfg(feature = "native-model-detail")]
        model_detail: None,
        #[cfg(feature = "native-download-targets")]
        download_targets: None,
        #[cfg(feature = "native-badges")]
        badges: None,
        #[cfg(feature = "native-bench-runs")]
        bench_runs: None,
        #[cfg(feature = "native-section-writes")]
        section_writes: None,
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

/// A fake child answering the token test with whatever `test` holds (500 when empty).
async fn child(test: Arc<Mutex<Option<String>>>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let test = test.clone();
            tokio::spawn(async move {
                let mut seen = Vec::new();
                let mut buf = [0u8; 4096];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buf[..n]);
                }
                let ok = String::from_utf8_lossy(&seen)
                    .starts_with("GET /api/v1/internal/hf-token-test ");
                let (status, body) = match (ok, test.lock().unwrap().clone()) {
                    (true, Some(body)) => (200, body),
                    _ => (500, "Internal Server Error".to_owned()),
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
            });
        }
    });
    base
}

async fn put(state: Arc<StateData>, body: &str) -> (u16, Option<String>, String) {
    let response = noevia_model_manager::router(state)
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/settings")
                .header("x-model-loader-token", TOKEN)
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    (
        response.status().as_u16(),
        response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned()),
        String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap(),
    )
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/settings-write-python.v1.json")).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_python_differential() {
    let fx = fixture();
    let test = Arc::new(Mutex::new(None));
    let base = child(test.clone()).await;
    for case in fx["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model_loader.db");
        let c = Connection::open(&path).unwrap();
        let _: String = c
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        for sql in fx["schema"].as_array().unwrap() {
            c.execute_batch(sql.as_str().unwrap()).unwrap();
        }
        if let Some(initial) = case["initial"].as_str() {
            c.execute(
                "INSERT INTO kv(key, value) VALUES('hf_token', ?1)",
                [initial],
            )
            .unwrap();
        }
        drop(c);
        let s = state(dir.path(), &base);
        for (i, (step, want)) in case["steps"]
            .as_array()
            .unwrap()
            .iter()
            .zip(case["answers"].as_array().unwrap())
            .enumerate()
        {
            *test.lock().unwrap() = want["runtime"].as_str().map(str::to_owned);
            let (status, ct, body) = put(s.clone(), step.as_str().unwrap()).await;
            assert_eq!(
                u64::from(status),
                want["status"].as_u64().unwrap(),
                "{id} step {i}: {body}"
            );
            assert_eq!(
                ct.as_deref(),
                want["content_type"].as_str(),
                "{id} step {i}"
            );
            assert_eq!(body, want["body"].as_str().unwrap(), "{id} step {i}");
            let stored: Option<String> = Connection::open(&path)
                .unwrap()
                .query_row("SELECT value FROM kv WHERE key='hf_token'", [], |r| {
                    r.get(0)
                })
                .ok();
            assert_eq!(
                stored.as_deref(),
                want["stored"].as_str(),
                "{id} step {i}: stored token"
            );
        }
        let mode: String = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal", "{id}");
    }
}

#[tokio::test]
async fn unavailable_test_and_limits() {
    let dir = tempfile::tempdir().unwrap();
    let c = Connection::open(dir.path().join("model_loader.db")).unwrap();
    c.execute_batch("CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
        .unwrap();
    drop(c);
    let s = state(dir.path(), "http://127.0.0.1:9");
    let (status, _, body) = put(s.clone(), r#"{"hfToken":"hf_x_12345678","test":true}"#).await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        r#"{"hasToken":true,"tokenHint":"…5678","test":{"ok":false,"message":"Test failed: the token test is unavailable"}}"#
    );
    let long = format!(r#"{{"hfToken":"{}"}}"#, "x".repeat(64 * 1024 + 1));
    let (status, _, _) = put(s.clone(), &long).await;
    assert_eq!(status, 400);
    // The refused value was not written.
    let (_, _, body) = put(s, "{}").await;
    assert_eq!(body, r#"{"hasToken":true,"tokenHint":"…5678"}"#);
    // A missing store is never created.
    let empty = tempfile::tempdir().unwrap();
    let (status, _, _) = put(
        state(empty.path(), "http://127.0.0.1:9"),
        r#"{"hfToken":"a"}"#,
    )
    .await;
    assert_eq!(status, 500);
    assert!(!empty.path().join("model_loader.db").exists());
}
