// Archived pre-retirement private-input oracle; cfg(test) library only.
//! sbstndalton/noevia#1668: settings-driven embedding service.
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
    docker::{allowed_write, ScopedControl},
    embedding::{
        parse_models, Action, Desired, EmbeddingService, Entry, Probe, Refusal, Served,
        ServiceControl,
    },
    Container, DockerRead, Job, Redactor, StateData,
};
use serde_json::Value;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
};
use tower::ServiceExt;

#[derive(Default)]
struct Control {
    actions: Mutex<Vec<Action>>,
    fail: Mutex<bool>,
}
impl ServiceControl for Control {
    fn act(&self, action: Action) -> Job<'_, ()> {
        self.actions.lock().unwrap().push(action);
        let fail = *self.fail.lock().unwrap();
        Box::pin(async move {
            if fail {
                Err("Docker refused the embedding container action")
            } else {
                Ok(())
            }
        })
    }
}

// A stub embedding router: what /v1/models reports, and which loads were asked for.
#[derive(Default)]
struct Router {
    models: Mutex<Vec<Served>>,
    loads: Mutex<Vec<String>>,
    reachable: Mutex<bool>,
    // The router accepts loads but never finishes one.
    stuck: Mutex<bool>,
}
impl Router {
    fn set(&self, models: &[(&str, bool)]) {
        *self.reachable.lock().unwrap() = true;
        *self.models.lock().unwrap() = models
            .iter()
            .map(|(id, loaded)| Served {
                id: (*id).into(),
                loaded: *loaded,
            })
            .collect();
    }
}
impl Probe for Router {
    fn models(&self) -> Job<'_, Vec<Served>> {
        let reachable = *self.reachable.lock().unwrap();
        let models = self.models.lock().unwrap().clone();
        Box::pin(async move {
            if reachable {
                Ok(models)
            } else {
                Err("embedding service unreachable")
            }
        })
    }
    fn load<'a>(&'a self, model: &'a str) -> Job<'a, ()> {
        self.loads.lock().unwrap().push(model.into());
        if *self.stuck.lock().unwrap() {
            return Box::pin(async { Ok(()) });
        }
        // Loading succeeds: the router now reports only that model loaded.
        let mut models = self.models.lock().unwrap();
        for m in models.iter_mut() {
            m.loaded = m.id == model;
        }
        Box::pin(async { Ok(()) })
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    control: Arc<Control>,
    router: Arc<Router>,
    service: Arc<EmbeddingService>,
}

fn fixture() -> Fixture {
    fixture_with(|s| s)
}

fn fixture_with(tune: impl FnOnce(EmbeddingService) -> EmbeddingService) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    for file in [
        "nomic-embed-text-v1/nomic-embed-text-v1.Q8_0.gguf",
        "embeddinggemma-2/embeddinggemma-2-Q8_0.gguf",
    ] {
        let path = models.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"GGUF").unwrap();
    }
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    let control = Arc::new(Control::default());
    let router = Arc::new(Router::default());
    let service = EmbeddingService::new(
        "cowork-embed-1".into(),
        models,
        "/models".into(),
        dir.path().join("config/embedding.ini"),
        dir.path().join("data/embedding-service.json"),
        control.clone(),
        router.clone(),
    )
    .with_warm(Duration::from_millis(5), 50);
    let service = tune(service);
    Fixture {
        dir,
        control,
        router,
        service: Arc::new(service),
    }
}

const NOMIC: &str = "nomic-embed-text-v1";
const NOMIC_FILE: &str = "nomic-embed-text-v1/nomic-embed-text-v1.Q8_0.gguf";
const GEMMA: &str = "embeddinggemma-2";
const GEMMA_FILE: &str = "embeddinggemma-2/embeddinggemma-2-Q8_0.gguf";

fn entry(model: &str, file: &str) -> Entry {
    Entry {
        model: model.into(),
        file: file.into(),
    }
}
fn serve(entries: &[(&str, &str)]) -> Desired {
    Desired {
        models: entries.iter().map(|(m, f)| entry(m, f)).collect(),
    }
}
fn preset(f: &Fixture) -> String {
    std::fs::read_to_string(f.dir.path().join("config/embedding.ini")).unwrap()
}
async fn settle(f: &Fixture) {
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        if f.service.saved().unwrap().unwrap().previous.is_empty() {
            return;
        }
    }
}

