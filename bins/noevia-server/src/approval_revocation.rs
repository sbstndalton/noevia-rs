//! Completion barrier for every revocation, including context invalidation: record it first, then have Node advance
//! its executor epoch. Time and wrapper-entry reports never establish that a write was sent.
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

#[derive(Debug)]
pub struct Pending;

#[derive(Clone)]
pub struct Revoker {
    client: Option<reqwest::Client>,
    base: String,
    token: Option<HeaderValue>,
    cleanup: Arc<Semaphore>,
}
impl std::fmt::Debug for Revoker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Revoker { .. }")
    }
}
impl Revoker {
    pub fn new(base: String, token: Option<String>) -> Self {
        let token = token
            .filter(|t| !t.is_empty())
            .and_then(|t| HeaderValue::from_str(&t).ok())
            .map(|mut t| {
                t.set_sensitive(true);
                t
            });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .ok();
        Self {
            client,
            base,
            token,
            cleanup: Arc::new(Semaphore::new(1)),
        }
    }
    pub async fn ack(&self, user: &str, chat: Option<&str>) -> Result<(), Pending> {
        let client = self.client.as_ref().ok_or(Pending)?;
        let token = self.token.as_ref().ok_or(Pending)?;
        // Covers response body consumption as well as connection and response headers.
        tokio::time::timeout(Duration::from_secs(2), async {
            let response = client
                .post(format!("{}/internal/approvals/revoke", self.base))
                .header(
                    HeaderName::from_static("x-noevia-store-token"),
                    token.clone(),
                )
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(
                    json!({"user":user,"chat":chat,"protocol":crate::approval_registry::PROTOCOL})
                        .to_string(),
                )
                .send()
                .await
                .map_err(|_| Pending)?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(Pending);
            }
            let bytes = response.bytes().await.map_err(|_| Pending)?;
            let answer: Value = serde_json::from_slice(&bytes).map_err(|_| Pending)?;
            if answer.get("acknowledged").and_then(Value::as_bool) == Some(true)
                && answer.get("protocol").and_then(Value::as_str)
                    == Some(crate::approval_registry::PROTOCOL)
            {
                Ok(())
            } else {
                Err(Pending)
            }
        })
        .await
        .map_err(|_| Pending)?
    }
}
impl crate::App {
    /// Only the requested scope gates completion. Its generation is captured before the await;
    /// a concurrent newer invalidation cannot be retired by this acknowledgement.
    async fn target_approval_fence(&self, user: &str, chat: Option<&str>) -> Result<(), Pending> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let cover = self.approvals.covering_fence().map_err(|_| Pending)?;
            self.config.approval_revocation.ack(user, chat).await?;
            self.approvals
                .acknowledge_through(user, chat, cover.through)
                .map_err(|_| Pending)
        })
        .await
        .map_err(|_| Pending)?
    }

    /// At most one bounded cleanup runs per peer. No caller awaits unrelated acknowledgements;
    /// each successful scope retires its covered generations, and failures stay retryable.
    pub fn drain_approval_invalidations(&self) {
        let revoker = self.config.approval_revocation.clone();
        let Ok(permit) = Arc::clone(&revoker.cleanup).try_acquire_owned() else {
            return;
        };
        let registry = Arc::clone(&self.approvals);
        tokio::spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                let Ok(cover) = registry.covering_fence() else {
                    return;
                };
                for (user, chat) in cover.chats {
                    if revoker.ack(&user, Some(&chat)).await.is_ok() {
                        let _ = registry.acknowledge_through(&user, Some(&chat), cover.through);
                    }
                }
            })
            .await;
        });
    }

    pub async fn confirm_approval_invalidation(
        &self,
        user: &str,
        chat: &str,
        mut generation: Option<u64>,
    ) -> Result<(), Pending> {
        tokio::time::timeout(Duration::from_secs(2), async {
            while generation.is_some() {
                self.target_approval_fence(user, Some(chat)).await?;
                generation = self
                    .approvals
                    .pending_invalidation(user, chat)
                    .map_err(|_| Pending)?;
            }
            Ok::<(), Pending>(())
        })
        .await
        .map_err(|_| Pending)??;
        self.drain_approval_invalidations();
        Ok(())
    }

    pub async fn revoke_approvals(&self, user: &str, chat: &str) -> Result<bool, Pending> {
        let revoked = self.approvals.revoke(user, chat);
        self.target_approval_fence(user, Some(chat)).await?;
        self.drain_approval_invalidations();
        Ok(revoked)
    }
    pub async fn revoke_project_approvals(
        &self,
        user: &str,
        project: &str,
    ) -> Result<usize, Pending> {
        let revoked = self.approvals.revoke_project(user, project);
        self.target_approval_fence(user, None).await?;
        self.drain_approval_invalidations();
        Ok(revoked)
    }
}
