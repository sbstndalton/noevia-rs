//! Request handling. Python (agent/rust_forward.py) forwards the request here with its raw path,
//! query, headers and body plus loopback-only headers: the per-process forward secret (sent to this
//! process on stdin), `x-noevia-diary-verify: 1`, and whether Python has already recovered the
//! tenant's state. Anything without the matching secret is refused with Python's own 401 bytes.
//!
//! Tenant assertion (D8 verifier move): this process is the ONLY verifier of every request Python
//! forwards with `x-noevia-diary-verify: 1`. It checks the assertion here (assertion::Verifier:
//! shape, restart guard, signature, clock window, bounded nonce cache) BEFORE anything else, with
//! TenantAssertionMiddleware's exact refusals (401 `invalid tenant assertion`, 413 `request body
//! too large` above 64 MiB). Every reply to a request it verified carries
//! `x-noevia-diary-verified: 1`: Python then records the nonce in its own cache and, for a decline,
//! serves the request without verifying it again. Python verifies everything else itself (a
//! request it does not forward, or one this side declined without that header or never answered).
//! A request forwarded without the verify header (an older forwarder) keeps the D4 rule: it is
//! served only when `x-noevia-diary-asserted: 1` says Python verified it.
//!
//! This side serves the request exactly as app.py would, or DECLINES it (421 +
//! `x-noevia-diary-decline: 1`).
//!
//! D5 writes (PUT /api/file, POST /api/directory, POST /api/entries/{append,edit}) are served only
//! for a request Python marks with `x-noevia-diary-writer: 1`: the tenant's writer is this
//! process (rust_forward.py, DIARY_RUST_WRITES). They run under the tenant's write lock
//! (write::Writers) and decline only before writing anything.

use crate::assertion::{self, Signed, Verifier};
use crate::config::Config;
use crate::corpus::{self, BaseCorpus, Layout};
use crate::health::{self, Models};
use crate::managed::{self, Managed, Settings};
use crate::py;
use crate::workspace::{self, Reply};
use crate::write::Writers;
use crate::writes;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, Response, StatusCode};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use subtle::ConstantTimeEq;

pub const DECLINE_HEADER: &str = "x-noevia-diary-decline";
/// rust_forward.BUSY, the body of app.py's `writer_busy` 503 (WriterBusy other than "restarting").
pub const BUSY: &[u8] = br#"{"detail":"Diary is saving another change. Try again in a moment."}"#;
pub const CONTROL_HEADER: &str = "x-noevia-diary-control";
/// Set by Python on every forwarded request: the per-process forward secret.
pub const FORWARD_HEADER: &str = "x-noevia-diary-forward";
/// "1" when Python's middleware verified the tenant assertion of this request.
pub const ASSERTED_HEADER: &str = "x-noevia-diary-asserted";
/// "1": this process verifies the tenant assertion of this request (Python did not).
pub const VERIFY_HEADER: &str = "x-noevia-diary-verify";
/// "1" on every reply to a request whose assertion this process verified and accepted.
pub const VERIFIED_HEADER: &str = "x-noevia-diary-verified";
/// "1" when Python holds a recovered managed state for this tenant (journal replayed, dirty
/// documents reindexed once), so a read here skips nothing Python would have done.
pub const RECOVERED_HEADER: &str = "x-noevia-diary-recovered";
/// "1" when this process is the tenant's writer: set by Python only on the D5 write routes of a
/// tenant whose writes all go here.
pub const WRITER_HEADER: &str = "x-noevia-diary-writer";
const MAX_STORAGE_HEADER_LEN: usize = 4096;
/// Python's bound on a buffered JSON body (app.py MAX_SIGNED_BODY); the forwarder sends nothing
/// larger.
pub const MAX_BODY: usize = 64 * 1024 * 1024;

pub struct App {
    pub auth_token: String,
    pub control_token: String,
    pub data_root: PathBuf,
    pub legacy_user_id: String,
    pub forward_secret: String,
    /// DIARY_TENANT_KEY: the secretRef check and the tenant assertion (`verifier`).
    pub tenant_key: String,
    /// The tenant assertion verifier and nonce cache for the requests forwarded with
    /// `x-noevia-diary-verify: 1`.
    pub verifier: Verifier,
    pub corpus: BaseCorpus,
    pub models: Option<Models>,
    pub http: reqwest::Client,
    pub export: crate::config::WorkspaceExport,
    /// DIARY_CORPUS_WRITE_IMPL in Python: entries are written here only when Python itself uses
    /// the same native transforms ("rust").
    pub corpus_write_impl: String,
    pub writers: Writers,
    /// Native replay state; retrieval indexing has a separate Python lifecycle.
    pub recovered: Mutex<HashMap<String, Arc<Mutex<crate::recovery::State>>>>,
    /// R9: the internal answer endpoint's config (None: 404) and the companion engine it calls.
    pub answer: Option<crate::answer::AnswerConfig>,
    pub llm_main: health::ClientConfig,
    /// R9: the configured external source paths (None: the route is Python's).
    pub external_sources: Option<Vec<String>>,
    /// R9 main mode (front.rs): how long a native write waits for the tenant's writer slot (the
    /// Python child may hold it as a lease) before answering 503 BUSY, as Python's forwarder did.
    /// None (Python as the main process): without limit, as before.
    pub write_wait: Option<std::time::Duration>,
}

