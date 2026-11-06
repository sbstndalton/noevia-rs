//! Non-managed ("legacy") Diary corpora and their one way out: `POST /api/storage-import`, the
//! explicit copy-and-verify import into app-managed storage (app.py `api_storage_import`,
//! diary_migration.snapshot, ManagedCorpusBackend.activate).
//!
//! The three legacy backends are read here exactly as Python reads them for an import: the
//! WebDAV/Nextcloud client (webdav.py), the path-style S3 client (s3_storage.py) and a local folder
//! (local_storage.py), including an operator volume from DIARY_LOCAL_VOLUMES (dedicated_storage.py)
//! and the default `users/<uid>/corpus`. Each remote credential has its own back-off gate
//! (diary-storage-backoff), with Python's statuses for a refused login (424), a throttle (503) and
//! an upstream failure (502).
//!
//! Python stays the fallback: anything this side does not reproduce byte for byte is DECLINED
//! before any effect (an import only reads until its final activation), and Python serves it.
//! Python opts a request in only when it holds a recovered legacy state for the same storage key
//! with no pending journal work and no closed gate (`x-noevia-diary-legacy-ready: 1`); the
//! state build and recovery stay Python's.
//!
//! The import runs in one detached blocking task holding the tenant's writer and the same
//! `managed-diary.lock` flock Python's migration lock takes, until the activation has committed or
//! rolled back: dropping the HTTP future never releases them early. Python's LegacyWriteGuard
//! takes that flock and re-checks `meta.active` before every legacy write, so no old in-flight
//! write lands in the retired corpus after the activation.
//!
//! Never logged or echoed: credentials, remote URLs, remote bodies or headers.

use crate::server::{self, App};
use crate::{managed, py, workspace, write};
use axum::body::Body;
use axum::http::{HeaderMap, Response};
use diary_storage_backoff::{GateKind, StorageGate};
use reqwest::blocking::{Client, RequestBuilder, Response as RemoteResponse};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// "1" when Python holds a recovered legacy state for this request's storage key, with no
/// pending journal entry and no storage cool-down (app.py `_rust_legacy_ready`).
pub const LEGACY_READY_HEADER: &str = "x-noevia-diary-legacy-ready";

const MIB: usize = 1024 * 1024;
const FILE_LIMIT: usize = 64 * MIB;
const TOTAL_LIMIT: usize = 256 * MIB;
const MAX_FILES: usize = 5000;
const DEBOUNCE_S: f64 = 3.0;
/// Fixed Python texts (workspace_files.StorageLoginRejected, storage_backoff).
const LOGIN_MESSAGE: &str =
    "Storage login rejected. Check your storage credentials in Settings → Diary & storage.";
const UPSTREAM_MESSAGE: &str =
    "The storage server returned an error or could not be reached. Try again shortly.";
const VOLUME_UNAVAILABLE: &str =
    "Dedicated Diary storage is unavailable; restore its configured volume.";
const PROPFIND_BODY: &str = "<?xml version=\"1.0\"?><d:propfind xmlns:d=\"DAV:\"><d:prop><d:getetag/><d:getlastmodified/><d:resourcetype/></d:prop></d:propfind>";

// ------------------------------------------------------------------ configuration

/// What Python resolves from its base config for the legacy backends (rust_forward.server_config).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct LegacyConfig {
    /// DIARY_LOCAL_VOLUMES, raw.
    pub local_volumes: String,
    /// Base `corpus.root` and `corpus.webdav.remote_root` (CorpusStore.remote_root's fallbacks).
    pub corpus_root: String,
    pub remote_root: String,
    /// The base backend, used by the legacy owner without a storage descriptor.
    pub base: BaseBackend,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct BaseBackend {
    /// `corpus.backend`, lower-cased ("local", "webdav", "s3"; anything else declines).
    pub kind: String,
    /// `Path(corpus.local.root).expanduser().resolve()`.
    pub local_root: String,
    pub volume_identity: String,
    pub webdav_base_url: String,
    pub webdav_username: String,
    pub webdav_password: String,
    pub s3_endpoint_url: String,
    pub s3_bucket: String,
    pub s3_access_key: String,
    pub s3_secret_key: String,
    pub s3_session_token: String,
    pub s3_region: String,
    pub s3_prefix: String,
    pub webdav_timeout_s: Option<f64>,
    pub s3_timeout_s: Option<f64>,
}

// ------------------------------------------------------------------ failures

/// How a failed import is answered. `Decline` = Python serves the request (nothing was written).
#[derive(Debug, Clone, PartialEq)]
pub enum Fail {
    Decline,
    /// HTTPException(409, message).
    Conflict(String),
    /// StorageLoginRejected → 424.
    Login,
    /// StorageThrottled → 503 + Retry-After.
    Throttle(u64),
    /// StorageUpstreamError → 502.
    Upstream,
    /// StorageUnavailable → 503 {"detail": message}.
    Unavailable(&'static str),
    /// An unhandled exception: Starlette's plain-text 500.
    Internal,
}

fn conflict(text: &str) -> Fail {
    Fail::Conflict(text.to_string())
}

fn js(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn raw_json(status: u16, body: String, retry_after: Option<u64>) -> Response<Body> {
    let mut b = Response::builder()
        .status(status)
        .header("content-type", "application/json");
    if let Some(s) = retry_after {
        b = b.header("retry-after", s.to_string());
    }
    b.body(Body::from(body)).unwrap_or_default()
}

pub fn fail_response(f: &Fail) -> Response<Body> {
    match f {
        Fail::Decline => server::decline(),
        Fail::Conflict(m) => server::detail(409, m),
        Fail::Login => raw_json(
            424,
            format!(
                "{{\"detail\":{},\"code\":\"storageLoginRejected\"}}",
                js(LOGIN_MESSAGE)
            ),
            None,
        ),
        Fail::Throttle(s) => raw_json(
            503,
            format!(
                "{{\"detail\":{},\"code\":\"storageThrottled\",\"retryAfter\":{s}}}",
                js(&format!(
                    "Storage is busy and asked us to slow down. Try again in about {s} seconds."
                ))
            ),
            Some(*s),
        ),
        Fail::Upstream => raw_json(
            502,
            format!(
                "{{\"detail\":{},\"code\":\"storageUpstream\"}}",
                js(UPSTREAM_MESSAGE)
            ),
            None,
        ),
        Fail::Unavailable(m) => server::detail(503, m),
        Fail::Internal => server::internal_error(),
    }
}

// ------------------------------------------------------------------ backends

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

pub trait Source {
    fn list_dir(&self, dir: &str) -> Result<Vec<Row>, Fail>;
    fn get_bounded(&self, path: &str, limit: usize) -> Result<Option<Vec<u8>>, Fail>;
}

// ---- back-off gates, one per credential (storage_backoff.gate_for)

type Gate = Arc<Mutex<StorageGate>>;

fn mono() -> f64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    // Offset so a fresh process starts with an open gate whatever the gate's reset spacing.
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
        + 1000.0
}

/// `gate_for(*parts)`: sha256 of the NUL-joined parts. Secrets are only hashed.
pub fn gate_for(parts: &[&str]) -> Gate {
    static GATES: OnceLock<Mutex<HashMap<String, Gate>>> = OnceLock::new();
    let key = format!("{:x}", Sha256::digest(parts.join("\0").as_bytes()));
    let mut gates = GATES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !gates.contains_key(&key) && gates.len() >= 256 {
        let idle: Vec<String> = gates
            .iter()
            .filter(|(_, g)| {
                g.lock()
                    .ok()
                    .and_then(|g| g.state(mono()).ok())
                    .is_some_and(|s| s.kind.is_none())
            })
            .map(|(k, _)| k.clone())
            .take(128)
            .collect();
        for k in idle {
            gates.remove(&k);
        }
        if gates.len() >= 256 {
            let oldest = gates
                .iter()
                .min_by(|a, b| {
                    let rem = |g: &Gate| {
                        g.lock()
                            .ok()
                            .and_then(|g| g.state(mono()).ok())
                            .map_or(0.0, |s| s.remaining)
                    };
                    rem(a.1).total_cmp(&rem(b.1))
                })
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                gates.remove(&k);
            }
        }
    }
    gates.entry(key).or_default().clone()
}

