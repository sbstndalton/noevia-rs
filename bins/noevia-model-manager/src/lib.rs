#![forbid(unsafe_code)]
#[cfg(feature = "native-badges")]
pub mod badges;
pub mod diagnose;
#[cfg(feature = "native-diagnose")]
pub mod diagnose_docker;
pub mod docker;
#[cfg(feature = "native-download-targets")]
pub mod download_targets;
pub mod embedding;
pub mod host;
pub mod idle_unload;
#[cfg(feature = "native-model-detail")]
pub mod model_detail;
#[cfg(feature = "native-model-updates")]
pub mod model_updates;
#[cfg(feature = "native-prompts")]
pub mod prompts;
pub mod sections;
#[cfg(feature = "native-settings")]
pub mod settings;

use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use futures_util::StreamExt;
use regex::Regex;
use serde_json::json;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

pub const LOG_CAP: usize = 1024 * 1024;
const INVENTORY_CAP: usize = 1024;
const REQUEST_CAP: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
pub const LEGACY_BASE: &str = "http://127.0.0.1:8091";
pub const LEGACY_TIMEOUT: Duration = Duration::from_secs(600);
pub fn legacy_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(LEGACY_TIMEOUT)
        .build()
}
pub type Job<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;

#[derive(Clone, Debug)]
pub struct Container {
    pub name: String,
    pub first_image_tag: String,
}

/// Production uses bounded native Unix ingress and Bollard model/query types.
/// Synthetic adapters are confined to tests.
pub trait DockerRead: Send + Sync {
    fn containers(&self) -> Job<'_, Vec<Container>>;
    fn logs<'a>(&'a self, name: &'a str, tail: usize) -> Job<'a, Vec<u8>>;
}

pub struct StateData {
    #[cfg(feature = "native-diagnose")]
    pub diagnosis: Option<Arc<diagnose_docker::Diagnosis>>,
    #[cfg(feature = "native-prompts")]
    pub prompts: Option<Arc<prompts::Prompts>>,
    /// noevia#1318: native read-only `GET /api/v1/settings`; PUT stays with Python.
    #[cfg(feature = "native-settings")]
    pub settings: Option<Arc<settings::Settings>>,
    /// noevia#1318: native read-only `GET /api/v1/models/updates`; Python keeps every writer.
    #[cfg(feature = "native-model-updates")]
    pub model_updates: Option<Arc<model_updates::ModelUpdates>>,
    /// noevia#1318: native read-only `GET /api/v1/models/detail`; its live fields (`loadedOn`,
    /// `fit`) are asked of the Python child, never re-implemented.
    #[cfg(feature = "native-model-detail")]
    pub model_detail: Option<Arc<model_detail::ModelDetail>>,
    /// noevia#1318: native `PUT`/`DELETE /api/v1/badges`.
    #[cfg(feature = "native-badges")]
    pub badges: Option<Arc<badges::Badges>>,
    /// noevia#1318: native read-only `GET /api/v1/download-targets`.
    #[cfg(feature = "native-download-targets")]
    pub download_targets: Option<Arc<download_targets::DownloadTargets>>,
    pub sections: Option<Arc<sections::Sections>>,
    /// #1668: present only when EMBED_CONTAINER names the embedding service.
    pub embedding: Option<Arc<embedding::EmbeddingService>>,
    pub host: Option<Arc<tokio::sync::Mutex<host::HostHistory>>>,
    pub token: String,
    pub configured: Vec<String>,
    pub docker: Arc<dyn DockerRead>,
    pub redactor: Redactor,
    pub permits: Semaphore,
    pub legacy: reqwest::Client,
    /// The legacy child's address (production: LEGACY_BASE); tests use an ephemeral port.
    pub legacy_base: String,
    /// #1747: keeps the chat preset's global `sleep-idle-seconds` in line with the setting.
    pub idle_unload: Option<Arc<idle_unload::Reconciler>>,
}

/// #1668: what the Python child is told about the guarded embedding service: its container and
/// address, so it skips only env names that point at it. `None` clears them all.
pub fn embed_guard_env(service: Option<(&str, &str)>) -> [(&'static str, Option<String>); 3] {
    [
        (
            "NOEVIA_EMBED_SERVICE_GUARDED",
            service.map(|_| "1".to_owned()),
        ),
        (
            "NOEVIA_EMBED_SERVICE_CONTAINER",
            service.map(|(c, _)| c.to_owned()),
        ),
        (
            "NOEVIA_EMBED_SERVICE_URL",
            service.map(|(_, u)| u.to_owned()),
        ),
    ]
}

/// #1668: PUT /api/v1/embedding-service restarts and stops a container, so it is never left open.
pub fn require_embed_token(container: &str, token: &str) -> Result<(), &'static str> {
    if !container.is_empty() && token.is_empty() {
        return Err("EMBED_CONTAINER needs MODEL_LOADER_TOKEN: the embedding service route must not be open");
    }
    Ok(())
}

pub fn validate_token(raw: &str) -> Result<String, &'static str> {
    if raw.is_empty() {
        return Ok(String::new());
    }
    let token =
        raw.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{001c}'..='\u{001f}'));
    if token.chars().count() < 32 || token.len() > 4096 {
        return Err("MODEL_LOADER_TOKEN must contain 32..4096 non-whitespace characters");
    }
    Ok(token.to_owned())
}

