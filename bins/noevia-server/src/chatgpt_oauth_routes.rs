//! Sign in with ChatGPT, the token broker (sbstndalton/noevia#2190 round 5), ported from core
//! chatgpt-oauth.cjs and routes/providers.cjs. The Rust front is the single owner of the device
//! sign-in, the per-account token store (`chatgpt_oauth_tokens`, `enc:v2` bound to the owner, as
//! core secrets.cjs seals it), the refresh and the model catalogue cache. Node's chat transport keeps
//! translating /chat/completions to the Codex Responses API, but holds no token: it asks this broker
//! for a current access token over the internal store channel (project_store.rs: loopback only,
//! per-start secret header, constant-time compare) and never sees a refresh token.
//!
//! Public routes (capability `rust-chatgpt-oauth`; core rust-chatgpt-oauth.cjs refuses them with
//! `RUST_CHATGPT_OAUTH_OWNED`):
//!
//! - `POST /api/providers/chatgpt/device`        -> `{ loginId, userCode, verificationUrl, interval, expiresAt }`
//! - `POST /api/providers/chatgpt/device/poll`   `{ loginId }` -> `pending | connected | expired | cancelled`
//! - `POST /api/providers/chatgpt/device/cancel` `{ loginId }` -> `{ ok }`
//! - `GET  /api/providers/chatgpt/models[?refresh=1]` -> `{ models, cached, fetchedAt, throttled? }`
//!
//! Order as Node: the session gate (401/403/CSRF), the `chatgptOAuth` flag (off: 404), then the
//! operation; a failure answers `{ error }` with its status (Node's `e.status < 600 ? e.status : 502`).
//!
//! Internal operations (`POST /v1/chatgpt/*` on the store listener, `{ user, ... }`):
//!
//! - `session { user, rejected? }` -> `{ accessToken, accountId, fedRamp, expiresAt, generation }`
//!   or 401/503 `{ error, code }` (`disconnected`, `reconnect`, `transient`). Refresh-ahead inside
//!   five minutes of expiry; `rejected` (the access token a 401 answered) refreshes only when it is
//!   still the stored one.
//! - `reject { user, generation }` -> `{ marked }`: the second 401 marks THIS sign-in (by its
//!   generation, a digest of its ciphertext) as needing a reconnect; a newer one is left alone.
//! - `catalogue { user }` -> `{ models: [{ slug, effort, summary, summaryParam, levels }] }`
//! - `models { user, refresh }` -> the picker's answer, or `{ error, status }`
//! - `disconnect { user }`, `forget { user }` -> `{ ok }`; `connected { user }` -> `{ exists }`
//!
//! Refresh lock: one async mutex per account serializes the refresh, the sign-in's save and the
//! disconnect. A waiter re-reads the row after taking it, so a refresh that completed meanwhile is
//! used rather than spending the (rotated) refresh token twice; every write is a compare-and-swap on
//! the ciphertext it started from (a disconnect or newer sign-in wins). After each committed change
//! the broker rewrites `chatgpt-token-changes.json`, whose file signal invalidates Node's short
//! in-process access-token cache (core shared-reread.cjs `fileSignal`).
//!
//! Outbound, as Node's `postJson` and fetches: Node's hosts (`https://auth.openai.com`,
//! `https://chatgpt.com/backend-api/codex`), no redirect followed, no environment proxy, 15 s for
//! the whole exchange, replies capped (256 KiB token answers, 2 MiB catalogue). The two hosts may be
//! replaced only by a loopback QA server (NOEVIA_QA_CHATGPT_ISSUER / NOEVIA_QA_CHATGPT_CODEX_BASE).
//! Tokens are never logged and never in an error.
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use js_json::{wtf16, JValue};
use rand_core::{OsRng, RngCore};
use server_auth::secrets::SecretKeys;
use server_store::RustChatgptOAuth;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zeroize::Zeroizing;

pub const DEVICE: &str = "/api/providers/chatgpt/device";
pub const POLL: &str = "/api/providers/chatgpt/device/poll";
pub const CANCEL: &str = "/api/providers/chatgpt/device/cancel";
pub const MODELS: &str = "/api/providers/chatgpt/models";
/// The four routes this module answers (routes.toml gives Rust exactly these methods).
pub const ROUTES: &[&str] = &[DEVICE, POLL, CANCEL, MODELS];
/// The file whose signal tells Node a token row changed.
pub const SIGNAL_FILE: &str = "chatgpt-token-changes.json";

const ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_BASE: &str = "https://chatgpt.com/backend-api/codex";
const CODEX_VERSION: &str = "0.155.0";
const DEVICE_TTL_MS: i64 = 15 * 60 * 1000;
const MAX_PENDING_PER_USER: usize = 3;
const REFRESH_MARGIN_MS: f64 = 5.0 * 60.0 * 1000.0;
const MODEL_CACHE_MS: i64 = 5 * 60 * 1000;
const MODEL_FAILURE_CACHE_MS: i64 = 60 * 1000;
const MODEL_REFRESH_GAP_MS: i64 = 10 * 1000;
const START_WINDOW_MS: i64 = 10 * 60 * 1000;
const MAX_STARTS_PER_WINDOW: usize = 5;
const TIMEOUT: Duration = Duration::from_secs(15);
const TOKEN_REPLY_CAP: usize = 256 * 1024;
const CATALOGUE_REPLY_CAP: usize = 2 * 1024 * 1024;
const BODY_LIMIT: usize = 1024 * 1024;
const BODY_DEADLINE: Duration = Duration::from_secs(10);
/// Most accounts whose explicit-refresh time is remembered (core: 1000).
const REFRESHES_MAX: usize = 1000;

const OFF_MESSAGE: &str = "Sign in with ChatGPT is turned off on this server.";
const DISCONNECTED: &str =
    "ChatGPT is not connected. Sign in with ChatGPT in Settings → AI providers.";
const RECONNECT: &str = "Reconnect needed: your ChatGPT sign-in expired or was revoked. Sign in with ChatGPT again in Settings → AI providers.";
const TRANSIENT: &str = "ChatGPT sign-in could not be refreshed right now. Try again in a moment.";

/// A failure as core's `fail(message, status, code)`: written for people, never a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fail {
    pub status: u16,
    pub message: String,
    pub code: Option<&'static str>,
}

fn fail(message: impl Into<String>, status: u16) -> Fail {
    Fail {
        status,
        message: message.into(),
        code: None,
    }
}

/// core `authError(code)`.
fn auth_error(code: &'static str) -> Fail {
    let (message, status) = match code {
        "disconnected" => (DISCONNECTED, 401),
        "transient" => (TRANSIENT, 503),
        _ => (RECONNECT, 401),
    };
    Fail {
        status,
        message: message.into(),
        code: Some(code),
    }
}

fn now_ms() -> i64 {
    identity::now_ms()
}

// ---------------------------------------------------------------------------------------------
// JWT claims and the token answer (display and routing only; the token came from OpenAI over TLS).

/// core `jwtClaims`: the middle part, base64url, as a JSON object.
pub fn jwt_claims(token: &str) -> Option<JValue> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    // Buffer.from(_, 'base64url') skips characters outside the alphabet and padding.
    let clean: String = parts
        .get(1)?
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(clean.trim_end_matches('='))
        .ok()?;
    match wtf16::parse(&String::from_utf8_lossy(&bytes)) {
        Ok(v @ JValue::Obj(_)) => Some(v),
        _ => None,
    }
}

fn nonempty_str(v: &JValue) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// core `accountIdFrom`.
pub fn account_id_from(token: Option<&str>) -> Option<String> {
    let claims = jwt_claims(token?)?;
    let auth = claims.get("https://api.openai.com/auth");
    if auth.is_object() {
        if let Some(id) = nonempty_str(auth.get("chatgpt_account_id")) {
            return Some(id);
        }
    }
    if let Some(id) = nonempty_str(claims.get("chatgpt_account_id")) {
        return Some(id);
    }
    match claims.get("organizations") {
        JValue::Arr(orgs) => match orgs.first() {
            Some(first @ JValue::Obj(_)) => nonempty_str(first.get("id")),
            _ => None,
        },
        _ => None,
    }
}

fn is_fed_ramp(token: Option<&str>) -> bool {
    token
        .and_then(jwt_claims)
        .map(|c| {
            let auth = c.get("https://api.openai.com/auth");
            auth.is_object() && auth.get("chatgpt_account_is_fedramp").as_bool() == Some(true)
        })
        .unwrap_or(false)
}

/// The stored token JSON (core `tokensFrom` plus `connectedAt`), in core's key order.
#[derive(Clone)]
pub struct Tokens {
    pub access: Zeroizing<String>,
    pub refresh: Option<Zeroizing<String>>,
    pub account_id: String,
    pub fed_ramp: bool,
    pub expires_at: Option<f64>,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub connected_at: Option<f64>,
}

