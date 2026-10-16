#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;
use serde_json::Value;
pub(super) fn oracle() -> Value {
    use sha2::{Digest, Sha256};
    let bytes = if let Some(path) = std::env::var_os("NOEVIA_PROMPTS_ORACLE") {
        std::fs::read(path).unwrap()
    } else {
        include_bytes!("../../tests/fixtures/prompts-python.v1.json").to_vec()
    };
    assert!(bytes.len() <= 16_777_216);
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["services"],
        "2360cb51f6475122ba0aa3d32272849a63028d66"
    );
    assert_eq!(
        value["generator_sha256"],
        format!(
            "{:x}",
            Sha256::digest(include_bytes!("../bin/generate-prompts-oracle.rs"))
        )
    );
    assert_eq!(
        value["process_sha256"],
        format!(
            "{:x}",
            Sha256::digest(include_bytes!("../bin/support/prompts_process.rs"))
        )
    );
    value
}
pub(super) fn body(q: &Value) -> Vec<u8> {
    if let Some(hex) = q["body_hex"].as_str() {
        hex.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
            .collect()
    } else {
        q["body"].as_str().unwrap_or("").as_bytes().to_vec()
    }
}
#[test]
fn literal_bytes_and_python_coercion_from_real_oracle() {
    let oracle = oracle();
    let ranges: Vec<(u32, u32)> = oracle["cases"][0]["expected"]["nonprintable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r[0].as_u64().unwrap() as u32, r[1].as_u64().unwrap() as u32))
        .collect();
    assert_eq!(literals::NONPRINTABLE, ranges);
    for ((name, body), literal) in literals::SEED
        .iter()
        .zip(oracle["versions"]["seed"].as_array().unwrap())
    {
        assert_eq!(*name, literal[0].as_str().unwrap());
        assert_eq!(*body, literal[1].as_str().unwrap());
    }
    for case in oracle["cases"].as_array().unwrap() {
        if case["request"]["op"] == "coercion" {
            let fields =
                parse_fields(case["request"]["body"].as_str().unwrap().as_bytes()).unwrap();
            assert_eq!(
                serde_json::json!([fields.0, fields.1]),
                case["expected"]["fields"],
                "coercion case {}",
                case["id"]
            );
            continue;
        }
        if case["request"]["method"] != "POST" || case["request"]["path"] != "/api/v1/prompts" {
            continue;
        }
        if !json_content_type(case["request"]["content_type"].as_str()) {
            continue;
        }
        let result = parse_fields(&body(&case["request"]));
        if case["expected"]["status"] == 200 {
            let id = &case["expected"]["body"]["id"];
            let prompt = case["expected"]["body"]["prompts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["id"] == *id)
                .unwrap();
            assert_eq!(
                result.unwrap(),
                (
                    prompt["name"].as_str().unwrap().to_owned(),
                    prompt["body"].as_str().unwrap().to_owned()
                )
            );
        } else {
            assert!(result.is_err(), "{case}");
        }
    }
}
#[test]
fn resource_caps_apply_before_native_text_and_store() {
    assert_eq!(
        (NAME_CAP, BODY_CAP, ROW_CAP, RESPONSE_CAP),
        (4096, 262_144, 4096, 16_777_216)
    );
    assert_eq!(
        parse_fields(
            format!(
                "{{\"name\":\"{}\",\"body\":\"x\"}}",
                "a".repeat(NAME_CAP + 1)
            )
            .as_bytes()
        ),
        Err(Error::Text)
    );
    assert_eq!(
        parse_fields(
            format!(
                "{{\"name\":\"x\",\"body\":\"{}\"}}",
                "a".repeat(BODY_CAP + 1)
            )
            .as_bytes()
        ),
        Err(Error::Text)
    );
    assert!(parse_fields(
        format!(
            "{{\"name\":\"x\",\"body\":{}0{}}}",
            "[".repeat(33),
            "]".repeat(33)
        )
        .as_bytes()
    )
    .is_err());
    assert_eq!(
        parse_fields(b"{\"name\":NaN,\"body\":\"x\"}"),
        Ok(("nan".into(), "x".into()))
    );
    assert!(parse_fields(b"{\"name\":\"x\",\"body\":\"\\ud800\"}").is_err());
}
proptest::proptest! {
    #[test]
    fn direct_unicode_string_and_python_strip_are_preserved(s in ".{1,128}") {
        let raw=serde_json::to_vec(&serde_json::json!({"name":"fixture","body":format!("\u{001c}{s}\u{3000}")})).unwrap();
        let expected=model_autoconfig::pyval::py_strip(&s);
        if expected.is_empty(){proptest::prop_assert_eq!(parse_fields(&raw),Err(Error::Required));}
        else {proptest::prop_assert_eq!(parse_fields(&raw).unwrap().1,expected);}
    }
    #[test]
    fn signed_integer_id_roundtrip(id in proptest::num::i64::ANY) {proptest::prop_assert_eq!(parse_id(&id.to_string()),Ok(id));}
}

struct NoDocker;
impl crate::DockerRead for NoDocker {
    fn containers(&self) -> crate::Job<'_, Vec<crate::Container>> {
        Box::pin(async { panic!("prompt route reached Docker") })
    }
    fn logs<'a>(&'a self, _: &'a str, _: usize) -> crate::Job<'a, Vec<u8>> {
        Box::pin(async { panic!("prompt route reached Docker") })
    }
}
async fn state(root: &std::path::Path) -> std::sync::Arc<crate::StateData> {
    std::sync::Arc::new(crate::StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis: None,
        #[cfg(feature = "native-settings")]
        settings: None,
        #[cfg(feature = "native-model-updates")]
        model_updates: None,
        #[cfg(feature = "native-model-detail")]
        model_detail: None,
        prompts: Some(std::sync::Arc::new(
            Prompts::open_with_clock(root, || Ok(1_700_000_000.125))
                .await
                .unwrap(),
        )),
        sections: None,
        embedding: None,
        host: None,
        token: "synthetic-service-token".into(),
        configured: vec![],
        docker: std::sync::Arc::new(NoDocker),
        redactor: crate::Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(4),
        legacy: crate::legacy_client().unwrap(),
        legacy_base: crate::LEGACY_BASE.to_owned(),
        idle_unload: None,
        #[cfg(feature = "native-download-targets")]
        download_targets: None,
        #[cfg(feature = "native-badges")]
        badges: None,
        #[cfg(feature = "native-bench-runs")]
        bench_runs: None,
        #[cfg(feature = "native-section-writes")]
        section_writes: None,
    })
}
fn root() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap()
}
#[test]
fn prompt_namespace_preserves_unrelated_malformed_ownership() {
    for path in [
        "/api/v1/not-owned/%zz",
        "/api/v1/sections/%ff",
        "/api/v1/promptsfoo/%zz",
        "/api/v1/%zz/prompts",
        "/api/v1/prompts%zz",
    ] {
        assert_eq!(classify(path), Route::NotOwned, "{path}");
    }
    assert_eq!(
        classify(&format!("/other/{}", "x".repeat(8192))),
        Route::NotOwned
    );
    for path in [
        "/api/v1/prompts/%zz",
        "/api/v1/%70rompts/%ff",
        "/api/v1/prompts%2f1",
    ] {
        assert_eq!(classify(path), Route::Invalid, "{path}");
    }
    assert_eq!(classify("/api/v1/%70rompts"), Route::Collection);
    assert_eq!(
        classify("/api/v1/prompts/"),
        Route::Redirect("/api/v1/prompts".into())
    );
}
#[tokio::test]
async fn prompt_selector_preserves_unrelated_dispatch_and_auth_first() {
    let root = root();
    let mut on = state(root.path()).await;
    for path in ["/api/v1/not-owned/%zz", "/api/v1/not-owned/%ff"] {
        assert_eq!(request(on.clone(), "GET", path, "", true).await.0, 404);
        assert_eq!(request(on.clone(), "GET", path, "", false).await.0, 401);
    }
    assert_eq!(
        request(on.clone(), "GET", "/api/v1/prompts/%zz", "", true)
            .await
            .0,
        400
    );
    assert_eq!(
        request(on.clone(), "GET", "/api/v1/prompts/%zz", "", false)
            .await
            .0,
        401
    );
    let ordinary = request(on.clone(), "GET", "/api/v1/sections/ordinary", "", true).await;
    let malformed = request(on.clone(), "GET", "/api/v1/sections/%zz", "", true).await;
    std::sync::Arc::get_mut(&mut on).unwrap().prompts = None;
    assert_eq!(
        request(on.clone(), "GET", "/api/v1/sections/ordinary", "", true).await,
        ordinary
    );
    assert_eq!(
        request(on.clone(), "GET", "/api/v1/sections/%zz", "", true).await,
        malformed
    );
    assert_eq!(
        request(on, "GET", "/api/v1/not-owned/%zz", "", true)
            .await
            .0,
        404
    );
}
async fn request(
    state: std::sync::Arc<crate::StateData>,
    method: &str,
    path: &str,
    body: &str,
    auth: bool,
) -> (u16, Value, Option<String>) {
    request_bytes(
        state,
        method,
        path,
        body.as_bytes(),
        auth,
        Some("application/json"),
    )
    .await
}
async fn request_bytes(
    state: std::sync::Arc<crate::StateData>,
    method: &str,
    path: &str,
    body: &[u8],
    auth: bool,
    content_type: Option<&str>,
) -> (u16, Value, Option<String>) {
    use tower::ServiceExt;
    let url = reqwest::Url::parse(&format!("http://testserver{path}")).unwrap();
    let path = format!(
        "{}{}",
        url.path(),
        url.query().map(|q| format!("?{q}")).unwrap_or_default()
    );
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "testserver");
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    if auth {
        builder = builder.header("x-model-loader-token", "synthetic-service-token");
    }
    let response = crate::router(state)
        .oneshot(builder.body(axum::body::Body::from(body.to_vec())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .map(|h| h.to_str().unwrap().to_owned());
    let bytes = axum::body::to_bytes(response.into_body(), RESPONSE_CAP)
        .await
        .unwrap();
    let value = if method == "HEAD" {
        Value::String(String::new())
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8(bytes.to_vec()).unwrap()))
    };
    (status, value, location)
}
#[tokio::test]
async fn malformed_json_diagnostics_match_actual_fastapi_character_offsets() {
    let root = root();
    let state = state(root.path()).await;
    let oracle = oracle();
    let mut count = 0;
    let cases = oracle["cases"].as_array().unwrap();
    let end = cases
        .iter()
        .rposition(|case| case["request"]["name"] == "raw-encoding")
        .unwrap();
    for case in cases.iter().take(end + 1).skip(2) {
        let q = &case["request"];
        if q["name"] == "json-diagnostic" || q["name"] == "raw-encoding" {
            count += 1;
        }
        let response = request_bytes(
            state.clone(),
            "POST",
            "/api/v1/prompts",
            &body(q),
            true,
            Some("application/json"),
        )
        .await;
        assert_eq!(
            response.0,
            case["expected"]["status"].as_u64().unwrap() as u16,
            "{q}"
        );
        assert_eq!(response.1, case["expected"]["body"], "{q}");
    }
    assert_eq!(count, 42);
}
#[tokio::test]
async fn whole_http_actual_python_corpus_and_explicit_narrowing() {
    let root = root();
    let state = state(root.path()).await;
    let oracle = oracle();
    for case in oracle["cases"].as_array().unwrap().iter().skip(2) {
        if case["request"].get("op").is_some() {
            continue;
        }
        let q = &case["request"];
        let method = q["method"].as_str().unwrap();
        let path = q["path"].as_str().unwrap();
        let content_type = if q.get("content_type").is_some() {
            q["content_type"].as_str()
        } else {
            Some("application/json")
        };
        let response =
            request_bytes(state.clone(), method, path, &body(q), true, content_type).await;
        // Approved bounded compatibility restrictions are asserted, never silently skipped.
        if path.contains("%FF") || path.ends_with('%') || path.contains("%2F") {
            assert_eq!(response.0, 400, "{q}");
            continue;
        }
        if path.ends_with("9223372036854775808") {
            assert_eq!(response.0, 422);
            continue;
        }
        assert_eq!(
            response.0,
            case["expected"]["status"].as_u64().unwrap() as u16,
            "{q}"
        );
        assert_eq!(response.1, case["expected"]["body"], "{q}");
        assert_eq!(
            response.2,
            case["expected"]["location"].as_str().map(str::to_owned),
            "{q}"
        );
    }
}
#[tokio::test]
async fn auth_precedes_method_path_body_and_no_fallback() {
    let root = root();
    let state = state(root.path()).await;
    for (method, path, body) in [
        ("POST", "/api/v1/prompts", "{"),
        ("DELETE", "/api/v1/prompts/%FF", ""),
        ("PUT", "/api/v1/prompts", "{}"),
        ("GET", "/api/v1/prompts/1", ""),
        ("GET", "/api/v1/prompts/unknown/path", ""),
    ] {
        assert_eq!(
            request(state.clone(), method, path, body, false).await,
            (401, serde_json::json!({"error":"unauthorized"}), None)
        );
    }
    let db = rusqlite::Connection::open(root.path().join("model_loader.db")).unwrap();
    db.execute(
        "INSERT INTO prompts(name,body,created_at) VALUES('oversize',?,1)",
        ["x".repeat(BODY_CAP + 1)],
    )
    .unwrap();
    let failed = request(state, "GET", "/api/v1/prompts", "", true).await;
    assert_eq!(failed.0, 503);
    assert_eq!(
        failed.1,
        serde_json::json!({"detail":"prompt store exceeded limit"})
    );
}

#[tokio::test]
async fn explicit_raw_input_restrictions_have_actual_oracle_proof_and_no_writes() {
    let root = root();
    let state = state(root.path()).await;
    let oracle = oracle();
    let before = request(state.clone(), "GET", "/api/v1/prompts", "", true).await;
    let mut count = 0;
    for case in oracle["cases"].as_array().unwrap() {
        if case["request"]["op"] != "restriction" {
            continue;
        }
        count += 1;
        assert!([200, 500].contains(&case["expected"]["status"].as_u64().unwrap()));
        let q = &case["request"]["request"];
        let actual = request_bytes(
            state.clone(),
            "POST",
            "/api/v1/prompts",
            &body(q),
            true,
            q["content_type"].as_str().or(Some("application/json")),
        )
        .await;
        assert_eq!(
            actual.0,
            case["request"]["native_refusal"]["status"]
                .as_u64()
                .unwrap() as u16,
            "{}",
            case["request"]["name"]
        );
        assert_eq!(actual.1, case["request"]["native_refusal"]["body"]);
        assert_eq!(
            request(state.clone(), "GET", "/api/v1/prompts", "", true).await,
            before
        );
    }
    assert_eq!(count, 10);
}