pub fn effective_names(
    configured: &[String],
    inventory: &[Container],
) -> Result<Vec<String>, &'static str> {
    if configured.len() > INVENTORY_CAP || inventory.len() > INVENTORY_CAP {
        return Err("container inventory exceeded limit");
    }
    let mut names = Vec::new();
    names
        .try_reserve(configured.len().saturating_add(inventory.len()))
        .map_err(|_| "allocation refused")?;
    let mut seen = std::collections::HashSet::new();
    seen.try_reserve(configured.len().saturating_add(inventory.len()))
        .map_err(|_| "allocation refused")?;
    for name in configured.iter().map(String::as_str).chain(
        inventory
            .iter()
            .filter(|c| {
                c.name != "model-loader" && c.first_image_tag.to_lowercase().contains("llama.cpp")
            })
            .map(|c| c.name.as_str()),
    ) {
        if name.len() > 256 {
            return Err("container name exceeded limit");
        }
        if !name.is_empty() && seen.insert(name) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// Rust regex has linear matching. Python's lookaround exclusions are checked on captures.
pub struct Redactor {
    patterns: Vec<(Regex, &'static str)>,
    keys: Regex,
}
// Python re uses str.isalnum plus underscore for word characters; Rust regex
// additionally includes marks and connector punctuation. Python whitespace also
// includes the four information separators. Nested classes preserve negation.
fn python_classes(pattern: &str) -> String {
    pattern
        .replace(r"\w", r"[\p{L}\p{N}_]")
        .replace(r"\s", r"[\s\x1c-\x1f]")
}
impl Redactor {
    pub fn new() -> Result<Self, regex::Error> {
        let mut patterns = Vec::new();
        for (p, replacement) in [
            (
                r#"(?i)(author[iİı]zat[iİı]on\s*[:=]\s*)(?:bearer|bas[iİı]c|token)?\s*[^\s,;\"']+"#,
                "${1}[redacted]",
            ),
            (
                r"(?i)(bearer\s+)[A-Za-zİıſK0-9._~+/=-]{8,}",
                "${1}[redacted]",
            ),
            (
                r"(?i)([a-zİıſK][a-z0-9+.-]*://)[^\s/:@]+:[^\s/@]+@",
                "${1}[redacted]@",
            ),
            (r"hf_[A-Za-z0-9]{20,}", "[redacted]"),
            (r"sk-[A-Za-z0-9_-]{16,}", "[redacted]"),
            (r"gh[pousr]_[A-Za-z0-9]{20,}", "[redacted]"),
            (r"AKIA[0-9A-Z]{16}", "[redacted]"),
            (
                r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
                "[redacted]",
            ),
        ] {
            patterns.push((Regex::new(&python_classes(p))?, replacement));
        }
        Ok(Self {
            patterns,
            keys: Regex::new(&python_classes(
                r#"(?i)(([\w.-]*(?:ap[iİı][_-]?key|ap[iİı]key|token|secret|password|passwd|pwd|credent[iİı]al|cook[iİı]e|sess[iİı]on)[\w.-]*)[\"']?\s*[:=]\s*[\"']?)([^\s,;&\"']+)"#,
            ))?,
        })
    }
    pub fn redact(&self, line: &str) -> Result<String, &'static str> {
        if line.len() > LOG_CAP {
            return Err("log response exceeded limit");
        }
        let mut text = line.to_owned();
        for (index, (pattern, replacement)) in self.patterns.iter().enumerate() {
            if index == 2 {
                text = self
                    .keys
                    .replace_all(&text, |caps: &regex::Captures<'_>| {
                        let key = caps
                            .get(2)
                            .map(|m| m.as_str())
                            .unwrap_or_default()
                            .to_lowercase();
                        let value = caps.get(3).map(|m| m.as_str()).unwrap_or_default();
                        // Broader redaction is intentional: numeric-looking credentials and token
                        // suffixes are also scrubbed. Preserve established numeric counter fields.
                        let counter = (key.starts_with("n_")
                            && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'))
                            || key == "tokens"
                            || key == "token_count"
                            || (key == "session"
                                && value.chars().all(|c| c.is_ascii_digit() || c == '.'));
                        if counter {
                            caps.get(0)
                                .map(|m| m.as_str())
                                .unwrap_or_default()
                                .to_owned()
                        } else {
                            format!(
                                "{}[redacted]",
                                caps.get(1).map(|m| m.as_str()).unwrap_or_default()
                            )
                        }
                    })
                    .into_owned();
            }
            text = pattern.replace_all(&text, *replacement).into_owned();
            if text.len() > LOG_CAP * 2 {
                return Err("redacted logs exceeded limit");
            }
        }
        Ok(text)
    }
    pub fn filter(&self, raw: &[u8], q: &str, level: &str) -> Result<Vec<String>, &'static str> {
        if raw.len() > LOG_CAP || q.len() > 4096 || level.len() > 16 {
            return Err("log input exceeded limit");
        }
        let decoded = String::from_utf8_lossy(raw);
        // Scrub across line separators before splitting: a separator between a key
        // and its value must not turn the value into an unlabelled secret line.
        let decoded = self.redact(&decoded)?;
        // Python's contextual Unicode lower differs from Rust. Non-ASCII search is refused
        // rather than potentially letting a secret-dependent filter differ from the oracle.
        if !q.is_ascii() {
            return Err("non-ASCII log filters are not supported");
        }
        let needle = q.to_ascii_lowercase();
        let mut out = Vec::new();
        out.try_reserve(2000).map_err(|_| "allocation refused")?;
        let mut bytes = 0usize;
        for line in python_lines(&decoded) {
            let line = self.redact(line)?;
            let low = line.to_lowercase();
            if !needle.is_empty() && !low.contains(&needle) {
                continue;
            }
            if level == "error" && !["err", "fatal"].iter().any(|s| low.contains(s)) {
                continue;
            }
            if level == "warn"
                && !["err", "warn", "wrn", "fatal"]
                    .iter()
                    .any(|s| low.contains(s))
            {
                continue;
            }
            bytes = bytes
                .checked_add(line.len())
                .ok_or("log output exceeded limit")?;
            if bytes > LOG_CAP * 2 || out.len() >= 4000 {
                return Err("log output exceeded limit");
            }
            out.try_reserve(1).map_err(|_| "allocation refused")?;
            out.push(line);
        }
        Ok(out)
    }
}

