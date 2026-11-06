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

fn permit(dir: &std::path::Path) -> (std::path::PathBuf, model_publication::Fence) {
    let path = dir.join("authority.sqlite");
    let guard = model_publication::Guard::at(&path).unwrap();
    guard.authority().unwrap();
    guard
        .save_authority(&model_publication::AuthorityRecord {
            incarnation: "fixture-epoch".into(),
            lease: "fixture-lease".into(),
            generation: 1,
            active: true,
            until_ms: model_publication::now_ms() + 60_000,
            recovery: false,
        })
        .unwrap();
    let fence = model_publication::Fence {
        incarnation: "fixture-epoch".into(),
        lease: "fixture-lease".into(),
        generation: 1,
        operation: "fixture-operation".into(),
        kind: "embedding-service".into(),
        expected_revision: None,
    };
    guard.begin(&fence).unwrap();
    guard.commit().unwrap();
    (path, fence)
}
fn receipt(path: &std::path::Path, fence: &model_publication::Fence) -> String {
    let guard = model_publication::Guard::at(path).unwrap();
    let state = guard.status(fence).unwrap().unwrap().state;
    guard.commit().unwrap();
    state
}
#[tokio::test]
async fn shared_permit_is_single_use_and_settles_after_late_docker() {
    let (dir, service, docker) = fixture();
    let (path, fence) = permit(dir.path());
    let operation =
        noevia_model_manager::embedding::Operation::new(path.clone(), fence.clone()).unwrap();
    assert!(noevia_model_manager::embedding::Operation::new(path.clone(), fence.clone()).is_err());
    let s = service.clone();
    let call = tokio::spawn(async move { s.apply_operation(desired(), Some(operation)).await });
    docker.entered.notified().await;
    assert!(call.await.unwrap().is_err());
    assert_eq!(receipt(&path, &fence), "running");
    assert!(
        tokio::time::timeout(Duration::from_millis(30), service.hold())
            .await
            .is_err()
    );
    docker.finish.notify_one();
    let _held = service.hold().await;
    assert_eq!(receipt(&path, &fence), "settled");
}
#[tokio::test]
async fn lost_generation_after_docker_never_runs_followup_publication_or_restore() {
    let (dir, service, docker) = fixture();
    let (path, fence) = permit(dir.path());
    let operation =
        noevia_model_manager::embedding::Operation::new(path.clone(), fence.clone()).unwrap();
    let s = service.clone();
    let call = tokio::spawn(async move { s.apply_operation(desired(), Some(operation)).await });
    docker.entered.notified().await;
    let before = std::fs::read(dir.path().join("embedding-service.json")).unwrap();
    let guard = model_publication::Guard::at(&path).unwrap();
    let mut authority = guard.authority().unwrap().unwrap();
    authority.generation += 1;
    guard.save_authority(&authority).unwrap();
    guard.commit().unwrap();
    docker.finish.notify_one();
    assert!(call.await.unwrap().is_err());
    let _held = service.hold().await;
    assert_eq!(receipt(&path, &fence), "recovery");
    assert_eq!(
        std::fs::read(dir.path().join("embedding-service.json")).unwrap(),
        before
    );
}
#[test]
fn stale_shared_permit_cannot_begin_a_container_effect() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut fence) = permit(dir.path());
    fence.generation += 1;
    assert!(noevia_model_manager::embedding::Operation::new(path, fence).is_err());
}
