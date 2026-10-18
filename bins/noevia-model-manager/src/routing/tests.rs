#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
#[test]
fn actual_asgi_framework_route_method_redirect_oracle() {
    let fx: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/routing-python.v1.json")).unwrap();
    assert_eq!(fx["framework"]["starlette"], "0.41.3");
    let registration=ROUTES.iter().take_while(|(_,path)|*path!="/api/v1/health").map(|(method,path)|serde_json::json!([method,path])).collect::<Vec<_>>();
    assert_eq!(fx["registered"],serde_json::json!(registration));
    for case in fx["cases"].as_array().unwrap() {
        if case["auth"] == false {
            continue;
        }
        let uri: Uri = case["path"].as_str().unwrap().parse().unwrap();
        let method: Method = case["method"].as_str().unwrap().parse().unwrap();
        let decoded = crate::percent_decode_lossy(uri.path());
        let route = classify(&method, &decoded, false, false);
        let status = case["status"].as_u64().unwrap();
        match route {
            Route::Method(allowed) => {
                assert_eq!(status, 405, "{case}");
                assert_eq!(case["headers"]["allow"], allowed, "{case}");
            }
            Route::Redirect(path) => {
                assert_eq!(status, 307, "{case}");
                assert_eq!(
                    case["headers"]["location"],
                    location(&path, &uri, "qa.example:8080"),
                    "{case}"
                );
            }
            Route::NotFound => assert_eq!(status, 404, "{case}"),
            Route::Match { canonical, .. } => {
                assert!([200, 422].contains(&status), "{case}");
                // Existing handlers decode this safe canonical URI once, retaining literal%.
                assert_eq!(
                    crate::percent_decode_lossy(&canonical_path(&canonical)),
                    canonical
                );
            }
        }
    }
}
#[test]
fn startup_requires_actual_native_owners() {
    let state = crate::test_support::test_state("http://127.0.0.1:9");
    assert!(validate_owners(&state).is_err());
}

#[tokio::test]
async fn router_framework_answers_match_without_body_reads_or_a_child() {
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let fx: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/routing-python.v1.json")).unwrap();
    let mut state = crate::test_support::test_state("http://127.0.0.1:9");
    state.token = "qa-token".into();
    let app = crate::router(Arc::new(state)).layer(Extension(Enabled));
    for case in fx["cases"].as_array().unwrap() {
        let expected = case["status"].as_u64().unwrap() as u16;
        if ![401, 404, 405, 307].contains(&expected) {
            continue;
        }
        let mut req = Request::builder()
            .method(case["method"].as_str().unwrap())
            .uri(case["path"].as_str().unwrap())
            .header("host", "qa.example:8080")
            .header("content-type", "application/json");
        if case["auth"] == true {
            req = req.header("x-model-loader-token", "qa-token");
        }
        let response = app
            .clone()
            .oneshot(
                req.body(Body::from(
                    case["request_body"].as_str().unwrap().to_owned(),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected, "{case}");
        for (key, value) in case["headers"].as_object().unwrap() {
            assert_eq!(
                response.headers().get(key).unwrap(),
                value.as_str().unwrap(),
                "{case}"
            );
        }
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            bytes.as_ref(),
            case["body"].as_str().unwrap().as_bytes(),
            "{case}"
        );
    }
    for (path, expected) in [("/api/v1/%68ealth", 200), ("/api/v1/%2568ealth", 401)] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    let unavailable = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/%73ettings")
                .header("x-model-loader-token", "qa-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        unavailable.status(),
        503,
        "missing native owner must not forward"
    );
}
#[tokio::test]
async fn encoded_backend_names_preserve_body_order_and_effective_name_controls() {
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };
    use tower::ServiceExt;
    struct Docker {
        inventories: AtomicUsize,
        specs: Mutex<Vec<String>>,
    }
    impl crate::DockerRead for Docker {
        fn containers(&self) -> crate::Job<'_, Vec<crate::Container>> {
            self.inventories.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(vec![]) })
        }
        fn logs<'a>(&'a self, _: &'a str, _: usize) -> crate::Job<'a, Vec<u8>> {
            panic!("unexpected logs")
        }
        fn backend_spec<'a>(&'a self, name: &'a str) -> crate::Job<'a, Option<crate::BackendSpec>> {
            self.specs.lock().unwrap().push(name.into());
            Box::pin(async { Ok(None) })
        }
        fn restart_backend<'a>(&'a self, _: &'a str) -> crate::Job<'a, ()> {
            panic!("no restart approved")
        }
    }
    let docker = Arc::new(Docker {
        inventories: AtomicUsize::new(0),
        specs: Mutex::new(vec![]),
    });
    let mut state = crate::test_support::test_state("http://127.0.0.1:9");
    state.configured = vec!["qa-backend".into()];
    state.docker = docker.clone();
    state.backends = Some(Arc::new(crate::backends::Backends::new(None).unwrap()));
    let app = crate::router(Arc::new(state)).layer(Extension(Enabled));
    for (path, body, expected, inventories) in [
        ("/%61pi/v1/backends/qa%2Dbackend/%72estart", "{bad", 200, 1),
        ("/api/v1/backends/qa%2Dbackend/%74est", "{bad", 422, 1),
        ("/api/v1/backends/qa%252Fbackend/test", "{bad", 422, 1),
        ("/api/v1/backends/qa%2Fbackend/test", "{bad", 404, 1),
        ("/api/v1/backends/qa%252Fbackend/test", "{}", 404, 2),
        ("/api/v1/backends/qa%2Dbackend/test", "{}", 400, 3),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
        assert_eq!(
            docker.inventories.load(Ordering::SeqCst),
            inventories,
            "{path}"
        );
        if expected == 200 {
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                serde_json::json!({"ok":false,"message":"container not found"})
            );
        }
    }
    assert_eq!(*docker.specs.lock().unwrap(), vec!["qa-backend"]);
}

#[test]
fn trusted_configuration_must_be_readable_well_formed_and_nonempty_for_native_routing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trusted.json");
    assert!(crate::search::live::load_trusted(&path).is_err());
    for value in [
        "{bad",
        "{}",
        "{\"quantisers\":[],\"labs\":[]}",
        "{\"quantisers\":[\"qa\"],\"labs\":[]}",
        "{\"quantisers\":[],\"labs\":[\"qa\"]}",
    ] {
        std::fs::write(&path, value).unwrap();
        assert!(
            !crate::search::live::load_trusted(&path).is_ok_and(|t| t.routing_ready()),
            "{value}"
        );
    }
    std::fs::write(
        &path,
        r#"{"quantisers":["qa-publisher"],"labs":["qa-lab"]}"#,
    )
    .unwrap();
    assert!(crate::search::live::load_trusted(&path)
        .unwrap()
        .routing_ready());
}