fn python_lines(text: &str) -> impl Iterator<Item = &str> {
    let mut pieces = text.split_inclusive([
        '\n', '\r', '\u{000b}', '\u{000c}', '\u{001c}', '\u{001d}', '\u{001e}', '\u{0085}',
        '\u{2028}', '\u{2029}',
    ]);
    let mut previous_cr = false;
    std::iter::from_fn(move || loop {
        let piece = pieces.next()?;
        if previous_cr && piece == "\n" {
            previous_cr = false;
            continue;
        }
        previous_cr = piece.ends_with('\r');
        let last = piece.chars().next_back()?;
        return Some(
            if matches!(
                last,
                '\n' | '\r'
                    | '\u{000b}'
                    | '\u{000c}'
                    | '\u{001c}'
                    | '\u{001d}'
                    | '\u{001e}'
                    | '\u{0085}'
                    | '\u{2028}'
                    | '\u{2029}'
            ) {
                piece
                    .get(..piece.len().saturating_sub(last.len_utf8()))
                    .unwrap_or_default()
            } else {
                piece
            },
        );
    })
}

pub fn router(state: Arc<StateData>) -> Router {
    Router::new().fallback(handler).with_state(state)
}
/// Only the exact GET path is native; PUT (the writer), other methods and slash variants keep
/// their existing forward/405/404 handling.
#[cfg(feature = "native-settings")]
fn native_settings_route(state: &StateData, method: &Method, path: &str) -> bool {
    state.settings.is_some() && *method == Method::GET && path == "/api/v1/settings"
}

/// Only the exact GET path is native; other methods and slash variants keep their existing
/// forward/405/404 handling.
#[cfg(feature = "native-model-updates")]
fn native_model_updates_route(state: &StateData, method: &Method, path: &str) -> bool {
    state.model_updates.is_some() && *method == Method::GET && path == "/api/v1/models/updates"
}

/// Only the exact GET path is native; other methods and slash variants keep their existing
/// forward/405/404 handling.
#[cfg(feature = "native-model-detail")]
fn native_model_detail_route(state: &StateData, method: &Method, path: &str) -> bool {
    state.model_detail.is_some() && *method == Method::GET && path == "/api/v1/models/detail"
}

/// Only the exact GET path is native; `POST /api/v1/downloads` (which validates its `target`
/// against the same list) and other methods keep their existing forward/405/404 handling.
#[cfg(feature = "native-download-targets")]
fn native_download_targets_route(state: &StateData, method: &Method, path: &str) -> bool {
    state.download_targets.is_some() && *method == Method::GET && path == "/api/v1/download-targets"
}

/// Only the exact path with PUT or DELETE is native; GET and other methods keep the existing 405
/// and the slash variant its forward.
#[cfg(feature = "native-badges")]
fn native_badges_route(state: &StateData, method: &Method, path: &str) -> bool {
    state.badges.is_some()
        && (*method == Method::PUT || *method == Method::DELETE)
        && path == "/api/v1/badges"
}

/// FastAPI's answer to an unhandled exception, byte for byte.
#[cfg(any(
    feature = "native-model-updates",
    feature = "native-model-detail",
    feature = "native-download-targets",
    feature = "native-badges"
))]
fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        "Internal Server Error",
    )
        .into_response()
}

fn detail(status: StatusCode, text: &'static str) -> Response {
    (status, Json(json!({"detail":text}))).into_response()
}

async fn handler(State(state): State<Arc<StateData>>, req: Request) -> Response {
    #[cfg(feature = "native-diagnose")]
    let diagnosis_deadline = std::time::Instant::now() + TIMEOUT;
    let path = req.uri().path();
    if path != "/api/v1/health" && !state.token.is_empty() {
        let supplied = req
            .headers()
            .get("x-model-loader-token")
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        if !server_auth::js::ct_eq(supplied, &state.token) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"unauthorized"})),
            )
                .into_response();
        }
    }
    if req.uri().to_string().len() > 8192 {
        return detail(StatusCode::URI_TOO_LONG, "request URI exceeded limit");
    }
    #[cfg(feature = "native-diagnose")]
    if let Some(diagnosis) = &state.diagnosis {
        let route = diagnose_docker::classify(path);
        if !matches!(route, diagnose_docker::Route::NotOwned) {
            return native_diagnosis(
                state.clone(),
                diagnosis.clone(),
                req,
                route,
                diagnosis_deadline,
            )
            .await;
        }
    }
    if path == "/api/v1/health" {
        return dispatch(state, req).await;
    }
    if req.method() == Method::GET
        && state.sections.is_some()
        && (path == "/api/v1/sections"
            || path
                .strip_prefix("/api/v1/sections/")
                .is_some_and(|n| !n.is_empty() && !n.contains('/')))
    {
        return dispatch(state, req).await;
    }
    let Some(admission) = Admission::try_new(&state) else {
        return detail(StatusCode::SERVICE_UNAVAILABLE, "service is busy");
    };
    let native = path
        .strip_prefix("/api/v1/backends/")
        .and_then(|p| p.strip_suffix("/logs"))
        .is_some();
    #[cfg(feature = "native-prompts")]
    let native = native
        || (state.prompts.is_some()
            && !matches!(prompts::classify(path), prompts::Route::NotOwned));
    #[cfg(feature = "native-settings")]
    let native = native || native_settings_route(&state, req.method(), path);
    #[cfg(feature = "native-model-updates")]
    let native = native || native_model_updates_route(&state, req.method(), path);
    #[cfg(feature = "native-download-targets")]
    let native = native || native_download_targets_route(&state, req.method(), path);
    #[cfg(feature = "native-badges")]
    let native = native || native_badges_route(&state, req.method(), path);
    // The detail route waits on the Python child for its live fields: a forwarded route's budget.
    #[cfg(feature = "native-model-detail")]
    let native = native && !native_model_detail_route(&state, req.method(), path);
    let budget = if native || (path == "/api/v1/host" && state.host.is_some()) {
        TIMEOUT
    } else if path == "/api/v1/embedding-service" && state.embedding.is_some() {
        // The Docker action is bounded by embedding::ACT_TIMEOUT and runs in its own task.
        embedding::ACT_TIMEOUT + embedding::LOCK_WAIT + Duration::from_secs(5)
    } else {
        LEGACY_TIMEOUT
    };
    // The dispatch future is dropped inside the scope (also on a timeout), so a Docker driver it
    // leaves behind is registered with this admission and joined before the permit returns.
    // (tokio drops a scoped future inside its scope, also when the request itself is dropped.)
    let result = docker::ORPHANS
        .scope(
            admission.orphans.clone(),
            tokio::time::timeout(budget, dispatch(state.clone(), req)),
        )
        .await;
    admission.release().await;
    match result {
        Ok(r) => r,
        Err(_) => detail(
            StatusCode::GATEWAY_TIMEOUT,
            "model manager request timed out",
        ),
    }
}

