//! noevia#1318 slice 13: native `GET /api/v1/backends` and the backend sampler through the real
//! router and the real Unix Docker transport (`docker::UnixDocker`), against a fake Docker daemon
//! on a Unix socket that answers the recorded worlds of `fixtures/backends-python.v1.json` (the
//! actual services Python) with real HTTP status lines. Ticks and GETs must match Python's cache
//! and answers; route shapes the native read does not own reach the Python child. Every name is
//! synthetic; no Docker daemon, container, GPU or model is touched.
#![cfg(feature = "native-backends")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{
    backend_stats::{gpu_vram_map, BackendStats, Config, REFUSAL},
    docker::UnixDocker,
    Container, DockerRead, Redactor, StateData,
};
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

fn state(stats: Option<Arc<BackendStats>>, _legacy_trap_base: &str) -> Arc<StateData> {
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
        token: TOKEN.into(),
        configured: vec![],
        docker: Arc::new(NoDocker),
        redactor: Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(4),
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
        backend_stats: stats,
        #[cfg(feature = "native-benchmark-runner")]
        bench_runner: None,
        #[cfg(feature = "native-backend-control")]
        backends: None,
    })
}

async fn call(state: Arc<StateData>, method: &str, uri: &str, token: bool) -> (u16, String) {
    let mut request = Request::builder().method(method).uri(uri);
    if token {
        request = request.header("x-model-loader-token", TOKEN);
    }
    let response = noevia_model_manager::router(state)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn closed_port() -> String {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", closed.local_addr().unwrap().port());
    drop(closed);
    base
}

/// Reads one HTTP request head (and its body) from a stream.
async fn read_request<S: tokio::io::AsyncRead + Unpin>(sock: &mut S) -> Option<(String, Vec<u8>)> {
    let mut data = Vec::new();
    let mut buf = [0u8; 65536];
    let end = loop {
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&data[..end]).into_owned();
    let length: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap())
        })
        .unwrap_or(0);
    while data.len() < end + length {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    }
    Some((head, data[end..end + length].to_vec()))
}

type World = Arc<Mutex<Value>>;

/// The fake Docker daemon: the generator's fake daemon, over a Unix socket, real status lines.
fn daemon(socket: &Path, world: World, seen: Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (world, seen) = (world.clone(), seen.clone());
            tokio::spawn(async move {
                let Some((head, _)) = read_request(&mut sock).await else {
                    return;
                };
                let line = head.lines().next().unwrap().to_owned();
                let target = line.split(' ').nth(1).unwrap().to_owned();
                seen.lock()
                    .unwrap()
                    .push(format!("{} {target}", line.split(' ').next().unwrap()));
                let w = world.lock().unwrap().clone();
                let (status, body) = answer(&w, &target);
                let reason = match status {
                    200 => "OK",
                    404 => "Not Found",
                    _ => "Internal Server Error",
                };
                let raw = match &body {
                    Value::String(t) => t.clone(),
                    v => v.to_string(),
                };
                let reply = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{raw}",
                    raw.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            });
        }
    });
}

fn answer(w: &Value, target: &str) -> (u16, Value) {
    let (path, _) = target.split_once('?').unwrap_or((target, ""));
    let parts: Vec<&str> = path.split('/').collect();
    let containers = w["containers"].as_array().unwrap();
    let find = |r: &str| {
        containers
            .iter()
            .find(|c| c["Id"] == r || c["Name"].as_str().unwrap()[1..] == *r)
    };
    if path == "/version" {
        return (200, json!({"ApiVersion": "1.47"}));
    }
    if path == "/containers/json" {
        return (
            200,
            Value::Array(
                containers
                    .iter()
                    .map(|c| json!({"Id": c["Id"], "Names": [c["Name"]], "Image": c["Image"]}))
                    .collect(),
            ),
        );
    }
    match parts.as_slice() {
        ["", "containers", r, "json"] => {
            if let Some(e) = w["inspect_errors"].get(*r) {
                return (e[0].as_u64().unwrap() as u16, e[1].clone());
            }
            find(r).map_or(
                (404, json!({"message": format!("No such container: {r}")})),
                |c| (200, c.clone()),
            )
        }
        ["", "containers", r, "stats"] => {
            let id = find(r).unwrap()["Id"].as_str().unwrap().to_owned();
            if let Some(st) = w["stats_status"].get(&id) {
                return (
                    st.as_u64().unwrap() as u16,
                    json!({"message": "synthetic stats failure"}),
                );
            }
            (200, w["stats"].get(&id).cloned().unwrap_or(json!({})))
        }
        ["", "images", hex, "json"] => {
            let id = format!("sha256:{hex}");
            if let Some(e) = w["image_errors"].get(&id) {
                return (e[0].as_u64().unwrap() as u16, e[1].clone());
            }
            let missing = w["missing_images"].as_array().unwrap();
            match w["images"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["Id"] == id)
            {
                Some(i) if !missing.iter().any(|m| *m == id) => (200, i.clone()),
                _ => (404, json!({"message": format!("No such image: {id}")})),
            }
        }
        _ => (404, json!({"message": "page not found"})),
    }
}

