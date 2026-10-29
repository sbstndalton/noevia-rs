//! `GET /api/plugins/directory?kind=mcp|skills&q=&starters=1` (sbstndalton/noevia#2190), ported
//! from core routes/plugin-directory.cjs: read-only browsing of the public MCP registry and of
//! Anthropic's public skills repository, for any signed-in user. Permanent, no switch; Node
//! refuses the route (core rust-plugin-directory.cjs `RUST_PLUGIN_DIRECTORY_OWNED`), so the web
//! supervisor pairs this front's `rust-plugin-directory` capability with that Core.
//!
//! Outbound policy, as Node's: only the two fixed hosts are ever fetched (the query is the only
//! request input and travels percent-encoded as `encodeURIComponent` does); no redirect is
//! followed (Node's `redirect: 'error'`); an 8 s deadline; a 4 MiB body cap; no proxy from the
//! environment (Node's fetch reads none). Any failure is the one 502 Node answered. Answers are
//! kept an hour per (kind, query), the whole cache dropped past 200 keys, as Node's map was.
//!
//! The QA overrides: `NOEVIA_QA_MCP_REGISTRY` (Node read it for the starter lookups only; here it
//! is the registry base for the listing too) and `NOEVIA_QA_SKILLS_DIRECTORY` (new: the skills
//! listing URL). They exist so that synthetic runs (tests, the contract replay) never reach the
//! Internet; production leaves both unset. `NOEVIA_QA_ALLOW_LOOPBACK_MCP=1` lets an
//! `http://127.0.0.1:<port>/` remote count as installable, as core directory-mcp.cjs does.
//!
//! Order follows core: the session gate (401/403/CSRF), then a 405 for every method but GET.

use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::JValue;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DIRECTORY: &str = "/api/plugins/directory";

const REGISTRY: &str = "https://registry.modelcontextprotocol.io";
const SKILLS: &str = "https://api.github.com/repos/anthropics/skills/contents/skills";
const TTL: Duration = Duration::from_secs(60 * 60);
const DEADLINE: Duration = Duration::from_secs(8);
/// A directory listing of 40 servers or one repo's skill folders; 4 MB is far above that (#920).
const DIRECTORY_CAP: usize = 4 * 1024 * 1024;
const UNREACHABLE: &str = "The directory could not be reached right now.";
const STARTERS: &str = include_str!("pages/plugin-starters.json");

/// Where the directory is read from (config.rs, from the QA keys above).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sources {
    /// The MCP registry base, no trailing slash.
    pub registry: String,
    /// The skills listing URL.
    pub skills: String,
    /// `NOEVIA_QA_ALLOW_LOOPBACK_MCP === '1'`.
    pub loopback_mcp: bool,
}

impl Default for Sources {
    fn default() -> Self {
        Sources {
            registry: REGISTRY.into(),
            skills: SKILLS.into(),
            loopback_mcp: false,
        }
    }
}

impl Sources {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Self {
        let set = |k: &str| get(k).filter(|v| !v.is_empty());
        Sources {
            registry: set("NOEVIA_QA_MCP_REGISTRY").unwrap_or_else(|| REGISTRY.into()),
            skills: set("NOEVIA_QA_SKILLS_DIRECTORY").unwrap_or_else(|| SKILLS.into()),
            loopback_mcp: get("NOEVIA_QA_ALLOW_LOOPBACK_MCP").as_deref() == Some("1"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Mcp,
    Skills,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Mcp => "mcp",
            Kind::Skills => "skills",
        }
    }