impl Tokens {
    fn to_json(&self) -> Zeroizing<String> {
        let opt = |v: &Option<String>| v.clone().map(JValue::Str).unwrap_or(JValue::Null);
        let num = |v: Option<f64>| v.map(JValue::Num).unwrap_or(JValue::Null);
        let mut pairs = vec![
            ("accessToken", JValue::Str(self.access.to_string())),
            (
                "refreshToken",
                self.refresh
                    .as_ref()
                    .map(|r| JValue::Str(r.to_string()))
                    .unwrap_or(JValue::Null),
            ),
            ("accountId", JValue::Str(self.account_id.clone())),
            ("fedRamp", JValue::Bool(self.fed_ramp)),
            ("expiresAt", num(self.expires_at)),
            ("email", opt(&self.email)),
            ("plan", opt(&self.plan)),
        ];
        if let Some(at) = self.connected_at {
            pairs.push(("connectedAt", JValue::Num(at)));
        }
        Zeroizing::new(wtf16::stringify(&JValue::obj(pairs)).unwrap_or_default())
    }

    /// The stored JSON as core reads it: `accessToken` must be a string (else `reconnect`).
    fn from_json(v: &JValue) -> Option<Tokens> {
        let access = v.get("accessToken").as_str()?.to_owned();
        let s = |k: &str| v.get(k).as_str().map(str::to_owned);
        Some(Tokens {
            access: Zeroizing::new(access),
            refresh: s("refreshToken")
                .filter(|r| !r.is_empty())
                .map(Zeroizing::new),
            account_id: match v.get("accountId") {
                JValue::Str(a) => a.clone(),
                JValue::Num(n) => js_json::number_to_string(*n),
                _ => String::new(),
            },
            fed_ramp: v.get("fedRamp").truthy(),
            expires_at: v.get("expiresAt").as_num(),
            email: s("email"),
            plan: s("plan"),
            connected_at: v.get("connectedAt").as_num(),
        })
    }
}

/// JS `Number(v)` for the token answer's `expires_in` (a number or a numeric string).
fn js_number(v: &JValue) -> Option<f64> {
    match v {
        JValue::Num(n) => Some(*n),
        JValue::Str(s) => {
            let t = s.trim();
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok()
            }
        }
        JValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        JValue::Null => Some(0.0),
        _ => None,
    }
}

/// core `tokensFrom(payload, { now, previous })`.
pub fn tokens_from(payload: &JValue, now: i64, previous: Option<&Tokens>) -> Result<Tokens, Fail> {
    if !payload.is_object() {
        return Err(fail("ChatGPT did not issue an access token.", 502));
    }
    let Some(access) = nonempty_str(payload.get("access_token")) else {
        return Err(fail("ChatGPT did not issue an access token.", 502));
    };
    let id_token = payload.get("id_token").as_str().map(str::to_owned);
    let account_id = account_id_from(id_token.as_deref())
        .or_else(|| account_id_from(Some(&access)))
        .or_else(|| {
            previous
                .map(|p| p.account_id.clone())
                .filter(|a| !a.is_empty())
        })
        .ok_or_else(|| fail("ChatGPT did not say which account signed in.", 502))?;
    let id_claims = id_token
        .as_deref()
        .and_then(jwt_claims)
        .unwrap_or(JValue::Obj(Vec::new()));
    let access_claims = jwt_claims(&access);
    let auth = {
        let a = id_claims.get("https://api.openai.com/auth");
        if a.is_object() {
            a.clone()
        } else {
            access_claims
                .as_ref()
                .map(|c| c.get("https://api.openai.com/auth").clone())
                .filter(JValue::is_object)
                .unwrap_or(JValue::Obj(Vec::new()))
        }
    };
    let exp = access_claims
        .as_ref()
        .and_then(|c| js_number(c.get("exp")))
        .filter(|e| e.is_finite() && *e > 0.0);
    let expires_in = js_number(payload.get("expires_in")).filter(|e| e.is_finite() && *e > 0.0);
    let expires_at = match (expires_in, exp) {
        (Some(s), _) => Some(now as f64 + s * 1000.0),
        (None, Some(e)) => Some(e * 1000.0),
        _ => None,
    };
    let refresh = nonempty_str(payload.get("refresh_token"))
        .map(Zeroizing::new)
        .or_else(|| previous.and_then(|p| p.refresh.clone()));
    let email = id_claims
        .get("email")
        .as_str()
        .map(str::to_owned)
        .or_else(|| previous.and_then(|p| p.email.clone()));
    let plan = auth
        .get("chatgpt_plan_type")
        .as_str()
        .map(|p| wtf16::slice(p, 40))
        .or_else(|| previous.and_then(|p| p.plan.clone()));
    let fed_ramp = is_fed_ramp(id_token.as_deref()) || is_fed_ramp(Some(&access));
    Ok(Tokens {
        access: Zeroizing::new(access),
        refresh,
        account_id,
        fed_ramp,
        expires_at,
        email,
        plan,
        connected_at: None,
    })
}

// ---------------------------------------------------------------------------------------------
// Outbound (core postJson and the catalogue fetch).

/// What an outbound exchange gave: core's `{ ok, status, body }` (body `None` when not JSON or
/// over the cap), or thrown (network, timeout, redirect).
struct Reply {
    status: u16,
    text: Option<String>,
}

impl Reply {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    fn json(&self) -> JValue {
        self.text
            .as_deref()
            .and_then(|t| wtf16::parse(t).ok())
            .unwrap_or(JValue::Null)
    }
}

/// A loopback http(s) URL, the only replacement accepted for Node's fixed hosts (QA servers).
fn loopback_url(raw: &str) -> Option<String> {
    let u = url::Url::parse(raw).ok()?;
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some()
    {
        return None;
    }
    let loopback = match u.host()? {
        url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    };
    loopback.then(|| raw.trim_end_matches('/').to_owned())
}

