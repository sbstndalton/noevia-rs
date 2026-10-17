//! noevia#1318 slice 10: the native download job table against the actual Python one.
//!
//! `fixtures/downloads-python.v1.json` comes from `tools/gen-downloads-oracle.py`, which runs the
//! real services app: its planning request (the start routes run without queueing), its
//! `DownloadManager` transfers and `api._job`, against a scripted fake file server (a "hub"
//! origin that may get the token, and a "cdn" origin). This test serves the same script from a
//! fake file server here, answers the parent's planning requests from a fake child with the
//! recorded plans, drives the native routes through the same steps, and requires the same
//! answers, job lists, files left in the models folder, `download_history` rows and requests.
//! Speeds, ETAs and timestamps are compared as "set or not"; their exact rendering is checked by
//! the library tests against the same fixture.
#![cfg(feature = "native-downloads")]
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
    downloads::{pyfmt, Config, Downloads, TokenOrigin},
    Container, DockerRead, Redactor, StateData,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

const TOKEN: &str = "synthetic-model-loader-token-0123456789abcdef";
/// sha256 of the services files the fixture was generated from (noevia-services bf552c1).
const DOWNLOADER_SHA: &str = "496545fa337a647af7865d56e06714827519cbbc5f2a034ee2d7302af9e47d8a";
const MAIN_SHA: &str = "eddf36c21981c7d4f76db447a133a4782b13f763caa8dcf579c6915fac1f9ce2";
const API_SHA: &str = "eb8e12f6929baf15d2b1a1998aceff9529fbe48ca8242094ac6b7526c253a91d";

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

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/downloads-python.v1.json")).unwrap()
}

fn wall() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn content(seed: u64, n: usize) -> Vec<u8> {
    (0..n as u64)
        .map(|i| u8::try_from((seed + i * 7) % 251).unwrap())
        .collect()
}

/// `json.dumps(v)` (Python's default separators) for the ASCII rows this test sorts by.
fn py_dumps(v: &Value) -> String {
    match v {
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(py_dumps).collect::<Vec<_>>().join(", ")
        ),
        other => other.to_string(),
    }
}

// ---- the fake file server (the generator's `handle`, rule for rule) -------------------------

struct Fake {
    files: HashMap<(String, String), Value>,
    log: Mutex<Vec<Value>>,
    release: tokio::sync::watch::Sender<bool>,
    ports: Mutex<HashMap<String, u16>>,
}

impl Fake {
    fn host(&self, server: &str) -> String {
        format!("127.0.0.1:{}", self.ports.lock().unwrap()[server])
    }
}

fn head_text(status: u16, headers: &[(String, String)]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} X\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    out.into_bytes()
}

async fn respond(
    sock: &mut tokio::net::TcpStream,
    status: u16,
    headers: Vec<(String, String)>,
    body: &[u8],
    send_body: bool,
) {
    let mut out = head_text(status, &headers);
    if send_body {
        out.extend_from_slice(body);
    }
    let _ = sock.write_all(&out).await;
}

fn h(k: &str, v: impl ToString) -> (String, String) {
    (k.to_owned(), v.to_string())
}

