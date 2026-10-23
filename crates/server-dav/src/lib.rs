//! Source-only inbound Diary DAV contract. No production adapter or listener is shipped.
//! A future adapter must prove tenant isolation, live revocation and symlink-safe
//! Diary operations before this handler can be wired into an image.
#![forbid(unsafe_code)]

mod xml;

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode};
use base64::Engine;
use percent_encoding::{percent_decode_str, utf8_percent_encode};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use url::Url;

pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Lan,
    Public,
}
impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Lan => "lan",
            Scope::Public => "public",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    host: String,
    scope: Scope,
    proxy_token: Option<String>,
    deadline: Duration,
}

impl Config {
    /// Exact origin and transport policy. Callers must not bind a listener until
    /// this succeeds. Public DAV requires TLS termination with a private token.
    pub fn new(
        origin: &str,
        scope: Scope,
        proxy_token: Option<String>,
    ) -> Result<Self, &'static str> {
        let url = Url::parse(origin).map_err(|_| "invalid DAV origin")?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("DAV origin must be a clean HTTP(S) origin");
        }
        if scope == Scope::Public && url.scheme() != "https" {
            return Err("public DAV needs HTTPS");
        }
        let token = if url.scheme() == "https" {
            let t = proxy_token.ok_or("HTTPS DAV needs a proxy token")?;
            if !(64..=128).contains(&t.len())
                || !t
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err("invalid DAV proxy token");
            }
            Some(t)
        } else {
            None
        };
        let host = url[url::Position::BeforeHost..url::Position::AfterPort].to_owned();
        if host.is_empty() {
            return Err("DAV origin has no host");
        }
        Ok(Self {
            host,
            scope,
            proxy_token: token,
            deadline: Duration::from_secs(45),
        })
    }
    /// A shorter deadline is useful in synthetic cancellation tests.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub user_id: String,
    pub credential_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub modified_unix: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub content: String,
    pub version: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteCondition {
    Match(String),
    Create,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub mkdir: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    NotFound,
    Forbidden,
    Conflict,
    AlreadyExists,
    MissingParent,
    Busy,
    Unavailable,
    Ambiguous,
    TooMany,
}

/// Every method receives an authenticated user, not an arbitrary tenant ID.
/// The future production adapter must use server-auth::app_passwords::verify_dav,
/// recheck credential/user/Diary/scope/local-storage on every allowed() call.
/// Every list/read/write/mkdir implementation must ALSO enforce this live check
/// at its own operation boundary (atomically with writes), so revocation between
/// allowed() and a backend call cannot disclose or mutate data. list() must
/// bound its result before allocation (at most 1024 direct children). Mutations
/// must route through Diary's guarded workspace operations. The adapter must
/// refuse symlink traversal; this crate never opens a filesystem path.
pub trait Backend: Send + Sync + 'static {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    fn authenticate<'a>(
        &'a self,
        username: &'a str,
        password: &'a str,
        scope: Scope,
    ) -> BackendFuture<'a, Result<Option<Identity>, Fault>>;
    fn allowed<'a>(
        &'a self,
        identity: &'a Identity,
        scope: Scope,
    ) -> BackendFuture<'a, Result<bool, Fault>>;
    fn list<'a>(
        &'a self,
        identity: &'a Identity,
        parent: &'a str,
    ) -> BackendFuture<'a, Result<Vec<Entry>, Fault>>;
    fn read<'a>(
        &'a self,
        identity: &'a Identity,
        path: &'a str,
    ) -> BackendFuture<'a, Result<Option<File>, Fault>>;
    fn write<'a>(
        &'a self,
        identity: &'a Identity,
        path: &'a str,
        content: &'a str,
        condition: WriteCondition,
    ) -> BackendFuture<'a, Result<String, Fault>>;
    fn mkdir<'a>(
        &'a self,
        _identity: &'a Identity,
        _path: &'a str,
    ) -> BackendFuture<'a, Result<(), Fault>> {
        Box::pin(async { Err(Fault::Unavailable) })
    }
}

