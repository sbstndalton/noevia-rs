//! What `modelManager.variants(repo)` and `modelManager.pull(...)` send, per manager kind
//! (`model-manager.cjs` for `lemonade` and `none`, `llamacpp-manager.cjs` and
//! `llamacpp-variants.cjs` for `llamacpp`), as [`Request`]s the host sends with http.cjs
//! `fetchJson`'s semantics, and how their answers are read. Also `models.cjs`
//! `deriveUserModelName`.
//!
//! No request carries the model loader token (`MODEL_LOADER_TOKEN`): the lemonade manager and the
//! llama.cpp router get `Authorization: Bearer <MODEL_MANAGER_API_KEY or the inference key>`
//! unless that key is empty or `local`, and the llama.cpp variants request to Hugging Face gets
//! no headers at all.

use crate::js;
use js_json::{wtf16, JValue};

/// `MODEL_MANAGER_KIND` (as `createModelManager` reads it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// No model manager: `enabled` is false.
    None,
    Lemonade,
    Llamacpp,
}

impl Kind {
    /// `String(kind || 'none').toLowerCase()`; `Err` is the boot error for any other kind.
    pub fn parse(kind: Option<&str>) -> Result<Kind, String> {
        let k = kind
            .filter(|k| !k.is_empty())
            .unwrap_or("none")
            .to_lowercase();
        match k.as_str() {
            "none" => Ok(Kind::None),
            "lemonade" => Ok(Kind::Lemonade),
            "llamacpp" => Ok(Kind::Llamacpp),
            _ => Err(format!("unsupported MODEL_MANAGER_KIND: {k}")),
        }
    }
}

/// The model manager the deployment configured (index.cjs: `MODEL_MANAGER_KIND`,
/// `MODEL_MANAGER_BASE_URL` (or `LEMONADE_BASE_URL`, or the inference base), `MODEL_MANAGER_API_KEY`
/// (or the inference key)).
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub kind: Kind,
    pub base_url: String,
    pub api_key: Option<String>,
}

/// Where a [`Request`] goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The configured model manager (its base URL): the llama.cpp manager's maintenance hold
    /// header belongs on these only.
    Manager,
    /// The public Hugging Face API: no credentials, no hold header, nothing but what the request
    /// lists.
    PublicHub,
}

/// Debug without the key (it is a credential).
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Debug with the `Authorization` value redacted.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let secret = k.eq_ignore_ascii_case("authorization");
                (k.as_str(), if secret { "<redacted>" } else { v.as_str() })
            })
            .collect();
        f.debug_struct("Request")
            .field("target", &self.target)
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body", &self.body)
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

impl Config {
    /// The base every manager path is appended to: without trailing slashes, and for llama.cpp
    /// without a final `/v1` (llamacpp-manager.cjs also refuses at boot a base that is not an
    /// http(s) URL without credentials, query or fragment).
    pub fn base(&self) -> String {
        let trimmed = self.base_url.trim_end_matches('/');
        match self.kind {
            Kind::Llamacpp => trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string(),
            _ => trimmed.to_string(),
        }
    }

    /// `headers()`: `Content-Type`, then `Authorization` for a key that is set and not `local`.
    /// The llama.cpp manager adds `X-Noevia-Maintenance` while this process holds a maintenance
    /// hold (`holdHeaders()`); that one is the host's to add.
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut out = vec![("Content-Type".to_string(), "application/json".to_string())];
        if let Some(key) = self
            .api_key
            .as_deref()
            .filter(|k| !k.is_empty() && *k != "local")
        {
            out.push(("Authorization".to_string(), format!("Bearer {key}")));
        }
        out
    }
}

/// One HTTP request the host sends as http.cjs `fetchJson(url, init, timeoutMs)` does: `redirect:
/// 'error'` (a redirect throws), an 8 MiB reply cap, the deadline over the whole exchange.
#[derive(Clone, PartialEq)]
pub struct Request {
    /// Who the request is for: the host attaches nothing of the manager's (its key, the
    /// maintenance hold header) to a [`Target::PublicHub`] request.
    pub target: Target,
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// JSON text.
    pub body: Option<String>,
    pub timeout_ms: u32,
}

