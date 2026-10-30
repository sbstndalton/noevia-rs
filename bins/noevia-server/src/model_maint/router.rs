//! The llama.cpp router, as the maintenance routes talk to it (llamacpp-manager.cjs `request`,
//! `rawModels`, http.cjs `fetchJson`): plain http, no redirects, no proxy, a per-call timeout and
//! the 8 MiB response cap. An answer is `{ ok, status, body }` where `body` is the parsed JSON or,
//! when the text is not JSON, the text itself (as `fetchJson` settles it). A transport failure, a
//! timeout or a redirect is an `Err`, the JS promise's rejection.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use std::time::Duration;

/// http.cjs `DEFAULT_MAX_RESPONSE_BYTES`.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// llamacpp-manager.cjs `rawRequest`'s default timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(8000);

/// What a request settles to (a rejection is `Err(Fault)`).
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub ok: bool,
    pub status: u16,
    /// `None`: the response was too large to read (fetchJson answers `{ok:false, error}` with no body).
    pub body: Option<Value>,
}

/// A request that did not settle: the engine did not answer, answered after the timeout, or
/// redirected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault;

#[derive(Clone)]
pub struct Router {
    client: reqwest::Client,
    base: String,
    authorization: Option<HeaderValue>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key.
        f.debug_struct("Router")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Router {
    /// `base` as the engine adapter holds it (no trailing slash, no `/v1`), the key as configured.
    pub fn new(base: &str, key: Option<&str>) -> Result<Router, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| format!("model router client: {e}"))?;
        let authorization = key
            .filter(|k| !k.is_empty() && *k != "local")
            .and_then(|k| HeaderValue::from_str(&format!("Bearer {k}")).ok());
        Ok(Router {
            client,
            base: base.to_owned(),
            authorization,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// One request. `hold` is the maintenance hold token this process holds (its requests carry it).
    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        hold: Option<&str>,
        timeout: Duration,
    ) -> Result<Reply, Fault> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(a) = &self.authorization {
            headers.insert(AUTHORIZATION, a.clone());
        }
        if let Some(h) = hold.and_then(|h| HeaderValue::from_str(h).ok()) {
            headers.insert(HeaderName::from_static("x-noevia-maintenance"), h);
        }
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.base))
            .headers(headers)
            .timeout(timeout);
        if let Some(b) = body {
            req = req.body(b.to_string());
        }
        let res = req.send().await.map_err(|_| Fault)?;
        let status = res.status().as_u16();
        let ok = res.status().is_success();
        let bytes = match read_capped(res).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                return Ok(Reply {
                    ok: false,
                    status,
                    body: None,
                })
            }
            Err(()) => return Err(Fault),
        };
        let body = match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => v,
            Err(_) => Value::String(String::from_utf8_lossy(&bytes).into_owned()),
        };
        Ok(Reply {
            ok,
            status,
            body: Some(body),
        })
    }

    pub async fn get(
        &self,
        path: &str,
        hold: Option<&str>,
        timeout: Duration,
    ) -> Result<Reply, Fault> {
        self.request(reqwest::Method::GET, path, None, hold, timeout)
            .await
    }

    /// `rawModels()`: the router's listing.
    pub async fn models(&self, hold: Option<&str>) -> Result<Reply, Fault> {
        self.get("/models", hold, DEFAULT_TIMEOUT).await
    }

    /// `post('/models/unload', { model }, 60000)`.
    pub async fn unload(&self, model: &str, hold: Option<&str>) -> Result<Reply, Fault> {
        self.request(
            reqwest::Method::POST,
            "/models/unload",
            Some(&serde_json::json!({ "model": model })),
            hold,
            Duration::from_secs(60),
        )
        .await
    }
}

/// The body, or `Ok(None)` when it passes [`MAX_RESPONSE_BYTES`].
async fn read_capped(mut res: reqwest::Response) -> Result<Option<Vec<u8>>, ()> {
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|_| ())? {
        if out.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Ok(None);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Some(out))
}

/// The rows of a listing body (`body.data`), when it is an array.
pub fn rows(body: &Option<Value>) -> Option<&Vec<Value>> {
    body.as_ref()?.get("data")?.as_array()
}
