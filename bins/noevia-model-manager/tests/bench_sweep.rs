//! noevia#1318 slice 9b: native llama-bench sweeps and startup recovery against the actual
//! Python runner, through the real Unix sweep transport and its allowlist.
//!
//! `fixtures/bench-sweep-python.v1.json` comes from `tools/gen-bench-sweep-oracle.py`, which runs
//! the real services routes and `bench.py` against a scripted fake Docker client, a fake
//! llama-server and a fixed-step clock. Here the same script is served by a fake Docker daemon on
//! a Unix socket, and `docker::sweep::UnixSweepDocker` talks to it, so every request the native
//! sweep makes passes `allowed_sweep` exactly as it would in production. Required to match:
//! the answers, the llama-server requests, the backend restarts, the containers created, killed
//! and removed, and the `bench_run` / `bench_sweep` rows.
//!
//! Two differences are deliberate and checked as such: a sweep mounts only the backend mounts
//! that hold the model (Python mounts all of them, read-only), and it copies the backend's GPU
//! request instead of asking for every GPU (both are compared as "has a GPU request").
#![cfg(feature = "native-benchmark-runner")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{
    bench_runner::{Clock, Runner, Sweeps, Task},
    docker::sweep::{UnixSweepDocker, LIST_PATH, SWEEP_LABEL},
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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

const TOKEN: &str = "synthetic-model-loader-token-0123456789abcdef";
const API_SHA: &str = "eb8e12f6929baf15d2b1a1998aceff9529fbe48ca8242094ac6b7526c253a91d";
const BENCH_SHA: &str = "2e59f9b057e2686b7f05e5dac04826c8ff37e3c1eb4398bf671faa2d74f3b14b";

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

static WALL: AtomicU64 = AtomicU64::new(0);
#[allow(clippy::cast_precision_loss)]
fn wall() -> f64 {
    1_700_000_000.0 + 1.5 * WALL.fetch_add(1, Ordering::SeqCst) as f64
}
/// The clock of the tests that do not compare with the oracle (the differential owns `WALL`).
fn other_wall() -> f64 {
    1_800_000_000.0
}
fn perf() -> f64 {
    0.0
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/bench-sweep-python.v1.json")).unwrap()
}

fn image_id(backend: &str) -> String {
    match backend {
        "127.0.0.1" => format!("sha256:{}", "ab".repeat(32)),
        "igpu" => format!("sha256:{}", "ef".repeat(32)),
        _ => format!("sha256:{}", "cd".repeat(32)),
    }
}

/// The effective backend names the sweep transport may inspect.
fn names(list: &[&str]) -> noevia_model_manager::docker::sweep::BackendNames {
    let list: Vec<String> = list.iter().map(|s| (*s).to_owned()).collect();
    Arc::new(move || {
        let list = list.clone();
        Box::pin(async move { list })
    })
}

/// The fixture's backends plus `missing`, which is configured but has no container.
const FIXTURE_BACKENDS: &[&str] = &["127.0.0.1", "notag", "missing"];

/// One scripted sweep container while it exists.
#[derive(Clone)]
struct Live {
    statuses: Vec<String>,
    repeat: Option<String>,
    stdout: String,
    stderr: String,
    start_error: Option<String>,
    removes: Vec<String>,
}

struct Daemon {
    backends: Value,
    runs: Vec<Value>,
    listed: Vec<Value>,
    live: std::collections::HashMap<String, Live>,
    log: Vec<Value>,
    /// Every request that reached the daemon, `"METHOD path"`.
    requests: Vec<String>,
    /// Never answer a sweep container's inspect (a stalled daemon, noevia#1944).
    stall: bool,
}

fn answer(status: u16, body: &[u8], kind: &str) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn message(status: u16, text: &str) -> Vec<u8> {
    answer(
        status,
        json!({"message": text}).to_string().as_bytes(),
        "application/json",
    )
}

fn frames(stream: u8, data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut out = vec![stream, 0, 0, 0];
    out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    out.extend_from_slice(data);
    out
}

impl Daemon {
    /// The answer, or `None` to hold the connection open without one.
    fn handle(&mut self, method: &str, path: &str, body: &[u8]) -> Option<Vec<u8>> {
        self.requests.push(format!("{method} {path}"));
        let parts: Vec<&str> = path.split('?').next().unwrap().split('/').collect();
        if self.stall && method == "GET" {
            if let ["", "containers", id, "json"] = parts.as_slice() {
                if self.live.contains_key(*id) {
                    return None;
                }
            }
        }
        Some(self.answer(method, path, body))
    }

