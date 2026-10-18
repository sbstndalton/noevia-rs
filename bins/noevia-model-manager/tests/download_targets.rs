//! noevia#1318: native read-only `GET /api/v1/download-targets` against the actual Python answers.
//!
//! `fixtures/download-targets-python.v1.json` is produced by `tools/gen-download-targets-oracle.py`,
//! which runs the real services FastAPI app on synthetic models folders and synthetic mount tables;
//! CI regenerates it from the pinned services ref and requires identical bytes. Each case's tree and
//! mount table are rebuilt here from the same declaration. Nothing real is read or mounted.
#![cfg(feature = "native-download-targets")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{
    download_targets::DownloadTargets, Container, DockerRead, Redactor, StateData,
};
use serde_json::Value;
use std::{future::Future, path::Path, pin::Pin, sync::Arc};
use tower::ServiceExt;

/// sha256 of the services files the fixture was generated from (noevia-services 1d44f5e2).
const API_SHA: &str = "800edae73e7f7764845510ddf4e108bc2ea0a639f721e06e825a4a4644600dd6";
const SERVICES_SHA: &str = "aadf227ddc3666cebb1631a0ae543c8090539a382e8ca52710fc7a5458f1d041";
const MAIN_SHA: &str = "87eeac941fa5c0687160f315517b60c02b29324266d780851c547f6a1d749ee2";
const CONFIG_SHA: &str = "ab6e348127b65f224d58fa2f805fefdfc7a472e14eb9e9ff540120e51f21f57f";
const TOKEN: &str = "synthetic-model-loader-token-0123456789abcdef";
const PATH: &str = "/api/v1/download-targets";

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

fn state(targets: Option<DownloadTargets>, legacy_base: &str) -> Arc<StateData> {
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
        download_targets: targets.map(Arc::new),
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

struct Answer {
    status: u16,
    content_type: Option<String>,
    body: Vec<u8>,
}

async fn send(state: Arc<StateData>, method: &str, path: &str, token: bool) -> Answer {
    let mut request = Request::builder().method(method).uri(path);
    if token {
        request = request.header("x-model-loader-token", TOKEN);
    }
    let response = noevia_model_manager::router(state)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    Answer {
        status: response.status().as_u16(),
        content_type: response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned()),
        body: response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    }
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/download-targets-python.v1.json")).unwrap()
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// The same tree and mount table the generator built; returns (root, mount table path).
fn build(base: &Path, case: &Value) -> (String, std::path::PathBuf) {
    let real = base.join("real-models");
    let root = match case["root"].as_str().unwrap() {
        "missing" => base.join("no-such-models"),
        mode => {
            std::fs::create_dir(&real).unwrap();
            for entry in case["entries"].as_array().unwrap() {
                let name = entry[1].as_str().unwrap();
                match entry[0].as_str().unwrap() {
                    "d" => std::fs::create_dir(real.join(name)).unwrap(),
                    "f" => std::fs::write(real.join(name), b"synthetic").unwrap(),
                    _ => std::os::unix::fs::symlink(entry[2].as_str().unwrap(), real.join(name))
                        .unwrap(),
                }
            }
            if mode == "symlink" {
                let link = base.join("models-link");
                std::os::unix::fs::symlink(&real, &link).unwrap();
                link
            } else {
                real.clone()
            }
        }
    };
    let root = root.to_str().unwrap().to_owned();
    let table = base.join("mountinfo");
    let raw = match &case["mountinfo"] {
        Value::Null => None,
        Value::String(text) => Some(text.as_bytes().to_vec()),
        Value::Object(o) => Some(unhex(o["hex"].as_str().unwrap())),
        other => panic!("unknown table {other}"),
    };
    if let Some(raw) = raw {
        let text = replace(&raw, b"{ROOT}", root.as_bytes());
        std::fs::write(
            &table,
            replace(&text, b"{REAL}", real.to_str().unwrap().as_bytes()),
        )
        .unwrap();
    }
    (root, table)
}

fn replace(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

#[test]
fn fixture_provenance_is_the_pinned_services_source() {
    let oracle = &fixture()["oracle"];
    assert_eq!(oracle["api.py"], API_SHA);
    assert_eq!(oracle["services.py"], SERVICES_SHA);
    assert_eq!(oracle["main.py"], MAIN_SHA);
    assert_eq!(oracle["config.py"], CONFIG_SHA);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_python_differential() {
    let cases = fixture()["cases"].as_array().unwrap().clone();
    assert!(cases.len() >= 20);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (root, table) = build(&base, &case);
        let targets = DownloadTargets::new(&root, case["declared"].as_str().unwrap(), &table);
        let answer = send(
            state(Some(targets), "http://127.0.0.1:9"),
            "GET",
            PATH,
            true,
        )
        .await;
        assert_eq!(
            u64::from(answer.status),
            case["status"].as_u64().unwrap(),
            "{id}"
        );
        assert_eq!(
            answer.content_type.as_deref(),
            case["content_type"].as_str(),
            "{id}"
        );
        let body = String::from_utf8(answer.body)
            .unwrap()
            .replace(&root, "{ROOT}");
        assert_eq!(body, case["body"].as_str().unwrap(), "{id}");
    }
}

/// A one-shot fake for the Python child: records the request line and answers fixed JSON.
async fn legacy() -> (String, tokio::task::JoinHandle<String>) {
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
        let body = br#"{"legacy":true}"#;
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(body).await.unwrap();
        String::from_utf8_lossy(&seen)
            .lines()
            .next()
            .unwrap()
            .to_owned()
    });
    (base, task)
}

