//! The engine the statistics come from, configured from the same environment keys core
//! `server/index.cjs` reads, and the reads `model-manager.cjs` / `llamacpp-manager.cjs` make for
//! `/api/stats`, with the same paths, headers, timeouts and http.cjs `fetchJson` rules (redirects
//! refused, bodies capped at 8 MiB, `JSON.parse` or the text).
//!
//! The address is configuration only: nothing in a request reaches it. Model names from the
//! engine's own listing go into the query string through `encodeURIComponent`.
//!
//! Stricter than Node, by design of the front: plain `http` only (the front has no TLS client; an
//! `https` engine address is a startup refusal, see [`Engine::from_lookup`]), no proxy from the
//! environment (Node's fetch uses none either) and no `Accept-Encoding` (so a compressed reply is
//! never asked for; Node's fetch would have decompressed one).

use crate::compose::{self, Read, Reads, Reply};
use crate::js::{self, Fault};
use crate::metrics::{parse_metrics, summarize, Row, Summary};
use prompt_framing::json::Value;
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use std::time::Duration;

/// http.cjs DEFAULT_MAX_RESPONSE_BYTES.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// The adapter `MODEL_MANAGER_KIND` selects (model-manager.cjs createModelManager).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// No model management: every read is `{ ok: false }`.
    None,
    Lemonade,
    LlamaCpp,
}

/// Upstream timeouts, as the JS passes them to fetchJson.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// llama.cpp `/models`; Lemonade `/api/v1/health` and `/api/v1/models`.
    pub listing: Duration,
    /// llama.cpp `/metrics?model=`; Lemonade `/v1/stats`, `/v1/system-stats`, `/metrics`.
    pub metrics: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            listing: Duration::from_millis(8000),
            metrics: Duration::from_millis(6000),
        }
    }
}

/// llamacpp-manager.cjs reads the loaded models' metrics four at a time.
const BATCH: usize = 4;

pub struct Engine {
    kind: Kind,
    /// The base the JS concatenates paths to (trailing slashes, and for llama.cpp `/v1`, removed).
    base: String,
    /// `Bearer <key>` when a key other than `local` is set; `Err` when fetch would refuse it.
    authorization: Option<Result<HeaderValue, ()>>,
    client: reqwest::Client,
    timeouts: Timeouts,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key.
        f.debug_struct("Engine")
            .field("kind", &self.kind)
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// `a || b` over environment values (an empty value falls through).
fn first(get: &impl Fn(&str) -> Option<String>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| get(k).filter(|v| !v.is_empty()))
}

/// `value.replace(/\/+$/, '')`.
fn trim_slashes(s: &str) -> &str {
    s.trim_end_matches('/')
}

impl Engine {
    /// index.cjs: `MODEL_MANAGER_KIND || (LEMONADE_BASE_URL ? 'lemonade' : 'none')`, lower-cased;
    /// the base `MODEL_MANAGER_BASE_URL || LEMONADE_BASE_URL || INFERENCE_BASE_URL ||
    /// LEMONADE_BASE_URL || 'http://host.docker.internal:11434'`; the key `MODEL_MANAGER_API_KEY ||
    /// INFERENCE_API_KEY || LEMONADE_API_KEY`. Errors where Node refuses to start (an unknown kind,
    /// an invalid llama.cpp router URL) and where this port does (an `https` engine).
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Engine, String> {
        let kind_raw = first(get, &["MODEL_MANAGER_KIND"]).unwrap_or_else(|| {
            if first(get, &["LEMONADE_BASE_URL"]).is_some() {
                "lemonade".into()
            } else {
                "none".into()
            }
        });
        let kind = match kind_raw.to_lowercase().as_str() {
            "llamacpp" => Kind::LlamaCpp,
            "lemonade" => Kind::Lemonade,
            "none" => Kind::None,
            other => return Err(format!("unsupported MODEL_MANAGER_KIND: {other}")),
        };
        let base = first(
            get,
            &[
                "MODEL_MANAGER_BASE_URL",
                "LEMONADE_BASE_URL",
                "INFERENCE_BASE_URL",
            ],
        )
        .unwrap_or_else(|| "http://host.docker.internal:11434".into());
        let key = first(
            get,
            &[
                "MODEL_MANAGER_API_KEY",
                "INFERENCE_API_KEY",
                "LEMONADE_API_KEY",
            ],
        );
        Engine::new(kind, &base, key.as_deref(), Timeouts::default())
    }