/// One HTTP storage client: no redirects, no proxies, identity encoding, gated.
struct Remote {
    client: Client,
    gate: Gate,
    /// Decoded request paths that count as the account root (a 403 there closes the gate).
    roots: Vec<String>,
}

impl Remote {
    fn new(timeout_s: f64, gate: Gate, roots: Vec<String>) -> Result<Self, Fail> {
        let timeout = if timeout_s.is_finite() && timeout_s > 0.0 {
            Duration::from_secs_f64(timeout_s.min(600.0))
        } else {
            Duration::from_secs(60)
        };
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .map_err(|_| Fail::Decline)?;
        Ok(Remote {
            client,
            gate,
            roots,
        })
    }

    fn blocked(&self) -> Result<(), Fail> {
        let gate = self.gate.lock().map_err(|_| Fail::Decline)?;
        match gate.check(mono()).map_err(|_| Fail::Decline)? {
            None => Ok(()),
            Some(b) => Err(match b.kind {
                Some(GateKind::Login) => Fail::Login,
                Some(GateKind::Throttle) => Fail::Throttle(b.retry_after),
                None => Fail::Decline,
            }),
        }
    }

    /// GatedTransport.handle_request: refuse in a cool-down, else send and learn from the status.
    /// A transport failure is a storage failure (502).
    fn send(&self, request: RequestBuilder, decoded_path: &str) -> Result<RemoteResponse, Fail> {
        self.blocked()?;
        let response = request
            .header("accept-encoding", "identity")
            .send()
            .map_err(|_| Fail::Upstream)?;
        let root = self
            .roots
            .iter()
            .any(|r| r == decoded_path.trim_end_matches('/'));
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);
        self.gate
            .lock()
            .map_err(|_| Fail::Decline)?
            .record(
                mono(),
                write::now_epoch(),
                response.status().as_u16(),
                retry_after.as_deref(),
                root,
            )
            .map_err(|_| Fail::Decline)?;
        Ok(response)
    }
}

/// `ensure_not_redirect` (RuntimeError → 500) then `raise_for_status` (classify → 424/503/502).
fn status_fail(response: &RemoteResponse) -> Option<Fail> {
    let code = response.status().as_u16();
    if (300..400).contains(&code) {
        return Some(Fail::Internal);
    }
    if code < 400 {
        return None;
    }
    Some(match code {
        401 | 403 => Fail::Login,
        429 => {
            let header = response
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok());
            let seconds = diary_storage_backoff::parse_retry_after(
                header,
                diary_storage_backoff::DEFAULT_RETRY_AFTER_S,
                write::now_epoch(),
            )
            .seconds;
            Fail::Throttle(((seconds + 0.999).trunc() as u64).max(1))
        }
        _ => Fail::Upstream,
    })
}

/// A body read with Python's decoded-size budget. Python decodes Content-Encoding (httpx); a
/// compressed reply is declined rather than read differently.
fn read_bounded(response: RemoteResponse, limit: usize) -> Result<Vec<u8>, Option<Vec<u8>>> {
    if response
        .headers()
        .get("content-encoding")
        .is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Err(None);
    }
    let mut data = Vec::new();
    match response.take(limit as u64 + 1).read_to_end(&mut data) {
        Ok(_) => Ok(data),
        Err(_) => Err(None),
    }
}

fn get_with_limit(
    remote: &Remote,
    request: RequestBuilder,
    decoded_path: &str,
    limit: usize,
    redirect_first: bool,
) -> Result<Option<Vec<u8>>, Fail> {
    let response = remote.send(request, decoded_path)?;
    let code = response.status().as_u16();
    if redirect_first && (300..400).contains(&code) {
        return Err(Fail::Internal);
    }
    if code == 404 {
        return Ok(None);
    }
    if let Some(f) = status_fail(&response) {
        return Err(f);
    }
    // A failure while streaming the body is not tagged as storage's in Python (a JSON 500):
    // decline it (nothing was written yet).
    let data = read_bounded(response, limit).map_err(|_| Fail::Decline)?;
    if data.len() > limit {
        return Err(conflict("File exceeds its safety limit"));
    }
    Ok(Some(data))
}

/// Python `urllib.parse.unquote` for valid UTF-8 only (else None: decline).
fn unquote(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = *b.get(i)?;
        if c == b'%' {
            let h = b
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok());
            if let Some(v) = h.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// `urlparse(url).path` for the shapes this reads; None (decline) for `;params`.
fn url_path(href: &str) -> Option<String> {
    let mut s = href;
    if let Some(i) = s.find('#') {
        s = s.get(..i)?;
    }
    if let Some(i) = s.find('?') {
        s = s.get(..i)?;
    }
    if let Some(i) = s.find(':') {
        let scheme = s.get(..i)?;
        if !scheme.is_empty()
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            s = s.get(i + 1..)?;
        }
    }
    if let Some(rest) = s.strip_prefix("//") {
        s = rest.find('/').and_then(|i| rest.get(i..)).unwrap_or("");
    }
    if s.contains(';') {
        return None;
    }
    Some(s.to_string())
}

fn xml_safe(body: &str) -> bool {
    !(body.contains("<!DOCTYPE") || body.contains("<!ENTITY"))
}

/// ElementTree `.text` of an element: its first text child, if that comes first.
fn text_of<'a, 'i>(node: roxmltree::Node<'a, 'i>) -> Option<&'a str> {
    let first = node.first_child()?;
    if first.is_text() {
        first.text()
    } else {
        None
    }
}

fn child<'a, 'i>(
    node: roxmltree::Node<'a, 'i>,
    ns: Option<&str>,
    name: &str,
) -> Option<roxmltree::Node<'a, 'i>> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name && c.tag_name().namespace() == ns)
}

