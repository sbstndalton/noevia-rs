//! noevia#1318: native `GET /api/v1/benchmark` against the actual Python answers.
//!
//! `fixtures/benchmark-overview-python.v1.json` comes from `tools/gen-benchmark-overview-oracle.py`,
//! which runs the real services FastAPI app on a synthetic models.ini, store and Docker inventory
//! with a fixed runner state; it also records what the child's private bench-job call answers for
//! that state, which the fake child below serves. CI regenerates the fixture from the pinned
//! services ref and requires identical bytes.
#![cfg(feature = "native-benchmark-overview")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{
    benchmark_overview::BenchmarkOverview, Container, DockerRead, Redactor, StateData,
};
use rusqlite::Connection;
use serde_json::Value;
use std::{future::Future, path::Path, pin::Pin, sync::Arc};
use tower::ServiceExt;

/// sha256 of the services files the fixture was generated from (noevia-services d475f42).
const API_SHA: &str = "eb8e12f6929baf15d2b1a1998aceff9529fbe48ca8242094ac6b7526c253a91d";
const BENCH_SHA: &str = "2e59f9b057e2686b7f05e5dac04826c8ff37e3c1eb4398bf671faa2d74f3b14b";
const DB_SHA: &str = "7ac62912f2295d0c96387f55c21e6f42e5815800c44a3920b7ebc2f3a0217478";
const INI_SHA: &str = "450a042cbecc36ea7ab68d6e08a10ffa2400c35721f78e62fe592d2a67e41ef0";
const SERVICES_SHA: &str = "aadf227ddc3666cebb1631a0ae543c8090539a382e8ca52710fc7a5458f1d041";
const TOKEN: &str = "synthetic-model-loader-token-0123456789abcdef";
const DEAD: &str = "http://127.0.0.1:9";

type Job<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;
/// `None` is an unreachable Docker.
struct FakeDocker(Option<Vec<Container>>);
impl DockerRead for FakeDocker {
    fn containers(&self) -> Job<'_, Vec<Container>> {
        let answer = self.0.clone().ok_or("docker unreachable");
        Box::pin(async move { answer })
    }
    fn logs<'a>(&'a self, _: &'a str, _: usize) -> Job<'a, Vec<u8>> {
        Box::pin(async { Err("not used") })
    }
}

fn state(
    root: Option<&Path>,
    legacy_base: &str,
    configured: Vec<String>,
    docker: FakeDocker,
) -> Arc<StateData> {
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
        #[cfg(feature = "native-model-list")]
        model_list: None,
        #[cfg(feature = "native-download-targets")]
        download_targets: None,
        #[cfg(feature = "native-badges")]
        badges: None,
        bench_runs: None,
        section_writes: None,
        #[cfg(feature = "native-settings-write")]
        settings_write: None,
        benchmark_overview: root
            .map(|r| Arc::new(BenchmarkOverview::new(r, &r.join("models.ini")))),
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
        configured,
        docker: Arc::new(docker),
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

/// A one-shot fake child: records the request head and answers `status` with `body`.
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
    serde_json::from_str(include_str!("fixtures/benchmark-overview-python.v1.json")).unwrap()
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
    if let Some(ini) = case["ini"].as_str() {
        let bytes = if case["ini_bytes"].as_bool().unwrap() {
            ini.chars()
                .map(|c| u8::try_from(u32::from(c)).unwrap())
                .collect()
        } else {
            ini.as_bytes().to_vec()
        };
        std::fs::write(root.join("models.ini"), bytes).unwrap();
    }
}

fn docker(case: &Value) -> FakeDocker {
    FakeDocker(case["containers"].as_array().map(|list| {
        list.iter()
            .map(|c| Container {
                name: c[0].as_str().unwrap().into(),
                first_image_tag: c[1].as_str().unwrap().into(),
            })
            .collect()
    }))
}

