//! Bounded native HTTP exchanges. Destinations come from operator-owned configurations.
use crate::{Control, Fault, Result};
use reqwest::{
    header::{HeaderMap, HeaderValue, CONTENT_TYPE},
    Client, Method,
};
use std::time::Duration;
use url::Url;
#[derive(Clone)]
pub struct Endpoint {
    url: Url,
    headers: HeaderMap,
}
impl Endpoint {
    /// An explicit origin allow-list is required even for model engines and MCP servers.
    /// Callers must apply the account's egress policy before constructing this capability.
    pub fn new(raw: &str, allowed_origins: &[String], bearer: Option<&str>) -> Result<Self> {
        let url = Url::parse(raw).map_err(|_| Fault::Invalid)?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !allowed_origins.contains(&url.origin().ascii_serialization())
        {
            return Err(Fault::Forbidden);
        }
        let mut headers = HeaderMap::new();
        if let Some(token) = bearer {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| Fault::Invalid)?,
            );
        }
        Ok(Self { url, headers })
    }
}
#[derive(Clone)]
pub struct Transport {
    client: Client,
}
pub struct Response {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}
impl Transport {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(|_| Fault::Transport)?,
        })
    }
    /// Incremental bounded provider SSE. The caller's callback can stop disclosure and
    /// cancel the underlying response immediately; no body task outlives this future.
    pub async fn post_stream(
        &self,
        endpoint: &Endpoint,
        body: Vec<u8>,
        control: &Control,
        mut consume: impl FnMut(&[u8]) -> Result<bool> + Send,
    ) -> Result<()> {
        if body.len() > 8 * 1024 * 1024 {
            return Err(Fault::Capacity);
        }
        let exchange = async {
            let mut response = self
                .client
                .post(endpoint.url.clone())
                .headers(endpoint.headers.clone())
                .header(CONTENT_TYPE, "application/json")
                .body(body)
                .timeout(control.remaining()?)
                .send()
                .await
                .map_err(|_| Fault::Transport)?;
            if !response.status().is_success() {
                return Err(Fault::Transport);
            }
            if !response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.split(';')
                        .next()
                        .is_some_and(|s| s.trim().eq_ignore_ascii_case("text/event-stream"))
                })
            {
                return Err(Fault::Protocol);
            }
            let mut total = 0usize;
            while let Some(chunk) = response.chunk().await.map_err(|_| Fault::Transport)? {
                control.check()?;
                total = total.checked_add(chunk.len()).ok_or(Fault::Capacity)?;
                if total > 8 * 1024 * 1024 {
                    return Err(Fault::Capacity);
                }
                if consume(&chunk)? {
                    break;
                }
            }
            control.check()
        };
        tokio::pin!(exchange);
        loop {
            control.check()?;
            tokio::select! {result=&mut exchange=>return result,_=tokio::time::sleep(control.remaining()?.min(Duration::from_millis(50)))=>{}}
        }
    }
    pub async fn request(
        &self,
        endpoint: &Endpoint,
        method: Method,
        extra: HeaderMap,
        body: Option<Vec<u8>>,
        max: usize,
        control: &Control,
    ) -> Result<Response> {
        control.check()?;
        if max == 0
            || max > 32 * 1024 * 1024
            || body.as_ref().is_some_and(|b| b.len() > 8 * 1024 * 1024)
        {
            return Err(Fault::Capacity);
        }
        // A cancelled request future is dropped; no detached body task or retry survives it.
        let exchange = async {
            let mut request = self
                .client
                .request(method, endpoint.url.clone())
                .headers(endpoint.headers.clone())
                .headers(extra)
                .timeout(control.remaining()?);
            if let Some(body) = body {
                request = request.header(CONTENT_TYPE, "application/json").body(body);
            }
            let mut response = request.send().await.map_err(|_| Fault::Transport)?;
            if response.content_length().is_some_and(|n| n > max as u64) {
                return Err(Fault::Capacity);
            }
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| Fault::Transport)? {
                control.check()?;
                if body.len().saturating_add(chunk.len()) > max {
                    return Err(Fault::Capacity);
                }
                body.extend_from_slice(&chunk);
            }
            control.check()?;
            Ok(Response {
                status,
                headers,
                body,
            })
        };
        tokio::pin!(exchange);
        loop {
            control.check()?;
            tokio::select! { result=&mut exchange=>return result, _=tokio::time::sleep(control.remaining()?.min(Duration::from_millis(50)))=>{} }
        }
    }
}
