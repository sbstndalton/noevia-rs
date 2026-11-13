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
struct Proof {
    core: String,
    token: Zeroizing<String>,
    start: String,
}
#[derive(Clone)]
pub struct Http {
    admission: Option<Arc<Admission>>,
    proof: Option<Arc<Proof>>,
    client: Client,
    loader: Option<Url>,
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
            proof: None,
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| err())?,
            loader: if loader.is_empty() {
                None
            } else {
                Some(url(loader)?)
            },
            loader_token: loader_token.map(Zeroizing::new),
            inference: url(inference)?,
            inference_token: inference_token.map(Zeroizing::new),
        })
    }
    pub fn loader_configured(&self) -> bool {
        self.loader.is_some()
    }
    pub fn with_ownership(mut self, core: &str, token: &str, start: &str) -> Self {
        self.proof = Some(Arc::new(Proof {
            core: core.into(),
            token: Zeroizing::new(token.into()),
            start: start.into(),
        }));
        self
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
    pub async fn outbox_ready(&self, base: &str, token: &str, start: &str) -> Result<bool, String> {
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
        Ok(status == 200
            && body.get("protocol").and_then(Value::as_str) == Some("rust-embedding-worker-v1")
            && body.get("nodeWorkersStopped") == Some(&Value::Bool(true))
            && body.get("start").and_then(Value::as_str) == Some(start)
            && body.get("indexOutboxStopped") == Some(&Value::Bool(true))
            && body.get("foregroundBusy") == Some(&Value::Bool(false))
            && !crate::foreground::busy())
    }
    pub async fn media_ready(
        &self,
        base: &str,
        token: &str,
        start: &str,
        foreground: bool,
    ) -> Result<bool, String> {
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
        Ok(status == 200
            && body.get("protocol").and_then(Value::as_str) == Some("rust-embedding-worker-v1")
            && body.get("start").and_then(Value::as_str) == Some(start)
            && body.get("mediaWorkersStopped") == Some(&Value::Bool(true))
            && (!foreground
                || (body.get("foregroundBusy") == Some(&Value::Bool(false))
                    && !crate::foreground::busy())))
    }
    /// One query vector, as core `rag.cjs` `embedFramed` asks for a single framed text: exactly one
    /// item with a vector, else an error. A foreground read: no admission lease and no ownership
    /// proof (Node's query embedding takes neither); same address rules, headers and caps as the
    /// passes' requests.
    pub async fn embed_query(&self, target: &Target, framed: &[u16]) -> Result<Vec<f32>, String> {
        let mut vectors = self.once(target, &[framed.to_vec()]).await?;
        match (vectors.pop(), vectors.is_empty()) {
            (Some(v), true) if !v.is_empty() => Ok(v),
            _ => Err("embeddings returned no vector".into()),
        }
    }
    /// Notify the still-owned media scheduler without delegating any outbox work.
    pub async fn source_changed(
        &self,
        base: &str,
        token: &str,
        start: &str,
        user: &str,
        project: &str,
        file: &str,
    ) -> Result<(), String> {
        let (status, body) = self
            .request(
                api(
                    &url(base)?,
                    "/internal/embedding-worker-owner/source-changed",
                )?,
                Method::POST,
                headers(Some(token), "x-noevia-store-token")?,
                Some(
                    js_json::wtf16::stringify(&js_json::JValue::obj([
                        ("start", js_json::JValue::from(start)),
                        ("user", js_json::JValue::from(user)),
                        ("project", js_json::JValue::from(project)),
                        ("file", js_json::JValue::from(file)),
                    ]))
                    .ok_or_else(err)?,
                ),
                Duration::from_secs(3),
                8192,
            )
            .await?;
        if status == 200
            && body.get("start").and_then(Value::as_str) == Some(start)
            && body.get("protocol").and_then(Value::as_str) == Some("rust-embedding-worker-v1")
            && body.get("notified") == Some(&Value::Bool(true))
        {
            Ok(())
        } else {
            Err("media notification unavailable".into())
        }
    }
    /// One image request, using the same admission and ownership authority as text.
    pub async fn media(
        &self,
        target: &Target,
        model: &str,
        part: Value,
        control: &Control,
    ) -> Result<Vec<f32>, String> {
        self.authorize(control).await?;
        let cancel = model_stream::Cancel::new();
        let lease = if let Some(admission) = &self.admission {
            Some(tokio::select! {
                grant=admission.enter(Ask::Embedding {model}, &cancel)=>grant.map_err(|_| "embedding admission refused".to_owned())?,
                _=control.cancelled()=>{cancel.cancel();return Err(err());}
            })
        } else {
            None
        };
        let operation = async {
            let (base, headers) = self.target(target)?;
            let (status, body) = self.request(api(&base, "/v1/embeddings")?, Method::POST, headers, Some(serde_json::json!({"model":model,"input":[{"content":[part]}],"encoding_format":"float"}).to_string()), Duration::from_millis(crate::media_policy::media_timeout_ms(std::env::var("RAG_MEDIA_EMBED_TIMEOUT_MS").ok().as_deref())), 4*1024*1024).await?;
            if !(200..300).contains(&status) {
                return Err(err());
            }
            let data = body
                .get("data")
                .and_then(Value::as_array)
                .filter(|v| v.len() == 1)
                .ok_or_else(err)?;
            let vector = data
                .first()
                .and_then(|v| v.get("embedding"))
                .and_then(Value::as_array)
                .ok_or_else(err)?;
            vector
                .iter()
                .map(|v| {
                    v.as_f64()
                        .map(|v| v as f32)
                        .filter(|v| v.is_finite())
                        .ok_or_else(err)
                })
                .collect()
        };
        let result = if let Some(lease) = lease {
            let result = tokio::select! {r=operation=>r,_=lease.lost().cancelled()=>Err(err()),_=control.cancelled()=>{cancel.cancel();Err(err())}};
            lease.release().await;
            result
        } else {
            tokio::select! {r=operation=>r,_=control.cancelled()=>Err(err())}
        };
        result
    }
    pub async fn release_media(&self, target: &Target, alias: &str) {
        if alias == target.model {
            return;
        }
        if let Ok((base, headers)) = self.target(target) {
            if let Ok(endpoint) = api(&base, "/models/unload") {
                let _ = self
                    .request(
                        endpoint,
                        Method::POST,
                        headers,
                        Some(serde_json::json!({"model":alias}).to_string()),
                        Duration::from_secs(10),
                        4096,
                    )
                    .await;
            }
        }
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
        if rows.iter().all(|r| {
            r.get("index")
                .and_then(Value::as_f64)
                .is_some_and(|n| n.is_finite() && n.fract() == 0.0)
        }) {
            rows.sort_by(|a, b| {
                a.get("index")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .total_cmp(&b.get("index").and_then(Value::as_f64).unwrap_or(0.0))
            });
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
                self.loader
                    .as_ref()
                    .ok_or_else(err)?
                    .as_str()
                    .trim_end_matches('/')
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
    fn authorize<'a>(&'a self, control: &'a Control) -> Task<'a, Result<(), String>> {
        Box::pin(async move {
            if control.stopped() || control.suspended() {
                return Err("embedding work suspended".into());
            }
            if let Some(proof) = &self.proof {
                let http = self.clone();
                let proof_copy = proof.clone();
                let runtime = tokio::runtime::Handle::current();
                control.set_publication_probe(move || {
                    // SQLite publication is synchronous on the blocking host.
                    // Drive its bounded proof on a separate joined thread while
                    // the existing async runtime continues servicing sockets.
                    std::thread::scope(|scope| {
                        scope
                            .spawn(|| {
                                runtime.block_on(async {
                                    tokio::time::timeout(
                                        Duration::from_millis(250),
                                        http.ownership(
                                            &proof_copy.core,
                                            &proof_copy.token,
                                            &proof_copy.start,
                                        ),
                                    )
                                    .await
                                    .map_err(|_| {
                                        "publication ownership proof timed out".to_owned()
                                    })?
                                })
                            })
                            .join()
                            .map_err(|_| "publication ownership proof unavailable".to_owned())?
                    })
                });
                let result = tokio::select! {r=self.ownership(&proof.core,&proof.token,&proof.start)=>r,_=control.cancelled()=>Err("embedding work suspended".into())};
                if result.is_err() && !control.stopped() {
                    control.suspend();
                } else if result.is_ok() {
                    control.confirm_ownership();
                }
                result
            } else {
                Ok(())
            }
        })
    }

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
                let grant = tokio::select! {
                    grant=admission.enter(Ask::Embedding {model:&target.model},&cancel)=>grant.map_err(|_|"embedding admission refused".to_owned())?,
                    _=control.cancelled()=>{cancel.cancel();return Err("embedding work cancelled".into());}
                };
                Some(grant)
            } else {
                None
            };
            // The duty-cycle clock starts at the grant, not at queue entry.
            let busy_started = std::time::Instant::now();
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
                    self.authorize(control).await?;
                    let mut v = self.once(target, std::slice::from_ref(text)).await?;
                    if v.len() != 1 || v.iter().any(Vec::is_empty) {
                        return Err("embeddings returned no vector".into());
                    }
                    out.append(&mut v);
                }
                Ok(out)
            };
            if let Some(lease) = lease {
                let result = tokio::select! {r=operation=>r,_=lease.lost().cancelled()=>Err("embedding admission lost".into()),_=control.cancelled()=>{cancel.cancel();Err("embedding work cancelled".into())}};
                control.record_embedding_busy(busy_started.elapsed().as_millis() as u64);
                lease.release().await;
                result
            } else {
                let result = tokio::select! {r=operation=>r,_=control.cancelled()=>Err("embedding work cancelled".into())};
                control.record_embedding_busy(busy_started.elapsed().as_millis() as u64);
                result
            }
        })
    }
}