impl App {
    pub fn new(cfg: Config) -> Self {
        let models = health::supported(&[&cfg.llm.main, &cfg.llm.aux]).then(|| Models {
            main: health::Client::new(cfg.llm.main.clone()),
            aux: health::Client::new(cfg.llm.aux.clone()),
            aux_prefers_main: cfg.llm.aux_prefers_main,
            configured: cfg.llm.configured.clone(),
        });
        let key = cfg.tenant_key.trim().to_string();
        App {
            auth_token: cfg.auth_token.trim().to_string(),
            control_token: cfg.control_token,
            data_root: PathBuf::from(cfg.data_root),
            legacy_user_id: cfg.legacy_user_id,
            forward_secret: cfg.forward_secret,
            verifier: Verifier::new(key.clone(), now()),
            tenant_key: key,
            corpus: cfg.corpus,
            models,
            http: reqwest::Client::builder().build().unwrap_or_default(),
            export: cfg.workspace_export,
            corpus_write_impl: cfg.corpus_write_impl,
            writers: Writers::default(),
            recovered: Mutex::new(HashMap::new()),
            answer: cfg.answer,
            llm_main: cfg.llm.main,
            external_sources: cfg.external_sources,
            write_wait: None,
        }
    }
}

/// Seconds since the Unix epoch (Python `time.time()`).
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------- responses

pub(crate) fn json_response<T: Serialize>(status: u16, value: &T) -> Response<Body> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap_or_default()
}

pub(crate) fn detail(status: u16, text: &str) -> Response<Body> {
    json_response(status, &json!({ "detail": text }))
}

fn error(status: u16, text: &str) -> Response<Body> {
    json_response(status, &json!({ "error": text }))
}

/// Starlette's ServerErrorMiddleware answer to an unhandled exception.
pub(crate) fn internal_error() -> Response<Body> {
    Response::builder()
        .status(500)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from("Internal Server Error"))
        .unwrap_or_default()
}

pub fn decline() -> Response<Body> {
    Response::builder()
        .status(StatusCode::MISDIRECTED_REQUEST)
        .header(DECLINE_HEADER, "1")
        .body(Body::empty())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- request view

/// The first value of a header, latin-1 decoded (Starlette `headers.get`), or "".
fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .map(|v| py::latin1(v.as_bytes()))
        .unwrap_or_default()
}

/// Starlette's decoded path: percent-decoding of the raw path, as UTF-8 (errors replaced).
pub(crate) fn decoded_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes.get(i).copied().unwrap_or(0);
        if b == b'%' {
            let h = bytes.get(i + 1).and_then(|c| (*c as char).to_digit(16));
            let l = bytes.get(i + 2).and_then(|c| (*c as char).to_digit(16));
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    Health,
    Months,
    Day,
    /// R9: GET /api/external-sources (the scan of the operator's external folders).
    ExternalSources,
    /// D4b: GET /api/files, GET /api/storage-status, GET /api/workspace-export.
    Files,
    StorageStatus,
    WorkspaceExport,
    /// R9 main mode: POST /api/file (read a file by the JSON body's path). Only the main process
    /// serves it (it needs the body); Python as the parent never forwards a read with a body.
    ReadFile,
    /// D5: PUT /api/file, POST /api/directory, POST /api/entries/append, POST /api/entries/edit.
    PutFile,
    Directory,
    Append,
    Edit,
}

impl Route {
    /// The routes whose 401 is FastAPI's HTTPException (`{"detail": ...}`).
    fn d4b(self) -> bool {
        matches!(
            self,
            Route::Files
                | Route::StorageStatus
                | Route::WorkspaceExport
                | Route::ReadFile
                | Route::PutFile
                | Route::Directory
        )
    }

    pub fn write(self) -> bool {
        matches!(
            self,
            Route::PutFile | Route::Directory | Route::Append | Route::Edit
        )
    }
}

pub fn route(method: &Method, path: &str) -> Option<Route> {
    match (method.as_str(), path) {
        ("GET", "/api/health") => Some(Route::Health),
        ("GET", "/api/months") => Some(Route::Months),
        ("GET", "/api/day") => Some(Route::Day),
        ("GET", "/api/external-sources") => Some(Route::ExternalSources),
        ("GET", "/api/files") => Some(Route::Files),
        ("GET", "/api/storage-status") => Some(Route::StorageStatus),
        ("GET", "/api/workspace-export") => Some(Route::WorkspaceExport),
        ("POST", "/api/file") => Some(Route::ReadFile),
        ("PUT", "/api/file") => Some(Route::PutFile),
        ("POST", "/api/directory") => Some(Route::Directory),
        ("POST", "/api/entries/append") => Some(Route::Append),
        ("POST", "/api/entries/edit") => Some(Route::Edit),
        _ => None,
    }
}

/// The X-Cowork-Storage descriptor, when it decodes strictly (unpadded base64url, a JSON
/// object). Err(()) means "Python's lenient decoder might read it differently": decline.
fn strict_descriptor(raw: &str) -> Result<Option<serde_json::Map<String, Value>>, ()> {
    if raw.is_empty() || raw.len() > MAX_STORAGE_HEADER_LEN {
        return Ok(None);
    }
    let mut bits: u32 = 0;
    let mut nbits = 0u32;
    let mut out = Vec::new();
    for c in raw.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err(()),
        };
        bits = (bits << 6) | u32::from(v);
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push(((bits >> nbits) & 0xff) as u8);
        }
    }
    if raw.len() % 4 == 1 {
        return Err(());
    }
    match serde_json::from_slice::<Value>(&out) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        Ok(_) => Ok(None),
        Err(_) => Err(()),
    }
}

/// A managed tenant this process can serve, resolved without credentials.
struct Tenant {
    store: Managed,
    layout: Layout,
    descriptor: Option<serde_json::Map<String, Value>>,
    settings: Settings,
}