/// Explicit fail-closed placeholder. It cannot authenticate any request.
pub struct DenyAll;
impl Backend for DenyAll {
    fn authenticate<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Scope,
    ) -> BackendFuture<'a, Result<Option<Identity>, Fault>> {
        Box::pin(async { Ok(None) })
    }
    fn allowed<'a>(&'a self, _: &'a Identity, _: Scope) -> BackendFuture<'a, Result<bool, Fault>> {
        Box::pin(async { Ok(false) })
    }
    fn list<'a>(
        &'a self,
        _: &'a Identity,
        _: &'a str,
    ) -> BackendFuture<'a, Result<Vec<Entry>, Fault>> {
        Box::pin(async { Err(Fault::Unavailable) })
    }
    fn read<'a>(
        &'a self,
        _: &'a Identity,
        _: &'a str,
    ) -> BackendFuture<'a, Result<Option<File>, Fault>> {
        Box::pin(async { Err(Fault::Unavailable) })
    }
    fn write<'a>(
        &'a self,
        _: &'a Identity,
        _: &'a str,
        _: &'a str,
        _: WriteCondition,
    ) -> BackendFuture<'a, Result<String, Fault>> {
        Box::pin(async { Err(Fault::Unavailable) })
    }
}

pub struct DavService<B: Backend> {
    backend: Arc<B>,
    config: Config,
    active: Semaphore,
    rate: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

impl<B: Backend> DavService<B> {
    pub fn new(backend: Arc<B>, config: Config) -> Self {
        Self {
            backend,
            config,
            active: Semaphore::new(4),
            rate: Mutex::new(HashMap::new()),
        }
    }

    /// The caller supplies the actual socket address. Forwarded client-IP headers
    /// never affect this budget. The returned future owns no detached work.
    pub async fn handle(&self, req: Request<Body>, peer: IpAddr) -> Response<Body> {
        let is_head = req.method() == axum::http::Method::HEAD;
        let mut result = self.handle_inner(req, peer).await;
        if is_head {
            *result.body_mut() = Body::empty();
        }
        result
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        result.headers_mut().insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        result
    }

    async fn handle_inner(&self, req: Request<Body>, peer: IpAddr) -> Response<Body> {
        let headers = req.headers();
        if duplicate_security_header(headers) {
            return reply(StatusCode::BAD_REQUEST, "Duplicate security header", false);
        }
        if headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .is_none_or(|h| !h.eq_ignore_ascii_case(&self.config.host))
        {
            return reply(
                StatusCode::MISDIRECTED_REQUEST,
                "Wrong sharing authority",
                false,
            );
        }
        if let Some(token) = &self.config.proxy_token {
            let given = headers
                .get("x-cowork-dav-proxy-token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let good =
                given.len() == token.len() && bool::from(given.as_bytes().ct_eq(token.as_bytes()));
            if !good
                || headers
                    .get("x-forwarded-proto")
                    .and_then(|v| v.to_str().ok())
                    != Some("https")
            {
                return reply(
                    StatusCode::FORBIDDEN,
                    "HTTPS proxy verification required",
                    false,
                );
            }
        }
        let raw = req.uri().path_and_query().map(|v| v.as_str()).unwrap_or("");
        let (username, path) = match parse_path(raw) {
            Ok(v) => v,
            Err(status) => {
                return reply(
                    status,
                    if status == StatusCode::NOT_FOUND {
                        "Unknown sharing path"
                    } else {
                        "Invalid path"
                    },
                    false,
                )
            }
        };
        if self.rate_limited(peer) {
            return reply(StatusCode::TOO_MANY_REQUESTS, "Try later", false);
        }
        let Ok(_permit) = self.active.try_acquire() else {
            return reply(StatusCode::SERVICE_UNAVAILABLE, "Sharing is busy", false);
        };
        let (auth_user, password) = match basic(headers) {
            Some(v) => v,
            None => return challenge(),
        };
        if auth_user.to_lowercase() != username.to_lowercase() {
            return challenge();
        }
        let identity = match tokio::time::timeout(
            self.config.deadline,
            self.backend
                .authenticate(&auth_user, &password, self.config.scope),
        )
        .await
        {
            Ok(Ok(Some(id))) => id,
            Ok(Ok(None)) => return challenge(),
            Ok(Err(f)) => return fault_reply(f, false),
            Err(_) => return fault_reply(Fault::Unavailable, false),
        };
        if !self.is_allowed(&identity).await {
            return reply(
                StatusCode::FORBIDDEN,
                "Sharing is off for this account or storage",
                false,
            );
        }
        self.dispatch(req, &identity, &username, &path).await
    }

    fn rate_limited(&self, peer: IpAddr) -> bool {
        let Ok(mut rates) = self.rate.lock() else {
            return true;
        };
        let now = Instant::now();
        if !rates.contains_key(&peer) && rates.len() >= 4096 {
            rates.retain(|_, recent| {
                while recent
                    .front()
                    .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
                {
                    recent.pop_front();
                }
                !recent.is_empty()
            });
            if rates.len() >= 4096 {
                return true;
            }
        }
        let recent = rates.entry(peer).or_default();
        while recent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            recent.pop_front();
        }
        if recent.len() >= 120 {
            return true;
        }
        recent.push_back(now);
        false
    }

