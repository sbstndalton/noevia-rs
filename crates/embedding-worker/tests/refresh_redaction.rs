#![allow(clippy::unwrap_used)]
use embedding_worker::{
    service::{Loader, Service},
    Task,
};
use serde_json::{json, Value};
struct LoaderReply(bool);
impl Loader for LoaderReply {
    fn get(&self) -> Task<'_, Result<(u16, Value), String>> {
        Box::pin(async move {
            if self.0 {
                Ok((200, json!({"managed":true,"enabled":true})))
            } else {
                Err("synthetic-reflected-secret".into())
            }
        })
    }
}
#[tokio::test]
async fn failed_refresh_cannot_publish_a_transport_secret() {
    let mut service = Service::default();
    service.refresh(&LoaderReply(true), || 1).await;
    service.refresh(&LoaderReply(false), || 2).await;
    assert_eq!(
        service
            .status(2)
            .unwrap()
            .get("lastError")
            .and_then(Value::as_str),
        Some("embedding HTTP unavailable")
    );
    assert!(!service
        .status(2)
        .unwrap()
        .to_string()
        .contains("synthetic-reflected-secret"));
}
