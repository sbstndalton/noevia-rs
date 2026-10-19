#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The actual Python differential: every case of `tests/fixtures/backends-python.v1.json`
//! (recorded from the services app by `tools/gen-backends-oracle.py`) is replayed step by step:
//! a sampler tick against the recorded Docker world, sysfs files and GPU reads must leave exactly
//! the cache Python's sampler left; a GET against the
//! recorded world and llama-servers must give Python's status and body byte for byte. Every name
//! is synthetic; the Docker daemon, the child and the llama-servers are fakes on loopback.
use super::*;
use serde_json::Value as V;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture() -> V {
    serde_json::from_str(include_str!("../../tests/fixtures/backends-python.v1.json")).unwrap()
}

/// The recorded Docker world, answered as the fake daemon of the generator answers it.
pub(crate) struct World(pub(crate) Mutex<Option<V>>);

fn answer(status: u16, body: &V) -> Raw {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "",
    };
    Raw {
        status,
        reason: reason.into(),
        body: match body {
            V::String(text) => text.clone().into_bytes(),
            other => other.to_string().into_bytes(),
        },
    }
}

impl DockerApi for World {
    fn get<'a>(&'a self, path: &'a str) -> Job<'a, Raw> {
        let w = self.0.lock().unwrap().clone();
        Box::pin(async move {
            let Some(w) = w else {
                return Err("docker unreachable");
            };
            let (path, _query) = path.split_once('?').unwrap_or((path, ""));
            let parts: Vec<&str> = path.split('/').collect();
            let containers = w["containers"].as_array().unwrap();
            let find = |r: &str| {
                containers
                    .iter()
                    .find(|c| c["Id"] == r || c["Name"].as_str().unwrap()[1..] == *r)
            };
            if path == "/version" {
                return Ok(answer(200, &serde_json::json!({"ApiVersion": "1.47"})));
            }
            if path == "/containers/json" {
                let list: Vec<V> = containers
                    .iter()
                    .map(|c| serde_json::json!({"Id": c["Id"], "Names": [c["Name"]], "Image": c["Image"]}))
                    .collect();
                return Ok(answer(200, &V::Array(list)));
            }
            match parts.as_slice() {
                ["", "containers", r, "json"] => {
                    if let Some(e) = w["inspect_errors"].get(*r) {
                        return Ok(answer(e[0].as_u64().unwrap() as u16, &e[1]));
                    }
                    match find(r) {
                        Some(c) => Ok(answer(200, c)),
                        None => Ok(answer(
                            404,
                            &serde_json::json!({"message": format!("No such container: {r}")}),
                        )),
                    }
                }
                ["", "containers", r, "stats"] => {
                    let Some(c) = find(r) else {
                        return Ok(answer(404, &serde_json::json!({"message": "x"})));
                    };
                    let id = c["Id"].as_str().unwrap();
                    if let Some(st) = w["stats_status"].get(id) {
                        return Ok(answer(
                            st.as_u64().unwrap() as u16,
                            &serde_json::json!({"message": "synthetic stats failure"}),
                        ));
                    }
                    Ok(answer(
                        200,
                        w["stats"].get(id).unwrap_or(&serde_json::json!({})),
                    ))
                }
                ["", "images", hex, "json"] => {
                    let id = format!("sha256:{hex}");
                    if let Some(e) = w["image_errors"].get(&id) {
                        return Ok(answer(e[0].as_u64().unwrap() as u16, &e[1]));
                    }
                    let missing = w["missing_images"].as_array().unwrap();
                    let image = w["images"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|i| i["Id"] == id);
                    match image {
                        Some(i) if !missing.iter().any(|m| *m == id) => Ok(answer(200, i)),
                        _ => Ok(answer(
                            404,
                            &serde_json::json!({"message": format!("No such image: {id}")}),
                        )),
                    }
                }
                _ => Ok(answer(
                    404,
                    &serde_json::json!({"message": "page not found"}),
                )),
            }
        })
    }
}

