//! A synthetic llama.cpp router and inference gate for the native calibration worker behind the real
//! front (#2303), with the GGUF file and models.ini of tests/fixtures/calibration-prepare-node.json's
//! first case. No engine or model is touched; the long prompt stalls until the client goes away, so a
//! launched job stays running until it is cancelled.
#![allow(dead_code)]
use axum::body::Body;
use axum::http::{Request, Response};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const MODEL: &str = "synth";
pub const FILE: &str = "Synth-8B-Q4_K_M.gguf";
pub const INI: &str =
    "[*]\nn-gpu-layers = 99\n\n[synth]\nmodel = /models/Synth-8B-Q4_K_M.gguf\nctx-size = 4096\n";
pub const LAYA_INI: &str = "\n[laya-mini]\nmodel = /models/laya-mini.gguf\n";

/// The fixture's GGUF header, written in the oracle script's key order.
pub fn gguf() -> Vec<u8> {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/calibration-prepare-node.json")).unwrap();
    let header = fixture["header"].as_object().unwrap();
    const ORDER: [&str; 7] = [
        "general.architecture",
        "llama.context_length",
        "llama.embedding_length",
        "llama.block_count",
        "llama.attention.head_count",
        "llama.attention.head_count_kv",
        "tokenizer.chat_template",
    ];
    let mut out = b"GGUF".to_vec();
    out.extend(3u32.to_le_bytes());
    out.extend(0u64.to_le_bytes());
    out.extend((header.len() as u64).to_le_bytes());
    let s = |out: &mut Vec<u8>, t: &str| {
        out.extend((t.len() as u64).to_le_bytes());
        out.extend(t.as_bytes());
    };
    for k in ORDER {
        let v = &header[k];
        s(&mut out, k);
        let t = v[0].as_u64().unwrap() as u32;
        out.extend(t.to_le_bytes());
        if t == 4 {
            out.extend((v[1].as_u64().unwrap() as u32).to_le_bytes());
        } else {
            s(&mut out, v[1].as_str().unwrap());
        }
    }
    out
}

/// What the engine answers, and what it was asked.
#[derive(Default)]
pub struct Engine {
    pub port: u16,
    /// `/models` answers 502.
    pub down: AtomicBool,
    /// The row's `meta.n_ctx_train`.
    pub native: AtomicU64,
    /// The gate refuses the maintenance hold: (status, code, message).
    pub refuse: Mutex<Option<(u16, String, String)>>,
    /// `GET /props` answers this late (ms).
    pub props_delay_ms: AtomicU64,
    /// `GET /models?reload=1` answers this late (ms).
    pub reload_delay_ms: AtomicU64,
    /// Other models live besides the two the profile names (`extra-<n>`, loaded).
    pub extra_live: AtomicU64,
    pub seen: Mutex<Vec<String>>,
}

impl Engine {
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
    pub fn count(&self, prefix: &str) -> usize {
        self.seen().iter().filter(|l| l.starts_with(prefix)).count()
    }
}

fn reply(code: u16, v: Value) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