/// The exact settings text Python's export would send its exporter, when this side reproduces
/// the export at all: WORKSPACE_EXPORT_IMPL=rust (the Python exporter stamps the current local
/// time into the ZIP) with the exporter binary installed (else Python answers 409).
fn export_settings(app: &App, tenant: &Settings) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    if app.export.implementation != "rust" {
        return None;
    }
    let meta = std::fs::metadata(&app.export.binary).ok()?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return None;
    }
    workspace::export_settings_text(&app.export.settings, tenant)
}

/// api_health's `has_tenant`: Some(effective user id) when Python would add tenant detail (journal,
/// retrieval stats) unless `_tenant_state` refuses the tenant.
fn health_tenant(app: &App, headers: &HeaderMap) -> Option<String> {
    let header_uid = header(headers, "x-cowork-user-id");
    let uid = if header_uid.is_empty() {
        app.legacy_user_id.clone()
    } else {
        header_uid
    };
    let has_tenant = is_uuid(&uid)
        && (!header(headers, "x-cowork-storage").is_empty()
            || (!app.legacy_user_id.is_empty() && uid == app.legacy_user_id));
    has_tenant.then_some(uid)
}

enum Plan {
    Decline,
    /// Serve; `Some` when the request resolves to a ready managed tenant.
    Serve(Option<Tenant>),
}

fn plan(app: &App, r: Route, headers: &HeaderMap, recovered: bool) -> Plan {
    let header_uid = header(headers, "x-cowork-user-id");
    match r {
        Route::Health => {
            if app.models.is_none() {
                return Plan::Decline;
            }
            // Tenant detail (journal, retrieval stats) stays with Python. Served here with a
            // tenant: only the refusals `_tenant_state` makes before touching storage, whose reply
            // carries no detail (an unasserted legacy fallback, an oversized storage header). The
            // handler re-checks them.
            if health_tenant(app, headers).is_none()
                || (header_uid.is_empty() && !app.tenant_key.is_empty())
                || header(headers, "x-cowork-storage").len() > MAX_STORAGE_HEADER_LEN
            {
                Plan::Serve(None)
            } else {
                Plan::Decline
            }
        }
        Route::Months
        | Route::Day
        | Route::ExternalSources
        | Route::Files
        | Route::StorageStatus
        | Route::WorkspaceExport
        | Route::ReadFile
        | Route::PutFile
        | Route::Directory
        | Route::Append
        | Route::Edit => {
            if r.write() && header_uid.is_empty() {
                // The legacy (header-less) tenant's writes stay with Python.
                return Plan::Decline;
            }
            if matches!(r, Route::Append | Route::Edit) && app.corpus_write_impl != "rust" {
                return Plan::Decline;
            }
            if r == Route::WorkspaceExport && export_settings(app, &Settings::new()).is_none() {
                return Plan::Decline;
            }
            if r == Route::ExternalSources && app.external_sources.is_none() {
                return Plan::Decline;
            }
            if header_uid.is_empty() {
                return if app.legacy_user_id.is_empty() {
                    Plan::Serve(None)
                } else {
                    Plan::Decline
                };
            }
            if !is_uuid(&header_uid) {
                return Plan::Serve(None);
            }
            let storage = header(headers, "x-cowork-storage");
            let Ok(descriptor) = strict_descriptor(&storage) else {
                return Plan::Decline;
            };
            if !recovered && !r.write() {
                // Python recovers a tenant's state on its first request (journal replay, dirty
                // reindex, outbox): let it serve until it has.
                return Plan::Decline;
            }
            let uid = header_uid.to_ascii_lowercase();
            let root = app.data_root.join("users").join(&uid);
            let ready = if r.write() {
                if app.writers.is_fenced(&uid) {
                    return Plan::Decline;
                }
                managed::ready_for_write(&root, &uid, false)
            } else {
                managed::ready(&root, &uid)
            };
            match ready {
                Ok(Some((_, ref settings)))
                    if r == Route::WorkspaceExport && export_settings(app, settings).is_none() =>
                {
                    Plan::Decline
                }
                Ok(Some((store, settings))) => match corpus::layout(&app.corpus, &settings) {
                    Some(layout) => Plan::Serve(Some(Tenant {
                        store,
                        layout,
                        descriptor,
                        settings,
                    })),
                    None => Plan::Decline,
                },
                _ => Plan::Decline,
            }
        }
    }
}

// ---------------------------------------------------------------- handling

pub(crate) fn check_auth(app: &App, headers: &HeaderMap) -> Result<bool, ()> {
    if app.auth_token.is_empty() {
        return Ok(true);
    }
    let auth = header(headers, "authorization");
    let supplied = if auth.to_lowercase().starts_with("bearer ") {
        py::strip(auth.get(7..).unwrap_or("")).to_string()
    } else {
        header(headers, "x-diary-token")
    };
    if supplied.is_empty() {
        return Ok(false);
    }
    // secrets.compare_digest refuses non-ASCII str with a TypeError (an unhandled 500).
    if !supplied.is_ascii() || !app.auth_token.is_ascii() {
        return Err(());
    }
    Ok(bool::from(
        supplied.as_bytes().ct_eq(app.auth_token.as_bytes()),
    ))
}

/// Python's 401 for an unverifiable request (TenantAssertionMiddleware).
fn unverified() -> Response<Body> {
    detail(401, "invalid tenant assertion")
}

fn flag(headers: &HeaderMap, name: &str) -> bool {
    headers.get(name).map(|v| v.as_bytes()) == Some(b"1")
}