/// The fake child: GPU reads answered from the step's recorded `hw.exec_probe` results, pushes
/// recorded.
struct Child {
    requests: Arc<std::sync::atomic::AtomicUsize>,
    probes: Arc<Mutex<V>>,
    pushed: Arc<Mutex<Vec<String>>>,
    base: String,
}

async fn child() -> Child {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let probes = Arc::new(Mutex::new(V::Null));
    let pushed = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = requests.clone();
    let (p, q) = (probes.clone(), pushed.clone());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            seen.fetch_add(1, Ordering::SeqCst);
            let (p, q) = (p.clone(), q.clone());
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buf = [0u8; 65536];
                let end = loop {
                    if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
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
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&buf[..n]);
                }
                let line = head.lines().next().unwrap().to_owned();
                assert!(
                    head.contains("x-model-loader-token: synthetic-token"),
                    "{head}"
                );
                let body = if line.starts_with("PUT /api/v1/internal/backend-stats ") {
                    q.lock()
                        .unwrap()
                        .push(String::from_utf8(data[end..end + length].to_vec()).unwrap());
                    r#"{"ok":true}"#.to_owned()
                } else {
                    // noevia#1318 step 2: the sampler never asks the child for a GPU read; a
                    // request here is recorded as a push no case expects, which fails the case.
                    let _ = &p;
                    q.lock()
                        .unwrap()
                        .push(format!("UNEXPECTED CHILD REQUEST {line}"));
                    r#"{"gpu":null}"#.to_owned()
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            });
        }
    });
    Child {
        requests,
        probes,
        pushed,
        base,
    }
}

/// A fake llama-server answering `/v1/models` with the recorded status and body.
pub(super) async fn llama(port: u32, status: u64, body: String) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port as u16))
        .await
        .unwrap_or_else(|e| panic!("port {port}: {e}"));
    serve_llama(listener, status, body)
}

fn serve_llama(
    listener: tokio::net::TcpListener,
    status: u64,
    body: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if sock.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
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

/// abort requests cancellation; awaiting it releases the listener before a fixture rebinds.
pub(super) async fn stop_llamas(servers: Vec<tokio::task::JoinHandle<()>>) {
    for server in &servers {
        server.abort();
    }
    for server in servers {
        if let Err(error) = tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("fake backend listener did not stop")
        {
            assert!(
                error.is_cancelled(),
                "fake backend listener failed: {error}"
            );
        }
    }
}

/// Keep the listener owned from allocation through cancellation: no free-port discovery race.
/// On a current-thread runtime, abort without awaiting its completion cannot pass this rebind.
#[tokio::test]
async fn fixture_listener_is_released_before_the_same_port_is_reused() {
    let mut listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    for _ in 0..32 {
        let server = serve_llama(listener, 200, "{}".into());
        stop_llamas(vec![server]).await;
        listener = tokio::net::TcpListener::bind(address).await.unwrap();
    }
}

fn write_sysfs(root: &Path, files: &V) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).unwrap();
    for (rel, text) in files.as_object().unwrap() {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text.as_str().unwrap()).unwrap();
    }
}

