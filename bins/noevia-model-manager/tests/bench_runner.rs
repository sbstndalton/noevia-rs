//! noevia#1318: the native benchmark runner (prompt suite) against the actual Python runner.
//!
//! `fixtures/bench-runner-python.v1.json` comes from `tools/gen-bench-runner-oracle.py`, which
//! drives the real services routes and `bench.py` against a fake llama-server, a fake Docker
//! client, fixed observations and fixed-step clocks, with the worker held back until a scripted
//! `run` step. This test replays every step against the native routes with the same fakes and
//! requires the same answers, the same llama-server requests, the same observation calls and the
//! same `bench_run` / `bench_variant` / `bench_result` rows.
#![cfg(feature = "native-benchmark-runner")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{
    bench_runner::{Clock, Observed, Runner, Task},
    Container, DockerRead, Redactor, StateData,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tower::ServiceExt;

/// sha256 of the services files the fixture was generated from (noevia-services d475f42).
const API_SHA: &str = "eb8e12f6929baf15d2b1a1998aceff9529fbe48ca8242094ac6b7526c253a91d";
const BENCH_SHA: &str = "2e59f9b057e2686b7f05e5dac04826c8ff37e3c1eb4398bf671faa2d74f3b14b";
const DB_SHA: &str = "7ac62912f2295d0c96387f55c21e6f42e5815800c44a3920b7ebc2f3a0217478";
const TOKEN: &str = "synthetic-model-loader-token-0123456789abcdef";

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

static PERF: AtomicU64 = AtomicU64::new(0);
static WALL: AtomicU64 = AtomicU64::new(0);

#[allow(clippy::cast_precision_loss)]
fn perf() -> f64 {
    100.0 + 0.125 * PERF.fetch_add(1, Ordering::SeqCst) as f64
}

#[allow(clippy::cast_precision_loss)]
fn wall() -> f64 {
    1_700_000_000.0 + 1.5 * WALL.fetch_add(1, Ordering::SeqCst) as f64
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/bench-runner-python.v1.json")).unwrap()
}

fn state(runner: Arc<Runner>) -> Arc<StateData> {
    Arc::new(StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis: None,
        prompts: None,
        #[cfg(feature = "native-settings")]
        settings: None,
        #[cfg(feature = "native-model-updates")]
        model_updates: None,
        #[cfg(feature = "native-model-detail")]
        model_detail: None,
        #[cfg(feature = "native-model-list")]
        model_list: None,
        #[cfg(feature = "native-download-targets")]
        download_targets: None,
        #[cfg(feature = "native-badges")]
        badges: None,
        bench_runs: None,
        section_writes: None,
        #[cfg(feature = "native-settings-write")]
        settings_write: None,
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
        bench_runner: Some(runner),
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

async fn send(state: Arc<StateData>, method: &str, uri: &str, body: Option<String>) -> Value {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-model-loader-token", TOKEN);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let response = noevia_model_manager::router(state)
        .oneshot(request.body(Body::from(body.unwrap_or_default())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    json!({"status": status, "content_type": content_type, "body": String::from_utf8(bytes.to_vec()).unwrap()})
}

/// A fake llama-server: `/v1/models` from `models`, chat answers popped from `chats`.
async fn llama(models: Value, chats: Vec<Value>) -> (u16, Arc<Mutex<Vec<Value>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let chats = Arc::new(Mutex::new(chats));
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buf = [0u8; 8192];
            let head_end = loop {
                if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break data.len();
                }
                data.extend_from_slice(&buf[..n]);
            };
            let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
            let length: usize = head
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, v)| v.trim().parse().unwrap());
            while data.len() < head_end + length {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
            }
            let mut first = head.lines().next().unwrap().split(' ');
            let method = first.next().unwrap().to_owned();
            let path = first.next().unwrap().to_owned();
            let answer = if method == "GET" {
                record
                    .lock()
                    .unwrap()
                    .push(json!({"method": "GET", "path": path, "body": null}));
                models.clone()
            } else {
                let body = String::from_utf8(data[head_end..head_end + length].to_vec()).unwrap();
                record
                    .lock()
                    .unwrap()
                    .push(json!({"method": "POST", "path": path, "body": body}));
                let mut c = chats.lock().unwrap();
                if c.is_empty() {
                    json!({"status": 500, "body": "no scripted answer"})
                } else {
                    c.remove(0)
                }
            };
            let body = answer["body"].as_str().unwrap().as_bytes().to_vec();
            let status = answer["status"].as_u64().unwrap();
            let kind = if status == 200 {
                "text/event-stream"
            } else {
                "text/plain"
            };
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
            socket.shutdown().await.ok();
        }
    });
    (port, seen)
}

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
}

