//! noevia#1318: native backend restart and test against the actual Python answers.
//!
//! `fixtures/backend-control-python.v1.json` comes from `tools/gen-backend-control-oracle.py`: the
//! real services app with a fake Docker client and a fake llama-server on 127.0.0.1 (the backend
//! is named `127.0.0.1`), `time.time` pinned inside `test_prompt`. Here the same fake Docker answers
//! through `DockerRead`, a fake llama-server serves each step's responses, and a fake child
//! records the restart-error calls. No Docker daemon, container or model is touched.
#![cfg(feature = "native-backend-control")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{
    backends::Backends, BackendSpec, Container, DockerRead, Redactor, StateData,
};
use serde_json::Value;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
};
use tower::ServiceExt;

const TOKEN: &str = "synthetic-model-loader-token-0123456789abcdef";
const NAME: &str = "127.0.0.1";

type Job<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;
struct FakeDocker {
    spec: Option<BackendSpec>,
    restart_error: Option<String>,
}
impl DockerRead for FakeDocker {
    fn containers(&self) -> Job<'_, Vec<Container>> {
        Box::pin(async {
            Ok(vec![Container {
                name: NAME.into(),
                first_image_tag: "ghcr.io/ggml-org/llama.cpp:server".into(),
            }])
        })
    }
    fn logs<'a>(&'a self, _: &'a str, _: usize) -> Job<'a, Vec<u8>> {
        Box::pin(async { Err("not used") })
    }
    fn backend_spec<'a>(&'a self, name: &'a str) -> Job<'a, Option<BackendSpec>> {
        let spec = (name == NAME).then(|| self.spec.clone()).flatten();
        Box::pin(async move { Ok(spec) })
    }
    fn restart_backend<'a>(&'a self, _: &'a str) -> Job<'a, ()> {
        let error = self.restart_error.clone();
        Box::pin(async move {
            match error {
                // The fixture's Python fake raises APIError("<this> (HTTP 500)").
                Some(_) => Err("Docker refused the restart (HTTP 500)"),
                None => Ok(()),
            }
        })
    }
}

fn state(docker: FakeDocker, _legacy_trap_base: &str, backends: Backends) -> Arc<StateData> {
    Arc::new(StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis: None,
        prompts: None,
        #[cfg(feature = "native-settings")]
        settings: None,
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
        #[cfg(feature = "native-backends")]
        backend_stats: Some(Arc::new(
            noevia_model_manager::backend_stats::BackendStats::new(
                noevia_model_manager::backend_stats::Config {
                    configured: vec![NAME.into()],
                    gpu_vram: vec![],
                    default_port: Some(8080),
                    drm_root: std::path::PathBuf::from("/qa-absent-drm"),
                    meminfo: std::path::PathBuf::from("/qa-absent-meminfo"),
                    ram_reserve_gb: Some(32.0),
                },
                Arc::new(noevia_model_manager::docker::UnixDocker {
                    socket: std::path::PathBuf::from("/qa-absent-docker.sock"),
                }),
            )
            .unwrap(),
        )),
        #[cfg(feature = "native-benchmark-runner")]
        bench_runner: None,
        backends: Some(Arc::new(backends)),
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
        #[cfg(feature = "native-bench-runs")]
        bench_runs: None,
        #[cfg(feature = "native-section-writes")]
        section_writes: None,
        sections: None,
        embedding: None,
        host: None,
        token: TOKEN.into(),
        configured: vec![],
        docker: Arc::new(docker),
        redactor: Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(4),
        idle_unload: None,
        #[cfg(feature = "native-section-autoconfig")]
        section_autoconfig: None,
    })
}

type Shared<T> = Arc<Mutex<T>>;