fn configured(env: &V) -> Vec<String> {
    env["LLAMA_CONTAINERS"]
        .as_str()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

#[test]
fn fixture_provenance() {
    let f = fixture();
    assert_eq!(f["api_version"], "1.47");
    for name in ["api.py", "config.py", "hw.py", "services.py"] {
        assert_eq!(f["sources"][name].as_str().unwrap().len(), 64, "{name}");
    }
    assert!(f["cases"].as_array().unwrap().len() >= 9);
}

// Both complete replays use the frozen backend ports; unrelated sampler tests stay parallel.
static ORACLE_PORTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_cache_and_public_responses_match_actual_python_without_mirrors() {
    let _ports = ORACLE_PORTS.lock().await;
    let f = fixture();
    let mut ticks = 0;
    let mut gets = 0;
    for case in f["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let drm = dir.path().join("drm");
        let world = Arc::new(World(Mutex::new(None)));
        let fake_child = child().await;
        let now = Arc::new(AtomicU64::new(0));
        let n = now.clone();
        let env = &case["env"];
        let default_port = match env["LLAMA_DEFAULT_PORT"].as_str() {
            Some(p) => Some(p.parse::<u16>().unwrap()).filter(|p| *p != 0),
            None => Some(8080),
        };
        let stats = BackendStats::with_clock(
            Config {
                configured: configured(env),
                gpu_vram: gpu_vram_map(env["GPU_VRAM"].as_str().unwrap_or("")).unwrap(),
                default_port,
                drm_root: drm.clone(),
                legacy_base: fake_child.base.clone(),
                token: "synthetic-token".into(),
                meminfo: "/nonexistent/meminfo".into(),
                ram_reserve_gb: Some(32.0),
            },
            world.clone(),
            Arc::new(move || f64::from_bits(n.load(Ordering::SeqCst))),
        )
        .unwrap();
        let seeded: Vec<(String, String)> = case["restart_errors"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        stats.seed_restart_errors(&seeded);
        for (i, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            let ctx = format!("{id} step {i}");
            now.store(step["at"].as_f64().unwrap().to_bits(), Ordering::SeqCst);
            *world.0.lock().unwrap() = (!step["docker"].is_null()).then(|| step["docker"].clone());
            if step["op"] == "tick" {
                write_sysfs(&drm, &step["sysfs"]);
                *fake_child.probes.lock().unwrap() = step["probes"].clone();
                fake_child.pushed.lock().unwrap().clear();
                stats.tick().await;
                assert!(
                    fake_child.pushed.lock().unwrap().is_empty(),
                    "{ctx}: a private cache mirror was written"
                );
                let pushed: V = serde_json::from_str(&stats.push_body().unwrap()).unwrap();
                assert_eq!(pushed["backends"], step["cache"], "{ctx}");
                ticks += 1;
            } else {
                let mut servers = Vec::new();
                if let Some(containers) = step["docker"]["containers"].as_array() {
                    for c in containers {
                        let name = c["Name"].as_str().unwrap()[1..].to_owned();
                        let Some(a) = step["llama"].get(&name).filter(|a| !a.is_null()) else {
                            continue;
                        };
                        let attrs = model_files::json::parse(&c.to_string()).unwrap();
                        let port =
                            crate::backends::internal_port(&spec_of(&attrs).unwrap(), default_port)
                                .unwrap();
                        servers.push(
                            llama(
                                port,
                                a[0].as_u64().unwrap(),
                                a[1].as_str().unwrap().to_owned(),
                            )
                            .await,
                        );
                    }
                }
                let outcome = stats.get().await;
                stop_llamas(servers).await;
                match outcome {
                    Outcome::Json(body) => {
                        assert_eq!(step["status"], 200, "{ctx}");
                        assert_eq!(body, step["body"].as_str().unwrap(), "{ctx}");
                    }
                    Outcome::Failed => assert_eq!(step["status"], 500, "{ctx}"),
                    Outcome::Refused => panic!("{ctx}: refused"),
                }
                gets += 1;
            }
            assert_eq!(
                fake_child.requests.load(Ordering::SeqCst),
                0,
                "{ctx}: native sampler used private HTTP"
            );
        }
    }
    assert!(ticks >= 140 && gets >= 12, "{ticks} {gets}");
}

#[test]
fn neumaier_sum_and_python_max() {
    // Python 3.12: sum([0.1] * 10) == 1.0 (plain addition gives 0.9999999999999999).
    assert_eq!(py_sum(&[0.1; 10]), 1.0);
    assert_eq!(py_sum(&[1e100, 1.0, -1e100, 1.0]), 2.0);
    assert_eq!(py_sum(&[-0.0]), 0.0);
    assert!(py_sum(&[-0.0]).is_sign_positive());
    assert_eq!(py_sum(&[]), 0.0);
    assert_eq!(
        py_max(&[Num::Int(2900), Num::Float(0.0)]),
        Some(Num::Int(2900))
    );
    assert_eq!(
        py_max(&[Num::Float(0.0), Num::Int(0)]),
        Some(Num::Float(0.0))
    );
    assert_eq!(py_max(&[Num::Float(0.0), Num::Int(5)]), Some(Num::Int(5)));
}

#[test]
fn started_at_shapes() {
    let now = 1_792_240_000.0;
    let ok = parse_started("2026-10-17T10:00:00.123456789Z", now).unwrap();
    assert_eq!(ok.local, "2026-10-17 10:00");
    assert!(ok.secs.is_some());
    assert_eq!(
        parse_started("0001-01-01T00:00:00Z", now).unwrap(),
        Started {
            local: String::new(),
            up: String::new(),
            secs: None
        }
    );
    // fromisoformat's own range checks: the raw text, no uptime.
    let bad = parse_started("2026-02-30T10:00:00Z", now).unwrap();
    assert_eq!(
        (bad.local.as_str(), bad.secs),
        ("2026-02-30T10:00:00Z", None)
    );
    // A start in the future: negative seconds in the text, 0 in `uptime_s`.
    let future = parse_started("2026-10-17T13:00:05Z", now).unwrap();
    assert!(
        future.up.starts_with('-') && future.secs == Some(0),
        "{future:?}"
    );
    // Shapes Python's fromisoformat might also accept but this side does not parse: refused.
    for odd in [
        "2026-10-17 10:00:00Z",
        "2026-10-17T10:00:00+02:00",
        "2026-10-17T10:00:00",
        "x",
    ] {
        assert_eq!(parse_started(odd, now), Err(Fail::Refused), "{odd}");
    }
}

#[test]
fn docker_errors_read_as_docker_py_writes_them() {
    let raw = |status, body: &str| Raw {
        status,
        reason: "Teapot Reason".into(),
        body: body.as_bytes().to_vec(),
    };
    assert_eq!(
        api_error(&raw(404, r#"{"message":"x"}"#), "1.47", "/p"),
        ReadErr::NotFound
    );
    assert_eq!(
        api_error(&raw(500, r#"{"message":"boom"}"#), "1.47", "/containers/a/json"),
        ReadErr::Api(
            "500 Server Error for http+docker://localhost/v1.47/containers/a/json: Teapot Reason (\"boom\")"
                .into()
        )
    );
    assert_eq!(
        api_error(&raw(409, "  plain text \n"), "1.44", "/x"),
        ReadErr::Api(
            "409 Client Error for http+docker://localhost/v1.44/x: Teapot Reason (\"plain text\")"
                .into()
        )
    );
    assert_eq!(
        api_error(&raw(503, ""), "1.47", "/x"),
        ReadErr::Api("503 Server Error for http+docker://localhost/v1.47/x: Teapot Reason".into())
    );
    // `['message']` of a dict without it (KeyError) or of a list (TypeError): not reproduced.
    assert_eq!(
        api_error(&raw(500, r#"{"m":1}"#), "1.47", "/x"),
        ReadErr::Odd
    );
    assert_eq!(api_error(&raw(500, "[1]"), "1.47", "/x"), ReadErr::Odd);
    assert_eq!(quote("a b/c:d%"), "a%20b/c:d%25");
}

/// A stats body that is not JSON: Python's sample ends with requests' JSONDecodeError text; this
/// side gives the generic sampler error (documented).
#[tokio::test]
async fn stats_that_are_not_json_give_the_generic_sampler_error() {
    let f = fixture();
    let case = f["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "stats-edges")
        .unwrap();
    let mut w = case["steps"][0]["docker"].clone();
    let id = w["containers"][0]["Id"].as_str().unwrap().to_owned();
    w["stats"][&id] = V::String("not json".into());
    let dir = tempfile::tempdir().unwrap();
    let stats = BackendStats::with_clock(
        Config {
            configured: vec![],
            gpu_vram: vec![],
            default_port: Some(18230),
            drm_root: dir.path().join("drm"),
            legacy_base: "http://127.0.0.1:9".into(),
            token: String::new(),
            meminfo: "/nonexistent/meminfo".into(),
            ram_reserve_gb: Some(32.0),
        },
        Arc::new(World(Mutex::new(Some(w)))),
        Arc::new(|| 1.0),
    )
    .unwrap();
    let got = stats.collect("127.0.0.1").await;
    assert_eq!(got, Stats::failed(SAMPLER_ERROR));
}

#[test]
fn container_runtime_python_semantics() {
    let parse = |t: &str| model_files::json::parse(t).unwrap();
    // A non-dict cpu_stats: AttributeError, not caught (the sample ends).
    assert!(runtime(&parse(r#"{"cpu_stats": [1]}"#)).is_err());
    // A string counter: TypeError, caught (no runtime figures).
    assert_eq!(
        runtime(&parse(
            r#"{"cpu_stats": {"cpu_usage": {"total_usage": "5"}}}"#
        )),
        Ok(None)
    );
    // online_cpus missing: len(percpu) or 1, evaluated only then.
    let r = runtime(&parse(
        r#"{"cpu_stats": {"cpu_usage": {"total_usage": 30, "percpu_usage": [1, 2, 3]}, "system_cpu_usage": 100},
            "precpu_stats": {"cpu_usage": {"total_usage": 0}, "system_cpu_usage": 0}, "memory_stats": {}}"#,
    ))
    .unwrap()
    .unwrap();
    assert_eq!(r.cpu_pct, Num::Float(90.0));
    // A non-number online count is a TypeError only when it is used.
    assert_eq!(
        runtime(&parse(
            r#"{"cpu_stats": {"online_cpus": "4", "system_cpu_usage": 1}}"#
        )),
        Ok(None)
    );
    assert!(runtime(&parse(r#"{"cpu_stats": {"online_cpus": "4"}}"#))
        .unwrap()
        .is_some());
    // "anon" present as null does not fall back to "rss".
    let r = runtime(&parse(
        r#"{"memory_stats": {"stats": {"anon": null, "rss": 1073741824}}}"#,
    ))
    .unwrap()
    .unwrap();
    assert_eq!(r.mem_anon_gb, None);
}

#[test]
fn gpu_vram_map_follows_settings() {
    assert_eq!(
        gpu_vram_map(" a : 8 , b:x, c, a:+12, d:1_6").unwrap(),
        vec![("a".to_owned(), 12.0), ("d".to_owned(), 16.0)]
    );
    assert!(gpu_vram_map("a:\u{0661}").is_err());
}

// Frozen child-era transport oracle. This adapter exists only in this cfg(test) module;
// production BackendStats has no mirror field, path, switch or sending branch.
mod historical_backend_stats {
    use super::*;
    struct BackendStats {
        native: super::super::BackendStats,
        mirror: reqwest::Client,
        base: String,
        token: String,
    }
    impl std::ops::Deref for BackendStats {
        type Target = super::super::BackendStats;
        fn deref(&self) -> &Self::Target {
            &self.native
        }
    }
    impl BackendStats {
        fn with_clock(
            config: Config,
            docker: Arc<dyn DockerApi>,
            clock: Clock,
        ) -> Result<Self, reqwest::Error> {
            let base = config.legacy_base.clone();
            let token = config.token.clone();
            Ok(Self {
                native: super::super::BackendStats::with_clock(config, docker, clock)?,
                mirror: reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(3))
                    .build()?,
                base,
                token,
            })
        }
        async fn tick(&self) {
            self.native.tick().await;
            let body = self
                .native
                .push_body()
                .expect("the recorded cache is serializable");
            let _ = self
                .mirror
                .put(format!("{}{PUSH_PATH}", self.base))
                .header("x-model-loader-token", &self.token)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn actual_python_differential() {
        let _ports = super::ORACLE_PORTS.lock().await;
        frozen_original_differential().await;
    }

    // Original child-era assertions are kept byte-for-byte behind the synchronized wrapper.
    async fn frozen_original_differential() {
        let f = fixture();
        let mut ticks = 0;
        let mut gets = 0;
        for case in f["cases"].as_array().unwrap() {
            let id = case["id"].as_str().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let drm = dir.path().join("drm");
            let world = Arc::new(World(Mutex::new(None)));
            let fake_child = child().await;
            let now = Arc::new(AtomicU64::new(0));
            let n = now.clone();
            let env = &case["env"];
            let default_port = match env["LLAMA_DEFAULT_PORT"].as_str() {
                Some(p) => Some(p.parse::<u16>().unwrap()).filter(|p| *p != 0),
                None => Some(8080),
            };
            let stats = BackendStats::with_clock(
                Config {
                    configured: configured(env),
                    gpu_vram: gpu_vram_map(env["GPU_VRAM"].as_str().unwrap_or("")).unwrap(),
                    default_port,
                    drm_root: drm.clone(),
                    legacy_base: fake_child.base.clone(),
                    token: "synthetic-token".into(),
                    meminfo: "/nonexistent/meminfo".into(),
                    ram_reserve_gb: Some(32.0),
                },
                world.clone(),
                Arc::new(move || f64::from_bits(n.load(Ordering::SeqCst))),
            )
            .unwrap();
            let seeded: Vec<(String, String)> = case["restart_errors"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                .collect();
            stats.seed_restart_errors(&seeded);
            for (i, step) in case["steps"].as_array().unwrap().iter().enumerate() {
                let ctx = format!("{id} step {i}");
                now.store(step["at"].as_f64().unwrap().to_bits(), Ordering::SeqCst);
                *world.0.lock().unwrap() =
                    (!step["docker"].is_null()).then(|| step["docker"].clone());
                if step["op"] == "tick" {
                    write_sysfs(&drm, &step["sysfs"]);
                    *fake_child.probes.lock().unwrap() = step["probes"].clone();
                    fake_child.pushed.lock().unwrap().clear();
                    stats.tick().await;
                    let pushed = fake_child.pushed.lock().unwrap().clone();
                    assert_eq!(pushed.len(), 1, "{ctx}");
                    let pushed: V = serde_json::from_str(&pushed[0]).unwrap();
                    assert_eq!(pushed["backends"], step["cache"], "{ctx}");
                    ticks += 1;
                } else {
                    let mut servers = Vec::new();
                    if let Some(containers) = step["docker"]["containers"].as_array() {
                        for c in containers {
                            let name = c["Name"].as_str().unwrap()[1..].to_owned();
                            let Some(a) = step["llama"].get(&name).filter(|a| !a.is_null()) else {
                                continue;
                            };
                            let attrs = model_files::json::parse(&c.to_string()).unwrap();
                            let port = crate::backends::internal_port(
                                &spec_of(&attrs).unwrap(),
                                default_port,
                            )
                            .unwrap();
                            servers.push(
                                llama(
                                    port,
                                    a[0].as_u64().unwrap(),
                                    a[1].as_str().unwrap().to_owned(),
                                )
                                .await,
                            );
                        }
                    }
                    let outcome = stats.get().await;
                    stop_llamas(servers).await;
                    match outcome {
                        Outcome::Json(body) => {
                            assert_eq!(step["status"], 200, "{ctx}");
                            assert_eq!(body, step["body"].as_str().unwrap(), "{ctx}");
                        }
                        Outcome::Failed => assert_eq!(step["status"], 500, "{ctx}"),
                        Outcome::Refused => panic!("{ctx}: refused"),
                    }
                    gets += 1;
                }
            }
        }
        assert!(ticks >= 140 && gets >= 12, "{ticks} {gets}");
    }
}
