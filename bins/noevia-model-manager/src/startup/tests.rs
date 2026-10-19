#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[tokio::test]
async fn only_the_exact_private_advertisement_completes_startup() {
    for (status, body, expected) in [
        (200, BODY.to_vec(), true),
        (200, b"{\"ok\":true}".to_vec(), false),
        (200, vec![b'x'; BODY.len() + 1], false),
        (404, Vec::new(), false),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let app = axum::Router::new().route(
                PATH,
                axum::routing::get(move |headers: axum::http::HeaderMap| {
                    let body = body.clone();
                    async move {
                        assert_eq!(
                            headers.get("x-model-loader-token").unwrap(),
                            "synthetic-token"
                        );
                        (axum::http::StatusCode::from_u16(status).unwrap(), body)
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let started = AtomicBool::new(false);
        assert_eq!(
            ready(&reqwest::Client::new(), &base, "synthetic-token", &started).await,
            expected
        );
        assert_eq!(started.load(Ordering::Acquire), expected);
        server.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn an_unreachable_child_has_a_finite_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let started = AtomicBool::new(false);
    assert!(!ready(&reqwest::Client::new(), &base, "", &started).await);
    assert!(!started.load(Ordering::Acquire));
}

/// CI sets these paths to a pinned services checkout and its installed Python interpreter.
/// This exercises the real app/lifespan, not a fake HTTP 200 health response.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_actual_child_startup_advertisement_satisfies_the_production_barrier() {
    let (Ok(python), Ok(app)) = (
        std::env::var("STARTUP_TEST_PYTHON"),
        std::env::var("STARTUP_TEST_APP"),
    ) else {
        eprintln!("actual startup integration requires STARTUP_TEST_PYTHON and STARTUP_TEST_APP");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    let script = dir.path().join("child.py");
    std::fs::write(&script, include_str!("child.py")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let token = "synthetic-actual-startup-token-0123456789abcdef";
    let mut child = tokio::process::Command::new(python)
        .arg("-I")
        .arg(script)
        .arg(app)
        .arg(address.port().to_string())
        .env_clear()
        .env("MODEL_LOADER_TOKEN", token)
        .env("DATA_DIR", dir.path().join("data"))
        .env("MODELS_DIR", &models)
        .env("MODELS_INI_PATH", models.join("models.ini"))
        .env("NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED", "1")
        .env(
            "NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_RUNNER_CONFIRMED",
            "1",
        )
        .env("MIGRATE_CACHE_RAM_ON_START", "false")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let base = format!("http://{address}");
    let client = reqwest::Client::new();
    let started = AtomicBool::new(false);
    let result = ready(&client, &base, token, &started).await;
    if !result {
        let _ = child.kill().await;
    }
    assert!(result, "actual services startup never advertised readiness");
    assert!(dir.path().join("data/model_loader.db").exists());
    assert_eq!(
        client
            .get(format!("{base}/api/v1/health"))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    assert_eq!(
        client
            .get(format!("{base}{PATH}"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    #[cfg(all(feature = "native-search", feature = "native-backends"))]
    {
        use crate::backend_stats::{BackendStats, Config};
        let stats = BackendStats::new(
            Config {
                configured: Vec::new(),
                gpu_vram: Vec::new(),
                default_port: Some(8080),
                drm_root: dir.path().join("drm"),
                meminfo: dir.path().join("meminfo"),
                ram_reserve_gb: Some(2.0),
            },
            std::sync::Arc::new(crate::backend_stats::tests_world(None)),
        )
        .unwrap();
        let source = crate::search::live::Source {
            stats: std::sync::Arc::new(stats),
            models: models.clone(),
            ini: models.join("models.ini"),
            trusted: crate::search::live::Trusted::default(),
            estimates_cache_cap_mib: None,
        };
        let search = crate::search::Search::new(&dir.path().join("data"), &base, token).unwrap();
        // This actual production search runtime used to ask public child health and fail.
        let body = search
            .startup_fixture_runtime(&source)
            .await
            .expect("native search runtime after actual schema startup");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "budget":"0.0", "downloaded":{}, "quantisers":[], "labs":[]
            })
        );
        // Success remains per Search instance, even when the companion is gone.
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        assert!(search.startup_fixture_runtime(&source).await.is_some());
    }
    #[cfg(not(all(feature = "native-search", feature = "native-backends")))]
    {
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }
}

#[tokio::test]
async fn incomplete_startup_is_retried_before_caching_success() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let server = tokio::spawn(async move {
        let app = axum::Router::new().route(
            PATH,
            axum::routing::get(move || {
                let count = seen.fetch_add(1, Ordering::SeqCst);
                async move {
                    if count == 0 {
                        (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            "incomplete".to_owned(),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            String::from_utf8(BODY.to_vec()).unwrap(),
                        )
                    }
                }
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });
    let started = AtomicBool::new(false);
    let client = reqwest::Client::new();
    assert!(ready(&client, &base, "", &started).await);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(ready(&client, &base, "", &started).await);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}
