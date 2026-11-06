#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use embedding_worker::{
    service::{self, Loader, Service},
    service_owner, Task,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::sync::Notify;

#[derive(Clone)]
struct Fake {
    entered: Arc<Notify>,
    settle: Arc<Notify>,
    calls: Arc<AtomicUsize>,
    effects: Arc<Mutex<Vec<Value>>>,
}
impl Fake {
    fn new() -> Self {
        Self {
            entered: Arc::new(Notify::new()),
            settle: Arc::new(Notify::new()),
            calls: Arc::new(AtomicUsize::new(0)),
            effects: Arc::new(Mutex::new(vec![])),
        }
    }
}
impl Loader for Fake {
    fn get(&self) -> Task<'_, Result<(u16, Value), String>> {
        Box::pin(async {
            Ok((
                200,
                json!({"managed":true,"enabled":true,"running":{"loaded":["synthetic"]}}),
            ))
        })
    }
    fn put<'a>(&'a self, models: &'a Value) -> Task<'a, Result<(u16, Value), String>> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                self.settle.notified().await;
            }
            self.effects.lock().unwrap().push(models.clone());
            Ok((200, json!({"managed":true,"enabled":true,"models":models})))
        })
    }
}
#[tokio::test]
async fn disconnected_caller_keeps_turn_until_mutation_and_restore_settle() {
    let fake = Fake::new();
    let (client, owner) = service_owner::start(fake.clone());
    let first = client.clone();
    let caller = tokio::spawn(async move {
        first
            .execute(|service, loader| {
                Box::pin(async move {
                    service
                        .serve(
                            loader,
                            &json!([{"model":"temporary","file":"temp.gguf"}]),
                            || 1,
                        )
                        .await
                        .unwrap();
                    // Same turn includes failed-load restoration.
                    service.serve(loader, &json!([]), || 2).await.unwrap();
                    Value::Null
                })
            })
            .await
    });
    fake.entered.notified().await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let second = client.clone();
    let queued = tokio::spawn(async move {
        second
            .execute(|service, loader| {
                Box::pin(async move {
                    service
                        .serve(
                            loader,
                            &json!([{"model":"after","file":"after.gguf"}]),
                            || 3,
                        )
                        .await
                        .unwrap();
                    Value::Null
                })
            })
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert!(fake.effects.lock().unwrap().is_empty());
    fake.settle.notify_one();
    queued.await.unwrap().unwrap();
    drop(client);
    owner.await.unwrap();
    assert_eq!(
        *fake.effects.lock().unwrap(),
        vec![
            json!([{"model":"temporary","file":"temp.gguf"}]),
            json!([]),
            json!([{"model":"after","file":"after.gguf"}])
        ]
    );
}

#[tokio::test]
async fn poll_wait_and_sticky_service_state() {
    let fake = Fake::new();
    let mut service = Service::default();
    assert!(
        service
            .wait_loaded(
                &fake,
                &["synthetic".into()],
                std::time::Duration::ZERO,
                || 1
            )
            .await
    );
    assert!(
        !service
            .wait_loaded(&fake, &["missing".into()], std::time::Duration::ZERO, || 2)
            .await
    );
    assert!(service.enabled());
}
struct Refused;
impl Loader for Refused {
    fn get(&self) -> Task<'_, Result<(u16, Value), String>> {
        Box::pin(async { Err("synthetic-reflected-secret".into()) })
    }
    fn put<'a>(&'a self, _: &'a Value) -> Task<'a, Result<(u16, Value), String>> {
        Box::pin(async { Ok((401, json!({"detail":"synthetic-reflected-secret"}))) })
    }
}
#[tokio::test]
async fn upstream_errors_never_enter_public_mutation_or_sticky_state() {
    let fake = Fake::new();
    let mut service = Service::default();
    service.refresh(&fake, || 1).await;
    service.refresh(&Refused, || 2).await;
    assert!(!service
        .status(2)
        .unwrap()
        .to_string()
        .contains("synthetic-reflected-secret"));
    let error = service.serve(&Refused, &json!([]), || 2).await.unwrap_err();
    assert_eq!(error.status, 502);
    assert_eq!(error.code, "serviceRefused");
    assert_eq!(
        error.message,
        "The embedding service could not be changed: HTTP 401"
    );
}
#[test]
fn installed_inventory_preserves_first_valid_match() {
    let inventory = json!([{"modelId":"wanted","file":"wrong.gguf","companion":true},{"modelId":"wanted","file":"wrong.txt"},{"sections":["wanted"],"file":"dir/right.GGUF"},{"stem":"wanted","file":"later.gguf"}]);
    assert_eq!(
        service::installed_file(&inventory, "wanted"),
        Some("dir/right.GGUF".into())
    );
    assert_eq!(service::installed_file(&inventory, "unknown"), None);
}

struct Inventory;
impl Loader for Inventory {
    fn get(&self) -> Task<'_, Result<(u16, Value), String>> {
        Box::pin(async {
            Ok((
                200,
                json!({"managed":true,"enabled":true,"models":[{"model":"a","file":"a.gguf"},{"model":"b","file":"b.gguf"}],"previous":[{"model":"c","file":"c.gguf"}]}),
            ))
        })
    }
}
#[tokio::test]
async fn preserve_diary_default_and_refuse_required_overflow_before_shrinking() {
    let mut service = Service::default();
    service.refresh(&Inventory, || 1).await;
    let projects = json!({"active":{"model":"a","baseUrl":"http://service/v1"},"next":{"model":"b","baseUrl":"http://service"},"previous":{"model":"c","baseUrl":"http://service"}});
    let diary = json!({"active":{"model":"c","base_url":""}});
    assert!(
        service::served_set(&service, Some("http://service"), &projects, &diary, true).is_none()
    );
    assert_eq!(
        service::served_set(&service, Some("http://service"), &projects, &diary, false),
        Some(vec![
            json!({"model":"a","file":"a.gguf"}),
            json!({"model":"b","file":"b.gguf"})
        ])
    );
    let projects = json!({"active":{"model":"missing","baseUrl":"http://service"},"previous":{"model":"a","baseUrl":"http://service"}});
    assert_eq!(
        service::served_set(&service, Some("http://service"), &projects, &diary, true),
        Some(vec![
            json!({"model":"c","file":"c.gguf"}),
            json!({"model":"a","file":"a.gguf"})
        ])
    );
}