/// A tiny HTTP server: answers GET with `models`, POST with `chat` (recording the JSON body), and
/// PUT (the child's restart-error call) by recording its body.
async fn server(
    models: Shared<(u16, String)>,
    chat: Shared<(u16, String)>,
    requests: Shared<Vec<Value>>,
    restart: Shared<Vec<Value>>,
) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (models, chat, requests, restart) = (
                models.clone(),
                chat.clone(),
                requests.clone(),
                restart.clone(),
            );
            tokio::spawn(async move {
                let mut seen = Vec::new();
                let mut buf = [0u8; 8192];
                let head_end = loop {
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    if let Some(i) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&seen[..head_end]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap_or(0))
                    })
                    .unwrap_or(0);
                while seen.len() < head_end + len {
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    seen.extend_from_slice(&buf[..n]);
                }
                let body = &seen[head_end..];
                let (status, answer) = if head.starts_with("get ") {
                    models.lock().unwrap().clone()
                } else if head.starts_with("post ") {
                    requests
                        .lock()
                        .unwrap()
                        .push(serde_json::from_slice(body).unwrap());
                    chat.lock().unwrap().clone()
                } else {
                    restart
                        .lock()
                        .unwrap()
                        .push(serde_json::from_slice(body).unwrap());
                    (200, r#"{"ok":true}"#.to_owned())
                };
                let reason = match status {
                    200 => "OK",
                    500 => "Internal Server Error",
                    503 => "Service Unavailable",
                    _ => "X",
                };
                let out = format!("HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}", answer.len());
                let _ = socket.write_all(out.as_bytes()).await;
            });
        }
    });
    port
}

