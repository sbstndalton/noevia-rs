//! noevia#1318: native `GET /api/v1/benchmark/runs/{id}` against the actual Python answers.
//!
//! `fixtures/bench-runs-python.v1.json` comes from `tools/gen-bench-runs-oracle.py`, which runs the
//! real services FastAPI app on synthetic stores with the two live chart inputs fixed; it also
//! records what the child's private runtime call answers for those inputs, which the fake child
//! below serves. CI regenerates the fixture from the pinned services ref and requires identical
//! bytes.
#![cfg(feature = "native-bench-runs")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{bench_runs::BenchRuns, Container, DockerRead, Redactor, StateData};
use rusqlite::Connection;
use serde_json::Value;
use std::{future::Future, path::Path, pin::Pin, sync::Arc};
use tower::ServiceExt;

/// sha256 of the services files the fixture was generated from (noevia-services 641c9c3).
const API_SHA: &str = "c8cf58e15afb97cc0008c9b11c28b4f4a853a3e59ce8cbbd6ba5336c65cee035";
const DB_SHA: &str = "7ac62912f2295d0c96387f55c21e6f42e5815800c44a3920b7ebc2f3a0217478";
const HELPERS_SHA: &str = "fcf2ccb14e5e068f97bb7a4b7985ab7f4f4855b940eb6d9e3838f239ad97918d";
const TOKEN: &str = "synthetic-model-loader-token-0123456789abcdef";
const DEAD: &str = "http://127.0.0.1:9";

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
        prompts: None,
        #[cfg(feature = "native-settings")]
        settings: None,
        #[cfg(feature = "native-model-updates")]
        model_updates: None,
        #[cfg(feature = "native-model-detail")]
        model_detail: None,
        #[cfg(feature = "native-download-targets")]
        download_targets: None,
        #[cfg(feature = "native-badges")]
        badges: None,
        bench_runs: root.map(|r| Arc::new(BenchRuns::new(r))),
        #[cfg(feature = "native-section-writes")]
        section_writes: None,
        #[cfg(feature = "native-settings-write")]
        settings_write: None,
        #[cfg(feature = "native-benchmark-overview")]
        benchmark_overview: None,
        #[cfg(feature = "native-check-updates")]
        check_updates: None,
        #[cfg(feature = "native-downloads")]
        downloads: None,
        #[cfg(feature = "native-model-delete")]
        model_delete: None,
        #[cfg(feature = "native-draft-heads")]
        draft_heads: None,
        #[cfg(feature = "native-backends")]
        backend_stats: None,
        #[cfg(feature = "native-benchmark-runner")]
        bench_runner: None,
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

#[derive(Debug)]
struct Answer {
    status: u16,
    content_type: Option<String>,
    body: String,
}

async fn send(state: Arc<StateData>, method: &str, uri: &str) -> Answer {
    let response = noevia_model_manager::router(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-model-loader-token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
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

/// A one-shot fake child: records the request line and answers `status` with `body`.
async fn child(status: u16, body: String) -> (String, tokio::task::JoinHandle<String>) {
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
        let head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&seen).into_owned()
    });
    (base, task)
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/bench-runs-python.v1.json")).unwrap()
}

fn build(root: &Path, case: &Value) {
    let c = Connection::open(root.join("model_loader.db")).unwrap();
    let _: String = c
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    for sql in fixture()["schema"].as_array().unwrap() {
        c.execute_batch(sql.as_str().unwrap()).unwrap();
    }
    for sql in case["seed"].as_array().unwrap() {
        c.execute_batch(sql.as_str().unwrap()).unwrap();
    }
}

#[test]
fn fixture_provenance_is_the_pinned_services_source() {
    let oracle = &fixture()["oracle"];
    assert_eq!(oracle["api.py"], API_SHA);
    assert_eq!(oracle["db.py"], DB_SHA);
    assert_eq!(oracle["helpers.py"], HELPERS_SHA);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_python_differential() {
    let cases = fixture()["cases"].as_array().unwrap().clone();
    assert!(cases.len() >= 9);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), &case);
        let uri = format!(
            "/api/v1/benchmark/runs/{}",
            case["segment"].as_str().unwrap()
        );
        let (answer, seen) = match case["runtime"].as_str() {
            Some(runtime) => {
                let (base, seen) = child(200, runtime.to_owned()).await;
                let answer = send(state(Some(dir.path()), &base), "GET", &uri).await;
                (answer, Some(seen.await.unwrap()))
            }
            None => (send(state(Some(dir.path()), DEAD), "GET", &uri).await, None),
        };
        assert_eq!(
            u64::from(answer.status),
            case["status"].as_u64().unwrap(),
            "{id}: {}",
            answer.body
        );
        assert_eq!(
            answer.content_type.as_deref(),
            case["content_type"].as_str(),
            "{id}"
        );
        assert_eq!(answer.body, case["body"].as_str().unwrap(), "{id}");
        if let Some(seen) = seen {
            // The private call carries the token and only the vram_labels, in order.
            let line = seen.lines().next().unwrap().to_owned();
            assert!(
                line.starts_with("GET /api/v1/internal/bench-run-runtime"),
                "{id}: {line}"
            );
            assert!(seen
                .to_ascii_lowercase()
                .contains(&format!("x-model-loader-token: {TOKEN}").to_ascii_lowercase()));
            let labels: Vec<String> = serde_json::from_str::<Value>(&answer.body).unwrap()
                ["charts"]["vram_labels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            let query = line
                .split_whitespace()
                .nth(1)
                .unwrap()
                .split_once('?')
                .map_or("", |(_, q)| q);
            let sent: Vec<String> = form_pairs(query);
            assert_eq!(sent, labels, "{id}");
        }
    }
}