/// `fetchJson`'s answer: `ok` is a 2xx status; `body` the reply parsed as JSON (with lone
/// surrogates kept: [`js_json::wtf16::parse`]), else its text; `undefined` for a reply over the
/// cap (Node's `{ ok: false, status, error: 'response too large' }`).
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub ok: bool,
    pub status: u16,
    pub body: JValue,
}

/// An exception that leaves the route handler: index.cjs answers it with http.cjs
/// `errorResponse` (a 4xx status keeps its message; anything else is `Internal error`).
#[derive(Debug, Clone, PartialEq)]
pub struct Thrown {
    /// `Number(err.status)`, when the error carries one.
    pub status: Option<f64>,
    pub message: String,
}

impl Thrown {
    /// An error without a status (a TypeError, a network failure): a 500.
    pub fn internal(message: &str) -> Thrown {
        Thrown {
            status: None,
            message: message.to_string(),
        }
    }

    /// `errorResponse(err)`: the status and body index.cjs answers with.
    pub fn answer(&self) -> (u16, JValue) {
        let status = self.status.filter(|s| s.is_finite() && s.trunc() == *s);
        match status {
            Some(s) if (400.0..500.0).contains(&s) => {
                let message = if self.message.is_empty() {
                    "Request failed"
                } else {
                    &self.message
                };
                (s as u16, JValue::obj([("error", JValue::from(message))]))
            }
            Some(s) if (500.0..600.0).contains(&s) => (
                s as u16,
                JValue::obj([("error", JValue::from("Internal error"))]),
            ),
            _ => (
                500,
                JValue::obj([("error", JValue::from("Internal error"))]),
            ),
        }
    }
}

/// The error `requireEnabled()` throws on a deployment without a model manager.
pub const DISABLED: &str =
    "model management is disabled; configure MODEL_MANAGER_KIND to enable it";

/// `modelManager.variants(repo)`'s request, for a repository the route has checked.
pub fn variants_request(config: &Config, repo: &str) -> Result<Request, Thrown> {
    match config.kind {
        Kind::None => Err(Thrown::internal(DISABLED)),
        Kind::Lemonade => Ok(Request {
            target: Target::Manager,
            method: "GET",
            url: format!(
                "{}/api/v1/pull/variants?checkpoint={}",
                config.base(),
                js::encode_uri_component(repo)
                    .ok_or_else(|| Thrown::internal(js::URI_MALFORMED))?
            ),
            headers: config.headers(),
            body: None,
            timeout_ms: 20_000,
        }),
        // Public Hugging Face file metadata; the inference key is not forwarded.
        Kind::Llamacpp => Ok(Request {
            target: Target::PublicHub,
            method: "GET",
            url: format!("https://huggingface.co/api/models/{repo}?blobs=true"),
            headers: Vec::new(),
            body: None,
            timeout_ms: 20_000,
        }),
    }
}

/// `modelManager.variants(repo)`: the manager's answer (lemonade), or llamacpp-variants.cjs's
/// reading of Hugging Face's file list.
pub fn variants(
    config: &Config,
    repo: &str,
    send: &mut dyn FnMut(&Request) -> Result<Reply, Thrown>,
) -> Result<Reply, Thrown> {
    let request = variants_request(config, repo)?;
    let reply = send(&request)?;
    if config.kind != Kind::Llamacpp || !reply.ok {
        return Ok(reply);
    }
    let unavailable = || Reply {
        ok: false,
        status: 502,
        body: JValue::obj([("error", JValue::from("Public file metadata unavailable"))]),
    };
    if reply.body.get("private").truthy() {
        return Ok(unavailable());
    }
    let JValue::Arr(siblings) = reply.body.get("siblings") else {
        return Ok(unavailable());
    };
    Ok(Reply {
        ok: true,
        status: 200,
        body: JValue::obj([
            ("suggested_name", JValue::from(repo)),
            ("variants", JValue::Arr(variant_rows(siblings)?)),
        ]),
    })
}

