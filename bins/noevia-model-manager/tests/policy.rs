#![allow(
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
        token: token.into(),
        configured: vec!["synthetic-engine".into()],
        docker: fake,
        redactor: Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(2),
        legacy: noevia_model_manager::legacy_client().unwrap(),
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

#[test]
fn actual_python_policy_goldens() {
    let source: Value = serde_json::from_str(include_str!("fixtures/logs.json")).unwrap();
    let redactor = Redactor::new().unwrap();
    for row in source["inventories"].as_array().unwrap() {
        let configured: Vec<String> = serde_json::from_value(row["configured"].clone()).unwrap();
        let inventory: Vec<Container> = row["inventory"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| Container {
                name: c["name"].as_str().unwrap().into(),
                first_image_tag: c["first_image_tag"].as_str().unwrap().into(),
            })
            .collect();
        assert_eq!(
            serde_json::to_value(effective_names(&configured, &inventory).unwrap()).unwrap(),
            row["expected"]
        );
    }
    for row in source["cases"].as_array().unwrap() {
        assert_eq!(
            noevia_model_manager::parse_tail(Some(&row["tail"].to_string())).unwrap(),
            row["clamped"].as_u64().unwrap() as usize
        );
        let lines = redactor
            .filter(
                row["raw"].as_str().unwrap().as_bytes(),
                row["q"].as_str().unwrap(),
                row["level"].as_str().unwrap(),
            )
            .unwrap();
        if row["adversarial"].as_bool() == Some(true) {
            assert!(
                !lines.iter().any(|line| line.contains("syntheticSECRET")
                    || line.contains("AKIA1234567890123456")),
                "{row}"
            );
            if !row["q"].as_str().unwrap().is_empty() {
                assert!(lines.is_empty(), "{row}");
            }
        } else {
            assert_eq!(json!({"ok":true,"lines":lines}), row["answer"], "{row}");
        }
    }
}
#[tokio::test]
async fn auth_before_docker_and_exact_health_exemption() {
    let fake = fake();
    let router = noevia_model_manager::router(fixture(fake.clone(), "synthetic-token"));
    for path in [
        "/api/v1/backends/synthetic-engine/logs",
        "/unknown",
        "/api/v1/health/",
    ] {
        assert_eq!(
            router
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            401
        );
    }
    assert_eq!(fake.reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        router
            .oneshot(Request::get("/api/v1/health").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(fake.reads.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn native_error_never_falls_back_and_unknown_never_reads_logs() {
    let fake = Arc::new(Fake {
        reads: AtomicUsize::new(0),
        inventory: vec![],
        raw: vec![],
        fail: true,
    });
    let router = noevia_model_manager::router(fixture(fake.clone(), ""));
    let denied = router
        .clone()
        .oneshot(
            Request::get("/api/v1/backends/other/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), 404);
    assert_eq!(fake.reads.load(Ordering::SeqCst), 1);
    let response = router
        .oneshot(
            Request::get("/api/v1/backends/synthetic-engine/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        body,
        json!({"ok":false,"error":"docker unreachable","lines":[]})
    );
    assert_eq!(fake.reads.load(Ordering::SeqCst), 3);
}
#[test]
fn admission_and_ownership() {
    assert!(validate_token("").is_ok());
    assert!(validate_token(" ").is_err());
    assert!(validate_token("short").is_err());
    assert!(!python_owned("GET", "/api/v1/backends/a/logs"));
    assert!(!python_owned("GET", "/api/v1/health"));
    assert!(python_owned("GET", "/api/v1/backends"));
    assert!(python_owned("POST", "/api/v1/backends/a/restart"));
    assert!(!python_owned("DELETE", "/api/v1/backends/a/restart"));
    let names = effective_names(
        &["configured".into()],
        &[
            Container {
                name: "stopped".into(),
                first_image_tag: "LLAMA.CPP:server".into(),
            },
            Container {
                name: "model-loader".into(),
                first_image_tag: "llama.cpp".into(),
            },
            Container {
                name: "unrelated".into(),
                first_image_tag: "postgres".into(),
            },
        ],
    )
    .unwrap();
    assert_eq!(names, vec!["configured", "stopped"]);
    assert!(Redactor::new()
        .unwrap()
        .filter(&vec![b'x'; LOG_CAP + 1], "", "")
        .is_err());
}

#[tokio::test]
async fn concurrency_limit_refuses_before_docker_and_health_stays_light() {
    let fake = fake();
    let mut state = fixture(fake.clone(), "");
    Arc::get_mut(&mut state).unwrap().permits = tokio::sync::Semaphore::new(0);
    let router = noevia_model_manager::router(state);
    assert_eq!(
        router
            .clone()
            .oneshot(
                Request::get("/api/v1/backends/synthetic-engine/logs")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        503
    );
    assert_eq!(
        router
            .oneshot(Request::get("/api/v1/health").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(fake.reads.load(Ordering::SeqCst), 0);
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
#[tokio::test(start_paused = true)]
async fn timeout_returns_native_refusal_without_fallback() {
    let fake = fake();
    let mut state = fixture(fake, "");
    Arc::get_mut(&mut state).unwrap().docker = Arc::new(Pending);
    let router = noevia_model_manager::router(state);
    assert_eq!(
        router
            .oneshot(
                Request::get("/api/v1/backends/synthetic-engine/logs")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        504
    );
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
proptest! {
    #[test]
    fn secret_filter_never_reveals_value(secret in "[A-Za-z]{8,64}") {
        prop_assume!(!"[redacted]".contains(&secret.to_lowercase()));
        let raw=format!("password={secret}"); let redactor=Redactor::new().unwrap();
        prop_assert!(!redactor.redact(&raw).unwrap().contains(&secret));
        prop_assert!(redactor.filter(raw.as_bytes(),&secret,"").unwrap().is_empty());
    }
}

#[test]
fn unicode_credentials_never_survive_output_or_secret_queries() {
    let redactor = Redactor::new().unwrap();
    for separator in [
        "\u{301}", "\u{203f}", "\u{200c}", "\u{200d}", "\u{2054}", "\u{20dd}", "é",
    ] {
        for key in ["password", "token", "secret", "api_key", "cookie"] {
            let raw = format!("n_{separator}{key}=syntheticSECRET");
            assert!(!redactor.redact(&raw).unwrap().contains("syntheticSECRET"));
            assert!(redactor
                .filter(raw.as_bytes(), "syntheticSECRET", "")
                .unwrap()
                .is_empty());
        }
    }
    for separator in ['\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}', '\u{85}', '\u{a0}'] {
        for raw in [
            format!("password{separator}=syntheticSECRET"),
            format!("Authorization{separator}:syntheticSECRET"),
            format!("bearer{separator}syntheticSECRET"),
        ] {
            assert!(!redactor.redact(&raw).unwrap().contains("syntheticSECRET"));
            assert!(redactor
                .filter(raw.as_bytes(), "syntheticSECRET", "")
                .unwrap()
                .is_empty());
        }
    }
    for suffix in ["\u{301}", "\u{203f}", "a"] {
        let raw = format!("AKIA1234567890123456{suffix}");
        assert!(!redactor
            .redact(&raw)
            .unwrap()
            .contains("AKIA1234567890123456"));
        assert!(redactor
            .filter(raw.as_bytes(), "AKIA1234567890123456", "")
            .unwrap()
            .is_empty());
    }
}