/// One of the request permits (noevia#1385). The permit is taken out of the semaphore with
/// `forget` and given back only after every Docker driver the request left behind has been
/// joined: inline in [`Admission::release`], or, when the request future is dropped (the client
/// went away), by a task that owns those joins and the permit until they are observed.
struct Admission {
    state: Arc<StateData>,
    orphans: docker::Orphans,
    held: bool,
}

impl Admission {
    fn try_new(state: &Arc<StateData>) -> Option<Self> {
        state.permits.try_acquire().ok()?.forget();
        Some(Admission {
            state: state.clone(),
            orphans: Default::default(),
            held: true,
        })
    }

    async fn release(mut self) {
        docker::observe(&self.orphans).await;
        self.held = false;
        self.state.permits.add_permits(1);
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        if !self.held {
            return;
        }
        self.held = false;
        let (state, orphans) = (self.state.clone(), self.orphans.clone());
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    docker::observe(&orphans).await;
                    state.permits.add_permits(1);
                });
            }
            // No runtime left (shutdown): no task can still be running a driver.
            Err(_) => state.permits.add_permits(1),
        }
    }
}

#[cfg(feature = "native-diagnose")]
async fn native_diagnosis(
    state: Arc<StateData>,
    diagnosis: Arc<diagnose_docker::Diagnosis>,
    req: Request,
    route: diagnose_docker::Route,
    deadline: std::time::Instant,
) -> Response {
    fn head(mut response: Response, method: &Method) -> Response {
        if *method == Method::HEAD {
            *response.body_mut() = Body::empty();
        }
        response
    }
    match route {
        diagnose_docker::Route::NotFound | diagnose_docker::Route::NotOwned => {
            head(detail(StatusCode::NOT_FOUND, "Not Found"), req.method())
        }
        diagnose_docker::Route::Redirect(canonical) => {
            let host = req
                .headers()
                .get("host")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("localhost");
            let mut url = match reqwest::Url::parse(&format!("http://{host}")) {
                Ok(url) => url,
                Err(_) => return detail(StatusCode::BAD_REQUEST, "invalid request path"),
            };
            url.set_path(&canonical);
            url.set_query(req.uri().query());
            (
                StatusCode::TEMPORARY_REDIRECT,
                [(axum::http::header::LOCATION, url.as_str())],
                Body::empty(),
            )
                .into_response()
        }
        diagnose_docker::Route::Item(name) => {
            if req.method() != Method::GET {
                let mut response = detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
                response.headers_mut().insert(
                    axum::http::header::ALLOW,
                    axum::http::HeaderValue::from_static("GET"),
                );
                return head(response, req.method());
            }
            if diagnose_docker::identifier(&name).is_err() {
                return detail(StatusCode::BAD_REQUEST, "invalid backend name");
            }
            match diagnosis.request(state, name, deadline).await {
                diagnose_docker::Outcome::Json(bytes) => (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                diagnose_docker::Outcome::Unknown => {
                    detail(StatusCode::NOT_FOUND, "unknown backend")
                }
                diagnose_docker::Outcome::Busy => {
                    detail(StatusCode::SERVICE_UNAVAILABLE, "service is busy")
                }
                diagnose_docker::Outcome::Refused => detail(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Native backend diagnosis is unavailable",
                ),
            }
        }
    }
}

async fn dispatch(state: Arc<StateData>, req: Request) -> Response {
    let path = req.uri().path();
    #[cfg(feature = "native-prompts")]
    if let Some(prompts) = &state.prompts {
        let route = prompts::classify(path);
        match &route {
            prompts::Route::Invalid => {
                return detail(StatusCode::BAD_REQUEST, "invalid request path")
            }
            prompts::Route::Redirect(canonical) => {
                let host = req
                    .headers()
                    .get("host")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("localhost");
                let query = req
                    .uri()
                    .query()
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default();
                return (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(
                        axum::http::header::LOCATION,
                        format!("http://{host}{canonical}{query}"),
                    )],
                )
                    .into_response();
            }
            _ => {}
        }
        if matches!(route, prompts::Route::Collection | prompts::Route::Item(_)) {
            let item = match &route {
                prompts::Route::Item(id) => Some(id.as_str()),
                _ => None,
            };
            let operation = match (req.method(), item) {
                (&Method::GET, None) => prompts::Operation::List,
                (&Method::POST, None) => {
                    let json_content = match req.headers().get(axum::http::header::CONTENT_TYPE) {
                        Some(value) => match value.to_str() {
                            Ok(value) if value.len() <= 8192 => {
                                prompts::json_content_type(Some(value))
                            }
                            _ => return prompt_error(prompts::Error::Body),
                        },
                        None => true,
                    };
                    let bytes = match to_bytes(req.into_body(), REQUEST_CAP).await {
                        Ok(b) => b,
                        Err(_) => {
                            return detail(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                "request body exceeded limit",
                            )
                        }
                    };
                    if !json_content && !bytes.is_empty() {
                        return match prompts::non_json_validation(&bytes) {
                            Ok(error) => {
                                (StatusCode::UNPROCESSABLE_ENTITY, Json(error)).into_response()
                            }
                            Err(e) => prompt_error(e),
                        };
                    }
                    let decoded = match prompts::decode_body(&bytes) {
                        Ok(text) => text,
                        Err(e) => return prompt_error(e),
                    };
                    if let Some(error) =
                        prompts::validation_with_empty(decoded.as_bytes(), bytes.is_empty())
                    {
                        return (StatusCode::UNPROCESSABLE_ENTITY, Json(error)).into_response();
                    }
                    let (name, body) = match prompts::parse_fields_json(&decoded) {
                        Ok(v) => v,
                        Err(e) => return prompt_error(e),
                    };
                    prompts::Operation::Add(name, body)
                }
                (&Method::DELETE, Some(id)) => {
                    let id=match prompts::parse_id(id){Ok(id)=>id,Err(_)=>return (StatusCode::UNPROCESSABLE_ENTITY,Json(json!({"detail":[{"type":"int_parsing","loc":["path","pid"],"msg":"Input should be a valid integer, unable to parse string as an integer","input":id}]}))).into_response()};
                    prompts::Operation::Delete(id)
                }
                _ => return detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed"),
            };
            return match prompts.response(operation).await {
                Ok(bytes) => (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                Err(e) => prompt_error(e),
            };
        }
        if crate::decode_segment(path)
            .ok()
            .is_some_and(|path| path.starts_with("/api/v1/prompts/"))
        {
            return detail(StatusCode::NOT_FOUND, "Not Found");
        }
    }
    if req.method() == Method::GET {
        if let Some(sections) = &state.sections {
            if let Some(trimmed) = path.strip_suffix('/') {
                if trimmed == "/api/v1/sections"
                    || trimmed
                        .strip_prefix("/api/v1/sections/")
                        .is_some_and(|n| !n.is_empty() && !n.contains('/'))
                {
                    let host = req
                        .headers()
                        .get("host")
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("localhost");
                    let query = req
                        .uri()
                        .query()
                        .map(|q| format!("?{q}"))
                        .unwrap_or_default();
                    return (
                        StatusCode::TEMPORARY_REDIRECT,
                        [(
                            axum::http::header::LOCATION,
                            format!("http://{host}{trimmed}{query}"),
                        )],
                    )
                        .into_response();
                }
            }
            if path == "/api/v1/sections"
                || path
                    .strip_prefix("/api/v1/sections/")
                    .is_some_and(|name| !name.is_empty() && !name.contains('/'))
            {
                return native_sections(sections, path, req.uri().query().unwrap_or_default())
                    .await;
            }
        }
    }
    #[cfg(feature = "native-settings")]
    if native_settings_route(&state, req.method(), path) {
        if let Some(settings) = &state.settings {
            return match settings.response().await {
                settings::Outcome::Json(bytes) => (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                // Byte-identical to FastAPI's unhandled-exception answer.
                settings::Outcome::Failed => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/plain; charset=utf-8",
                    )],
                    "Internal Server Error",
                )
                    .into_response(),
            };
        }
    }
    #[cfg(feature = "native-model-updates")]
    if native_model_updates_route(&state, req.method(), path) {
        if let Some(updates) = &state.model_updates {
            return match updates.response().await {
                model_updates::Outcome::Json(bytes) => (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                model_updates::Outcome::Failed => internal_error(),
                model_updates::Outcome::Refused => {
                    detail(StatusCode::SERVICE_UNAVAILABLE, model_updates::REFUSAL)
                }
            };
        }
    }
    #[cfg(feature = "native-badges")]
    if native_badges_route(&state, req.method(), path) {
        if let Some(badges) = state.badges.clone() {
            return native_badges(&badges, req).await;
        }
    }
    #[cfg(feature = "native-download-targets")]
    if native_download_targets_route(&state, req.method(), path) {
        if let Some(targets) = &state.download_targets {
            return match targets.response().await {
                download_targets::Outcome::Json(bytes) => (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                download_targets::Outcome::Failed => internal_error(),
            };
        }
    }
    #[cfg(feature = "native-model-detail")]
    if native_model_detail_route(&state, req.method(), path) {
        if let Some(detail) = state.model_detail.clone() {
            return native_model_detail(&state, &detail, req.uri().query().unwrap_or_default())
                .await;
        }
    }
    if path == "/api/v1/health" {
        return if req.method() == Method::GET {
            Json(json!({"ok":true})).into_response()
        } else {
            detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")
        };
    }
    if path == "/api/v1/host" {
        if let Some(host) = &state.host {
            return if req.method() == Method::GET {
                Json(host.lock().await.response()).into_response()
            } else {
                detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")
            };
        }
    }
    if path == IDLE_UNLOAD_ROUTE {
        if let Some(reconciler) = state.idle_unload.clone() {
            return native_idle_unload(&reconciler, req).await;
        }
    }
    if let Some(service) = state.embedding.clone() {
        if path == "/api/v1/embedding-service" {
            return native_embedding(&service, req).await;
        }
        if path == "/api/v1/models/delete" && req.method() == Method::POST {
            return guarded_delete(state, service, req).await;
        }
    }
    let name = path
        .strip_prefix("/api/v1/backends/")
        .and_then(|p| p.strip_suffix("/logs"))
        .filter(|p| !p.is_empty() && !p.contains('/'));
    if let Some(name) = name {
        if req.method() != Method::GET {
            return detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
        }
        return native_logs(&state, name, req.uri().query().unwrap_or_default()).await;
    }
    forward(state, req).await
}

/// noevia#1318: the file part natively, then the live fields from the child.
#[cfg(feature = "native-model-detail")]
async fn native_model_detail(
    state: &StateData,
    detail: &model_detail::ModelDetail,
    query: &str,
) -> Response {
    let outcome = match detail.locate(query).await {
        model_detail::Step::Done(outcome) => outcome,
        model_detail::Step::Located(located) => {
            match model_detail_runtime(state, &model_detail::runtime_query(&located)).await {
                Ok(body) => model_detail::splice(&located, &body),
                Err(outcome) => outcome,
            }
        }
    };
    match outcome {
        model_detail::Outcome::Json(status, bytes) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        model_detail::Outcome::Failed => internal_error(),
        model_detail::Outcome::Refused => {
            detail_response(StatusCode::SERVICE_UNAVAILABLE, model_detail::REFUSAL)
        }
        model_detail::Outcome::Unavailable => {
            detail_response(StatusCode::BAD_GATEWAY, "Python-owned endpoint unavailable")
        }
    }
}

#[cfg(feature = "native-model-detail")]
fn detail_response(status: StatusCode, text: &'static str) -> Response {
    detail(status, text)
}

/// The child's `{"loaded": api._loaded_map(), "fit": services.vram_fit_chips(bytes)}`. Its own
/// unhandled error is Python's 500; anything else it cannot answer is the forwarded 502.
#[cfg(feature = "native-model-detail")]
async fn model_detail_runtime(
    state: &StateData,
    query: &str,
) -> Result<Vec<u8>, model_detail::Outcome> {
    let mut request = state.legacy.get(format!(
        "{}{}?{query}",
        state.legacy_base,
        model_detail::RUNTIME_PATH
    ));
    if !state.token.is_empty() {
        request = request.header("x-model-loader-token", &state.token);
    }
    let response = request
        .send()
        .await
        .map_err(|_| model_detail::Outcome::Unavailable)?;
    let status = response.status();
    let mut chunks = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| model_detail::Outcome::Unavailable)?;
        if bytes.len().saturating_add(chunk.len()) > model_detail::RUNTIME_CAP {
            return Err(model_detail::Outcome::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    match status {
        StatusCode::OK => Ok(bytes),
        StatusCode::INTERNAL_SERVER_ERROR => Err(model_detail::Outcome::Failed),
        _ => Err(model_detail::Outcome::Unavailable),
    }
}

async fn forward(state: Arc<StateData>, req: Request) -> Response {
    let path = req.uri().path();
    if !python_owned(req.method().as_str(), path) {
        if ["GET", "POST", "PUT", "DELETE"]
            .iter()
            .any(|method| python_owned(method, path))
        {
            return detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
        }
        return detail(StatusCode::NOT_FOUND, "Not Found");
    }
    let (parts, body) = req.into_parts();
    let preset_write = idle_unload::writes_models_ini(parts.method.as_str(), parts.uri.path());
    let bytes = match to_bytes(body, REQUEST_CAP).await {
        Ok(b) => b,
        Err(_) => return detail(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeded limit"),
    };
    let target = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let mut legacy = state
        .legacy
        .request(parts.method, format!("{}{target}", state.legacy_base))
        .body(bytes);
    if let Some(content_type) = parts.headers.get("content-type") {
        legacy = legacy.header("content-type", content_type);
    }
    if !state.token.is_empty() {
        legacy = legacy.header("x-model-loader-token", &state.token);
    }
    let response = match legacy.send().await {
        Ok(r) => r,
        Err(_) => return detail(StatusCode::BAD_GATEWAY, "Python-owned endpoint unavailable"),
    };
    let status = response.status();
    let mut chunks = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(_) => return detail(StatusCode::BAD_GATEWAY, "Python-owned response unavailable"),
        };
        if bytes.len().saturating_add(chunk.len()) > 16 * LOG_CAP {
            return detail(
                StatusCode::BAD_GATEWAY,
                "Python-owned response exceeded limit",
            );
        }
        if bytes.try_reserve(chunk.len()).is_err() {
            return detail(StatusCode::SERVICE_UNAVAILABLE, "allocation refused");
        }
        bytes.extend_from_slice(&chunk);
    }
    if preset_write && status.is_success() {
        if let Some(reconciler) = &state.idle_unload {
            bytes = after_preset_write(reconciler, bytes).await;
        }
    }
    (
        status,
        [("content-type", "application/json")],
        Body::from(bytes),
    )
        .into_response()
}

/// #1747: a models.ini save may have dropped the chat preset's global idle key (the raw editor
/// saves whatever text it was given). Put it back, and report the revision of the file as it now
/// is, so the editor's next save is not refused as a conflict with our own rewrite. Only the
/// models.ini save routes get here, and their `revision` is always the models.ini revision.
async fn after_preset_write(reconciler: &idle_unload::Reconciler, bytes: Vec<u8>) -> Vec<u8> {
    let Ok(Some(revision)) = reconciler.reconcile().await else {
        return bytes;
    };
    let Ok(serde_json::Value::Object(mut body)) = serde_json::from_slice(&bytes) else {
        return bytes;
    };
    if !matches!(body.get("revision"), Some(serde_json::Value::String(_))) {
        return bytes;
    }
    body.insert("revision".into(), serde_json::Value::String(revision));
    serde_json::to_vec(&body).unwrap_or(bytes)
}

/// noevia#1863: the chat-model idle unload setting. Native only: the Python child never owned it.
pub const IDLE_UNLOAD_ROUTE: &str = "/api/v1/idle-unload";

/// `GET` reports the effective timeout; `PUT {"seconds": n}` saves an admin choice (0 is
/// "Never", otherwise 300..=86400) and `PUT {"seconds": null}` clears it, back to the env seed.
/// The chat preset's `[*]` key is rewritten before the answer; the caller reloads the router.
async fn native_idle_unload(reconciler: &idle_unload::Reconciler, req: Request) -> Response {
    match *req.method() {
        Method::GET => Json(reconciler.view().json()).into_response(),
        Method::PUT => {
            if !reconciler.writable() {
                return detail(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the idle unload setting cannot be saved here",
                );
            }
            let Ok(raw) = to_bytes(req.into_body(), 4096).await else {
                return detail(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeded limit");
            };
            let Ok(serde_json::Value::Object(body)) = serde_json::from_slice(&raw) else {
                return detail(StatusCode::BAD_REQUEST, "invalid idle unload request");
            };
            let value = match body.get("seconds") {
                Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::Number(n)) => {
                    match n.as_u64().and_then(idle_unload::admin_value) {
                        Some(v) => Some(v),
                        None => {
                            return detail(
                                StatusCode::BAD_REQUEST,
                                "seconds must be 0 (never) or 300 to 86400",
                            )
                        }
                    }
                }
                _ => {
                    return detail(
                        StatusCode::BAD_REQUEST,
                        "seconds must be 0 (never), 300 to 86400, or null",
                    )
                }
            };
            match reconciler.update(value).await {
                Ok((view, preset)) => {
                    let mut out = view.json();
                    let (state, error) = match preset {
                        idle_unload::Preset::Written => ("written", None),
                        idle_unload::Preset::Unchanged => ("unchanged", None),
                        idle_unload::Preset::Failed(e) => ("failed", Some(e)),
                    };
                    if let serde_json::Value::Object(map) = &mut out {
                        map.insert("preset".into(), json!(state));
                        if let Some(e) = error {
                            map.insert("presetError".into(), json!(e));
                        }
                    }
                    Json(out).into_response()
                }
                Err(_) => detail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the idle unload setting could not be saved",
                ),
            }
        }
        _ => detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed"),
    }
}