/// One `.gguf` file name read by llamacpp-variants.cjs's pattern
/// `/(?:^|[.\/_-])((?:IQ|Q)\d[\w]*|BF16|F16|F32)(?:-(\d{5})-of-(\d{5}))?\.gguf$/i`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quant {
    /// The quantization, upper case.
    pub quant: String,
    pub shard: Option<String>,
    pub count: Option<String>,
}

fn digits(b: &[u8], at: usize, n: usize) -> Option<String> {
    let run = b.get(at..at + n)?;
    run.iter()
        .all(u8::is_ascii_digit)
        .then(|| String::from_utf8_lossy(run).into_owned())
}

/// The pattern's tail at `t`: `(?:-(\d{5})-of-(\d{5}))?\.gguf$`.
fn tail(b: &[u8], t: usize) -> Option<(Option<String>, Option<String>)> {
    let rest = b.get(t..)?;
    if rest.len() == 20
        && rest.first() == Some(&b'-')
        && js::starts_with_ci(rest.get(6..)?, "-of-")
        && js::starts_with_ci(rest.get(15..)?, ".gguf")
    {
        if let (Some(shard), Some(count)) = (digits(b, t + 1, 5), digits(b, t + 10, 5)) {
            return Some((Some(shard), Some(count)));
        }
    }
    (rest.len() == 5 && js::starts_with_ci(rest, ".gguf")).then_some((None, None))
}

/// The pattern's first match in `name`, if any.
pub fn quant_of(name: &str) -> Option<Quant> {
    let b = name.as_bytes();
    let delimiter = |i: usize| matches!(b.get(i), Some(b'.' | b'/' | b'_' | b'-'));
    let at = |g: usize| -> Option<Quant> {
        let rest = b.get(g..)?;
        let mut tries: Vec<usize> = Vec::new();
        // (?:IQ|Q)\d[\w]*: the word run is taken whole (the tail starts with '-' or '.', which it
        // cannot hold, so a shorter run never matches).
        for lead in ["IQ", "Q"] {
            if js::starts_with_ci(rest, lead)
                && rest.get(lead.len()).is_some_and(u8::is_ascii_digit)
            {
                let mut end = g + lead.len() + 1;
                while b.get(end).copied().is_some_and(js::is_word) {
                    end += 1;
                }
                tries.push(end);
            }
        }
        for fixed in ["BF16", "F16", "F32"] {
            if js::starts_with_ci(rest, fixed) {
                tries.push(g + fixed.len());
            }
        }
        tries.into_iter().find_map(|t| {
            let (shard, count) = tail(b, t)?;
            Some(Quant {
                quant: String::from_utf8_lossy(b.get(g..t)?).to_ascii_uppercase(),
                shard,
                count,
            })
        })
    };
    for s in 0..=b.len() {
        let mut starts = Vec::new();
        if s == 0 {
            starts.push(0);
        }
        if delimiter(s) {
            starts.push(s + 1);
        }
        if let Some(q) = starts.into_iter().find_map(at) {
            return Some(q);
        }
    }
    None
}

/// `/mmproj/i.test(name)`.
fn is_mmproj(name: &str) -> bool {
    let b = name.as_bytes();
    (0..b.len()).any(|i| js::starts_with_ci(b.get(i..).unwrap_or(&[]), "mmproj"))
}

/// `f.name.replace(/-\d{5}-of-\d{5}\.gguf$/i, '')`.
fn stem(name: &str) -> &str {
    let b = name.as_bytes();
    let Some(t) = b.len().checked_sub(20) else {
        return name;
    };
    match tail(b, t) {
        Some((Some(_), Some(_))) => name.get(..t).unwrap_or(name),
        _ => name,
    }
}

struct File {
    name: String,
    shard: Option<String>,
    count: Option<String>,
}

fn number(s: Option<&String>) -> f64 {
    s.map_or(f64::NAN, |s| chat_turn::js::string_to_number(s))
}

