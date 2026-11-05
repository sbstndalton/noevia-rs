//! Completion barrier for every explicit revocation: record it first, then have Node advance
//! its executor epoch. Time and wrapper-entry reports never establish that a write was sent.
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug)]
pub struct Pending;

#[derive(Clone)]
pub struct Revoker {
    client: Option<reqwest::Client>,
    base: String,
    token: Option<HeaderValue>,
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
    pub async fn revoke_approvals(&self, user: &str, chat: &str) -> Result<bool, Pending> {
        let revoked = self.approvals.revoke(user, chat);
        self.config
            .approval_revocation
            .ack(user, Some(chat))
            .await?;
        Ok(revoked)
    }
    pub async fn revoke_project_approvals(
        &self,
        user: &str,
        project: &str,
    ) -> Result<usize, Pending> {
        let revoked = self.approvals.revoke_project(user, project);
        // User-wide fence includes tickets whose grants were already removed or expired.
        self.config.approval_revocation.ack(user, None).await?;
        Ok(revoked)
    }
}