/// The fake child: recorded GPU reads, recorded pushes.
async fn child(probes: Arc<Mutex<Value>>, pushed: Arc<Mutex<Vec<String>>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (p, q) = (probes.clone(), pushed.clone());
            tokio::spawn(async move {
                let Some((head, body)) = read_request(&mut sock).await else {
                    return;
                };
                let line = head.lines().next().unwrap().to_owned();
                let reply = if line.starts_with("PUT /api/v1/internal/backend-stats ") {
                    q.lock().unwrap().push(String::from_utf8(body).unwrap());
                    r#"{"ok":true}"#.to_owned()
                } else {
                    // noevia#1318 step 2: never asked for a GPU read; recorded so the case fails.
                    let _ = &p;
                    q.lock()
                        .unwrap()
                        .push(format!("UNEXPECTED CHILD REQUEST {line}"));
                    json!({"gpu": null}).to_string()
                };
                let out = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = sock.write_all(out.as_bytes()).await;
            });
        }
    });
    base
}

async fn llama(port: u16, status: u64, body: String) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                if read_request(&mut sock).await.is_none() {
                    return;
                }
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            });
        }
    })
}

/// The port a recorded container's llama-server is probed on (`_resolve_internal_port`).
fn probe_port(container: &Value, default: Option<u16>) -> u16 {
    let mut port = None;
    if let Some(ports) = container["NetworkSettings"]["Ports"].as_object() {
        for key in ports.keys() {
            if let Ok(p) = key.split('/').next().unwrap().parse::<u16>() {
                port = Some(p);
            }
        }
    }
    if port.is_none() {
        let cmd: Vec<String> = container["Config"]["Cmd"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
            .unwrap_or_default();
        let joined = cmd.join(" ");
        if let Some(at) = joined.find("--port") {
            let rest = joined[at + 6..].trim_start_matches(['=', ' ']);
            port = rest
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|d| d.parse().ok());
        }
    }
    port.or(default).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recorded_worlds_through_the_router_and_the_unix_transport() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/backends-python.v1.json")).unwrap();
    let mut compared = 0;
    for id in [
        "vulkan-sysfs-apu",
        "docker-errors",
        "probes-and-uptime",
        "configured-rocm",
        "docker-down",
    ] {
        let case = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("docker.sock");
        let drm = dir.path().join("drm");
        let world: World = Arc::new(Mutex::new(Value::Null));
        let seen = Arc::new(Mutex::new(Vec::new()));
        daemon(&socket, world.clone(), seen.clone());
        let probes = Arc::new(Mutex::new(Value::Null));
        let pushed = Arc::new(Mutex::new(Vec::new()));
        let _legacy_trap_base = child(probes.clone(), pushed.clone()).await;
        let now = Arc::new(AtomicU64::new(0));
        let n = now.clone();
        let env = &case["env"];
        let default_port = match env["LLAMA_DEFAULT_PORT"].as_str() {
            Some(p) => Some(p.parse::<u16>().unwrap()).filter(|p| *p != 0),
            None => Some(8080),
        };
        let configured: Vec<String> = env["LLAMA_CONTAINERS"]
            .as_str()
            .unwrap_or("")
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        // A daemon that is down: a socket path nothing listens on.
        let gone = dir.path().join("gone.sock");
        let docker_for = |down: bool| {
            Arc::new(UnixDocker {
                socket: if down { gone.clone() } else { socket.clone() },
            })
        };
        let down = case["steps"][0]["docker"].is_null();
        let stats = Arc::new(
            BackendStats::with_clock(
                Config {
                    configured,
                    gpu_vram: gpu_vram_map(env["GPU_VRAM"].as_str().unwrap_or("")).unwrap(),
                    default_port,
                    drm_root: drm.clone(),
                    meminfo: "/nonexistent/meminfo".into(),
                    ram_reserve_gb: Some(32.0),
                },
                docker_for(down),
                Arc::new(move || f64::from_bits(n.load(Ordering::SeqCst))),
            )
            .unwrap(),
        );
        let errors: Vec<(String, String)> = case["restart_errors"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        stats.seed_restart_errors(&errors);
        let shared = state(Some(stats.clone()), &closed_port());
        for (i, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            let ctx = format!("{id} step {i}");
            now.store(step["at"].as_f64().unwrap().to_bits(), Ordering::SeqCst);
            *world.lock().unwrap() = step["docker"].clone();
            if step["op"] == "tick" {
                let _ = std::fs::remove_dir_all(&drm);
                std::fs::create_dir_all(&drm).unwrap();
                for (rel, text) in step["sysfs"].as_object().unwrap() {
                    let p = drm.join(rel);
                    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                    std::fs::write(p, text.as_str().unwrap()).unwrap();
                }
                *probes.lock().unwrap() = step["probes"].clone();
                pushed.lock().unwrap().clear();
                stats.tick().await;
                assert!(
                    pushed.lock().unwrap().is_empty(),
                    "{ctx}: unexpected private mirror"
                );
                let sampled: Value = serde_json::from_str(&stats.push_body().unwrap()).unwrap();
                assert_eq!(sampled["backends"], step["cache"], "{ctx}");
            } else {
                let mut servers = Vec::new();
                for c in step["docker"]["containers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    let name = &c["Name"].as_str().unwrap()[1..];
                    if let Some(a) = step["llama"].get(name).filter(|a| !a.is_null()) {
                        servers.push(
                            llama(
                                probe_port(c, default_port),
                                a[0].as_u64().unwrap(),
                                a[1].as_str().unwrap().to_owned(),
                            )
                            .await,
                        );
                    }
                }
                let (status, body) = call(shared.clone(), "GET", "/api/v1/backends", true).await;
                for s in servers {
                    s.abort();
                }
                assert_eq!(
                    u64::from(status),
                    step["status"].as_u64().unwrap(),
                    "{ctx}: {body}"
                );
                assert_eq!(body, step["body"].as_str().unwrap(), "{ctx}");
                compared += 1;
            }
        }
        // Only the allowlisted read-only GETs ever reached the daemon.
        for request in seen.lock().unwrap().iter() {
            assert!(request.starts_with("GET /"), "{request}");
            let path = request.trim_start_matches("GET ");
            assert!(
                noevia_model_manager::docker::allowed_backend_read(path),
                "{request}"
            );
        }
    }
    assert!(compared >= 8, "{compared}");
}

#[tokio::test]
async fn route_shapes_and_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let stats = Arc::new(
        BackendStats::new(
            Config {
                configured: vec!["llama-a".into()],
                gpu_vram: vec![],
                default_port: Some(8080),
                drm_root: dir.path().join("drm"),
                meminfo: "/nonexistent/meminfo".into(),
                ram_reserve_gb: Some(32.0),
            },
            Arc::new(UnixDocker {
                socket: dir.path().join("absent.sock"),
            }),
        )
        .unwrap(),
    );
    let native = state(Some(stats), &closed_port());
    // The token is checked first.
    assert_eq!(
        call(native.clone(), "GET", "/api/v1/backends", false)
            .await
            .0,
        401
    );
    // Docker unreachable: every configured backend says so, nothing sampled yet.
    let (status, body) = call(native.clone(), "GET", "/api/v1/backends", true).await;
    assert_eq!(status, 200, "{body}");
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["backends"][0]["status"], "docker unreachable");
    assert_eq!(parsed["backends"][0]["stats"]["error"], "warming up…");
    // Other methods, the slash variant and the per-backend routes stay with the child (a closed
    // port here: 502).
    for (method, path) in [
        ("POST", "/api/v1/backends"),
        ("GET", "/api/v1/backends/"),
        ("GET", "/api/v1/backends/llama-a/logs?tail=20"),
    ] {
        let (status, body) = call(native.clone(), method, path, true).await;
        assert_ne!(status, 200, "{method} {path}: {body}");
        assert!(!body.contains(REFUSAL), "{method} {path}");
    }
    // Missing native owner refuses locally.
    let python = state(None, &closed_port());
    assert_eq!(call(python, "GET", "/api/v1/backends", true).await.0, 503);
    // The private child calls are never reachable from outside.
    for path in [
        "/api/v1/internal/backend-stats",
        "/api/v1/internal/gpu-exec-probe?name=a&probe=nvidia",
    ] {
        assert_eq!(
            call(native.clone(), "GET", path, true).await.0,
            404,
            "{path}"
        );
    }
}

#[test]
fn allowlist_is_read_only_and_exact() {
    use noevia_model_manager::docker::allowed_backend_read as ok;
    for path in [
        "/version",
        "/containers/json?all=true&limit=1025&size=false",
        "/containers/llama-a/json",
        "/images/abc123/json",
        "/containers/0123abcd/stats?stream=false",
    ] {
        assert!(ok(path), "{path}");
    }
    for path in [
        "/containers/llama-a/exec",
        "/containers/llama-a/start",
        "/containers/llama-a/stats",
        "/containers/llama-a/stats?stream=true",
        "/containers/../json",
        "/containers/a b/json",
        "/exec/x/start",
        "/containers/json?all=true",
        "/info",
    ] {
        assert!(!ok(path), "{path}");
    }
}