/// The variants llamacpp-variants.cjs offers for Hugging Face's `siblings`: one row per
/// quantization that is a single file or a complete, consistently named shard set (`name`,
/// `primary_file`, `files`), in the order the quantizations first appear. The route only looks a
/// row up by name, so the rows' `size_bytes` and their `localeCompare` order are not computed.
/// `Err`: a `null` sibling (`file.rfilename` throws).
pub fn variant_rows(siblings: &[JValue]) -> Result<Vec<JValue>, Thrown> {
    // In the order the quantizations first appear (core's Map), indexed by quantization.
    let mut groups: Vec<(String, Vec<File>)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for file in siblings {
        if file == &JValue::Null {
            return Err(Thrown::internal(
                "Cannot read properties of null (reading 'rfilename')",
            ));
        }
        let Some(name) = file.get("rfilename").as_str() else {
            continue;
        };
        if is_mmproj(name) {
            continue;
        }
        let Some(q) = quant_of(name) else { continue };
        let f = File {
            name: name.to_string(),
            shard: q.shard,
            count: q.count,
        };
        match index.get(&q.quant).and_then(|&at| groups.get_mut(at)) {
            Some((_, list)) => list.push(f),
            None => {
                index.insert(q.quant.clone(), groups.len());
                groups.push((q.quant, vec![f]));
            }
        }
    }
    let mut rows = Vec::new();
    for (name, files) in groups {
        let Some(first) = files.first() else { continue };
        let count = number(first.count.as_ref());
        let complete = first.shard.is_some()
            && count == files.len() as f64
            && {
                let mut shards: Vec<Option<&String>> =
                    files.iter().map(|f| f.shard.as_ref()).collect();
                shards.sort();
                shards.dedup();
                shards.len() as f64 == count
            }
            && files.iter().all(|f| {
                let n = number(f.shard.as_ref());
                f.count == first.count
                    && stem(&f.name) == stem(&first.name)
                    && n >= 1.0
                    && n <= count
            });
        if (files.len() != 1 && !complete) || (first.shard.is_some() && !complete) {
            continue;
        }
        rows.push(JValue::obj([
            ("name", JValue::from(name)),
            ("primary_file", JValue::from(first.name.as_str())),
            (
                "files",
                JValue::Arr(
                    files
                        .iter()
                        .map(|f| JValue::from(f.name.as_str()))
                        .collect(),
                ),
            ),
        ]));
    }
    Ok(rows)
}

/// `models.cjs` `deriveUserModelName(checkpoint)`: `user.<last repo segment>[-<variant>]`, every
/// UTF-16 unit outside `[A-Za-z0-9._-]` as `-` (a character outside the BMP is two). `Err`:
/// `String(checkpoint)` throws.
pub fn derive_user_model_name(checkpoint: &JValue) -> Result<String, Thrown> {
    let text = js::to_string(checkpoint).ok_or_else(|| Thrown::internal(js::NO_PRIMITIVE))?;
    let mut parts = text.split(':');
    let repo = parts.next().unwrap_or("");
    let variant = parts.next();
    let last = repo.rsplit('/').next().unwrap_or("");
    let base = if last.is_empty() { repo } else { last };
    let safe = |t: &str| -> String {
        let mut out = String::new();
        for c in t.chars() {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                out.push(c);
            } else {
                let units = wtf16::len(c.encode_utf8(&mut [0u8; 4]));
                out.extend(std::iter::repeat_n('-', units));
            }
        }
        out
    };
    let variant = variant.map(safe).unwrap_or_default();
    Ok(if variant.is_empty() {
        format!("user.{}", safe(base))
    } else {
        format!("user.{}-{variant}", safe(base))
    })
}

/// `/^[\w.-]+\/[\w.-]+(?::[\w.-]+)?$/` (llamacpp-manager.cjs `pull`).
pub fn is_llamacpp_checkpoint(s: &str) -> bool {
    let (repo, quant) = match s.split_once(':') {
        Some((repo, quant)) => (repo, Some(quant)),
        None => (s, None),
    };
    js::is_repo(repo)
        && quant.is_none_or(|q| {
            !q.is_empty() && q.bytes().all(|b| js::is_word(b) || b == b'.' || b == b'-')
        })
}

/// The llama.cpp manager's refusal of a checkpoint that is not `owner/repo[:quant]`.
pub const LLAMACPP_CHECKPOINT: &str = "Choose a Hugging Face repository and quantization";