// ---- WebDAV

pub struct WebDav {
    remote: Remote,
    base_url: String,
    base_path: String,
    username: String,
    password: String,
}

impl WebDav {
    pub fn new(
        base_url: &str,
        username: &str,
        password: &str,
        timeout_s: f64,
    ) -> Result<Self, Fail> {
        let base_url = format!("{}/", base_url.trim_end_matches('/'));
        // httpx sends this URL as given; the url crate must not rewrite it.
        let parsed = reqwest::Url::parse(&base_url).map_err(|_| Fail::Decline)?;
        if parsed.as_str() != base_url
            || !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(Fail::Decline);
        }
        let base_path = format!(
            "{}/",
            unquote(parsed.path())
                .ok_or(Fail::Decline)?
                .trim_end_matches('/')
        );
        let gate = gate_for(&["webdav", &base_url, username, password]);
        let roots = vec![base_path.trim_end_matches('/').to_string()];
        Ok(WebDav {
            remote: Remote::new(timeout_s, gate, roots)?,
            base_url,
            base_path,
            username: username.to_string(),
            password: password.to_string(),
        })
    }

    fn encoded(remote_path: &str) -> String {
        remote_path
            .trim_start_matches('/')
            .split('/')
            .map(s3_sign::uri_encode)
            .collect::<Vec<_>>()
            .join("/")
    }

    fn request(&self, method: &str, remote_path: &str) -> Result<(RequestBuilder, String), Fail> {
        let encoded = Self::encoded(remote_path);
        let url = reqwest::Url::parse(&format!("{}{encoded}", self.base_url))
            .map_err(|_| Fail::Decline)?;
        let decoded = unquote(url.path()).ok_or(Fail::Decline)?;
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| Fail::Decline)?;
        let mut rb = self.remote.client.request(method, url);
        if !self.username.is_empty() {
            rb = rb.basic_auth(&self.username, Some(&self.password));
        }
        Ok((rb, decoded))
    }

    /// The Python list_dir result, from a 2xx PROPFIND body.
    pub fn parse_listing(&self, body: &str, remote_dir: &str) -> Result<Vec<Row>, Fail> {
        let invalid = || conflict("Invalid WebDAV directory response");
        if !xml_safe(body) {
            return Err(Fail::Decline);
        }
        let doc = roxmltree::Document::parse(body).map_err(|_| invalid())?;
        let root = doc.root_element();
        if root.tag_name().namespace() != Some("DAV:") || root.tag_name().name() != "multistatus" {
            return Err(invalid());
        }
        let dav = Some("DAV:");
        let want = remote_dir.trim_matches('/');
        let mut entries = Vec::new();
        for response in root.children().filter(|c| {
            c.is_element() && c.tag_name().namespace() == dav && c.tag_name().name() == "response"
        }) {
            let href = child(response, dav, "href").map(|h| text_of(h).unwrap_or(""));
            let href = match href {
                Some(h) if !h.is_empty() => h,
                _ => return Err(conflict("WebDAV directory response omitted a path")),
            };
            let href_path = unquote(&url_path(href).ok_or(Fail::Decline)?).ok_or(Fail::Decline)?;
            let Some(rest) = href_path.strip_prefix(&self.base_path) else {
                return Err(conflict("WebDAV directory response escaped its root"));
            };
            let path = rest.trim_end_matches('/').to_string();
            if path == want {
                continue;
            }
            let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
            if parent != want {
                return Err(conflict(
                    "WebDAV directory response contained an unrelated path",
                ));
            }
            let mut props = None;
            for propstat in response.children().filter(|c| {
                c.is_element()
                    && c.tag_name().namespace() == dav
                    && c.tag_name().name() == "propstat"
            }) {
                let status = child(propstat, dav, "status")
                    .and_then(text_of)
                    .unwrap_or("");
                if status.contains(" 200 ") {
                    props = child(propstat, dav, "prop");
                    break;
                }
            }
            let Some(props) = props else {
                return Err(conflict("WebDAV directory properties could not be read"));
            };
            let is_dir = child(props, dav, "resourcetype")
                .is_some_and(|r| child(r, dav, "collection").is_some());
            entries.push(Row {
                name: path.rsplit('/').next().unwrap_or("").to_string(),
                path,
                is_dir,
            });
        }
        Ok(entries)
    }
}

impl Source for WebDav {
    fn list_dir(&self, remote_dir: &str) -> Result<Vec<Row>, Fail> {
        let (rb, decoded) = self.request(
            "PROPFIND",
            &format!("{}/", remote_dir.trim_end_matches('/')),
        )?;
        let rb = rb
            .header("depth", "1")
            .header("content-type", "application/xml")
            .body(PROPFIND_BODY);
        let response = self.remote.send(rb, &decoded)?;
        if response.status().as_u16() == 404 {
            return Ok(Vec::new());
        }
        if let Some(f) = status_fail(&response) {
            return Err(f);
        }
        let data = read_bounded(response, FILE_LIMIT).map_err(|_| Fail::Decline)?;
        let body = String::from_utf8(data).map_err(|_| Fail::Decline)?;
        self.parse_listing(&body, remote_dir)
    }

    fn get_bounded(&self, path: &str, limit: usize) -> Result<Option<Vec<u8>>, Fail> {
        let (rb, decoded) = self.request("GET", path)?;
        get_with_limit(
            &self.remote,
            rb.header("accept", "*/*"),
            &decoded,
            limit,
            false,
        )
    }
}

// ---- S3 (path style)

pub struct S3 {
    remote: Remote,
    scheme: String,
    host: String,
    bucket: String,
    prefix: String,
    region: String,
    access_key: String,
    secret_key: String,
    session_token: String,
}