    /// An engine of `kind` at `base` (as configured, before the adapter trims it).
    pub fn new(
        kind: Kind,
        base: &str,
        key: Option<&str>,
        timeouts: Timeouts,
    ) -> Result<Engine, String> {
        let base = match kind {
            Kind::LlamaCpp => {
                let trimmed = trim_slashes(base);
                let base = trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string();
                // llamacpp-manager.cjs: `new URL(base)`, http(s), no credentials, query or fragment.
                let url = reqwest::Url::parse(&base)
                    .map_err(|_| "Invalid llama.cpp router URL".to_string())?;
                if !["http", "https"].contains(&url.scheme())
                    || !url.username().is_empty()
                    || url.password().is_some_and(|p| !p.is_empty())
                    || url.query().is_some_and(|q| !q.is_empty())
                    || url.fragment().is_some_and(|f| !f.is_empty())
                {
                    return Err("Invalid llama.cpp router URL".into());
                }
                if url.scheme() != "http" {
                    return Err(
                        "the Rust front reads engine statistics over plain http only".into(),
                    );
                }
                base
            }
            Kind::Lemonade => {
                let base = trim_slashes(base).to_string();
                if !base.starts_with("http://") {
                    return Err(
                        "the Rust front reads engine statistics over plain http only".into(),
                    );
                }
                base
            }
            Kind::None => String::new(),
        };
        let authorization = key
            .filter(|k| !k.is_empty() && *k != "local")
            .map(|k| bearer(k).ok_or(()));
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| format!("engine statistics client: {e}"))?;
        Ok(Engine {
            kind,
            base,
            authorization,
            client,
            timeouts,
        })
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// One shared answer's worth of reads, and the reply they give (routes/models.cjs `statsRead`
    /// and the handler). For llama.cpp only the reads whose results reach the reply are made: Node
    /// also calls `health()` and `listModels()` (more `/models` and a `/props` per loaded model),
    /// whose results the handler never uses for that kind.
    pub async fn read(&self) -> Reply {
        match self.kind {
            Kind::None => compose::general(&Reads::disabled()),
            Kind::LlamaCpp => compose::llamacpp(self.llamacpp_stats().await),
            Kind::Lemonade => {
                let t = self.timeouts;
                let (gen, sys, health, metrics, models) = tokio::join!(
                    self.fetch("/v1/stats".into(), t.metrics),
                    self.fetch("/v1/system-stats".into(), t.metrics),
                    self.fetch("/api/v1/health".into(), t.listing),
                    self.fetch("/metrics".into(), t.metrics),
                    self.fetch("/api/v1/models".into(), t.listing),
                );
                compose::general(&Reads {
                    gen,
                    sys,
                    health,
                    metrics,
                    models,
                })
            }
        }
    }

    /// llamacpp-manager.cjs `stats()`: `Ok(None)` where the JS returns a not-ok listing or
    /// rejects (both make `up` false), `Err` only where the port cannot follow the JS.
    async fn llamacpp_stats(&self) -> Result<Option<Summary>, Fault> {
        let listing = match self.fetch("/models".into(), self.timeouts.listing).await {
            Read::Done { ok: true, body } => body,
            _ => return Ok(None),
        };
        let loaded = match loaded_models(&listing) {
            Ok(l) => l,
            Err(Fault::Throws) => return Ok(None),
            Err(f) => return Err(f),
        };
        let mut rows = Vec::with_capacity(loaded.len());
        for batch in loaded.chunks(BATCH) {
            let results = futures_join(batch.iter().map(|m| self.model_metrics(m))).await;
            // parseMetrics threw for one of them: Promise.all, and with it stats(), rejects.
            if results.contains(&Err(Fault::Throws)) {
                return Ok(None);
            }
            for r in results {
                rows.push(r?);
            }
        }
        Ok(Some(summarize(&rows)))
    }

