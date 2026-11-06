//! Explicit gate settlement handshake. Call again after a connection failure; a health answer,
//! configured credential or endpoint name never establishes settlement ownership.
use crate::model_maint::router::Router;
use reqwest::header::{HeaderName, HeaderValue};
use std::time::Duration;

pub async fn verified(router: &Router, admin: Option<(HeaderName, HeaderValue)>) -> bool {
    let Some(admin) = admin else { return false };
    let Ok(reply) = router
        .request_with(
            reqwest::Method::GET,
            "/gate/v1/capabilities",
            None,
            None,
            &[admin],
            Duration::from_secs(5),
        )
        .await
    else {
        return false;
    };
    if reply.status != 200 {
        return false;
    }
    let Some(body) = reply.body else { return false };
    body.get("protocol").and_then(serde_json::Value::as_str) == Some("noevia-gate-capabilities/1")
        && body
            .get("capabilities")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .filter(|v| v.as_str() == Some("noevia-model-load-settlement/1"))
                    .count()
                    == 1
            })
}