    fn answer(&mut self, method: &str, path: &str, body: &[u8]) -> Vec<u8> {
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let parts: Vec<&str> = route.split('/').collect();
        if method == "GET" && path == LIST_PATH {
            let listed: Vec<Value> = self
                .listed
                .iter()
                .map(|c| {
                    let labels = c
                        .get("labels")
                        .cloned()
                        .unwrap_or(json!({SWEEP_LABEL: "1"}));
                    json!({"Id": c["id"], "Labels": labels})
                })
                .collect();
            for c in &self.listed {
                self.live.insert(
                    c["id"].as_str().unwrap().to_owned(),
                    Live {
                        statuses: vec![],
                        repeat: Some("running".into()),
                        stdout: String::new(),
                        stderr: String::new(),
                        start_error: None,
                        removes: c["removes"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|r| r.as_str().unwrap().to_owned())
                            .collect(),
                    },
                );
            }
            return answer(
                200,
                Value::Array(listed).to_string().as_bytes(),
                "application/json",
            );
        }
        match (method, parts.as_slice()) {
            ("GET", ["", "containers", name, "json"]) if self.backends.get(*name).is_some() => {
                let spec = &self.backends[*name];
                let mounts: Vec<Value> = spec["mounts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| json!({"Source": m[0], "Destination": m[1], "Type": "bind"}))
                    .collect();
                let gpu = if spec["gpu"].as_bool().unwrap() {
                    json!([{"Driver": "nvidia", "Count": -1, "DeviceIDs": null,
                            "Capabilities": [["gpu"]], "Options": {}}])
                } else {
                    Value::Null
                };
                let body = json!({"Image": image_id(name), "Mounts": mounts,
                    "HostConfig": {"DeviceRequests": gpu,
                        "Devices": spec.get("devices").cloned().unwrap_or(Value::Null),
                        "GroupAdd": spec.get("group_add").cloned().unwrap_or(Value::Null)},
                    "Config": {"WorkingDir": spec["workdir"]}});
                answer(200, body.to_string().as_bytes(), "application/json")
            }
            ("GET", ["", "images", id, "json"]) => {
                let tags = ["127.0.0.1", "notag", "igpu"]
                    .iter()
                    .find(|b| image_id(b) == *id)
                    .map_or(Value::Null, |b| self.backends[*b]["tags"].clone());
                answer(
                    200,
                    json!({"RepoTags": tags}).to_string().as_bytes(),
                    "application/json",
                )
            }
            ("POST", ["", "containers", "create"]) if query.is_empty() => {
                let body: Value = serde_json::from_slice(body).unwrap();
                let host = &body["HostConfig"];
                let mut binds: Vec<Value> = host["Binds"].as_array().unwrap().clone();
                binds.sort_by_key(ToString::to_string);
                let entry = json!({
                    "image": body["Image"], "cmd": body["Cmd"],
                    "entrypoint": body["Entrypoint"][0], "binds": binds,
                    "network": host["NetworkMode"], "labels": body["Labels"],
                    "gpu": host["DeviceRequests"].as_array().is_some_and(|d| !d.is_empty()),
                    "keys": body.as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
                    "host_keys": host.as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
                    "device_requests": host["DeviceRequests"],
                    "devices": host["Devices"],
                    "group_add": host["GroupAdd"],
                });
                self.log.push(json!(["create", entry]));
                let spec = self.runs.remove(0);
                if let Some(e) = spec["create_error"].as_str() {
                    return message(500, e);
                }
                let id = spec["id"].as_str().unwrap().to_owned();
                let strings = |k: &str| -> Vec<String> {
                    spec[k]
                        .as_array()
                        .map(|a| a.iter().map(|s| s.as_str().unwrap().to_owned()).collect())
                        .unwrap_or_default()
                };
                self.live.insert(
                    id.clone(),
                    Live {
                        statuses: strings("statuses"),
                        repeat: spec["repeat"].as_str().map(str::to_owned),
                        stdout: spec["stdout"].as_str().unwrap_or("").to_owned(),
                        stderr: spec["stderr"].as_str().unwrap_or("").to_owned(),
                        start_error: spec["start_error"].as_str().map(str::to_owned),
                        removes: vec![],
                    },
                );
                answer(
                    201,
                    json!({"Id": id, "Warnings": []}).to_string().as_bytes(),
                    "application/json",
                )
            }
            (_, ["", "containers", id, ..]) if !self.live.contains_key(*id) => {
                // A backend that does not exist is an ordinary inspect miss.
                if (method, parts.len()) != ("GET", 4) {
                    self.log.push(json!(["unexpected", method, path]));
                }
                message(404, &format!("No such container: {id}"))
            }
            ("POST", ["", "containers", id, "start"]) => match self.live[*id].start_error.clone() {
                Some(e) => message(500, &e),
                None => answer(204, b"", "text/plain"),
            },
            ("GET", ["", "containers", id, "json"]) => {
                let live = self.live.get_mut(*id).unwrap();
                let status = if live.statuses.is_empty() {
                    live.repeat.clone().unwrap_or_else(|| "exited".into())
                } else {
                    live.statuses.remove(0)
                };
                answer(
                    200,
                    json!({"Id": id, "State": {"Status": status}})
                        .to_string()
                        .as_bytes(),
                    "application/json",
                )
            }
            ("POST", ["", "containers", id, "kill"]) => {
                self.log.push(json!(["kill", id]));
                answer(204, b"", "text/plain")
            }
            ("GET", ["", "containers", id, "logs"]) => {
                let live = &self.live[*id];
                let raw = if query.starts_with("stdout=true") {
                    frames(1, live.stdout.as_bytes())
                } else {
                    frames(2, live.stderr.as_bytes())
                };
                answer(200, &raw, "application/vnd.docker.raw-stream")
            }
            ("DELETE", ["", "containers", id]) => {
                self.log.push(json!(["remove", id]));
                let live = self.live.get_mut(*id).unwrap();
                let what = if live.removes.is_empty() {
                    "ok".to_owned()
                } else {
                    live.removes.remove(0)
                };
                if what == "notfound" {
                    return message(404, &format!("No such container: {id}"));
                }
                if let Some(e) = what.strip_prefix("error:") {
                    return message(409, e);
                }
                answer(204, b"", "text/plain")
            }
            _ => {
                self.log.push(json!(["unexpected", method, path]));
                message(500, "unexpected request")
            }
        }
    }
}

/// A fake Docker daemon on a Unix socket, scripted like the oracle's fake client.
fn daemon(dir: &Path, d: Daemon) -> (std::path::PathBuf, Arc<Mutex<Daemon>>) {
    let socket = dir.join("docker.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let shared = Arc::new(Mutex::new(d));
    let state = shared.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let state = state.clone();
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buf = [0u8; 8192];
                let head_end = loop {
                    if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    let n = s.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
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
                    let n = s.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                }
                let mut first = head.lines().next().unwrap().split(' ');
                let method = first.next().unwrap().to_owned();
                let path = first.next().unwrap().to_owned();
                let reply = state.lock().unwrap().handle(
                    &method,
                    &path,
                    &data[head_end..head_end + length],
                );
                let Some(reply) = reply else {
                    // Hold the connection without answering until the client gives up.
                    let _ = s.read(&mut buf).await;
                    return;
                };
                s.write_all(&reply).await.unwrap();
                s.shutdown().await.ok();
            });
        }
    });
    (socket, shared)
}