async fn docker_peer(socket: &Path, status: &'static str) -> tokio::task::JoinHandle<String> {
    let listener = UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        while !raw.ends_with(b"\r\n\r\n") {
            let mut chunk = [0u8; 512];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            raw.extend_from_slice(&chunk[..count]);
        }
        stream
            .write_all(
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        String::from_utf8_lossy(&raw)
            .lines()
            .next()
            .unwrap()
            .to_owned()
    })
}

struct NoDocker;
impl DockerRead for NoDocker {
    fn containers(&self) -> Job<'_, Vec<Container>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn logs<'a>(&'a self, _: &'a str, _: usize) -> Job<'a, Vec<u8>> {
        Box::pin(async { Ok(vec![]) })
    }
}

fn state(service: Arc<EmbeddingService>) -> Arc<StateData> {
    state_at(service, noevia_model_manager::LEGACY_BASE)
}

fn state_at(service: Arc<EmbeddingService>, legacy_base: &str) -> Arc<StateData> {
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
        embedding: Some(service),
        host: None,
        token: String::new(),
        configured: vec![],
        docker: Arc::new(NoDocker),
        redactor: Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(2),
        legacy: noevia_model_manager::legacy_client().unwrap(),
        legacy_base: legacy_base.to_owned(),
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
        #[cfg(feature = "native-backends")]
        backend_stats: None,
        #[cfg(feature = "native-benchmark-runner")]
        bench_runner: None,
        #[cfg(feature = "native-backend-control")]
        backends: None,
    })
}