    /// core SOURCES[kind], in its key order.
    fn source(self) -> JValue {
        let (label, home) = match self {
            Kind::Mcp => ("MCP registry", "https://registry.modelcontextprotocol.io"),
            Kind::Skills => (
                "anthropics/skills on GitHub",
                "https://github.com/anthropics/skills",
            ),
        };
        JValue::obj([
            ("label", JValue::Str(label.into())),
            ("home", JValue::Str(home.into())),
        ])
    }
}

/// The per-process directory: its HTTP client and the hour-long answer cache.
pub struct Directory {
    client: Option<reqwest::Client>,
    sources: Sources,
    cache: Mutex<HashMap<String, (Instant, Vec<JValue>)>>,
}

#[derive(Debug)]
struct Failed;

impl Directory {
    pub fn new(sources: Sources) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(DEADLINE)
            .build()
            .ok();
        Directory {
            client,
            sources,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn hit(&self, key: &str) -> Option<Vec<JValue>> {
        let cache = self.cache.lock().ok()?;
        cache
            .get(key)
            .filter(|(at, _)| at.elapsed() < TTL)
            .map(|(_, items)| items.clone())
    }

    fn keep(&self, key: String, items: &[JValue], bounded: bool) {
        if let Ok(mut cache) = self.cache.lock() {
            if bounded && cache.len() > 200 {
                cache.clear();
            }
            cache.insert(key, (Instant::now(), items.to_vec()));
        }
    }

    /// GET `url` with Node's headers; the parsed JSON of a 2xx reply under the cap.
    async fn fetch(&self, url: &str) -> Result<JValue, Failed> {
        let client = self.client.as_ref().ok_or(Failed)?;
        let mut res = client
            .get(url)
            .header("accept", "application/json")
            .header("user-agent", "noevia")
            .send()
            .await
            .map_err(|_| Failed)?;
        if !res.status().is_success() {
            return Err(Failed);
        }
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|_| Failed)? {
            if bytes.len() + chunk.len() > DIRECTORY_CAP {
                return Err(Failed);
            }
            bytes.extend_from_slice(&chunk);
        }
        js_json::parse(&String::from_utf8_lossy(&bytes)).map_err(|_| Failed)
    }

    /// core `load(kind, q)`.
    async fn load(&self, kind: Kind, q: &str) -> Result<Vec<JValue>, Failed> {
        let key = format!("{}:{q}", kind.name());
        if let Some(items) = self.hit(&key) {
            return Ok(items);
        }
        let url = match kind {
            Kind::Mcp if q.is_empty() => format!("{}/v0/servers?limit=40", self.sources.registry),
            Kind::Mcp => format!(
                "{}/v0/servers?limit=40&search={}",
                self.sources.registry,
                encode_uri_component(q)
            ),
            Kind::Skills => self.sources.skills.clone(),
        };
        let listing = self.fetch(&url).await?;
        let mut items = match kind {
            Kind::Mcp => mcp_items(&listing, self.sources.loopback_mcp),
            Kind::Skills => skill_items(&listing),
        };
        if kind == Kind::Skills && !q.is_empty() {
            let needle = q.to_lowercase();
            items.retain(|i| {
                i.get("name")
                    .as_str()
                    .is_some_and(|n| n.to_lowercase().contains(&needle))
            });
        }
        self.keep(key, &items, true);
        Ok(items)
    }

    /// core `findRegistryServer(name)`: the registry entry with exactly this name, or none.
    async fn find_registry_server(&self, name: &str) -> Result<Option<JValue>, Failed> {
        if name.is_empty() || js_json::js_len(name) > 200 {
            return Ok(None);
        }
        let url = format!(
            "{}/v0/servers?limit=40&search={}",
            self.sources.registry,
            encode_uri_component(name)
        );
        let listing = self.fetch(&url).await?;
        Ok(mcp_items(&listing, self.sources.loopback_mcp)
            .into_iter()
            .find(|i| i.get("id").as_str() == Some(name)))
    }

