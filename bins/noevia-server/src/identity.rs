//! The validated identity for Rust-owned routes (full-Rust migration M2): server-auth's port of
//! core index.cjs's session gate over a read-only `UI_DATA_DIR/cowork.db`.
//!
//! No route uses it yet: every auth route is still owner = "node" in contracts/http/routes.toml
//! and Node keeps answering (and gating) everything it owns, so live behaviour is unchanged.
//! A future Rust-owned handler takes [`ValidatedIdentity`] as an argument; the request then only
//! reaches the handler when Node's gate would have let it through, and is refused with Node's
//! exact 401/403 otherwise.
//!
//! Fail closed: until `cowork.db` exists and has the schema this build knows, or when the auth
//! environment is invalid (an unknown NOEVIA_FEATURE_NATIVE_CLIENT_AUTH value, which also stops
//! Node), every extraction answers 503 and nothing is treated as signed in. That does not stop the
//! front, which keeps proxying to Node: while no route needs an identity, refusing to start would
//! only take the proxy down. Once a Rust-owned route depends on it, main() should exit on
//! [`Status::Refused`] (schema newer than known) instead of logging it.

use crate::reply;
use crate::App;
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use server_auth::{Authenticator, Creds, Identity, Refusal};
use server_store::{Store, StoreError};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// What [`IdentityLayer::status`] reports at startup (never a token or path content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Ready,
    /// Not usable yet (no UI_DATA_DIR, no database yet, Node not finished migrating).
    Unavailable(String),
    /// Usable never without an update: the schema is newer than this build, or the auth
    /// environment is invalid.
    Refused(String),
}

pub struct IdentityLayer {
    data_dir: Option<PathBuf>,
    auth: Result<Authenticator, String>,
    /// Opened on first use and kept once it opens; a failed open is retried next time.
    store: Mutex<Option<Arc<Store>>>,
}

impl IdentityLayer {
    pub fn new(config: &crate::config::Config) -> Self {
        IdentityLayer {
            data_dir: config.data_dir.clone(),
            auth: config.auth.clone().map(Authenticator::new),
            store: Mutex::new(None),
        }
    }

    fn store(&self) -> Result<Arc<Store>, StoreError> {
        let mut slot = self.store.lock().map_err(|_| StoreError::Poisoned)?;
        if let Some(s) = slot.as_ref() {
            return Ok(Arc::clone(s));
        }
        let dir = self
            .data_dir
            .as_ref()
            .ok_or_else(|| StoreError::NotReady("UI_DATA_DIR is not set".into()))?;
        let store = Arc::new(Store::open(dir)?);
        *slot = Some(Arc::clone(&store));
        Ok(store)
    }

    /// The request gate's authenticator, unless the auth environment is invalid.
    pub fn authenticator(&self) -> Option<&Authenticator> {
        self.auth.as_ref().ok()
    }

    /// The read-only store (blocking: opens it the first time).
    pub fn store_blocking(&self) -> Result<Arc<Store>, StoreError> {
        self.store()
    }

    /// Blocking: opens the store if needed.
    pub fn status(&self) -> Status {
        if let Err(e) = &self.auth {
            return Status::Refused(e.clone());
        }
        match self.store() {
            Ok(_) => Status::Ready,
            Err(e @ StoreError::SchemaTooNew { .. }) => Status::Refused(e.to_string()),
            Err(e) => Status::Unavailable(e.to_string()),
        }
    }

    /// Node's gate for an /api/ request outside publicAuthRoutes. Blocking (SQLite).
    pub fn gate_blocking(
        &self,
        creds: &Creds,
        pathname: &str,
        now_ms: i64,
    ) -> Result<Identity, Rejection> {
        let auth = self.auth.as_ref().map_err(|_| Rejection::Unavailable)?;
        let store = self.store().map_err(|_| Rejection::Unavailable)?;
        match auth.gate(&store, creds, pathname, now_ms) {
            Ok(Ok(id)) => Ok(id),
            Ok(Err(refusal)) => Err(Rejection::Refused(refusal)),
            Err(_) => Err(Rejection::Unavailable),
        }
    }
}

/// The credential-bearing headers of a request, as Node's `req.headers` presents them.
pub fn creds_of(parts: &Parts) -> Creds {
    Creds::from_headers(
        parts.method.as_str(),
        parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_bytes())),
    )
}

/// `Date.now()`.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// `new URL(req.url, 'http://h').pathname`, or None when it does not parse.
pub fn whatwg_pathname(parts: &Parts) -> Option<String> {
    let target = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path(), |pq| pq.as_str());
    let base = url::Url::parse("http://h").ok()?;
    let parsed = url::Url::options()
        .base_url(Some(&base))
        .parse(target)
        .ok()?;
    Some(parsed.path().to_string())
}

/// A request Node's gate lets through, with who it is.
#[derive(Debug, Clone)]
pub struct ValidatedIdentity(pub Identity);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    Refused(Refusal),
    /// The store or the auth configuration is not usable: 503, never signed in.
    Unavailable,
    /// The path is not the one WHATWG URL parsing gives (http.cjs badRequestUrl): 400.
    BadUrl,
}

impl IntoResponse for Rejection {
    fn into_response(self) -> Response<Body> {
        match self {
            // http.cjs unauthorized().
            Rejection::Refused(Refusal::Unauthorized) => {
                let mut res = reply::json(StatusCode::UNAUTHORIZED, &serde_json::json!({"error": "unauthorized"}), true);
                res.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Bearer realm=\"cowork\""),
                );
                res
            }
            Rejection::Refused(Refusal::Csrf) => {
                reply::json(StatusCode::FORBIDDEN, &serde_json::json!({"error": "invalid CSRF token"}), true)
            }
            // Node's key order: error, then code.
            Rejection::Refused(Refusal::BrowserOnly) => reply::json_text(
                StatusCode::FORBIDDEN,
                r#"{"error":"This needs a signed-in browser session.","code":"browser_session_required"}"#.to_string(),
                true,
            ),
            Rejection::BadUrl => {
                reply::json(StatusCode::BAD_REQUEST, &serde_json::json!({"error": "invalid URL"}), true)
            }
            Rejection::Unavailable => reply::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Sign-in is unavailable right now. Try again shortly.",
                true,
            ),
        }
    }
}

impl FromRequestParts<Arc<App>> for ValidatedIdentity {
    type Rejection = Rejection;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &Arc<App>,
    ) -> Result<Self, Self::Rejection> {
        // Node gates on `new URL(req.url, base).pathname`; a path that WHATWG parsing changes
        // ("..", "%2e", "\\") would be gated here on a different path than Node's, so it is refused
        // before the gate, like Node's badRequestUrl (review F2).
        let path = parts.uri.path().to_string();
        if whatwg_pathname(parts).as_deref() != Some(path.as_str()) {
            return Err(Rejection::BadUrl);
        }
        let creds = creds_of(parts);
        let layer = Arc::clone(&app.identity);
        let now = now_ms();
        tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &path, now))
            .await
            .map_err(|_| Rejection::Unavailable)?
            .map(ValidatedIdentity)
    }
}
