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

// noevia#1937: the child's private `/api/v1/internal/*` endpoints are answered 404 by the parent
// and never reach the child, whatever the method or spelling.

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

#[test]
fn counter_exemptions_require_a_single_ascii_word() {
    let redactor = Redactor::new().unwrap();
    for separator in [
        ".", "-", "..", "--", ".-", "-.", "·", "/", "@", ":", " ", "\t",
    ] {
        for key in [
            "password",
            "token",
            "api_key",
            "secret",
            "cookie",
            "session",
            "credential",
        ] {
            let raw = format!("n_stats{separator}{key}=syntheticSECRET");
            assert!(
                !redactor.redact(&raw).unwrap().contains("syntheticSECRET"),
                "{raw}"
            );
            assert!(
                redactor
                    .filter(raw.as_bytes(), "syntheticSECRET", "")
                    .unwrap()
                    .is_empty(),
                "{raw}"
            );
        }
    }
    for raw in ["n_tokens=512", "n_prompt_tokens=40", "n_ctx_slot=8192"] {
        assert_eq!(redactor.redact(raw).unwrap(), raw);
    }
}

#[cfg(feature = "native-host")]
#[tokio::test]
async fn native_host_auth_method_empty_and_history_use_only_rust_state() {
    let fake = fake();
    let mut state = fixture(fake.clone(), "synthetic-token");
    let history = std::sync::Arc::new(tokio::sync::Mutex::new(
        noevia_model_manager::host::HostHistory::default(),
    ));
    Arc::get_mut(&mut state).unwrap().host = Some(history.clone());
    let app = noevia_model_manager::router(state);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/host")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    for (method, status) in [("GET", 200), ("POST", 405)] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/api/v1/host")
                    .header("x-model-loader-token", "synthetic-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        if method == "GET" {
            assert_eq!(
                serde_json::from_slice::<Value>(
                    &response.into_body().collect().await.unwrap().to_bytes()
                )
                .unwrap(),
                json!({"current":null,"history":[]})
            );
        }
    }
    for i in 0..460 {
        history
            .lock()
            .await
            .sample(Ok("cpu 0 0 0 0"), Ok(""), i as f64)
            .unwrap();
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/host")
                .header("x-model-loader-token", "synthetic-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let value: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(value["history"].as_array().unwrap().len(), 180);
    assert_eq!(value["current"]["ts"], json!(459.0));
    assert_eq!(fake.reads.load(Ordering::SeqCst), 0);
}

// The non-cfg(test) library is used here. A missing owner never revives any private RPC.
#[tokio::test]
async fn missing_native_owners_refuse_without_any_child_connection() {
    let trap = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut state = Arc::try_unwrap(fixture(fake(), "qa-token")).ok().unwrap();
    state.legacy_base = format!("http://{}", trap.local_addr().unwrap());
    let app = noevia_model_manager::router(Arc::new(state));
    for (method, uri) in [
        ("GET", "/api/v1/settings"),
        ("GET", "/api/v1/models"),
        ("GET", "/api/v1/overview"),
        ("PUT", "/api/v1/badges"),
        ("GET", "/api/v1/downloads"),
        ("GET", "/api/v1/benchmark/runs/1"),
    ] {
        let answer = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("x-model-loader-token", "qa-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(answer.status().as_u16(), 503, "{method} {uri}");
    }
    let wrong = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/settings")
                .header("x-model-loader-token", "qa-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong.status().as_u16(), 405);
    let hidden = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/internal/backend-stats")
                .header("x-model-loader-token", "qa-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hidden.status().as_u16(), 404);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), trap.accept())
            .await
            .is_err(),
        "production library attempted a retired private RPC"
    );
}