async fn call(state: Arc<StateData>, method: &str, path: &str, body: &str) -> (u16, Value) {
    let response = noevia_model_manager::router(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let raw = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

// A fake Python delete on an ephemeral loopback port (#1678; returns its base URL): signals when a delete arrives, waits, then removes the
// file, like a slow `delete_gguf`.
async fn slow_python_delete(
    file: std::path::PathBuf,
) -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (arrived, arrived_rx) = tokio::sync::oneshot::channel();
    let arrived = Arc::new(Mutex::new(Some(arrived)));
    let app = axum::Router::new().route(
        "/api/v1/models/delete",
        axum::routing::post(move || {
            let arrived = arrived.clone();
            let file = file.clone();
            async move {
                if let Some(tx) = arrived.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                std::fs::remove_file(&file).unwrap();
                axum::Json(serde_json::json!({"results":[{"ok":true}]}))
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base, arrived_rx, server)
}

#[tokio::test]
async fn switch_to_a_model_being_deleted_never_saves_it_as_served() {
    let f = fixture();
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    let (base, arrived, server) =
        slow_python_delete(f.dir.path().join("models").join(GEMMA_FILE)).await;
    let s = state_at(f.service.clone(), &base);
    let delete = tokio::spawn(call(
        s,
        "POST",
        "/api/v1/models/delete",
        r#"{"models":["embeddinggemma-2/embeddinggemma-2-Q8_0.gguf"]}"#,
    ));
    // The delete passed the check and Python is removing the file: now switch to that model.
    arrived.await.unwrap();
    let applied = f
        .service
        .apply(serve(&[(NOMIC, NOMIC_FILE), (GEMMA, GEMMA_FILE)]))
        .await;
    let (status, _) = delete.await.unwrap();
    server.abort();
    assert_eq!(status, 200);
    assert_eq!(applied, Err(Refusal::Invalid("model file not found")));
    let saved = f.service.saved().unwrap().unwrap();
    assert_eq!(saved.models, vec![entry(NOMIC, NOMIC_FILE)]);
    assert!(!preset(&f).contains(GEMMA));
    assert!(f.control.actions.lock().unwrap().is_empty());
}

// Docker that never answers.
struct Hang;
impl ServiceControl for Hang {
    fn act(&self, _: Action) -> Job<'_, ()> {
        Box::pin(std::future::pending())
    }
}

// Docker that answers until told to hang (#1682).
#[derive(Default)]
struct HangLater {
    hang: Mutex<bool>,
}
impl ServiceControl for HangLater {
    fn act(&self, _: Action) -> Job<'_, ()> {
        if *self.hang.lock().unwrap() {
            Box::pin(std::future::pending())
        } else {
            Box::pin(async { Ok(()) })
        }
    }
}

// #1717: a watched service with a short probe interval, seeded with nomic and settled (no
// boot warm-up running).
fn watched(attempts: u32) -> Fixture {
    let f = fixture_with(|s| {
        s.with_warm(Duration::from_millis(5), attempts)
            .with_watch(Duration::from_millis(20))
    });
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    f
}

// ── #1726: image presets (multimodal RAG) ───────────────────────────────────────────────────────
fn with_projector(f: &Fixture, names: &[&str]) {
    for name in names {
        std::fs::write(
            f.dir.path().join("models/embeddinggemma-2").join(name),
            b"GGUF",
        )
        .unwrap();
    }
}

// noevia#1984/#1994: with native downloads, the embedding-guarded Python delete also holds the
// whole download table while it is forwarded, and lets go when the child answers.
#[cfg(feature = "native-downloads")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guarded_python_delete_holds_the_download_table_while_forwarded() {
    use noevia_model_manager::downloads::{Config, Downloads};
    let f = fixture();
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (arrived, arrived_rx) = tokio::sync::oneshot::channel::<()>();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let hooks = Arc::new(Mutex::new(Some((arrived, released))));
    let app = axum::Router::new().route(
        "/api/v1/models/delete",
        axum::routing::post(move || {
            let hooks = hooks.clone();
            async move {
                let (arrived, released) = hooks.lock().unwrap().take().unwrap();
                let _ = arrived.send(());
                let _ = released.await;
                axum::Json(serde_json::json!({"results": [{"ok": true}]}))
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let data = f.dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let models = f.dir.path().join("models");
    let table = Downloads::new(Config::production(
        models.to_str().unwrap().to_owned(),
        data.to_str().unwrap(),
        1,
    ))
    .unwrap();
    let mut s = Arc::try_unwrap(state_at(f.service.clone(), &base))
        .ok()
        .unwrap();
    s.downloads = Some(table.clone());
    let delete = tokio::spawn(call(
        Arc::new(s),
        "POST",
        "/api/v1/models/delete",
        r#"{"models":["embeddinggemma-2/embeddinggemma-2-Q8_0.gguf"]}"#,
    ));
    tokio::time::timeout(Duration::from_secs(20), arrived_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(table.whole_reservations(), 1, "not held while forwarded");
    release.send(()).unwrap();
    let (status, body) = delete.await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, serde_json::json!({"results": [{"ok": true}]}));
    assert_eq!(table.whole_reservations(), 0);
    server.abort();
}

// A child that takes a delete and answers only when told: its base, the arrival, the answer.
async fn child_holding_delete() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (arrived, arrived_rx) = tokio::sync::oneshot::channel::<()>();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let hooks = Arc::new(Mutex::new(Some((arrived, released))));
    let app = axum::Router::new().route(
        "/api/v1/models/delete",
        axum::routing::post(move || {
            let hooks = hooks.clone();
            async move {
                let (arrived, released) = hooks.lock().unwrap().take().unwrap();
                let _ = arrived.send(());
                let _ = released.await;
                axum::Json(serde_json::json!({"results": [{"ok": true}]}))
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base, arrived_rx, release, server)
}

// Starts the guarded delete of a model nothing protects, waits until the child has it, then drops
// the request (the client went away).
async fn drop_forwarded_delete(
    state: &Arc<StateData>,
    arrived: tokio::sync::oneshot::Receiver<()>,
) {
    let delete = tokio::spawn(call(
        state.clone(),
        "POST",
        "/api/v1/models/delete",
        r#"{"models":["synthetic/a.gguf"]}"#,
    ));
    tokio::time::timeout(Duration::from_secs(20), arrived)
        .await
        .unwrap()
        .unwrap();
    delete.abort();
    assert!(delete.await.unwrap_err().is_cancelled());
}

// True when the embedding service's hold is taken (a switch would wait).
async fn hold_taken(service: &EmbeddingService) -> bool {
    tokio::time::timeout(Duration::from_millis(100), service.hold())
        .await
        .is_err()
}

// Waits (at most 20 s) until all `n` request permits are back.
async fn permits_back(state: &StateData, n: usize) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while state.permits.available_permits() < n {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

// noevia#1997/#1998: a guarded delete forwarded to the child and dropped before the child
// answers (the client went away) keeps the embedding service's hold, so no switch can choose
// the model being deleted, and its admission permit until the child has answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_guarded_delete_keeps_the_hold_and_its_permit_until_the_child_answers() {
    let f = fixture();
    let (base, arrived, release, server) = child_holding_delete().await;
    let state = state_at(f.service.clone(), &base);
    drop_forwarded_delete(&state, arrived).await;
    assert!(hold_taken(&f.service).await, "hold released mid-forward");
    assert_eq!(state.permits.available_permits(), 1);
    release.send(()).unwrap();
    permits_back(&state, 2).await;
    assert!(!hold_taken(&f.service).await);
    server.abort();
}

// noevia#1997/#1998: the same with native downloads (the whole table reserved while the child
// deletes): the hold, the reservation and the permit all last until the child has answered.
#[cfg(feature = "native-downloads")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_guarded_python_delete_keeps_the_hold_table_and_permit_until_the_child_answers() {
    use noevia_model_manager::downloads::{Config, Downloads};
    let f = fixture();
    let (base, arrived, release, server) = child_holding_delete().await;
    let data = f.dir.path().join("data");
    let models = f.dir.path().join("models");
    let table = Downloads::new(Config::production(
        models.to_str().unwrap().to_owned(),
        data.to_str().unwrap(),
        1,
    ))
    .unwrap();
    let mut s = Arc::try_unwrap(state_at(f.service.clone(), &base))
        .ok()
        .unwrap();
    s.downloads = Some(table.clone());
    let state = Arc::new(s);
    drop_forwarded_delete(&state, arrived).await;
    assert!(hold_taken(&f.service).await, "hold released mid-forward");
    assert_eq!(table.whole_reservations(), 1);
    assert_eq!(state.permits.available_permits(), 1);
    release.send(()).unwrap();
    permits_back(&state, 2).await;
    assert_eq!(table.whole_reservations(), 0);
    assert!(!hold_taken(&f.service).await);
    server.abort();
}

// Frozen child-era environment mapping for #1668. The native-only service does not
// launch a Python child; retain the old mapping and assertions only as a test oracle.
/// #1668: what the Python child is told about the guarded embedding service: its container and
/// address, so it skips only env names that point at it. `None` clears them all.
fn embed_guard_env(service: Option<(&str, &str)>) -> [(&'static str, Option<String>); 3] {
    [
        (
            "NOEVIA_EMBED_SERVICE_GUARDED",
            service.map(|_| "1".to_owned()),
        ),
        (
            "NOEVIA_EMBED_SERVICE_CONTAINER",
            service.map(|(c, _)| c.to_owned()),
        ),
        (
            "NOEVIA_EMBED_SERVICE_URL",
            service.map(|(_, u)| u.to_owned()),
        ),
    ]
}

#[test]
fn the_python_child_learns_which_container_and_address_are_guarded() {
    let on = embed_guard_env(Some(("cowork-embed-1", "http://embed:8080")));
    assert_eq!(
        on.map(|(k, v)| (k, v.unwrap_or_default())),
        [
            ("NOEVIA_EMBED_SERVICE_GUARDED", "1".to_owned()),
            (
                "NOEVIA_EMBED_SERVICE_CONTAINER",
                "cowork-embed-1".to_owned()
            ),
            ("NOEVIA_EMBED_SERVICE_URL", "http://embed:8080".to_owned()),
        ]
    );
    assert!(embed_guard_env(None).iter().all(|(_, v)| v.is_none()));
}
