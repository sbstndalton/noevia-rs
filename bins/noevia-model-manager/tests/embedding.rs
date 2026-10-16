//! sbstndalton/noevia#1668: settings-driven embedding service.
#![allow(
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

/// A stub embedding router: what /v1/models reports, and which loads were asked for.
#[derive(Default)]
struct Router {
    models: Mutex<Vec<Served>>,
    loads: Mutex<Vec<String>>,
    reachable: Mutex<bool>,
    /// The router accepts loads but never finishes one.
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

#[test]
fn env_seeds_once_and_saved_state_wins_afterwards() {
    let f = fixture();
    let seeded = f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    assert_eq!(seeded.models, vec![entry(NOMIC, NOMIC_FILE)]);
    assert!(preset(&f).contains("[nomic-embed-text-v1]\nmodel = /models/nomic-embed-text-v1/"));
    // A later boot with a different env keeps the saved state.
    let again = f.service.seed(GEMMA, GEMMA_FILE).unwrap();
    assert_eq!(again.models, vec![entry(NOMIC, NOMIC_FILE)]);
    assert!(!preset(&f).contains(GEMMA));
}

#[test]
fn unset_seed_means_off_and_preset_has_no_model() {
    let f = fixture();
    let seeded = f.service.seed("", "").unwrap();
    assert!(seeded.models.is_empty());
    assert!(!preset(&f).contains('['));
}

#[test]
fn preset_is_rerendered_from_saved_state_on_boot() {
    let f = fixture();
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    std::fs::write(f.dir.path().join("config/embedding.ini"), "hand edit").unwrap();
    f.service.seed(GEMMA, GEMMA_FILE).unwrap();
    assert!(preset(&f).contains("[nomic-embed-text-v1]"));
}

#[tokio::test]
async fn switch_serves_both_while_reindexing_then_frees_the_old_model() {
    let f = fixture();
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    f.router.set(&[(NOMIC, true)]);
    // Start: the active model keeps serving search while the next one is built.
    let saved = f
        .service
        .apply(serve(&[(NOMIC, NOMIC_FILE), (GEMMA, GEMMA_FILE)]))
        .await
        .unwrap();
    assert!(saved.previous.is_empty());
    assert_eq!(*f.control.actions.lock().unwrap(), vec![Action::Restart]);
    let text = preset(&f);
    assert!(
        text.contains("[nomic-embed-text-v1]")
            && text.contains("[embeddinggemma-2]\nmodel = /models/embeddinggemma-2/")
    );
    f.router.set(&[(NOMIC, false), (GEMMA, false)]);
    settle(&f).await;
    for _ in 0..100 {
        if f.router.models.lock().unwrap().iter().all(|m| m.loaded) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(f.router.loads.lock().unwrap().contains(&GEMMA.to_owned()));
    assert!(f.service.protects(NOMIC_FILE).await);
    assert!(f.service.protects(GEMMA).await);
    // Finish: only the new model stays. The old one is protected until the router drops it.
    let saved = f
        .service
        .apply(serve(&[(GEMMA, GEMMA_FILE)]))
        .await
        .unwrap();
    assert_eq!(saved.previous, vec![entry(NOMIC, NOMIC_FILE)]);
    assert!(!preset(&f).contains(NOMIC));
    assert!(f.service.protects(NOMIC_FILE).await);
    f.router.set(&[(GEMMA, true)]);
    settle(&f).await;
    assert!(f.service.saved().unwrap().unwrap().previous.is_empty());
    assert!(!f.service.protects(NOMIC_FILE).await);
    assert!(!f.service.protects("nomic-embed-text-v1").await);
    assert!(f.service.protects(GEMMA_FILE).await);
    assert_eq!(
        *f.control.actions.lock().unwrap(),
        vec![Action::Restart, Action::Restart]
    );
}

#[tokio::test]
async fn old_model_stays_protected_while_the_router_still_reports_it_loaded() {
    let f = fixture();
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    f.router.set(&[(NOMIC, true)]);
    f.service
        .apply(serve(&[(GEMMA, GEMMA_FILE)]))
        .await
        .unwrap();
    // The router has not restarted yet: nomic is still the loaded model.
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        f.service.saved().unwrap().unwrap().previous,
        vec![entry(NOMIC, NOMIC_FILE)]
    );
    assert!(f.service.protects(NOMIC_FILE).await);
}

#[tokio::test]
async fn turning_off_stops_the_service_and_frees_the_last_model() {
    let f = fixture();
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    f.router.set(&[(NOMIC, true)]);
    assert!(f.service.protects(NOMIC).await);
    let saved = f.service.apply(Desired::default()).await.unwrap();
    assert!(saved.models.is_empty() && saved.previous.is_empty());
    assert_eq!(*f.control.actions.lock().unwrap(), vec![Action::Stop]);
    assert!(!preset(&f).contains('['));
    // The stopped container answers nothing.
    *f.router.reachable.lock().unwrap() = false;
    assert!(!f.service.protects(NOMIC_FILE).await);
    let status = f.service.status().await;
    assert_eq!(status["enabled"], false);
    assert_eq!(status["running"]["reachable"], false);
}

#[tokio::test]
async fn delete_protection_follows_running_state_not_only_saved_state() {
    let f = fixture();
    f.service.seed("", "").unwrap();
    // Saved says off, but the router still reports nomic loaded: never delete it (#336).
    f.router.set(&[(NOMIC, true)]);
    assert!(f.service.protects(NOMIC).await);
    assert!(!f.service.protects(GEMMA_FILE).await);
}

#[tokio::test]
async fn unreadable_saved_state_fails_closed() {
    let f = fixture();
    std::fs::write(
        f.dir.path().join("data/embedding-service.json"),
        "{not json",
    )
    .unwrap();
    assert!(f.service.protects("anything.gguf").await);
}

#[tokio::test]
async fn invalid_requests_are_refused_without_touching_docker() {
    let f = fixture();
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    for bad in [
        serve(&[(GEMMA, "../etc/passwd.gguf")]),
        serve(&[(GEMMA, "/abs/model.gguf")]),
        serve(&[(GEMMA, "missing/model.gguf")]),
        serve(&[(GEMMA, "embeddinggemma-2/notes.txt")]),
        serve(&[("bad]\n[x", GEMMA_FILE)]),
        serve(&[(GEMMA, GEMMA_FILE), (GEMMA, GEMMA_FILE)]),
        serve(&[
            (NOMIC, NOMIC_FILE),
            (GEMMA, GEMMA_FILE),
            ("third", GEMMA_FILE),
        ]),
    ] {
        assert!(matches!(
            f.service.apply(bad).await,
            Err(Refusal::Invalid(_))
        ));
    }
    assert!(f.control.actions.lock().unwrap().is_empty());
    assert_eq!(
        f.service.saved().unwrap().unwrap().models,
        vec![entry(NOMIC, NOMIC_FILE)]
    );
}

#[test]
fn symlink_escape_from_models_dir_is_refused() {
    let f = fixture();
    let outside = f.dir.path().join("outside.gguf");
    std::fs::write(&outside, b"GGUF").unwrap();
    std::os::unix::fs::symlink(&outside, f.dir.path().join("models/escape.gguf")).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(matches!(
        rt.block_on(f.service.apply(serve(&[(GEMMA, "escape.gguf")]))),
        Err(Refusal::Invalid(_))
    ));
}

#[test]
fn router_status_values_decide_loaded() {
    let raw = br#"{"data":[{"id":"a","status":{"value":"loaded"}},{"id":"b","status":{"value":"unloaded"}},{"id":"c"}]}"#;
    let models = parse_models(raw).unwrap();
    assert_eq!(
        models,
        vec![
            Served {
                id: "a".into(),
                loaded: true
            },
            Served {
                id: "b".into(),
                loaded: false
            },
            Served {
                id: "c".into(),
                loaded: true
            },
        ]
    );
}

#[test]
fn docker_writes_are_limited_to_the_configured_container() {
    assert!(allowed_write(
        "/containers/cowork-embed-1/restart?t=10",
        "cowork-embed-1"
    ));
    assert!(allowed_write(
        "/containers/cowork-embed-1/stop?t=10",
        "cowork-embed-1"
    ));
    assert!(!allowed_write(
        "/containers/cowork-llama-1/restart?t=10",
        "cowork-embed-1"
    ));
    assert!(!allowed_write(
        "/containers/cowork-embed-1/kill",
        "cowork-embed-1"
    ));
    assert!(!allowed_write("/containers/create", "cowork-embed-1"));
    assert!(!allowed_write(
        "/containers/cowork-embed-1/exec",
        "cowork-embed-1"
    ));
    assert!(ScopedControl::new("/x".into(), "../evil").is_err());
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

#[tokio::test]
async fn scoped_control_posts_restart_and_stop_to_the_unix_peer() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("docker.sock");
    let peer = docker_peer(&socket, "204 No Content").await;
    let control = ScopedControl::new(socket.clone(), "cowork-embed-1").unwrap();
    control.act(Action::Restart).await.unwrap();
    assert_eq!(
        peer.await.unwrap(),
        "POST /containers/cowork-embed-1/restart?t=10 HTTP/1.1"
    );
    std::fs::remove_file(&socket).unwrap();
    let peer = docker_peer(&socket, "304 Not Modified").await;
    control.act(Action::Stop).await.unwrap();
    assert_eq!(
        peer.await.unwrap(),
        "POST /containers/cowork-embed-1/stop?t=10 HTTP/1.1"
    );
    std::fs::remove_file(&socket).unwrap();
    let peer = docker_peer(&socket, "404 Not Found").await;
    assert!(control.act(Action::Restart).await.is_err());
    peer.await.unwrap();
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

#[tokio::test]
async fn delete_route_refuses_the_service_model_before_python_sees_it() {
    let f = fixture();
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    f.router.set(&[(NOMIC, true)]);
    let (status, body) = call(
        state(f.service.clone()),
        "POST",
        "/api/v1/models/delete",
        r#"{"models":["nomic-embed-text-v1/nomic-embed-text-v1.Q8_0.gguf"]}"#,
    )
    .await;
    assert_eq!(status, 409);
    assert!(body["detail"].as_str().unwrap().contains("embedding"));
}

#[tokio::test]
async fn service_route_reports_and_applies_choices() {
    let f = fixture();
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    f.router.set(&[(NOMIC, true)]);
    let s = state(f.service.clone());
    let (status, body) = call(s.clone(), "GET", "/api/v1/embedding-service", "").await;
    assert_eq!(status, 200);
    assert_eq!(body["models"][0]["model"], NOMIC);
    assert_eq!(body["running"]["loaded"][0], NOMIC);
    let (status, _) = call(
        s.clone(),
        "PUT",
        "/api/v1/embedding-service",
        r#"{"models":[{"model":"x","file":"../x.gguf"}]}"#,
    )
    .await;
    assert_eq!(status, 400);
    let (status, body) = call(s, "PUT", "/api/v1/embedding-service", r#"{"models":[]}"#).await;
    assert_eq!(status, 202);
    assert_eq!(body["enabled"], false);
}

#[tokio::test]
async fn boot_warms_the_saved_models_so_the_router_reports_them_loaded() {
    let f = fixture();
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    // After a container restart the router lists the preset but has not loaded it yet.
    f.router.set(&[(NOMIC, false)]);
    f.service.start_warm();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        if !f.router.loads.lock().unwrap().is_empty() {
            break;
        }
    }
    assert_eq!(*f.router.loads.lock().unwrap(), vec![NOMIC.to_owned()]);
    assert!(
        f.control.actions.lock().unwrap().is_empty(),
        "boot never restarts or stops anything"
    );
}

/// A fake Python delete on an ephemeral loopback port (#1678; returns its base URL): signals when a delete arrives, waits, then removes the
/// file, like a slow `delete_gguf`.
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

#[tokio::test]
async fn sharded_models_are_protected_by_their_shard_base_key() {
    let f = fixture();
    let dir = f.dir.path().join("models/big-embed");
    std::fs::create_dir_all(&dir).unwrap();
    for part in [
        "big-embed-00001-of-00002.gguf",
        "big-embed-00002-of-00002.gguf",
    ] {
        std::fs::write(dir.join(part), b"GGUF").unwrap();
    }
    f.service.seed("", "").unwrap();
    f.service
        .apply(serve(&[(
            "big-embed",
            "big-embed/big-embed-00001-of-00002.gguf",
        )]))
        .await
        .unwrap();
    // services.py keys the entry by subdir + shard base; a flat copy by the base alone.
    assert!(f.service.protects("big-embed/big-embed.gguf").await);
    assert!(f.service.protects("big-embed.gguf").await);
    assert!(!f.service.protects("other/other.gguf").await);
}

#[test]
fn a_bad_state_file_or_read_only_config_leaves_an_error_state_that_refuses_deletes() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    // Unreadable saved state.
    let f = fixture();
    std::fs::write(
        f.dir.path().join("data/embedding-service.json"),
        "{not json",
    )
    .unwrap();
    assert!(f.service.seed(NOMIC, NOMIC_FILE).is_err());
    assert!(rt.block_on(f.service.protects("anything.gguf")));
    assert!(rt.block_on(f.service.status())["error"].is_string());
    // Preset directory not writable (stands in for a read-only /config): nothing saved, so only
    // the error state keeps deletes refused.
    let f = fixture();
    std::fs::remove_dir_all(f.dir.path().join("config")).unwrap();
    assert!(f.service.seed("", "").is_err());
    assert!(f.service.saved().unwrap().is_none());
    assert!(rt.block_on(f.service.protects(NOMIC_FILE)));
    assert!(rt.block_on(f.service.status())["error"].is_string());
}

#[test]
fn an_empty_first_boot_seed_is_not_saved_as_off_for_good() {
    let f = fixture();
    assert!(f.service.seed("", "").unwrap().models.is_empty());
    assert!(f.service.saved().unwrap().is_none());
    let seeded = f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    assert_eq!(seeded.models, vec![entry(NOMIC, NOMIC_FILE)]);
}

/// Docker that never answers.
struct Hang;
impl ServiceControl for Hang {
    fn act(&self, _: Action) -> Job<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn a_docker_timeout_rolls_back_and_keeps_both_models_protected() {
    let dir = tempfile::tempdir().unwrap();
    let base = fixture();
    let models = base.dir.path().join("models");
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    let service = Arc::new(
        EmbeddingService::new(
            "cowork-embed-1".into(),
            models,
            "/models".into(),
            dir.path().join("config/embedding.ini"),
            dir.path().join("data/embedding-service.json"),
            Arc::new(Hang),
            base.router.clone(),
        )
        .with_act_timeout(Duration::from_millis(50)),
    );
    service.seed(NOMIC, NOMIC_FILE).unwrap();
    let result = service.apply(serve(&[(GEMMA, GEMMA_FILE)])).await;
    assert!(matches!(result, Err(Refusal::Unavailable(_))));
    let saved = service.saved().unwrap().unwrap();
    assert_eq!(saved.models, vec![entry(NOMIC, NOMIC_FILE)]);
    assert_eq!(saved.previous, vec![entry(GEMMA, GEMMA_FILE)]);
    let ini = std::fs::read_to_string(dir.path().join("config/embedding.ini")).unwrap();
    assert!(ini.contains(NOMIC) && !ini.contains(GEMMA));
    // Docker may still restart onto either preset: both stay protected.
    assert!(service.protects(NOMIC_FILE).await);
    assert!(service.protects(GEMMA_FILE).await);
    // Turning off from the same state also rolls back instead of claiming it is off.
    assert!(service.apply(Desired::default()).await.is_err());
    assert_eq!(
        service.saved().unwrap().unwrap().models,
        vec![entry(NOMIC, NOMIC_FILE)]
    );
}

#[tokio::test]
async fn previous_keeps_each_model_once_even_when_not_adjacent() {
    let f = fixture();
    let third = f.dir.path().join("models/third/third.gguf");
    std::fs::create_dir_all(third.parent().unwrap()).unwrap();
    std::fs::write(&third, b"GGUF").unwrap();
    std::fs::write(
        f.dir.path().join("data/embedding-service.json"),
        serde_json::json!({
            "models": [entry(GEMMA, GEMMA_FILE)],
            "previous": [entry(NOMIC, NOMIC_FILE), entry(GEMMA, GEMMA_FILE), entry(NOMIC, NOMIC_FILE)],
        })
        .to_string(),
    )
    .unwrap();
    f.service
        .apply(serve(&[("third", "third/third.gguf")]))
        .await
        .unwrap();
    assert_eq!(
        f.service.saved().unwrap().unwrap().previous,
        vec![entry(NOMIC, NOMIC_FILE), entry(GEMMA, GEMMA_FILE)]
    );
}

#[test]
fn the_service_route_is_never_left_without_a_token() {
    assert!(noevia_model_manager::require_embed_token("cowork-embed-1", "").is_err());
    assert!(noevia_model_manager::require_embed_token("cowork-embed-1", "t").is_ok());
    assert!(noevia_model_manager::require_embed_token("", "").is_ok());
}

#[test]
fn all_hex_container_names_are_refused() {
    for id in ["abc123", "deadbeef", "0123456789"] {
        assert!(ScopedControl::new("/x".into(), id).is_err(), "{id}");
    }
    assert!(ScopedControl::new("/x".into(), "cowork-embed-1").is_ok());
}

#[test]
fn the_python_child_learns_which_container_and_address_are_guarded() {
    let on = noevia_model_manager::embed_guard_env(Some(("cowork-embed-1", "http://embed:8080")));
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
    assert!(noevia_model_manager::embed_guard_env(None)
        .iter()
        .all(|(_, v)| v.is_none()));
}

#[tokio::test]
async fn a_change_that_cannot_get_the_lock_in_time_gives_up_and_never_applies_later() {
    // #1677: the caller's request budget is ACT_TIMEOUT + LOCK_WAIT; past it the caller was told
    // the change failed, so it must not happen afterwards.
    let f = fixture_with(|s| s.with_lock_wait(Duration::from_millis(50)));
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    let held = f.service.hold().await;
    let applied = f
        .service
        .apply(serve(&[(NOMIC, NOMIC_FILE), (GEMMA, GEMMA_FILE)]))
        .await;
    assert!(
        matches!(applied, Err(Refusal::Unavailable(_))),
        "{applied:?}"
    );
    drop(held);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        f.service.saved().unwrap().unwrap().models,
        vec![entry(NOMIC, NOMIC_FILE)]
    );
    assert!(!preset(&f).contains(GEMMA));
    assert!(f.control.actions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_roll_back_never_loads_the_attempted_set() {
    // #1678 / #1685: a failed change rolls back; the model only the attempted set had is never
    // loaded and stays protected in `previous`.
    let f = fixture();
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    *f.router.stuck.lock().unwrap() = true;
    f.router.set(&[(NOMIC, false)]);
    f.service
        .apply(serve(&[(NOMIC, NOMIC_FILE)]))
        .await
        .unwrap();
    *f.control.fail.lock().unwrap() = true;
    assert!(f
        .service
        .apply(serve(&[(GEMMA, GEMMA_FILE)]))
        .await
        .is_err());
    let saved = f.service.saved().unwrap().unwrap();
    assert_eq!(saved.models, vec![entry(NOMIC, NOMIC_FILE)], "rolled back");
    f.router.set(&[(NOMIC, true), (GEMMA, true)]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let loads = f.router.loads.lock().unwrap().clone();
    assert!(
        !loads.iter().any(|m| m == GEMMA),
        "the attempted model is never loaded: {loads:?}"
    );
    assert!(
        f.service
            .saved()
            .unwrap()
            .unwrap()
            .previous
            .contains(&entry(GEMMA, GEMMA_FILE)),
        "the attempted model stays protected while the router may still hold it"
    );
}

#[tokio::test]
async fn a_stop_then_failed_start_never_lets_the_old_warm_up_load() {
    // #1696: [A] warming (stuck router), stop succeeds, [A] again fails in Docker and rolls back
    // to off. A late Docker restart lists A unloaded: nothing may load it.
    let f = fixture();
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    *f.router.stuck.lock().unwrap() = true;
    f.router.set(&[]);
    f.service
        .apply(serve(&[(NOMIC, NOMIC_FILE)]))
        .await
        .unwrap();
    f.service.apply(Desired::default()).await.unwrap();
    *f.control.fail.lock().unwrap() = true;
    assert!(f
        .service
        .apply(serve(&[(NOMIC, NOMIC_FILE)]))
        .await
        .is_err());
    assert!(
        f.service.saved().unwrap().unwrap().models.is_empty(),
        "rolled back to off"
    );
    f.router.loads.lock().unwrap().clear();
    f.router.set(&[(NOMIC, false)]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let loads = f.router.loads.lock().unwrap().clone();
    assert!(
        loads.is_empty(),
        "no stale warm-up loads after roll-back to off: {loads:?}"
    );
}

/// Docker that answers until told to hang (#1682).
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

#[tokio::test]
async fn a_roll_back_warms_the_restored_set_again() {
    // #1682: apply [A,B] succeeds while its warm-up is still loading B; apply [B] then times out
    // in Docker and rolls back to [A,B]. The roll-back stops the old warm-up, so it must start a
    // new one, or B could stay unloaded and `previous` never be released.
    let dir = tempfile::tempdir().unwrap();
    let base = fixture();
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    let control = Arc::new(HangLater::default());
    let service = Arc::new(
        EmbeddingService::new(
            "cowork-embed-1".into(),
            base.dir.path().join("models"),
            "/models".into(),
            dir.path().join("config/embedding.ini"),
            dir.path().join("data/embedding-service.json"),
            control.clone(),
            base.router.clone(),
        )
        .with_warm(Duration::from_millis(5), 200)
        .with_act_timeout(Duration::from_millis(50)),
    );
    service.seed(NOMIC, NOMIC_FILE).unwrap();
    *base.router.stuck.lock().unwrap() = true;
    base.router.set(&[(NOMIC, true), (GEMMA, false)]);
    service
        .apply(serve(&[(NOMIC, NOMIC_FILE), (GEMMA, GEMMA_FILE)]))
        .await
        .unwrap();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        if !base.router.loads.lock().unwrap().is_empty() {
            break;
        }
    }
    assert!(
        base.router.loads.lock().unwrap().iter().any(|m| m == GEMMA),
        "the [A,B] warm-up is loading B"
    );
    *control.hang.lock().unwrap() = true;
    assert!(matches!(
        service.apply(serve(&[(GEMMA, GEMMA_FILE)])).await,
        Err(Refusal::Unavailable(_))
    ));
    assert_eq!(
        service.saved().unwrap().unwrap().models,
        vec![entry(NOMIC, NOMIC_FILE), entry(GEMMA, GEMMA_FILE)],
        "rolled back to [A,B]"
    );
    base.router.loads.lock().unwrap().clear();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let loads = base.router.loads.lock().unwrap().clone();
    assert!(
        loads.iter().any(|m| m == GEMMA),
        "the restored set still receives load calls after the roll-back: {loads:?}"
    );
}

/// #1717: a watched service with a short probe interval, seeded with nomic and settled (no
/// boot warm-up running).
fn watched(attempts: u32) -> Fixture {
    let f = fixture_with(|s| {
        s.with_warm(Duration::from_millis(5), attempts)
            .with_watch(Duration::from_millis(20))
    });
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    f
}

#[tokio::test]
async fn an_embed_restart_behind_our_back_is_warmed_within_the_probe_interval() {
    let f = watched(50);
    f.router.set(&[(NOMIC, true)]);
    let watcher = f.service.watch();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        f.router.loads.lock().unwrap().is_empty(),
        "nothing to do while loaded"
    );
    // The container restarts on its own: the router lists the preset, loads nothing.
    f.router.set(&[(NOMIC, false)]);
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        if f.router.models.lock().unwrap()[0].loaded {
            break;
        }
    }
    watcher.abort();
    assert_eq!(*f.router.loads.lock().unwrap(), vec![NOMIC.to_owned()]);
    assert!(f.router.models.lock().unwrap()[0].loaded);
    assert!(
        f.control.actions.lock().unwrap().is_empty(),
        "the watcher never restarts or stops anything"
    );
}

#[tokio::test]
async fn the_watcher_never_warms_while_a_change_holds_the_lock() {
    let f = watched(50);
    f.router.set(&[(NOMIC, false)]);
    let guard = f.service.hold().await;
    let watcher = f.service.watch();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        f.router.loads.lock().unwrap().is_empty(),
        "an in-flight apply owns the service"
    );
    drop(guard);
    tokio::time::sleep(Duration::from_millis(150)).await;
    watcher.abort();
    assert_eq!(*f.router.loads.lock().unwrap(), vec![NOMIC.to_owned()]);
}

#[tokio::test]
async fn the_watcher_never_warms_when_embedding_is_off() {
    let f = watched(50);
    f.service.apply(serve(&[])).await.unwrap();
    // Even if something is listed, an off service loads nothing.
    f.router.set(&[(NOMIC, false), (GEMMA, false)]);
    let watcher = f.service.watch();
    tokio::time::sleep(Duration::from_millis(200)).await;
    watcher.abort();
    assert!(f.router.loads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_watcher_only_loads_the_saved_models() {
    let f = watched(50);
    f.router.set(&[(NOMIC, false), (GEMMA, false)]);
    let watcher = f.service.watch();
    tokio::time::sleep(Duration::from_millis(200)).await;
    watcher.abort();
    let loads = f.router.loads.lock().unwrap().clone();
    assert!(!loads.is_empty());
    assert!(loads.iter().all(|m| m == NOMIC), "{loads:?}");
}

#[tokio::test]
async fn a_failing_re_warm_backs_off() {
    // Each warm-up tries twice (2 loads) and fails; without back-off a 20 ms probe would start
    // one about every 30 ms, i.e. ~40 loads in 600 ms.
    let f = watched(2);
    *f.router.stuck.lock().unwrap() = true;
    f.router.set(&[(NOMIC, false)]);
    let watcher = f.service.watch();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let failing = f.router.loads.lock().unwrap().len();
    assert!(failing >= 2, "it tried at least once: {failing}");
    assert!(failing <= 12, "back-off limits retries: {failing}");
    // Once the router can load again, the watcher recovers on a later probe.
    *f.router.stuck.lock().unwrap() = false;
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if f.router.models.lock().unwrap()[0].loaded {
            break;
        }
    }
    watcher.abort();
    assert!(f.router.models.lock().unwrap()[0].loaded);
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

#[test]
fn image_presets_are_off_unless_multimodal_rag_is_on() {
    let f = fixture();
    with_projector(&f, &["mmproj-embeddinggemma-2-F16.gguf"]);
    f.service.seed(GEMMA, GEMMA_FILE).unwrap();
    assert!(!preset(&f).contains(".vision"), "{}", preset(&f));
    assert!(!preset(&f).contains("mmproj"));
}

#[tokio::test]
async fn a_projector_beside_the_model_gets_its_own_preset_that_never_loads_at_start() {
    let f = fixture_with(|s| s.with_vision(true));
    with_projector(&f, &["mmproj-embeddinggemma-2-F16.gguf"]);
    f.service.seed(GEMMA, GEMMA_FILE).unwrap();
    let text = preset(&f);
    assert!(
        text.contains("\n[embeddinggemma-2]\nmodel = /models/embeddinggemma-2/embeddinggemma-2-Q8_0.gguf\n\n[embeddinggemma-2.vision]\nmodel = /models/embeddinggemma-2/embeddinggemma-2-Q8_0.gguf\nmmproj = /models/embeddinggemma-2/mmproj-embeddinggemma-2-F16.gguf\nload-on-startup = false\n"),
        "{text}"
    );
    // The text preset itself carries no projector: text embedding never loads it.
    assert_eq!(text.matches("mmproj = ").count(), 1);
    // Status names the alias for web (rag.cjs mediaModelFor).
    f.router
        .set(&[(GEMMA, false), ("embeddinggemma-2.vision", false)]);
    let status = f.service.status().await;
    assert_eq!(status["vision"][0]["model"], GEMMA);
    assert_eq!(status["vision"][0]["alias"], "embeddinggemma-2.vision");
    assert_eq!(
        status["vision"][0]["mmproj"],
        "embeddinggemma-2/mmproj-embeddinggemma-2-F16.gguf"
    );
    // A warm-up loads the text model only.
    f.service.warm(&[GEMMA.to_string()]).await;
    assert_eq!(
        f.router.loads.lock().unwrap().clone(),
        vec![GEMMA.to_string()]
    );
}

#[test]
fn a_model_without_exactly_one_projector_gets_no_image_preset() {
    let f = fixture_with(|s| s.with_vision(true));
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    assert!(!preset(&f).contains(".vision"));
    let g = fixture_with(|s| s.with_vision(true));
    with_projector(&g, &["mmproj-a.gguf", "mmproj-b.gguf"]);
    g.service.seed(GEMMA, GEMMA_FILE).unwrap();
    assert!(!preset(&g).contains(".vision"), "ambiguous: two projectors");
}

#[tokio::test]
async fn the_projector_of_a_served_model_cannot_be_deleted() {
    let f = fixture_with(|s| s.with_vision(true));
    with_projector(&f, &["mmproj-embeddinggemma-2-F16.gguf"]);
    f.service.seed(GEMMA, GEMMA_FILE).unwrap();
    f.router.set(&[(GEMMA, true)]);
    assert!(
        f.service
            .protects("embeddinggemma-2/mmproj-embeddinggemma-2-F16.gguf")
            .await
    );
    assert!(f.service.protects("mmproj-embeddinggemma-2-F16").await);
    let off = fixture();
    with_projector(&off, &["mmproj-embeddinggemma-2-F16.gguf"]);
    off.service.seed(NOMIC, NOMIC_FILE).unwrap();
    off.router.set(&[(NOMIC, true)]);
    assert!(
        !off.service
            .protects("embeddinggemma-2/mmproj-embeddinggemma-2-F16.gguf")
            .await
    );
}

#[tokio::test]
async fn a_loaded_image_preset_does_not_hold_back_releasing_the_old_model() {
    let f = fixture_with(|s| s.with_vision(true));
    with_projector(&f, &["mmproj-embeddinggemma-2-F16.gguf"]);
    f.service.seed(NOMIC, NOMIC_FILE).unwrap();
    f.router.set(&[(NOMIC, true)]);
    f.service
        .apply(serve(&[(GEMMA, GEMMA_FILE)]))
        .await
        .unwrap();
    assert_eq!(f.service.saved().unwrap().unwrap().previous.len(), 1);
    // The router restarted onto the new preset: the new model and its image preset are loaded.
    f.router
        .set(&[(GEMMA, true), ("embeddinggemma-2.vision", true)]);
    settle(&f).await;
    assert!(f.service.saved().unwrap().unwrap().previous.is_empty());
}