fn tables(root: &Path) -> Value {
    let c = Connection::open(root.join("model_loader.db")).unwrap();
    let mut out = serde_json::Map::new();
    for (table, cols) in [
        ("bench_run", "quote(id), quote(backend), quote(status), quote(started_at), quote(finished_at), quote(reps), quote(max_tokens), quote(note)"),
        ("bench_variant", "quote(id), quote(run_id), quote(alias), quote(argv_json), quote(load_ms)"),
        ("bench_result", "quote(variant_id), quote(prompt_name), quote(rep), quote(cold), quote(ttft_ms), quote(ttft_answer_ms), quote(truncated), quote(total_ms), quote(prompt_n), quote(prompt_tps), quote(gen_n), quote(gen_tps), quote(draft_n), quote(draft_acc), quote(peak_vram_json), quote(contended), quote(err), quote(response_text)"),
    ] {
        let mut statement = c
            .prepare(&format!("SELECT {cols} FROM {table} ORDER BY id"))
            .unwrap();
        let n = statement.column_count();
        let rows: Vec<Value> = statement
            .query_map([], |r| {
                Ok(Value::Array(
                    (0..n)
                        .map(|i| Value::String(r.get::<_, String>(i).unwrap()))
                        .collect(),
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        out.insert(table.into(), Value::Array(rows));
    }
    Value::Object(out)
}

/// The child's observation queries as the Python oracle records the three calls.
fn as_python_calls(queries: &[String]) -> Value {
    let mut out = Vec::new();
    for q in queries {
        let pairs: Vec<(String, String)> = form_urlencoded::parse(q.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let one = |k: &str| pairs.iter().find(|(n, _)| n == k).unwrap().1.clone();
        let mut aliases: Vec<String> = pairs
            .iter()
            .filter(|(n, _)| n == "aliases")
            .map(|(_, v)| v.clone())
            .collect();
        aliases.sort();
        aliases.dedup();
        out.push(json!({"peak": one("backend")}));
        out.push(json!({"other": one("backend"), "aliases": aliases,
                        "since": one("since").parse::<f64>().unwrap()}));
        if one("cold") == "1" {
            out.push(json!({"load": one("backend"), "alias": one("alias")}));
        }
    }
    Value::Array(out)
}

#[test]
fn fixture_provenance_is_the_pinned_services_source() {
    let oracle = &fixture()["oracle"];
    assert_eq!(oracle["api.py"], API_SHA);
    assert_eq!(oracle["bench.py"], BENCH_SHA);
    assert_eq!(oracle["db.py"], DB_SHA);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_python_differential() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap().clone();
    assert!(cases.len() >= 14);
    let peak = serde_json::to_string(&fixture["peak"])
        .unwrap()
        .replace(',', ", ");
    let load_ms = fixture["load_ms"].as_f64().unwrap();
    for case in cases {
        let id = case["id"].as_str().unwrap().to_owned();
        PERF.store(0, Ordering::SeqCst);
        WALL.store(0, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), &case);
        let (port, seen) = llama(
            case["models"].clone(),
            case["chats"].as_array().unwrap().clone(),
        )
        .await;
        let held: Arc<Mutex<Vec<Task>>> = Arc::new(Mutex::new(Vec::new()));
        let hold = held.clone();
        let queries: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let record = queries.clone();
        let contended = case["contended"].as_bool().unwrap();
        let peak = peak.clone();
        let runner = Runner::new(
            dir.path(),
            Clock {
                perf_counter: perf,
                time: wall,
            },
            Arc::new(move |task| hold.lock().unwrap().push(task)),
            Arc::new(move |query: String| {
                let cold = query.contains("cold=1");
                record.lock().unwrap().push(query);
                let observed = Observed {
                    peak_vram_json: peak.clone(),
                    contended,
                    load_ms: cold.then_some(load_ms),
                };
                Box::pin(async move { Some(observed) })
            }),
            Arc::new(move |backend: String| {
                Box::pin(async move {
                    match backend.as_str() {
                        "127.0.0.1" => Ok(format!("http://127.0.0.1:{port}")),
                        "noport" => Err("no internal port exposed".to_owned()),
                        other => Err(format!("NotFound: No such container: {other}")),
                    }
                })
            }),
        )
        .unwrap();
        let state = state(runner);
        let mut answers = Vec::new();
        for step in case["steps"].as_array().unwrap() {
            let answer = match step[0].as_str().unwrap() {
                "start" => {
                    send(
                        state.clone(),
                        "POST",
                        "/api/v1/benchmark/start",
                        Some(serde_json::to_string(&step[1]).unwrap()),
                    )
                    .await
                }
                "progress" => send(state.clone(), "GET", "/api/v1/benchmark/progress", None).await,
                "cancel" => send(state.clone(), "POST", "/api/v1/benchmark/cancel", None).await,
                _ => {
                    loop {
                        let task = held.lock().unwrap().pop();
                        match task {
                            Some(t) => t.await,
                            None => break,
                        }
                    }
                    Value::Null
                }
            };
            answers.push(answer);
        }
        for (i, (got, want)) in answers
            .iter()
            .zip(case["answers"].as_array().unwrap())
            .enumerate()
        {
            assert_eq!(got, want, "{id}: step {i}");
        }
        assert_eq!(
            answers.len(),
            case["answers"].as_array().unwrap().len(),
            "{id}"
        );
        assert_eq!(
            Value::Array(seen.lock().unwrap().clone()),
            case["llama"],
            "{id}: llama requests"
        );
        // The native runner asks for the cold load time in the same read-only observation call,
        // before the result insert; Python reads it after. Whether it is used shows in
        // `bench_variant.load_ms`, which the store comparison covers, so compare the reads
        // without the load-time entries.
        let no_load = |v: &Value| {
            Value::Array(
                v.as_array()
                    .unwrap()
                    .iter()
                    .filter(|c| c.get("load").is_none())
                    .cloned()
                    .collect(),
            )
        };
        assert_eq!(
            no_load(&as_python_calls(&queries.lock().unwrap())),
            no_load(&case["observed"]),
            "{id}: observations"
        );
        assert_eq!(tables(dir.path()), case["tables"], "{id}: store");
    }
}

/// These tests leave `PERF`/`WALL` to the differential test, which runs alongside.
fn fixed() -> f64 {
    1_700_000_000.0
}

/// A runner over the first fixture case's store whose resolver is `resolve`, with workers held.
fn held_runner(
    root: &Path,
    resolve: noevia_model_manager::bench_runner::Resolver,
) -> (Arc<Runner>, Arc<Mutex<Vec<Task>>>) {
    build(root, &fixture()["cases"][0]);
    let held: Arc<Mutex<Vec<Task>>> = Arc::new(Mutex::new(Vec::new()));
    let hold = held.clone();
    let runner = Runner::new(
        root,
        Clock {
            perf_counter: fixed,
            time: fixed,
        },
        Arc::new(move |task| hold.lock().unwrap().push(task)),
        Arc::new(|_query: String| Box::pin(async { None })),
        resolve,
    )
    .unwrap();
    (runner, held)
}

fn suite_request() -> noevia_model_manager::bench_runner::StartRequest {
    noevia_model_manager::bench_runner::StartRequest {
        backend: "synthetic-backend".into(),
        aliases: vec!["m1".into()],
        prompt_ids: vec![1],
        reps: 1,
        max_tokens: 16,
    }
}

fn run_rows(root: &Path) -> Vec<(String, String)> {
    let c = Connection::open(root.join("model_loader.db")).unwrap();
    let mut s = c
        .prepare("SELECT status, coalesce(note, '') FROM bench_run ORDER BY id")
        .unwrap();
    s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// noevia#1943: a start whose request future is dropped while the backend lookup hangs gives
/// the slot back; the next start is not refused with 409's "already running".
#[tokio::test]
async fn dropped_start_releases_the_slot() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let count = calls.clone();
    let (runner, held) = held_runner(
        dir.path(),
        Arc::new(move |_backend: String| {
            let first = count.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                if first {
                    std::future::pending::<()>().await;
                }
                Err("synthetic: no such backend".to_owned())
            })
        }),
    );
    let dropped = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        runner.start(suite_request()),
    )
    .await;
    assert!(dropped.is_err(), "the never-returning resolver must hang");
    assert_eq!(runner.snapshot().status, "idle");
    assert_eq!(
        runner.start(suite_request()).await,
        Ok(Err("synthetic: no such backend".to_owned()))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(held.lock().unwrap().is_empty());
    assert!(run_rows(dir.path()).is_empty());
}

/// noevia#1948: a cancel while the job is "starting" is not lost when the worker takes over.
#[tokio::test]
async fn cancel_while_starting_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let open = gate.clone();
    let (runner, held) = held_runner(
        dir.path(),
        Arc::new(move |_backend: String| {
            let open = open.clone();
            Box::pin(async move {
                open.notified().await;
                Ok("http://127.0.0.1:9".to_owned())
            })
        }),
    );
    let starting = tokio::spawn({
        let runner = runner.clone();
        async move { runner.start(suite_request()).await }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while runner.snapshot().status != "starting" {
        assert!(std::time::Instant::now() < deadline, "start never claimed");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(runner.cancel());
    gate.notify_one();
    assert_eq!(starting.await.unwrap(), Ok(Ok(())));
    assert_eq!(runner.snapshot().status, "cancelling");
    let task = held.lock().unwrap().pop().unwrap();
    task.await;
    assert_eq!(runner.snapshot().status, "cancelled");
    assert_eq!(run_rows(dir.path())[0].0, "cancelled");
}