    async fn is_allowed(&self, identity: &Identity) -> bool {
        matches!(
            tokio::time::timeout(
                self.config.deadline,
                self.backend.allowed(identity, self.config.scope)
            )
            .await,
            Ok(Ok(true))
        )
    }
}

fn duplicate_security_header(headers: &HeaderMap) -> bool {
    [
        header::HOST,
        header::AUTHORIZATION,
        header::IF_MATCH,
        header::IF_NONE_MATCH,
        header::HeaderName::from_static("x-cowork-dav-proxy-token"),
        header::HeaderName::from_static("x-forwarded-proto"),
    ]
    .iter()
    .any(|name| headers.get_all(name).iter().nth(1).is_some())
}

fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if raw.len() > 512 {
        return None;
    }
    let (scheme, encoded) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Basic")
        || encoded.is_empty()
        || !encoded
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
    {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, password) = text.split_once(':')?;
    if user.is_empty() {
        return None;
    }
    Some((user.to_owned(), password.to_owned()))
}

fn parse_path(raw: &str) -> Result<(String, String), StatusCode> {
    if !raw.starts_with("/dav/")
        || raw.contains('?')
        || raw.contains('#')
        || raw.chars().any(|c| c == '\\' || c.is_control())
    {
        return Err(StatusCode::NOT_FOUND);
    }
    if raw.len() > 2000 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut segments = Vec::new();
    for raw_part in raw[5..].split('/') {
        let part = percent_decode_str(raw_part)
            .decode_utf8()
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        if part.is_empty() {
            segments.push(String::new());
            continue;
        }
        if part.starts_with('.')
            || part
                .chars()
                .any(|c| c == '/' || c == '\\' || c == '%' || c.is_control() || c == '\u{7f}')
        {
            return Err(StatusCode::BAD_REQUEST);
        }
        segments.push(part.into_owned());
    }
    if segments.last().is_some_and(String::is_empty) {
        segments.pop();
    }
    let Some(username) = segments.first().cloned() else {
        return Err(StatusCode::BAD_REQUEST);
    };
    if username.is_empty() || segments.iter().skip(1).any(String::is_empty) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = segments
        .iter()
        .skip(1)
        .cloned()
        .collect::<Vec<_>>()
        .join("/");
    if path.len() > 500 {
        return Err(StatusCode::URI_TOO_LONG);
    }
    Ok((username, path))
}

fn reply(status: StatusCode, body: &str, head: bool) -> Response<Body> {
    let mut response = Response::new(if head {
        Body::empty()
    } else {
        Body::from(body.to_owned())
    });
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string()).unwrap_or(HeaderValue::from_static("0")),
    );
    response
}
fn challenge() -> Response<Body> {
    let mut response = reply(
        StatusCode::UNAUTHORIZED,
        "Device app password required",
        false,
    );
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"noevia diary files\", charset=\"UTF-8\""),
    );
    response
}
fn fault_reply(fault: Fault, write: bool) -> Response<Body> {
    let (status, message) = match fault {
        Fault::NotFound => (StatusCode::NOT_FOUND, "File or folder not found"),
        Fault::Forbidden => (StatusCode::FORBIDDEN, "Sharing was disabled"),
        Fault::Conflict if write => (StatusCode::PRECONDITION_FAILED, "Conflict"),
        Fault::Conflict | Fault::MissingParent => (StatusCode::CONFLICT, "Conflict"),
        Fault::AlreadyExists => (StatusCode::METHOD_NOT_ALLOWED, "Collection already exists"),
        Fault::Busy => (StatusCode::SERVICE_UNAVAILABLE, "Sharing is busy"),
        Fault::Ambiguous => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The outcome is unknown; read the file again before retrying",
        ),
        Fault::TooMany => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "Folder exceeds the sharing limit",
        ),
        Fault::Unavailable => (StatusCode::BAD_GATEWAY, "Diary storage is unavailable"),
    };
    let mut response = reply(status, message, false);
    if fault == Fault::Ambiguous {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    }
    response
}