pub async fn handle(app: Arc<App>, req: Request<Body>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let raw_path = parts.uri.path().to_string();
    let query: Vec<u8> = parts
        .uri
        .query()
        .map(|q| q.as_bytes().to_vec())
        .unwrap_or_default();
    let path = decoded_path(&raw_path);
    let headers = parts.headers;

    if path.starts_with("/internal/") {
        if parts.method == Method::POST && path == "/internal/fence" {
            return fence(app, &query, &headers).await;
        }
        if parts.method == Method::POST && path == "/internal/answer" {
            return crate::answer::internal(app, &query, &headers, body).await;
        }
        if parts.method == Method::POST && path == "/internal/reindex" {
            return crate::reindex::internal(app, &query, &headers, body).await;
        }
        if let Some(resp) =
            crate::lease::internal(&app, &parts.method, &path, &query, &headers).await
        {
            return resp;
        }
        return control(&app, &parts.method, &path, &headers);
    }
    // Only Python's forwarder knows the secret; everything else is refused before any work.
    let supplied = headers
        .get(FORWARD_HEADER)
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    if app.forward_secret.is_empty() || !bool::from(supplied.ct_eq(app.forward_secret.as_bytes())) {
        return unverified();
    }
    let verify = flag(&headers, VERIFY_HEADER);
    let r = route(&parts.method, &path);
    // The body is read once, bounded by Python's MAX_SIGNED_BODY: for a write, and for any request
    // this side verifies (its hash is signed).
    let bytes = if verify || r.is_some_and(Route::write) {
        match axum::body::to_bytes(body, MAX_BODY).await {
            Ok(b) => Some(b),
            // TenantAssertionMiddleware's 413; without verification, Python's to answer.
            Err(_) if verify => return detail(413, "request body too large"),
            Err(_) => return decline(),
        }
    } else {
        None
    };
    let verified = if verify {
        match verify_assertion(
            &app,
            &headers,
            parts.method.as_str(),
            &raw_path,
            &query,
            bytes.as_deref().unwrap_or_default(),
            now(),
        ) {
            Ok(v) => v,
            Err(reason) => {
                // Reason codes only (never the header or any request value).
                eprintln!("noevia-diary: tenant assertion rejected: {reason}");
                return unverified();
            }
        }
    } else {
        false
    };
    let asserted = if verify {
        verified
    } else {
        flag(&headers, ASSERTED_HEADER)
    };
    let backup = parts.method == Method::POST && path == "/api/storage-backup";
    let mut resp = serve(app, r, backup, query, headers, bytes, asserted).await;
    if verified {
        resp.headers_mut()
            .insert(VERIFIED_HEADER, axum::http::HeaderValue::from_static("1"));
    }
    resp
}

/// TenantAssertionMiddleware for a request forwarded with the verify header: Ok(true) when it
/// names a tenant and its assertion is valid and unseen, Ok(false) when nothing is to be verified
/// (no tenant header, or no tenant key: Python's middleware lets those through unverified), Err
/// with a log reason otherwise.
pub(crate) fn verify_assertion(
    app: &App,
    headers: &HeaderMap,
    method: &str,
    raw_path: &str,
    query: &[u8],
    body: &[u8],
    now: f64,
) -> Result<bool, &'static str> {
    let user_id = header(headers, "x-cowork-user-id");
    if user_id.is_empty() || app.tenant_key.is_empty() {
        return Ok(false);
    }
    let content_type = header(headers, "content-type");
    let body_hash = if assertion::body_is_hashed(&content_type) {
        assertion::sha256_hex(body)
    } else {
        assertion::STREAM.to_string()
    };
    app.verifier
        .verify(
            &Signed {
                user_id: &user_id,
                assertion: &header(headers, "x-cowork-tenant-assertion"),
                method,
                path: raw_path,
                query,
                body_hash: &body_hash,
                storage: &header(headers, "x-cowork-storage"),
                legacy_owner: &header(headers, "x-cowork-legacy-owner"),
                blocked: &header(headers, "x-cowork-storage-blocked"),
            },
            now,
        )
        .map(|()| true)
}

/// R9 S2a (front.rs): a native read route for the main process, with the request's own headers
/// (internal ones replaced by the parent's facts) and its verification verdict.
pub(crate) async fn serve_native(
    app: Arc<App>,
    r: Route,
    query: Vec<u8>,
    headers: HeaderMap,
    asserted: bool,
    body: Option<axum::body::Bytes>,
) -> Response<Body> {
    serve(app, Some(r), false, query, headers, body, asserted).await
}

/// R9 S3 (front.rs): a writer tenant's write (or the managed backup) for the main process, with
/// the writer header set by the parent and the whole body.
pub(crate) async fn serve_native_write(
    app: Arc<App>,
    r: Option<Route>,
    backup: bool,
    query: Vec<u8>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
    asserted: bool,
) -> Response<Body> {
    serve(app, r, backup, query, headers, Some(bytes), asserted).await
}