/// The router and gate on one port; `ini` is read for the context size each load gets.
pub async fn engine(ini: PathBuf) -> Arc<Engine> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let e = Arc::new(Engine {
        port: listener.local_addr().unwrap().port(),
        native: AtomicU64::new(32768),
        ..Engine::default()
    });
    let status = Arc::new(Mutex::new("unloaded".to_owned()));
    let state = Arc::clone(&e);
    let app = axum::Router::new().fallback(move |req: Request<Body>| {
        let (e, status, ini) = (Arc::clone(&state), Arc::clone(&status), ini.clone());
        async move {
            let (parts, body) = req.into_parts();
            let body = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            e.seen
                .lock()
                .unwrap()
                .push(format!("{} {}", parts.method, parts.uri));
            let ctx = || -> f64 {
                std::fs::read_to_string(&ini)
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|l| l.strip_prefix("ctx-size = "))
                    .next_back()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0)
            };
            match (parts.method.as_str(), parts.uri.path()) {
                ("POST", "/gate/v1/maintenance") => {
                    if let Some((code, kind, message)) = e.refuse.lock().unwrap().clone() {
                        return reply(
                            code,
                            json!({"error": {"type": "gate_error", "code": kind, "message": message, "retryable": false}}),
                        );
                    }
                    reply(
                        201,
                        json!({"hold": "0123456789abcdef0123456789abcdef", "idle_ttl_ms": 60000}),
                    )
                }
                ("GET", "/gate/v1/status") => reply(200, json!({"maintenance": {"held": false}})),
                ("DELETE", _) => Response::builder().status(204).body(Body::empty()).unwrap(),
                (_, p) if p.starts_with("/gate/") => reply(200, json!({})),
                ("GET", "/models") => {
                    if parts.uri.query() == Some("reload=1") {
                        let delay = e.reload_delay_ms.load(Ordering::Acquire);
                        if delay > 0 {
                            tokio::time::sleep(Duration::from_millis(delay)).await;
                        }
                    }
                    if e.down.load(Ordering::Acquire) {
                        return reply(502, json!({}));
                    }
                    let v = status.lock().unwrap().clone();
                    let native = e.native.load(Ordering::Acquire);
                    let mut rows = vec![
                        json!({"id": MODEL, "status": {"value": v, "args": ["--model", format!("/models/{FILE}")]}, "meta": {"n_ctx_train": native}}),
                        json!({"id": "laya-mini", "status": {"value": "unloaded", "args": ["--model", "/models/laya-mini.gguf"]}}),
                    ];
                    rows.extend((0..e.extra_live.load(Ordering::Acquire)).map(
                        |n| json!({"id": format!("extra-{n}"), "status": {"value": "loaded"}}),
                    ));
                    reply(200, json!({ "data": rows }))
                }
                ("POST", "/models/unload") => {
                    *status.lock().unwrap() = "unloaded".into();
                    reply(200, json!({"success": true}))
                }
                ("POST", "/models/load") => {
                    *status.lock().unwrap() = if ctx() <= 16384.0 {
                        "loaded"
                    } else {
                        "failed"
                    }
                    .into();
                    reply(200, json!({"success": true}))
                }
                ("GET", "/props") => {
                    let delay = e.props_delay_ms.load(Ordering::Acquire);
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    reply(200, json!({"build_info": "b1234"}))
                }
                ("POST", "/tokenize") => reply(200, json!({"tokens": vec![1; 650]})),
                ("POST", "/v1/chat/completions") if body["stream"] == Value::Bool(true) => {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    reply(500, json!({}))
                }
                ("POST", "/v1/chat/completions") => {
                    reply(200, json!({"choices": [{"message": {"content": "OK"}}]}))
                }
                _ => reply(404, json!({"error": "not found"})),
            }
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await });
    e
}

/// `dir/models` with the model files, `dir/models.ini`; the engine; the front's environment for a
/// llama.cpp manager over them (`gate`: the inference gate's admin token, else Node's lease).
pub async fn native(dir: &Path, gate: bool) -> (Arc<Engine>, Vec<(&'static str, String)>) {
    let models = dir.join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::fs::write(models.join(FILE), gguf()).unwrap();
    std::fs::write(models.join("laya-mini.gguf"), gguf()).unwrap();
    std::fs::write(dir.join("models.ini"), format!("{INI}{LAYA_INI}")).unwrap();
    let e = engine(dir.join("models.ini")).await;
    let mut env = vec![
        ("MODEL_MANAGER_KIND", "llamacpp".to_owned()),
        ("MODEL_MANAGER_BASE_URL", e.base()),
        (
            "LLAMACPP_PRESET_PATH",
            dir.join("models.ini").display().to_string(),
        ),
        ("LLAMACPP_MODELS_PATH", models.display().to_string()),
    ];
    if gate {
        env.push(("INFERENCE_GATE_ADMIN_TOKEN", "synthetic-admin".to_owned()));
    }
    (e, env)
}

/// The worker's state file for this engine: `native-calibration-<sha256(base)[..16]>.json`.
pub fn state_file(dir: &Path, e: &Engine) -> PathBuf {
    let hash: String = Sha256::digest(e.base().as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    dir.join(format!("native-calibration-{hash}.json"))
}

pub fn ini(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("models.ini")).unwrap()
}
