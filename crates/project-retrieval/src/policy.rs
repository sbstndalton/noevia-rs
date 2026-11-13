//! The two decisions a search makes before it embeds a query, ported from noevia-core and checked
//! against outputs recorded from it (`tests/generate_policy_fixtures.cjs`):
//! - [`rerank_target`]: `rerank-target.cjs` `rerankTarget(env)`, whether the reranker is on (#697,
//!   #1665): a native search answers only while it is off, until the rerank client is ported.
//! - [`embedding_off`]: `rag.cjs` `embeddingOff(target)` over `embedding-service.cjs` in
//!   native-owner mode, where the service state is the native embedding worker's status snapshot
//!   (`embedding-worker-status.json`, as `rust-embedding-background.cjs` `snapshot()` reads it).

use std::collections::HashMap;

use prompt_framing::js::{lossy, to_lower, trim, units};
use serde_json::Value;

fn js_trim(s: &str) -> String {
    lossy(trim(&units(s)))
}

/// `/^(1|true|on)$/i.test(String(v || '').trim())`.
fn truthy(v: Option<&str>) -> bool {
    let t = lossy(&to_lower(&units(&js_trim(v.unwrap_or_default()))));
    matches!(t.as_str(), "1" | "true" | "on")
}

/// `origin(value)`: `${protocol}//${host}` of `new URL(String(value || '').trim())`, lower-cased,
/// or `None` when it does not parse. `host` carries the port only when it is not the default.
fn origin(value: Option<&str>) -> Option<String> {
    let url = url::Url::parse(&js_trim(value.unwrap_or_default())).ok()?;
    let host = url.host_str().unwrap_or_default();
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    Some(format!("{}://{host}", url.scheme()).to_lowercase())
}

/// The rerank wire protocol (`rerankBackendName`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// llama.cpp `--reranking`, `POST /v1/rerank`.
    Llamacpp,
    /// text-embeddings-inference, `POST /rerank`.
    Tei,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Llamacpp => "llamacpp",
            Backend::Tei => "tei",
        }
    }
}

/// `DEFAULT_TEI_URL`.
pub const DEFAULT_TEI_URL: &str = "http://rerank:8080";

/// `rerankTarget(env)`'s answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RerankTarget {
    pub enabled: bool,
    pub backend: Option<Backend>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// The reranker runs on the shared inference engine.
    pub shared: bool,
    /// Why it is off, when it was asked for but cannot run.
    pub reason: Option<&'static str>,
}

const REASON_BACKEND: &str =
    "RERANK_BACKEND must be tei or llamacpp. Retrieval uses cosine ranking.";
const REASON_SHARED: &str = "RERANK_BASE_URL points at the shared inference engine, which holds one model within the inference memory budget; run the reranker on its own endpoint (compose.rerank.yaml) or set RERANK_SHARED_ENGINE=allow. Retrieval uses cosine ranking.";

/// `rerankBackendName(env)`: an own key of `BACKENDS` only (`toString` is not one).
fn backend_name(env: &HashMap<String, String>) -> Option<Backend> {
    let raw = env
        .get("RERANK_BACKEND")
        .map(String::as_str)
        .unwrap_or_default();
    match lossy(&to_lower(&units(&js_trim(raw)))).as_str() {
        "" | "llamacpp" | "llama.cpp" | "llama" => Some(Backend::Llamacpp),
        "tei" => Some(Backend::Tei),
        _ => None,
    }
}

/// `rerankTarget(env)` (`rerank-target.cjs:44-60`). `env` is the reranker's effective environment
/// (the deployment's, with the admin's saved switch over it).
pub fn rerank_target(env: &HashMap<String, String>) -> RerankTarget {
    let get = |k: &str| env.get(k).map(String::as_str);
    let backend = backend_name(env);
    let base_url = Some(js_trim(get("RERANK_BASE_URL").unwrap_or_default()))
        .filter(|s| !s.is_empty())
        .or_else(|| (backend == Some(Backend::Tei)).then(|| DEFAULT_TEI_URL.to_owned()));
    let model = Some(js_trim(get("RERANK_MODEL").unwrap_or_default())).filter(|s| !s.is_empty());
    let off = |shared, reason| RerankTarget {
        enabled: false,
        backend,
        base_url: base_url.clone(),
        model: model.clone(),
        shared,
        reason,
    };
    if !truthy(get("NOEVIA_FEATURE_RAG_RERANK")) || base_url.is_none() {
        return off(false, None);
    }
    if backend.is_none() {
        return off(false, Some(REASON_BACKEND));
    }
    let target = origin(base_url.as_deref());
    let engines: Vec<String> = [
        "INFERENCE_BASE_URL",
        "MODEL_MANAGER_BASE_URL",
        "LEMONADE_BASE_URL",
    ]
    .iter()
    .filter_map(|k| origin(get(k)))
    .collect();
    let shared = target.as_ref().is_some_and(|t| engines.contains(t));
    let allow = lossy(&to_lower(&units(&js_trim(
        get("RERANK_SHARED_ENGINE").unwrap_or_default(),
    )))) == "allow";
    if shared && !allow {
        return off(true, Some(REASON_SHARED));
    }
    RerankTarget {
        enabled: true,
        backend,
        base_url,
        model,
        shared,
        reason: None,
    }
}