impl S3 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        endpoint_url: &str,
        bucket: &str,
        access_key: &str,
        secret_key: &str,
        session_token: &str,
        region: &str,
        prefix: &str,
        timeout_s: f64,
    ) -> Result<Self, Fail> {
        let with_scheme = if endpoint_url.contains("://") {
            endpoint_url.to_string()
        } else {
            format!("https://{endpoint_url}")
        };
        if !with_scheme.is_ascii() {
            return Err(Fail::Decline);
        }
        let parsed = reqwest::Url::parse(&with_scheme).map_err(|_| Fail::Decline)?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(Fail::Decline);
        }
        let scheme = parsed.scheme().to_string();
        if !matches!(scheme.as_str(), "http" | "https") {
            return Err(Fail::Decline);
        }
        let host_name = parsed.host_str().ok_or(Fail::Decline)?.to_ascii_lowercase();
        let host = match parsed.port() {
            Some(p) => format!("{host_name}:{p}"),
            None => host_name,
        };
        let base = format!("{scheme}://{host}");
        let gate = gate_for(&["s3", &base, access_key, secret_key, session_token]);
        Ok(S3 {
            remote: Remote::new(timeout_s, gate, Vec::new())?,
            scheme,
            host,
            bucket: bucket.trim_matches('/').to_string(),
            prefix: prefix.trim_matches('/').to_string(),
            region: if region.is_empty() {
                "us-east-1".into()
            } else {
                region.to_string()
            },
            access_key: access_key.to_string(),
            secret_key: secret_key.to_string(),
            session_token: session_token.to_string(),
        })
    }

    fn key(&self, remote_path: &str) -> String {
        let clean = remote_path.trim_start_matches('/');
        if self.prefix.is_empty() {
            clean.to_string()
        } else {
            format!("{}/{clean}", self.prefix)
        }
    }

    fn path(&self, key: &str) -> String {
        format!(
            "/{}",
            format!("{}/{key}", self.bucket)
                .split('/')
                .map(s3_sign::uri_encode)
                .collect::<Vec<_>>()
                .join("/")
        )
    }

    fn request(&self, key: &str, query: &[(&str, &str)]) -> Result<(RequestBuilder, String), Fail> {
        let path = self.path(key);
        let qs = query
            .iter()
            .map(|(k, v)| format!("{}={}", s3_sign::uri_encode(k), s3_sign::uri_encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!(
            "{}://{}{path}{}",
            self.scheme,
            self.host,
            if qs.is_empty() {
                String::new()
            } else {
                format!("?{qs}")
            }
        );
        let url = reqwest::Url::parse(&url).map_err(|_| Fail::Decline)?;
        let decoded = unquote(url.path()).ok_or(Fail::Decline)?;
        let mut rb = self.remote.client.get(url);
        if !self.secret_key.is_empty() {
            let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let signed = s3_sign::sign(&s3_sign::Request {
                method: "GET",
                host: &self.host,
                pathname: &path,
                query,
                payload: b"",
                access_key: &self.access_key,
                secret_key: self.secret_key.as_bytes(),
                region: &self.region,
                session_token: &self.session_token,
                amz_date: &amz_date,
            })
            .map_err(|_| Fail::Decline)?;
            for (name, value) in signed.headers {
                if name != "host" {
                    rb = rb.header(name, value);
                }
            }
        }
        Ok((rb, decoded))
    }

    fn page(&self, query: &[(&str, &str)]) -> Result<Option<String>, Fail> {
        let (rb, decoded) = self.request("", query)?;
        let response = self.remote.send(rb, &decoded)?;
        let code = response.status().as_u16();
        if (300..400).contains(&code) {
            return Err(Fail::Internal);
        }
        if code == 404 {
            return Ok(None);
        }
        if let Some(f) = status_fail(&response) {
            return Err(f);
        }
        let data = read_bounded(response, FILE_LIMIT).map_err(|_| Fail::Decline)?;
        Ok(Some(String::from_utf8(data).map_err(|_| Fail::Decline)?))
    }
}

/// One parsed ListObjectsV2 page: (namespace, children in order as (local name, element text,
/// first-child texts by local name)).
struct S3Page {
    ns: Option<String>,
    truncated: bool,
    token: Option<String>,
    dirs: Vec<String>,
    keys: Vec<String>,
}

fn parse_s3_page(body: &str) -> Result<S3Page, Fail> {
    if !xml_safe(body) {
        return Err(Fail::Decline);
    }
    // ElementTree.ParseError is not handled by the app: 500.
    let doc = roxmltree::Document::parse(body).map_err(|_| Fail::Internal)?;
    let root = doc.root_element();
    let ns = root.tag_name().namespace().map(str::to_string);
    let mut values: HashMap<&str, Option<&str>> = HashMap::new();
    for n in root.children().filter(|c| c.is_element()) {
        values.insert(n.tag_name().name(), text_of(n));
    }
    let truncated = values.get("IsTruncated").copied().flatten() == Some("true");
    let token = values
        .get("NextContinuationToken")
        .copied()
        .flatten()
        .map(str::to_string);
    let nsr = ns.as_deref();
    let mut dirs = Vec::new();
    let mut keys = Vec::new();
    for c in root
        .children()
        .filter(|c| c.is_element() && c.tag_name().namespace() == nsr)
    {
        match c.tag_name().name() {
            "CommonPrefixes" => {
                if let Some(p) = child(c, nsr, "Prefix") {
                    dirs.push(text_of(p).unwrap_or("").to_string());
                } else {
                    dirs.push(String::new());
                }
            }
            "Contents" => {
                keys.push(
                    child(c, nsr, "Key")
                        .map(|k| text_of(k).unwrap_or("").to_string())
                        .unwrap_or_default(),
                );
            }
            _ => {}
        }
    }
    Ok(S3Page {
        ns,
        truncated,
        token,
        dirs,
        keys,
    })
}

impl Source for S3 {
    fn list_dir(&self, remote_dir: &str) -> Result<Vec<Row>, Fail> {
        let directory = remote_dir.trim_matches('/');
        let full = if self.prefix.is_empty() {
            directory.to_string()
        } else {
            format!("{}/{directory}", self.prefix)
        };
        let list_prefix = if full.is_empty() {
            String::new()
        } else {
            format!("{full}/")
        };
        let base: Vec<(&str, &str)> = vec![
            ("list-type", "2"),
            ("prefix", &list_prefix),
            ("delimiter", "/"),
            ("max-keys", "1000"),
        ];
        let Some(body) = self.page(&base)? else {
            return Ok(Vec::new());
        };
        let first = parse_s3_page(&body)?;
        let ns = first.ns.clone();
        let (mut dirs, mut keys) = (first.dirs.clone(), first.keys.clone());
        let mut page = first;
        let mut tokens = HashSet::new();
        while page.truncated {
            let token = match page.token.clone() {
                Some(t) if !t.is_empty() && !tokens.contains(&t) => t,
                _ => return Err(Fail::Internal),
            };
            tokens.insert(token.clone());
            let mut q = base.clone();
            q.push(("continuation-token", &token));
            // A 404 here is raise_for_status'd (502); Python only special-cases the first page.
            let (rb, decoded) = self.request("", &q)?;
            let response = self.remote.send(rb, &decoded)?;
            if let Some(f) = status_fail(&response) {
                return Err(f);
            }
            let data = read_bounded(response, FILE_LIMIT).map_err(|_| Fail::Decline)?;
            let next = parse_s3_page(&String::from_utf8(data).map_err(|_| Fail::Decline)?)?;
            if next.ns != ns {
                return Err(Fail::Decline);
            }
            dirs.extend(next.dirs.iter().cloned());
            keys.extend(next.keys.iter().cloned());
            page = next;
        }
        let stripped = if self.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.prefix)
        };
        let rel_of = |full: &str| -> String {
            if !stripped.is_empty() {
                if let Some(r) = full.strip_prefix(&stripped) {
                    return r.to_string();
                }
            }
            full.to_string()
        };
        let mut entries = Vec::new();
        for d in dirs {
            let rel = rel_of(d.trim_end_matches('/'));
            if rel.is_empty() {
                continue;
            }
            entries.push(Row {
                name: rel.rsplit('/').next().unwrap_or("").to_string(),
                path: rel,
                is_dir: true,
            });
        }
        for k in keys {
            if k.is_empty() || k.ends_with('/') {
                continue;
            }
            let rel = rel_of(&k);
            let name = rel.rsplit('/').next().unwrap_or("").to_string();
            let parent = if rel.contains('/') {
                rel.get(..rel.len() - name.len() - 1).unwrap_or("")
            } else {
                ""
            };
            if parent != directory {
                continue;
            }
            entries.push(Row {
                name,
                path: rel,
                is_dir: false,
            });
        }
        // Python's stable sort by name.lower(); non-ASCII case folding is declined.
        if entries.iter().any(|e| !e.name.is_ascii()) {
            return Err(Fail::Decline);
        }
        entries.sort_by_key(|e| e.name.to_ascii_lowercase());
        Ok(entries)
    }

    fn get_bounded(&self, path: &str, limit: usize) -> Result<Option<Vec<u8>>, Fail> {
        let (rb, decoded) = self.request(&self.key(path), &[])?;
        get_with_limit(&self.remote, rb, &decoded, limit, true)
    }
}

