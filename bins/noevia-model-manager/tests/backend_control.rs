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

fn state(docker: FakeDocker, child: &str, backends: Backends) -> Arc<StateData> {
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
        backends: Some(Arc::new(backends)),
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
        sections: None,
        embedding: None,
        host: None,
        token: TOKEN.into(),
        configured: vec![],
        docker: Arc::new(docker),
        redactor: Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(4),
        legacy: noevia_model_manager::legacy_client().unwrap(),
        legacy_base: child.into(),
        idle_unload: None,
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
            let response = noevia_model_manager::router(s)
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
            // The restart error the child ends up holding: the last set/clear call after the task ran.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let calls = restart.lock().unwrap().clone();
                let expected = want["restart_error"].as_str();
                let last = calls.last().map(|c| c["error"].as_str().map(str::to_owned));
                let done = match expected {
                    Some(e) => last == Some(Some(e.to_owned())),
                    // Only a queued restart clears the error first.
                    None => !got.contains("restart queued") || last == Some(None),
                };
                if done {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{id} step {i}: restart calls {calls:?}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
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
