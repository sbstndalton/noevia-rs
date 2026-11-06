//! Synthetic Docker effects must settle before delete or another service writer enters.
#![allow(clippy::unwrap_used)]
use noevia_model_manager::{
    embedding::{Action, Desired, EmbeddingService, Entry, Probe, Served, ServiceControl},
    Job,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;
#[derive(Default)]
struct Docker {
    entered: Notify,
    finish: Notify,
}
impl ServiceControl for Docker {
    fn act(&self, _: Action) -> Job<'_, ()> {
        Box::pin(async move {
            self.entered.notify_one();
            self.finish.notified().await;
            Ok(())
        })
    }
}
struct Router;
impl Probe for Router {
    fn models(&self) -> Job<'_, Vec<Served>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn load<'a>(&'a self, _: &'a str) -> Job<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}
fn fixture() -> (tempfile::TempDir, Arc<EmbeddingService>, Arc<Docker>) {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    std::fs::write(models.join("synthetic.gguf"), b"GGUF").unwrap();
    let docker = Arc::new(Docker::default());
    let service = Arc::new(
        EmbeddingService::new(
            "synthetic-embed".into(),
            models,
            "/models".into(),
            dir.path().join("embedding.ini"),
            dir.path().join("embedding-service.json"),
            docker.clone(),
            Arc::new(Router),
        )
        .with_act_timeout(Duration::from_millis(30)),
    );
    (dir, service, docker)
}
fn desired() -> Desired {
    Desired {
        models: vec![Entry {
            model: "synthetic".into(),
            file: "synthetic.gguf".into(),
        }],
    }
}
#[tokio::test]
async fn timeout_keeps_exclusion_until_docker_effect_settles() {
    let (_dir, service, docker) = fixture();
    let s = service.clone();
    let call = tokio::spawn(async move { s.apply(desired()).await });
    docker.entered.notified().await;
    assert!(tokio::time::timeout(Duration::from_secs(1), call)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), service.hold())
            .await
            .is_err(),
        "HTTP timeout released exclusion before late Docker effect"
    );
    docker.finish.notify_one();
    let _held = tokio::time::timeout(Duration::from_secs(1), service.hold())
        .await
        .unwrap();
    assert_eq!(service.saved().unwrap().unwrap().models, desired().models);
}
#[tokio::test]
async fn cancellation_keeps_exclusion_until_docker_effect_settles() {
    let (_dir, service, docker) = fixture();
    let s = service.clone();
    let call = tokio::spawn(async move { s.apply(desired()).await });
    docker.entered.notified().await;
    call.abort();
    let _ = call.await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), service.hold())
            .await
            .is_err(),
        "cancelled caller released exclusion before late Docker effect"
    );
    docker.finish.notify_one();
    let _held = tokio::time::timeout(Duration::from_secs(1), service.hold())
        .await
        .unwrap();
    assert_eq!(service.saved().unwrap().unwrap().models, desired().models);
}

#[tokio::test]
async fn cancellation_while_waiting_for_exclusion_dispatches_nothing() {
    let (_dir, service, docker) = fixture();
    let held = service.hold().await;
    let s = service.clone();
    let call = tokio::spawn(async move { s.apply(desired()).await });
    tokio::task::yield_now().await;
    call.abort();
    let _ = call.await;
    drop(held);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), docker.entered.notified())
            .await
            .is_err()
    );
    assert!(service.saved().unwrap().is_none());
}