async fn handle(fake: Arc<Fake>, server: String, mut sock: tokio::net::TcpStream) {
    let mut data = Vec::new();
    let mut buf = [0u8; 65536];
    while !data.windows(4).any(|w| w == b"\r\n\r\n") {
        match sock.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    }
    let end = data.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head: String = data[..end].iter().map(|b| char::from(*b)).collect();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap().splitn(3, ' ');
    let (method, target) = (
        first.next().unwrap().to_owned(),
        first.next().unwrap().to_owned(),
    );
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_owned());
        }
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    fake.log.lock().unwrap().push(json!([
        server,
        method,
        target,
        headers.get("range"),
        headers.get("if-range"),
        headers.get("authorization")
    ]));
    let Some(spec) = fake.files.get(&(server.clone(), path.to_owned())).cloned() else {
        respond(
            &mut sock,
            404,
            vec![h("Content-Length", 9)],
            b"not found",
            method != "HEAD",
        )
        .await;
        return;
    };
    let get = method == "GET";
    if spec.get("redirect_loop").is_some_and(|v| v == true) {
        let n: u64 = query.strip_prefix("r=").unwrap_or("0").parse().unwrap_or(0) + 1;
        let location = format!("http://{}{path}?r={n}", fake.host(&server));
        respond(
            &mut sock,
            302,
            vec![h("Location", location), h("Content-Length", 0)],
            b"",
            true,
        )
        .await;
        return;
    }
    if let Some(to) = spec.get("redirect") {
        let location = format!(
            "http://{}{}",
            fake.host(to["server"].as_str().unwrap()),
            to["path"].as_str().unwrap()
        );
        respond(
            &mut sock,
            302,
            vec![h("Location", location), h("Content-Length", 0)],
            b"",
            true,
        )
        .await;
        return;
    }
    let size = usize::try_from(spec["size"].as_u64().unwrap()).unwrap();
    let seed = spec["seed"].as_u64().unwrap();
    let body = content(seed, size);
    let mut meta = Vec::new();
    if spec.get("ranges").is_some_and(|v| v == true) {
        meta.push(h("Accept-Ranges", "bytes"));
    }
    for (key, name) in [
        ("etag", "ETag"),
        ("last_modified", "Last-Modified"),
        ("date", "Date"),
    ] {
        if let Some(v) = spec.get(key).and_then(Value::as_str) {
            meta.push(h(name, v));
        }
    }
    let with = |mut first: Vec<(String, String)>, meta: &[(String, String)]| {
        first.extend_from_slice(meta);
        first
    };
    if let Some(status) = spec.get("status").and_then(Value::as_u64) {
        let text = spec["status_body"]
            .as_str()
            .unwrap_or("")
            .as_bytes()
            .to_vec();
        respond(
            &mut sock,
            u16::try_from(status).unwrap(),
            vec![h("Content-Length", text.len())],
            &text,
            get,
        )
        .await;
        return;
    }
    if !get {
        let declared = spec
            .get("head_size")
            .and_then(Value::as_u64)
            .map_or(size, |v| usize::try_from(v).unwrap());
        respond(
            &mut sock,
            200,
            with(vec![h("Content-Length", declared)], &meta),
            b"",
            false,
        )
        .await;
        return;
    }
    let wanted = headers.get("range").cloned();
    if let Some(wanted) = wanted.filter(|_| {
        spec.get("ranges").is_some_and(|v| v == true) || spec.get("range_mode").is_some()
    }) {
        let (first, last) = wanted.trim_start_matches("bytes=").split_once('-').unwrap();
        let first: usize = first.parse().unwrap();
        if !last.is_empty() {
            let last: usize = last.parse().unwrap();
            if spec.get("fail_range_start").and_then(Value::as_u64) == Some(first as u64) {
                respond(
                    &mut sock,
                    200,
                    with(vec![h("Content-Length", size)], &meta),
                    &body,
                    true,
                )
                .await;
                return;
            }
            let total = if spec.get("bad_range_start").and_then(Value::as_u64) == Some(first as u64)
            {
                size + 1
            } else {
                size
            };
            let part = &body[first..=last];
            respond(
                &mut sock,
                206,
                with(
                    vec![
                        h("Content-Length", part.len()),
                        h("Content-Range", format!("bytes {first}-{last}/{total}")),
                    ],
                    &meta,
                ),
                part,
                true,
            )
            .await;
            return;
        }
        let mode = spec
            .get("range_mode")
            .and_then(Value::as_str)
            .unwrap_or("honor");
        let current = spec
            .get("etag")
            .and_then(Value::as_str)
            .or_else(|| spec.get("last_modified").and_then(Value::as_str));
        let if_range = headers.get("if-range").map(String::as_str);
        match mode {
            "416" => {
                respond(
                    &mut sock,
                    416,
                    vec![
                        h("Content-Length", 0),
                        h("Content-Range", format!("bytes */{size}")),
                    ],
                    b"",
                    true,
                )
                .await;
                return;
            }
            "wrong_offset" => {
                respond(
                    &mut sock,
                    206,
                    with(
                        vec![
                            h("Content-Length", size),
                            h("Content-Range", format!("bytes 0-{}/{size}", size - 1)),
                        ],
                        &meta,
                    ),
                    &body,
                    true,
                )
                .await;
                return;
            }
            "other_total" => {
                let part = &body[first..];
                respond(
                    &mut sock,
                    206,
                    with(
                        vec![
                            h("Content-Length", part.len()),
                            h(
                                "Content-Range",
                                format!("bytes {first}-{}/{}", size - 1, size + 7),
                            ),
                        ],
                        &meta,
                    ),
                    part,
                    true,
                )
                .await;
                return;
            }
            "honor" if if_range.is_none() || if_range == current => {
                let part = &body[first..];
                respond(
                    &mut sock,
                    206,
                    with(
                        vec![
                            h("Content-Length", part.len()),
                            h(
                                "Content-Range",
                                format!("bytes {first}-{}/{size}", size - 1),
                            ),
                        ],
                        &meta,
                    ),
                    part,
                    true,
                )
                .await;
                return;
            }
            _ => {}
        }
    }
    let get_size = spec
        .get("get_size")
        .and_then(Value::as_u64)
        .map_or(size, |v| usize::try_from(v).unwrap());
    let full = content(seed, get_size);
    if let Some(n) = spec.get("truncate_after").and_then(Value::as_u64) {
        let n = usize::try_from(n).unwrap();
        respond(
            &mut sock,
            200,
            with(vec![h("Content-Length", full.len())], &meta),
            &full[..n],
            true,
        )
        .await;
        return;
    }
    if let Some(n) = spec.get("stall_after").and_then(Value::as_u64) {
        let n = usize::try_from(n).unwrap();
        respond(
            &mut sock,
            200,
            with(vec![h("Content-Length", full.len())], &meta),
            &full[..n],
            true,
        )
        .await;
        let mut release = fake.release.subscribe();
        let _ = tokio::time::timeout(Duration::from_secs(30), release.wait_for(|r| *r)).await;
        let _ = sock.write_all(&full[n..]).await;
        return;
    }
    respond(
        &mut sock,
        200,
        with(vec![h("Content-Length", full.len())], &meta),
        &full,
        true,
    )
    .await;
}

async fn serve(fake: &Arc<Fake>, server: &str) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    fake.ports
        .lock()
        .unwrap()
        .insert(server.to_owned(), listener.local_addr().unwrap().port());
    let (fake, server) = (fake.clone(), server.to_owned());
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(handle(fake.clone(), server.clone(), sock));
        }
    });
}

