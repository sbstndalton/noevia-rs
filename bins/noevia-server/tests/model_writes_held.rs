//! Track R11: PUT /api/models/idle-unload with `NOEVIA_RUST_MODEL_WRITES=rust` on a llama.cpp
//! deployment, through the whole front: the model manager's write and the router's reload run under
//! the maintenance hold (a scripted inference gate and router on one port, as in production; a
//! scripted model manager on another). The legacy upstream is dead (tests/maint_support). Synthetic
//! data only; no engine is touched.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use maint_support::*;
use serde_json::{json, Value};
use std::sync::Arc;

const HOLD: &str = "0123456789abcdef0123456789abcdef";

/// The gate grants (or refuses) the hold; the router lists one unloaded model and reloads.
fn gate_and_router(refuse: bool) -> Handler {
    Arc::new(move |req: &Req| {
        match (req.method.as_str(), req.target.as_str()) {
        ("POST", "/gate/v1/maintenance") if refuse => Answer::Reply(
            409,
            json!({"error": {"type": "gate_error", "code": "maintenance_held", "message": "held", "retryable": true}})
                .to_string(),
        ),
        ("POST", "/gate/v1/maintenance") => {
            Answer::Reply(201, json!({"hold": HOLD, "idle_ttl_ms": 600_000}).to_string())
        }
        ("DELETE", t) if t.starts_with("/gate/v1/") => Answer::Reply(204, String::new()),
        ("GET", "/models") => Answer::Reply(
            200,
            json!({"data": [{"id": "qa-model", "status": {"value": "unloaded", "preset": "p"}}]})
                .to_string(),
        ),
        ("GET", "/models?reload=1") => Answer::Reply(200, r#"{"success":true}"#.into()),
        _ => Answer::Reply(404, "{}".into()),
    }
    })
}

fn loader() -> Handler {
    Arc::new(
        |req: &Req| match (req.method.as_str(), req.target.as_str()) {
            ("GET", "/api/v1/idle-unload") => {
                Answer::Reply(200, r#"{"seconds":1800,"source":"default"}"#.into())
            }
            ("PUT", "/api/v1/idle-unload") => Answer::Reply(
                200,
                r#"{"seconds":600,"source":"setting","preset":"written"}"#.into(),
            ),
            _ => Answer::Reply(404, "{}".into()),
        },
    )
}

async fn front(
    dir: &std::path::Path,
    engine: u16,
    loader: u16,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let ini = dir.join("models.ini");
    std::fs::write(
        &ini,
        "[*]\nsleep-idle-seconds = 600\n\n[qa-model]\nmodel = /models/qa.gguf\n",
    )
    .unwrap();
    start(
        dir,
        &[
            ("NOEVIA_RUST_MODEL_WRITES".into(), "rust".into()),
            ("MODEL_MANAGER_KIND".into(), "llamacpp".into()),
            (
                "MODEL_MANAGER_BASE_URL".into(),
                format!("http://127.0.0.1:{engine}/v1"),
            ),
            ("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()),
            (
                "INFERENCE_GATE_ADMIN_TOKEN".into(),
                "synthetic-admin-token".into(),
            ),
            (
                "MODEL_LOADER_URL".into(),
                format!("http://127.0.0.1:{loader}"),
            ),
        ],
    )
    .await
}

fn targets(seen: &std::sync::Mutex<Vec<Req>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|r| format!("{} {}", r.method, r.target))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_write_and_its_reload_run_under_one_hold() {
    let d = tempfile::tempdir().unwrap();
    let db = seed_database(d.path());
    let (engine_port, engine_seen) = engine(gate_and_router(false)).await;
    let (loader_port, loader_seen) = engine(loader()).await;
    let (addr, stop) = front(d.path(), engine_port, loader_port).await;
    let (status, _, body) = call(
        addr,
        "PUT",
        "/api/models/idle-unload",
        Some(ADMIN),
        br#"{"seconds":600}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["seconds"], 600);
    assert_eq!(body["apply"], json!({"state": "applied"}), "{body}");
    assert!(body["applied"].is_boolean(), "{body}");
    assert_eq!(
        targets(&loader_seen),
        ["GET /api/v1/idle-unload", "PUT /api/v1/idle-unload"]
    );
    let engine_calls = targets(&engine_seen);
    let at = |t: &str| {
        engine_calls
            .iter()
            .position(|c| c == t)
            .unwrap_or_else(|| panic!("{t}: {engine_calls:?}"))
    };
    // The hold first, then the router's reload, then the hold given back.
    assert!(
        at("POST /gate/v1/maintenance") < at("GET /models?reload=1"),
        "{engine_calls:?}"
    );
    assert!(
        at("GET /models?reload=1") < at(&format!("DELETE /gate/v1/maintenance/{HOLD}")),
        "{engine_calls:?}"
    );
    let audits: i64 = db
        .query_row(
            "SELECT count(*) FROM audit_events WHERE action='models.idle_unload'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audits, 1);
    let _ = stop.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_hold_changes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let db = seed_database(d.path());
    let (engine_port, engine_seen) = engine(gate_and_router(true)).await;
    let (loader_port, loader_seen) = engine(loader()).await;
    let (addr, stop) = front(d.path(), engine_port, loader_port).await;
    let (status, _, body) = call(
        addr,
        "PUT",
        "/api/models/idle-unload",
        Some(ADMIN),
        br#"{"seconds":600}"#,
    )
    .await;
    assert_eq!(status, 409, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        body,
        json!({"error": "Models are busy, so the idle unload time was not changed. Try again shortly.", "errorId": "idleUnloadBusy"})
    );
    // The previous value was read (for the audit); nothing was written, reloaded or audited.
    assert_eq!(targets(&loader_seen), ["GET /api/v1/idle-unload"]);
    assert!(
        !targets(&engine_seen)
            .iter()
            .any(|t| t.starts_with("GET /models")),
        "{:?}",
        targets(&engine_seen)
    );
    let audits: i64 = db
        .query_row(
            "SELECT count(*) FROM audit_events WHERE action='models.idle_unload'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audits, 0);
    let _ = stop.send(());
}