fn fill(v: &Value, port: u16) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace("{PORT}", &port.to_string())),
        Value::Array(a) => Value::Array(a.iter().map(|x| fill(x, port)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.replace("{PORT}", &port.to_string()), fill(x, port)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn spec(v: &Value, port: u16) -> Option<BackendSpec> {
    let v = fill(v, port);
    let Value::Object(o) = v else { return None };
    let strings = |k: &str| {
        o[k].as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_owned())
            .collect()
    };
    let mut ports: Vec<(String, Vec<String>)> = o["ports"]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| (k.clone(), vec![]))
        .collect();
    ports.sort();
    Some(BackendSpec {
        ports,
        cmd: strings("cmd"),
        entrypoint: strings("entrypoint"),
        env: strings("env"),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_python_differential() {
    let fx: Value =
        serde_json::from_str(include_str!("fixtures/backend-control-python.v1.json")).unwrap();
    let models = Arc::new(Mutex::new((200, String::new())));
    let chat = Arc::new(Mutex::new((200, String::new())));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let restart = Arc::new(Mutex::new(Vec::new()));
    let port = server(
        models.clone(),
        chat.clone(),
        requests.clone(),
        restart.clone(),
    )
    .await;
    let child = format!("http://127.0.0.1:{port}");
    for case in fx["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        for (i, (st, want)) in case["steps"]
            .as_array()
            .unwrap()
            .iter()
            .zip(case["answers"].as_array().unwrap())
            .enumerate()
        {
            let pair = |v: &Value| {
                (
                    u16::try_from(v[0].as_u64().unwrap()).unwrap(),
                    v[1].as_str().unwrap().to_owned(),
                )
            };
            *models.lock().unwrap() = pair(&st["llama"]["models"]);
            *chat.lock().unwrap() = pair(&st["llama"]["chat"]);
            requests.lock().unwrap().clear();
            restart.lock().unwrap().clear();
            let ticks = Arc::new(AtomicU32::new(0));
            let clock = {
                let ticks = ticks.clone();
                Arc::new(move || {
                    if ticks.fetch_add(1, Ordering::SeqCst) == 0 {
                        1000.0
                    } else {
                        1000.5
                    }
                })
            };
            let docker = FakeDocker {
                spec: spec(&case["spec"], port),
                restart_error: st["restart_error"].as_str().map(str::to_owned),
            };
            let s = state(
                docker,
                &child,
                Backends::with_clock(Some(8080), clock).unwrap(),
            );
            let mut request = Request::builder()
                .method("POST")
                .uri(st["path"].as_str().unwrap())
                .header("x-model-loader-token", TOKEN);
            let body = match st["body"].as_str() {
                Some(b) => {
                    request = request.header("content-type", "application/json");
                    Body::from(b.to_owned())
                }
                None => Body::empty(),
            };
            let response = noevia_model_manager::router(s.clone())
                .oneshot(request.body(body).unwrap())
                .await
                .unwrap();
            let status = response.status().as_u16();
            let ct = response
                .headers()
                .get("content-type")
                .map(|v| v.to_str().unwrap().to_owned());
            let got = String::from_utf8(
                response
                    .into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .to_vec(),
            )
            .unwrap()
            .replace(&port.to_string(), "{PORT}");
            assert_eq!(
                u64::from(status),
                want["status"].as_u64().unwrap(),
                "{id} step {i}: {got}"
            );
            assert_eq!(
                ct.as_deref(),
                want["content_type"].as_str(),
                "{id} step {i}"
            );
            assert_eq!(got, want["body"].as_str().unwrap(), "{id} step {i}");
            assert_eq!(
                Value::Array(requests.lock().unwrap().clone()),
                want["chat_requests"],
                "{id} step {i}: chat requests"
            );
            // The same recorded restart state now belongs to the native cache.
            #[cfg(feature = "native-backends")]
            {
                let stats = s.backend_stats.as_ref().unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while stats.restart_error(NAME).as_deref() != want["restart_error"].as_str() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "{id} step {i}: native restart state {:?}",
                        stats.restart_error(NAME)
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
            assert!(
                restart.lock().unwrap().is_empty(),
                "retired restart mirror was called"
            );
        }
    }
}

#[test]
fn max_tokens_follows_int_and_refuses_what_python_500s() {
    use noevia_model_manager::max_tokens;
    let j = |t: &str| noevia_model_manager::prompts::parse_json(t).unwrap();
    for (text, want) in [
        ("0", Some(256)),
        ("null", Some(256)),
        ("99999", Some(4096)),
        ("-5", Some(1)),
        ("2.9", Some(2)),
        ("\" 7 \"", Some(7)),
        ("true", Some(1)),
        ("\"abc\"", None),
        ("[1]", None),
        ("1e400", None),
    ] {
        assert_eq!(max_tokens(Some(&j(text))), want, "{text}");
    }
}

/// noevia#1318 step 2: a failed restart is kept in the native sampler's store, and nothing is sent
/// to a child: there is no second copy that could go stale.
#[cfg(feature = "native-backends")]
#[tokio::test]
async fn native_sampler_owns_restart_errors_without_child_mirrors() {
    use noevia_model_manager::backend_stats::{BackendStats, Config};
    let fx: Value =
        serde_json::from_str(include_str!("fixtures/backend-control-python.v1.json")).unwrap();
    let (case, step) = fx["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|c| {
            c["steps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| {
                    s["restart_error"].is_string()
                        && s["path"].as_str().unwrap().ends_with("/restart")
                })
                .map(|s| (c, s))
        })
        .expect("a fixture step whose restart fails");
    let models = Arc::new(Mutex::new((200, String::new())));
    let chat = Arc::new(Mutex::new((200, String::new())));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let restart = Arc::new(Mutex::new(Vec::new()));
    let port = server(models, chat, requests, restart.clone()).await;
    let child = format!("http://127.0.0.1:{port}");
    let dir = tempfile::tempdir().unwrap();
    let stats = Arc::new(
        BackendStats::new(
            Config {
                configured: vec![NAME.into()],
                gpu_vram: vec![],
                default_port: Some(8080),
                drm_root: dir.path().join("drm"),
                meminfo: dir.path().join("meminfo"),
                ram_reserve_gb: Some(32.0),
            },
            Arc::new(noevia_model_manager::docker::UnixDocker {
                socket: dir.path().join("absent.sock"),
            }),
        )
        .unwrap(),
    );
    stats.seed_restart_errors(&[(NAME.to_owned(), "APIError: an older failure".to_owned())]);
    let docker = FakeDocker {
        spec: spec(&case["spec"], port),
        restart_error: step["restart_error"].as_str().map(str::to_owned),
    };
    let mut s = Arc::into_inner(state(
        docker,
        &child,
        Backends::with_clock(Some(8080), Arc::new(|| 1000.0)).unwrap(),
    ))
    .unwrap();
    s.backend_stats = Some(stats.clone());
    let response = noevia_model_manager::router(Arc::new(s))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(step["path"].as_str().unwrap())
                .header("x-model-loader-token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while stats.restart_error(NAME).as_deref() == Some("APIError: an older failure")
        || stats.restart_error(NAME).is_none()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the mirror was never set"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let mirrored = stats.restart_error(NAME).unwrap();
    assert!(mirrored.starts_with("APIError: "));
    assert!(
        restart.lock().unwrap().is_empty(),
        "retired restart mirror was called"
    );
}

/// The sampler's Docker for the restart-error routes: the daemon answers, and the one configured
/// backend has no container, so its row carries only what the sampler's store says about it.
#[cfg(feature = "native-backends")]
struct EmptyDaemon;
#[cfg(feature = "native-backends")]
impl noevia_model_manager::backend_stats::DockerApi for EmptyDaemon {
    fn get<'a>(&'a self, path: &'a str) -> Job<'a, noevia_model_manager::backend_stats::Raw> {
        let (status, reason, body) = if path == "/version" {
            (200, "OK", r#"{"ApiVersion":"1.47"}"#)
        } else if path.starts_with("/containers/json") {
            (200, "OK", "[]")
        } else {
            (404, "Not Found", r#"{"message":"No such container"}"#)
        };
        Box::pin(async move {
            Ok(noevia_model_manager::backend_stats::Raw {
                status,
                reason: reason.to_owned(),
                body: body.as_bytes().to_vec(),
            })
        })
    }
}

/// What `GET /api/v1/backends` and the overview's backend list report for the backend.
#[cfg(feature = "native-backends")]
async fn reported_restart_errors(
    s: &Arc<StateData>,
    stats: &noevia_model_manager::backend_stats::BackendStats,
) -> (Value, Value) {
    let response = noevia_model_manager::router(s.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/backends")
                .header("x-model-loader-token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let backends: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(backends["backends"].as_array().unwrap().len(), 1);
    assert_eq!(backends["backends"][0]["name"], NAME);
    // The overview's backend list is this runtime body (`list.overview` asks nothing else).
    let overview: Value =
        serde_json::from_slice(&stats.overview_runtime(0).await.unwrap_or_default()).unwrap();
    assert_eq!(overview["backends"].as_array().unwrap().len(), 1);
    assert_eq!(overview["backends"][0]["name"], NAME);
    (
        backends["backends"][0]["last_restart_error"].clone(),
        overview["backends"][0]["last_restart_error"].clone(),
    )
}

#[cfg(feature = "native-backends")]
async fn post_restart(s: Arc<StateData>) {
    let response = noevia_model_manager::router(s)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/backends/{NAME}/restart"))
                .header("x-model-loader-token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        r#"{"ok":true,"message":"restart queued"}"#
    );
}

/// noevia#252: `GET /api/v1/backends` and the overview's backend list read `last_restart_error`
/// from the native sampler's store, the source of truth. They report the error of a failed
/// restart (the store's own text, not an older one) and report none once a restart succeeds.
#[cfg(feature = "native-backends")]
#[tokio::test]
async fn backends_and_overview_report_the_samplers_restart_error() {
    use noevia_model_manager::backend_stats::{BackendStats, Config};
    let dir = tempfile::tempdir().unwrap();
    let stats = Arc::new(
        BackendStats::new(
            Config {
                configured: vec![NAME.into()],
                gpu_vram: vec![],
                default_port: Some(8080),
                drm_root: dir.path().join("drm"),
                meminfo: dir.path().join("meminfo"),
                ram_reserve_gb: Some(32.0),
            },
            Arc::new(EmptyDaemon),
        )
        .unwrap(),
    );
    let spec = BackendSpec {
        ports: vec![],
        cmd: vec![],
        entrypoint: vec![],
        env: vec![],
    };
    let with_restart = |restart_error: Option<&str>| {
        let docker = FakeDocker {
            spec: Some(spec.clone()),
            restart_error: restart_error.map(str::to_owned),
        };
        let mut s = Arc::into_inner(state(
            docker,
            "http://127.0.0.1:9",
            Backends::with_clock(Some(8080), Arc::new(|| 1000.0)).unwrap(),
        ))
        .unwrap();
        s.backend_stats = Some(stats.clone());
        Arc::new(s)
    };
    let older = "APIError: an older failure";
    stats.seed_restart_errors(&[(NAME.to_owned(), older.to_owned())]);
    let (backends, overview) = reported_restart_errors(&with_restart(None), &stats).await;
    assert_eq!(backends, older, "the seeded store is what is reported");
    assert_eq!(overview, older);

    // A failed restart: the older entry is cleared, then the failure is stored and reported.
    let failing = with_restart(Some("synthetic refusal"));
    post_restart(failing.clone()).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let stored = loop {
        match stats.restart_error(NAME) {
            Some(e) if e != older => break e,
            _ => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the failure was never stored"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    };
    assert!(stored.starts_with("APIError: "), "{stored}");
    let (backends, overview) = reported_restart_errors(&failing, &stats).await;
    assert_eq!(backends, stored);
    assert_eq!(overview, stored);

    // A successful restart clears the entry before it is queued and sets nothing afterwards.
    let working = with_restart(None);
    post_restart(working.clone()).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(stats.restart_error(NAME), None);
    let (backends, overview) = reported_restart_errors(&working, &stats).await;
    assert_eq!(backends, Value::Null);
    assert_eq!(overview, Value::Null);
}
