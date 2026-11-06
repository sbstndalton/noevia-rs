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
    pub uncertain: bool,
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
    operation: Option<model_publication::Fence>,
    client: reqwest::Client,
    base: String,
    authorization: Option<HeaderValue>,
    budget_fence: Option<std::path::PathBuf>,
    budget_preset: Option<std::path::PathBuf>,
    budget_node: Option<super::budget_watch::NodeBridge>,
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
            operation: None,
            client,
            base: base.to_owned(),
            authorization,
            budget_fence: None,
            budget_preset: None,
            budget_node: None,
        })
    }

    pub fn with_budget_fence(
        mut self,
        dir: Option<std::path::PathBuf>,
        preset: Option<std::path::PathBuf>,
        node: super::budget_watch::NodeBridge,
    ) -> Self {
        self.budget_fence = dir;
        self.budget_preset = preset;
        self.budget_node = Some(node);
        self
    }

    pub(crate) fn with_operation(&self, fence: &model_publication::Fence) -> Self {
        let mut router = self.clone();
        router.operation = Some(fence.clone());
        router
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// #2286: build the image probe on this exact identity/admission target, with its credentials.
    /// The default chat provider may be a different engine and is never substituted here.
    pub(super) fn image_probe_request(
        &self,
        body: &Value,
        lease_headers: &[(String, String)],
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .post(format!("{}/v1/chat/completions", self.base))
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(authorization) = &self.authorization {
            request = request.header(AUTHORIZATION, authorization.clone());
        }
        for (name, value) in lease_headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request
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
        self.request_with(method, path, body, hold, &[], timeout)
            .await
    }

    /// [`Router::request`] with headers of the caller's own on top (the gate's admin header on the
    /// memory watchdog's forced unload, the model-loader token on its backend read).
    pub async fn request_with(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        hold: Option<&str>,
        extra: &[(HeaderName, HeaderValue)],
        timeout: Duration,
    ) -> Result<Reply, Fault> {
        // All native maintenance loaders (including calibration) use this shared router.
        // A pending or recorded watchdog obligation fences admission even while Core is unavailable.
        if method == reqwest::Method::POST && path.split('?').next() == Some("/models/load") {
            if let (Some(dir), Some(model)) = (
                &self.budget_fence,
                body.and_then(|b| b.get("model")).and_then(Value::as_str),
            ) {
                let fenced = if super::budget_watch::journal::fenced(dir, model) {
                    let revision = match &self.budget_preset {
                        Some(file) => crate::model_sizing::read_presets(file)
                            .ok()
                            .map(|p| Some(p.revision)),
                        None => Some(None),
                    };
                    let budget = match &self.budget_node {
                        Some(node) => node.budget().await,
                        None => None,
                    };
                    super::budget_watch::journal::admission(
                        dir,
                        model,
                        &super::budget_watch::journal::Context::current(revision, budget),
                    )
                } else {
                    false
                };
                if fenced {
                    return Ok(Reply {
                        uncertain: false,
                        ok: false,
                        status: 409,
                        body: Some(
                            serde_json::json!({ "error": "This model stays unloaded until its settings or inference budget change.", "code": "inference_budget_unloaded" }),
                        ),
                    });
                }
            }
        }
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(a) = &self.authorization {
            headers.insert(AUTHORIZATION, a.clone());
        }
        if let Some(h) = hold.and_then(|h| HeaderValue::from_str(h).ok()) {
            headers.insert(HeaderName::from_static("x-noevia-maintenance"), h);
        }
        for (name, value) in extra {
            headers.insert(name.clone(), value.clone());
        }
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.base))
            .headers(headers)
            .timeout(timeout);
        if let Some(b) = body {
            req = req.body(b.to_string());
        }
        if let Some(fence) = &self.operation {
            req = req.header(
                "x-noevia-maintenance-fence",
                serde_json::to_string(fence).map_err(|_| Fault)?,
            );
        }
        let res = req.send().await.map_err(|_| Fault)?;
        let uncertain = res
            .headers()
            .get("x-noevia-maintenance-uncertain")
            .is_some_and(|v| v == "1");
        let status = res.status().as_u16();
        let ok = res.status().is_success();
        let bytes = match read_capped(res).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                return Ok(Reply {
                    uncertain,
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
            uncertain,
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

    /// A streamed request (`fetch` with the body left to the caller): the response once its head
    /// arrived, or `Err` when it did not start. `timeout` bounds the whole exchange.
    pub async fn stream(
        &self,
        path: &str,
        body: &Value,
        hold: Option<&str>,
        timeout: Duration,
    ) -> Result<reqwest::Response, Fault> {
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
            .post(format!("{}{path}", self.base))
            .headers(headers)
            .timeout(timeout)
            .body(body.to_string());
        if let Some(fence) = &self.operation {
            req = req.header(
                "x-noevia-maintenance-fence",
                serde_json::to_string(fence).map_err(|_| Fault)?,
            );
        }
        req.send().await.map_err(|_| Fault)
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