/// `STALE_MS` (`embedding-service.cjs`): a service observation older than this is stale.
pub const SERVICE_STALE_MS: f64 = 15_000.0;
/// `rust-embedding-background.cjs` `PROTOCOL`.
pub const SNAPSHOT_PROTOCOL: &str = "rust-embedding-worker-v1";

/// `snapshot()` (`rust-embedding-background.cjs:30-35`): the worker's status file is Node's only
/// while it is this start's, of this protocol, and at most 15 s old.
pub fn current_snapshot<'a>(file: &'a Value, start: &str, now_ms: f64) -> Option<&'a Value> {
    let at = file.get("at").and_then(Value::as_f64)?;
    (file.get("protocol").and_then(Value::as_str) == Some(SNAPSHOT_PROTOCOL)
        && file.get("start").and_then(Value::as_str) == Some(start)
        && at <= now_ms
        && now_ms - at <= SERVICE_STALE_MS)
        .then_some(file)
}

/// JS truthiness of a JSON value.
fn js_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `sameAddress(a, b)` (`embedding-service.cjs:6-9`): trailing slashes and one trailing `/v1` do
/// not count. Case counts.
pub fn same_address(a: Option<&str>, b: Option<&str>) -> bool {
    let n = |v: Option<&str>| {
        let t = js_trim(v.unwrap_or_default());
        let t = t.trim_end_matches('/');
        let t = t.strip_suffix("/v1").unwrap_or(t);
        (!t.is_empty()).then(|| t.to_owned())
    };
    n(a) == n(b)
}

/// `embeddingOff(target)` (`rag.cjs:289-294`) with the service in native-owner mode.
/// - `snapshot`: [`current_snapshot`] of the worker's status file, if any;
/// - `service_url`: `EMBEDDING_SERVICE_URL`;
/// - `target_base`: the active target's `baseUrl` (`None` or empty: the inference engine).
///
/// True when the model manager runs the embedding service, it is not known to be enabled (off, or
/// its observation is stale), and the target embeds on that service or on the shared engine.
pub fn embedding_off(
    snapshot: Option<&Value>,
    now_ms: f64,
    service_url: Option<&str>,
    target_base: Option<&str>,
) -> bool {
    // observeNative: an observation needs a truthy service and a finite time not in the future,
    // and more than 6 s after the (never set, here) last mutation of this process.
    let Some(s) = snapshot else { return false };
    let service = s.get("service");
    let Some(at) = s.get("serviceAt").and_then(Value::as_f64) else {
        return false;
    };
    if !js_truthy(service) || at > now_ms || at <= 6000.0 {
        return false;
    }
    // enabled(): null while the observation is stale, else `enabled === true`.
    let stale = js_truthy(service.and_then(|v| v.get("stale"))) || now_ms - at > SERVICE_STALE_MS;
    let enabled = !stale && service.and_then(|v| v.get("enabled")) == Some(&Value::Bool(true));
    if enabled {
        return false;
    }
    let base = String::from_utf16_lossy(trim(&units(service_url.unwrap_or_default())));
    let base = base.trim_end_matches('/');
    match target_base.filter(|b| !b.is_empty()) {
        None => true,
        Some(address) => !base.is_empty() && same_address(Some(address), Some(base)),
    }
}

#[cfg(test)]
mod tests {
    use super::{origin, same_address};

    #[test]
    fn origins_follow_the_url_standard() {
        assert_eq!(
            origin(Some("http://QA-Engine:80/v1")).as_deref(),
            Some("http://qa-engine")
        );
        assert_eq!(
            origin(Some(" https://qa:443 ")).as_deref(),
            Some("https://qa")
        );
        assert_eq!(
            origin(Some("http://[0:0::1]:8080")).as_deref(),
            Some("http://[::1]:8080")
        );
        assert_eq!(origin(Some("not a url")), None);
        assert_eq!(origin(None), None);
    }

    #[test]
    fn addresses() {
        assert!(same_address(Some("http://a:1/v1/"), Some("http://a:1")));
        assert!(!same_address(Some("http://A:1"), Some("http://a:1")));
        assert!(same_address(None, Some("  ")));
    }
}