impl<B: Backend> DavService<B> {
    async fn call<T>(&self, future: BackendFuture<'_, Result<T, Fault>>) -> Result<T, Fault> {
        tokio::time::timeout(self.config.deadline, future)
            .await
            .map_err(|_| Fault::Unavailable)?
    }

    async fn lookup(
        &self,
        identity: &Identity,
        username: &str,
        path: &str,
    ) -> Result<Option<Entry>, Fault> {
        if path.is_empty() {
            return Ok(Some(Entry {
                path: String::new(),
                name: username.to_owned(),
                is_dir: true,
                modified_unix: None,
            }));
        }
        let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
        let entries = self.call(self.backend.list(identity, parent)).await?;
        if entries.len() > 1024 {
            return Err(Fault::TooMany);
        }
        if !entries.iter().all(|e| valid_entry(parent, e)) {
            return Err(Fault::Unavailable);
        }
        Ok(entries.into_iter().find(|e| e.path == path))
    }

    async fn dispatch(
        &self,
        req: Request<Body>,
        identity: &Identity,
        username: &str,
        path: &str,
    ) -> Response<Body> {
        let method = req.method().as_str().to_owned();
        let headers = req.headers().clone();
        let cap = self.backend.capabilities();
        let allow = if cap.mkdir {
            "OPTIONS, HEAD, GET, PROPFIND, PUT, MKCOL"
        } else {
            "OPTIONS, HEAD, GET, PROPFIND, PUT"
        };
        if method == "OPTIONS" {
            let mut response = reply(StatusCode::OK, "", false);
            response.headers_mut().insert(
                header::ALLOW,
                HeaderValue::from_static(if cap.mkdir {
                    "OPTIONS, HEAD, GET, PROPFIND, PUT, MKCOL"
                } else {
                    "OPTIONS, HEAD, GET, PROPFIND, PUT"
                }),
            );
            // Class 1 is not advertised: DELETE/MOVE/COPY are still absent.
            return response;
        }
        if !matches!(
            method.as_str(),
            "GET" | "HEAD" | "PROPFIND" | "PUT" | "MKCOL"
        ) || (method == "MKCOL" && !cap.mkdir)
        {
            let mut response = reply(
                StatusCode::METHOD_NOT_ALLOWED,
                "This Markdown endpoint does not support that operation",
                false,
            );
            if let Ok(value) = HeaderValue::from_str(allow) {
                response.headers_mut().insert(header::ALLOW, value);
            }
            return response;
        }
        match method.as_str() {
            "GET" | "HEAD" => {
                self.get(identity, username, path, &headers, method == "HEAD")
                    .await
            }
            "PROPFIND" => {
                self.propfind(identity, username, path, &headers, req.into_body())
                    .await
            }
            "PUT" => {
                // A trailing slash names a collection on the wire, even when
                // parse_path normalizes it for directory methods.
                if req.uri().path().ends_with('/') {
                    return reply(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        "Only Markdown files can be written",
                        false,
                    );
                }
                self.put(identity, username, path, &headers, req.into_body())
                    .await
            }
            "MKCOL" => self.mkcol(identity, path, &headers, req.into_body()).await,
            _ => reply(StatusCode::METHOD_NOT_ALLOWED, "Unsupported", false),
        }
    }