/// The broker's settings: Node's constants, the catalogue's client version
/// (NOEVIA_CHATGPT_CODEX_VERSION) and the loopback QA hosts.
#[derive(Debug, Clone)]
pub struct Settings {
    pub issuer: String,
    pub codex_base: String,
    pub client_version: String,
    pub user_agent: String,
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>, version: Option<&str>) -> Settings {
        let set = |k: &str| get(k).filter(|v| !v.is_empty());
        Settings {
            issuer: set("NOEVIA_QA_CHATGPT_ISSUER")
                .and_then(|v| loopback_url(&v))
                .unwrap_or_else(|| ISSUER.into()),
            codex_base: set("NOEVIA_QA_CHATGPT_CODEX_BASE")
                .and_then(|v| loopback_url(&v))
                .unwrap_or_else(|| CODEX_BASE.into()),
            client_version: set("NOEVIA_CHATGPT_CODEX_VERSION")
                .unwrap_or_else(|| CODEX_VERSION.into()),
            // core USER_AGENT: noevia/<package version> (chatgpt-oauth; +https://github.com/sbstndalton/noevia)
            user_agent: format!(
                "noevia/{} (chatgpt-oauth; +https://github.com/sbstndalton/noevia)",
                version.filter(|v| !v.is_empty()).unwrap_or("0")
            ),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The broker's memory.

struct Pending {
    user: String,
    device_auth_id: String,
    user_code: String,
    interval: i64,
    expires: i64,
    next_poll_at: i64,
    polling: bool,
    cancelled: bool,
    generation: u64,
}

/// One catalogue model (core `fetchCatalogue`'s projection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub slug: String,
    pub effort: Option<String>,
    pub summary: Option<String>,
    pub summary_param: bool,
    pub levels: Option<Vec<String>>,
}

impl Model {
    fn to_js(&self) -> JValue {
        let opt = |v: &Option<String>| v.clone().map(JValue::Str).unwrap_or(JValue::Null);
        JValue::obj([
            ("slug", JValue::Str(self.slug.clone())),
            ("effort", opt(&self.effort)),
            ("summary", opt(&self.summary)),
            ("summaryParam", JValue::Bool(self.summary_param)),
            (
                "levels",
                self.levels
                    .as_ref()
                    .map(|l| JValue::Arr(l.iter().cloned().map(JValue::Str).collect()))
                    .unwrap_or(JValue::Null),
            ),
        ])
    }
}

type Listed = Result<(Arc<Vec<Model>>, i64), Fail>;

enum Cached {
    Good {
        until: i64,
        ids: Arc<Vec<Model>>,
        fetched_at: i64,
    },
    Failed {
        until: i64,
        error: String,
    },
}

impl Cached {
    fn until(&self) -> i64 {
        match self {
            Cached::Good { until, .. } | Cached::Failed { until, .. } => *until,
        }
    }
}

#[derive(Default)]
struct Mem {
    pending: HashMap<String, Pending>,
    starts: HashMap<String, Vec<i64>>,
    generations: HashMap<String, u64>,
    epochs: HashMap<String, u64>,
    models: HashMap<String, Cached>,
    refreshes: HashMap<String, i64>,
    /// The last finished catalogue fetch per account: (sequence, epoch, result), so callers that
    /// waited on the fetch lock share it as core's callers share one in-flight promise.
    fetched: HashMap<String, (u64, u64, Listed)>,
    fetch_seq: u64,
    /// Accounts whose catalogue fetch is running (with its epoch).
    flying: HashMap<String, u64>,
}

impl Mem {
    fn generation(&self, user: &str) -> u64 {
        self.generations.get(user).copied().unwrap_or(0)
    }
    fn epoch(&self, user: &str) -> u64 {
        self.epochs.get(user).copied().unwrap_or(0)
    }
    fn reset_catalogue(&mut self, user: &str) {
        let next = self.epoch(user) + 1;
        self.epochs.insert(user.to_owned(), next);
        self.models.remove(user);
        self.refreshes.remove(user);
        self.fetched.remove(user);
    }
    fn prune(&mut self, now: i64) {
        self.pending.retain(|_, p| p.expires >= now);
        self.starts.retain(|_, v| {
            v.retain(|at| *at > now - START_WINDOW_MS);
            !v.is_empty()
        });
        self.models.retain(|_, c| c.until() > now - MODEL_CACHE_MS);
    }
    fn drop_pending(&mut self, user: &str) {
        self.pending.retain(|_, p| p.user != user);
    }
}

/// The broker: the front's one owner of Sign in with ChatGPT state.
pub struct Broker {
    settings: Settings,
    client: Option<reqwest::Client>,
    mem: Mutex<Mem>,
    /// Per-account refresh lock (single flight: one refresh per account at a time).
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The connect/disconnect fence: a sign-in's "still wanted?" check and its save, and a
    /// disconnect's generation bump and delete, are each one step (held only around the database
    /// statement, never across a network call, so a disconnect never waits for a refresh).
    fence: Mutex<()>,
    /// Per-account catalogue fetch lock (one fetch in flight per account).
    fetch_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    data_dir: PathBuf,
    feature_env: Option<bool>,
    previous_key_env: Option<Zeroizing<String>>,
    keys: std::sync::OnceLock<Arc<SecretKeys>>,
}

fn lock_for(
    map: &Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    user: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    let mut m = map.lock().unwrap_or_else(|e| e.into_inner());
    // Drop the locks nobody holds or waits on, so the map stays bounded by the active accounts.
    m.retain(|_, l| Arc::strong_count(l) > 1);
    Arc::clone(
        m.entry(user.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

/// What one stored row is: its state, the ciphertext and the opened tokens (`None`: unreadable).
struct Row {
    state: String,
    raw: String,
    tokens: Option<Tokens>,
}

/// A digest of a row's ciphertext: what Node names a sign-in by (never the ciphertext itself).
pub fn generation_of(raw: &str) -> String {
    Sha256::digest(raw.as_bytes())
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    OsRng.fill_bytes(&mut out);
    out
}

/// JS `parseInt(String(v), 10)` (NaN as `None`).
fn parse_int(v: &JValue) -> Option<i64> {
    let s = match v {
        JValue::Undefined | JValue::Null => "5".to_owned(),
        JValue::Num(n) => js_json::number_to_string(*n),
        JValue::Str(s) => s.clone(),
        JValue::Bool(b) => b.to_string(),
        _ => return None,
    };
    let t = s.trim_start();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let d: String = digits.chars().take_while(char::is_ascii_digit).collect();
    if d.is_empty() {
        return None;
    }
    let n: i64 = d.get(..d.len().min(15))?.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// A word core's catalogue keeps: `^[a-z_]{1,20}$`.
fn word(v: &JValue) -> Option<String> {
    let s = v.as_str()?;
    ((1..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
        .then(|| s.to_owned())
}

/// core `knownError` for the catalogue's error answers.
fn known_error(e: &JValue, now: i64) -> Option<String> {
    if !e.is_object() {
        return None;
    }
    let ty = match (e.get("type"), e.get("code")) {
        (t, _) if t.truthy() => js_json::to_js_string(t).unwrap_or_default(),
        (_, c) if c.truthy() => js_json::to_js_string(c).unwrap_or_default(),
        _ => String::new(),
    };
    let message = e
        .get("message")
        .as_str()
        .or_else(|| e.get("detail").as_str())
        .unwrap_or("");
    if ty == "usage_limit_reached" || message.to_ascii_lowercase().contains("usage limit") {
        let resets_at = js_number(e.get("resets_at")).filter(|n| *n > 0.0);
        let resets_in = js_number(e.get("resets_in_seconds")).filter(|n| *n > 0.0);
        let at = resets_at
            .map(|s| s * 1000.0)
            .or_else(|| resets_in.map(|s| now as f64 + s * 1000.0));
        let plan = e
            .get("plan_type")
            .as_str()
            .filter(|p| {
                (1..=20).contains(&p.len())
                    && p.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_')
            })
            .map(|p| {
                let mut c = p.chars();
                let first = c.next().map(|f| f.to_ascii_uppercase()).unwrap_or_default();
                format!(" on your {first}{} plan", c.as_str())
            })
            .unwrap_or_default();
        let when = at
            .and_then(|ms| iso_minute(ms as i64))
            .map(|s| format!(" It resets at {s} UTC."))
            .unwrap_or_default();
        return Some(format!(
            "ChatGPT usage limit reached{plan}.{when} Pick another provider in the model popup until then."
        ));
    }
    if ty == "usage_not_included" {
        return Some(
            "Your ChatGPT plan does not include the Codex usage that Sign in with ChatGPT relies on."
                .into(),
        );
    }
    if [
        "insufficient_quota",
        "credit_balance_exhausted",
        "organization_usage_limit_exceeded",
    ]
    .contains(&ty.as_str())
    {
        return Some("Your ChatGPT account has no usage quota left.".into());
    }
    None
}

/// `new Date(ms).toISOString().slice(0, 16).replace('T', ' ')`.
fn iso_minute(ms: i64) -> Option<String> {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil_from_days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    if !(0..=9999).contains(&y) {
        return None;
    }
    Some(format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    ))
}

fn error_object(v: &JValue) -> Option<JValue> {
    let inner = v.get("error");
    if v.is_object() && inner.is_object() {
        let mut pairs = match v {
            JValue::Obj(p) => p.clone(),
            _ => Vec::new(),
        };
        if let JValue::Obj(extra) = inner {
            for (k, val) in extra {
                wtf16::set(&mut pairs, k, val.clone());
            }
        }
        return Some(JValue::Obj(pairs));
    }
    v.is_object().then(|| v.clone())
}

/// core `upstreamMessage(text, status)`.
fn upstream_message(text: &str, status: u16, now: i64) -> String {
    let parsed = wtf16::parse(text).unwrap_or(JValue::Undefined);
    let e = error_object(&parsed);
    let known = e.as_ref().and_then(|e| known_error(e, now)).or_else(|| {
        (e.is_none() && text.to_ascii_lowercase().contains("usage limit")).then(|| {
            known_error(
                &JValue::obj([("type", JValue::Str("usage_limit_reached".into()))]),
                now,
            )
        })?
    });
    if let Some(k) = known {
        return wtf16::slice(&k, 300);
    }
    let detail = e
        .as_ref()
        .map(|e| {
            e.get("detail")
                .as_str()
                .or_else(|| e.get("message").as_str())
                .unwrap_or("")
                .to_owned()
        })
        .unwrap_or_default();
    let tail = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    if status == 429 {
        return wtf16::slice(
            &format!("ChatGPT is rate-limiting this account{tail}. Try again shortly."),
            300,
        );
    }
    wtf16::slice(&format!("ChatGPT returned {status}{tail}"), 300)
}

impl Broker {
    pub fn new(config: &crate::config::Config, version: Option<&str>) -> Option<Broker> {
        let status = config.rust_chatgpt_status.as_ref()?;
        let data_dir = config.data_dir.clone()?;
        let mut settings = config
            .chatgpt_oauth
            .clone()
            .unwrap_or_else(|| Settings::from_lookup(&|_: &str| None, version));
        if let Some(v) = version.filter(|v| !v.is_empty()) {
            settings.user_agent =
                format!("noevia/{v} (chatgpt-oauth; +https://github.com/sbstndalton/noevia)");
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(settings.user_agent.clone())
            .build()
            .ok();
        Some(Broker {
            settings,
            client,
            mem: Mutex::new(Mem::default()),
            locks: Mutex::new(HashMap::new()),
            fence: Mutex::new(()),
            fetch_locks: Mutex::new(HashMap::new()),
            data_dir,
            feature_env: status.feature_env,
            previous_key_env: status.previous_key_env.0.clone().map(Zeroizing::new),
            keys: std::sync::OnceLock::new(),
        })
    }

    fn mem(&self) -> std::sync::MutexGuard<'_, Mem> {
        self.mem.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The key snapshot (core createSecretStore loads its keys once at start).
    fn keys(&self) -> Option<Arc<SecretKeys>> {
        if let Some(k) = self.keys.get() {
            return Some(Arc::clone(k));
        }
        let loaded = server_auth::secrets::load_existing(
            &self.data_dir,
            self.previous_key_env.as_deref().map(String::as_str),
        )
        .ok()?;
        Some(Arc::clone(self.keys.get_or_init(|| Arc::new(loaded))))
    }

    /// Tells Node a token row changed (its cache re-reads on this file's signal).
    fn signal(&self) {
        let doc = JValue::obj([
            ("version", JValue::Num(1.0)),
            ("at", JValue::Num(now_ms() as f64)),
            (
                "nonce",
                JValue::Str(random::<8>().iter().map(|b| format!("{b:02x}")).collect()),
            ),
        ]);
        let _ = write_atomic(
            &self.data_dir.join(SIGNAL_FILE),
            &js_json::stringify(&doc).unwrap_or_default(),
        );
    }

    async fn send(&self, request: reqwest::RequestBuilder, cap: usize) -> Result<Reply, ()> {
        let exchange = async {
            let mut response = request.send().await.map_err(|_| ())?;
            let status = response.status().as_u16();
            // undici's redirect: 'error' throws on a redirect answer.
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Err(());
            }
            let mut body = Vec::new();
            let mut over = false;
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        if body.len() + chunk.len() > cap {
                            over = true;
                            break;
                        }
                        body.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    // A body cut off mid-way: core's readCappedText rejects, read as ''.
                    Err(_) => {
                        over = true;
                        break;
                    }
                }
            }
            Ok(Reply {
                status,
                text: (!over).then(|| String::from_utf8_lossy(&body).into_owned()),
            })
        };
        tokio::time::timeout(TIMEOUT, exchange)
            .await
            .unwrap_or(Err(()))
    }

    /// core `postJson(url, body, { form })`.
    async fn post_json(&self, url: &str, body: &[(&str, &str)], form: bool) -> Result<Reply, ()> {
        let client = self.client.as_ref().ok_or(())?;
        let text = if form {
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(body.iter())
                .finish()
        } else {
            js_json::stringify(&JValue::obj(
                body.iter().map(|(k, v)| (*k, JValue::Str((*v).to_owned()))),
            ))
            .unwrap_or_default()
        };
        let request = client
            .post(url)
            .header(
                "content-type",
                if form {
                    "application/x-www-form-urlencoded"
                } else {
                    "application/json"
                },
            )
            .header("accept", "application/json")
            .body(text);
        self.send(request, TOKEN_REPLY_CAP).await
    }

    // ── the token store (blocking; called on the blocking pool) ──

    fn read_row(&self, app: &App, user: &str) -> Result<Option<Row>, Fail> {
        let writer = app.writes.writer().map_err(|_| auth_error("transient"))?;
        let row: Option<(String, String)> = writer
            .read(|r| {
                r.row(
                    "SELECT data_enc, state FROM chatgpt_oauth_tokens WHERE user_id=?1",
                    [user],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .map_err(|_| auth_error("transient"))?;
        let Some((raw, state)) = row else {
            return Ok(None);
        };
        let tokens = self.keys().and_then(|keys| {
            let (plain, _) = keys.open(&raw, Some(user)).ok()?;
            // Not JSON, not an object, or no string accessToken: core's `reconnect`.
            match wtf16::parse(&plain) {
                Ok(v @ JValue::Obj(_)) => Tokens::from_json(&v),
                _ => None,
            }
        });
        Ok(Some(Row { state, raw, tokens }))
    }

    fn audit(&self, app: &App, user: &str, action: &str, detail: &JValue) {
        let detail = js_json::stringify(detail).unwrap_or_else(|| "{}".into());
        if let Ok(w) = app.writes.writer() {
            let _ = w.write(|tx| {
                tx.execute(
                    "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)",
                    server_store::rusqlite::params![user, action, detail, now_ms()],
                )
            });
        }
    }

    fn seal(&self, user: &str, tokens: &Tokens) -> Result<String, Fail> {
        let keys = self.keys().ok_or_else(|| auth_error("transient"))?;
        keys.seal(&tokens.to_json(), Some(user), &random::<12>())
            .map_err(|_| auth_error("transient"))
    }

    fn save(&self, app: &App, user: &str, tokens: &Tokens) -> Result<(), Fail> {
        let sealed = self.seal(user, tokens)?;
        let writer = app.writes.writer().map_err(|_| auth_error("transient"))?;
        writer
            .write(|tx| {
                tx.save_chatgpt_tokens(
                    RustChatgptOAuth::owned(),
                    user,
                    &sealed,
                    "connected",
                    now_ms(),
                )
            })
            .map_err(|_| auth_error("transient"))?;
        self.signal();
        Ok(())
    }

    fn mark_reconnect(&self, app: &App, user: &str, raw: Option<&str>) -> bool {
        let Ok(writer) = app.writes.writer() else {
            return false;
        };
        let changed = writer
            .write(|tx| tx.mark_chatgpt_reconnect(RustChatgptOAuth::owned(), user, raw, now_ms()))
            .unwrap_or(0);
        if changed > 0 {
            self.signal();
            self.audit(
                app,
                user,
                "chatgpt.oauth.reconnect_needed",
                &JValue::obj::<&str>([]),
            );
        }
        changed > 0
    }

    fn delete(&self, app: &App, user: &str) -> Result<(), Fail> {
        let writer = app.writes.writer().map_err(|_| auth_error("transient"))?;
        let deleted = writer
            .write(|tx| tx.delete_chatgpt_tokens(RustChatgptOAuth::owned(), user))
            .map_err(|_| auth_error("transient"))?;
        // Only a change moves Node's signal (an account that never signed in leaves no file).
        if deleted > 0 {
            self.signal();
        }
        Ok(())
    }

    /// The `chatgptOAuth` flag, read per request.
    fn flag(&self, app: &App) -> Result<bool, ()> {
        let writer = app.writes.writer().map_err(|_| ())?;
        writer
            .read(|r| server_chatgpt_status::enabled(r, self.feature_env))
            .map_err(|_| ())
    }

    /// The `chatgptOAuth` flag (read on the blocking pool), for the provider model list.
    pub async fn enabled(&self, app: &Arc<App>) -> bool {
        let a = Arc::clone(app);
        tokio::task::spawn_blocking(move || {
            a.chatgpt_oauth
                .as_ref()
                .is_some_and(|b| b.flag(&a) == Ok(true))
        })
        .await
        .unwrap_or(false)
    }

    /// Whether the account has a token row (any state): core then lists its ChatGPT provider row.
    pub async fn has_tokens(&self, app: &Arc<App>, user: &str) -> bool {
        matches!(self.row(app, user).await, Ok(Some(_)))
    }

    // ── device-code sign-in ──

    pub async fn start(&self, app: &Arc<App>, user: &str) -> Result<JValue, Fail> {
        let now = now_ms();
        {
            let mut m = self.mem();
            m.prune(now);
            let recent: Vec<i64> = m
                .starts
                .get(user)
                .map(|v| {
                    v.iter()
                        .copied()
                        .filter(|at| *at > now - START_WINDOW_MS)
                        .collect()
                })
                .unwrap_or_default();
            if recent.len() >= MAX_STARTS_PER_WINDOW {
                return Err(fail(
                    "Too many ChatGPT sign-in attempts. Wait a few minutes and try again.",
                    429,
                ));
            }
            let mut next = recent;
            next.push(now);
            m.starts.insert(user.to_owned(), next);
            let mut own: Vec<(i64, String)> = m
                .pending
                .iter()
                .filter(|(_, p)| p.user == user)
                .map(|(k, p)| (p.expires, k.clone()))
                .collect();
            // Oldest first (Map insertion order in core; the expiry is set at insertion).
            own.sort();
            while own.len() >= MAX_PENDING_PER_USER {
                let (_, k) = own.remove(0);
                m.pending.remove(&k);
            }
        }
        let r = self
            .post_json(
                &format!("{}/api/accounts/deviceauth/usercode", self.settings.issuer),
                &[("client_id", CLIENT_ID)],
                false,
            )
            .await
            .map_err(|_| fail("ChatGPT sign-in could not be reached. Try again.", 502))?;
        if r.status == 404 {
            return Err(fail(
                "ChatGPT device sign-in is not available right now.",
                502,
            ));
        }
        let body = r.json();
        let device_auth_id = body.get("device_auth_id").as_str().map(str::to_owned);
        let user_code = match body.get("user_code") {
            JValue::Undefined | JValue::Null => body.get("usercode"),
            v => v,
        }
        .as_str()
        .map(str::to_owned);
        let (Some(device_auth_id), Some(user_code)) = (device_auth_id, user_code) else {
            return Err(fail(
                format!("ChatGPT sign-in could not start ({}).", r.status),
                502,
            ));
        };
        if !r.ok()
            || device_auth_id.is_empty()
            || user_code.is_empty()
            || wtf16::len(&device_auth_id) > 512
            || wtf16::len(&user_code) > 64
        {
            return Err(fail(
                format!("ChatGPT sign-in could not start ({}).", r.status),
                502,
            ));
        }
        let interval = parse_int(body.get("interval"))
            .filter(|n| *n != 0)
            .unwrap_or(5)
            .clamp(1, 30);
        use base64::Engine as _;
        let login_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random::<18>());
        let now = now_ms();
        let expires = now + DEVICE_TTL_MS;
        {
            let mut m = self.mem();
            let generation = m.generation(user);
            m.pending.insert(
                login_id.clone(),
                Pending {
                    user: user.to_owned(),
                    device_auth_id,
                    user_code: user_code.clone(),
                    interval,
                    expires,
                    next_poll_at: now + interval * 1000,
                    polling: false,
                    cancelled: false,
                    generation,
                },
            );
        }
        let (app2, user2) = (Arc::clone(app), user.to_owned());
        let _ = tokio::task::spawn_blocking(move || {
            if let Some(b) = app2.chatgpt_oauth.as_ref() {
                b.audit(
                    &app2,
                    &user2,
                    "chatgpt.oauth.start",
                    &JValue::obj::<&str>([]),
                );
            }
        })
        .await;
        Ok(JValue::obj([
            ("loginId", JValue::Str(login_id)),
            ("userCode", JValue::Str(user_code)),
            (
                "verificationUrl",
                JValue::Str(format!("{}/codex/device", self.settings.issuer)),
            ),
            ("interval", JValue::Num(interval as f64)),
            ("expiresAt", JValue::Num(expires as f64)),
        ]))
    }

    fn abandoned(&self, login_id: &str, user: &str, generation: u64) -> bool {
        let m = self.mem();
        let cancelled = m.pending.get(login_id).map(|p| p.cancelled).unwrap_or(true);
        cancelled || m.generation(user) != generation
    }

    pub async fn poll(&self, app: &Arc<App>, user: &str, login_id: &str) -> Result<JValue, Fail> {
        let state = |s: &str| JValue::obj([("state", JValue::Str(s.into()))]);
        let pending = |interval: i64| {
            JValue::obj([
                ("state", JValue::Str("pending".into())),
                ("interval", JValue::Num(interval as f64)),
            ])
        };
        let (device_auth_id, user_code, interval, generation) = {
            let mut m = self.mem();
            let now = now_ms();
            let Some(p) = m.pending.get_mut(login_id) else {
                return Ok(state("expired"));
            };
            if p.user != user {
                return Err(fail(
                    "This sign-in was started by a different account.",
                    403,
                ));
            }
            if p.expires < now {
                m.pending.remove(login_id);
                return Ok(state("expired"));
            }
            if p.polling || now < p.next_poll_at {
                return Ok(pending(p.interval));
            }
            p.polling = true;
            p.next_poll_at = now + p.interval * 1000;
            (
                p.device_auth_id.clone(),
                p.user_code.clone(),
                p.interval,
                p.generation,
            )
        };
        let out = self
            .poll_once(
                app,
                user,
                login_id,
                &device_auth_id,
                &user_code,
                interval,
                generation,
            )
            .await;
        if let Some(p) = self.mem().pending.get_mut(login_id) {
            p.polling = false;
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    async fn poll_once(
        &self,
        app: &Arc<App>,
        user: &str,
        login_id: &str,
        device_auth_id: &str,
        user_code: &str,
        interval: i64,
        generation: u64,
    ) -> Result<JValue, Fail> {
        let state = |s: &str| JValue::obj([("state", JValue::Str(s.into()))]);
        let pending = || {
            JValue::obj([
                ("state", JValue::Str("pending".into())),
                ("interval", JValue::Num(interval as f64)),
            ])
        };
        let Ok(r) = self
            .post_json(
                &format!("{}/api/accounts/deviceauth/token", self.settings.issuer),
                &[("device_auth_id", device_auth_id), ("user_code", user_code)],
                false,
            )
            .await
        else {
            return Ok(pending());
        };
        if r.status == 403 || r.status == 404 {
            return Ok(pending());
        }
        let body = r.json();
        let code = nonempty_str(body.get("authorization_code"));
        let verifier = nonempty_str(body.get("code_verifier"));
        let (true, Some(code), Some(verifier)) = (r.ok(), code, verifier) else {
            self.mem().pending.remove(login_id);
            return Err(fail(
                format!("ChatGPT sign-in failed ({}). Start again.", r.status),
                502,
            ));
        };
        let code = Zeroizing::new(code);
        let verifier = Zeroizing::new(verifier);
        if self.abandoned(login_id, user, generation) {
            return Ok(state("cancelled"));
        }
        let exchanged = self.exchange(&code, &verifier).await;
        // The entry stayed pending through the exchange, so a cancel or disconnect during it wins.
        let mut tokens = match exchanged {
            Ok(t) => t,
            Err(e) => {
                self.mem().pending.remove(login_id);
                return Err(e);
            }
        };
        tokens.connected_at = Some(now_ms() as f64);
        let (a, u, t, l) = (
            Arc::clone(app),
            user.to_owned(),
            tokens.clone(),
            login_id.to_owned(),
        );
        let saved = tokio::task::spawn_blocking(move || {
            let b = a
                .chatgpt_oauth
                .as_ref()
                .ok_or_else(|| auth_error("transient"))?;
            // Checked and saved under the fence: a disconnect cannot land between the two.
            let fence = b.fence.lock().unwrap_or_else(|e| e.into_inner());
            let abandoned = b.abandoned(&l, &u, generation);
            b.mem().pending.remove(&l);
            if abandoned {
                drop(fence);
                b.audit(&a, &u, "chatgpt.oauth.cancelled", &JValue::obj::<&str>([]));
                return Ok::<bool, Fail>(false);
            }
            b.save(&a, &u, &t)?;
            drop(fence);
            let plan = server_chatgpt_status::public_plan(
                &t.plan.clone().map(JValue::Str).unwrap_or(JValue::Null),
            );
            b.audit(
                &a,
                &u,
                "chatgpt.oauth.connect",
                &JValue::obj(plan.map(|p| ("plan", JValue::Str(p)))),
            );
            Ok(true)
        })
        .await
        .unwrap_or_else(|_| Err(auth_error("transient")));
        if !saved? {
            return Ok(state("cancelled"));
        }
        self.mem().reset_catalogue(user);
        let email = server_chatgpt_status::mask_email(
            &tokens
                .email
                .clone()
                .map(JValue::Str)
                .unwrap_or(JValue::Null),
        );
        let plan = server_chatgpt_status::public_plan(
            &tokens.plan.clone().map(JValue::Str).unwrap_or(JValue::Null),
        );
        Ok(JValue::obj([
            ("state", JValue::Str("connected".into())),
            (
                "account",
                JValue::obj([
                    ("email", email.map(JValue::Str).unwrap_or(JValue::Null)),
                    ("plan", plan.map(JValue::Str).unwrap_or(JValue::Null)),
                ]),
            ),
        ]))
    }

    /// core `exchange(code, verifier)`: the authorization code for tokens (a form, as the CLI).
    async fn exchange(&self, code: &str, verifier: &str) -> Result<Tokens, Fail> {
        let redirect = format!("{}/deviceauth/callback", self.settings.issuer);
        let r = self
            .post_json(
                &format!("{}/oauth/token", self.settings.issuer),
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", &redirect),
                    ("client_id", CLIENT_ID),
                    ("code_verifier", verifier),
                ],
                true,
            )
            .await
            .map_err(|_| fail("ChatGPT sign-in could not be completed. Start again.", 502))?;
        if !r.ok() {
            return Err(fail(
                format!("ChatGPT sign-in was refused ({}). Start again.", r.status),
                502,
            ));
        }
        tokens_from(&r.json(), now_ms(), None)
    }

    pub fn cancel(&self, user: &str, login_id: &str) -> Result<bool, Fail> {
        let mut m = self.mem();
        let Some(p) = m.pending.get_mut(login_id) else {
            return Ok(false);
        };
        if p.user != user {
            return Err(fail(
                "This sign-in was started by a different account.",
                403,
            ));
        }
        p.cancelled = true;
        m.pending.remove(login_id);
        Ok(true)
    }

    /// core `disconnect` (and `forgetUser`, which also forgets the start rate limit).
    pub async fn disconnect(&self, app: &Arc<App>, user: &str, forget: bool) -> Result<(), Fail> {
        let (a, u) = (Arc::clone(app), user.to_owned());
        tokio::task::spawn_blocking(move || {
            let b = a
                .chatgpt_oauth
                .as_ref()
                .ok_or_else(|| auth_error("transient"))?;
            {
                // The bump and the delete are one step for a sign-in finishing at the same time; a
                // refresh in flight is not waited for: its compare-and-swap finds no row.
                let _fence = b.fence.lock().unwrap_or_else(|e| e.into_inner());
                {
                    let mut m = b.mem();
                    let next = m.generation(&u) + 1;
                    m.generations.insert(u.clone(), next);
                    m.drop_pending(&u);
                    m.reset_catalogue(&u);
                    if forget {
                        m.starts.remove(&u);
                    }
                }
                b.delete(&a, &u)?;
            }
            if !forget {
                b.audit(&a, &u, "chatgpt.oauth.disconnect", &JValue::obj::<&str>([]));
            }
            Ok::<(), Fail>(())
        })
        .await
        .unwrap_or_else(|_| Err(auth_error("transient")))
    }

    // ── tokens in use ──

    async fn row(&self, app: &Arc<App>, user: &str) -> Result<Option<Row>, Fail> {
        let (a, u) = (Arc::clone(app), user.to_owned());
        tokio::task::spawn_blocking(move || {
            a.chatgpt_oauth
                .as_ref()
                .ok_or_else(|| auth_error("transient"))?
                .read_row(&a, &u)
        })
        .await
        .unwrap_or_else(|_| Err(auth_error("transient")))
    }

    async fn mark(&self, app: &Arc<App>, user: &str, raw: Option<String>) -> bool {
        let (a, u) = (Arc::clone(app), user.to_owned());
        tokio::task::spawn_blocking(move || {
            a.chatgpt_oauth
                .as_ref()
                .map(|b| b.mark_reconnect(&a, &u, raw.as_deref()))
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }

    /// core `refresh(userId, row)`, single-flight per account: a waiter that finds the row changed
    /// since it read it uses the newer sign-in instead of spending a refresh token again.
    async fn refresh(
        &self,
        app: &Arc<App>,
        user: &str,
        start: Row,
    ) -> Result<(Tokens, String), Fail> {
        let lock = lock_for(&self.locks, user);
        let _held = lock.lock().await;
        let latest = self.row(app, user).await?;
        let row = match latest {
            Some(l) if l.raw != start.raw => {
                return match (l.state.as_str(), l.tokens) {
                    ("connected", Some(t)) => Ok((t, l.raw)),
                    _ => Err(auth_error("reconnect")),
                };
            }
            Some(l) => l,
            None => return Err(auth_error("disconnected")),
        };
        let current = row.tokens.clone().ok_or_else(|| auth_error("reconnect"))?;
        let Some(refresh_token) = current.refresh.clone() else {
            self.mark(app, user, Some(row.raw.clone())).await;
            return Err(auth_error("reconnect"));
        };
        let r = self
            .post_json(
                &format!("{}/oauth/token", self.settings.issuer),
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &refresh_token),
                    ("client_id", CLIENT_ID),
                ],
                false,
            )
            .await
            .map_err(|_| auth_error("transient"))?;
        if [400, 401, 403].contains(&r.status) {
            self.mark(app, user, Some(row.raw.clone())).await;
            // A re-sign-in that landed meanwhile is still good: use it rather than failing.
            if let Some(l) = self.row(app, user).await? {
                if l.raw != row.raw && l.state == "connected" {
                    if let Some(t) = l.tokens {
                        return Ok((t, l.raw));
                    }
                }
            }
            return Err(auth_error("reconnect"));
        }
        if !r.ok() {
            return Err(auth_error("transient"));
        }
        let mut next = tokens_from(&r.json(), now_ms(), Some(&current))
            .map_err(|_| auth_error("transient"))?;
        next.connected_at = Some(
            current
                .connected_at
                .filter(|c| *c != 0.0)
                .unwrap_or(now_ms() as f64),
        );
        let (a, u, n, prev) = (
            Arc::clone(app),
            user.to_owned(),
            next.clone(),
            row.raw.clone(),
        );
        let swapped = tokio::task::spawn_blocking(move || {
            let b = a
                .chatgpt_oauth
                .as_ref()
                .ok_or_else(|| auth_error("transient"))?;
            let sealed = b.seal(&u, &n)?;
            let writer = a.writes.writer().map_err(|_| auth_error("transient"))?;
            let kept = writer
                .write(|tx| {
                    tx.swap_chatgpt_tokens(RustChatgptOAuth::owned(), &u, &sealed, now_ms(), &prev)
                })
                .map_err(|_| auth_error("transient"))?;
            if kept > 0 {
                b.signal();
            }
            Ok::<Option<String>, Fail>((kept > 0).then_some(sealed))
        })
        .await
        .unwrap_or_else(|_| Err(auth_error("transient")))?;
        if let Some(raw) = swapped {
            return Ok((next, raw));
        }
        // A disconnect or a newer sign-in landed while the refresh was in flight: it wins.
        match self.row(app, user).await? {
            Some(l) if l.state == "connected" && l.tokens.is_some() => {
                Ok((l.tokens.unwrap_or(next), l.raw))
            }
            Some(_) => Err(auth_error("reconnect")),
            None => Err(auth_error("disconnected")),
        }
    }

    /// core `sessionRow(userId, { rejectedToken })`: usable tokens and the ciphertext they live in.
    pub async fn session(
        &self,
        app: &Arc<App>,
        user: &str,
        rejected: Option<&str>,
    ) -> Result<(Tokens, String), Fail> {
        let row = self
            .row(app, user)
            .await?
            .ok_or_else(|| auth_error("disconnected"))?;
        if row.state != "connected" {
            return Err(auth_error("reconnect"));
        }
        let t = row.tokens.clone().ok_or_else(|| auth_error("reconnect"))?;
        if let Some(rejected) = rejected {
            // Another request may already have refreshed; use that rather than burning a refresh.
            if t.access.as_str() != rejected {
                return Ok((t, row.raw));
            }
            return self.refresh(app, user, row).await;
        }
        let now = now_ms() as f64;
        if let Some(exp) = t.expires_at.filter(|e| *e != 0.0) {
            if exp - now <= REFRESH_MARGIN_MS {
                let raw = row.raw.clone();
                return match self.refresh(app, user, row).await {
                    Ok(v) => Ok(v),
                    // A soft failure inside the early-refresh window keeps the still-valid token.
                    Err(e) if e.code == Some("transient") && exp > now => Ok((t, raw)),
                    Err(e) => Err(e),
                };
            }
        }
        Ok((t, row.raw))
    }

    /// The second 401: marks the sign-in named by `generation` (a digest of its ciphertext).
    pub async fn reject(&self, app: &Arc<App>, user: &str, generation: &str) -> bool {
        let Ok(Some(row)) = self.row(app, user).await else {
            return false;
        };
        if generation_of(&row.raw) != generation {
            return false;
        }
        self.mark(app, user, Some(row.raw)).await
    }

    // ── the model catalogue ──

    /// One authorised catalogue fetch with the refresh-once-on-401 rule (core `authorised`).
    async fn fetch_catalogue(&self, app: &Arc<App>, user: &str, epoch: u64) -> Listed {
        let (t, _) = self.session(app, user, None).await?;
        let mut r = self.catalogue_request(&t).await;
        if matches!(&r, Ok(reply) if reply.status == 401) {
            let (t, raw) = self.session(app, user, Some(t.access.as_str())).await?;
            r = self.catalogue_request(&t).await;
            if matches!(&r, Ok(reply) if reply.status == 401) {
                // Only the sign-in that actually failed: a newer one is left alone (#934).
                self.mark(app, user, Some(raw)).await;
                return Err(fail(RECONNECT, 401));
            }
        }
        let current = |m: &Mem| m.epoch(user) == epoch;
        let remember = |error: String| -> Fail {
            let mut m = self.mem();
            let now = now_ms();
            let good =
                matches!(m.models.get(user), Some(Cached::Good { until, .. }) if *until > now);
            if current(&m) && !good {
                m.models.insert(
                    user.to_owned(),
                    Cached::Failed {
                        until: now + MODEL_FAILURE_CACHE_MS,
                        error: error.clone(),
                    },
                );
            }
            fail(error, 502)
        };
        // A network failure: core's fetch threw out of `authorised` (not a provider message).
        let reply = r.map_err(|_| fail("ChatGPT sign-in failed.", 502))?;
        let text = reply.text.clone().unwrap_or_default();
        if !reply.ok() {
            return Err(remember(upstream_message(&text, reply.status, now_ms())));
        }
        let parsed = wtf16::parse(&text).unwrap_or(JValue::Null);
        let mut ids = Vec::new();
        if let JValue::Arr(models) = parsed.get("models") {
            for m in models {
                if !m.is_object() {
                    continue;
                }
                let Some(slug) = nonempty_str(m.get("slug")) else {
                    continue;
                };
                if m.get("supported_in_api") == &JValue::Bool(false) {
                    continue;
                }
                match m.get("visibility") {
                    JValue::Undefined => {}
                    JValue::Str(v) if v == "list" => {}
                    _ => continue,
                }
                let levels = match m.get("supported_reasoning_levels") {
                    JValue::Arr(l) => Some(
                        l.iter()
                            .filter_map(|l| {
                                if l.is_object() {
                                    word(l.get("effort"))
                                } else {
                                    None
                                }
                            })
                            .take(12)
                            .collect(),
                    ),
                    _ => None,
                };
                ids.push(Model {
                    slug: wtf16::slice(&slug, 120),
                    effort: word(m.get("default_reasoning_level")),
                    summary: word(m.get("default_reasoning_summary")),
                    summary_param: m.get("supports_reasoning_summary_parameter")
                        != &JValue::Bool(false),
                    levels,
                });
                if ids.len() >= 50 {
                    break;
                }
            }
        }
        if ids.is_empty() {
            return Err(remember(
                "ChatGPT returned no models for this account.".into(),
            ));
        }
        let fetched_at = now_ms();
        let ids = Arc::new(ids);
        let mut m = self.mem();
        if current(&m) {
            m.models.insert(
                user.to_owned(),
                Cached::Good {
                    until: fetched_at + MODEL_CACHE_MS,
                    ids: Arc::clone(&ids),
                    fetched_at,
                },
            );
        }
        Ok((ids, fetched_at))
    }

    async fn catalogue_request(&self, t: &Tokens) -> Result<Reply, ()> {
        let client = self.client.as_ref().ok_or(())?;
        let url = format!(
            "{}/models?client_version={}",
            self.settings.codex_base,
            url::form_urlencoded::byte_serialize(self.settings.client_version.as_bytes())
                .collect::<String>()
                .replace('+', "%20")
        );
        let mut request = client
            .get(url)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("authorization", format!("Bearer {}", t.access.as_str()))
            .header("chatgpt-account-id", t.account_id.as_str());
        if t.fed_ramp {
            request = request.header("x-openai-fedramp", "true");
        }
        self.send(request, CATALOGUE_REPLY_CAP).await
    }

    /// core `catalogueEntry(userId, { refresh })`: `(ids, cached, fetchedAt, throttled)`.
    pub async fn catalogue(
        &self,
        app: &Arc<App>,
        user: &str,
        refresh: bool,
    ) -> Result<(Arc<Vec<Model>>, bool, i64, bool), Fail> {
        let (epoch, seen) = {
            let mut m = self.mem();
            let now = now_ms();
            let throttled = refresh
                && m.refreshes
                    .get(user)
                    .is_some_and(|at| now - *at < MODEL_REFRESH_GAP_MS);
            match m.models.get(user) {
                Some(c) if c.until() > now && (!refresh || throttled) => {
                    return match c {
                        Cached::Good {
                            ids, fetched_at, ..
                        } => Ok((Arc::clone(ids), true, *fetched_at, throttled)),
                        Cached::Failed { error, .. } => Err(fail(error.clone(), 502)),
                    };
                }
                _ => {}
            }
            let epoch = m.epoch(user);
            // Throttled with nothing usable cached: join the refresh still in flight, else refuse.
            if throttled && m.flying.get(user) != Some(&epoch) {
                return Err(fail(
                    "The ChatGPT model list was refreshed a moment ago. Try again in a few seconds.",
                    429,
                ));
            }
            if refresh && !throttled {
                m.refreshes.insert(user.to_owned(), now);
                if m.refreshes.len() > REFRESHES_MAX {
                    if let Some(oldest) = m
                        .refreshes
                        .iter()
                        .min_by_key(|(_, at)| **at)
                        .map(|(k, _)| k.clone())
                    {
                        m.refreshes.remove(&oldest);
                    }
                }
            }
            (epoch, m.fetch_seq)
        };
        let lock = lock_for(&self.fetch_locks, user);
        let _held = lock.lock().await;
        // A fetch that finished while this call waited is this call's answer (one request shared).
        {
            let m = self.mem();
            if let Some((seq, e, result)) = m.fetched.get(user) {
                if *seq > seen && *e == epoch {
                    return result.clone().map(|(ids, at)| (ids, false, at, false));
                }
            }
        }
        self.mem().flying.insert(user.to_owned(), epoch);
        let result = self.fetch_catalogue(app, user, epoch).await;
        let mut m = self.mem();
        m.flying.remove(user);
        m.fetch_seq += 1;
        let seq = m.fetch_seq;
        if m.epoch(user) == epoch {
            m.fetched
                .insert(user.to_owned(), (seq, epoch, result.clone()));
        }
        drop(m);
        result.map(|(ids, at)| (ids, false, at, false))
    }

    /// core `modelList(userId, { refresh })`: the picker's answer.
    pub async fn model_list(
        &self,
        app: &Arc<App>,
        user: &str,
        refresh: bool,
    ) -> Result<JValue, Fail> {
        let (ids, cached, fetched_at, throttled) = self.catalogue(app, user, refresh).await?;
        let mut pairs = vec![
            (
                "models",
                JValue::Arr(ids.iter().map(|m| JValue::Str(m.slug.clone())).collect()),
            ),
            ("cached", JValue::Bool(cached)),
            ("fetchedAt", JValue::Num(fetched_at as f64)),
        ];
        if throttled {
            pairs.push(("throttled", JValue::Bool(true)));
        }
        Ok(JValue::obj(pairs))
    }
}

fn write_atomic(file: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        u64::from_le_bytes(random::<8>())
    ));
    let tmp = PathBuf::from(tmp);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    out.write_all(text.as_bytes())?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(&tmp, file)
}

// ---------------------------------------------------------------------------------------------
// The public routes.

fn answer(status: u16, body: &JValue) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        js_json::stringify(body).unwrap_or_else(|| "{}".into()),
        true,
    )
}

/// routes/providers.cjs handleChatGpt's catch: `e.status < 600 ? e.status : 502`, `{ error }`.
fn failed(e: &Fail) -> Response<Body> {
    let status = if e.status > 0 && e.status < 600 {
        e.status
    } else {
        502
    };
    answer(
        status,
        &JValue::obj([("error", JValue::Str(e.message.clone()))]),
    )
}

/// core http.cjs readJson, as handleChatGpt reads it: `readJson(req).catch(() => ({}))`.
async fn read_object(mut body: Body) -> JValue {
    let read = async {
        let mut out = Vec::new();
        while let Some(frame) = body.frame().await {
            let Ok(frame) = frame else { return None };
            if let Ok(data) = frame.into_data() {
                if out.len() + data.len() > BODY_LIMIT {
                    return None;
                }
                out.extend_from_slice(&data);
            }
        }
        Some(out)
    };
    match tokio::time::timeout(BODY_DEADLINE, read).await {
        Ok(Some(bytes)) if !bytes.is_empty() => {
            js_json::parse(&String::from_utf8_lossy(&bytes)).unwrap_or(JValue::Obj(Vec::new()))
        }
        _ => JValue::Obj(Vec::new()),
    }
}

/// `String(body?.loginId || '')`.
fn login_id(body: &JValue) -> String {
    let v = body.get("loginId");
    if !v.truthy() {
        return String::new();
    }
    js_json::to_js_string(v).unwrap_or_default()
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let Some(broker) = app.chatgpt_oauth.as_ref().map(Arc::clone) else {
        return app.proxy.forward(conn.peer, req).await;
    };
    let method = req.method().clone();
    let op = match (method, path.as_str()) {
        (Method::POST, DEVICE) | (Method::POST, POLL) | (Method::POST, CANCEL) => path.clone(),
        (Method::GET, MODELS) => path.clone(),
        _ => return app.proxy.forward(conn.peer, req).await,
    };
    let refresh = req
        .uri()
        .query()
        .and_then(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .find(|(k, _)| k == "refresh")
                .map(|(_, v)| v == "1")
        })
        .unwrap_or(false);
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let now = now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || gate.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let user = id.user.id.clone();
    let (a, b) = (Arc::clone(app), Arc::clone(&broker));
    match tokio::task::spawn_blocking(move || b.flag(&a)).await {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            return answer(
                404,
                &JValue::obj([("error", JValue::Str(OFF_MESSAGE.into()))]),
            )
        }
        _ => {
            return answer(
                502,
                &JValue::obj([("error", JValue::Str("ChatGPT sign-in failed.".into()))]),
            )
        }
    }
    let result = match op.as_str() {
        DEVICE => broker.start(app, &user).await,
        POLL => {
            let body = read_object(body).await;
            broker.poll(app, &user, &login_id(&body)).await
        }
        CANCEL => {
            let body = read_object(body).await;
            broker
                .cancel(&user, &login_id(&body))
                .map(|ok| JValue::obj([("ok", JValue::Bool(ok))]))
        }
        _ => broker.model_list(app, &user, refresh).await,
    };
    match result {
        Ok(v) => answer(200, &v),
        Err(e) => failed(&e),
    }
}

// ---------------------------------------------------------------------------------------------
// The internal operations (project_store.rs routes them after its secret check).

fn internal(status: u16, body: &JValue) -> Response<Body> {
    let mut res = Response::new(Body::from(js_json::stringify(body).unwrap_or_default()));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    res.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("application/json"),
    );
    res.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    res
}

