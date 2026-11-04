//! Request handling. Python (agent/rust_forward.py) forwards a request here with its raw path,
//! query, headers and body; this side either serves it exactly as app.py would, or DECLINES it
//! (421 + `x-noevia-diary-decline: 1`) before looking at any credential, in which case Python
//! serves it itself. A decline never consumes a nonce, so an assertion is verified by exactly one
//! process.

use crate::assertion::{Signed, Verifier};
use crate::config::Config;
use crate::corpus::{self, BaseCorpus, Layout};
use crate::health::{self, Models};
use crate::managed::{self, Managed, Settings};
use crate::py;
use crate::workspace::{self, Reply};
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use subtle::ConstantTimeEq;

pub const DECLINE_HEADER: &str = "x-noevia-diary-decline";
pub const CONTROL_HEADER: &str = "x-noevia-diary-control";
const MAX_SIGNED_BODY: usize = 64 * 1024 * 1024;
const MAX_STORAGE_HEADER_LEN: usize = 4096;

pub struct App {
    pub auth_token: String,
    pub control_token: String,
    pub data_root: PathBuf,
    pub legacy_user_id: String,
    pub verifier: Option<Verifier>,
    pub corpus: BaseCorpus,
    pub models: Option<Models>,
    pub http: reqwest::Client,
    pub export: crate::config::WorkspaceExport,
}

impl App {
    pub fn new(cfg: Config, started_at: f64) -> Self {
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
            verifier: (!key.is_empty()).then(|| Verifier::new(key, started_at)),
            corpus: cfg.corpus,
            models,
            http: reqwest::Client::builder().build().unwrap_or_default(),
            export: cfg.workspace_export,
        }
    }
}

// ---------------------------------------------------------------- responses

fn json_response<T: Serialize>(status: u16, value: &T) -> Response<Body> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap_or_default()
}

fn detail(status: u16, text: &str) -> Response<Body> {
    json_response(status, &json!({ "detail": text }))
}

fn error(status: u16, text: &str) -> Response<Body> {
    json_response(status, &json!({ "error": text }))
}