// ---- local folder / operator volume

/// `os.path.realpath(p)` (non-strict) for an absolute path; Err(Decline) on a symlink loop.
pub fn realpath(p: &Path) -> Result<PathBuf, Fail> {
    let mut out = PathBuf::from("/");
    let mut pending: VecDeque<OsString> = VecDeque::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => pending.push_back(n.to_os_string()),
            Component::ParentDir => pending.push_back("..".into()),
            _ => {}
        }
    }
    let mut hops = 0;
    while let Some(c) = pending.pop_front() {
        if c == ".." {
            out.pop();
            continue;
        }
        let next = out.join(&c);
        match std::fs::symlink_metadata(&next) {
            Ok(m) if m.file_type().is_symlink() => {
                hops += 1;
                if hops > 40 {
                    return Err(Fail::Decline);
                }
                let target = std::fs::read_link(&next).map_err(|_| Fail::Decline)?;
                if target.is_absolute() {
                    out = PathBuf::from("/");
                }
                let mut parts = Vec::new();
                for t in target.components() {
                    match t {
                        Component::Normal(n) => parts.push(n.to_os_string()),
                        Component::ParentDir => parts.push("..".into()),
                        _ => {}
                    }
                }
                for part in parts.into_iter().rev() {
                    pending.push_front(part);
                }
            }
            _ => out = next,
        }
    }
    Ok(out)
}

fn absolute(p: &Path) -> Result<PathBuf, Fail> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir().map_err(|_| Fail::Decline)?.join(p))
    }
}

/// Python `repr(str)` for printable ASCII text (else None: decline).
pub fn py_repr(s: &str) -> Option<String> {
    if !s.is_ascii() {
        return None;
    }
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    Some(out)
}

/// dedicated_storage.check_volume.
pub fn check_volume(root: &Path, identity: &str) -> Result<(), Fail> {
    let marker = root.join(".noevia-diary-volume");
    let ok = root.is_dir()
        && !marker.is_symlink()
        && std::fs::read(&marker)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .is_some_and(|t| py::strip(&t) == identity);
    if ok {
        Ok(())
    } else {
        Err(Fail::Unavailable(VOLUME_UNAVAILABLE))
    }
}

pub struct Local {
    root: PathBuf,
    volume_identity: Option<String>,
}

impl Local {
    pub fn new(root: &Path, volume_identity: Option<&str>) -> Result<Self, Fail> {
        let root = realpath(&absolute(root)?)?;
        let volume_identity = volume_identity
            .filter(|v| !v.is_empty())
            .map(str::to_string);
        if let Some(id) = &volume_identity {
            check_volume(&root, id)?;
        } else if !root.is_dir() {
            // Python would create it while building the state; that state is not ours.
            return Err(Fail::Decline);
        }
        Ok(Local {
            root,
            volume_identity,
        })
    }

    fn inside(&self, p: &Path) -> bool {
        p == self.root || p.starts_with(&self.root)
    }

    fn path(&self, relative: &str) -> Result<PathBuf, Fail> {
        if let Some(id) = &self.volume_identity {
            check_volume(&self.root, id)?;
        }
        let candidate = realpath(&self.root.join(relative.trim_start_matches('/')))?;
        if !self.inside(&candidate) {
            let shown = py_repr(relative).ok_or(Fail::Decline)?;
            return Err(Fail::Conflict(format!(
                "corpus path escapes configured root: {shown}"
            )));
        }
        Ok(candidate)
    }
}

impl Source for Local {
    fn list_dir(&self, path: &str) -> Result<Vec<Row>, Fail> {
        let directory = self.path(path)?;
        match std::fs::metadata(&directory) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(Fail::Decline),
            Ok(m) if !m.is_dir() => return Err(Fail::Internal),
            Ok(_) => {}
        }
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&directory).map_err(|_| Fail::Internal)? {
            let entry = entry.map_err(|_| Fail::Internal)?;
            let name = entry.file_name().into_string().map_err(|_| Fail::Decline)?;
            if !name.is_ascii() {
                // str.lower() ordering of non-ASCII names is Python's to reproduce.
                return Err(Fail::Decline);
            }
            names.push(name);
        }
        names.sort_by_key(|n| n.to_ascii_lowercase());
        let mut rows = Vec::new();
        for name in names {
            let child = directory.join(&name);
            // Never follow a symlink out of the configured root.
            let resolved = realpath(&child)?;
            if !self.inside(&resolved) {
                continue;
            }
            let is_dir = std::fs::metadata(&child).is_ok_and(|m| m.is_dir());
            // child.stat(): a dangling link inside the root is an unhandled error in Python.
            std::fs::metadata(&child).map_err(|_| Fail::Internal)?;
            let parent = path.trim_matches('/');
            rows.push(Row {
                path: if parent.is_empty() {
                    name.clone()
                } else {
                    format!("{parent}/{name}")
                },
                name,
                is_dir,
            });
        }
        Ok(rows)
    }

    fn get_bounded(&self, path: &str, limit: usize) -> Result<Option<Vec<u8>>, Fail> {
        let target = self.path(path)?;
        let file = match std::fs::File::open(&target) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Fail::Internal),
        };
        if file.metadata().map_err(|_| Fail::Internal)?.is_dir() {
            return Err(Fail::Internal);
        }
        let mut data = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|_| Fail::Internal)?;
        if data.len() > limit {
            return Err(conflict("Import file exceeds its safety limit"));
        }
        Ok(Some(data))
    }
}

// ------------------------------------------------------------------ snapshot

pub struct Snapshot {
    pub files: BTreeMap<String, Vec<u8>>,
    pub directories: Vec<String>,
    pub fingerprint: String,
    pub total: usize,
}

