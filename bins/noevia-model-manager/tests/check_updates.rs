//! noevia#1318: native `POST /api/v1/models/check-updates` against the actual Python answers.
//!
//! `fixtures/check-updates-python.v1.json` comes from `tools/gen-check-updates-oracle.py`, which
//! runs the real services FastAPI app on synthetic stores and model folders with a local fake Hub
//! and a pinned clock. It records the answer, every Hub request and the `update_check` table
//! afterwards; this test replays each case against the same fake Hub and requires all three to be
//! identical. CI regenerates the fixture from the pinned services ref and requires identical bytes.
#![cfg(feature = "native-check-updates")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{
    check_updates::CheckUpdates, Container, DockerRead, Redactor, StateData,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

/// sha256 of the services files the fixture was generated from (noevia-services d475f42).
const API_SHA: &str = "eb8e12f6929baf15d2b1a1998aceff9529fbe48ca8242094ac6b7526c253a91d";
const DB_SHA: &str = "7ac62912f2295d0c96387f55c21e6f42e5815800c44a3920b7ebc2f3a0217478";
const HELPERS_SHA: &str = "fcf2ccb14e5e068f97bb7a4b7985ab7f4f4855b940eb6d9e3838f239ad97918d";
const HF_SHA: &str = "43fcc6344cf4792e70ce7d2bfe141b90c78d2ecff253236f8d352542988a55fa";
const SERVICES_SHA: &str = "aadf227ddc3666cebb1631a0ae543c8090539a382e8ca52710fc7a5458f1d041";
const TOKEN: &str = "synthetic-model-loader-token-0123456789abcdef";
const NOW: f64 = 1_700_000_500.75;

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

fn clock() -> f64 {
    NOW
}

fn state(check: Option<CheckUpdates>) -> Arc<StateData> {
    Arc::new(StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis: None,
        prompts: None,
        #[cfg(feature = "native-settings")]
        settings: None,
        model_updates: None,
        #[cfg(feature = "native-model-detail")]
        model_detail: None,
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
        check_updates: check.map(Arc::new),
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
        legacy_base: "http://127.0.0.1:9".into(),
        idle_unload: None,
    })
}

async fn post(state: Arc<StateData>, method: &str) -> (u16, Option<String>, String) {
    let response = noevia_model_manager::router(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri("/api/v1/models/check-updates")
                .header("x-model-loader-token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8(body.to_vec()).unwrap(),
    )
}

/// A fake Hub: answers each path from `hub` (404 otherwise) and records every request.
async fn hub(responses: Value) -> (String, Arc<Mutex<Vec<Value>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&buf[..n]);
            }
            let text = String::from_utf8_lossy(&head).into_owned();
            let mut lines = text.split("\r\n");
            let mut first = lines.next().unwrap().split(' ');
            let method = first.next().unwrap().to_owned();
            let target = first.next().unwrap().to_owned();
            let (path, query) = target.split_once('?').unwrap_or((&target, ""));
            let authorization = lines
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.trim_start().to_owned());
            record.lock().unwrap().push(json!({
                "method": method, "path": path, "query": query, "authorization": authorization,
            }));
            let answer = responses
                .get(path)
                .cloned()
                .unwrap_or(json!({"status": 404, "body": hex(br#"{"error":"not found"}"#)}));
            let body = unhex(answer["body"].as_str().unwrap());
            let head = format!(
                "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                answer["status"],
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        }
    });
    (base, seen)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/check-updates-python.v1.json")).unwrap()
}

/// The store, the models folder and the ini path for one case.
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
    let models = root.join("models");
    std::fs::create_dir(&models).unwrap();
    let mtime =
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(case["mtime"].as_u64().unwrap());
    for name in case["models"].as_array().unwrap() {
        let path = models.join(name.as_str().unwrap());
        std::fs::write(&path, b"GGUF").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }
}

fn table(root: &Path) -> Value {
    let c = Connection::open(root.join("model_loader.db")).unwrap();
    let mut statement = c
        .prepare(
            "SELECT quote(filename), quote(repo_id), quote(hf_last_modified), quote(checked_at) \
             FROM update_check ORDER BY filename",
        )
        .unwrap();
    let rows: Vec<Value> = statement
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?
            ]))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    Value::Array(rows)
}

fn check(root: &Path, base: &str) -> CheckUpdates {
    let models = root.join("models");
    CheckUpdates::with(&models, &models.join("models.ini"), root, base, clock).unwrap()
}

