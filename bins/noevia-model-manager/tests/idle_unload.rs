//! sbstndalton/noevia#1747: idle chat models unload themselves through the chat preset's global
//! `sleep-idle-seconds` key, which the model manager keeps in place across Python writes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
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

/// A fake Python editor: PUT writes the text it is given verbatim (as `write_raw_text` does) and
/// answers with that text's revision; `fail` makes it refuse with 409 without writing.
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

fn state(base: &str, reconciler: Option<Arc<Reconciler>>) -> Arc<StateData> {
    Arc::new(StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis: None,
        #[cfg(feature = "native-prompts")]
        prompts: None,
        #[cfg(feature = "native-settings")]
        settings: None,
        #[cfg(feature = "native-model-updates")]
        model_updates: None,
        sections: None,
        embedding: None,
        host: None,
        token: String::new(),
        configured: vec![],
        docker: Arc::new(NoDocker),
        redactor: Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(2),
        legacy: noevia_model_manager::legacy_client().unwrap(),
        legacy_base: base.to_owned(),
        idle_unload: reconciler,
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

const SAVED: &str = "[gemma-4-12b]\nmodel = synthetic/chat.gguf\nctx-size = 8192\n";

#[tokio::test]
async fn editor_save_without_the_key_gets_it_back_with_a_matching_revision() {
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("models.ini");
    let embed = dir.path().join("embedding.ini");
    std::fs::write(&ini, "").unwrap();
    std::fs::write(&embed, "[nomic]\nmodel = synthetic/embed.gguf\n").unwrap();
    let base = fake_python(ini.clone(), SAVED, false).await;
    let reconciler = Arc::new(Reconciler::new(
        ini.clone(),
        idle_unload::parse(None).unwrap(),
    ));
    let (status, body) = call(state(&base, Some(reconciler)), "PUT").await;
    assert_eq!(status, 200);
    let now = std::fs::read(&ini).unwrap();
    assert_eq!(
        String::from_utf8(now.clone()).unwrap(),
        format!("[*]\nsleep-idle-seconds = 900\n\n{SAVED}")
    );
    // The editor's next save must be based on the file as it now is.
    assert_eq!(body["revision"], revision(&now));
    assert_eq!(body["ok"], true);
    // #1717: the embedding router's preset stays exactly as it was.
    assert_eq!(
        std::fs::read_to_string(&embed).unwrap(),
        "[nomic]\nmodel = synthetic/embed.gguf\n"
    );
}

#[tokio::test]
async fn refused_writes_and_reads_leave_the_preset_alone() {
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("models.ini");
    std::fs::write(&ini, SAVED).unwrap();
    let reconciler = Arc::new(Reconciler::new(ini.clone(), Some(900)));
    let base = fake_python(ini.clone(), SAVED, true).await;
    let (status, _) = call(state(&base, Some(reconciler.clone())), "PUT").await;
    assert_eq!(status, 409);
    let base = fake_python(ini.clone(), SAVED, false).await;
    let (status, body) = call(state(&base, Some(reconciler)), "GET").await;
    assert_eq!(status, 200);
    assert_eq!(body["revision"], revision(SAVED.as_bytes()));
    assert_eq!(std::fs::read_to_string(&ini).unwrap(), SAVED);
}

#[tokio::test]
async fn zero_disables_and_removes_the_global_key() {
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("models.ini");
    std::fs::write(&ini, "").unwrap();
    const WITH_KEY: &str = "[*]\nsleep-idle-seconds = 900\n[gemma-4-12b]\nmodel = c.gguf\n";
    let base = fake_python(ini.clone(), WITH_KEY, false).await;
    let reconciler = Arc::new(Reconciler::new(
        ini.clone(),
        idle_unload::parse(Some("0")).unwrap(),
    ));
    let (status, _) = call(state(&base, Some(reconciler)), "PUT").await;
    assert_eq!(status, 200);
    assert_eq!(
        std::fs::read_to_string(&ini).unwrap(),
        "[*]\n[gemma-4-12b]\nmodel = c.gguf\n"
    );
}

/// The manager never unloads anything itself; llama.cpp only sleeps a child whose task queue is
/// empty. What the manager must not do is overwrite a per-model choice, e.g. an administrator
/// keeping one model resident with `sleep-idle-seconds = -1`.
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

/// Another file's save (here `PUT /api/v1/settings`) is forwarded untouched: no reconcile, and
/// its own `revision` is never replaced with the models.ini hash.
#[tokio::test]
async fn other_routes_keep_their_revision_and_never_reconcile() {
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("models.ini");
    std::fs::write(&ini, SAVED).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/api/v1/settings",
        axum::routing::put(|| async {
            axum::Json(serde_json::json!({"ok":true,"revision":"settings-revision-7"}))
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let reconciler = Arc::new(Reconciler::new(ini.clone(), Some(900)));
    let response = noevia_model_manager::router(state(&base, Some(reconciler)))
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/settings")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let raw = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(body["revision"], "settings-revision-7");
    assert_eq!(std::fs::read_to_string(&ini).unwrap(), SAVED);
}