    /// core `starters(kind)`: noevia's curated picks, each resolved against the live directory.
    async fn starters(&self, kind: Kind) -> Result<Vec<JValue>, Failed> {
        let key = format!("starters:{}", kind.name());
        if let Some(items) = self.hit(&key) {
            return Ok(items);
        }
        let picks = starter_picks(kind);
        let mut items = Vec::new();
        match kind {
            Kind::Skills => {
                let all = self.load(Kind::Skills, "").await?;
                for (id, why) in &picks {
                    if let Some(item) = all.iter().find(|i| i.get("id").as_str() == Some(id)) {
                        items.push(with_why(item.clone(), why));
                    }
                }
            }
            Kind::Mcp => {
                let found =
                    futures_join(picks.iter().map(|(id, _)| self.find_registry_server(id))).await;
                for ((_, why), item) in picks.iter().zip(found) {
                    if let Ok(Some(item)) = item {
                        items.push(with_why(item, why));
                    }
                }
            }
        }
        self.keep(key, &items, false);
        Ok(items)
    }
}

/// Every future to completion, polled together, outputs in order (Node's `Promise.all` over
/// lookups that never reject).
async fn futures_join<F: std::future::Future>(futs: impl Iterator<Item = F>) -> Vec<F::Output> {
    let mut pending: Vec<Option<std::pin::Pin<Box<F>>>> = futs.map(|f| Some(Box::pin(f))).collect();
    let mut results: Vec<Option<F::Output>> = pending.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut busy = false;
        for (slot, result) in pending.iter_mut().zip(results.iter_mut()) {
            if let Some(f) = slot.as_mut() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => {
                        *result = Some(v);
                        *slot = None;
                    }
                    std::task::Poll::Pending => busy = true,
                }
            }
        }
        if busy {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    results.into_iter().flatten().collect()
}

/// `{ ...item, why }`: `why` replaces an existing key in place, else goes last.
fn with_why(item: JValue, why: &str) -> JValue {
    match item {
        JValue::Obj(mut pairs) => {
            match pairs.iter_mut().find(|(k, _)| k == "why") {
                Some((_, v)) => *v = JValue::Str(why.into()),
                None => pairs.push(("why".into(), JValue::Str(why.into()))),
            }
            JValue::Obj(pairs)
        }
        other => other,
    }
}