fn synthetic() -> (tempfile::TempDir, DownloadTargets) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let targets = DownloadTargets::new(root.to_str().unwrap(), "", &root.join("no-table"));
    (dir, targets)
}

#[tokio::test]
async fn python_mode_forwards_get_unchanged() {
    let (base, seen) = legacy().await;
    let answer = send(state(None, &base), "GET", PATH, true).await;
    assert_eq!(answer.body, br#"{"legacy":true}"#);
    assert_eq!(seen.await.unwrap(), "GET /api/v1/download-targets HTTP/1.1");
}

#[tokio::test]
async fn downloads_start_stays_with_python() {
    // POST /downloads validates `target` against Python's own list: never answered here.
    let (_dir, targets) = synthetic();
    let (base, seen) = legacy().await;
    let answer = send(
        state(Some(targets), &base),
        "POST",
        "/api/v1/downloads",
        true,
    )
    .await;
    assert_eq!(answer.body, br#"{"legacy":true}"#);
    assert_eq!(seen.await.unwrap(), "POST /api/v1/downloads HTTP/1.1");
}

#[tokio::test]
async fn native_route_keeps_auth_and_non_get_policy() {
    let (_dir, targets) = synthetic();
    let state = state(Some(targets), "http://127.0.0.1:9");
    let unauthorized = send(state.clone(), "GET", PATH, false).await;
    assert_eq!(unauthorized.status, 401);
    assert_eq!(unauthorized.body, br#"{"error":"unauthorized"}"#);
    for method in ["POST", "PUT", "DELETE"] {
        assert_eq!(
            send(state.clone(), method, PATH, true).await.status,
            405,
            "{method}"
        );
    }
    let ok = send(state.clone(), "GET", PATH, true).await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.content_type.as_deref(), Some("application/json"));
}

#[tokio::test]
async fn entry_cap_answers_500_instead_of_reading_on() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    for i in 0..=noevia_model_manager::download_targets::ENTRY_CAP {
        std::fs::write(root.join(format!("f{i}")), b"").unwrap();
    }
    let targets = DownloadTargets::new(root.to_str().unwrap(), "", &root.join("none"));
    let answer = send(
        state(Some(targets), "http://127.0.0.1:9"),
        "GET",
        PATH,
        true,
    )
    .await;
    assert_eq!(answer.status, 500);
    assert_eq!(answer.body, b"Internal Server Error");
    assert_eq!(
        answer.content_type.as_deref(),
        Some("text/plain; charset=utf-8")
    );
}

#[tokio::test]
async fn oversized_mount_table_answers_500() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let table = root.join("mountinfo");
    let cap = usize::try_from(noevia_model_manager::download_targets::MOUNTINFO_CAP).unwrap();
    for (len, status) in [(cap, 200), (cap + 1, 500)] {
        std::fs::write(&table, vec![b'x'; len]).unwrap();
        let targets = DownloadTargets::new(root.to_str().unwrap(), "", &table);
        let answer = send(
            state(Some(targets), "http://127.0.0.1:9"),
            "GET",
            PATH,
            true,
        )
        .await;
        assert_eq!(answer.status, status, "{len}");
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn non_utf8_names_match_python() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let odd = std::ffi::OsStr::from_bytes(b"bad\xffname");
    std::fs::create_dir(root.join(odd)).unwrap();
    // Not a mount point and never declared (env is UTF-8): ignored, as Python ignores it.
    let targets = DownloadTargets::new(root.to_str().unwrap(), "", &root.join("none"));
    let answer = send(
        state(Some(targets), "http://127.0.0.1:9"),
        "GET",
        PATH,
        true,
    )
    .await;
    assert_eq!(answer.status, 200);
    // Listed by the mount table under its replacement-decoded spelling: still no match.
    let table = root.join("mountinfo");
    let mut line = b"1 2 0:3 / ".to_vec();
    line.extend_from_slice(root.as_os_str().as_bytes());
    line.extend_from_slice(b"/bad\xffname rw - ext4 x rw");
    std::fs::write(&table, line).unwrap();
    let targets = DownloadTargets::new(root.to_str().unwrap(), "", &table);
    let answer = send(
        state(Some(targets), "http://127.0.0.1:9"),
        "GET",
        PATH,
        true,
    )
    .await;
    assert_eq!(answer.status, 200);
    assert!(!String::from_utf8_lossy(&answer.body).contains("bad"));
}