    async fn get(
        &self,
        identity: &Identity,
        username: &str,
        path: &str,
        headers: &HeaderMap,
        head: bool,
    ) -> Response<Body> {
        let item = match self.lookup(identity, username, path).await {
            Ok(Some(e)) => e,
            Ok(None) => return reply(StatusCode::NOT_FOUND, "File or folder not found", head),
            Err(f) => return fault_reply(f, false),
        };
        if item.is_dir {
            return reply(
                StatusCode::METHOD_NOT_ALLOWED,
                "Use PROPFIND to list this folder",
                head,
            );
        }
        let file = match self.call(self.backend.read(identity, path)).await {
            Ok(Some(f)) => f,
            Ok(None) => return reply(StatusCode::NOT_FOUND, "File not found", head),
            Err(f) => return fault_reply(f, false),
        };
        if !valid_tag(&file.version) || file.content.len() > 512 * 1024 {
            return fault_reply(Fault::Unavailable, false);
        }
        let tag = format!("\"{}\"", file.version);
        if headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == tag || v == "*")
        {
            let mut response = reply(StatusCode::NOT_MODIFIED, "", true);
            if let Ok(value) = HeaderValue::from_str(&tag) {
                response.headers_mut().insert(header::ETAG, value);
            }
            return response;
        }
        let mut response = typed_reply(
            StatusCode::OK,
            &file.content,
            "text/markdown; charset=utf-8",
            head,
        );
        if let Ok(value) = HeaderValue::from_str(&tag) {
            response.headers_mut().insert(header::ETAG, value);
        }
        response
    }

    async fn propfind(
        &self,
        identity: &Identity,
        username: &str,
        path: &str,
        headers: &HeaderMap,
        body: Body,
    ) -> Response<Body> {
        let depth = headers
            .get("depth")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("infinity");
        if depth != "0" && depth != "1" {
            return reply(
                StatusCode::FORBIDDEN,
                "Only Depth 0 and 1 are supported",
                false,
            );
        }
        let bytes = match tokio::time::timeout(self.config.deadline, to_bytes(body, 8192)).await {
            Ok(Ok(b)) => b,
            Ok(Err(_)) => return reply(StatusCode::PAYLOAD_TOO_LARGE, "Request too large", false),
            Err(_) => {
                return reply(
                    StatusCode::REQUEST_TIMEOUT,
                    "Property read timed out",
                    false,
                )
            }
        };
        let text = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => return reply(StatusCode::BAD_REQUEST, "UTF-8 text required", false),
        };
        let selection = match xml::selection(text) {
            Ok(s) => s,
            Err(()) => return reply(StatusCode::BAD_REQUEST, "Unsupported property XML", false),
        };
        let item = match self.lookup(identity, username, path).await {
            Ok(Some(e)) => e,
            Ok(None) => return reply(StatusCode::NOT_FOUND, "File or folder not found", false),
            Err(f) => return fault_reply(f, false),
        };
        let mut entries = vec![item.clone()];
        if item.is_dir && depth == "1" {
            let children = match self.call(self.backend.list(identity, path)).await {
                Ok(v) => v,
                Err(f) => return fault_reply(f, false),
            };
            if children.len() > 100 {
                return reply(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Folder exceeds the 100-child sharing limit",
                    false,
                );
            }
            if !children.iter().all(|e| valid_entry(path, e)) {
                return fault_reply(Fault::Unavailable, false);
            }
            entries.extend(children);
        }
        let deadline = Instant::now() + self.config.deadline;
        let mut rows = String::new();
        for entry in &entries {
            if Instant::now() >= deadline {
                return reply(
                    StatusCode::GATEWAY_TIMEOUT,
                    "Property read timed out",
                    false,
                );
            }
            if !self.is_allowed(identity).await {
                return reply(
                    StatusCode::FORBIDDEN,
                    "Sharing was disabled or credential revoked",
                    false,
                );
            }
            let wants = |name: &str| {
                selection
                    .as_ref()
                    .is_none_or(|s| s.iter().any(|p| p.uri == "DAV:" && p.local == name))
            };
            let mut values: Vec<(&str, String)> = vec![
                ("displayname", xml::escape(&entry.name)),
                (
                    "resourcetype",
                    if entry.is_dir {
                        "<d:collection/>".to_owned()
                    } else {
                        String::new()
                    },
                ),
                (
                    "getcontenttype",
                    if entry.is_dir {
                        "httpd/unix-directory".to_owned()
                    } else {
                        "text/markdown; charset=utf-8".to_owned()
                    },
                ),
            ];
            if let Some(seconds) = entry.modified_unix.filter(|s| *s <= 253_402_300_799) {
                if let Some(when) = std::time::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
                {
                    values.push(("getlastmodified", httpdate::fmt_http_date(when)));
                }
            }
            if !entry.is_dir && (wants("getetag") || wants("getcontentlength")) {
                let file = match self.call(self.backend.read(identity, &entry.path)).await {
                    Ok(Some(f)) => f,
                    Ok(None) => continue,
                    Err(f) => return fault_reply(f, false),
                };
                if !valid_tag(&file.version) || file.content.len() > 512 * 1024 {
                    return fault_reply(Fault::Unavailable, false);
                }
                values.push(("getetag", xml::escape(&format!("\"{}\"", file.version))));
                values.push(("getcontentlength", file.content.len().to_string()));
            }
            let wanted: Vec<xml::Property> = selection.clone().unwrap_or_else(|| {
                values
                    .iter()
                    .map(|(local, _)| xml::Property {
                        uri: "DAV:".into(),
                        local: (*local).into(),
                    })
                    .collect()
            });
            let mut ok = String::new();
            let mut missing = String::new();
            for p in wanted {
                if p.uri == "DAV:" {
                    if let Some((_, value)) = values.iter().find(|(name, _)| *name == p.local) {
                        ok.push_str(&format!("<d:{0}>{1}</d:{0}>", p.local, value));
                        continue;
                    }
                }
                missing.push_str(&format!(
                    "<x:{0} xmlns:x=\"{1}\"/>",
                    p.local,
                    xml::escape(&p.uri)
                ));
            }
            let href = format!(
                "/dav/{}/{}{}",
                encode(username),
                entry
                    .path
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .map(encode)
                    .collect::<Vec<_>>()
                    .join("/"),
                if entry.is_dir && !entry.path.is_empty() {
                    "/"
                } else {
                    ""
                }
            );
            rows.push_str(&format!(
                "<d:response><d:href>{}</d:href>{}{}</d:response>",
                xml::escape(&href),
                propstat("200 OK", &ok),
                propstat("404 Not Found", &missing)
            ));
            if rows.len() > 1024 * 1024 {
                return reply(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Property reply too large",
                    false,
                );
            }
        }
        let xml = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><d:multistatus xmlns:d=\"DAV:\">{rows}</d:multistatus>");
        if xml.len() > 1024 * 1024 {
            return reply(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Property reply too large",
                false,
            );
        }
        typed_reply(
            StatusCode::MULTI_STATUS,
            &xml,
            "application/xml; charset=utf-8",
            false,
        )
    }

    async fn put(
        &self,
        identity: &Identity,
        username: &str,
        path: &str,
        headers: &HeaderMap,
        body: Body,
    ) -> Response<Body> {
        if !path.to_lowercase().ends_with(".md") || path.is_empty() {
            return reply(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Only Markdown files can be written",
                false,
            );
        }
        if headers
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
        {
            return reply(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Encoded bodies are unsupported",
                false,
            );
        }
        if headers.contains_key("if") || headers.contains_key(header::IF_UNMODIFIED_SINCE) {
            return reply(
                StatusCode::BAD_REQUEST,
                "Use If-Match or If-None-Match",
                false,
            );
        }
        let matching = headers.get(header::IF_MATCH).and_then(|v| v.to_str().ok());
        let new_only = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok());
        if (matching.is_some() && new_only.is_some())
            || matching.is_some_and(|v| !valid_quoted_tag(v))
            || new_only.is_some_and(|v| v != "*")
            || (headers.contains_key(header::IF_MATCH) && matching.is_none())
            || (headers.contains_key(header::IF_NONE_MATCH) && new_only.is_none())
        {
            return reply(
                StatusCode::BAD_REQUEST,
                "A single strong ETag or create-only condition is required",
                false,
            );
        }
        let condition = match (matching, new_only) {
            (Some(tag), None) => WriteCondition::Match(tag.trim_matches('"').to_owned()),
            (None, Some(_)) => WriteCondition::Create,
            _ => {
                return reply(
                    StatusCode::PRECONDITION_REQUIRED,
                    "Read the ETag and use If-Match; create with If-None-Match: *",
                    false,
                )
            }
        };
        let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
        match self.lookup(identity, username, parent).await {
            Ok(Some(e)) if e.is_dir => (),
            Ok(_) => return reply(StatusCode::CONFLICT, "Parent folder does not exist", false),
            Err(f) => return fault_reply(f, true),
        }
        let bytes = match tokio::time::timeout(self.config.deadline, to_bytes(body, 512 * 1024))
            .await
        {
            Ok(Ok(b)) => b,
            Ok(Err(_)) => return reply(StatusCode::PAYLOAD_TOO_LARGE, "Request too large", false),
            Err(_) => return reply(StatusCode::REQUEST_TIMEOUT, "Request timed out", false),
        };
        let content = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => return reply(StatusCode::BAD_REQUEST, "UTF-8 text required", false),
        };
        if !self.is_allowed(identity).await {
            return reply(StatusCode::FORBIDDEN, "Sharing was disabled", false);
        }
        let created = condition == WriteCondition::Create;
        let result = tokio::time::timeout(
            self.config.deadline,
            self.backend.write(identity, path, content, condition),
        )
        .await;
        let version = match result {
            Ok(Ok(v)) if valid_tag(&v) => v,
            Ok(Ok(_)) => return fault_reply(Fault::Ambiguous, true),
            Ok(Err(f)) => return fault_reply(f, true),
            Err(_) => return fault_reply(Fault::Ambiguous, true),
        };
        let mut response = reply(
            if created {
                StatusCode::CREATED
            } else {
                StatusCode::NO_CONTENT
            },
            "",
            false,
        );
        if let Ok(tag) = HeaderValue::from_str(&format!("\"{version}\"")) {
            response.headers_mut().insert(header::ETAG, tag);
        }
        response
    }

    async fn mkcol(
        &self,
        identity: &Identity,
        path: &str,
        headers: &HeaderMap,
        body: Body,
    ) -> Response<Body> {
        if path.is_empty() {
            return reply(
                StatusCode::METHOD_NOT_ALLOWED,
                "The diary root already exists",
                false,
            );
        }
        if headers
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
        {
            return reply(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Encoded bodies are unsupported",
                false,
            );
        }
        if headers.contains_key("if")
            || headers.contains_key(header::IF_MATCH)
            || headers.contains_key(header::IF_NONE_MATCH)
        {
            return reply(
                StatusCode::BAD_REQUEST,
                "Conditional MKCOL is unsupported",
                false,
            );
        }
        let bytes = match tokio::time::timeout(self.config.deadline, to_bytes(body, 8192)).await {
            Ok(Ok(b)) => b,
            Ok(Err(_)) => return reply(StatusCode::PAYLOAD_TOO_LARGE, "Request too large", false),
            Err(_) => return reply(StatusCode::REQUEST_TIMEOUT, "Request timed out", false),
        };
        if !bytes.is_empty() {
            return reply(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "MKCOL request bodies are unsupported",
                false,
            );
        }
        if !self.is_allowed(identity).await {
            return reply(StatusCode::FORBIDDEN, "Sharing was disabled", false);
        }
        match tokio::time::timeout(self.config.deadline, self.backend.mkdir(identity, path)).await {
            Ok(Ok(())) => reply(StatusCode::CREATED, "", false),
            Ok(Err(f)) => fault_reply(f, false),
            Err(_) => fault_reply(Fault::Ambiguous, true),
        }
    }
}

