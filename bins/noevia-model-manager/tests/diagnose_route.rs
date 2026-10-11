#![cfg(feature = "native-diagnose")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[allow(dead_code)]
#[path = "../src/bin/support/diagnose_process.rs"]
mod bounded_process;
#[path = "support/diagnose.rs"]
mod support;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use noevia_model_manager::{
    diagnose_docker::{classify, Error, Route},
    router,
};
use std::sync::{atomic::Ordering, Arc};
use tower::ServiceExt;
#[test]
fn native_selector_refuses_invalid_timezone_and_mode_before_startup() {
    let owned = tempfile::tempdir().unwrap();
    let marker = owned.path().join("child-marker");
    let data = owned.path().join("unopened-data");
    for (mode, zone, expected) in [
        (
            "invalid",
            Some("UTC"),
            "MODEL_MANAGER_DIAGNOSE_IMPL must be python or rust",
        ),
        ("rust", None, "native-diagnose requires exact TZ=UTC"),
        (
            "rust",
            Some("Etc/UTC"),
            "native-diagnose requires exact TZ=UTC",
        ),
        (
            "rust",
            Some("America/New_York"),
            "native-diagnose requires exact TZ=UTC",
        ),
    ] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"));
        command
            .env_clear()
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_DIAGNOSE_IMPL", mode)
            .env("DATA_DIR", &data)
            .env("DIAGNOSE_CHILD_MARKER", &marker);
        if let Some(zone) = zone {
            command.env("TZ", zone);
        }
        let output =
            bounded_process::bounded(command, Vec::new(), std::time::Duration::from_secs(2))
                .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, format!("Error: {expected:?}\n").as_bytes());
        assert!(!marker.exists());
        assert!(!data.exists());
    }
}
#[tokio::test]
async fn actual_fastapi_method_path_corpus_and_named_identifier_narrowing() {
    let bytes = if let Some(path) = std::env::var_os("NOEVIA_DIAGNOSE_ROUTE_ORACLE") {
        use std::io::Read;
        let file = std::fs::File::open(path).unwrap();
        assert!(file.metadata().unwrap().len() <= 1_048_576);
        let mut bytes = Vec::new();
        file.take(1_048_577).read_to_end(&mut bytes).unwrap();
        assert!(bytes.len() <= 1_048_576);
        bytes
    } else {
        include_bytes!("fixtures/diagnose-transport-python.v1.json").to_vec()
    };
    let fixture: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for case in fixture.get("cases").unwrap().as_array().unwrap() {
        let id = case.get("id").unwrap().as_str().unwrap();
        let text = case.get("log").unwrap().as_str().unwrap();
        let fake = Arc::new(support::Fake::new(text.as_bytes().to_vec()));
        let configured = case
            .get("configured")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect();
        let state = support::state(fake.clone(), configured);
        let request = Request::builder()
            .method(case.get("method").unwrap().as_str().unwrap())
            .uri(case.get("path").unwrap().as_str().unwrap())
            .header("host", "synthetic.invalid")
            .header(
                "x-model-loader-token",
                case.get("token").unwrap().as_str().unwrap(),
            )
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let narrowed = matches!(
            id,
            "encoded-backslash-GET"
                | "encoded-percent-GET"
                | "unicode-GET"
                | "invalid-utf8-GET"
                | "malformed-percent-GET"
        );
        let expected_status = if narrowed {
            400
        } else {
            case.get("status").unwrap().as_u64().unwrap()
        };
        assert_eq!(
            u64::from(response.status().as_u16()),
            expected_status,
            "{id}"
        );
        if !narrowed {
            for (key, value) in case.get("headers").unwrap().as_object().unwrap() {
                assert_eq!(
                    response
                        .headers()
                        .get(key)
                        .and_then(|header| header.to_str().ok()),
                    value.as_str(),
                    "{id}/{key}"
                );
            }
        }
        let raw = to_bytes(
            response.into_body(),
            noevia_model_manager::diagnose::JSON_CAP,
        )
        .await
        .unwrap();
        let expected = case.get("body").unwrap().as_str().unwrap();
        if narrowed {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&raw).unwrap(),
                serde_json::json!({"detail":"invalid backend name"})
            );
        } else if expected.is_empty() {
            assert!(raw.is_empty(), "{id}");
        } else {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&raw).unwrap(),
                serde_json::from_str::<serde_json::Value>(expected).unwrap(),
                "{id}"
            );
        }
        if id.ends_with("-GET") && !narrowed && expected_status == 200 {
            assert_eq!(fake.logs.load(Ordering::SeqCst), 1, "{id}");
        } else {
            assert_eq!(fake.logs.load(Ordering::SeqCst), 0, "{id}");
        }
        state.diagnosis.as_ref().unwrap().shutdown().await;
    }
}
#[tokio::test]
async fn native_failure_never_uses_python_fallback() {
    use std::io::{Read, Write};
    struct OwnedFallback {
        stop: Arc<std::sync::atomic::AtomicBool>,
        daemon: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    }
    impl OwnedFallback {
        fn finish(&mut self) -> std::io::Result<()> {
            self.stop.store(true, Ordering::Release);
            if let Some(daemon) = self.daemon.take() {
                daemon
                    .join()
                    .map_err(|_| std::io::Error::other("owned fallback fixture panic"))??;
            }
            Ok(())
        }
    }
    impl Drop for OwnedFallback {
        fn drop(&mut self) {
            let _ = self.finish();
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:8091").unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = stop.clone();
    let daemon = std::thread::spawn(move || -> std::io::Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !stopping.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                Err(e) => return Err(e),
            };
            stream.set_read_timeout(Some(std::time::Duration::from_millis(500)))?;
            stream.set_write_timeout(Some(std::time::Duration::from_millis(500)))?;
            let mut raw = Vec::new();
            while !raw.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                if stream.read(&mut byte)? == 0 || raw.len() >= 8192 {
                    return Err(std::io::Error::other(
                        "owned fallback fixture header cap/EOF",
                    ));
                }
                raw.push(byte[0]);
            }
            let body = b"{\"failures\":[]}";
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len())?;
            stream.write_all(body)?;
        }
        Ok(())
    });
    let mut owned_fallback = OwnedFallback {
        stop,
        daemon: Some(daemon),
    };
    let mut fake = support::Fake::new(Vec::new());
    fake.error = Some(Error::Refused);
    let fake = Arc::new(fake);
    let state = support::state(fake.clone(), vec!["synthetic-backend".into()]);
    // Any erroneous fallback reaches only this exclusively owned closed loopback fixture.
    let request = Request::builder()
        .uri("/api/v1/backends/synthetic-backend/diagnose")
        .header("x-model-loader-token", support::TOKEN)
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let raw = to_bytes(response.into_body(), 4096).await.unwrap();
    state.diagnosis.as_ref().unwrap().shutdown().await;
    owned_fallback.finish().unwrap();
    assert_eq!(
        (
            status.as_u16(),
            serde_json::from_slice::<serde_json::Value>(&raw).unwrap()
        ),
        (
            503,
            serde_json::json!({"detail":"Native backend diagnosis is unavailable"})
        )
    );
}
#[tokio::test]
async fn auth_unknown_and_method_controls_do_not_read_logs() {
    let fake = Arc::new(support::Fake::new(b"fatal synthetic".to_vec()));
    let state = support::state(fake.clone(), vec!["synthetic-backend".into()]);
    for (method, path, token, status) in [
        (
            "GET",
            "/api/v1/backends/synthetic-backend/diagnose",
            "wrong",
            401,
        ),
        (
            "POST",
            "/api/v1/backends/synthetic-backend/diagnose",
            support::TOKEN,
            405,
        ),
        (
            "GET",
            "/api/v1/backends/unknown/diagnose",
            support::TOKEN,
            404,
        ),
        (
            "GET",
            "/api/v1/backends/bad%25name/diagnose",
            support::TOKEN,
            400,
        ),
        (
            "GET",
            "/api/v1/backends/bad%2Fname/diagnose",
            support::TOKEN,
            404,
        ),
    ] {
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("x-model-loader-token", token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
    }
    assert_eq!(fake.logs.load(Ordering::SeqCst), 0);
    assert_eq!(fake.lists.load(Ordering::SeqCst), 1);
    state.diagnosis.as_ref().unwrap().shutdown().await;
}
#[test]
fn selected_route_is_complete_and_restart_test_are_not_owned() {
    assert_eq!(
        classify("/api/v1/backends/synthetic%2Fid/diagnose"),
        Route::NotFound
    );
    assert_eq!(
        classify("/api/v1/backends/synthetic-id/%64iagnose"),
        Route::Item("synthetic-id".into())
    );
    for path in [
        "/api/v1/backends/synthetic-id/restart",
        "/api/v1/backends/synthetic-id/test",
        "/api/v1/prompts",
    ] {
        assert_eq!(classify(path), Route::NotOwned);
    }
}
