//! noevia-core's sign-in and account routes in Rust (full-Rust migration M3,
//! docs/adr-0001-rust-and-repo-split.md "Amendment 2026-10-10" in sbstndalton/noevia), for when
//! the deployment switch NOEVIA_RUST_AUTH=1 hands them to the Rust front.
//!
//! [`Account::handle`] runs core index.cjs handleRequestScoped's order for the paths Rust owns:
//! routes/device-auth.cjs `open` (the feature switch, ambiguous credentials, the RFC 8628 code
//! and token endpoints), routes/auth.cjs `open` (setup, password and passkey sign-in,
//! invitations, recovery), the session gate (server-auth), routes/device-auth.cjs `account`,
//! routes/account.cjs (custom instructions, memory, preferences), then routes/auth.cjs `account`
//! (session, sign-out, profile, appearance, app passwords, passkeys, sessions, the
//! administrator's user list, invitations, disabling, recovery links). Where Node's chain would
//! fall through to a later module (a method or a path shape these routes do not answer),
//! [`Outcome::Pass`] hands the request to Node unchanged.
//!
//! Writes go through server-store's [`server_store::Writer`] (only the tables Rust owns), in one
//! transaction wherever Node's statements ran without yielding. Bodies, statuses, headers, cookies,
//! audit details and rate limits are Node's; the differences are listed in the crate README
//! section of noevia-rs (stricter refusals only).

pub mod argon;
pub mod rate;
pub mod unicode;
pub mod util;

mod account;
mod device;
mod files;
mod open;
mod passkeys;
mod users;

use js_json::JValue;
use server_auth::{Authenticator, Creds};
use server_store::{RustAuth, StoreError, Writer};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// http.cjs `readBody`'s default limit (readJson).
pub const BODY_LIMIT: usize = 1024 * 1024;

/// A request body as the front read it: at most [`BODY_LIMIT`] + 1 bytes.
#[derive(Debug, Clone, Default)]
pub struct Body {
    pub bytes: Vec<u8>,
    /// More than [`BODY_LIMIT`] bytes were sent.
    pub over: bool,
}

/// What a route reads from a request.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// The WHATWG-parsed pathname Node routes on.
    pub path: String,
    pub creds: Creds,
    /// `req.headers['user-agent']` (latin1).
    pub user_agent: String,
    /// `req.headers['content-type']`.
    pub content_type: String,
    /// auth.cjs `clientAddress(req, trustProxy)`, from the client's own connection (the front
    /// sees it directly, so per-address limits are per client: sbstndalton/noevia#1245).
    pub client_ip: String,
    pub body: Body,
}

/// Why a handler stopped: Node's `errorResponse` shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// An unexpected error: 500 `{"error":"Internal error"}`.
    Internal,
    /// A deliberate 4xx with its message (http.cjs: `{ error: message }`).
    Status(u16, String),
    /// The store is not usable: 503 (never signed in).
    Unavailable,
}

impl From<StoreError> for Fault {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Missing
            | StoreError::NotReady(_)
            | StoreError::SchemaTooNew { .. }
            | StoreError::Poisoned => Fault::Unavailable,
            StoreError::Sqlite(_) | StoreError::NotOwned(_) => Fault::Internal,
        }
    }
}

/// A JSON reply (http.cjs `json`): the front adds Content-Type, Cache-Control: no-store and the
/// security headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    /// `JSON.stringify(body)`.
    pub body: String,
    /// Set-Cookie values, in order.
    pub cookies: Vec<String>,
    /// Other headers (WWW-Authenticate on a 401).
    pub headers: Vec<(String, String)>,
}

impl Reply {
    pub fn json(status: u16, body: &JValue) -> Self {
        Reply {
            status,
            body: js_json::stringify(body).unwrap_or_default(),
            cookies: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// `{ error: message }`.
    pub fn error(status: u16, message: &str) -> Self {
        Self::json(status, &JValue::obj([("error", JValue::from(message))]))
    }

    fn with_cookies(mut self, cookies: Vec<String>) -> Self {
        self.cookies = cookies;
        self
    }
}

/// What the front does with a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Reply(Reply),
    /// Node's chain would have reached a later module: proxy the request to Node.
    Pass,
}

impl From<Reply> for Outcome {
    fn from(r: Reply) -> Self {
        Outcome::Reply(r)
    }
}

type Handled = Result<Outcome, Fault>;

/// The environment the routes read, with core index.cjs's names.
#[derive(Debug, Clone)]
pub struct Settings {
    /// PUBLIC_ORIGIN.
    pub public_origin: String,
    /// WEBAUTHN_RP_ID.
    pub webauthn_rp_id: String,
    /// TRUST_PROXY === 'true'.
    pub trust_proxy: bool,
    /// UI_DATA_DIR.
    pub data_dir: PathBuf,
    /// dav-settings.cjs `configuration(env).available`: `Number(COWORK_DAV_PORT || 0)` is set.
    pub dav_available: bool,
}

impl Settings {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>, data_dir: PathBuf) -> Self {
        let port = util::js_number(&get("COWORK_DAV_PORT").unwrap_or_default());
        Settings {
            public_origin: get("PUBLIC_ORIGIN").unwrap_or_default(),
            webauthn_rp_id: get("WEBAUTHN_RP_ID").unwrap_or_default(),
            trust_proxy: get("TRUST_PROXY").as_deref() == Some("true"),
            data_dir,
            dav_available: port != 0.0 && !port.is_nan(),
        }
    }
}