// ---- the fake child: answers each planning request with the recorded plan ------------------

async fn child(plans: Arc<Mutex<Vec<(String, String, String)>>>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let plans = plans.clone();
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buf = [0u8; 65536];
                let end = loop {
                    if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    let n = sock.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buf[..n]);
                };
                let head = String::from_utf8_lossy(&data[..end]).to_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse().unwrap());
                while data.len() < end + length {
                    let n = sock.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buf[..n]);
                }
                assert!(head.contains("x-noevia-download-plan: 1"), "{head}");
                assert!(head.contains(&format!("x-model-loader-token: {}", TOKEN.to_lowercase())));
                let (path, body, answer) = plans.lock().unwrap().remove(0);
                assert!(
                    head.starts_with(&format!("post {} ", path.to_lowercase())),
                    "{head}"
                );
                assert_eq!(String::from_utf8_lossy(&data[end..end + length]), body);
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            });
        }
    });
    port
}

fn state(table: Arc<Downloads>, child_port: u16) -> Arc<StateData> {
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
        check_updates: None,
        downloads: Some(table),
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
        permits: tokio::sync::Semaphore::new(16),
        legacy: noevia_model_manager::legacy_client().unwrap(),
        legacy_base: format!("http://127.0.0.1:{child_port}"),
        idle_unload: None,
    })
}