fn internal_fail(e: &Fail) -> Response<Body> {
    let mut pairs = vec![("error", JValue::Str(e.message.clone()))];
    if let Some(code) = e.code {
        pairs.push(("code", JValue::Str(code.into())));
    }
    pairs.push(("status", JValue::Num(f64::from(e.status))));
    let status = if (400..600).contains(&e.status) {
        e.status
    } else {
        502
    };
    internal(status, &JValue::obj(pairs))
}

/// `POST /v1/chatgpt/<op>` after the store secret was checked.
pub async fn internal_op(app: Option<Arc<App>>, op: &str, raw: &[u8]) -> Response<Body> {
    let Some(app) = app else {
        return internal(
            503,
            &JValue::obj([("error", JValue::Str("unavailable".into()))]),
        );
    };
    let Some(broker) = app.chatgpt_oauth.as_ref().map(Arc::clone) else {
        return internal(
            503,
            &JValue::obj([("error", JValue::Str("unavailable".into()))]),
        );
    };
    let body = match js_json::parse(&String::from_utf8_lossy(raw)) {
        Ok(b @ JValue::Obj(_)) => b,
        _ => {
            return internal(
                400,
                &JValue::obj([("error", JValue::Str("invalid JSON".into()))]),
            )
        }
    };
    let user = match body.get("user") {
        JValue::Str(u) if !u.is_empty() && u.len() <= 256 => u.clone(),
        _ => {
            return internal(
                400,
                &JValue::obj([("error", JValue::Str("invalid user".into()))]),
            )
        }
    };
    match op {
        "session" => {
            let rejected = body.get("rejected").as_str().map(str::to_owned);
            match broker.session(&app, &user, rejected.as_deref()).await {
                Ok((t, raw)) => internal(
                    200,
                    &JValue::obj([
                        ("accessToken", JValue::Str(t.access.to_string())),
                        ("accountId", JValue::Str(t.account_id.clone())),
                        ("fedRamp", JValue::Bool(t.fed_ramp)),
                        (
                            "expiresAt",
                            t.expires_at.map(JValue::Num).unwrap_or(JValue::Null),
                        ),
                        ("generation", JValue::Str(generation_of(&raw))),
                    ]),
                ),
                Err(e) => internal_fail(&e),
            }
        }
        "reject" => {
            let generation = body.get("generation").as_str().unwrap_or("").to_owned();
            let marked = broker.reject(&app, &user, &generation).await;
            internal(200, &JValue::obj([("marked", JValue::Bool(marked))]))
        }
        "catalogue" => match broker.catalogue(&app, &user, false).await {
            Ok((ids, ..)) => internal(
                200,
                &JValue::obj([(
                    "models",
                    JValue::Arr(ids.iter().map(Model::to_js).collect()),
                )]),
            ),
            Err(e) => internal_fail(&e),
        },
        "models" => {
            let refresh = body.get("refresh").as_bool() == Some(true);
            match broker.model_list(&app, &user, refresh).await {
                Ok(v) => internal(200, &v),
                Err(e) => internal_fail(&e),
            }
        }
        "disconnect" | "forget" => match broker.disconnect(&app, &user, op == "forget").await {
            Ok(()) => internal(200, &JValue::obj([("ok", JValue::Bool(true))])),
            Err(e) => internal_fail(&e),
        },
        "connected" => match broker.row(&app, &user).await {
            Ok(row) => internal(200, &JValue::obj([("exists", JValue::Bool(row.is_some()))])),
            Err(e) => internal_fail(&e),
        },
        _ => internal(
            404,
            &JValue::obj([("error", JValue::Str("not found".into()))]),
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn jwt(claims: &str) -> String {
        use base64::Engine as _;
        format!(
            "h.{}.s",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims)
        )
    }

    #[test]
    fn tokens_from_is_core_tokens_from() {
        let id = jwt(
            r#"{"email":"ada@example.test","https://api.openai.com/auth":{"chatgpt_account_id":"acct-1","chatgpt_plan_type":"plus","chatgpt_account_is_fedramp":true}}"#,
        );
        let payload = js_json::parse(&format!(
            r#"{{"access_token":"{}","refresh_token":"r1","id_token":"{id}","expires_in":"3600"}}"#,
            jwt(r#"{"exp":99}"#)
        ))
        .unwrap();
        let t = tokens_from(&payload, 1000, None).unwrap();
        assert_eq!(t.account_id, "acct-1");
        assert!(t.fed_ramp);
        assert_eq!(t.expires_at, Some(1000.0 + 3_600_000.0));
        assert_eq!(t.email.as_deref(), Some("ada@example.test"));
        assert_eq!(t.plan.as_deref(), Some("plus"));
        // A refresh without a new refresh token, id token or expires_in keeps the old ones.
        let p2 =
            js_json::parse(&format!(r#"{{"access_token":"{}"}}"#, jwt(r#"{"exp":99}"#))).unwrap();
        let t2 = tokens_from(&p2, 5, Some(&t)).unwrap();
        assert_eq!(t2.refresh.as_deref().map(String::as_str), Some("r1"));
        assert_eq!(t2.account_id, "acct-1");
        assert_eq!(t2.expires_at, Some(99_000.0));
        assert_eq!(t2.plan.as_deref(), Some("plus"));
        assert!(!t2.fed_ramp);
        assert_eq!(
            tokens_from(&js_json::parse("{}").unwrap(), 0, None)
                .err()
                .unwrap()
                .message,
            "ChatGPT did not issue an access token."
        );
        assert_eq!(
            tokens_from(&js_json::parse(r#"{"access_token":"x"}"#).unwrap(), 0, None)
                .err()
                .unwrap()
                .message,
            "ChatGPT did not say which account signed in."
        );
        // organizations[0].id as the last resort.
        assert_eq!(
            account_id_from(Some(&jwt(r#"{"organizations":[{"id":"org-9"}]}"#))).as_deref(),
            Some("org-9")
        );
    }

    #[test]
    fn stored_json_round_trips_in_core_key_order() {
        let t = Tokens {
            access: Zeroizing::new("a".into()),
            refresh: None,
            account_id: "acct".into(),
            fed_ramp: false,
            expires_at: None,
            email: None,
            plan: Some("team".into()),
            connected_at: Some(7.0),
        };
        assert_eq!(
            t.to_json().as_str(),
            r#"{"accessToken":"a","refreshToken":null,"accountId":"acct","fedRamp":false,"expiresAt":null,"email":null,"plan":"team","connectedAt":7}"#
        );
        let back = Tokens::from_json(&js_json::parse(&t.to_json()).unwrap()).unwrap();
        assert_eq!(back.plan.as_deref(), Some("team"));
        assert!(Tokens::from_json(&js_json::parse(r#"{"accessToken":1}"#).unwrap()).is_none());
    }

    #[test]
    fn upstream_messages_are_core_messages() {
        assert_eq!(upstream_message("", 500, 0), "ChatGPT returned 500");
        assert_eq!(
            upstream_message(r#"{"error":{"message":"nope"}}"#, 429, 0),
            "ChatGPT is rate-limiting this account: nope. Try again shortly."
        );
        assert_eq!(
            upstream_message(
                r#"{"error":{"type":"usage_limit_reached","plan_type":"plus","resets_at":1767225600}}"#,
                429,
                0
            ),
            "ChatGPT usage limit reached on your Plus plan. It resets at 2026-01-01 00:00 UTC. Pick another provider in the model popup until then."
        );
        assert_eq!(
            upstream_message(
                r#"{"detail":"insufficient_quota","code":"insufficient_quota"}"#,
                402,
                0
            ),
            "Your ChatGPT account has no usage quota left."
        );
    }

    #[test]
    fn small_js_helpers() {
        assert_eq!(parse_int(&JValue::Str("7s".into())), Some(7));
        assert_eq!(parse_int(&JValue::Null), Some(5));
        assert_eq!(parse_int(&JValue::Str("x".into())), None);
        assert_eq!(parse_int(&JValue::Num(0.0)), Some(0));
        assert_eq!(
            loopback_url("http://127.0.0.1:9/x/").as_deref(),
            Some("http://127.0.0.1:9/x")
        );
        assert_eq!(loopback_url("https://auth.example.test"), None);
        assert_eq!(loopback_url("http://u:p@127.0.0.1/"), None);
        assert_eq!(generation_of("abc").len(), 32);
    }

    #[test]
    fn settings_take_only_loopback_hosts_and_the_version_override() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let s = Settings::from_lookup(&env(&[]), Some("1.2.3"));
        assert_eq!(s.issuer, "https://auth.openai.com");
        assert_eq!(s.codex_base, "https://chatgpt.com/backend-api/codex");
        assert_eq!(s.client_version, "0.155.0");
        assert_eq!(
            s.user_agent,
            "noevia/1.2.3 (chatgpt-oauth; +https://github.com/sbstndalton/noevia)"
        );
        let s = Settings::from_lookup(
            &env(&[
                ("NOEVIA_QA_CHATGPT_ISSUER", "https://evil.example.test"),
                ("NOEVIA_QA_CHATGPT_CODEX_BASE", "http://127.0.0.1:9/codex/"),
                ("NOEVIA_CHATGPT_CODEX_VERSION", "0.199.2-fixture"),
            ]),
            None,
        );
        assert_eq!(
            s.issuer, "https://auth.openai.com",
            "a non-loopback override is ignored"
        );
        assert_eq!(s.codex_base, "http://127.0.0.1:9/codex");
        assert_eq!(s.client_version, "0.199.2-fixture");
        assert_eq!(
            s.user_agent,
            "noevia/0 (chatgpt-oauth; +https://github.com/sbstndalton/noevia)"
        );
    }
}