async fn serve(
    app: Arc<App>,
    r: Option<Route>,
    backup: bool,
    query: Vec<u8>,
    headers: HeaderMap,
    bytes: Option<axum::body::Bytes>,
    asserted: bool,
) -> Response<Body> {
    let recovered = flag(&headers, RECOVERED_HEADER);
    // R4 managed backup, served after the D8 assertion check (`asserted`) and inside `serve`, so
    // a verified request's reply carries the verified marker (Python commits its nonce and never
    // serves or verifies it again).
    if backup {
        if !flag(&headers, WRITER_HEADER) {
            return decline();
        }
        match check_auth(&app, &headers) {
            Ok(true) => {}
            Ok(false) => return detail(401, "unauthorized"),
            Err(()) => return internal_error(),
        }
        let uid = header(&headers, "x-cowork-user-id").to_ascii_lowercase();
        if !is_uuid(&uid) {
            return detail(400, "invalid tenant");
        }
        if !app.tenant_key.is_empty() && !asserted {
            return unverified();
        }
        let Ok(storage) = strict_descriptor(&header(&headers, "x-cowork-storage")) else {
            return decline();
        };
        if storage.as_ref().is_some_and(|s| {
            ["baseUrl", "username", "secret", "corpusRoot"]
                .iter()
                .any(|key| s.get(*key).is_some_and(|v| !v.is_string()))
        }) {
            return decline();
        }
        let app2 = app.clone();
        // Dropping the HTTP future detaches this task; its locks remain until all effects settle.
        return tokio::task::spawn_blocking(move || {
            match app2.writers.with_lock_as(&uid, None, app2.write_wait, |_| {
                crate::backup::run(
                    &app2.data_root.join("users").join(&uid),
                    &uid,
                    storage.as_ref(),
                )
            }) {
                Ok(resp) => {
                    if app2.write_wait.is_some() && resp.status() != StatusCode::MISDIRECTED_REQUEST
                    {
                        app2.writers.note_native_write(&uid);
                    }
                    resp
                }
                Err(crate::write::Refused::Busy) => reply(Reply::Busy),
                Err(crate::write::Refused::Fenced) => detail(410, "tenant deleted"),
            }
        })
        .await
        .unwrap_or_else(|_| internal_error());
    }
    let Some(r) = r else {
        return decline();
    };
    if r.write()
        && crate::backup::pending(
            &app.data_root
                .join("users")
                .join(header(&headers, "x-cowork-user-id").to_ascii_lowercase()),
        )
    {
        return crate::backup::uncertain();
    }
    if r.write() && !flag(&headers, WRITER_HEADER) {
        // This process is not the tenant's writer.
        return decline();
    }
    let tenant = match plan(&app, r, &headers, recovered) {
        Plan::Decline => return decline(),
        Plan::Serve(t) => t,
    };
    let header_uid = header(&headers, "x-cowork-user-id");
    // With a key set, Python forwards a request naming a tenant only after verifying it.
    if !header_uid.is_empty() && !app.tenant_key.is_empty() && !asserted {
        return unverified();
    }

    // FastAPI validates a write's body against the handler's parameter (its 422) before the
    // handler runs `check_auth` (its 401): a body it would refuse is Python's to answer, whatever
    // the token.
    let write = if r.write() {
        match write_body(r, &headers, bytes.as_deref().unwrap_or_default()) {
            Some(w) => Some(w),
            None => return decline(),
        }
    } else {
        None
    };
    // POST /api/file's `body: dict`, validated by FastAPI before the handler (422: Python's).
    let read_body = if r == Route::ReadFile {
        match json_body(&headers, bytes.as_deref().unwrap_or_default()) {
            Some(j) => Some(j),
            None => return decline(),
        }
    } else {
        None
    };

    // ---- seam: the routes below read the tenant only from `header_uid` and `asserted` (the
    // trusted forward headers above).
    match check_auth(&app, &headers) {
        Err(()) => return internal_error(),
        // app.py raises HTTPException(401) on the D4b routes: {"detail": ...}.
        Ok(false) if r.d4b() => return detail(401, "unauthorized"),
        Ok(false) => return error(401, "unauthorized"),
        Ok(true) => {}
    }

    match r {
        Route::Health => {
            let Some(models) = app.models.as_ref() else {
                return internal_error();
            };
            if let Some(uid) = health_tenant(&app, &headers) {
                // Tenant detail needs Python's per-process state (sqlite-vec, the live embedding
                // target, recovery): only the refusals that leave it out are answered here.
                match resolve_tenant(&app, &uid, asserted, &headers, None) {
                    Err(resp) if resp.status().is_client_error() => {}
                    _ => return decline(),
                }
            }
            let h = health::health(&app.http, models, !app.auth_token.is_empty()).await;
            json_response(200, &h)
        }
        Route::ReadFile => {
            let tenant = match resolve_tenant(&app, &header_uid, asserted, &headers, tenant) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let json = read_body.unwrap_or_default();
            let task =
                tokio::task::spawn_blocking(move || workspace::file_read(&tenant.store, &json));
            match task.await {
                Ok(r) => reply(r),
                Err(_) => internal_error(),
            }
        }
        Route::Files | Route::StorageStatus | Route::WorkspaceExport => {
            let tenant = match resolve_tenant(&app, &header_uid, asserted, &headers, tenant) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let app2 = app.clone();
            let task =
                tokio::task::spawn_blocking(move || serve_workspace(&app2, r, &query, &tenant));
            match task.await {
                Ok(r) => reply(r),
                Err(_) => internal_error(),
            }
        }
        Route::Months | Route::Day | Route::ExternalSources => {
            let tenant = match resolve_tenant(&app, &header_uid, asserted, &headers, tenant) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let app2 = app.clone();
            let task = tokio::task::spawn_blocking(move || serve_corpus(&app2, r, &query, &tenant));
            task.await.unwrap_or_else(|_| internal_error())
        }
        Route::PutFile | Route::Directory | Route::Append | Route::Edit => {
            serve_write(app, r, &header_uid, asserted, &headers, tenant, write).await
        }
    }
}

/// Reply → HTTP response.
pub(crate) fn reply(r: Reply) -> Response<Body> {
    match r {
        Reply::Json(status, body) => Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap_or_default(),
        Reply::Zip(zip) => zip_response(zip),
        Reply::Text(status, text) => Response::builder()
            .status(status)
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from(text))
            .unwrap_or_default(),
        Reply::Busy => Response::builder()
            .status(503)
            .header("retry-after", "1")
            .header("content-type", "application/json")
            .body(Body::from(BUSY))
            .unwrap_or_default(),
        Reply::Decline => decline(),
    }
}