fn configured(case: &Value) -> Vec<String> {
    case["configured"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn fixture_provenance_is_the_pinned_services_source() {
    let oracle = &fixture()["oracle"];
    assert_eq!(oracle["api.py"], API_SHA);
    assert_eq!(oracle["bench.py"], BENCH_SHA);
    assert_eq!(oracle["db.py"], DB_SHA);
    assert_eq!(oracle["ini.py"], INI_SHA);
    assert_eq!(oracle["services.py"], SERVICES_SHA);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_python_differential() {
    let cases = fixture()["cases"].as_array().unwrap().clone();
    assert!(cases.len() >= 10);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), &case);
        let asks_child = case["status"] == 200;
        let (answer, seen) = if asks_child {
            let (base, seen) = child(200, case["job"].as_str().unwrap().to_owned()).await;
            let s = state(Some(dir.path()), &base, configured(&case), docker(&case));
            let answer = send(s, "GET", "/api/v1/benchmark?ignored=1").await;
            (answer, Some(seen.await.unwrap()))
        } else {
            let s = state(Some(dir.path()), DEAD, configured(&case), docker(&case));
            (send(s, "GET", "/api/v1/benchmark").await, None)
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
            assert!(
                seen.starts_with("GET /api/v1/internal/bench-job HTTP/1.1\r\n"),
                "{id}: {seen}"
            );
            assert!(seen
                .to_ascii_lowercase()
                .contains(&format!("x-model-loader-token: {TOKEN}")));
        }
    }
}

fn full_case() -> Value {
    fixture()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "full")
        .unwrap()
        .clone()
}

#[tokio::test]
async fn child_that_cannot_answer_is_502_and_its_own_error_is_500() {
    let case = full_case();
    let dir = tempfile::tempdir().unwrap();
    build(dir.path(), &case);
    let s = state(Some(dir.path()), DEAD, configured(&case), docker(&case));
    let answer = send(s, "GET", "/api/v1/benchmark").await;
    assert_eq!(answer.status, 502);
    assert_eq!(
        answer.body,
        r#"{"detail":"Python-owned endpoint unavailable"}"#
    );
    for (status, body, expected) in [
        (500, "Internal Server Error".to_owned(), 500),
        (200, "[1,2]".to_owned(), 500),
        (200, "not json".to_owned(), 500),
        (404, r#"{"detail":"Not Found"}"#.to_owned(), 502),
    ] {
        let (base, seen) = child(status, body).await;
        let s = state(Some(dir.path()), &base, configured(&case), docker(&case));
        let answer = send(s, "GET", "/api/v1/benchmark").await;
        seen.await.unwrap();
        assert_eq!(answer.status, expected, "{status}");
    }
}

#[tokio::test]
async fn only_get_of_the_exact_path_is_native() {
    let dir = tempfile::tempdir().unwrap();
    build(dir.path(), &full_case());
    for (method, uri, forwarded) in [
        // Omission: with no native overview the GET is forwarded unchanged.
        ("GET", "/api/v1/benchmark", None),
        ("GET", "/api/v1/benchmark/progress", Some(())),
        ("POST", "/api/v1/benchmark/start", Some(())),
        ("POST", "/api/v1/benchmark/cancel", Some(())),
        ("POST", "/api/v1/benchmark/sweep", Some(())),
    ] {
        let (base, seen) = child(200, "{}".to_owned()).await;
        let root = forwarded.map(|()| dir.path());
        let s = state(root, &base, vec![], FakeDocker(Some(vec![])));
        let answer = send(s, method, uri).await;
        let seen = seen.await.unwrap();
        assert!(
            seen.starts_with(&format!("{method} {uri} HTTP/1.1\r\n")),
            "{method} {uri} reached the child as {seen}"
        );
        assert_eq!(answer.body, "{}", "{method} {uri}");
    }
    // Another method on the overview path is not native either: the front's own 405.
    let s = state(Some(dir.path()), DEAD, vec![], FakeDocker(Some(vec![])));
    let answer = send(s, "POST", "/api/v1/benchmark").await;
    assert_eq!(answer.status, 405, "{}", answer.body);
}

#[tokio::test]
async fn the_private_job_path_is_never_forwarded() {
    let dir = tempfile::tempdir().unwrap();
    let s = state(Some(dir.path()), DEAD, vec![], FakeDocker(Some(vec![])));
    let answer = send(s, "GET", "/api/v1/internal/bench-job").await;
    assert_eq!(answer.status, 404, "{}", answer.body);
}