async fn native_embedding(service: &Arc<embedding::EmbeddingService>, req: Request) -> Response {
    match *req.method() {
        Method::GET => Json(service.status().await).into_response(),
        Method::PUT => {
            let Ok(raw) = to_bytes(req.into_body(), 16 * 1024).await else {
                return detail(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeded limit");
            };
            let Ok(desired) = serde_json::from_slice::<embedding::Desired>(&raw) else {
                return detail(StatusCode::BAD_REQUEST, "invalid embedding service request");
            };
            match service.apply(desired).await {
                Ok(_) => (StatusCode::ACCEPTED, Json(service.status().await)).into_response(),
                Err(embedding::Refusal::Invalid(text)) => detail(StatusCode::BAD_REQUEST, text),
                Err(embedding::Refusal::Unavailable(text)) => {
                    detail(StatusCode::SERVICE_UNAVAILABLE, text)
                }
            }
        }
        _ => detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed"),
    }
}

/// #336/#1668: the embedding router holds its model through the preset, not its command line,
/// so the legacy holder scan cannot see it. Refuse here, before anything is forwarded.
async fn guarded_delete(
    state: Arc<StateData>,
    service: Arc<embedding::EmbeddingService>,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(raw) = to_bytes(body, REQUEST_CAP).await else {
        return detail(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeded limit");
    };
    let keys: Vec<String> = serde_json::from_slice::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("models").and_then(|m| m.as_array()).cloned())
        .unwrap_or_default()
        .iter()
        .map(|k| match k {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    // Held across the check and the forward, so a concurrent switch cannot choose a model whose
    // file this request is deleting.
    let _held = service.hold().await;
    for key in &keys {
        if service.protects(key).await {
            return detail(
                StatusCode::CONFLICT,
                "The embedding service is set to run this model or still has it loaded. Switch embedding to another model or turn embedding off first.",
            );
        }
    }
    forward(state, Request::from_parts(parts, Body::from(raw))).await
}

/// noevia#1318: `PUT` reads a FastAPI `dict` body exactly as the prompt writer does (same
/// content-type, encoding, 422 and size rules); `DELETE` reads its two required query values.
#[cfg(feature = "native-badges")]
async fn native_badges(badges: &badges::Badges, req: Request) -> Response {
    let outcome = if req.method() == Method::DELETE {
        badges.delete(req.uri().query().unwrap_or_default()).await
    } else {
        // A missing content type is JSON, as for the prompt writer.
        let header = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .map(|value| value.to_str().ok().filter(|v| v.len() <= 8192));
        let json_content = if let Some(value) = header {
            let Some(value) = value else {
                return prompt_error(prompts::Error::Body);
            };
            prompts::json_content_type(Some(value))
        } else {
            true
        };
        let Ok(bytes) = to_bytes(req.into_body(), REQUEST_CAP).await else {
            return detail(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeded limit");
        };
        if !json_content && !bytes.is_empty() {
            return match prompts::non_json_validation(&bytes) {
                Ok(error) => (StatusCode::UNPROCESSABLE_ENTITY, Json(error)).into_response(),
                Err(e) => prompt_error(e),
            };
        }
        let decoded = match prompts::decode_body(&bytes) {
            Ok(text) => text,
            Err(e) => return prompt_error(e),
        };
        if let Some(error) = prompts::validation_with_empty(decoded.as_bytes(), bytes.is_empty()) {
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(error)).into_response();
        }
        let body = match prompts::parse_json(&decoded) {
            Ok(value @ prompts::JsonValue::Obj(_)) => value,
            Ok(_) => return prompt_error(prompts::Error::Object),
            Err(e) => return prompt_error(e),
        };
        badges.put(&body).await
    };
    match outcome {
        badges::Outcome::Json(status, bytes) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        badges::Outcome::Failed => internal_error(),
    }
}

#[cfg(feature = "native-prompts")]
fn prompt_error(error: prompts::Error) -> Response {
    let status = match error {
        prompts::Error::Input
        | prompts::Error::Body
        | prompts::Error::Required
        | prompts::Error::Text => StatusCode::BAD_REQUEST,
        prompts::Error::Object => StatusCode::UNPROCESSABLE_ENTITY,
        prompts::Error::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(json!({"detail":error.to_string()}))).into_response()
}

async fn native_sections(sections: &sections::Sections, path: &str, query: &str) -> Response {
    let name = path.strip_prefix("/api/v1/sections/");
    let decoded = name.map(decode_segment);
    // FastAPI matches the decoded route before validating its query parameters.
    if decoded
        .as_ref()
        .is_some_and(|result| matches!(result, Ok(name) if name.contains('/')))
    {
        return detail(StatusCode::NOT_FOUND, "Not Found");
    }
    let parsed = reqwest::Url::parse(&format!("http://localhost/?{query}"));
    let mut defaults = false;
    if let Ok(url) = parsed {
        if let Some((_, value)) = url
            .query_pairs()
            .filter(|(key, _)| key == "defaults")
            .last()
        {
            let low = value.to_lowercase();
            defaults = match low.as_str() {
                "1"|"true"|"on"|"yes"|"y"|"t" => true,
                "0"|"false"|"off"|"no"|"n"|"f" => false,
                _ if name.is_some() => return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail":[{"type":"bool_parsing","loc":["query","defaults"],"msg":"Input should be a valid boolean, unable to interpret input","input":value}]}))).into_response(),
                _ => false,
            };
        }
    }
    let name = match decoded {
        Some(result) => match result {
            Ok(name) => Some(name),
            Err(_) => return detail(StatusCode::BAD_REQUEST, "invalid section name"),
        },
        None => None,
    };
    match sections.response(name, defaults).await {
        Ok(bytes) => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        Err(sections::Error::InvalidName) => {
            detail(StatusCode::BAD_REQUEST, "invalid section name")
        }
        Err(_) => detail(StatusCode::SERVICE_UNAVAILABLE, sections::REFUSAL),
    }
}
fn decode_segment(raw: &str) -> Result<String, ()> {
    let mut bytes = Vec::new();
    bytes.try_reserve(raw.len()).map_err(|_| ())?;
    let mut chars = raw.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let a = char::from(chars.next().ok_or(())?).to_digit(16).ok_or(())?;
            let b = char::from(chars.next().ok_or(())?).to_digit(16).ok_or(())?;
            bytes.push((a * 16 + b) as u8);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).map_err(|_| ())
}

async fn native_logs(state: &StateData, name: &str, query: &str) -> Response {
    if name.len() > 256 || !name.is_ascii() || name.contains('%') {
        return detail(StatusCode::BAD_REQUEST, "invalid backend name");
    }
    let params: std::collections::HashMap<_, _> =
        reqwest::Url::parse(&format!("http://localhost/?{query}"))
            .map(|u| {
                u.query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect()
            })
            .unwrap_or_default();
    let q = params.get("q").map(String::as_str).unwrap_or_default();
    let level = params.get("level").map(String::as_str).unwrap_or_default();
    let tail = match parse_tail(params.get("tail").map(String::as_str)) {
        Ok(t) => t,
        Err(_) => return detail(StatusCode::UNPROCESSABLE_ENTITY, "invalid tail"),
    };
    let inventory = match state.docker.containers().await {
        Ok(i) => i,
        Err("docker unreachable") => Vec::new(),
        Err(e) => return detail(StatusCode::BAD_GATEWAY, e),
    };
    let names = match effective_names(&state.configured, &inventory) {
        Ok(n) => n,
        Err(e) => return detail(StatusCode::BAD_GATEWAY, e),
    };
    if !names.iter().any(|n| n == name) {
        return detail(StatusCode::NOT_FOUND, "unknown backend");
    }
    match state
        .docker
        .logs(name, tail)
        .await
        .and_then(|raw| state.redactor.filter(&raw, q, level))
    {
        Ok(lines) => Json(json!({"ok":true,"lines":lines})).into_response(),
        Err(e) => Json(json!({"ok":false,"error":e,"lines":[]})).into_response(),
    }
}

pub fn parse_tail(value: Option<&str>) -> Result<usize, &'static str> {
    let Some(value) = value else {
        return Ok(400);
    };
    let value = value.trim();
    let (negative, digits) = if let Some(d) = value.strip_prefix('-') {
        (true, d)
    } else {
        (false, value.strip_prefix('+').unwrap_or(value))
    };
    if digits.is_empty() || digits.len() > 4096 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid tail");
    }
    let number = digits.bytes().fold(0usize, |n, b| {
        n.saturating_mul(10)
            .saturating_add(usize::from(b - b'0'))
            .min(2001)
    });
    Ok(if number == 0 {
        400
    } else if negative {
        20
    } else {
        number.clamp(20, 2000)
    })
}