#[test]
fn fixture_provenance_is_the_pinned_services_source() {
    let oracle = &fixture()["oracle"];
    assert_eq!(oracle["api.py"], API_SHA);
    assert_eq!(oracle["db.py"], DB_SHA);
    assert_eq!(oracle["helpers.py"], HELPERS_SHA);
    assert_eq!(oracle["hf.py"], HF_SHA);
    assert_eq!(oracle["services.py"], SERVICES_SHA);
    assert_eq!(fixture()["now"].as_f64(), Some(NOW));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_python_differential() {
    let cases = fixture()["cases"].as_array().unwrap().clone();
    assert!(cases.len() >= 25);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), &case);
        let (base, seen) = hub(case["hub"].clone()).await;
        let (status, content_type, body) =
            post(state(Some(check(dir.path(), &base))), "POST").await;
        // The models-folder walk is the kernel-contained (Linux) one; elsewhere a check that
        // Python answers 200 ends in the walk's 503 refusal, after the same requests and writes.
        if cfg!(target_os = "linux") || case["status"] != 200 {
            assert_eq!(
                u64::from(status),
                case["status"].as_u64().unwrap(),
                "{id}: {body}"
            );
            assert_eq!(
                content_type.as_deref(),
                case["content_type"].as_str(),
                "{id}"
            );
            assert_eq!(body, case["body"].as_str().unwrap(), "{id}");
        } else {
            assert_eq!(status, 503, "{id}: {body}");
        }
        let seen = Value::Array(seen.lock().unwrap().clone());
        assert_eq!(seen, case["requests"], "{id}: Hub requests");
        assert_eq!(
            table(dir.path()),
            case["update_check"],
            "{id}: update_check"
        );
    }
}

#[tokio::test]
async fn only_post_of_the_exact_path_is_native() {
    // Omission: no native writer, so the route is forwarded (here to a dead child: 502).
    let (status, _, _) = post(state(None), "POST").await;
    assert_eq!(status, 502);
    let dir = tempfile::tempdir().unwrap();
    build(dir.path(), &fixture()["cases"][1]);
    // GET is not a check: the front's own 405 (Python's route is POST only).
    let (status, _, _) = post(state(Some(check(dir.path(), "http://127.0.0.1:9"))), "GET").await;
    assert_eq!(status, 405);
}

#[tokio::test]
async fn unreachable_hub_records_no_dates() {
    let case = fixture()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "hub-500")
        .unwrap()
        .clone();
    let dir = tempfile::tempdir().unwrap();
    build(dir.path(), &case);
    // Nothing listens on port 9: httpx.HTTPError in Python, the same empty dates here.
    let (status, _, body) = post(
        state(Some(check(dir.path(), "http://127.0.0.1:9/api"))),
        "POST",
    )
    .await;
    if cfg!(target_os = "linux") {
        assert_eq!(status, 200);
        assert_eq!(body, case["body"].as_str().unwrap());
    } else {
        assert_eq!(status, 503);
    }
    assert_eq!(table(dir.path()), case["update_check"]);
}

/// noevia#1996: a tree page over the node limit (here `[1, 1, …]`, under the 16 MiB size cap) is
/// a failed request, answered exactly as Python answers a page it cannot parse (the recorded
/// `invalid-json` case: no dates). Unbounded, it would be parsed into ~262K big integers per
/// 512 KiB (a 64 MiB page: ~2 GiB), and Python would answer 500 for the non-object entries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_over_the_node_limit_records_no_dates() {
    let mut case = fixture()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "invalid-json")
        .unwrap()
        .clone();
    let nodes = noevia_model_manager::hub_page::PAGE_NODES;
    let page = format!("[{}]", vec!["1"; nodes].join(","));
    assert!(page.len() < noevia_model_manager::hub_page::PAGE_CAP);
    for answer in case["hub"].as_object_mut().unwrap().values_mut() {
        if answer["status"] == 200 {
            answer["body"] = json!(hex(page.as_bytes()));
        }
    }
    let dir = tempfile::tempdir().unwrap();
    build(dir.path(), &case);
    let (base, seen) = hub(case["hub"].clone()).await;
    let (status, _, body) = post(state(Some(check(dir.path(), &base))), "POST").await;
    if cfg!(target_os = "linux") {
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, case["body"].as_str().unwrap());
    } else {
        assert_eq!(status, 503, "{body}");
    }
    assert_eq!(Value::Array(seen.lock().unwrap().clone()), case["requests"]);
    assert_eq!(table(dir.path()), case["update_check"]);
}
