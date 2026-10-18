//! #1318: bounded native validation of the stored Hub token. No token is logged.
use crate::settings::{self, Stored};
use futures_util::StreamExt;
use model_files::json::Value as Json;
use std::{
    path::Path,
    time::{Duration, Instant},
};
const BUDGET: Duration = Duration::from_secs(10);
const CAP: usize = 32 * 1024;
pub(crate) struct TokenTest {
    client: reqwest::Client,
    endpoint: String,
}
fn answer(ok: bool, message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"ok":ok,"message":message})).unwrap_or_default()
}
fn unavailable() -> Vec<u8> {
    crate::settings_write::TEST_UNAVAILABLE.as_bytes().to_vec()
}
impl TokenTest {
    pub(crate) fn new() -> Result<Self, &'static str> {
        Self::with(&format!("{}/whoami-v2", crate::hub_page::HUB_API))
    }
    fn with(endpoint: &str) -> Result<Self, &'static str> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(BUDGET)
                .read_timeout(BUDGET)
                .timeout(BUDGET)
                // httpx does not follow redirects by default. Never forward credentials.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| "native token-test client unavailable")?,
            endpoint: endpoint.into(),
        })
    }
    pub(crate) async fn run(&self, database: &Path) -> Vec<u8> {
        let database = database.to_owned();
        let deadline = Instant::now() + Duration::from_millis(4500);
        let token =
            crate::docker::spawn_blocking_admitted(move || settings::read(&database, deadline))
                .await;
        let token = match token {
            Ok(Ok(None)) => String::new(),
            Ok(Ok(Some(Stored::Text(token)))) => token,
            Ok(Ok(Some(Stored::Blob(bytes)))) if bytes.is_empty() => String::new(),
            _ => return unavailable(),
        };
        if token.is_empty() {
            return answer(true, "anonymous (no token)");
        }
        // Match the text-only ordinary token contract. Refuse malformed headers without
        // propagating transport errors, which can contain credential material.
        if !token.is_ascii() || token.bytes().any(|b| b < 32 || b == 127) {
            return unavailable();
        }
        tokio::time::timeout(BUDGET, self.request(&token))
            .await
            .unwrap_or_else(|_| unavailable())
    }
    async fn request(&self, token: &str) -> Vec<u8> {
        let response = match self
            .client
            .get(&self.endpoint)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return unavailable(),
        };
        let status = response.status().as_u16();
        // The native path supports UTF-8/ASCII response text. Other declared encodings
        // refuse instead of guessing an HTTP-error message.
        if let Some(content) = response.headers().get(reqwest::header::CONTENT_TYPE) {
            let Ok(content) = content.to_str() else {
                return unavailable();
            };
            for part in content.split(';').skip(1) {
                if let Some((key, value)) = part.trim().split_once('=') {
                    if key.trim().eq_ignore_ascii_case("charset") {
                        let value = value.trim().trim_matches('"');
                        if !["utf-8", "utf8", "us-ascii", "ascii"]
                            .iter()
                            .any(|v| value.eq_ignore_ascii_case(v))
                        {
                            return unavailable();
                        }
                    }
                }
            }
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                return unavailable();
            };
            if bytes.len().saturating_add(chunk.len()) > CAP {
                return unavailable();
            }
            bytes.extend_from_slice(&chunk);
        }
        render(status, &bytes, token).unwrap_or_else(unavailable)
    }
}
fn render(status: u16, bytes: &[u8], token: &str) -> Option<Vec<u8>> {
    let text = String::from_utf8_lossy(bytes);
    let message = if status == 200 {
        let value = model_files::json::parse(&text).ok()?;
        let Json::Obj(_) = value else {
            return None;
        };
        let name = value
            .get("name")
            .filter(|v| v.truthy())
            .or_else(|| value.get("fullname").filter(|v| v.truthy()));
        let name = name
            .map_or_else(|| Ok("unknown".into()), |v| crate::prompts::py_str(Some(v)))
            .ok()?;
        format!("authenticated as {name}")
    } else {
        let prefix: String = text.chars().take(120).collect();
        format!("HTTP {status}: {prefix}")
    };
    // The upstream error body is untrusted and may echo the header; never return the token.
    let message = if token.is_empty() {
        message
    } else {
        message.replace(token, "[redacted]")
    };
    Some(answer(status == 200, &message))
}
#[cfg(test)]
mod tests;