fn typed_reply(
    status: StatusCode,
    body: &str,
    content_type: &'static str,
    head: bool,
) -> Response<Body> {
    let mut response = reply(status, body, head);
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}
fn valid_tag(tag: &str) -> bool {
    tag.len() == 64
        && tag
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}
fn valid_quoted_tag(tag: &str) -> bool {
    tag.len() == 66
        && tag
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .is_some_and(valid_tag)
}
fn valid_entry(parent: &str, entry: &Entry) -> bool {
    let expected = entry.path.rsplit_once('/').map_or("", |(p, _)| p);
    expected == parent
        && !entry.name.is_empty()
        && entry.path.rsplit('/').next() == Some(entry.name.as_str())
        && parse_path(&format!("/dav/synthetic/{}", entry.path)).is_ok()
}
fn propstat(status: &str, values: &str) -> String {
    if values.is_empty() {
        String::new()
    } else {
        format!("<d:propstat><d:prop>{values}</d:prop><d:status>HTTP/1.1 {status}</d:status></d:propstat>")
    }
}
const ENCODE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'{')
    .add(b'|')
    .add(b'}');
fn encode(value: &str) -> String {
    utf8_percent_encode(value, ENCODE).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn stale_rate_peers_are_pruned_before_capacity_refusal() -> Result<(), &'static str> {
        let service = DavService::new(
            Arc::new(DenyAll),
            Config::new("http://localhost", Scope::Lan, None)?,
        );
        let old = Instant::now() - Duration::from_secs(61);
        {
            let mut rates = service.rate.lock().map_err(|_| "rate lock")?;
            for n in 0u16..4096 {
                let ip = IpAddr::V4(Ipv4Addr::new(10, 1, (n / 256) as u8, (n % 256) as u8));
                rates.insert(ip, VecDeque::from([old]));
            }
        }
        let newcomer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        assert!(!service.rate_limited(newcomer));
        assert_eq!(service.rate.lock().map_err(|_| "rate lock")?.len(), 1);
        Ok(())
    }
}