/// plugin-starters.json `mcp` / `skills`: (id, why).
fn starter_picks(kind: Kind) -> Vec<(String, String)> {
    let Ok(all) = js_json::parse(STARTERS) else {
        return Vec::new();
    };
    match all.get(kind.name()) {
        JValue::Arr(list) => list
            .iter()
            .map(|p| {
                (
                    js_string(p.get("id")),
                    p.get("why").as_str().unwrap_or("").to_owned(),
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// JavaScript's WhiteSpace and LineTerminator (what `\s` and `String.prototype.trim` use).
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

/// `String(v)`; a value with no string form (never JSON data) is ''.
fn js_string(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

/// `String(v || '')`.
fn string_or_empty(v: &JValue) -> String {
    if v.truthy() {
        js_string(v)
    } else {
        String::new()
    }
}

/// `encodeURIComponent(s)`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// core directory-mcp.cjs `hostedUrlOk(url)`.
fn hosted_url_ok(url: &str, loopback: bool) -> bool {
    let plain = |rest: &str| !rest.chars().any(|c| c == '{' || c == '}' || js_space(c));
    if let Some(rest) = url.strip_prefix("https://") {
        if !rest.is_empty() && plain(rest) {
            return true;
        }
    }
    if !loopback {
        return false;
    }
    let Some(rest) = url.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let path = rest.get(digits..).unwrap_or("");
    digits > 0 && path.starts_with('/') && plain(path)
}

/// `RESERVED_HEADERS`, for a name already known to be `[A-Za-z0-9-]{1,64}`.
fn reserved_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(
        n.as_str(),
        "host"
            | "cookie"
            | "connection"
            | "content-length"
            | "content-type"
            | "transfer-encoding"
            | "origin"
            | "accept"
            | "mcp-session-id"
            | "mcp-protocol-version"
    ) || n.starts_with("proxy-")
        || n.starts_with("x-forwarded-")
}

fn plain_header_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `(s.match(/\{[^}]+\}/g) || []).length`.
fn template_slots(s: &str) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let (mut i, mut n) = (0, 0);
    while i < chars.len() {
        if chars.get(i) == Some(&'{') {
            if let Some(close) = chars
                .get(i + 1..)
                .and_then(|rest| rest.iter().position(|c| *c == '}'))
            {
                if close > 0 {
                    n += 1;
                    i += close + 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    n
}

/// core `cleanHeaders(list)`.
fn clean_headers(list: &JValue) -> Vec<JValue> {
    let JValue::Arr(list) = list else {
        return Vec::new();
    };
    list.iter()
        .filter(|h| h.truthy() && h.get("name").as_str().is_some())
        .map(|h| {
            let template = match h.get("value") {
                JValue::Str(v) if template_slots(v) == 1 => JValue::Str(js_json::js_slice(v, 200)),
                _ => JValue::Null,
            };
            JValue::obj([
                ("name", h.get("name").clone()),
                ("required", JValue::Bool(h.get("isRequired").truthy())),
                (
                    "secret",
                    JValue::Bool(h.get("isSecret") != &JValue::Bool(false)),
                ),
                (
                    "description",
                    JValue::Str(js_json::js_slice(
                        &string_or_empty(h.get("description")),
                        300,
                    )),
                ),
                ("template", template),
            ])
        })
        .collect()
}

/// core `installable(s)`: the keys spread onto the item.
fn installable(s: &JValue, loopback: bool) -> Vec<(String, JValue)> {
    let empty = Vec::new();
    let remotes = match s.get("remotes") {
        JValue::Arr(r) => r,
        _ => &empty,
    };
    let usable = remotes.iter().find(|r| {
        r.truthy()
            && r.get("type").as_str() == Some("streamable-http")
            && r.get("url")
                .as_str()
                .is_some_and(|u| hosted_url_ok(u, loopback))
            && clean_headers(r.get("headers")).iter().all(|h| {
                h.get("name")
                    .as_str()
                    .is_some_and(|n| plain_header_name(n) && !reserved_header(n))
            })
    });
    if let Some(usable) = usable {
        let headers = clean_headers(usable.get("headers"));
        let mut out = vec![
            ("remoteUrl".to_owned(), usable.get("url").clone()),
            ("installable".to_owned(), JValue::Bool(true)),
        ];
        if !headers.is_empty() {
            let needs_key = headers.iter().any(|h| h.get("required").truthy());
            out.push(("headers".into(), JValue::Arr(headers)));
            out.push(("needsKey".into(), JValue::Bool(needs_key)));
        }
        return out;
    }
    let why = if remotes.is_empty() {
        "Runs as a program on the server; not supported"
    } else if remotes
        .iter()
        .any(|r| r.get("type").as_str() == Some("streamable-http"))
    {
        "Uses an address or header noevia cannot accept"
    } else {
        "Uses a connection type noevia does not support"
    };
    vec![
        ("installable".into(), JValue::Bool(false)),
        ("notInstallable".into(), JValue::Str(why.into())),
    ]
}

const GENERIC_NAME_SEGMENTS: &[&str] = &[
    "com", "org", "net", "io", "ai", "co", "app", "dev", "www", "mcp",
];

/// core `titleCase(seg)`: `-`/`_` runs to a space, trimmed, each ASCII word start upper-cased.
fn title_case(seg: &str) -> String {
    let mut spaced = String::with_capacity(seg.len());
    let mut in_run = false;
    for c in seg.chars() {
        if c == '-' || c == '_' {
            if !in_run {
                spaced.push(' ');
            }
            in_run = true;
        } else {
            spaced.push(c);
            in_run = false;
        }
    }
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(spaced.len());
    let mut prev_word = false;
    for c in js_trim(&spaced).chars() {
        out.push(if word(c) && !prev_word {
            c.to_ascii_uppercase()
        } else {
            c
        });
        prev_word = word(c);
    }
    out
}

/// core `hostSlug(url)`.
fn host_slug(url: &JValue) -> String {
    if matches!(url, JValue::Undefined) {
        return String::new();
    }
    let Ok(parsed) = url::Url::parse(&js_string(url)) else {
        return String::new();
    };
    let host = parsed.host_str().unwrap_or("");
    let host = host.strip_prefix("www.").unwrap_or(host);
    host.split('.').next().unwrap_or("").to_owned()
}

/// core `fallbackServerName(s)`.
fn fallback_server_name(s: &JValue) -> JValue {
    let title = s.get("title");
    if title.truthy() {
        return title.clone();
    }
    let raw = string_or_empty(s.get("name"));
    let parts: Vec<&str> = raw.split('/').collect();
    let slug = parts.last().copied().unwrap_or("");
    if !slug.is_empty() && slug.to_lowercase() != "mcp" {
        return JValue::Str(slug.into());
    }
    let head = parts
        .get(..parts.len().saturating_sub(1))
        .unwrap_or(&[])
        .join(".");
    let meaningful: Vec<&str> = head
        .split('.')
        .filter(|seg| {
            !seg.is_empty() && !GENERIC_NAME_SEGMENTS.contains(&seg.to_lowercase().as_str())
        })
        .collect();
    let mut derived = meaningful
        .last()
        .map(|s| (*s).to_owned())
        .unwrap_or_default();
    if derived.is_empty() {
        derived = host_slug(s.get("repository").get("url"));
    }
    if derived.is_empty() {
        derived = host_slug(s.get("websiteUrl"));
    }
    if !derived.is_empty() {
        return JValue::Str(title_case(&derived));
    }
    JValue::Str(if raw.is_empty() { slug.into() } else { raw })
}

fn https_string(v: &JValue) -> Option<&str> {
    v.as_str().filter(|u| u.starts_with("https://"))
}

/// core `mcpItems(body)`.
pub fn mcp_items(body: &JValue, loopback: bool) -> Vec<JValue> {
    let JValue::Arr(list) = body.get("servers") else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for entry in list {
        let server = entry.get("server");
        let s = if server.truthy() { server } else { entry };
        let Some(name) = s.truthy().then(|| s.get("name").as_str()).flatten() else {
            continue;
        };
        if !seen.insert(name.to_owned()) {
            continue;
        }
        let version = s.get("version");
        let url = https_string(s.get("repository").get("url"))
            .or_else(|| https_string(s.get("websiteUrl")))
            .unwrap_or("");
        let remote = matches!(s.get("remotes"), JValue::Arr(r) if !r.is_empty());
        let mut pairs = vec![
            ("id".to_owned(), JValue::Str(name.into())),
            ("name".to_owned(), fallback_server_name(s)),
            (
                "publisher".to_owned(),
                JValue::Str(match name.split_once('/') {
                    Some((p, _)) => p.into(),
                    None => String::new(),
                }),
            ),
            (
                "description".to_owned(),
                JValue::Str(js_json::js_slice(
                    &string_or_empty(s.get("description")),
                    300,
                )),
            ),
            (
                "version".to_owned(),
                if version.truthy() {
                    version.clone()
                } else {
                    JValue::Str(String::new())
                },
            ),
            ("url".to_owned(), JValue::Str(url.into())),
            ("remote".to_owned(), JValue::Bool(remote)),
        ];
        pairs.extend(installable(s, loopback));
        out.push(JValue::Obj(pairs));
    }
    out
}

/// core `name.replace(/[-_]+/g, ' ').replace(/^./, c => c.toUpperCase())`.
fn skill_title(name: &str) -> String {
    let mut spaced = String::with_capacity(name.len());
    let mut in_run = false;
    for c in name.chars() {
        if c == '-' || c == '_' {
            if !in_run {
                spaced.push(' ');
            }
            in_run = true;
        } else {
            spaced.push(c);
            in_run = false;
        }
    }
    let mut chars = spaced.chars();
    match chars.next() {
        // `.` matches no line terminator, and only half of an astral character (left as is).
        Some(c) if !matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}') && (c as u32) <= 0xFFFF => {
            c.to_uppercase().chain(chars).collect()
        }
        _ => spaced,
    }
}

/// core `skillItems(body)`.
pub fn skill_items(body: &JValue) -> Vec<JValue> {
    let JValue::Arr(list) = body else {
        return Vec::new();
    };
    list.iter()
        .filter(|d| d.get("type").as_str() == Some("dir"))
        .filter_map(|d| d.get("name").as_str().map(|name| (d, name)))
        .map(|(d, name)| {
            let url = d
                .get("html_url")
                .as_str()
                .filter(|u| u.starts_with("https://github.com/"))
                .unwrap_or("");
            JValue::obj([
                ("id", JValue::Str(name.into())),
                ("name", JValue::Str(skill_title(name))),
                ("publisher", JValue::Str("Anthropic".into())),
                ("description", JValue::Str(String::new())),
                ("version", JValue::Str(String::new())),
                ("url", JValue::Str(url.into())),
                ("remote", JValue::Bool(false)),
            ])
        })
        .collect()
}

/// `URLSearchParams.get(name)` over the request's query (first value, form-decoded).
fn param(query: Option<&str>, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.unwrap_or("").as_bytes())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

fn answer(status: StatusCode, body: JValue) -> Response<Body> {
    reply::json_text(status, js_json::stringify(&body).unwrap_or_default(), true)
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    // Node matched the exact pathname; the table's trailing-slash tolerance is not its route.
    if path != DIRECTORY {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now)).await {
        Ok(Ok(_)) => {}
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    }
    if parts.method != Method::GET {
        return reply::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", true);
    }
    let query = parts.uri.query();
    let kind = if param(query, "kind").as_deref() == Some("skills") {
        Kind::Skills
    } else {
        Kind::Mcp
    };
    let q = js_json::js_slice(js_trim(&param(query, "q").unwrap_or_default()), 80);
    let directory = &app.plugin_directory;
    let result = if param(query, "starters").as_deref() == Some("1") {
        directory.starters(kind).await
    } else {
        directory.load(kind, &q).await
    };
    match result {
        Ok(items) => answer(
            StatusCode::OK,
            JValue::obj([("items", JValue::Arr(items)), ("source", kind.source())]),
        ),
        Err(Failed) => answer(
            StatusCode::BAD_GATEWAY,
            JValue::obj([
                ("error", JValue::Str(UNREACHABLE.into())),
                ("source", kind.source()),
            ]),
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn parse(s: &str) -> JValue {
        js_json::parse(s).unwrap()
    }

    fn text(v: &JValue) -> String {
        js_json::stringify(v).unwrap()
    }

    #[test]
    fn mcp_items_match_core_for_the_installable_shapes() {
        let body = parse(
            r#"{"servers":[
            {"server":{"name":"com.notion/mcp","description":"Notes","version":"1.0","repository":{"url":"https://github.com/n/m"},
              "remotes":[{"type":"streamable-http","url":"https://mcp.notion.com/mcp","headers":[{"name":"Authorization","isRequired":true,"value":"Bearer {api_key}","description":"Key"}]}]}},
            {"server":{"name":"com.notion/mcp"}},
            {"name":"ai.exa/exa","websiteUrl":"http://insecure","remotes":[{"type":"sse","url":"https://x"}]},
            {"name":"io.x/bad","remotes":[{"type":"streamable-http","url":"https://x/{id}"}]},
            {"name":"io.y/hdr","remotes":[{"type":"streamable-http","url":"https://y","headers":[{"name":"Cookie"}]}]},
            {"name":"com.cloudflare.mcp/mcp","title":""},
            {"name":"com.mcp/mcp","repository":{"url":"https://www.github.com/x"}},
            {"name":"pkg"},
            {"nope":1}
        ]}"#,
        );
        let items = mcp_items(&body, false);
        assert_eq!(items.len(), 7, "deduplicated, nameless dropped");
        assert_eq!(
            text(&items[0]),
            r#"{"id":"com.notion/mcp","name":"Notion","publisher":"com.notion","description":"Notes","version":"1.0","url":"https://github.com/n/m","remote":true,"remoteUrl":"https://mcp.notion.com/mcp","installable":true,"headers":[{"name":"Authorization","required":true,"secret":true,"description":"Key","template":"Bearer {api_key}"}],"needsKey":true}"#
        );
        assert_eq!(
            text(&items[1]),
            r#"{"id":"ai.exa/exa","name":"exa","publisher":"ai.exa","description":"","version":"","url":"","remote":true,"installable":false,"notInstallable":"Uses a connection type noevia does not support"}"#
        );
        assert_eq!(
            items[2].get("notInstallable").as_str(),
            Some("Uses an address or header noevia cannot accept")
        );
        assert_eq!(
            items[3].get("notInstallable").as_str(),
            Some("Uses an address or header noevia cannot accept")
        );
        assert_eq!(items[4].get("name").as_str(), Some("Cloudflare"));
        assert_eq!(
            items[5].get("name").as_str(),
            Some("Github"),
            "host slug, www. dropped"
        );
        assert_eq!(
            items[6].get("notInstallable").as_str(),
            Some("Runs as a program on the server; not supported")
        );
        assert_eq!(items[6].get("publisher").as_str(), Some(""));
        assert!(mcp_items(&JValue::Null, false).is_empty());
    }

    #[test]
    fn the_loopback_remote_is_installable_only_when_qa_allows_it() {
        let s = parse(
            r#"{"servers":[{"name":"a/b","remotes":[{"type":"streamable-http","url":"http://127.0.0.1:4100/mcp"}]}]}"#,
        );
        assert_eq!(
            mcp_items(&s, false)[0].get("installable"),
            &JValue::Bool(false)
        );
        assert_eq!(
            mcp_items(&s, true)[0].get("installable"),
            &JValue::Bool(true)
        );
        assert!(!hosted_url_ok("http://127.0.0.1:/x", true));
        assert!(!hosted_url_ok("https://", false));
        assert!(!hosted_url_ok("https://a b", false));
        assert!(!hosted_url_ok("https://a\u{feff}b", false));
        assert!(
            hosted_url_ok("https://a\u{85}b", false),
            "NEL is not a JS space"
        );
    }

    #[test]
    fn skills_titles_and_urls_follow_core() {
        let body = parse(
            r#"[{"type":"dir","name":"doc-coauthoring","html_url":"https://github.com/anthropics/skills/tree/main/skills/doc-coauthoring"},
                {"type":"file","name":"README.md"},{"type":"dir","name":"pdf","html_url":"http://x"},{"type":"dir"}]"#,
        );
        let items = skill_items(&body);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].get("name").as_str(), Some("Doc coauthoring"));
        assert_eq!(items[1].get("url").as_str(), Some(""));
        assert!(skill_items(&parse("{}")).is_empty());
    }

    #[test]
    fn helpers_match_javascript() {
        assert_eq!(encode_uri_component("a b/ü&*'"), "a%20b%2F%C3%BC%26*'");
        assert_eq!(template_slots("Bearer {key}"), 1);
        assert_eq!(template_slots("{} {a}{b}"), 2);
        assert_eq!(template_slots("{{a}"), 1);
        assert_eq!(template_slots("{a"), 0);
        assert_eq!(title_case("--hugging_face-"), "Hugging Face");
        assert_eq!(title_case("a1b c"), "A1b C");
        assert_eq!(js_trim("\u{feff} q \u{3000}"), "q");
        let starters = starter_picks(Kind::Mcp);
        assert_eq!(starters.len(), 5);
        assert_eq!(starters[0].0, "ai.exa/exa");
        assert_eq!(starter_picks(Kind::Skills).len(), 6);
        assert_eq!(
            text(&with_why(parse(r#"{"id":"x","why":"old","z":1}"#), "new")),
            r#"{"id":"x","why":"new","z":1}"#
        );
    }
}