/// `json.dumps(str)` with ensure_ascii=True.
pub fn py_ascii_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn sha_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// diary_migration.snapshot(backend, prefix).
pub fn snapshot(source: &dyn Source, prefix: &str) -> Result<Snapshot, Fail> {
    let prefix = prefix.trim_matches('/').to_string();
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut directories = vec![prefix.clone()];
    let mut seen: HashSet<String> = HashSet::new();
    let mut folders: Vec<String> = Vec::new();
    let mut total: usize = 0;
    let relative = |path: &str| -> String {
        if prefix.is_empty() {
            path.to_string()
        } else {
            path.get(prefix.len() + 1..).unwrap_or("").to_string()
        }
    };
    while let Some(directory) = directories.pop() {
        if seen.contains(&directory) || seen.len() > 5000 {
            return Err(conflict(
                "Import contains too many directories or a directory cycle",
            ));
        }
        seen.insert(directory.clone());
        let mut names: HashSet<String> = HashSet::new();
        for row in source.list_dir(&directory)? {
            let name = row.name.clone();
            if name.is_empty()
                || name.contains('/')
                || name == "."
                || name == ".."
                || name.contains('\\')
                || names.contains(&name)
            {
                return Err(conflict("Invalid import listing"));
            }
            names.insert(name.clone());
            if directory == prefix && name == "noevia-backups" {
                continue;
            }
            let path = if directory.is_empty() {
                name.clone()
            } else {
                format!("{directory}/{name}")
            };
            if row.path.trim_end_matches('/') != path {
                return Err(conflict("Import listing escaped its directory"));
            }
            if !managed::safe_key(&path) {
                return Err(conflict("Invalid corpus path"));
            }
            if row.is_dir {
                directories.push(path.clone());
                folders.push(relative(&path));
                continue;
            }
            let limit = FILE_LIMIT.min(TOTAL_LIMIT.saturating_sub(total));
            let Some(data) = source.get_bounded(&path, limit)? else {
                return Err(conflict("A source file changed during import"));
            };
            total += data.len();
            if data.len() > FILE_LIMIT || total > TOTAL_LIMIT || files.len() >= MAX_FILES {
                return Err(conflict(
                    "Import exceeds the 5,000 file / 256 MiB safety limit",
                ));
            }
            files.insert(relative(&path), data);
        }
    }
    if files.is_empty() {
        return Err(conflict(
            "No source files found. Verify the original connection and folder before importing.",
        ));
    }
    folders.sort();
    // json.dumps({'files': listing, 'directories': sorted(folders)}, sort_keys=True)
    let listing: Vec<String> = files
        .iter()
        .map(|(p, d)| {
            format!(
                "{{\"bytes\": {}, \"path\": {}, \"sha256\": \"{}\"}}",
                d.len(),
                py_ascii_str(p),
                sha_hex(d)
            )
        })
        .collect();
    let dirs: Vec<String> = folders.iter().map(|d| py_ascii_str(d)).collect();
    let canonical = format!(
        "{{\"directories\": [{}], \"files\": [{}]}}",
        dirs.join(", "),
        listing.join(", ")
    );
    Ok(Snapshot {
        fingerprint: sha_hex(canonical.as_bytes()),
        files,
        directories: folders,
        total,
    })
}

/// The preview report, as FastAPI renders the dict (compact, ensure_ascii=False).
pub fn report_json(s: &Snapshot) -> String {
    let files: Vec<String> = s
        .files
        .iter()
        .map(|(p, d)| {
            format!(
                "{{\"path\":{},\"bytes\":{},\"sha256\":\"{}\"}}",
                js(p),
                d.len(),
                sha_hex(d)
            )
        })
        .collect();
    let dirs: Vec<String> = s.directories.iter().map(|d| js(d)).collect();
    format!(
        "{{\"fingerprint\":\"{}\",\"files\":[{}],\"directories\":[{}],\"fileCount\":{},\"bytes\":{}}}",
        s.fingerprint,
        files.join(","),
        dirs.join(","),
        s.files.len(),
        s.total
    )
}

// ------------------------------------------------------------------ activation

/// `json.dumps(settings, sort_keys=True)` (ensure_ascii) for string/bool/int values; None when a
/// value would be rendered differently (decline).
pub fn settings_json(settings: &Map<String, Value>) -> Option<String> {
    let mut keys: Vec<&String> = settings.keys().collect();
    keys.sort();
    let mut items = Vec::new();
    for k in keys {
        let v = settings.get(k)?;
        let rendered = match v {
            Value::Null => continue,
            Value::Bool(true) => "true".to_string(),
            Value::Bool(false) => "false".to_string(),
            Value::String(s) => py_ascii_str(s),
            Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
            _ => return None,
        };
        items.push(format!("{}: {rendered}", py_ascii_str(k)));
    }
    Some(format!("{{{}}}", items.join(", ")))
}

fn meta(conn: &Connection, key: &str) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
        .optional()
}

/// ManagedCorpusBackend.activate, under the caller's migration lock. Ok(Err(fail)) refuses
/// before anything committed.
pub fn activate(
    root: &Path,
    uid: &str,
    files: &BTreeMap<String, Vec<u8>>,
    settings: &str,
    directories: &[String],
) -> Result<(), Fail> {
    let db = root.join("managed-diary.db");
    let mut conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| Fail::Internal)?;
    conn.busy_timeout(Duration::from_secs(30))
        .map_err(|_| Fail::Internal)?;
    conn.execute_batch("PRAGMA synchronous=FULL")
        .map_err(|_| Fail::Internal)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|_| Fail::Internal)?;
    let any_file: Option<i64> = tx
        .query_row("SELECT 1 FROM files LIMIT 1", [], |r| r.get(0))
        .optional()
        .map_err(|_| Fail::Internal)?;
    if meta(&tx, "active").map_err(|_| Fail::Internal)?.as_deref() == Some("1")
        || any_file.is_some()
    {
        return Err(conflict(
            "App diary already exists; automatic merge is refused",
        ));
    }
    let now = write::now_epoch();
    let (Some(now_s), Some(due_s)) = (write::py_float(now), write::py_float(now + DEBOUNCE_S))
    else {
        return Err(Fail::Internal);
    };
    for (path, data) in files {
        if !managed::safe_key(path) {
            return Err(conflict("Invalid corpus path"));
        }
        tx.execute(
            "INSERT INTO files VALUES (?,?,?,?)",
            rusqlite::params![path, data, sha_hex(data), now],
        )
        .map_err(|_| Fail::Internal)?;
    }
    for d in directories {
        if !managed::safe_key(d) {
            return Err(conflict("Invalid corpus path"));
        }
        tx.execute("INSERT INTO directories VALUES (?)", [d])
            .map_err(|_| Fail::Internal)?;
    }
    for (k, v) in [
        ("snapshot_time", now_s.as_str()),
        ("settings", settings),
        ("generation", "1"),
        ("due", due_s.as_str()),
        ("active", "1"),
    ] {
        tx.execute("INSERT OR REPLACE INTO meta VALUES (?,?)", [k, v])
            .map_err(|_| Fail::Internal)?;
    }
    // Python writes and fsyncs the marker (and the directory) inside the transaction, before the
    // commit: a crash in between leaves the explicit incomplete-activation condition.
    let mut marker =
        std::fs::File::create(root.join("managed-diary.active")).map_err(|_| Fail::Internal)?;
    marker
        .write_all(uid.as_bytes())
        .and_then(|_| marker.sync_all())
        .map_err(|_| Fail::Internal)?;
    std::fs::File::open(root)
        .and_then(|d| d.sync_all())
        .map_err(|_| Fail::Internal)?;
    tx.commit().map_err(|_| Fail::Internal)
}

