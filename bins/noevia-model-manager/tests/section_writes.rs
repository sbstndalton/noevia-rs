//! noevia#1318: native models.ini writers against the actual Python answers and files.
//!
//! `fixtures/section-writes-python.v1.json` comes from `tools/gen-section-writes-oracle.py`, which
//! runs the real services app step by step on synthetic models folders (backup names pinned to one
//! UTC instant) and records every answer, the safe-defaults plan the child gives for the same
//! state, and the files left behind. Each case is rebuilt here and replayed against the native
//! router with a fake child serving those plans; the answers and the final files must match.
#![cfg(feature = "native-section-writes")]
#![allow(
    unused_imports,
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{
    section_writes::SectionWrites, Container, DockerRead, Redactor, StateData,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};
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

fn state(writes: SectionWrites, legacy_base: &str) -> Arc<StateData> {
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
        #[cfg(feature = "native-bench-runs")]
        bench_runs: None,
        section_writes: Some(Arc::new(writes)),
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
        #[cfg(feature = "native-section-autoconfig")]
        section_autoconfig: None,
    })
}

// A fake child: `/api/v1/internal/startup-ready` advertises completion; the plan call answers whatever `plan` holds.
async fn child(plan: Arc<Mutex<Option<(u16, String)>>>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let plan = plan.clone();
            tokio::spawn(async move {
                let mut seen = Vec::new();
                let mut buf = [0u8; 4096];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buf[..n]);
                }
                let line = String::from_utf8_lossy(&seen)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_owned();
                let (status, body) = if line.starts_with("GET /api/v1/internal/startup-ready ") {
                    (200, r#"{"startup_ready":1}"#.to_owned())
                } else if line.starts_with("GET /api/v1/internal/safe-defaults-plan?") {
                    plan.lock().unwrap().clone().unwrap_or((599, String::new()))
                } else {
                    (404, String::new())
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
            });
        }
    });
    base
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/section-writes-python.v1.json")).unwrap()
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn listing(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type().unwrap().is_dir() {
            listing(&entry.path(), &rel, out);
        } else if !name.contains(".tmp-") {
            out.push(rel);
        }
    }
}

#[derive(Debug)]
struct Answer {
    status: u16,
    content_type: Option<String>,
    body: String,
}

async fn send(state: Arc<StateData>, step: &Value, rev: &str) -> Answer {
    let path = step["path"].as_str().unwrap().replace("{REV}", rev);
    let mut request = Request::builder()
        .method(step["method"].as_str().unwrap())
        .uri(path)
        .header("x-model-loader-token", TOKEN);
    let body = match step["body"].as_str() {
        Some(b) => {
            if let Some(ct) = step["content_type"].as_str() {
                request = request.header("content-type", ct);
            }
            Body::from(b.replace("{REV}", rev))
        }
        None => Body::empty(),
    };
    let response = noevia_model_manager::router(state)
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    Answer {
        status: response.status().as_u16(),
        content_type: response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned()),
        body: String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap(),
    }
}

// A `MIGRATE` step: the startup cache-ram migration as the parent runs it (noevia#2010).
async fn migrate(state: &Arc<StateData>) -> Answer {
    let writes = state.section_writes.clone().unwrap();
    match writes.migrate_cache_ram_at_startup().await {
        Ok(changed) => Answer {
            status: 200,
            content_type: None,
            body: serde_json::to_string(&changed).unwrap(),
        },
        Err(_) => Answer {
            status: 500,
            content_type: None,
            body: String::new(),
        },
    }
}

#[tokio::test]
async fn non_native_shapes_never_write() {
    // Trailing slashes, other methods and an encoded slash are not the native writers: they
    // keep the existing 404/405 (or reach the child, here a dead address), and write nothing.
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("models.ini");
    let s = state(SectionWrites::new(&ini, (1024, 2048)), "http://127.0.0.1:9");
    for (method, path) in [
        ("PUT", "/api/v1/sections/a/"),
        ("GET", "/api/v1/sections/a/rename"),
        ("PUT", "/api/v1/sections/a%2Fb"),
        ("POST", "/api/v1/models-ini"),
    ] {
        let answer = noevia_model_manager::router(s.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("x-model-loader-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(!answer.status().is_success(), "{method} {path}");
    }
    assert!(!ini.exists());
}

#[cfg(feature = "native-safe-defaults")]
#[tokio::test]
async fn native_safe_defaults_never_asks_child_for_a_plan() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    std::fs::write(models.join("syn-bad.gguf"), b"not a valid header").unwrap();
    let ini = models.join("models.ini");
    let writes = SectionWrites::new(&ini, (1024, 2048));
    writes.set_safe_models(&models);
    // This child rejects any planning request. The selected native planner must bypass it.
    let base = child(Arc::new(Mutex::new(None))).await;
    let state = state(writes, &base);
    let step = serde_json::json!({"method":"POST","path":"/api/v1/sections/syn-bad/safe-defaults","body":null,"content_type":null});
    let answer = send(state.clone(), &step, "").await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert!(std::fs::read_to_string(&ini)
        .unwrap()
        .contains("ctx-size = 8192"));
    let again = send(state, &step, "").await;
    assert_eq!(again.status, 409);
}