/// The body of a forwarded write: a JSON object Python's FastAPI would read the same way, else
/// None (decline; Python answers 422 or reads it its own way).
fn json_body(headers: &HeaderMap, bytes: &[u8]) -> Option<serde_json::Map<String, Value>> {
    let content_type = headers.get("content-type").map(|v| v.as_bytes());
    workspace::file_body(content_type, bytes)
}

/// A write's body as FastAPI's handler parameter reads it: `body: dict` for the file, directory
/// and append routes, `EditEntryRequest` for edit. None: FastAPI would answer 422 (decline).
struct WriteBody {
    json: serde_json::Map<String, Value>,
    edit: Option<writes::EditReq>,
}

fn write_body(r: Route, headers: &HeaderMap, body: &[u8]) -> Option<WriteBody> {
    let json = json_body(headers, body)?;
    let edit = if matches!(r, Route::Edit) {
        Some(writes::edit_model(&json)?)
    } else {
        None
    };
    Some(WriteBody { json, edit })
}

/// The D5 write routes, in app.py's order: FastAPI reads the body first (422 → decline, in
/// `handle` before auth), then auth (already checked), then for the entry routes the body
/// checks, then `_tenant_state`, then the write under the tenant's writer lock.
async fn serve_write(
    app: Arc<App>,
    r: Route,
    header_uid: &str,
    asserted: bool,
    headers: &HeaderMap,
    tenant: Option<Tenant>,
    write: Option<WriteBody>,
) -> Response<Body> {
    let Some(WriteBody { json, edit }) = write else {
        return decline();
    };
    enum Op {
        File,
        Dir,
        Append(writes::AppendReq),
        Edit(writes::EditReq),
    }
    let op = match r {
        Route::PutFile => Op::File,
        Route::Directory => Op::Dir,
        Route::Append => match writes::append_validate(&json) {
            Ok(req) => Op::Append(req),
            Err(resp) => return reply(resp),
        },
        Route::Edit => {
            let Some(req) = edit else {
                return decline();
            };
            if let Err(resp) = writes::edit_validate(&req) {
                return reply(resp);
            }
            Op::Edit(req)
        }
        _ => return decline(),
    };
    let tenant = match resolve_tenant(&app, header_uid, asserted, headers, tenant) {
        Ok(t) => t,
        Err(resp) => return *resp,
    };
    let uid = header_uid.to_ascii_lowercase();
    let app2 = app.clone();
    let task = tokio::task::spawn_blocking(move || {
        let reply = app2.writers
            .with_lock_as(&uid, None, app2.write_wait, |held| {
                if crate::backup::pending(&app2.data_root.join("users").join(&uid)) {
                    return workspace::Reply::Json(503, b"{\"detail\":\"Diary backup settlement is uncertain. Saving and deletion remain paused.\"}".to_vec());
                }
                // Re-checked under the lock: an earlier write may have left work pending.
                let root = app2.data_root.join("users").join(&uid);
                match managed::ready_for_write(&root, &uid, false) {
                    Ok(Some(_)) => {}
                    _ => return Reply::Decline,
                }
                let Ok(journal) = crate::write::Journal::open(&root.join("managed-index.db"), held)
                else {
                    return Reply::Decline;
                };
                let recovery = app2
                    .recovered
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(uid.clone())
                    .or_default()
                    .clone();
                let ctx = writes::Ctx {
                    recovery: &recovery,
                    reader: &tenant.store,
                    store: crate::write::Store::new(tenant.store.db_path()),
                    journal,
                    layout: &tenant.layout,
                };
                if !crate::recovery::supported(&ctx) {
                    return Reply::Decline;
                }
                // The forwarder's writer token covers this blocking task until its effect
                // settles (including caller cancellation / lost child barriers).
                match crate::recovery::recover(&ctx) {
                    Ok(()) => {}
                    // A definite refusal inside a publication transaction: nothing of this
                    // request ran (Python's _recover_state raises WriterBusy: 503 BUSY).
                    Err(e) if e.is_settlement_refusal() => return Reply::Busy,
                    Err(_) => return writes::internal(),
                }
                match &op {
                    Op::File => writes::file_write(&ctx, &json),
                    Op::Dir => writes::directory_create(&ctx, &json),
                    Op::Append(req) => writes::append(&ctx, req),
                    Op::Edit(req) => writes::edit(&ctx, req),
                }
            })
            .unwrap_or_else(|refused| match refused {
                crate::write::Refused::Busy => Reply::Busy,
                crate::write::Refused::Fenced => Reply::Decline,
            });
        // R9 main mode: counted here, in the blocking write, so a caller that went away meanwhile
        // cannot hide it from the child (front.rs tells it on the tenant's next forward).
        if app2.write_wait.is_some() && !matches!(reply, Reply::Decline | Reply::Busy) {
            app2.writers.note_native_write(&uid);
        }
        reply
    });
    match task.await {
        Ok(r) => reply(r),
        Err(_) => internal_error(),
    }
}

/// POST /internal/fence?tenant=<uuid> (control token): Python's tenant deletion. Every later
/// write for the tenant declines, and this returns once the write in flight (if any) is done.
async fn fence(app: Arc<App>, query: &[u8], headers: &HeaderMap) -> Response<Body> {
    let supplied = headers
        .get(CONTROL_HEADER)
        .map(|v| v.as_bytes().to_vec())
        .unwrap_or_default();
    if app.control_token.is_empty()
        || !bool::from(supplied.as_slice().ct_eq(app.control_token.as_bytes()))
    {
        return Response::builder()
            .status(403)
            .body(Body::empty())
            .unwrap_or_default();
    }
    let uid = py::query_get(query, "tenant")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !is_uuid(&uid) {
        return Response::builder()
            .status(400)
            .body(Body::empty())
            .unwrap_or_default();
    }
    let app2 = app.clone();
    let lease = crate::lease::lease_of(headers);
    match tokio::task::spawn_blocking(move || app2.writers.fence_as(&uid, lease)).await {
        Ok(()) => {
            let uid = py::query_get(query, "tenant")
                .unwrap_or_default()
                .to_ascii_lowercase();
            if crate::backup::pending(&app.data_root.join("users").join(uid)) {
                crate::backup::uncertain()
            } else {
                Response::new(Body::from("ok"))
            }
        }
        Err(_) => internal_error(),
    }
}

