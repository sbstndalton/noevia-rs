//! sbstndalton/noevia#1747: idle chat models unload themselves through the chat preset's global
//! `sleep-idle-seconds` key, which the model manager keeps in place across Python writes.
#![allow(
    unused_imports,
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{
    idle_unload::{self, Reconciler},
    Container, DockerRead, Job, Redactor, StateData,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc};
use tower::ServiceExt;

struct NoDocker;
impl DockerRead for NoDocker {
    fn containers(&self) -> Job<'_, Vec<Container>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn logs<'a>(&'a self, _: &'a str, _: usize) -> Job<'a, Vec<u8>> {
        Box::pin(async { Ok(vec![]) })
    }
}

fn revision(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// A fake Python editor: PUT writes the text it is given verbatim (as `write_raw_text` does) and
// answers with that text's revision; `fail` makes it refuse with 409 without writing.
async fn fake_python(ini: PathBuf, text: &'static str, fail: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let put_ini = ini.clone();
    let app = axum::Router::new().route(
        "/api/v1/models-ini",
        axum::routing::put(move || {
            let ini = put_ini.clone();
            async move {
                if fail {
                    return (
                        axum::http::StatusCode::CONFLICT,
                        axum::Json(serde_json::json!({"detail":"models.ini changed"})),
                    );
                }
                std::fs::write(&ini, text).unwrap();
                (
                    axum::http::StatusCode::OK,
                    axum::Json(serde_json::json!({"ok":true,"revision":revision(text.as_bytes())})),
                )
            }
        }),
    )
    // A read (GET /api/v1/models is Python-owned) answers with the current file's revision.
    .route(
        "/api/v1/models",
        axum::routing::get(move || {
            let ini = ini.clone();
            async move {
                let raw = std::fs::read(&ini).unwrap();
                axum::Json(serde_json::json!({"text":String::from_utf8(raw.clone()).unwrap(),"revision":revision(&raw)}))
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    base
}

fn state(_legacy_trap_base: &str, reconciler: Option<Arc<Reconciler>>) -> Arc<StateData> {
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
        token: String::new(),
        configured: vec![],
        docker: Arc::new(NoDocker),
        redactor: Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(2),
        idle_unload: reconciler,
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
        #[cfg(feature = "native-backends")]
        backend_stats: None,
        #[cfg(feature = "native-benchmark-runner")]
        bench_runner: None,
        #[cfg(feature = "native-backend-control")]
        backends: None,
    })
}

async fn call(state: Arc<StateData>, method: &str) -> (u16, Value) {
    let path = if method == "GET" {
        "/api/v1/models"
    } else {
        "/api/v1/models-ini"
    };
    let response = noevia_model_manager::router(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"x","baseRevision":"r"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let raw = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&raw).unwrap())
}

const SAVED: &str = "[synthetic-chat]\nmodel = synthetic/chat.gguf\nctx-size = 8192\n";

// The manager never unloads anything itself; llama.cpp only sleeps a child whose task queue is
// empty. What the manager must not do is overwrite a per-model choice, e.g. an administrator
// keeping one model resident with `sleep-idle-seconds = -1`.
#[tokio::test]
async fn per_model_override_is_never_touched() {
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("models.ini");
    std::fs::write(
        &ini,
        "[*]\nsleep-idle-seconds = 60\n[pinned]\nsleep-idle-seconds = -1\n",
    )
    .unwrap();
    let reconciler = Reconciler::new(ini.clone(), Some(900));
    assert!(reconciler.reconcile().await.unwrap().is_some());
    assert_eq!(
        std::fs::read_to_string(&ini).unwrap(),
        "[*]\nsleep-idle-seconds = 900\n[pinned]\nsleep-idle-seconds = -1\n"
    );
}

// Another file's save (here `PUT /api/v1/settings`) is forwarded untouched: no reconcile, and
// its own `revision` is never replaced with the models.ini hash.

async fn send(state: Arc<StateData>, method: &str, body: &str) -> (u16, Value) {
    let response = noevia_model_manager::router(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(noevia_model_manager::IDLE_UNLOAD_ROUTE)
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let raw = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&raw).unwrap())
}

fn stored(dir: &std::path::Path, env: Option<&str>) -> Arc<Reconciler> {
    Arc::new(Reconciler::with_store(
        dir.join("models.ini"),
        idle_unload::parse_env(env).unwrap(),
        Some(dir.join(idle_unload::STORE_FILE)),
    ))
}

// noevia#1863: the default is 30 minutes and is reported as such.
#[tokio::test]
async fn setting_defaults_to_thirty_minutes() {
    let dir = tempfile::tempdir().unwrap();
    let (status, body) = send(
        state("http://127.0.0.1:9", Some(stored(dir.path(), None))),
        "GET",
        "",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["seconds"], 1800);
    assert_eq!(body["source"], "default");
    assert_eq!(body["fallbackSeconds"], 1800);
    assert_eq!(body["defaultSeconds"], 1800);
    assert_eq!(body["minSeconds"], 300);
    assert_eq!(body["maxSeconds"], 86400);
}

// noevia#1863: a change is saved, rewrites the chat preset at once (no restart), survives a
// restart, wins over the env seed, survives a later editor save, and `null` returns to the env.

#[tokio::test]
async fn bad_requests_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("models.ini");
    std::fs::write(&ini, SAVED).unwrap();
    let reconciler = stored(dir.path(), None);
    for body in [
        r#"{"seconds":60}"#,
        r#"{"seconds":299}"#,
        r#"{"seconds":86401}"#,
        r#"{"seconds":-1}"#,
        r#"{"seconds":1800.5}"#,
        r#"{"seconds":"1800"}"#,
        r#"{"seconds":true}"#,
        r#"{}"#,
        r#"[1800]"#,
        "not json",
    ] {
        let (status, out) = send(
            state("http://127.0.0.1:9", Some(reconciler.clone())),
            "PUT",
            body,
        )
        .await;
        assert_eq!(status, 400, "{body}: {out}");
    }
    let (status, _) = send(
        state("http://127.0.0.1:9", Some(reconciler.clone())),
        "DELETE",
        "",
    )
    .await;
    assert_eq!(status, 405);
    assert_eq!(std::fs::read_to_string(&ini).unwrap(), SAVED);
    assert!(!dir.path().join(idle_unload::STORE_FILE).exists());
    assert_eq!(reconciler.setting(), Some(1800));
}

// A stored file that is not a valid choice is ignored (the env applies) rather than fatal.
#[tokio::test]
async fn unreadable_store_falls_back_to_env() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(idle_unload::STORE_FILE), r#"{"seconds":5}"#).unwrap();
    let reconciler = stored(dir.path(), Some("600"));
    assert_eq!(reconciler.setting(), Some(600));
    assert_eq!(reconciler.view().source, idle_unload::Source::Env);
}

// Without a store (no DATA_DIR) a save is refused instead of being lost on restart.
#[tokio::test]
async fn no_store_refuses_saves() {
    let dir = tempfile::tempdir().unwrap();
    let reconciler = Arc::new(Reconciler::new(dir.path().join("models.ini"), Some(1800)));
    let (status, _) = send(
        state("http://127.0.0.1:9", Some(reconciler)),
        "PUT",
        r#"{"seconds":600}"#,
    )
    .await;
    assert_eq!(status, 503);
}