/// A fake llama-server answering `/v1/models` with `models`, recording each request.
async fn llama(models: Value) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
            }
            let head = String::from_utf8_lossy(&data).into_owned();
            let mut first = head.lines().next().unwrap_or("GET /").split(' ');
            let method = first.next().unwrap().to_owned();
            let path = first.next().unwrap_or("/").to_owned();
            record
                .lock()
                .unwrap()
                .push(json!({"method": method, "path": path}));
            let body = models["body"].as_str().unwrap().as_bytes().to_vec();
            let status = models["status"].as_u64().unwrap();
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
            socket.shutdown().await.ok();
        }
    });
    (port, seen)
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

fn build(root: &Path, schema: &Value) {
    let c = Connection::open(root.join("model_loader.db")).unwrap();
    let _: String = c
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    for sql in schema.as_array().unwrap() {
        c.execute_batch(sql.as_str().unwrap()).unwrap();
    }
    c.execute_batch(
        "INSERT INTO prompts(id, name, body, created_at) VALUES(1, 'alpha', 'Say hi.', 1.0)",
    )
    .unwrap();
}

fn tables(root: &Path) -> Value {
    let c = Connection::open(root.join("model_loader.db")).unwrap();
    let mut out = serde_json::Map::new();
    for (table, cols) in [
        ("bench_run", "quote(id), quote(backend), quote(status), quote(started_at), quote(finished_at), quote(reps), quote(max_tokens), quote(note)"),
        ("bench_sweep", "quote(run_id), quote(alias), quote(model_type), quote(test), quote(n_prompt), quote(n_gen), quote(n_depth), quote(avg_ts), quote(stddev_ts), quote(n_gpu_layers), quote(n_cpu_moe), quote(n_ubatch), quote(type_k), quote(type_v), quote(flash_attn), quote(split_mode), quote(raw_json)"),
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

#[test]
fn fixture_provenance_is_the_pinned_services_source() {
    let oracle = &fixture()["oracle"];
    assert_eq!(oracle["api.py"], API_SHA);
    assert_eq!(oracle["bench.py"], BENCH_SHA);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_python_differential() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap().clone();
    assert!(cases.len() >= 10);
    for case in cases {
        let id = case["id"].as_str().unwrap().to_owned();
        WALL.store(0, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), &fixture["schema"]);
        std::fs::write(
            dir.path().join("models.ini"),
            fixture["ini"].as_str().unwrap(),
        )
        .unwrap();
        let (port, seen) = llama(case["models"].clone()).await;
        let (socket, fake) = daemon(
            dir.path(),
            Daemon {
                backends: fixture["backends"].clone(),
                runs: case["runs"].as_array().unwrap().clone(),
                listed: case["listed"].as_array().unwrap().clone(),
                live: std::collections::HashMap::new(),
                log: Vec::new(),
                requests: Vec::new(),
                stall: false,
            },
        );
        let held: Arc<Mutex<Vec<Task>>> = Arc::new(Mutex::new(Vec::new()));
        let hold = held.clone();
        let restarts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let record = restarts.clone();
        let sweeps = Sweeps {
            docker: Arc::new(UnixSweepDocker::new(
                socket,
                "/models",
                names(FIXTURE_BACKENDS),
            )),
            ini: dir.path().join("models.ini"),
            restart: Arc::new(move |name: String| {
                record.lock().unwrap().push(name);
                Box::pin(async { Ok(()) })
            }),
            sleep: Arc::new(|_| Box::pin(async {})),
        };
        let runner = Runner::with_sweeps(
            dir.path(),
            Clock {
                perf_counter: perf,
                time: wall,
            },
            Arc::new(move |task| hold.lock().unwrap().push(task)),
            Arc::new(|_| Box::pin(async { None })),
            Arc::new(move |backend: String| {
                Box::pin(async move {
                    match backend.as_str() {
                        "127.0.0.1" => Ok(format!("http://127.0.0.1:{port}")),
                        // Python addresses it by name, which does not resolve here either.
                        "notag" => Ok("http://127.0.0.1:1".to_owned()),
                        other => Err(format!("NotFound: No such container: {other}")),
                    }
                })
            }),
            Some(sweeps),
        )
        .unwrap();
        let state = state(runner.clone());
        let mut answers = Vec::new();
        for step in case["steps"].as_array().unwrap() {
            let body = || Some(serde_json::to_string(&step[1]).unwrap());
            let answer = match step[0].as_str().unwrap() {
                "sweep" => send(state.clone(), "POST", "/api/v1/benchmark/sweep", body()).await,
                "start" => send(state.clone(), "POST", "/api/v1/benchmark/start", body()).await,
                "progress" => send(state.clone(), "GET", "/api/v1/benchmark/progress", None).await,
                "cancel" => send(state.clone(), "POST", "/api/v1/benchmark/cancel", None).await,
                "reap" => {
                    let (reaped, stuck) = runner.reap_orphans().await;
                    json!({"reaped": reaped, "stuck": stuck})
                }
                _ => {
                    loop {
                        let task = {
                            let mut h = held.lock().unwrap();
                            if h.is_empty() {
                                None
                            } else {
                                Some(h.remove(0))
                            }
                        };
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
        let want = case["answers"].as_array().unwrap();
        for (i, (got, want)) in answers.iter().zip(want).enumerate() {
            assert_eq!(got, want, "{id}: step {i}");
        }
        assert_eq!(answers.len(), want.len(), "{id}");
        assert_eq!(
            Value::Array(seen.lock().unwrap().clone()),
            case["llama"],
            "{id}: llama requests"
        );
        assert_eq!(
            Value::Array(restarts.lock().unwrap().iter().map(|r| json!(r)).collect()),
            case["restarts"],
            "{id}: restarts"
        );
        let log = fake.lock().unwrap().log.clone();
        let python = case["docker"].as_array().unwrap();
        assert_eq!(log.len(), python.len(), "{id}: docker calls {log:?}");
        for (got, want) in log.iter().zip(python) {
            assert_eq!(got[0], want[0], "{id}: docker call");
            if got[0] != "create" {
                assert_eq!(got, want, "{id}: docker call");
                continue;
            }
            let (g, w) = (&got[1], &want[1]);
            for key in ["cmd", "entrypoint", "network", "labels", "gpu"] {
                assert_eq!(g[key], w[key], "{id}: create {key}");
            }
            // Python runs the backend's first tag; the native sweep runs the backend
            // container's own image id, so a re-pulled tag cannot change it (noevia#1948).
            assert_eq!(w["image"], json!("ghcr.io/example/llama:full"), "{id}");
            assert_eq!(g["image"], json!(image_id("127.0.0.1")), "{id}: image id");
            // Only the backend mounts that hold the model, all read-only; Python mounts every
            // backend mount.
            let binds = g["binds"].as_array().unwrap();
            assert!(
                binds
                    .iter()
                    .all(|b| w["binds"].as_array().unwrap().contains(b)),
                "{id}"
            );
            assert_eq!(
                binds,
                &vec![json!("/mnt/user/models:/models:ro")],
                "{id}: binds"
            );
            assert_eq!(
                g["keys"],
                json!(["Cmd", "Entrypoint", "HostConfig", "Image", "Labels"])
            );
            assert_eq!(
                g["host_keys"],
                json!([
                    "Binds",
                    "DeviceRequests",
                    "Devices",
                    "GroupAdd",
                    "NetworkMode"
                ])
            );
            // The fixture backend has no device nodes or extra groups: none are added.
            assert_eq!(g["devices"], json!([]), "{id}: devices");
            assert_eq!(g["group_add"], json!([]), "{id}: groups");
            assert_eq!(
                g["device_requests"],
                json!([{"Driver": "nvidia", "Count": -1, "DeviceIDs": [],
                        "Capabilities": [["gpu"]], "Options": {}}]),
                "{id}: the backend's GPU request, copied"
            );
        }
        assert_eq!(tables(dir.path()), case["tables"], "{id}: store");
    }
}

/// Every verb against a container this process neither created nor listed by label is refused
/// before anything reaches Docker.
#[tokio::test]
async fn non_sweep_containers_are_refused_for_every_verb() {
    use noevia_model_manager::bench_runner::SweepDocker;
    let dir = tempfile::tempdir().unwrap();
    let (socket, fake) = daemon(
        dir.path(),
        Daemon {
            backends: json!({}),
            runs: vec![],
            listed: vec![],
            live: std::collections::HashMap::new(),
            log: Vec::new(),
            requests: Vec::new(),
            stall: false,
        },
    );
    let docker = UnixSweepDocker::new(socket, "/models", names(FIXTURE_BACKENDS));
    let stranger = "f".repeat(64);
    for id in [stranger.as_str(), "noevia-llama", "ffff", "../x"] {
        assert_eq!(
            docker.status(id).await.unwrap(),
            Err("Docker operation refused".into())
        );
        assert_eq!(
            docker.kill(id).await.unwrap(),
            Err("Docker operation refused".into())
        );
        assert_eq!(
            docker.logs(id, true).await.unwrap(),
            Err("Docker operation refused".into())
        );
        assert_eq!(
            docker.logs(id, false).await.unwrap(),
            Err("Docker operation refused".into())
        );
        assert_eq!(
            docker.remove(id).await.unwrap(),
            Err("Docker operation refused".into())
        );
    }
    assert!(fake.lock().unwrap().log.is_empty());
}

/// A runner on the fake daemon with the given restart answer; `on_sleep` runs on every sleep.
async fn harness(
    dir: &Path,
    runs: Vec<Value>,
    listed: Vec<Value>,
    models: Value,
    restart: Result<(), String>,
    on_sleep: Arc<dyn Fn() + Send + Sync>,
    stall: bool,
) -> (
    Arc<Runner>,
    Arc<Mutex<Daemon>>,
    Arc<Mutex<Vec<Task>>>,
    Arc<Mutex<Vec<Value>>>,
) {
    let fixture = fixture();
    build(dir, &fixture["schema"]);
    std::fs::write(dir.join("models.ini"), fixture["ini"].as_str().unwrap()).unwrap();
    let (port, seen) = llama(models).await;
    let (socket, fake) = daemon(
        dir,
        Daemon {
            backends: fixture["backends"].clone(),
            runs,
            listed,
            live: std::collections::HashMap::new(),
            log: Vec::new(),
            requests: Vec::new(),
            stall,
        },
    );
    let held: Arc<Mutex<Vec<Task>>> = Arc::new(Mutex::new(Vec::new()));
    let hold = held.clone();
    let runner = Runner::with_sweeps(
        dir,
        Clock {
            perf_counter: perf,
            time: other_wall,
        },
        Arc::new(move |task| hold.lock().unwrap().push(task)),
        Arc::new(|_| Box::pin(async { None })),
        Arc::new(move |_| Box::pin(async move { Ok(format!("http://127.0.0.1:{port}")) })),
        Some(Sweeps {
            docker: Arc::new(
                UnixSweepDocker::new(socket, "/models", names(FIXTURE_BACKENDS))
                    .with_timeout(std::time::Duration::from_millis(500)),
            ),
            ini: dir.join("models.ini"),
            restart: Arc::new(move |_| {
                let r = restart.clone();
                Box::pin(async move { r })
            }),
            sleep: Arc::new(move |_| {
                on_sleep();
                Box::pin(async {})
            }),
        }),
    )
    .unwrap();
    (runner, fake, held, seen)
}

async fn drain(held: &Arc<Mutex<Vec<Task>>>) {
    loop {
        let task = {
            let mut h = held.lock().unwrap();
            if h.is_empty() {
                None
            } else {
                Some(h.remove(0))
            }
        };
        match task {
            Some(t) => t.await,
            None => break,
        }
    }
}

fn lines(runner: &Runner) -> Vec<String> {
    runner.snapshot().lines
}

#[tokio::test]
async fn a_cancel_while_llama_bench_runs_kills_and_removes_its_container() {
    let dir = tempfile::tempdir().unwrap();
    let slot: Arc<std::sync::OnceLock<Arc<Runner>>> = Arc::new(std::sync::OnceLock::new());
    let cancel = slot.clone();
    let id = "7".repeat(64);
    let (runner, fake, held, _) = harness(
        dir.path(),
        vec![json!({"id": id, "statuses": [], "repeat": "running", "stdout": "", "stderr": ""})],
        vec![],
        json!({"status": 200, "body": "{\"data\": []}"}),
        Ok(()),
        Arc::new(move || {
            if let Some(r) = cancel.get() {
                r.cancel();
            }
        }),
        false,
    )
    .await;
    slot.set(runner.clone()).ok();
    let request = noevia_model_manager::bench_runner::sweep_request(
        &model_files::json::parse(r#"{"backend": "127.0.0.1", "aliases": ["m1", "m2"]}"#).unwrap(),
    )
    .unwrap();
    runner.start_sweep(request).await.unwrap().unwrap();
    drain(&held).await;
    let job = runner.snapshot();
    assert_eq!(job.status, "cancelled");
    assert_eq!(job.done, 0);
    assert!(lines(&runner).contains(&"m1: cancelled".to_owned()));
    let log = fake.lock().unwrap().log.clone();
    let verbs: Vec<&str> = log.iter().map(|e| e[0].as_str().unwrap()).collect();
    assert_eq!(verbs, ["create", "kill", "remove"]);
    assert_eq!(log[1][1], json!(id));
}

#[tokio::test]
async fn a_refused_backend_restart_is_reported_not_polled_past() {
    let dir = tempfile::tempdir().unwrap();
    let (runner, _, held, seen) = harness(
        dir.path(),
        vec![json!({"id": "8".repeat(64), "statuses": ["exited"], "stdout": "[]", "stderr": ""})],
        vec![],
        json!({"status": 200, "body": "{\"data\": [{\"id\": \"m1\", \"status\": {\"value\": \"loaded\"}}]}"}),
        Err("container not in configured list".into()),
        Arc::new(|| {}),
        false,
    )
    .await;
    let request = noevia_model_manager::bench_runner::sweep_request(
        &model_files::json::parse(r#"{"backend": "127.0.0.1", "aliases": ["m1"]}"#).unwrap(),
    )
    .unwrap();
    runner.start_sweep(request).await.unwrap().unwrap();
    drain(&held).await;
    let lines = lines(&runner);
    assert_eq!(
        lines[..2],
        [
            "freeing GPU: restarting the backend so the sweep has the cards to itself".to_owned(),
            "could not restart 127.0.0.1: container not in configured list".to_owned()
        ]
    );
    assert!(!lines.iter().any(|l| l.contains("GPUs clear")));
    // Only the loaded-models read; no polling after a refused restart.
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_leftover_that_never_stops_is_given_up_on_after_the_bound() {
    use noevia_model_manager::bench_runner::ORPHAN_ATTEMPTS;
    let dir = tempfile::tempdir().unwrap();
    let removes = vec![json!("error:conflict"); ORPHAN_ATTEMPTS + 5];
    let (runner, fake, held, _) = harness(
        dir.path(),
        vec![],
        vec![json!({"id": "9".repeat(64), "removes": removes})],
        json!({"status": 200, "body": "{\"data\": []}"}),
        Ok(()),
        Arc::new(|| {}),
        false,
    )
    .await;
    assert_eq!(runner.reap_orphans().await, (0, 1));
    assert!(runner.snapshot().active());
    drain(&held).await;
    let job = runner.snapshot();
    assert_eq!(job.status, "error");
    assert!(job.error.contains("could not be stopped"));
    assert_eq!(fake.lock().unwrap().log.len(), ORPHAN_ATTEMPTS + 1);
}

fn empty_daemon(backends: Value, runs: Vec<Value>) -> Daemon {
    Daemon {
        backends,
        runs,
        listed: vec![],
        live: std::collections::HashMap::new(),
        log: Vec::new(),
        requests: Vec::new(),
        stall: false,
    }
}

fn deletes(fake: &Arc<Mutex<Daemon>>) -> Vec<String> {
    fake.lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.starts_with("DELETE "))
        .cloned()
        .collect()
}

fn sweep_of(json: &str) -> noevia_model_manager::bench_runner::SweepRequest {
    noevia_model_manager::bench_runner::sweep_request(&model_files::json::parse(json).unwrap())
        .unwrap()
}

/// noevia#1945: a sweep backend that is not an effective backend, by name or by an all-hex
/// string Docker would take as an id prefix, is refused before anything reaches Docker.
#[tokio::test]
async fn non_backend_names_are_refused_before_any_docker_call() {
    use noevia_model_manager::bench_runner::SweepDocker;
    let dir = tempfile::tempdir().unwrap();
    let backends = json!({"other-app": {"gpu": true, "mounts": [["/", "/models"]],
        "tags": ["ghcr.io/example/other:1"], "workdir": "/"}});
    let (socket, fake) = daemon(dir.path(), empty_daemon(backends, vec![]));
    // "abab12" names a configured backend, but an all-hex name is still refused: Docker would
    // resolve it as an id prefix.
    let docker = UnixSweepDocker::new(socket, "/models", names(&["127.0.0.1", "abab12"]));
    for name in [
        "other-app",
        "abab12",
        "ABAB",
        "0123456789abcdef",
        "127.0.0.2",
    ] {
        assert_eq!(
            docker.runtime(name).await.unwrap(),
            Err("container not in configured list".into()),
            "{name}"
        );
        assert_eq!(
            docker.endpoint(name).await,
            Err("container not in configured list".into()),
            "{name}"
        );
    }
    assert!(fake.lock().unwrap().requests.is_empty());
    // An effective backend is inspected as before.
    let _ = docker.runtime("127.0.0.1").await;
    assert_eq!(
        fake.lock().unwrap().requests,
        ["GET /containers/127.0.0.1/json"]
    );
}

/// noevia#1945 through the route: the sweep reports the refusal and creates nothing.
#[tokio::test]
async fn a_sweep_of_a_non_backend_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (runner, fake, held, _) = harness(
        dir.path(),
        vec![],
        vec![],
        json!({"status": 200, "body": "{\"data\": []}"}),
        Ok(()),
        Arc::new(|| {}),
        false,
    )
    .await;
    runner
        .start_sweep(sweep_of(
            r#"{"backend": "not-a-backend", "aliases": ["m1"]}"#,
        ))
        .await
        .unwrap()
        .unwrap();
    drain(&held).await;
    assert!(lines(&runner).contains(&"m1: container not in configured list".to_owned()));
    assert!(fake.lock().unwrap().requests.is_empty());
}

/// noevia#1946: the list is filtered by label again here; an entry without the label, with
/// another value, with no labels or with a short id is never acted on.
#[tokio::test]
async fn listed_containers_without_the_exact_label_are_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let good = "1".repeat(64);
    let (runner, fake, held, _) = harness(
        dir.path(),
        vec![],
        vec![
            json!({"id": "2".repeat(64), "labels": {}, "removes": []}),
            json!({"id": "3".repeat(64), "labels": {SWEEP_LABEL: "true"}, "removes": []}),
            json!({"id": "4".repeat(12), "removes": []}),
            json!({"id": "5".repeat(64), "labels": null, "removes": []}),
            json!({"id": "6".repeat(64), "labels": {"noevia.model-manager.bench2": "1"}, "removes": []}),
            json!({"id": good, "removes": []}),
        ],
        json!({"status": 200, "body": "{\"data\": []}"}),
        Ok(()),
        Arc::new(|| {}),
        false,
    )
    .await;
    assert_eq!(runner.reap_orphans().await, (1, 0));
    drain(&held).await;
    assert_eq!(
        deletes(&fake),
        [format!("DELETE /containers/{good}?force=true")]
    );
    assert_eq!(runner.snapshot().status, "idle");
}

/// noevia#1946: more than 64 listed containers fails closed: nothing is removed.
#[tokio::test]
async fn a_list_past_the_cap_removes_nothing() {
    for (n, reaped) in [(65, 0), (64, 64)] {
        let dir = tempfile::tempdir().unwrap();
        let listed: Vec<Value> = (0..n)
            .map(|i| json!({"id": format!("{i:064x}"), "removes": []}))
            .collect();
        let (runner, fake, held, _) = harness(
            dir.path(),
            vec![],
            listed,
            json!({"status": 200, "body": "{\"data\": []}"}),
            Ok(()),
            Arc::new(|| {}),
            false,
        )
        .await;
        assert_eq!(runner.reap_orphans().await, (reaped, 0), "{n}");
        drain(&held).await;
        assert_eq!(deletes(&fake).len(), reaped, "{n}");
    }
}

/// noevia#1944: a daemon that stops answering cannot pin the job. Each call is bounded, a
/// cancel that arrives meanwhile ends the job as cancelled, and without one it ends in error;
/// either way the container is removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_daemon_cannot_pin_the_job() {
    for cancel in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let id = "a".repeat(64);
        let (runner, fake, held, _) = harness(
            dir.path(),
            vec![
                json!({"id": id, "statuses": [], "repeat": "running", "stdout": "", "stderr": ""}),
            ],
            vec![],
            json!({"status": 200, "body": "{\"data\": []}"}),
            Ok(()),
            Arc::new(|| {}),
            true,
        )
        .await;
        runner
            .start_sweep(sweep_of(r#"{"backend": "127.0.0.1", "aliases": ["m1"]}"#))
            .await
            .unwrap()
            .unwrap();
        if cancel {
            let (runner, fake) = (runner.clone(), fake.clone());
            tokio::spawn(async move {
                let stalled = format!("GET /containers/{}/json", "a".repeat(64));
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while !fake.lock().unwrap().requests.contains(&stalled) {
                    assert!(std::time::Instant::now() < deadline);
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                runner.cancel();
            });
        }
        let started = std::time::Instant::now();
        tokio::time::timeout(std::time::Duration::from_secs(20), drain(&held))
            .await
            .expect("the sweep must finish while Docker is stalled");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let job = runner.snapshot();
        if cancel {
            assert_eq!(job.status, "cancelled", "{:?}", job.lines);
            assert!(job.lines.contains(&"m1: cancelled".to_owned()));
        } else {
            assert_eq!(job.status, "error", "{:?}", job.lines);
            assert!(job.error.starts_with("ReadTimeout:"), "{}", job.error);
        }
        assert_eq!(
            deletes(&fake),
            [format!("DELETE /containers/{id}?force=true")]
        );
    }
}

/// noevia#1947: an iGPU backend's device nodes and groups are copied exactly, never more.
#[tokio::test]
async fn an_igpu_backends_devices_and_groups_are_copied_exactly() {
    use noevia_model_manager::bench_runner::SweepDocker;
    use noevia_model_manager::docker::sweep::{create_body, DeviceMapping};
    let dir = tempfile::tempdir().unwrap();
    let devices = json!([
        {"PathOnHost": "/dev/dri/renderD128", "PathInContainer": "/dev/dri/renderD128", "CgroupPermissions": "rwm"},
        {"PathOnHost": "/dev/dri/card0", "PathInContainer": "/dev/dri/card0", "CgroupPermissions": "rwm"}
    ]);
    let backends = json!({
        "igpu": {"gpu": false, "mounts": [["/mnt/user/models", "/models"]],
                 "tags": ["ghcr.io/example/llama:vulkan"], "workdir": "/",
                 "devices": devices, "group_add": ["44", "105"]},
        "odd": {"gpu": false, "mounts": [], "tags": ["ghcr.io/example/llama:vulkan"],
                "workdir": "/", "devices": [{"PathOnHost": "/var/run/docker.sock",
                "PathInContainer": "/dev/x", "CgroupPermissions": "rwm"}]}
    });
    let id = "b".repeat(64);
    let (socket, fake) = daemon(
        dir.path(),
        empty_daemon(
            backends,
            vec![json!({"id": id, "statuses": ["exited"], "stdout": "[]", "stderr": ""})],
        ),
    );
    let docker = UnixSweepDocker::new(socket, "/models", names(&["igpu", "odd"]));
    let runtime = docker.runtime("igpu").await.unwrap().unwrap();
    assert_eq!(runtime.image, image_id("igpu"));
    assert_eq!(runtime.devices.len(), 2);
    assert_eq!(runtime.group_add, ["44", "105"]);
    assert!(runtime.device_requests.is_empty());
    // Anything beyond the backend's own is refused before Docker sees a create.
    let mut more = create_body(&runtime, "/models/x.gguf", vec!["bench".into()]);
    more.host_config.devices.push(DeviceMapping {
        path_on_host: "/dev/kfd".into(),
        path_in_container: "/dev/kfd".into(),
        cgroup_permissions: "rwm".into(),
    });
    assert!(docker.run(&more, &runtime).await.unwrap().is_err());
    let mut groups = create_body(&runtime, "/models/x.gguf", vec!["bench".into()]);
    groups.host_config.group_add.push("0".into());
    assert!(docker.run(&groups, &runtime).await.unwrap().is_err());
    assert!(fake.lock().unwrap().log.is_empty());
    let body = create_body(&runtime, "/models/x.gguf", vec!["bench".into()]);
    assert_eq!(docker.run(&body, &runtime).await.unwrap(), Ok(id.clone()));
    let log = fake.lock().unwrap().log.clone();
    assert_eq!(log[0][0], "create");
    assert_eq!(log[0][1]["devices"], devices);
    assert_eq!(log[0][1]["group_add"], json!(["44", "105"]));
    assert_eq!(log[0][1]["device_requests"], json!([]));
    assert_eq!(log[0][1]["image"], json!(image_id("igpu")));
    // A backend "device" that is not a device node is never passed on.
    assert_eq!(
        docker.runtime("odd").await.unwrap(),
        Err("backend devices are not device nodes".into())
    );
}

fn fifo(path: &Path) {
    std::fs::remove_file(path).unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

/// Lets the blocked models.ini read finish, then puts a regular file back.
fn release(path: &Path) {
    let text = fixture()["ini"].as_str().unwrap().to_owned();
    std::fs::write(path, &text).unwrap();
    std::fs::remove_file(path).unwrap();
    std::fs::write(path, text).unwrap();
}

async fn until_starting(runner: &Runner) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while runner.snapshot().status != "starting" {
        assert!(std::time::Instant::now() < deadline, "never claimed");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// noevia#1943: a sweep start whose request is dropped while it reads models.ini gives the
/// slot back; the next start is accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_sweep_start_releases_the_slot() {
    let dir = tempfile::tempdir().unwrap();
    let (runner, fake, held, _) = harness(
        dir.path(),
        vec![],
        vec![],
        json!({"status": 200, "body": "{\"data\": []}"}),
        Ok(()),
        Arc::new(|| {}),
        false,
    )
    .await;
    let ini = dir.path().join("models.ini");
    fifo(&ini);
    let start = runner.start_sweep(sweep_of(r#"{"backend": "127.0.0.1", "aliases": ["m1"]}"#));
    let dropped = tokio::time::timeout(std::time::Duration::from_millis(100), start).await;
    assert!(
        dropped.is_err(),
        "the models.ini read must still be blocked"
    );
    assert_eq!(runner.snapshot().status, "idle");
    let path = ini.clone();
    tokio::task::spawn_blocking(move || release(&path))
        .await
        .unwrap();
    assert_eq!(
        runner
            .start_sweep(sweep_of(r#"{"backend": "127.0.0.1", "aliases": ["m1"]}"#))
            .await,
        Ok(Ok(()))
    );
    assert_eq!(held.lock().unwrap().len(), 1);
    assert!(fake.lock().unwrap().requests.is_empty());
}

/// noevia#1948: a cancel while a sweep is "starting" is kept: no container is created.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_while_a_sweep_starts_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let (runner, fake, held, _) = harness(
        dir.path(),
        vec![],
        vec![],
        json!({"status": 200, "body": "{\"data\": []}"}),
        Ok(()),
        Arc::new(|| {}),
        false,
    )
    .await;
    let ini = dir.path().join("models.ini");
    fifo(&ini);
    let starting = tokio::spawn({
        let runner = runner.clone();
        async move {
            runner
                .start_sweep(sweep_of(r#"{"backend": "127.0.0.1", "aliases": ["m1"]}"#))
                .await
        }
    });
    until_starting(&runner).await;
    assert!(runner.cancel());
    let path = ini.clone();
    tokio::task::spawn_blocking(move || release(&path))
        .await
        .unwrap();
    assert_eq!(starting.await.unwrap(), Ok(Ok(())));
    assert_eq!(runner.snapshot().status, "cancelling");
    drain(&held).await;
    assert_eq!(runner.snapshot().status, "cancelled");
    assert!(fake.lock().unwrap().log.is_empty());
}