/// Starlette's ServerErrorMiddleware answer to an unhandled exception.
fn internal_error() -> Response<Body> {
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
fn decoded_path(raw: &str) -> String {
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

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    Health,
    Months,
    Day,
    /// D4b: GET /api/files, POST /api/file, GET /api/storage-status, GET /api/workspace-export.
    Files,
    FileRead,
    StorageStatus,
    WorkspaceExport,
}

impl Route {
    fn d4b(self) -> bool {
        matches!(
            self,
            Route::Files | Route::FileRead | Route::StorageStatus | Route::WorkspaceExport
        )
    }
}

pub fn route(method: &Method, path: &str) -> Option<Route> {
    match (method.as_str(), path) {
        ("GET", "/api/health") => Some(Route::Health),
        ("GET", "/api/months") => Some(Route::Months),
        ("GET", "/api/day") => Some(Route::Day),
        ("GET", "/api/files") => Some(Route::Files),
        ("POST", "/api/file") => Some(Route::FileRead),
        ("GET", "/api/storage-status") => Some(Route::StorageStatus),
        ("GET", "/api/workspace-export") => Some(Route::WorkspaceExport),
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

enum Plan {
    Decline,
    /// Serve; `Some` when the request resolves to a ready managed tenant.
    Serve(Option<Tenant>),
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

fn plan(app: &App, r: Route, headers: &HeaderMap) -> Plan {
    let header_uid = header(headers, "x-cowork-user-id");
    match r {
        Route::Health => {
            if app.models.is_none() {
                return Plan::Decline;
            }
            // With a tenant, only the refusals `_tenant_state` makes before touching storage, whose
            // reply carries no tenant detail, are served here: an unasserted legacy fallback (401)
            // and an oversized storage header (413). The handler re-checks them.
            if health_tenant(app, headers).is_none()
                || (header_uid.is_empty() && app.verifier.is_some())
                || header(headers, "x-cowork-storage").len() > MAX_STORAGE_HEADER_LEN
            {
                Plan::Serve(None)
            } else {
                Plan::Decline
            }
        }
        Route::Months
        | Route::Day
        | Route::Files
        | Route::FileRead
        | Route::StorageStatus
        | Route::WorkspaceExport => {
            if r == Route::WorkspaceExport && export_settings(app, &Settings::new()).is_none() {
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
            let uid = header_uid.to_ascii_lowercase();
            let root = app.data_root.join("users").join(&uid);
            match managed::ready(&root, &uid) {
                // D4b: a tenant Python still owes recovery (dirty documents, index outbox) is its.
                Ok(Some(_))
                    if r.d4b() && !matches!(workspace::recovery_pending(&root), Ok(false)) =>
                {
                    Plan::Decline
                }
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

fn check_auth(app: &App, headers: &HeaderMap) -> Result<bool, ()> {
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

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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
        return control(&app, &parts.method, &path, &headers, body).await;
    }
    let Some(r) = route(&parts.method, &path) else {
        return decline();
    };
    let tenant = match plan(&app, r, &headers) {
        Plan::Decline => return decline(),
        Plan::Serve(t) => t,
    };
    // Read once: the assertion hashes it and POST /api/file parses it. None: over the signed-body
    // bound (Python's forwarder never sends such a body here).
    let body = read_body(body).await;

    // TenantAssertionMiddleware.
    let header_uid = header(&headers, "x-cowork-user-id");
    let mut asserted = false;
    if !header_uid.is_empty() {
        if let Some(verifier) = app.verifier.as_ref() {
            let ctype = header(&headers, "content-type");
            let media = ctype.split(';').next().unwrap_or("").trim().to_lowercase();
            let body_hash = if media.is_empty() || media == "application/json" {
                let Some(bytes) = body.as_ref() else {
                    return detail(413, "request body too large");
                };
                sha256_hex(bytes)
            } else {
                "stream".to_string()
            };
            let assertion = header(&headers, "x-cowork-tenant-assertion");
            let storage = header(&headers, "x-cowork-storage");
            let legacy = header(&headers, "x-cowork-legacy-owner");
            let blocked = header(&headers, "x-cowork-storage-blocked");
            let signed = Signed {
                user_id: &header_uid,
                assertion: &assertion,
                method: parts.method.as_str(),
                path: &raw_path,
                query: &query,
                body_hash: &body_hash,
                storage: &storage,
                legacy_owner: &legacy,
                blocked: &blocked,
            };
            if let Err(reason) = verifier.verify(&signed, now_secs()) {
                eprintln!("noevia-diary: tenant assertion rejected: {reason}");
                return detail(401, "invalid tenant assertion");
            }
            asserted = true;
        }
    }

    // ---- seam: everything below reads the tenant only from `header_uid` and `asserted`.

    // FastAPI validates `body: dict` (422) before the handler checks the token.
    let file_body = if r == Route::FileRead {
        let ctype = headers.get("content-type").map(|v| v.as_bytes());
        match body.as_deref().and_then(|b| workspace::file_body(ctype, b)) {
            Some(map) => Some(map),
            None => return decline(),
        }
    } else {
        None
    };

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
                // Tenant detail needs Python's per-process state (recovery and reindex on a state's
                // first request, sqlite-vec, the live embedding target): serve only the refusals
                // `_tenant_state` makes before touching storage (401 unasserted, 413), whose reply
                // carries no detail.
                match resolve_tenant(&app, &uid, asserted, &headers, None) {
                    Err(resp) if resp.status().is_client_error() => {}
                    _ => return decline(),
                }
            }
            let h = health::health(&app.http, models, !app.auth_token.is_empty()).await;
            json_response(200, &h)
        }
        Route::Files | Route::FileRead | Route::StorageStatus | Route::WorkspaceExport => {
            let tenant = match resolve_tenant(&app, &header_uid, asserted, &headers, tenant) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let app2 = app.clone();
            let task = tokio::task::spawn_blocking(move || {
                serve_workspace(&app2, r, &query, file_body.as_ref(), &tenant)
            });
            match task.await {
                Ok(Reply::Json(status, body)) => Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap_or_default(),
                Ok(Reply::Zip(zip)) => zip_response(zip),
                Ok(Reply::Decline) => decline(),
                Err(_) => internal_error(),
            }
        }
        Route::Months | Route::Day => {
            let tenant = match resolve_tenant(&app, &header_uid, asserted, &headers, tenant) {
                Ok(t) => t,
                Err(resp) => return *resp,
            };
            let app2 = app.clone();
            let task = tokio::task::spawn_blocking(move || serve_corpus(&app2, r, &query, &tenant));
            task.await.unwrap_or_else(|_| internal_error())
        }
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

fn serve_workspace(
    app: &App,
    r: Route,
    query: &[u8],
    file_body: Option<&serde_json::Map<String, Value>>,
    t: &Tenant,
) -> Reply {
    match r {
        Route::Files => {
            // FastAPI `path: str = ""`: the last value given.
            let path = py::query_get(query, "path").unwrap_or_default();
            workspace::files(&t.store, &path)
        }
        Route::FileRead => match file_body {
            Some(body) => workspace::file_read(&t.store, body),
            None => Reply::Decline,
        },
        // _request_storage: the descriptor when it decodes to an object, else {}.
        Route::StorageStatus => workspace::storage_status(&t.store, t.descriptor.as_ref()),
        Route::WorkspaceExport => match export_settings(app, &t.settings) {
            Some(settings) => workspace::export(&t.store, &settings),
            None => Reply::Decline,
        },
        _ => Reply::Decline,
    }
}

async fn read_body(body: Body) -> Option<Bytes> {
    let limited = http_body_util::Limited::new(body, MAX_SIGNED_BODY);
    limited.collect().await.ok().map(|c| c.to_bytes())
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
    if app.verifier.is_some() && !asserted {
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
            if let (true, Some(v)) = (secret_truthy, app.verifier.as_ref()) {
                let matches = match &secret {
                    Value::String(s) => {
                        v.secret_ref_matches(&header_uid.to_ascii_lowercase(), s, reference)
                    }
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

fn serve_corpus(_app: &App, r: Route, query: &[u8], t: &Tenant) -> Response<Body> {
    match r {
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

async fn control(
    app: &App,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: Body,
) -> Response<Body> {
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
        ("POST", "/internal/floor") => {
            let Some(bytes) = read_body(body).await else {
                return Response::builder()
                    .status(413)
                    .body(Body::empty())
                    .unwrap_or_default();
            };
            let Some(second) = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok())
            else {
                return Response::builder()
                    .status(400)
                    .body(Body::empty())
                    .unwrap_or_default();
            };
            if let Some(v) = app.verifier.as_ref() {
                v.raise_floor(second);
            }
            Response::new(Body::from("ok"))
        }
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
