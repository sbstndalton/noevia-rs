//! Compatibility activity registration, deliberately separate from an exclusive Maint hold.
//! J/H will swap this implementation for the native authority. Route policy stays unchanged.
use crate::App;
use rand_core::{OsRng, RngCore};
use serde_json::json;
use std::{future::Future, sync::Arc, time::Duration};

pub struct ActivityError {
    pub status: u16,
    pub message: String,
}
pub trait MaintenanceActivity {
    fn enter(&self) -> impl Future<Output = Result<ActivityGuard, ActivityError>> + Send;
}
pub struct NodeActivity(pub Arc<App>);
pub struct ActivityGuard {
    app: Arc<App>,
    ticket: String,
    released: bool,
}
impl ActivityGuard {
    pub(super) fn ticket(&self) -> &str {
        &self.ticket
    }
    async fn call(&self, op: &str, within: Duration) -> Result<(), ActivityError> {
        let answer = self
            .app
            .config
            .research
            .node_call(
                &format!("/internal/model-mutations/{op}"),
                &json!({"ticket":self.ticket,"withinMs":within.as_millis().min(600000) as u64}),
                Duration::from_secs(5),
            )
            .await;
        if let Some((status, bytes)) = answer {
            if let Ok(body) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if status == 200
                    && body.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
                {
                    return Ok(());
                }
                if status == 503 {
                    return Err(ActivityError {
                        status,
                        message: body
                            .get("error")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("Model management is unavailable. Try again shortly.")
                            .to_owned(),
                    });
                }
            }
        }
        Err(ActivityError {
            status: 503,
            message: "Model management is unavailable. Try again shortly.".into(),
        })
    }
    pub async fn confirm(&self, within: Duration) -> Result<(), ActivityError> {
        self.call("touch", within).await
    }
    pub async fn leave(mut self) {
        self.released = self.call("leave", Duration::from_secs(1)).await.is_ok();
    }
}
impl MaintenanceActivity for NodeActivity {
    async fn enter(&self) -> Result<ActivityGuard, ActivityError> {
        let mut bytes = [0u8; 32];
        OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| ActivityError {
                status: 503,
                message: "Model management is unavailable. Try again shortly.".into(),
            })?;
        let ticket = bytes.iter().map(|b| format!("{b:02x}")).collect();
        // Guard exists before the first await: cancellation sends a tombstoned leave even if
        // Node's acquire is still in flight. No post-response guard allocation window.
        let guard = ActivityGuard {
            app: Arc::clone(&self.0),
            ticket,
            released: false,
        };
        guard.call("enter", Duration::from_secs(120)).await?;
        Ok(guard)
    }
}
impl Drop for ActivityGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let app = Arc::clone(&self.app);
        let ticket = self.ticket.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = app
                    .config
                    .research
                    .node_call(
                        "/internal/model-mutations/leave",
                        &json!({"ticket":ticket}),
                        Duration::from_secs(5),
                    )
                    .await;
            });
        }
    }
}
