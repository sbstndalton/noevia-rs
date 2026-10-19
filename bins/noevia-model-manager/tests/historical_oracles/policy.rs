// Archived pre-retirement private-input oracle; cfg(test) library only.
#![allow(
    unused_imports,
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{
    effective_names, python_owned, validate_token, Container, DockerRead, Redactor, StateData,
    LOG_CAP,
};
use proptest::prelude::*;
use serde_json::{json, Value};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tower::ServiceExt;

type Job<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;
struct Fake {
    reads: AtomicUsize,
    inventory: Vec<Container>,
    raw: Vec<u8>,
    fail: bool,
}
impl DockerRead for Fake {
    fn containers(&self) -> Job<'_, Vec<Container>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(self.inventory.clone()) })
    }
    fn logs<'a>(&'a self, _: &'a str, _: usize) -> Job<'a, Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            if self.fail {
                Err("docker unreachable")
            } else {
                Ok(self.raw.clone())
            }
        })
    }
}
fn fixture(fake: Arc<Fake>, token: &str) -> Arc<StateData> {
    Arc::new(StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis: None,
        #[cfg(feature = "native-prompts")]
        prompts: None,
        #[cfg(feature = "native-settings")]
        settings: None,
        #[cfg(feature = "native-model-updates")]
        model_updates: None,
        #[cfg(feature = "native-model-detail")]
        model_detail: None,
        #[cfg(feature = "native-model-list")]
        model_list: None,
        sections: None,
        embedding: None,
        host: None,
        token: token.into(),
        configured: vec!["synthetic-engine".into()],
        docker: fake,
        redactor: Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(2),
        legacy: noevia_model_manager::legacy_client().unwrap(),
        legacy_base: noevia_model_manager::LEGACY_BASE.to_owned(),
        idle_unload: None,
        #[cfg(feature = "native-section-autoconfig")]
        section_autoconfig: None,
        #[cfg(feature = "native-download-targets")]
        download_targets: None,
        #[cfg(feature = "native-badges")]
        badges: None,
        #[cfg(feature = "native-bench-runs")]
        bench_runs: None,
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
        #[cfg(feature = "native-search")]
        search: None,
        #[cfg(feature = "native-backends")]
        backend_stats: None,
        #[cfg(feature = "native-benchmark-runner")]
        bench_runner: None,
        #[cfg(feature = "native-backend-control")]
        backends: None,
    })
}
fn fake() -> Arc<Fake> {
    Arc::new(Fake {
        reads: AtomicUsize::new(0),
        inventory: vec![],
        raw: b"password=synthetic_secret\nplain\n".to_vec(),
        fail: false,
    })
}

struct Pending;
impl DockerRead for Pending {
    fn containers(&self) -> Job<'_, Vec<Container>> {
        Box::pin(std::future::pending())
    }
    fn logs<'a>(&'a self, _: &'a str, _: usize) -> Job<'a, Vec<u8>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn python_owned_response_can_exceed_native_log_timeout() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:8091").await.unwrap();
    let (received, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut chunk = [0u8; 512];
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&chunk[..n]);
            assert!(request.len() < 8192);
        }
        assert!(String::from_utf8_lossy(&request).starts_with("GET /api/v1/settings HTTP/1.1\r\n"));
        received.send(()).unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"synthetic\":true}").await.unwrap();
    });
    let router = noevia_model_manager::router(fixture(fake(), ""));
    let request = tokio::spawn(async move {
        router
            .oneshot(
                Request::get("/api/v1/settings")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    });
    wait.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(6)).await;
    let response = request.await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "{\"synthetic\":true}"
    );
    server.await.unwrap();
}
// noevia#1937: the child's private `/api/v1/internal/*` endpoints are answered 404 by the parent
// and never reach the child, whatever the method or spelling.
#[tokio::test]
async fn internal_child_paths_get_404_and_never_reach_the_child() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let reached = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = reached.clone();
    let child = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            // The head may arrive together with a request body, so look for the blank line.
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut chunk = [0u8; 512];
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
            }
            let line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            seen.lock().unwrap().push(line);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"reached\":true}\n")
                .await
                .unwrap();
        }
    });
    let mut state = fixture(fake(), "");
    Arc::get_mut(&mut state).unwrap().legacy_base = base;
    let router = noevia_model_manager::router(state);
    for (method, path) in [
        ("PUT", "/api/v1/internal/backend-restart-error"),
        ("GET", "/api/v1/internal/hf-token-test"),
        ("GET", "/api/v1/internal/bench-job"),
        ("GET", "/api/v1/internal/model-detail-runtime"),
        ("GET", "/api/v1/internal/"),
        ("PUT", "/api/v1/internal%2Fbackend-restart-error"),
        ("GET", "/api/v1%2Finternal%2Fhf-token-test"),
        ("GET", "/api/v1/%69nternal/hf-token-test"),
        ("GET", "/api/v1/internal%252Fhf-token-test"),
        ("GET", "/api/v1/sections/..%2Finternal%2Fbench-job"),
        ("GET", "/api/v1/sections/%2e%2e/internal/bench-job"),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{method} {path}");
    }
    assert_eq!(
        reached.lock().unwrap().as_slice(),
        &[] as &[String],
        "an internal path reached the child"
    );
    // Control: the harness does forward an ordinary Python-owned path.
    let response = router
        .oneshot(
            Request::get("/api/v1/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        reached.lock().unwrap().as_slice(),
        &["GET /api/v1/settings HTTP/1.1".to_owned()]
    );
    child.abort();
}

proptest! {}
