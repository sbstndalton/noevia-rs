//! Bounded no-proxy, no-redirect HTTP boundary. All errors are fixed labels;
//! endpoint URLs, response text and credentials never enter diagnostics.
use crate::{
    fill::Embed,
    service::{served_ids, Loader},
    Control, Target, Task,
};
use engine_admission::{Admission, Ask};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, Method, Url,
};
use serde_json::Value;
use std::sync::Arc;
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use zeroize::Zeroizing;
#[derive(Clone)]
pub struct Http {
    admission: Option<Arc<Admission>>,
    client: Client,
    loader: Url,
    loader_token: Option<Zeroizing<String>>,
    inference: Url,
    inference_token: Option<Zeroizing<String>>,
}
fn err() -> String {
    "embedding HTTP unavailable".into()
}
fn url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| err())?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(err());
    }
    Ok(url)
}
fn api(base: &Url, path: &str) -> Result<Url, String> {
    url(&format!(
        "{}{path}",
        base.as_str().trim_end_matches('/').trim_end_matches("/v1")
    ))
}
fn headers(token: Option<&str>, name: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        let token = if name == "authorization" {
            format!("Bearer {token}")
        } else {
            token.to_owned()
        };
        let mut value = HeaderValue::from_str(&token).map_err(|_| err())?;
        value.set_sensitive(true);
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| err())?;
        headers.insert(name, value);
    }
    Ok(headers)
}
fn allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || (ip.octets().first() == Some(&100)
                    && ip.octets().get(1).is_some_and(|n| (64..=127).contains(n)))
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.to_ipv4_mapped().is_some_and(|v| allowed(IpAddr::V4(v)))
                || ip.is_unique_local()
        }
    }
}
impl Http {
    pub fn new(
        loader: &str,
        loader_token: Option<String>,
        inference: &str,
        inference_token: Option<String>,
    ) -> Result<Self, String> {
        Ok(Self {
            admission: None,
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| err())?,
            loader: url(loader)?,
            loader_token: loader_token.map(Zeroizing::new),
            inference: url(inference)?,
            inference_token: inference_token.map(Zeroizing::new),
        })
    }
    pub fn with_admission(mut self, core: &str, token: &str) -> Result<Self, String> {
        self.admission = Some(Arc::new(
            Admission::new(core, Some(token)).map_err(|_| err())?,
        ));
        Ok(self)
    }
    async fn request(
        &self,
        url: Url,
        method: Method,
        headers: HeaderMap,
        body: Option<String>,
        timeout: Duration,
        cap: usize,
    ) -> Result<(u16, Value), String> {
        let operation = async {
            let host = url.host_str().ok_or_else(err)?;
            let host = host.trim_matches(['[', ']']);
            let port = url.port_or_known_default().ok_or_else(err)?;
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
                .await
                .map_err(|_| err())?
                .take(513)
                .collect();
            if addresses.is_empty()
                || addresses.len() > 512
                || addresses.iter().any(|a| !allowed(a.ip()))
            {
                return Err(err());
            }
            // Pin the checked DNS answers at the socket, preserving Host/SNI.
            let client = if host.parse::<IpAddr>().is_ok() {
                self.client.clone()
            } else {
                Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .resolve_to_addrs(host, &addresses)
                    .build()
                    .map_err(|_| err())?
            };
            let mut request = client
                .request(method, url)
                .headers(headers)
                .timeout(timeout);
            if let Some(body) = body {
                request = request.body(body);
            }
            let mut response = request.send().await.map_err(|_| err())?;
            let status = response.status().as_u16();
            if (300..400).contains(&status) {
                return Err(err());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| err())? {
                if chunk.len() > cap.saturating_sub(bytes.len()) {
                    return Err(err());
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok((status, serde_json::from_slice(&bytes).map_err(|_| err())?))
        };
        tokio::time::timeout(timeout, operation)
            .await
            .map_err(|_| err())?
    }
    fn target(&self, target: &Target) -> Result<(Url, HeaderMap), String> {
        match &target.base_url {
            Some(base) => Ok((url(base)?, headers(None, "authorization")?)),
            None => Ok((
                self.inference.clone(),
                headers(
                    self.inference_token.as_deref().map(|s| s.as_str()),
                    "authorization",
                )?,
            )),
        }
    }
    pub async fn served(&self, target: &Target) -> Result<Vec<String>, String> {
        let (base, headers) = self.target(target)?;
        let (status, body) = self
            .request(
                api(&base, "/v1/models")?,
                Method::GET,
                headers,
                None,
                Duration::from_secs(10),
                1024 * 1024,
            )
            .await?;
        if !(200..300).contains(&status) {
            return Err(err());
        }
        Ok(served_ids(&body))
    }
    /// Internal pairing proof. Core must authenticate this endpoint and return a
    /// proof bound to this front's start, after refusing its own worker startup.
    pub async fn ownership(&self, base: &str, token: &str, start: &str) -> Result<(), String> {
        let (status, body) = self
            .request(
                api(&url(base)?, "/internal/embedding-worker-owner")?,
                Method::GET,
                headers(Some(token), "x-noevia-store-token")?,
                None,
                Duration::from_secs(3),
                8192,
            )
            .await?;
        if status != 200
            || body.get("protocol").and_then(Value::as_str) != Some("rust-embedding-worker-v1")
            || body.get("start").and_then(Value::as_str) != Some(start)
            || body.get("nodeWorkersStopped") != Some(&Value::Bool(true))
        {
            return Err("embedding worker ownership unconfirmed".into());
        }
        Ok(())
    }
    async fn once(&self, target: &Target, input: &[Vec<u16>]) -> Result<Vec<Vec<f32>>, String> {
        let (base, headers) = self.target(target)?;
        let mut body = format!(
            "{{\"model\":{},\"input\":[",
            serde_json::to_string(&target.model).map_err(|_| err())?
        );
        for (i, text) in input.iter().enumerate() {
            if i > 0 {
                body.push(',');
            }
            quote_units(text, &mut body);
        }
        body.push_str("]}");
        let (status, value) = self
            .request(
                api(&base, "/v1/embeddings")?,
                Method::POST,
                headers,
                Some(body),
                Duration::from_secs(60),
                16 * 1024 * 1024,
            )
            .await?;
        if !(200..300).contains(&status) {
            return Err(err());
        }
        let rows = value
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(err)?;
        if rows.len() != input.len() {
            return Ok(vec![]);
        }
        let mut rows: Vec<_> = rows.iter().collect();
        if rows
            .iter()
            .all(|r| r.get("index").and_then(Value::as_i64).is_some())
        {
            rows.sort_by_key(|r| r.get("index").and_then(Value::as_i64));
        }
        rows.into_iter()
            .map(|row| {
                row.get("embedding")
                    .and_then(Value::as_array)
                    .ok_or_else(err)?
                    .iter()
                    .map(|n| {
                        n.as_f64()
                            .map(|n| n as f32)
                            .filter(|n| n.is_finite())
                            .ok_or_else(err)
                    })
                    .collect()
            })
            .collect()
    }
}
/// Quote code units, including lone surrogates, without lossy UTF-8 conversion.
fn quote_units(text: &[u16], out: &mut String) {
    out.push('"');
    for unit in text {
        out.push_str(&format!("\\u{unit:04x}"));
    }
    out.push('"');
}
impl Loader for Http {
    fn get(&self) -> Task<'_, Result<(u16, Value), String>> {
        Box::pin(async move {
            // Keep the configured loader path; unlike embedding endpoints, do not strip /v1.
            let endpoint = url(&format!(
                "{}/api/v1/embedding-service",
                self.loader.as_str().trim_end_matches('/')
            ))?;
            self.request(
                endpoint,
                Method::GET,
                headers(
                    self.loader_token.as_deref().map(|s| s.as_str()),
                    "x-model-loader-token",
                )?,
                None,
                Duration::from_secs(6),
                1024 * 1024,
            )
            .await
        })
    }
}
impl Embed for Http {
    fn embed<'a>(
        &'a self,
        target: &'a Target,
        input: &'a [Vec<u16>],
        control: &'a Control,
    ) -> Task<'a, Result<Vec<Vec<f32>>, String>> {
        Box::pin(async move {
            if control.stopped() {
                return Err("stopped".into());
            }
            if input.is_empty() {
                return Ok(vec![]);
            }
            let cancel = model_stream::Cancel::new();
            let lease = if let Some(admission) = &self.admission {
                Some(
                    admission
                        .enter(
                            Ask::Embedding {
                                model: &target.model,
                            },
                            &cancel,
                        )
                        .await
                        .map_err(|_| "embedding admission refused".to_owned())?,
                )
            } else {
                None
            };
            let operation = async {
                let vectors = self.once(target, input).await?;
                if vectors.len() == input.len() && vectors.iter().all(|v| !v.is_empty()) {
                    return Ok(vectors);
                }
                if input.len() == 1 {
                    return Err("embeddings returned no vector".into());
                }
                let mut out = vec![];
                for text in input {
                    if control.stopped() {
                        return Err("stopped".into());
                    }
                    let mut v = self.once(target, std::slice::from_ref(text)).await?;
                    if v.len() != 1 || v.iter().any(Vec::is_empty) {
                        return Err("embeddings returned no vector".into());
                    }
                    out.append(&mut v);
                }
                Ok(out)
            };
            if let Some(lease) = lease {
                let result = tokio::select! {r=operation=>r,_=lease.lost().cancelled()=>Err("embedding admission lost".into())};
                lease.release().await;
                result
            } else {
                operation.await
            }
        })
    }
}