/// The lemonade manager's pull request (`stream` + `subscribe: false` makes it answer with a job
/// snapshot at once).
pub fn lemonade_pull_request(
    config: &Config,
    model_name: &JValue,
    checkpoint: &JValue,
    recipe: &JValue,
) -> Request {
    let body = JValue::obj([
        ("model_name", model_name.clone()),
        ("checkpoint", checkpoint.clone()),
        ("recipe", recipe.clone()),
        ("stream", JValue::Bool(true)),
        ("subscribe", JValue::Bool(false)),
    ]);
    Request {
        target: Target::Manager,
        method: "POST",
        url: format!("{}/api/v1/pull", config.base()),
        headers: config.headers(),
        body: wtf16::stringify(&body),
        timeout_ms: 600_000,
    }
}

/// The llama.cpp router's pull request (`POST /models`, `post`'s default deadline).
pub fn llamacpp_pull_request(config: &Config, checkpoint: &JValue) -> Request {
    Request {
        target: Target::Manager,
        method: "POST",
        url: format!("{}/models", config.base()),
        headers: config.headers(),
        body: wtf16::stringify(&JValue::obj([("model", checkpoint.clone())])),
        timeout_ms: 120_000,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn quantizations_are_read_as_core_reads_them() {
        let q = |n: &str| quant_of(n).map(|q| (q.quant, q.shard, q.count));
        assert_eq!(q("m-Q4_K_M.gguf"), Some(("Q4_K_M".into(), None, None)));
        assert_eq!(
            q("x/m.iq2_xs-00001-of-00003.GGUF"),
            Some(("IQ2_XS".into(), Some("00001".into()), Some("00003".into())))
        );
        assert_eq!(q("bf16.gguf"), Some(("BF16".into(), None, None)));
        assert_eq!(q("m-Q.gguf"), None);
        assert_eq!(q("a.Q4.b_Q5_K.gguf"), Some(("Q5_K".into(), None, None)));
        assert_eq!(q("m-F16-1-of-2.gguf"), None);
    }

    #[test]
    fn debug_never_shows_the_key() {
        let key = "qa-secret-key-0451";
        let config = Config {
            kind: Kind::Lemonade,
            base_url: "http://qa-manager.invalid".into(),
            api_key: Some(key.into()),
        };
        let request = lemonade_pull_request(
            &config,
            &JValue::from("qa"),
            &JValue::from("qa/x"),
            &JValue::from("llamacpp"),
        );
        assert!(request.headers.iter().any(|(_, v)| v.contains(key)));
        for text in [
            format!("{config:?}"),
            format!("{request:?}"),
            format!("{config:#?}"),
            format!("{request:#?}"),
        ] {
            assert!(!text.contains(key), "{text}");
        }
    }

    #[test]
    fn the_public_hub_request_is_marked_and_bare() {
        let mut config = Config {
            kind: Kind::Llamacpp,
            base_url: "http://qa-router.invalid/v1".into(),
            api_key: Some("qa-secret-key-0451".into()),
        };
        let hub = variants_request(&config, "qa/x").unwrap();
        assert_eq!(hub.target, Target::PublicHub);
        assert!(hub.headers.is_empty());
        assert!(hub.url.starts_with("https://huggingface.co/"));
        assert_eq!(
            llamacpp_pull_request(&config, &JValue::from("qa/x")).target,
            Target::Manager
        );
        config.kind = Kind::Lemonade;
        assert_eq!(
            variants_request(&config, "qa/x").unwrap().target,
            Target::Manager
        );
    }

    #[test]
    fn derived_names_match_core() {
        let d = |v: JValue| derive_user_model_name(&v).unwrap();
        assert_eq!(
            d(JValue::from("qa-org/Model-1:Q4_K_M")),
            "user.Model-1-Q4_K_M"
        );
        assert_eq!(d(JValue::from("qa/é😀")), "user.---");
        assert_eq!(d(JValue::from("qa/")), "user.qa-");
        assert_eq!(d(JValue::from("a:")), "user.a");
        assert_eq!(d(JValue::Num(5.0)), "user.5");
    }
}
