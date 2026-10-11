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
        legacy: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
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
        assert_eq!(json!({"ok":true,"lines":lines}), row["answer"], "{row}");
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
proptest! {
    #[test]
    fn secret_filter_never_reveals_value(secret in "[A-Za-z]{8,64}") {
        prop_assume!(!"[redacted]".contains(&secret.to_lowercase()));
        let raw=format!("password={secret}"); let redactor=Redactor::new().unwrap();
        prop_assert!(!redactor.redact(&raw).unwrap().contains(&secret));
        prop_assert!(redactor.filter(raw.as_bytes(),&secret,"").unwrap().is_empty());
    }
}