    /// One loaded model's `{ model: m.id, values }`; a failed query or fetch is `values: {}`.
    async fn model_metrics(&self, m: &Value) -> Result<Row, Fault> {
        let id = js::get(Some(m), "id")?;
        let values = match js::encode_uri_component(id) {
            Ok(q) => {
                let path = format!("/metrics?model={q}&autoload=false");
                match self.fetch(path, self.timeouts.metrics).await {
                    Read::Done { ok: true, body } => {
                        let text = if js::truthy(Some(&body)) {
                            js::to_string(Some(&body))?
                        } else {
                            Vec::new()
                        };
                        parse_metrics(&text)
                    }
                    _ => Default::default(),
                }
            }
            // Thrown inside the JS's try: the model's values are {}.
            Err(Fault::Throws) => Default::default(),
            Err(f) => return Err(f),
        };
        Ok(Row {
            model: id.cloned(),
            values,
        })
    }

    /// http.cjs `fetchJson(base + path, { headers }, timeout)`, GET.
    async fn fetch(&self, path: String, timeout: Duration) -> Read {
        match tokio::time::timeout(timeout, self.fetch_inner(&path)).await {
            Ok(Some(read)) => read,
            _ => Read::Rejected,
        }
    }

    async fn fetch_inner(&self, path: &str) -> Option<Read> {
        let url = reqwest::Url::parse(&format!("{}{path}", self.base)).ok()?;
        if url.scheme() != "http" {
            return None;
        }
        let mut req = self
            .client
            .get(url)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(auth) = &self.authorization {
            req = req.header(AUTHORIZATION, auth.clone().ok()?);
        }
        let mut res = req.send().await.ok()?;
        // fetch's redirect: 'error' fails on every redirect status, with or without a Location.
        if [301, 302, 303, 307, 308].contains(&res.status().as_u16()) {
            return None;
        }
        let ok = res.status().is_success();
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = res.chunk().await.ok()? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Some(Read::Done {
                    ok: false,
                    body: Value::Null,
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        Some(Read::Done {
            ok,
            body: js::body_of(&bytes),
        })
    }
}

/// `(listing.body?.data || []).filter(m => m.status?.value === 'loaded')`.
fn loaded_models(listing: &Value) -> Result<Vec<Value>, Fault> {
    let data = js::opt(Some(listing), "data")?;
    if !js::truthy(data) {
        return Ok(Vec::new());
    }
    let Some(Value::Arr(items)) = data else {
        return Err(match data {
            Some(Value::Deep) => Fault::Unsupported,
            _ => Fault::Throws,
        });
    };
    let mut loaded = Vec::new();
    for m in items {
        let status = js::get(Some(m), "status")?;
        let value = js::opt(status, "value")?;
        if matches!(value, Some(Value::Str(s)) if s.iter().copied().eq("loaded".encode_utf16())) {
            loaded.push(m.clone());
        }
    }
    Ok(loaded)
}

/// `Promise.all` over one batch, in order.
async fn futures_join<F: std::future::Future>(futures: impl Iterator<Item = F>) -> Vec<F::Output> {
    let mut pinned: Vec<std::pin::Pin<Box<F>>> = futures.map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = pinned.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (f, slot) in pinned.iter_mut().zip(out.iter_mut()) {
            if slot.is_none() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => *slot = Some(v),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    out.into_iter().flatten().collect()
}

/// The Authorization value fetch sends for `'Bearer ' + key`: the header normalised (leading and
/// trailing tab, CR, LF and space removed) and refused where undici refuses it (a character past
/// U+00FF, or a control character other than tab).
fn bearer(key: &str) -> Option<HeaderValue> {
    let raw = format!("Bearer {key}");
    let mut bytes = Vec::with_capacity(raw.len());
    for c in raw.chars() {
        bytes.push(u8::try_from(u32::from(c)).ok()?);
    }
    let ws = |b: &u8| matches!(b, b'\t' | b'\r' | b'\n' | b' ');
    let start = bytes.iter().position(|b| !ws(b)).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|b| !ws(b)).map_or(start, |i| i + 1);
    let value = bytes.get(start..end)?;
    if value.iter().any(|&b| b != b'\t' && (b < 0x20 || b == 0x7f)) {
        return None;
    }
    HeaderValue::from_bytes(value).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn kind_and_base_follow_index_cjs() {
        let e = Engine::from_lookup(&env(&[])).unwrap();
        assert_eq!(e.kind(), Kind::None);
        let e = Engine::from_lookup(&env(&[("LEMONADE_BASE_URL", "http://l:1/")])).unwrap();
        assert_eq!((e.kind(), e.base.as_str()), (Kind::Lemonade, "http://l:1"));
        let e = Engine::from_lookup(&env(&[
            ("MODEL_MANAGER_KIND", "LlamaCpp"),
            ("INFERENCE_BASE_URL", "http://r:8080/v1//"),
        ]))
        .unwrap();
        assert_eq!(
            (e.kind(), e.base.as_str()),
            (Kind::LlamaCpp, "http://r:8080")
        );
        let e = Engine::from_lookup(&env(&[
            ("MODEL_MANAGER_KIND", "llamacpp"),
            ("MODEL_MANAGER_BASE_URL", ""),
            ("LEMONADE_BASE_URL", "http://lem:1/v1/v1"),
            ("INFERENCE_BASE_URL", "http://ignored:2"),
        ]))
        .unwrap();
        assert_eq!(e.base, "http://lem:1/v1");
        assert!(
            Engine::from_lookup(&env(&[("MODEL_MANAGER_KIND", "ollama")]))
                .unwrap_err()
                .contains("unsupported MODEL_MANAGER_KIND: ollama")
        );
        for bad in [
            "http://u:p@r:1",
            "http://r:1/?x=1",
            "http://r:1/#f",
            "ftp://r:1",
            "not a url",
        ] {
            assert!(
                Engine::from_lookup(&env(&[
                    ("MODEL_MANAGER_KIND", "llamacpp"),
                    ("MODEL_MANAGER_BASE_URL", bad)
                ]))
                .is_err(),
                "{bad}"
            );
        }
        assert!(Engine::from_lookup(&env(&[
            ("MODEL_MANAGER_KIND", "llamacpp"),
            ("MODEL_MANAGER_BASE_URL", "https://r:1")
        ]))
        .unwrap_err()
        .contains("plain http"));
        // An empty query or fragment is no query or fragment for WHATWG `search` / `hash`.
        assert!(Engine::from_lookup(&env(&[
            ("MODEL_MANAGER_KIND", "llamacpp"),
            ("MODEL_MANAGER_BASE_URL", "http://r:1/?#")
        ]))
        .is_ok());
    }

    #[test]
    fn bearer_is_normalised_like_fetch() {
        assert_eq!(bearer("k").unwrap(), "Bearer k");
        assert_eq!(bearer("k \t").unwrap(), "Bearer k");
        assert_eq!(bearer(" ").unwrap(), "Bearer");
        assert!(bearer("a\u{1}b").is_none());
        assert!(bearer("a\u{100}").is_none());
        assert_eq!(bearer("é").unwrap().as_bytes(), b"Bearer \xe9");
        let e = Engine::from_lookup(&env(&[("INFERENCE_API_KEY", "local")])).unwrap();
        assert!(e.authorization.is_none());
        let e = Engine::from_lookup(&env(&[
            ("MODEL_MANAGER_API_KEY", "m"),
            ("INFERENCE_API_KEY", "i"),
        ]))
        .unwrap();
        assert_eq!(e.authorization.unwrap().unwrap(), "Bearer m");
    }
}