/// Explicit Python-owned endpoint/method table. Native-owned paths never enter this table.
pub fn python_owned(method: &str, path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/v1/") else {
        return false;
    };
    let segments: Vec<_> = rest.split('/').collect();
    matches!(
        (method, segments.as_slice()),
        (
            "GET",
            ["overview"
                | "models"
                | "sections"
                | "backends"
                | "host"
                | "settings"
                | "search"
                | "downloads"
                | "download-targets"
                | "prompts"
                | "benchmark"]
        ) | ("GET", ["models", "detail" | "updates"])
            | ("GET", ["search", "repo"])
            | ("GET", ["benchmark", "progress"])
            | ("GET", ["sections", _, "draft-heads" | "autoconfig"])
            | ("GET", ["sections", _])
            | ("GET", ["backends", _, "diagnose"])
            | ("GET", ["benchmark", "runs", _])
            | ("POST", ["models", "delete" | "check-updates"])
            | ("POST", ["sections", _, "safe-defaults" | "rename"])
            | ("POST", ["sections", _, "draft-heads", "download"])
            | ("POST", ["backends", _, "restart" | "test"])
            | ("POST", ["downloads" | "prompts"])
            | ("POST", ["downloads", _, "cancel"])
            | ("POST", ["downloads", "clear"])
            | ("POST", ["benchmark", "start" | "sweep" | "cancel"])
            | ("PUT", ["sections", _])
            | ("PUT", ["models-ini" | "settings" | "badges"])
            | ("DELETE", ["sections" | "prompts", _])
            | ("DELETE", ["badges"])
    )
}