// ------------------------------------------------------------------ resolution

/// The tenant's legacy source and corpus settings (app._build_tenant_state_locked).
pub struct Resolved {
    pub source: Box<dyn Source + Send>,
    pub remote_root: String,
    pub settings: Map<String, Value>,
    retry: Option<Box<dyn Fn() + Send>>,
}

fn s(map: &Map<String, Value>, key: &str) -> Result<String, Fail> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(v)) => Ok(v.clone()),
        Some(_) => Err(Fail::Decline),
    }
}

/// dedicated_storage.tenant_volume for `uid`: Some((root, prefix)); any invalid configuration is
/// Python's to answer (decline).
pub fn tenant_volume(raw: &str, uid: &str) -> Result<Option<(PathBuf, String)>, Fail> {
    let raw = if raw.is_empty() { "{}" } else { raw };
    let Ok(Value::Object(volumes)) = serde_json::from_str::<Value>(raw) else {
        return Err(Fail::Decline);
    };
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut found = None;
    for (tenant, volume) in &volumes {
        if !server::is_uuid(tenant) || tenant.to_ascii_lowercase() != *tenant {
            return Err(Fail::Decline);
        }
        let Value::Object(v) = volume else {
            return Err(Fail::Decline);
        };
        let (Some(Value::String(root)), Some(Value::String(prefix))) =
            (v.get("root"), v.get("prefix"))
        else {
            return Err(Fail::Decline);
        };
        match v.get("reader_uid") {
            None | Some(Value::Null) => {}
            Some(Value::Number(n)) if n.as_u64().is_some() => {}
            _ => return Err(Fail::Decline),
        }
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            return Err(Fail::Decline);
        }
        if !prefix.is_empty()
            && (prefix.starts_with('/')
                || prefix.contains('\\')
                || prefix
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == ".."))
        {
            return Err(Fail::Decline);
        }
        let resolved = realpath(&root)?;
        if roots
            .iter()
            .any(|o| resolved.starts_with(o) || o.starts_with(&resolved))
        {
            return Err(Fail::Decline);
        }
        roots.push(resolved);
        if tenant == uid {
            found = Some((root, prefix.clone()));
        }
    }
    if let Some((root, _)) = &found {
        // StorageUnavailable propagates as-is (503 with its own message).
        check_volume(root, uid)?;
    }
    Ok(found)
}

pub fn resolve(
    app: &App,
    cfg: &LegacyConfig,
    uid: &str,
    legacy_owner: bool,
    storage: Option<&Map<String, Value>>,
) -> Result<Resolved, Fail> {
    let mut settings = app.export.settings.clone();
    let mut root_override: Option<String> = None;
    let volume = tenant_volume(&cfg.local_volumes, uid)?;
    let kind = match storage {
        Some(st) => s(st, "kind")?,
        None => String::new(),
    };
    let empty = Map::new();
    let st = storage.unwrap_or(&empty);
    let mut retry: Option<Box<dyn Fn() + Send>> = None;
    let source: Box<dyn Source + Send> = if let Some((root, prefix)) = volume {
        root_override = Some(prefix);
        Box::new(Local::new(&root, Some(uid))?)
    } else if matches!(kind.as_str(), "nextcloud" | "webdav") {
        root_override = Some(s(st, "corpusRoot")?);
        let dav = WebDav::new(
            &s(st, "baseUrl")?,
            &s(st, "username")?,
            &s(st, "secret")?,
            cfg.base.webdav_timeout_s.unwrap_or(60.0),
        )?;
        let gate = dav.remote.gate.clone();
        retry = Some(Box::new(move || {
            if let Ok(mut g) = gate.lock() {
                let _ = g.reset_login(mono());
            }
        }));
        Box::new(dav)
    } else if kind == "s3" {
        root_override = Some(String::new());
        settings.insert("monthly_prefix".into(), Value::String(String::new()));
        let region = s(st, "region")?.trim().to_lowercase();
        let region = if !region.is_empty()
            && region.len() <= 32
            && region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            region
        } else {
            "us-east-1".to_string()
        };
        let s3 = S3::new(
            &s(st, "baseUrl")?,
            &s(st, "bucket")?,
            &s(st, "username")?,
            &s(st, "secret")?,
            "",
            &region,
            &s(st, "corpusRoot")?,
            cfg.base.s3_timeout_s.unwrap_or(60.0),
        )?;
        let gate = s3.remote.gate.clone();
        retry = Some(Box::new(move || {
            if let Ok(mut g) = gate.lock() {
                let _ = g.reset_login(mono());
            }
        }));
        Box::new(s3)
    } else if !legacy_owner {
        root_override = Some(String::new());
        Box::new(Local::new(
            &app.data_root.join("users").join(uid).join("corpus"),
            None,
        )?)
    } else {
        let b = &cfg.base;
        match b.kind.as_str() {
            "local" => Box::new(Local::new(
                Path::new(&b.local_root),
                Some(b.volume_identity.as_str()),
            )?),
            "webdav" => Box::new(WebDav::new(
                &b.webdav_base_url,
                &b.webdav_username,
                &b.webdav_password,
                b.webdav_timeout_s.unwrap_or(60.0),
            )?),
            "s3" => Box::new(S3::new(
                &b.s3_endpoint_url,
                &b.s3_bucket,
                &b.s3_access_key,
                &b.s3_secret_key,
                &b.s3_session_token,
                &b.s3_region,
                &b.s3_prefix,
                b.s3_timeout_s.unwrap_or(60.0),
            )?),
            _ => return Err(Fail::Decline),
        }
    };
    let root = root_override.unwrap_or_else(|| cfg.corpus_root.clone());
    let root = if root.is_empty() {
        cfg.remote_root.clone()
    } else {
        root
    };
    Ok(Resolved {
        source,
        remote_root: root.trim_matches('/').to_string(),
        settings,
        retry,
    })
}

// ------------------------------------------------------------------ the route

fn flag(headers: &HeaderMap, name: &str) -> bool {
    headers.get(name).map(|v| v.as_bytes()) == Some(b"1")
}