fn form_pairs(query: &str) -> Vec<String> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let v = p.strip_prefix("alias=").unwrap().replace('+', " ");
            let bytes: Vec<u8> = {
                let mut out = Vec::new();
                let b = v.as_bytes();
                let mut i = 0;
                while i < b.len() {
                    if b[i] == b'%' {
                        out.push(u8::from_str_radix(&v[i + 1..i + 3], 16).unwrap());
                        i += 3;
                    } else {
                        out.push(b[i]);
                        i += 1;
                    }
                }
                out
            };
            String::from_utf8(bytes).unwrap()
        })
        .collect()
}

fn full_case() -> Value {
    fixture()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "full-run")
        .unwrap()
        .clone()
}

#[tokio::test]
async fn child_failures_map_to_500_or_refusal() {
    let case = full_case();
    for (status, body, want) in [
        (500, "Internal Server Error".to_owned(), 500),
        (503, r#"{"detail":"x"}"#.to_owned(), 503),
        (200, "not json".to_owned(), 503),
        // A label the child leaves out.
        (200, r#"{"capacity_gb":1.0,"predicted":{}}"#.to_owned(), 503),
    ] {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), &case);
        let (base, _seen) = child(status, body).await;
        let answer = send(
            state(Some(dir.path()), &base),
            "GET",
            "/api/v1/benchmark/runs/1",
        )
        .await;
        assert_eq!(answer.status, want);
        if want == 503 {
            assert_eq!(
                answer.body,
                r#"{"detail":"Native benchmark run is unavailable"}"#
            );
        }
    }
    // No child at all.
    let dir = tempfile::tempdir().unwrap();
    build(dir.path(), &case);
    let answer = send(
        state(Some(dir.path()), DEAD),
        "GET",
        "/api/v1/benchmark/runs/1",
    )
    .await;
    assert_eq!(answer.status, 503);
}

#[tokio::test]
async fn non_canonical_ids_python_mode_and_other_methods_are_not_native() {
    let dir = tempfile::tempdir().unwrap();
    build(dir.path(), &full_case());
    for segment in ["abc", "%31", "+1", "1.0", "9223372036854775808"] {
        let (base, seen) = child(200, r#"{"legacy":true}"#.to_owned()).await;
        let answer = send(
            state(Some(dir.path()), &base),
            "GET",
            &format!("/api/v1/benchmark/runs/{segment}"),
        )
        .await;
        assert_eq!(answer.body, r#"{"legacy":true}"#, "{segment}");
        assert!(seen
            .await
            .unwrap()
            .starts_with(&format!("GET /api/v1/benchmark/runs/{segment} ")));
    }
    let (base, seen) = child(200, r#"{"legacy":true}"#.to_owned()).await;
    let answer = send(state(None, &base), "GET", "/api/v1/benchmark/runs/1").await;
    assert_eq!(answer.body, r#"{"legacy":true}"#);
    assert!(seen
        .await
        .unwrap()
        .starts_with("GET /api/v1/benchmark/runs/1 "));
    let s = state(Some(dir.path()), DEAD);
    assert_eq!(
        send(s.clone(), "POST", "/api/v1/benchmark/runs/1")
            .await
            .status,
        405
    );
    // The private call is never reachable from outside.
    assert_eq!(
        send(s, "GET", "/api/v1/internal/bench-run-runtime")
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn missing_store_is_500_and_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let answer = send(
        state(Some(dir.path()), DEAD),
        "GET",
        "/api/v1/benchmark/runs/1",
    )
    .await;
    assert_eq!(answer.status, 500);
    assert!(!dir.path().join("model_loader.db").exists());
}