/// Starlette's Response for api_workspace_export: its headers in Starlette's order (the given
/// ones, then content-length, then content-type).
fn zip_response(zip: Vec<u8>) -> Response<Body> {
    Response::builder()
        .status(200)
        .header(
            "content-disposition",
            "attachment; filename=\"noevia-workspace.zip\"",
        )
        .header("cache-control", "no-store")
        .header("x-content-type-options", "nosniff")
        .header("content-length", zip.len())
        .header("content-type", "application/zip")
        .body(Body::from(zip))
        .unwrap_or_default()
}

/// The D4b routes. A decline here (a stored value this side does not reproduce) is safe: the
/// request was verified once, by Python, which then serves it without verifying it again.
fn serve_workspace(app: &App, r: Route, query: &[u8], t: &Tenant) -> Reply {
    match r {
        Route::Files => {
            // FastAPI `path: str = ""`: the last value given.
            let path = py::query_get(query, "path").unwrap_or_default();
            workspace::files(&t.store, &path)
        }
        // _request_storage: the descriptor when it decodes to an object, else {}.
        Route::StorageStatus => workspace::storage_status(&t.store, t.descriptor.as_ref()),
        Route::WorkspaceExport => match export_settings(app, &t.settings) {
            Some(settings) => workspace::export(&t.store, &settings),
            None => Reply::Decline,
        },
        _ => Reply::Decline,
    }
}

/// `_tenant_state` for a request planned as Serve: the same checks in the same order. A request
/// that reaches a real tenant was planned with one (else it was declined).
fn resolve_tenant(
    app: &App,
    header_uid: &str,
    asserted: bool,
    headers: &HeaderMap,
    tenant: Option<Tenant>,
) -> Result<Tenant, Box<Response<Body>>> {
    if !is_uuid(header_uid) {
        return Err(Box::new(detail(400, "missing or invalid X-Cowork-User-ID")));
    }
    if !app.tenant_key.is_empty() && !asserted {
        return Err(Box::new(detail(401, "tenant assertion required")));
    }
    if header(headers, "x-cowork-storage").len() > MAX_STORAGE_HEADER_LEN {
        return Err(Box::new(detail(413, "X-Cowork-Storage header too large")));
    }
    let Some(tenant) = tenant else {
        return Err(Box::new(internal_error()));
    };
    if let Some(d) = tenant.descriptor.as_ref() {
        let remote = matches!(
            d.get("kind").and_then(Value::as_str),
            Some("nextcloud" | "webdav" | "s3")
        );
        if remote && d.contains_key("secretRef") {
            let ok_ref = match d.get("secretRef") {
                Some(Value::String(r)) => {
                    r.is_empty()
                        || (r.len() == 32
                            && r.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
                }
                _ => false,
            };
            if !ok_ref {
                return Err(Box::new(detail(400, "invalid storage descriptor")));
            }
            let reference = d.get("secretRef").and_then(Value::as_str).unwrap_or("");
            let secret = d.get("secret").cloned().unwrap_or(Value::Null);
            let secret_truthy = match &secret {
                Value::Null => false,
                Value::Bool(b) => *b,
                Value::Number(n) => n.as_f64() != Some(0.0),
                Value::String(s) => !s.is_empty(),
                Value::Array(a) => !a.is_empty(),
                Value::Object(o) => !o.is_empty(),
            };
            if secret_truthy && !app.tenant_key.is_empty() {
                let matches = match &secret {
                    Value::String(s) => tenant_assertion::secret_ref_matches(
                        &app.tenant_key,
                        &header_uid.to_ascii_lowercase(),
                        s,
                        reference,
                    ),
                    _ => false,
                };
                if !matches {
                    return Err(Box::new(detail(400, "invalid storage descriptor")));
                }
            }
        }
    }
    Ok(tenant)
}

/// Python `api_external_sources` after `_tenant_state` (which only resolves the tenant): each
/// configured source scanned read-only, in order, with Python's key order. An error Python would
/// not catch there (its 500) is declined so Python answers it.
fn external_sources(paths: &[String]) -> Response<Body> {
    if paths.is_empty() {
        return json_response(
            200,
            &ExternalSources {
                configured: false,
                sources: Vec::new(),
                total: 0,
            },
        );
    }
    let mut scans = Vec::new();
    for p in paths {
        let Ok(scan) = diary_core::external_sources::scan_source(p) else {
            return decline();
        };
        if !python_serializes(&scan) {
            return decline();
        }
        scans.push(scan);
    }
    let total = scans.iter().fold(0i64, |n, s| {
        n.saturating_add(s.get("total").and_then(Value::as_i64).unwrap_or(0))
    });
    json_response(
        200,
        &ExternalSources {
            configured: true,
            sources: scans.iter().map(Scan::of).collect(),
            total,
        },
    )
}

/// False where Python's reply is a 500 that this side would answer 200 (the crate's known
/// differences): a file name that is not UTF-8 (Python keeps a lone surrogate, which its JSON
/// encoder refuses; here it reads as U+FFFD) or a modification time Python cannot date.
fn python_serializes(scan: &Value) -> bool {
    scan.get("files")
        .and_then(Value::as_array)
        .is_none_or(|files| {
            files.iter().all(|f| {
                ["name", "rel_path"].iter().all(|k| {
                    !f.get(*k)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .contains('\u{FFFD}')
                }) && f.get("date_source").and_then(Value::as_str) != Some("unavailable")
            })
        })
}

static NULL: Value = Value::Null;

fn field<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).unwrap_or(&NULL)
}