async fn send(
    state: &Arc<StateData>,
    method: &str,
    uri: &str,
    body: Option<String>,
) -> (u16, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-model-loader-token", TOKEN);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let response = noevia_model_manager::router(state.clone())
        .oneshot(request.body(Body::from(body.unwrap_or_default())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn jobs(state: &Arc<StateData>) -> Vec<Value> {
    let (status, body) = send(state, "GET", "/api/v1/downloads", None).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str::<Value>(&body).unwrap()["jobs"]
        .as_array()
        .unwrap()
        .clone()
}

async fn wait(state: &Arc<StateData>, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let end = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let list = jobs(state).await;
        if done(&list) {
            return list;
        }
        assert!(
            std::time::Instant::now() < end,
            "timed out waiting: {list:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn idle(list: &[Value]) -> bool {
    !list
        .iter()
        .any(|j| matches!(j["status"].as_str(), Some("queued" | "downloading")))
}

struct Ports {
    hub: String,
    cdn: String,
    dead: String,
}

impl Ports {
    fn hide(&self, text: &str) -> String {
        text.replace(&self.hub, "{HUB}")
            .replace(&self.cdn, "{CDN}")
            .replace(&self.dead, "{DEAD}")
    }
    fn show(&self, text: &str) -> String {
        text.replace("{HUB}", &self.hub)
            .replace("{CDN}", &self.cdn)
            .replace("{DEAD}", &self.dead)
    }
}

/// Each listed job's keys in the order the body writes them.
fn key_orders(body: &str) -> HashMap<String, Vec<String>> {
    use model_files::json::Value as Json;
    let parsed = model_files::json::parse(body).unwrap();
    let Some(Json::Arr(list)) = parsed.get("jobs") else {
        panic!("no jobs")
    };
    list.iter()
        .map(|j| {
            let Json::Obj(pairs) = j else { panic!("job") };
            let id = match j.get("id") {
                Some(Json::Str(id)) => id.clone(),
                _ => panic!("id"),
            };
            (id, pairs.iter().map(|(k, _)| k.clone()).collect())
        })
        .collect()
}

fn normal_job(j: &Value, ports: &Ports, progress: bool, keys: Vec<String>) -> Value {
    let chunks: Vec<Value> = if progress {
        j["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| json!({"index": c["index"], "pct": c["pct"], "status": c["status"]}))
            .collect()
    } else {
        vec![]
    };
    json!({
        "repo": ports.hide(j["repo"].as_str().unwrap()),
        "filename": j["filename"],
        "status": j["status"],
        "error": j["error"].as_str().map(|e| ports.hide(e)),
        "bytes": j["bytes"],
        "downloaded": if progress { j["downloaded"].clone() } else { Value::Null },
        "pct": if progress { j["pct"].clone() } else { Value::Null },
        "started": !j["startedAt"].is_null(),
        "completed": !j["completedAt"].is_null(),
        "parallel": j["parallel"],
        "chunks": chunks,
        "keys": keys,
    })
}

fn files_under(root: &Path) -> Vec<Value> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    let mut paths = Vec::new();
    walk(root, &mut paths);
    paths.sort();
    paths
        .iter()
        .map(|p| {
            let data = std::fs::read(p).unwrap();
            let mut entry = json!({
                "path": p.strip_prefix(root).unwrap().to_str().unwrap(),
                "size": data.len(),
                "sha256": format!("{:x}", Sha256::digest(&data)),
            });
            if p.to_str().unwrap().ends_with(".validator") {
                entry["text"] = json!(String::from_utf8(data).unwrap());
            }
            entry
        })
        .collect()
}

#[test]
fn fixture_provenance_is_the_pinned_services_source() {
    let oracle = &fixture()["oracle"];
    assert_eq!(oracle["downloader.py"], DOWNLOADER_SHA);
    assert_eq!(oracle["main.py"], MAIN_SHA);
    assert_eq!(oracle["api.py"], API_SHA);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_python_differential() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap().clone();
    assert!(cases.len() >= 25);
    for case in cases {
        let id = case["id"].as_str().unwrap().to_owned();
        let files: HashMap<(String, String), Value> = case["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    (
                        f["server"].as_str().unwrap().to_owned(),
                        f["path"].as_str().unwrap().to_owned(),
                    ),
                    f.clone(),
                )
            })
            .collect();
        let fake = Arc::new(Fake {
            files,
            log: Mutex::new(Vec::new()),
            release: tokio::sync::watch::channel(false).0,
            ports: Mutex::new(HashMap::new()),
        });
        serve(&fake, "hub").await;
        serve(&fake, "cdn").await;
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let ports = Ports {
            hub: fake.host("hub"),
            cdn: fake.host("cdn"),
            dead: format!("127.0.0.1:{dead}"),
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let models = root.join("models");
        std::fs::create_dir(&models).unwrap();
        for t in case["targets"].as_array().unwrap() {
            std::fs::create_dir(models.join(t.as_str().unwrap())).unwrap();
        }
        let db = Connection::open(root.join("model_loader.db")).unwrap();
        for sql in fixture["schema"].as_array().unwrap() {
            db.execute_batch(sql.as_str().unwrap()).unwrap();
        }
        if let Some(token) = case["token"].as_str() {
            db.execute("INSERT INTO kv(key, value) VALUES('hf_token', ?1)", [token])
                .unwrap();
        }
        let plans: Vec<(String, String, String)> = case["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["op"] == "start")
            .map(|s| {
                (
                    s["path"].as_str().unwrap().to_owned(),
                    ports.show(s["request"].as_str().unwrap()),
                    ports.show(&serde_json::to_string(&s["plan"]).unwrap()),
                )
            })
            .collect();
        let expected_plans = plans.len();
        let plans = Arc::new(Mutex::new(plans));
        let child_port = child(plans.clone()).await;
        let table = Downloads::new(Config {
            models_dir: models.to_str().unwrap().to_owned(),
            database: root.join("model_loader.db"),
            max_concurrent: usize::try_from(case["concurrent"].as_u64().unwrap()).unwrap(),
            token_origin: TokenOrigin {
                scheme: "http".into(),
                host: "127.0.0.1".into(),
                port: fake.ports.lock().unwrap()["hub"],
            },
            parallel_min: fixture["parallel_min"].as_i64().unwrap(),
            unknown_cap: noevia_model_manager::downloads::UNKNOWN_SIZE_CAP,
            clock: wall,
        })
        .unwrap();
        let state = state(table, child_port);
        let progress =
            case.get("progress").is_none_or(|p| p != false) && !id.starts_with("parallel-");
        let mut created = 0;
        for (n, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            let at = format!("{id}: step {n} {}", step["op"]);
            match step["op"].as_str().unwrap() {
                "partial" => {
                    let p = models.join(step["file"].as_str().unwrap());
                    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                    let total = usize::try_from(step["total"].as_u64().unwrap()).unwrap();
                    let keep = usize::try_from(step["size"].as_u64().unwrap()).unwrap();
                    std::fs::write(&p, &content(step["seed"].as_u64().unwrap(), total)[..keep])
                        .unwrap();
                    if let Some(v) = step["validator"].as_str() {
                        std::fs::write(format!("{}.validator", p.to_str().unwrap()), v).unwrap();
                    }
                }
                "start" => {
                    let before = jobs(&state).await.len();
                    let (status, body) = send(
                        &state,
                        "POST",
                        step["path"].as_str().unwrap(),
                        Some(ports.show(step["request"].as_str().unwrap())),
                    )
                    .await;
                    assert_eq!(status, step["plan"]["status"], "{at}: {body}");
                    assert_eq!(body, step["plan"]["body"].as_str().unwrap(), "{at}");
                    created += jobs(&state).await.len() - before;
                }
                "wait" => {
                    let until = step["until"].clone();
                    if until == "idle" {
                        wait(&state, idle).await;
                    } else {
                        wait(&state, |list| {
                            list.iter().any(|j| {
                                j["filename"] == until["file"]
                                    && match until.get("downloaded") {
                                        Some(d) => j["downloaded"].as_u64() >= d.as_u64(),
                                        None => j["status"] == until["status"],
                                    }
                            })
                        })
                        .await;
                    }
                }
                "list" => {
                    let (status, body) = send(&state, "GET", "/api/v1/downloads", None).await;
                    assert_eq!(status, 200, "{at}");
                    let orders = key_orders(&body);
                    let list: Value = serde_json::from_str(&body).unwrap();
                    let mut got: Vec<Value> = list["jobs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|j| {
                            let keys = orders[j["id"].as_str().unwrap()].clone();
                            normal_job(j, &ports, progress, keys)
                        })
                        .collect();
                    got.sort_by(|a, b| a["filename"].as_str().cmp(&b["filename"].as_str()));
                    assert_eq!(Value::Array(got), step["jobs"], "{at}");
                }
                "cancel" => {
                    let list = jobs(&state).await;
                    let job = list.iter().find(|j| j["filename"] == step["file"]).unwrap();
                    let uri = format!("/api/v1/downloads/{}/cancel", job["id"].as_str().unwrap());
                    let (status, body) = send(&state, "POST", &uri, None).await;
                    assert_eq!(
                        (json!(status), json!(body)),
                        (step["status"].clone(), step["body"].clone()),
                        "{at}"
                    );
                }
                "cancel_unknown" => {
                    let (status, body) = send(
                        &state,
                        "POST",
                        "/api/v1/downloads/synthetic-unknown/cancel",
                        None,
                    )
                    .await;
                    assert_eq!(
                        (json!(status), json!(body)),
                        (step["status"].clone(), step["body"].clone()),
                        "{at}"
                    );
                }
                "clear" => {
                    let (status, body) =
                        send(&state, "POST", "/api/v1/downloads/clear", None).await;
                    assert_eq!(
                        (json!(status), json!(body)),
                        (step["status"].clone(), step["body"].clone()),
                        "{at}"
                    );
                }
                "release" => {
                    fake.release.send_replace(true);
                }
                other => panic!("{id}: unknown step {other}"),
            }
        }
        wait(&state, idle).await;
        fake.release.send_replace(true);
        assert!(
            plans.lock().unwrap().is_empty(),
            "{id}: {expected_plans} plans not all asked for"
        );
        let end = std::time::Instant::now() + Duration::from_secs(20);
        let rows = loop {
            let rows: Vec<Value> = db
                .prepare("SELECT repo_id, filename, dest_path, total_bytes, status, error, started_at, completed_at FROM download_history ORDER BY id")
                .unwrap()
                .query_map([], |r| {
                    let dest: String = r.get(2)?;
                    let error: Option<String> = r.get(5)?;
                    let started: Option<f64> = r.get(6)?;
                    let completed: Option<f64> = r.get(7)?;
                    Ok(json!([
                        ports.hide(&r.get::<_, String>(0)?),
                        r.get::<_, String>(1)?,
                        Path::new(&dest).strip_prefix(&models).unwrap().to_str().unwrap(),
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                        error.map(|e| ports.hide(&e)),
                        started.is_some(),
                        completed.is_some()
                    ]))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            if rows.len() >= created {
                break rows;
            }
            assert!(std::time::Instant::now() < end, "{id}: history rows");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let mut rows = rows;
        rows.sort_by_key(py_dumps);
        assert_eq!(
            Value::Array(rows),
            case["history"],
            "{id}: download_history"
        );
        assert_eq!(
            Value::Array(files_under(&models)),
            case["models"],
            "{id}: models folder"
        );
        if !case["requests"].is_null() {
            let mut log: Vec<Value> = fake
                .log
                .lock()
                .unwrap()
                .iter()
                .map(|r| {
                    let mut r = r.clone();
                    r[2] = json!(ports.hide(r[2].as_str().unwrap()));
                    r
                })
                .collect();
            log.sort_by_key(py_dumps);
            assert_eq!(Value::Array(log), case["requests"], "{id}: requests");
        }
    }
}

/// `safe_dest` against Python's on the same tree: symlinks inside and outside, a loop, a
/// dangling link, absolute and climbing names.
#[test]
fn safe_dest_matches_python() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let models = base.join("models");
    std::fs::create_dir_all(models.join("sub")).unwrap();
    std::fs::create_dir(base.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(models.join("sub"), models.join("inside")).unwrap();
    std::os::unix::fs::symlink(base.join("elsewhere"), models.join("outside")).unwrap();
    std::os::unix::fs::symlink(models.join("loop2"), models.join("loop")).unwrap();
    std::os::unix::fs::symlink(models.join("loop"), models.join("loop2")).unwrap();
    std::os::unix::fs::symlink(models.join("nowhere"), models.join("dangling")).unwrap();
    let root = models.to_str().unwrap();
    for case in fixture()["safe_dest"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let got = match pyfmt::safe_dest(root, name) {
            pyfmt::Dest::Inside(dest) => {
                // `os.path.relpath(dest, models)`: lexically normalised.
                let mut parts: Vec<&str> = Vec::new();
                for p in dest.trim_start_matches(root).split('/') {
                    match p {
                        "" | "." => {}
                        ".." => {
                            parts.pop();
                        }
                        p => parts.push(p),
                    }
                }
                format!("inside:{}", parts.join("/"))
            }
            pyfmt::Dest::Outside => "outside".into(),
            pyfmt::Dest::Raised => "raised".into(),
        };
        assert_eq!(got, case["result"].as_str().unwrap(), "{name}");
    }
}

/// The route shapes: a segment that decodes to contain `/` is not a download route (the child
/// answers 404 for it); everything else is answered here.
#[test]
fn route_shapes() {
    use axum::http::Method;
    use noevia_model_manager::{downloads_route, DownloadRoute};
    assert_eq!(
        downloads_route(&Method::GET, "/api/v1/downloads"),
        Some(DownloadRoute::List)
    );
    assert_eq!(
        downloads_route(&Method::POST, "/api/v1/downloads"),
        Some(DownloadRoute::Start)
    );
    assert_eq!(
        downloads_route(&Method::POST, "/api/v1/downloads/clear"),
        Some(DownloadRoute::Clear)
    );
    assert_eq!(
        downloads_route(&Method::POST, "/api/v1/downloads/ab%20c/cancel"),
        Some(DownloadRoute::Cancel("ab c".into()))
    );
    assert_eq!(
        downloads_route(&Method::POST, "/api/v1/downloads/%zz/cancel"),
        Some(DownloadRoute::Cancel("%zz".into()))
    );
    assert_eq!(
        downloads_route(&Method::POST, "/api/v1/sections/m/draft-heads/download"),
        Some(DownloadRoute::Start)
    );
    for (method, path) in [
        (Method::POST, "/api/v1/downloads/a%2Fb/cancel"),
        (Method::POST, "/api/v1/downloads//cancel"),
        (Method::GET, "/api/v1/downloads/clear"),
        (Method::PUT, "/api/v1/downloads"),
        (Method::POST, "/api/v1/sections/a%2fb/draft-heads/download"),
        (Method::POST, "/api/v1/downloads/"),
        (Method::GET, "/api/v1/download-targets"),
    ] {
        assert_eq!(downloads_route(&method, path), None, "{method} {path}");
    }
}

/// A child that answers by request: a 500 for a start whose body says so, a plan that would
/// write outside the models folder, a plan for a URL that never answers, and an overview.
async fn scripted_child(hang: u16) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buf = [0u8; 65536];
                let end = loop {
                    if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    let n = sock.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&buf[..n]);
                };
                let head = String::from_utf8_lossy(&data[..end]).to_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse().unwrap());
                while data.len() < end + length {
                    let n = sock.read(&mut buf).await.unwrap();
                    data.extend_from_slice(&buf[..n]);
                }
                let body = String::from_utf8_lossy(&data[end..end + length]).into_owned();
                let plan = |jobs: Value| {
                    json!({"status": 200, "body": "{\"queued\":[\"x.gguf\"]}", "jobs": jobs})
                        .to_string()
                };
                let (status, kind, answer) = if head.starts_with("get /api/v1/overview ") {
                    (
                        200,
                        "application/json",
                        r#"{"models":1,"activeDownloads":0,"diskFree":1.5}"#.to_owned(),
                    )
                } else if body.contains("boom") {
                    (
                        500,
                        "text/plain; charset=utf-8",
                        "Internal Server Error".to_owned(),
                    )
                } else if body.contains("escape") {
                    (
                        200,
                        "application/json",
                        plan(
                            json!([{"repo_id": "example.test", "filename": "../escape.gguf", "url": "http://127.0.0.1:9/x", "total_bytes": 1}]),
                        ),
                    )
                } else {
                    (
                        200,
                        "application/json",
                        plan(
                            json!([{"repo_id": "example.test", "filename": "hang/x.gguf", "url": format!("http://127.0.0.1:{hang}/x.gguf"), "total_bytes": 1}]),
                        ),
                    )
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failures_and_the_overview_count() {
    // A server that accepts and never answers: the job it serves stays "downloading".
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hang = silent.local_addr().unwrap().port();
    let held = Arc::new(Mutex::new(Vec::new()));
    let keep = held.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = silent.accept().await {
            keep.lock().unwrap().push(sock);
        }
    });
    let child_port = scripted_child(hang).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let models = root.join("models");
    std::fs::create_dir(&models).unwrap();
    let db = Connection::open(root.join("model_loader.db")).unwrap();
    for sql in fixture()["schema"].as_array().unwrap() {
        db.execute_batch(sql.as_str().unwrap()).unwrap();
    }
    let config = || Config {
        models_dir: models.to_str().unwrap().to_owned(),
        database: root.join("model_loader.db"),
        max_concurrent: 2,
        token_origin: TokenOrigin::hub(),
        parallel_min: 64 * 1024,
        unknown_cap: noevia_model_manager::downloads::UNKNOWN_SIZE_CAP,
        clock: wall,
    };
    let table = Downloads::new(config()).unwrap();
    let state = state(table.clone(), child_port);
    let start = |body: &str| send(&state, "POST", "/api/v1/downloads", Some(body.to_owned()));

    // The child's own 500 is the route's 500; nothing is queued.
    assert_eq!(
        start(r#"{"boom": 1}"#).await,
        (500, "Internal Server Error".to_owned())
    );
    // A plan whose file would land outside the models folder: safe_dest raises, a 500.
    assert_eq!(
        start(r#"{"escape": 1}"#).await,
        (500, "Internal Server Error".to_owned())
    );
    assert!(jobs(&state).await.is_empty());
    assert!(!root.join("escape.gguf").exists());

    // The overview comes from the child; its count is this table's.
    let (status, body) = send(&state, "GET", "/api/v1/overview", None).await;
    assert_eq!(
        (status, body.as_str()),
        (200, r#"{"models":1,"activeDownloads":0,"diskFree":1.5}"#)
    );
    assert_eq!(start(r#"{"url": "x"}"#).await.0, 200);
    wait(&state, |list| {
        list.iter().any(|j| j["status"] == "downloading")
    })
    .await;
    let (_, body) = send(&state, "GET", "/api/v1/overview", None).await;
    assert_eq!(body, r#"{"models":1,"activeDownloads":1,"diskFree":1.5}"#);
    let id = jobs(&state).await[0]["id"].as_str().unwrap().to_owned();
    let (_, answer) = send(
        &state,
        "POST",
        &format!("/api/v1/downloads/{id}/cancel"),
        None,
    )
    .await;
    assert_eq!(answer, r#"{"ok":true}"#);
    let list = wait(&state, idle).await;
    assert_eq!(list[0]["status"], "canceled");
    assert!(!models.join("hang/x.gguf.download").exists());

    // noevia#1972: while a delete holds the folder, a start into it is a 409 and nothing is
    // queued; once it lets go, the same start queues again.
    let held = table.reserve(vec!["hang".into()]);
    let (status, body) = start(r#"{"url": "x"}"#).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"detail": noevia_model_manager::downloads::DELETE_IN_PROGRESS})
    );
    assert_eq!(jobs(&state).await.len(), 1);
    drop(held);
    assert_eq!(start(r#"{"url": "x"}"#).await.0, 200);
    assert_eq!(jobs(&state).await.len(), 2);
    let id = jobs(&state).await[0]["id"].as_str().unwrap().to_owned();
    send(
        &state,
        "POST",
        &format!("/api/v1/downloads/{id}/cancel"),
        None,
    )
    .await;
    wait(&state, idle).await;

    // No child: a gateway error, as for any forwarded route.
    let gone = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let orphan = state_with(Downloads::new(config()).unwrap(), gone);
    let (status, _) = send(&orphan, "POST", "/api/v1/downloads", Some("{}".into())).await;
    assert_eq!(status, 502);
}

fn state_with(table: Arc<Downloads>, child_port: u16) -> Arc<StateData> {
    state(table, child_port)
}

// ---- noevia#1984: mixed mode (native downloads, Python delete) ------------------------------

/// How the fake child answers one request.
enum Reply {
    Answer(u16, String),
    /// Close the connection without answering.
    Close,
}

/// A child whose answers the test hands out one at a time: every request is reported on
/// `arrivals` (`"delete"` or `"plan"`), then waits for the next reply of its kind. Replies can be
/// queued before the request comes.
struct Scripted {
    port: u16,
    arrivals: tokio::sync::mpsc::UnboundedReceiver<&'static str>,
    deletes: tokio::sync::mpsc::UnboundedSender<Reply>,
    plans: tokio::sync::mpsc::UnboundedSender<Reply>,
}

async fn turn_based_child() -> Scripted {
    type Queue = Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Reply>>>;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (arrived, arrivals) = tokio::sync::mpsc::unbounded_channel();
    let (deletes, delete_rx) = tokio::sync::mpsc::unbounded_channel();
    let (plans, plan_rx) = tokio::sync::mpsc::unbounded_channel();
    let delete_rx: Queue = Arc::new(tokio::sync::Mutex::new(delete_rx));
    let plan_rx: Queue = Arc::new(tokio::sync::Mutex::new(plan_rx));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let (arrived, delete_rx, plan_rx) =
                (arrived.clone(), delete_rx.clone(), plan_rx.clone());
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buf = [0u8; 65536];
                let end = loop {
                    if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    let n = sock.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&buf[..n]);
                };
                let head = String::from_utf8_lossy(&data[..end]).to_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse().unwrap());
                while data.len() < end + length {
                    let n = sock.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&buf[..n]);
                }
                let (kind, queue) = if head.starts_with("post /api/v1/models/delete ") {
                    assert!(!head.contains("x-noevia-download-plan"), "{head}");
                    ("delete", delete_rx)
                } else {
                    assert!(head.contains("x-noevia-download-plan: 1"), "{head}");
                    ("plan", plan_rx)
                };
                arrived.send(kind).unwrap();
                let reply = queue.lock().await.recv().await;
                if let Some(Reply::Answer(status, answer)) = reply {
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                        answer.len()
                    );
                    let _ = sock.write_all(reply.as_bytes()).await;
                }
            });
        }
    });
    Scripted {
        port,
        arrivals,
        deletes,
        plans,
    }
}

impl Scripted {
    async fn arrived(&mut self) -> &'static str {
        tokio::time::timeout(Duration::from_secs(20), self.arrivals.recv())
            .await
            .expect("the child got no request")
            .unwrap()
    }
}

/// A plan the child answers a start with: one file, served by a server that never answers.
fn one_file_plan(filename: &str, hang: u16) -> Reply {
    Reply::Answer(
        200,
        json!({
            "status": 200,
            "body": "{\"queued\":[\"x.gguf\"]}",
            "jobs": [{"repo_id": "example.test", "filename": filename,
                      "url": format!("http://127.0.0.1:{hang}/x.gguf"), "total_bytes": 1}],
        })
        .to_string(),
    )
}

async fn wait_released(table: &Downloads) {
    let end = std::time::Instant::now() + Duration::from_secs(20);
    while table.whole_reservations() != 0 {
        assert!(std::time::Instant::now() < end, "the reservation was kept");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

const DELETE: &str = r#"{"models":["synthetic/m.gguf"]}"#;
const URL_START: &str = "/api/v1/downloads";
const HEADS_START: &str = "/api/v1/sections/synthetic-chat/draft-heads/download";

/// noevia#1984: while a Python delete is forwarded, every native start (a URL plan, a draft-head
/// plan, also one whose plan was already in flight when the delete began) answers the delete
/// 409 and queues nothing. The whole-table reservation is released when the child answers (a
/// 200 or an error), when the forward fails, and, for a request dropped mid-forward, only once
/// the child has answered. Starts before and after a forward queue as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_delete_forward_reserves_the_whole_table() {
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hang = silent.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = silent.accept().await {
            held.push(sock);
        }
    });
    let mut child = turn_based_child().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let models = root.join("models");
    std::fs::create_dir(&models).unwrap();
    let db = Connection::open(root.join("model_loader.db")).unwrap();
    for sql in fixture()["schema"].as_array().unwrap() {
        db.execute_batch(sql.as_str().unwrap()).unwrap();
    }
    let table = Downloads::new(Config {
        models_dir: models.to_str().unwrap().to_owned(),
        database: root.join("model_loader.db"),
        max_concurrent: 2,
        token_origin: TokenOrigin::hub(),
        parallel_min: 64 * 1024,
        unknown_cap: noevia_model_manager::downloads::UNKNOWN_SIZE_CAP,
        clock: wall,
    })
    .unwrap();
    let state = state(table.clone(), child.port);
    let post = |uri: &'static str, body: &'static str| {
        let state = state.clone();
        tokio::spawn(async move { send(&state, "POST", uri, Some(body.to_owned())).await })
    };
    let conflict = json!({"detail": noevia_model_manager::downloads::DELETE_IN_PROGRESS});
    let cancel_all = |state: Arc<StateData>| async move {
        for job in jobs(&state).await {
            let id = job["id"].as_str().unwrap().to_owned();
            send(
                &state,
                "POST",
                &format!("/api/v1/downloads/{id}/cancel"),
                None,
            )
            .await;
        }
        wait(&state, idle).await;
    };

    // Before any forward a URL start queues as before.
    child
        .plans
        .send(one_file_plan("before/x.gguf", hang))
        .unwrap();
    assert_eq!(post(URL_START, r#"{"url": "x"}"#).await.unwrap().0, 200);
    assert_eq!(child.arrived().await, "plan");
    assert_eq!(jobs(&state).await.len(), 1);
    cancel_all(state.clone()).await;

    // A URL start whose plan is in flight when the delete begins.
    let url_start = post(URL_START, r#"{"url": "x"}"#);
    assert_eq!(child.arrived().await, "plan");
    let delete = post("/api/v1/models/delete", DELETE);
    assert_eq!(child.arrived().await, "delete");
    assert_eq!(table.whole_reservations(), 1);
    child
        .plans
        .send(one_file_plan("hang/x.gguf", hang))
        .unwrap();
    let (status, body) = url_start.await.unwrap();
    assert_eq!(status, 409, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), conflict);
    // A draft-head start begun during the delete.
    child
        .plans
        .send(one_file_plan("heads/d.gguf", hang))
        .unwrap();
    let (status, body) = post(HEADS_START, "{}").await.unwrap();
    assert_eq!(child.arrived().await, "plan");
    assert_eq!(status, 409, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), conflict);
    assert_eq!(jobs(&state).await.len(), 1, "nothing new was queued");
    assert!(!models.join("hang").exists() && !models.join("heads").exists());
    // The child answers: the answer is passed through and the reservation is gone.
    child
        .deletes
        .send(Reply::Answer(200, r#"{"results":[]}"#.into()))
        .unwrap();
    assert_eq!(delete.await.unwrap(), (200, r#"{"results":[]}"#.to_owned()));
    assert_eq!(table.whole_reservations(), 0);
    child
        .plans
        .send(one_file_plan("heads/d.gguf", hang))
        .unwrap();
    assert_eq!(post(HEADS_START, "{}").await.unwrap().0, 200);
    assert_eq!(child.arrived().await, "plan");
    assert_eq!(jobs(&state).await.len(), 2);
    cancel_all(state.clone()).await;

    // The child's error is passed through; the reservation is released.
    child
        .deletes
        .send(Reply::Answer(500, r#"{"detail":"boom"}"#.into()))
        .unwrap();
    assert_eq!(post("/api/v1/models/delete", DELETE).await.unwrap().0, 500);
    assert_eq!(child.arrived().await, "delete");
    assert_eq!(table.whole_reservations(), 0);
    // The child drops the connection: a gateway error; released.
    child.deletes.send(Reply::Close).unwrap();
    assert_eq!(post("/api/v1/models/delete", DELETE).await.unwrap().0, 502);
    assert_eq!(child.arrived().await, "delete");
    assert_eq!(table.whole_reservations(), 0);

    // The request is dropped mid-forward (the client went away; a route timeout drops it the
    // same way): Python may still be deleting, so the reservation stays until the child answers.
    let dropped = post("/api/v1/models/delete", DELETE);
    assert_eq!(child.arrived().await, "delete");
    dropped.abort();
    assert!(dropped.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        table.whole_reservations(),
        1,
        "released while Python deletes"
    );
    child
        .plans
        .send(one_file_plan("hang/x.gguf", hang))
        .unwrap();
    let (status, body) = post(URL_START, r#"{"url": "x"}"#).await.unwrap();
    assert_eq!(child.arrived().await, "plan");
    assert_eq!(status, 409, "{body}");
    child
        .deletes
        .send(Reply::Answer(200, r#"{"results":[]}"#.into()))
        .unwrap();
    wait_released(&table).await;
    child
        .plans
        .send(one_file_plan("hang/x.gguf", hang))
        .unwrap();
    assert_eq!(post(URL_START, r#"{"url": "x"}"#).await.unwrap().0, 200);
    assert_eq!(child.arrived().await, "plan");
    cancel_all(state.clone()).await;
}
