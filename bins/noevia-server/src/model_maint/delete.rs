//! Native router delete primitive. Public identity/protection/preset/reference policy is in
//! `mutations`; production uses `remove_model_registered` to retain and confirm the non-exclusive
//! activity registration across every bounded unload/delete attempt. No filesystem paths are
//! interpreted here: the existing native router remains the file/deletion authority.

use super::router::{Fault, Reply, Router};
use crate::storage_integration_routes::encode_uri_component;
use std::time::Duration;

/// Core's removeModel result: the last delete reply and whether either unload succeeded.
#[derive(Debug, Clone, PartialEq)]
pub struct Removed {
    pub reply: Reply,
    pub unloaded: bool,
}

impl Router {
    /// The caller must retain its active-mutation registration until this future finishes,
    /// including both retries. This method does not acquire or release maintenance itself.
    ///
    /// Failed/rejected unloads are tolerated; delete transport errors propagate without a retry.
    /// A settled refused delete gets exactly one further unload and one further delete. This is
    /// deliberately not retried by status: even a 404 or a gate refusal follows Core's same path.
    /// A supplied token is forwarded on every call; no admin override is sent.
    pub async fn remove_model(&self, model: &str, hold: Option<&str>) -> Result<Removed, Fault> {
        let mut unloaded = self.remove_unload(model, hold).await.is_ok_and(|r| r.ok);
        let path = format!("/models?model={}", encode_uri_component(model));
        let mut reply = self.delete_model_path(&path, hold).await?;
        if !reply.ok {
            unloaded |= self.remove_unload(model, hold).await.is_ok_and(|r| r.ok);
            reply = self.delete_model_path(&path, hold).await?;
        }
        Ok(Removed { reply, unloaded })
    }

    // removeModel uses post()'s default 120 s, unlike the maintenance router's 60 s unload.
    async fn remove_unload(&self, model: &str, hold: Option<&str>) -> Result<Reply, Fault> {
        self.request(
            reqwest::Method::POST,
            "/models/unload",
            Some(&serde_json::json!({"model": model})),
            hold,
            Duration::from_secs(120),
        )
        .await
    }

    async fn delete_model_path(&self, path: &str, hold: Option<&str>) -> Result<Reply, Fault> {
        let reply = self
            .request(
                reqwest::Method::DELETE,
                path,
                None,
                hold,
                Duration::from_secs(60),
            )
            .await?;
        if matches!(reply.status, 301 | 302 | 303 | 307 | 308) {
            return Err(Fault);
        }
        Ok(reply)
    }
}

/// Production caller retains the registration through every bounded engine step.
/// An ended registration is never reacquired to continue the old operation.
impl Router {
    pub async fn remove_model_registered(
        &self,
        model: &str,
        activity: &super::activity::ActivityGuard,
    ) -> Result<Removed, super::activity::ActivityError> {
        let fault = || super::activity::ActivityError {
            status: 500,
            message: "Internal error".into(),
        };
        let mut unloaded = false;
        let path = format!("/models?model={}", encode_uri_component(model));
        for attempt in 0..2 {
            activity.confirm(Duration::from_secs(120)).await?;
            unloaded |= self.remove_unload(model, None).await.is_ok_and(|r| r.ok);
            activity.confirm(Duration::from_secs(60)).await?;
            let reply = self
                .delete_model_path(&path, None)
                .await
                .map_err(|_| fault())?;
            if reply.ok || attempt == 1 {
                return Ok(Removed { reply, unloaded });
            }
        }
        Err(fault())
    }
}