/// The routes' state: the writer, the gate, the settings and the in-memory rate limiters Node
/// keeps per process (auth.cjs `rate` and device-auth.cjs's own).
pub struct Account {
    writer: Arc<Writer>,
    auth: Arc<Authenticator>,
    settings: Settings,
    switch: RustAuth,
    rate: Mutex<rate::RateLimiter>,
    device_rate: Mutex<rate::RateLimiter>,
    decoy_key: Mutex<Option<Vec<u8>>>,
}

/// The paths Node's open mounts answer without a session (index.cjs `publicAuthRoutes`).
pub const PUBLIC_AUTH_ROUTES: &[&str] = &[
    "/api/setup/status",
    "/api/setup/complete",
    "/api/auth/login/password",
    "/api/auth/login/passkey/options",
    "/api/auth/login/passkey/verify",
    "/api/auth/invitations/accept",
    "/api/auth/recovery/complete",
];

impl Account {
    pub fn new(
        writer: Arc<Writer>,
        auth: Arc<Authenticator>,
        settings: Settings,
        switch: RustAuth,
        now: i64,
    ) -> Self {
        Account {
            writer,
            auth,
            settings,
            switch,
            rate: Mutex::new(rate::RateLimiter::new(now)),
            device_rate: Mutex::new(rate::RateLimiter::new(now)),
            decoy_key: Mutex::new(None),
        }
    }

    /// One request at `now` (epoch ms). Blocking: SQLite and Argon2.
    pub fn handle(&self, req: &Request, now: i64) -> Outcome {
        match self.pipeline(req, now) {
            Ok(o) => o,
            Err(Fault::Status(status, message)) => Reply::error(status, &message).into(),
            Err(Fault::Unavailable) => {
                Reply::error(503, "Sign-in is unavailable right now. Try again shortly.").into()
            }
            Err(Fault::Internal) => Reply::error(500, "Internal error").into(),
        }
    }

    fn pipeline(&self, req: &Request, now: i64) -> Handled {
        if let Some(o) = device::open(self, req, now)? {
            return Ok(o);
        }
        if let Some(o) = open::open(self, req, now)? {
            return Ok(o);
        }
        // index.cjs: the session gate for every /api/ path outside publicAuthRoutes.
        let api = req.path.starts_with("/api/");
        let authn = if api {
            self.writer
                .read(|r| self.auth.authenticate_in(r, &req.creds, now))?
        } else {
            None
        };
        if api && !PUBLIC_AUTH_ROUTES.contains(&req.path.as_str()) && authn.is_none() {
            let mut r = Reply::error(401, "unauthorized");
            r.headers
                .push(("WWW-Authenticate".into(), "Bearer realm=\"cowork\"".into()));
            return Ok(r.into());
        }
        let Some(authn) = authn else {
            return Ok(Outcome::Pass);
        };
        let method = req.method.as_str();
        if !matches!(method, "GET" | "HEAD" | "OPTIONS") {
            let origin_ok = self
                .writer
                .read(|r| self.auth.origin_valid(r, &req.creds))?;
            if !origin_ok || !self.auth.csrf_valid(&req.creds, &authn) {
                return Ok(Reply::error(403, "invalid CSRF token").into());
            }
        }
        if server_auth::identity::browser_only(&authn, &req.path, method) {
            return Ok(Reply::json(
                403,
                &JValue::obj([
                    (
                        "error",
                        JValue::from("This needs a signed-in browser session."),
                    ),
                    ("code", JValue::from("browser_session_required")),
                ]),
            )
            .into());
        }
        if let Some(o) = device::account(self, req, &authn, now)? {
            return Ok(o);
        }
        if let Some(o) = files::account_files(self, req, &authn, now)? {
            return Ok(o);
        }
        if let Some(o) = account::account(self, req, &authn, now)? {
            return Ok(o);
        }
        Ok(Outcome::Pass)
    }

    /// `features.enabled('nativeClientAuth')`, read per request.
    fn native_client_auth(&self) -> Result<bool, Fault> {
        Ok(self.writer.read(|r| self.auth.native_client_auth(r))?)
    }

    /// The public address Node compares with (auth.cjs `origin` after refreshOrigin): '' when none.
    fn origin(&self, r: &server_store::Reader<'_>) -> Result<String, Fault> {
        Ok(match self.auth.current_origin(r)? {
            server_auth::CurrentOrigin::Exact(o) => o,
            _ => String::new(),
        })
    }
}

/// http.cjs `readJson(req, limit)`.
fn read_json(req: &Request, limit: usize) -> Result<JValue, Fault> {
    if req.body.over || req.body.bytes.len() > limit {
        return Err(Fault::Status(413, "Request exceeds size limit".into()));
    }
    let raw = String::from_utf8_lossy(&req.body.bytes);
    if raw.is_empty() {
        return Ok(JValue::Obj(Vec::new()));
    }
    js_json::parse(&raw).map_err(|_| Fault::Status(400, "invalid JSON".into()))
}

/// http.cjs `requireJsonObject(readJson)`.
fn read_object(req: &Request) -> Result<JValue, Fault> {
    let v = read_json(req, BODY_LIMIT)?;
    if !v.is_object() {
        return Err(Fault::Status(
            400,
            "request body must be a JSON object".into(),
        ));
    }
    Ok(v)
}

/// `decodeURIComponent(segment)`; `None` where it throws (URIError).
fn decode_segment(s: &str) -> Option<String> {
    server_auth::js::decode_uri_component(s)
}