/// The managed store exists, carries this tenant's identity, and has not been activated (no
/// marker): the state Python's ready callback saw.
fn managed_inactive(root: &Path, uid: &str) -> bool {
    if root.join("managed-diary.active").exists() {
        return false;
    }
    let Ok(conn) = managed::open_ro(&root.join("managed-diary.db")) else {
        return false;
    };
    let tables: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('files','directories','meta')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    tables == 3 && meta(&conn, "tenant").ok().flatten().as_deref() == Some(uid)
}

/// The import's work, under the writer and the migration flock.
pub fn perform(
    root: &Path,
    uid: &str,
    resolved: &Resolved,
    fingerprint: Option<&str>,
) -> Result<String, Fail> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("managed-diary.lock"))
        .map_err(|_| Fail::Internal)?;
    lock.lock().map_err(|_| Fail::Internal)?;
    let conn = managed::open_ro(&root.join("managed-diary.db")).map_err(|_| Fail::Decline)?;
    if meta(&conn, "active").map_err(|_| Fail::Decline)?.as_deref() == Some("1") {
        return Err(conflict(
            "The app diary is already active. Reload to see it.",
        ));
    }
    drop(conn);
    let settings = settings_json(&resolved.settings).ok_or(Fail::Decline)?;
    let first = snapshot(resolved.source.as_ref(), &resolved.remote_root)?;
    let Some(fingerprint) = fingerprint else {
        return Ok(report_json(&first));
    };
    if fingerprint != first.fingerprint {
        return Err(conflict(
            "Source files changed. Review a fresh import preview.",
        ));
    }
    let second = snapshot(resolved.source.as_ref(), &resolved.remote_root)?;
    if second.fingerprint != first.fingerprint {
        return Err(conflict(
            "Source files changed during verification. Try a fresh preview.",
        ));
    }
    activate(root, uid, &second.files, &settings, &second.directories)?;
    // The flock (`lock`) is released only here, after commit or rollback.
    drop(lock);
    Ok(format!(
        "{{\"imported\":true,\"fileCount\":{}}}",
        first.files.len()
    ))
}

/// POST /api/storage-import for a non-managed tenant.
pub async fn import(
    app: Arc<App>,
    headers: HeaderMap,
    asserted: bool,
    body: Body,
) -> Response<Body> {
    if !flag(&headers, server::WRITER_HEADER) || !flag(&headers, LEGACY_READY_HEADER) {
        return server::decline();
    }
    let header_uid = server::header(&headers, "x-cowork-user-id");
    let uid = if header_uid.is_empty() {
        // The header-less legacy tenant: its unasserted request is Python's 401 when a key is set.
        if !app.tenant_key.is_empty() {
            return server::decline();
        }
        app.legacy_user_id.clone()
    } else {
        header_uid.clone()
    };
    if !server::is_uuid(&uid) {
        return server::decline();
    }
    if !header_uid.is_empty() && !app.tenant_key.is_empty() && !asserted {
        return server::unverified();
    }
    match server::check_auth(&app, &headers) {
        Err(()) => return server::internal_error(),
        Ok(false) => return server::detail(401, "unauthorized"),
        Ok(true) => {}
    }
    let Ok(bytes) = axum::body::to_bytes(body, server::MAX_BODY).await else {
        return server::decline();
    };
    let content_type = headers.get("content-type").map(|v| v.as_bytes());
    let Some(request) = workspace::file_body(content_type, &bytes) else {
        return server::decline();
    };
    let fingerprint = match request.get("fingerprint") {
        None => None,
        Some(Value::String(f))
            if f.len() == 64
                && f.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
        {
            Some(f.clone())
        }
        Some(_) => return server::detail(400, "Invalid import request"),
    };
    let uid = uid.to_ascii_lowercase();
    if flag(&headers, "x-cowork-storage-blocked") {
        return server::decline();
    }
    let Ok(storage) = server::strict_descriptor(&server::header(&headers, "x-cowork-storage"))
    else {
        return server::decline();
    };
    if let Some(st) = &storage {
        let kind = st.get("kind").and_then(Value::as_str).unwrap_or("");
        if matches!(kind, "nextcloud" | "webdav" | "s3")
            && st.contains_key("secretRef")
            && st
                .get("secret")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
        {
            // The secret lives only in Python's cached state.
            return server::decline();
        }
    }
    let legacy_owner = flag(&headers, "x-cowork-legacy-owner") || uid == app.legacy_user_id;
    let retry = flag(&headers, "x-cowork-storage-retry");
    let app2 = app.clone();
    // Dropping the HTTP future detaches this task; the writer and flock stay held until the
    // activation has committed or rolled back.
    tokio::task::spawn_blocking(move || {
        let root = app2.data_root.join("users").join(&uid);
        if !managed_inactive(&root, &uid) {
            return server::decline();
        }
        let resolved = match resolve(&app2, &app2.legacy, &uid, legacy_owner, storage.as_ref()) {
            Ok(r) => r,
            Err(f) => return fail_response(&f),
        };
        if retry {
            if let Some(reset) = &resolved.retry {
                reset();
            }
        }
        app2.writers
            .with_lock(&uid, || {
                match perform(&root, &uid, &resolved, fingerprint.as_deref()) {
                    Ok(body) => raw_json(200, body, None),
                    Err(f) => fail_response(&f),
                }
            })
            .unwrap_or_else(|| server::detail(410, "tenant deleted"))
    })
    .await
    .unwrap_or_else(|_| server::internal_error())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn python_json_and_repr() {
        assert_eq!(
            py_ascii_str("é😀\"\\\n"),
            "\"\\u00e9\\ud83d\\ude00\\\"\\\\\\n\""
        );
        assert_eq!(py_repr("a'b").as_deref(), Some("\"a'b\""));
        assert_eq!(py_repr("a\tb").as_deref(), Some("'a\\tb'"));
        assert_eq!(py_repr("é"), None);
        let mut m = Map::new();
        m.insert("b".into(), Value::Bool(true));
        m.insert("a".into(), Value::String("x".into()));
        m.insert("c".into(), Value::Null);
        assert_eq!(settings_json(&m).unwrap(), "{\"a\": \"x\", \"b\": true}");
    }

    #[test]
    fn url_paths_like_urlparse() {
        assert_eq!(url_path("https://h:1/a/b/?q#f").unwrap(), "/a/b/");
        assert_eq!(url_path("/a%20b/").unwrap(), "/a%20b/");
        assert!(url_path("/a;p").is_none());
        assert_eq!(unquote("/a%20b%2F").unwrap(), "/a b/");
    }

    #[test]
    fn realpath_follows_links_and_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let root = realpath(dir.path()).unwrap();
        std::fs::create_dir(root.join("in")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("in/out")).unwrap();
        std::os::unix::fs::symlink("..", root.join("in/up")).unwrap();
        assert_eq!(
            realpath(&root.join("in/out/x")).unwrap(),
            PathBuf::from("/etc/x")
        );
        assert_eq!(realpath(&root.join("in/up/in")).unwrap(), root.join("in"));
        std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
        assert_eq!(realpath(&root.join("loop")), Err(Fail::Decline));
    }
}