#[derive(Serialize)]
struct ExternalSources<'a> {
    configured: bool,
    sources: Vec<Scan<'a>>,
    total: i64,
}

/// One source's scan in `scan_source`'s key order (serde_json maps here are sorted).
#[derive(Serialize)]
struct Scan<'a> {
    path: &'a Value,
    exists: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a Value>,
    files: Vec<ScannedFile<'a>>,
    total: &'a Value,
    truncated: &'a Value,
}

#[derive(Serialize)]
struct ScannedFile<'a> {
    name: &'a Value,
    rel_path: &'a Value,
    size: &'a Value,
    date: &'a Value,
    date_source: &'a Value,
}

impl<'a> Scan<'a> {
    fn of(v: &'a Value) -> Self {
        let files = v
            .get("files")
            .and_then(Value::as_array)
            .map(|files| {
                files
                    .iter()
                    .map(|f| ScannedFile {
                        name: field(f, "name"),
                        rel_path: field(f, "rel_path"),
                        size: field(f, "size"),
                        date: field(f, "date"),
                        date_source: field(f, "date_source"),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Scan {
            path: field(v, "path"),
            exists: field(v, "exists"),
            error: v.get("error"),
            files,
            total: field(v, "total"),
            truncated: field(v, "truncated"),
        }
    }
}

#[derive(Serialize)]
struct Months {
    months: Vec<corpus::MonthItem>,
}

#[derive(Serialize)]
struct MonthLog {
    month: String,
    log: String,
    standing: &'static str,
}

#[derive(Serialize)]
struct Today {
    day: String,
    today_log: String,
    standing: &'static str,
}

fn serve_corpus(app: &App, r: Route, query: &[u8], t: &Tenant) -> Response<Body> {
    match r {
        Route::ExternalSources => external_sources(app.external_sources.as_deref().unwrap_or(&[])),
        Route::Months => match corpus::list_months(&t.store, &t.layout) {
            Ok(months) => json_response(200, &Months { months }),
            Err(corpus::ListError::Template) => internal_error(),
        },
        Route::Day => {
            let month = py::query_get(query, "month").unwrap_or_default();
            if !month.is_empty() {
                let parsed = month
                    .split_once('-')
                    .and_then(|(y, m)| Some((py::digits(y, 4, 4)?, py::digits(m, 2, 2)?)));
                let Some((year, mon)) = parsed else {
                    return error(400, "month must be YYYY-MM");
                };
                if !(1..=12).contains(&mon) {
                    return error(400, "month out of range");
                }
                // Python's date() refuses year 0; the daily layout computes the month directory
                // before its try block (an unhandled 500), the monthly one degrades to "".
                if year == 0 {
                    if t.layout.daily {
                        return internal_error();
                    }
                    return json_response(
                        200,
                        &MonthLog {
                            month,
                            log: String::new(),
                            standing: "",
                        },
                    );
                }
                let log = corpus::read_month_text(&t.store, &t.layout, year as i32, mon);
                return json_response(
                    200,
                    &MonthLog {
                        month,
                        log,
                        standing: "",
                    },
                );
            }
            let day = chrono::Local::now().date_naive();
            let today_log = corpus::day_text(&t.store, &t.layout, day, 12000);
            json_response(
                200,
                &Today {
                    day: day.format("%Y-%m-%d").to_string(),
                    today_log,
                    standing: "",
                },
            )
        }
        _ => internal_error(),
    }
}

fn control(app: &App, method: &Method, path: &str, headers: &HeaderMap) -> Response<Body> {
    let supplied = headers
        .get(CONTROL_HEADER)
        .map(|v| v.as_bytes().to_vec())
        .unwrap_or_default();
    if app.control_token.is_empty()
        || !bool::from(supplied.as_slice().ct_eq(app.control_token.as_bytes()))
    {
        return Response::builder()
            .status(403)
            .body(Body::empty())
            .unwrap_or_default();
    }
    match (method.as_str(), path) {
        ("GET", "/internal/ready") => Response::new(Body::from("ok")),
        _ => Response::builder()
            .status(404)
            .body(Body::empty())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_path_matches_starlette() {
        assert_eq!(decoded_path("/api/%6Donths"), "/api/months");
        assert_eq!(decoded_path("/api/%zz"), "/api/%zz");
    }

    #[test]
    fn descriptor_strictness() {
        assert_eq!(strict_descriptor(""), Ok(None));
        // {"kind":"webdav"}
        let ok = strict_descriptor("eyJraW5kIjoid2ViZGF2In0");
        assert!(matches!(ok, Ok(Some(ref m)) if m.get("kind") == Some(&json!("webdav"))));
        assert_eq!(
            strict_descriptor("eyJraW5kIjoid2ViZGF2In0=").map(|_| ()),
            Err(())
        );
        assert_eq!(strict_descriptor("WzFd").map(|v| v.is_none()), Ok(true));
    }

    #[test]
    fn uuid_shape() {
        assert!(is_uuid("11111111-2222-4333-8444-555555555555"));
        assert!(is_uuid("ABCDEF11-2222-4333-8444-555555555555"));
        assert!(!is_uuid("11111111-2222-4333-8444-55555555555"));
    }
}
