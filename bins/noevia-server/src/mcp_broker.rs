//! The MCP credential broker (sbstndalton/noevia#2202), ported from core `mcp-oauth.cjs`,
//! `directory-mcp.cjs` (the per-account keys) and `routes/mcp-directory.cjs`. The Rust front is the
//! single owner of every MCP server credential an account holds:
//!
//! - the OAuth sign-ins (`mcp_oauth_tokens`, `enc:v2` bound to the owner) and the registered app of
//!   each server (`mcp_oauth_clients`, `enc:v1`): discovery, registration, PKCE authorisation, the
//!   callback, the single-flight refresh and the disconnect;
//! - each account's own API keys for a directory server (`directory_mcp_user_keys`, `enc:v2`).
//!
//! The rows keep core's tables, columns and sealed JSON shapes, so a rollback to a Core that owns
//! them reads what this wrote (new fields are additions core ignores). Node keeps the MCP client
//! (a chat's live session) and asks this broker over the internal store (`/v1/mcp/*`, mcp_routes.rs)
//! for the minimum one call needs: an access token, or the account's key headers. It caches neither
//! past the request, and never sees a refresh token.
//!
//! What Node needs synchronously per chat turn (is this account ready for this server?) is
//! published, never secret, in `mcp-credential-state.json`: rewritten atomically (0600) only when its
//! content changes, so Node's `createReread` on the file signal is also its "a credential moved"
//! signal. A file Node cannot read means nobody is ready (fails closed).
//!
//! Locking: one async mutex per (account, server) serializes the refresh, the re-seal and the
//! disconnect, and a waiter re-reads the row after taking it, so a rotated refresh token is never
//! spent twice; every write is a compare-and-swap on the ciphertext it started from. One mutex per
//! server serializes the app registration. Work a client could abandon (a sign-in's token exchange,
//! a refresh, a key check and store) runs on a spawned task that the handler awaits.
use crate::mcp_egress::{self, Egress};
use crate::{identity, App};
use js_json::JValue;
use rand_core::{OsRng, RngCore};
use secret_envelope::KeyUsed;
use server_auth::secrets::SecretKeys;
use server_store::RustMcpCredentials;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use zeroize::Zeroizing;

/// The published readiness (see the header).
pub const STATE_FILE: &str = "mcp-credential-state.json";

/// The published readiness when no account holds a credential.
const EMPTY_STATE: &str = r#"{"version":1,"oauth":{},"keys":{}}"#;

const STATE_TTL_MS: i64 = 10 * 60 * 1000;
const MAX_PENDING_PER_USER: usize = 20;
const MAX_PENDING: usize = 4000;
/// core `tokenFor`: a token inside this margin of its expiry is refreshed first.
const REFRESH_MARGIN_MS: i64 = 60_000;
/// core `fail(...)`'s default status.
const BAD_GATEWAY: u16 = 502;

const DISCONNECTED: &str = "This server needs you to sign in first: Plugins → Connected → Sign in.";
const RECONNECT: &str = "Your sign-in to this server expired or was revoked. Sign in again: Plugins → Connected → Sign in.";
const TRANSIENT: &str =
    "The sign-in to this server could not be refreshed right now. Try again in a moment.";
const NO_KEY: &str =
    "You have not added your own key for this server: Plugins → Connected → Add key.";

/// A failure: written for people, never a credential. `needs_client` is core's `needsClient`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fail {
    pub status: u16,
    pub message: String,
    pub code: Option<&'static str>,
    pub needs_client: bool,
    pub issuer: Option<String>,
}

pub fn fail(message: impl Into<String>, status: u16) -> Fail {
    Fail {
        status,
        message: message.into(),
        code: None,
        needs_client: false,
        issuer: None,
    }
}

/// `disconnected` (no sign-in or key), `reconnect` (one that no longer works), `transient`.
pub fn auth_error(code: &'static str) -> Fail {
    let (message, status) = match code {
        "disconnected" => (DISCONNECTED, 401),
        "transient" => (TRANSIENT, 503),
        _ => (RECONNECT, 401),
    };
    Fail {
        code: Some(code),
        ..fail(message, status)
    }
}

fn now_ms() -> i64 {
    identity::now_ms()
}

fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    OsRng.fill_bytes(&mut out);
    out
}

fn b64url(bytes: &[u8]) -> String {
    secret_envelope::encode_base64url(bytes)
}

/// A digest of a row's ciphertext: what Node names a sign-in by (never the ciphertext itself).
pub fn generation_of(raw: &str) -> String {
    Sha256::digest(raw.as_bytes())
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// JS `encodeURIComponent`.
fn uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn js_ws(c: char) -> bool {
    matches!(c, '\u{9}'..='\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn text(v: &JValue) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

// ---------------------------------------------------------------------------------------------
// The stored shapes.

/// One directory server (`directory_mcp_servers`), as the broker reads it.
#[derive(Debug, Clone)]
pub struct Server {
    pub id: String,
    pub title: String,
    pub url: String,
    pub added_by: Option<String>,
    pub oauth: bool,
    pub personal: bool,
    /// `declared_json` as stored (an array of `{ name, required, secret, description, template }`).
    pub declared: JValue,
}

/// A declared key header (core `declaredHeaders`).
#[derive(Debug, Clone)]
pub struct Declared {
    pub name: String,
    pub required: bool,
    pub template: Option<String>,
}

impl Server {
    pub fn declared_headers(&self) -> Vec<Declared> {
        let JValue::Arr(items) = &self.declared else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|h| {
                Some(Declared {
                    name: text(h.get("name"))?,
                    required: h.get("required").truthy(),
                    template: text(h.get("template")),
                })
            })
            .collect()
    }
}

/// core directory-mcp.cjs `checkHeaderValues`: one line, bounded, the declared names only.
pub fn check_header_values(
    declared: &[Declared],
    values: &JValue,
) -> Result<Vec<(String, String)>, Fail> {
    let mut out = Vec::new();
    for h in declared {
        let given = match values.get(&h.name) {
            JValue::Str(s) => s.trim_matches(js_ws).to_owned(),
            _ => String::new(),
        };
        if given.is_empty() {
            if h.required {
                return Err(fail(format!("Enter {}.", h.name), 400));
            }
            continue;
        }
        if js_json::js_len(&given) > 4096 || given.contains(['\r', '\n', '\0']) {
            return Err(fail(
                format!("{} must be a single line under 4 KB.", h.name),
                400,
            ));
        }
        let value = match &h.template {
            Some(t) => replace_first_placeholder(t, &given),
            None => given,
        };
        out.push((h.name.clone(), value));
    }
    Ok(out)
}

/// JS `template.replace(/\{[^}]+\}/, () => value)`.
fn replace_first_placeholder(template: &str, value: &str) -> String {
    let bytes = template.as_bytes();
    let mut open = 0;
    while let Some(start) = template
        .get(open..)
        .and_then(|r| r.find('{'))
        .map(|i| i + open)
    {
        let close = bytes
            .get(start + 1..)
            .and_then(|r| r.iter().position(|&b| b == b'}'))
            .map(|i| i + start + 1);
        match close {
            Some(end) if end > start + 1 => {
                return format!(
                    "{}{}{}",
                    template.get(..start).unwrap_or(""),
                    value,
                    template.get(end + 1..).unwrap_or("")
                );
            }
            Some(_) => open = start + 1,
            None => break,
        }
    }
    template.to_owned()
}

/// A registered app (core's `mcp_oauth_clients` JSON) with what its server's metadata said.
#[derive(Debug, Clone)]
pub struct Client {
    pub resource: String,
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub revocation_endpoint: Option<String>,
    pub secret_method: String,
    pub scopes: Vec<String>,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uri: String,
    pub token_auth: Option<String>,
    pub manual: bool,
}

impl Client {
    fn from_json(v: &JValue) -> Option<Client> {
        if !v.is_object() {
            return None;
        }
        Some(Client {
            resource: text(v.get("resource"))?,
            issuer: v.get("issuer").as_str().unwrap_or("").to_owned(),
            authorization_endpoint: text(v.get("authorizationEndpoint"))?,
            token_endpoint: text(v.get("tokenEndpoint"))?,
            registration_endpoint: text(v.get("registrationEndpoint")),
            revocation_endpoint: text(v.get("revocationEndpoint")),
            secret_method: text(v.get("secretMethod"))
                .unwrap_or_else(|| "client_secret_basic".into()),
            scopes: match v.get("scopes") {
                JValue::Arr(a) => a
                    .iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect(),
                _ => Vec::new(),
            },
            client_id: text(v.get("clientId"))?,
            client_secret: text(v.get("clientSecret")),
            redirect_uri: text(v.get("redirectUri"))?,
            token_auth: text(v.get("tokenAuth")),
            manual: v.get("manual").truthy(),
        })
    }

    /// Core's key order, then the additions.
    fn to_json(&self) -> Zeroizing<String> {
        let opt = |o: &Option<String>| o.clone().map(JValue::Str).unwrap_or(JValue::Null);
        let mut pairs = vec![
            ("resource", JValue::Str(self.resource.clone())),
            ("issuer", JValue::Str(self.issuer.clone())),
            (
                "authorizationEndpoint",
                JValue::Str(self.authorization_endpoint.clone()),
            ),
            ("tokenEndpoint", JValue::Str(self.token_endpoint.clone())),
            ("registrationEndpoint", opt(&self.registration_endpoint)),
            ("secretMethod", JValue::Str(self.secret_method.clone())),
            (
                "scopes",
                JValue::Arr(self.scopes.iter().cloned().map(JValue::Str).collect()),
            ),
            ("clientId", JValue::Str(self.client_id.clone())),
            ("clientSecret", opt(&self.client_secret)),
            ("redirectUri", JValue::Str(self.redirect_uri.clone())),
        ];
        if let Some(t) = &self.token_auth {
            pairs.push(("tokenAuth", JValue::Str(t.clone())));
        }
        if self.manual {
            pairs.push(("manual", JValue::Bool(true)));
        }
        if let Some(r) = &self.revocation_endpoint {
            pairs.push(("revocationEndpoint", JValue::Str(r.clone())));
        }
        Zeroizing::new(js_json::stringify(&JValue::obj(pairs)).unwrap_or_default())
    }

    /// core `tokenRequest`'s client authentication: a secret goes in the form for
    /// `client_secret_post`, else in a Basic header; an app without a secret sends neither.
    /// (core read `tokenAuth`, which only a hand-registered app had, so a registered app that was
    /// handed a secret was always sent Basic: the method its metadata chose is used now.)
    fn auth_method(&self) -> &str {
        self.token_auth.as_deref().unwrap_or(&self.secret_method)
    }
}

/// A sign-in's tokens (core's `mcp_oauth_tokens` JSON: `accessToken`, `refreshToken`, `expiresAt`).
#[derive(Clone)]
pub struct Tokens {
    pub access: Zeroizing<String>,
    pub refresh: Option<Zeroizing<String>>,
    pub expires_at: Option<f64>,
}

impl Tokens {
    fn from_json(v: &JValue) -> Option<Tokens> {
        if !v.is_object() {
            return None;
        }
        Some(Tokens {
            access: Zeroizing::new(text(v.get("accessToken"))?),
            refresh: text(v.get("refreshToken")).map(Zeroizing::new),
            expires_at: v
                .get("expiresAt")
                .as_num()
                .filter(|n| n.is_finite() && *n != 0.0),
        })
    }

    fn to_json(&self) -> Zeroizing<String> {
        let v = JValue::obj([
            ("accessToken", JValue::Str(self.access.to_string())),
            (
                "refreshToken",
                self.refresh
                    .as_ref()
                    .map(|r| JValue::Str(r.to_string()))
                    .unwrap_or(JValue::Null),
            ),
            (
                "expiresAt",
                self.expires_at.map(JValue::Num).unwrap_or(JValue::Null),
            ),
        ]);
        Zeroizing::new(js_json::stringify(&v).unwrap_or_default())
    }

    /// Inside the refresh margin of its expiry (a token with no expiry never is).
    fn expiring(&self, now: i64) -> bool {
        self.expires_at
            .is_some_and(|e| e - (now as f64) <= REFRESH_MARGIN_MS as f64)
    }

    fn expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|e| e <= now as f64)
    }

    /// Cannot be used or refreshed again: expired with no refresh token.
    fn dead(&self, now: i64) -> bool {
        self.refresh.is_none() && self.expired(now)
    }
}

/// An account's stored key for a server: `None` when it has no row, `Some(None)` for a row no key opens.
type StoredKey = Option<Vec<(String, String)>>;

/// What one stored sign-in row is: the ciphertext and the opened tokens (`None`: unreadable).
pub struct Row {
    pub raw: String,
    pub tokens: Option<Tokens>,
}

/// What a session call hands Node.
pub struct Issued {
    pub access: Zeroizing<String>,
    pub expires_at: Option<f64>,
    pub generation: String,
}

fn issued(row: &Row, t: &Tokens) -> Issued {
    Issued {
        access: t.access.clone(),
        expires_at: t.expires_at,
        generation: generation_of(&row.raw),
    }
}

struct Pending {
    user: String,
    server: String,
    verifier: Zeroizing<String>,
    expires: i64,
    purpose: String,
    redirect: String,
    issuer: String,
}

/// What `finish` reports.
pub struct Finished {
    pub purpose: String,
}

enum Refreshed {
    New(Tokens),
    /// The service said the grant is gone (`invalid_grant`).
    Rejected,
    Transient,
}

/// Settings a deployment gives the broker.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// `NOEVIA_QA_ALLOW_LOOPBACK_MCP=1` (never set in deployments): a synthetic MCP and OAuth server
    /// on `http://127.0.0.1`.
    pub qa_loopback: bool,
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Settings {
        Settings {
            qa_loopback: get("NOEVIA_QA_ALLOW_LOOPBACK_MCP").as_deref() == Some("1"),
        }
    }
}

type Locks = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

fn lock_for(map: &Locks, key: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut m = map.lock().unwrap_or_else(|e| e.into_inner());
    // Drop the locks nobody holds or waits on, so the map stays bounded by the active accounts.
    m.retain(|_, l| Arc::strong_count(l) > 1);
    Arc::clone(
        m.entry(key.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

fn pair_key(user: &str, server: &str) -> String {
    format!("{user}\u{0}{server}")
}

/// The broker: the front's one owner of the MCP server credentials.
pub struct Mcp {
    egress: Arc<Egress>,
    data_dir: PathBuf,
    public_origin_env: String,
    previous_key_env: Option<Zeroizing<String>>,
    keys: RwLock<Option<Arc<SecretKeys>>>,
    pending: Mutex<HashMap<String, Pending>>,
    /// Per (account, server): the refresh, the re-seal, the disconnect.
    locks: Locks,
    /// Per server: the app registration.
    client_locks: Locks,
    /// The last published readiness (serialises the publisher).
    published: Mutex<Option<String>>,
    swept: std::sync::atomic::AtomicBool,
    rpc_id: std::sync::atomic::AtomicI64,
}

impl Mcp {
    pub fn new(config: &crate::config::Config) -> Option<Mcp> {
        let data_dir = config.data_dir.clone()?;
        let settings = config.mcp.clone();
        Some(Mcp {
            egress: Egress::new(settings.qa_loopback)?,
            data_dir,
            public_origin_env: config
                .account
                .as_ref()
                .map(|a| a.public_origin.clone())
                .unwrap_or_default(),
            previous_key_env: config
                .rust_chatgpt_status
                .as_ref()
                .and_then(|s| s.previous_key_env.0.clone())
                .map(Zeroizing::new),
            keys: RwLock::new(None),
            pending: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            client_locks: Mutex::new(HashMap::new()),
            published: Mutex::new(None),
            swept: std::sync::atomic::AtomicBool::new(false),
            rpc_id: std::sync::atomic::AtomicI64::new(0),
        })
    }

    pub fn egress(&self) -> &Arc<Egress> {
        &self.egress
    }

    // ── keys and sealing ──

    /// The key snapshot: loaded on first use, replaced by a re-seal.
    fn keys(&self) -> Option<Arc<SecretKeys>> {
        if let Some(k) = self.keys.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
            return Some(Arc::clone(k));
        }
        let loaded = server_auth::secrets::load_existing(
            &self.data_dir,
            self.previous_key_env.as_deref().map(String::as_str),
        )
        .ok()?;
        let mut slot = self.keys.write().unwrap_or_else(|e| e.into_inner());
        Some(Arc::clone(slot.get_or_insert_with(|| Arc::new(loaded))))
    }

    /// core `dec(s, userId)`: the bound form first, then the unbound one (a row sealed before
    /// user binding shipped). `None`: no key opens it, or it is not a JSON object.
    fn open_json(&self, raw: &str, user: Option<&str>) -> Option<JValue> {
        let keys = self.keys()?;
        let plain = match user {
            Some(u) => keys
                .open(raw, Some(u))
                .or_else(|_| keys.open(raw, None))
                .ok()?,
            None => keys.open(raw, None).ok()?,
        };
        match js_json::parse(&plain.0) {
            Ok(v @ JValue::Obj(_)) => Some(v),
            _ => None,
        }
    }

    fn seal(&self, plain: &str, user: Option<&str>) -> Result<String, Fail> {
        let keys = self.keys().ok_or_else(|| auth_error("transient"))?;
        keys.seal(plain, user, &random::<12>())
            .map_err(|_| auth_error("transient"))
    }

    // ── blocking helpers ──

    async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        app: &Arc<App>,
        f: impl FnOnce(&Mcp, &App) -> T + Send + 'static,
    ) -> Result<T, Fail> {
        let (me, a) = (Arc::clone(self), Arc::clone(app));
        tokio::task::spawn_blocking(move || f(&me, &a))
            .await
            .map_err(|_| auth_error("transient"))
    }

    fn audit(&self, app: &App, actor: Option<&str>, action: &str, detail: &JValue) {
        let detail = js_json::stringify(detail).unwrap_or_else(|| "{}".into());
        if let Ok(w) = app.writes.writer() {
            let _ = w.write(|tx| {
                tx.execute(
                    "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)",
                    server_store::rusqlite::params![actor, action, detail, now_ms()],
                )
            });
        }
    }

    /// The directory servers (`[]` while Node has not made the table).
    pub fn servers(&self, app: &App) -> Result<Vec<Server>, Fail> {
        let writer = app.writes.writer().map_err(|_| auth_error("transient"))?;
        let rows = writer.read(|r| {
            r.rows(
                "SELECT id, title, url, added_by, oauth, personal, declared_json FROM directory_mcp_servers ORDER BY added_at",
                [],
                |row| {
                    Ok(Server {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        url: row.get(2)?,
                        added_by: row.get(3)?,
                        oauth: row.get::<_, i64>(4)? != 0,
                        personal: row.get::<_, i64>(5)? != 0,
                        declared: js_json::parse(&row.get::<_, String>(6)?)
                            .ok()
                            .filter(|v| matches!(v, JValue::Arr(_)))
                            .unwrap_or(JValue::Arr(Vec::new())),
                    })
                },
            )
        });
        Ok(rows.unwrap_or_default())
    }

    fn server(&self, app: &App, id: &str) -> Result<Option<Server>, Fail> {
        Ok(self.servers(app)?.into_iter().find(|s| s.id == id))
    }

    fn read_token(&self, app: &App, user: &str, server: &str) -> Result<Option<Row>, Fail> {
        let writer = app.writes.writer().map_err(|_| auth_error("transient"))?;
        let raw: Option<String> = writer
            .read(|r| {
                r.row(
                    "SELECT data_enc FROM mcp_oauth_tokens WHERE user_id=?1 AND server_id=?2",
                    [user, server],
                    |row| row.get(0),
                )
            })
            .map_err(|_| auth_error("transient"))?;
        Ok(raw.map(|raw| {
            let tokens = self
                .open_json(&raw, Some(user))
                .and_then(|v| Tokens::from_json(&v));
            Row { raw, tokens }
        }))
    }

    fn read_client_row(
        &self,
        app: &App,
        server: &str,
    ) -> Result<Option<(String, Option<Client>)>, Fail> {
        let writer = app.writes.writer().map_err(|_| auth_error("transient"))?;
        let raw: Option<String> = writer
            .read(|r| {
                r.row(
                    "SELECT data_enc FROM mcp_oauth_clients WHERE server_id=?1",
                    [server],
                    |row| row.get(0),
                )
            })
            .map_err(|_| auth_error("transient"))?;
        Ok(raw.map(|raw| {
            let client = self
                .open_json(&raw, None)
                .and_then(|v| Client::from_json(&v));
            (raw, client)
        }))
    }

    /// The account's stored key headers: `None` (no key), or `Some(None)` (one no key opens).
    fn read_key(&self, app: &App, user: &str, server: &str) -> Result<Option<StoredKey>, Fail> {
        let writer = app.writes.writer().map_err(|_| auth_error("transient"))?;
        let raw: Option<String> = writer
            .read(|r| {
                r.row(
                    "SELECT headers_enc FROM directory_mcp_user_keys WHERE user_id=?1 AND server_id=?2",
                    [user, server],
                    |row| row.get(0),
                )
            })
            .map_err(|_| auth_error("transient"))?;
        Ok(raw.map(|raw| match self.open_json(&raw, Some(user)) {
            Some(JValue::Obj(items)) => Some(
                items
                    .into_iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_owned())))
                    .collect(),
            ),
            _ => None,
        }))
    }

    // ── readiness ──

    /// Rewrites the published readiness if (and only if) it changed. Blocking.
    pub fn publish(&self, app: &App) {
        let mut last = self.published.lock().unwrap_or_else(|e| e.into_inner());
        let Some(doc) = self.readiness(app) else {
            return;
        };
        if last.as_deref() == Some(doc.as_str()) {
            return;
        }
        let file = self.data_dir.join(STATE_FILE);
        // Nobody holding a credential and no file yet is the state Node reads a missing file as, so no
        // file is made for it (a deployment that never used these credentials stays as it was).
        if doc == EMPTY_STATE && !file.exists() {
            *last = Some(doc);
            return;
        }
        if std::fs::read_to_string(&file).ok().as_deref() != Some(doc.as_str())
            && write_atomic(&file, &doc).is_err()
        {
            // Leave `last` alone: the next change tries again.
            return;
        }
        *last = Some(doc);
    }

    fn readiness(&self, app: &App) -> Option<String> {
        let writer = app.writes.writer().ok()?;
        let now = now_ms();
        let tokens: Vec<(String, String, String)> = writer
            .read(|r| {
                r.rows(
                    "SELECT user_id, server_id, data_enc FROM mcp_oauth_tokens ORDER BY user_id, server_id",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
            })
            .unwrap_or_default();
        let keys: Vec<(String, String)> = writer
            .read(|r| {
                r.rows(
                    "SELECT user_id, server_id FROM directory_mcp_user_keys ORDER BY user_id, server_id",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .unwrap_or_default();
        let mut oauth: BTreeMap<String, BTreeMap<String, &'static str>> = BTreeMap::new();
        for (user, server, raw) in tokens {
            let ready = self
                .open_json(&raw, Some(&user))
                .and_then(|v| Tokens::from_json(&v))
                .is_some_and(|t| !t.dead(now));
            oauth
                .entry(user)
                .or_default()
                .insert(server, if ready { "connected" } else { "reauth" });
        }
        let mut held: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (user, server) in keys {
            held.entry(user).or_default().push(server);
        }
        let doc = JValue::obj([
            ("version", JValue::Num(1.0)),
            (
                "oauth",
                JValue::obj(oauth.into_iter().map(|(u, servers)| {
                    (
                        u,
                        JValue::obj(
                            servers
                                .into_iter()
                                .map(|(s, st)| (s, JValue::Str(st.into()))),
                        ),
                    )
                })),
            ),
            (
                "keys",
                JValue::obj(
                    held.into_iter()
                        .map(|(u, s)| (u, JValue::Arr(s.into_iter().map(JValue::Str).collect()))),
                ),
            ),
        ]);
        js_json::stringify(&doc)
    }

    /// The start sweep (blocking): the sign-ins and keys of deleted accounts and removed servers.
    pub fn sweep_orphans(&self, app: &App) {
        use std::sync::atomic::Ordering;
        if self.swept.load(Ordering::Acquire) {
            return;
        }
        let Ok(writer) = app.writes.writer() else {
            return;
        };
        let users = writer
            .write(|tx| tx.mcp_delete_orphans_of_users(RustMcpCredentials::owned()))
            .ok();
        // Only where the server table exists and reads: a missing one must not look like "no servers".
        let servers_readable = writer
            .read(|r| {
                r.row("SELECT count(*) FROM directory_mcp_servers", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .is_ok();
        let servers = if servers_readable {
            writer
                .write(|tx| tx.mcp_delete_orphans_of_servers(RustMcpCredentials::owned()))
                .ok()
        } else {
            None
        };
        if let Some(n) = users {
            if servers.is_some() || !servers_readable {
                self.swept.store(true, Ordering::Release);
            }
            let removed = n + servers.unwrap_or(0);
            if removed > 0 {
                eprintln!("noevia-server: removed {removed} MCP credential(s) of deleted accounts or servers");
            }
        }
        self.publish(app);
    }

    pub async fn sweep_once(self: &Arc<Self>, app: &Arc<App>) {
        if self.swept.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let _ = self.blocking(app, |me, a| me.sweep_orphans(a)).await;
    }

    // ── the address a sign-in returns to ──

    fn redirect_uri(&self, app: &App) -> Result<String, Fail> {
        let writer = app.writes.writer().map_err(|_| auth_error("transient"))?;
        let setting = |key: &str| -> Option<String> {
            writer
                .read(|r| {
                    r.row("SELECT value FROM settings WHERE key=?1", [key], |row| {
                        row.get::<_, String>(0)
                    })
                })
                .ok()
                .flatten()
                .filter(|v| !v.is_empty())
        };
        let origin = setting("public_origin_admin")
            .or_else(|| Some(self.public_origin_env.clone()).filter(|v| !v.is_empty()))
            .or_else(|| setting("public_origin"))
            .unwrap_or_default();
        if origin.is_empty() {
            return Err(fail(
                "Set this server's web address (Settings → Web address) before signing in to a service.",
                409,
            ));
        }
        Ok(format!(
            "{}/api/mcp-oauth/callback",
            origin.strip_suffix('/').unwrap_or(&origin)
        ))
    }

    // ── discovery and registration ──

    /// core `discover`: the authorization server of an MCP server. `challenge` is its
    /// `WWW-Authenticate` header, if it sent one.
    pub async fn discover(&self, server_url: &str, challenge: &str) -> Result<Client, Fail> {
        let u = url::Url::parse(server_url)
            .map_err(|_| fail("The server address is not valid.", BAD_GATEWAY))?;
        let origin = u.origin().ascii_serialization();
        let path = u.path().strip_suffix('/').unwrap_or(u.path()).to_owned();
        let hinted = resource_metadata_hint(challenge);
        let candidates: Vec<String> = hinted
            .into_iter()
            .chain([
                format!("{origin}/.well-known/oauth-protected-resource{path}"),
                format!("{origin}/.well-known/oauth-protected-resource"),
            ])
            .collect();
        let mut prm = None;
        for c in &candidates {
            if let Ok(Some(found)) = self.egress.get_json(c).await {
                prm = Some(found);
                break;
            }
        }
        let prm = prm.unwrap_or(JValue::Undefined);
        // RFC 9728 §3.3: the resource named must be the one asked about.
        if let Some(named) = prm.get("resource").as_str() {
            if !same_resource(named, server_url) {
                return Err(fail(
                    "The server's sign-in information names a different resource.",
                    BAD_GATEWAY,
                ));
            }
        }
        let issuer = match prm.get("authorization_servers") {
            JValue::Arr(list) => list.first().and_then(|v| v.as_str()).map(str::to_owned),
            _ => Some(origin.clone()),
        };
        let Some(issuer) = issuer else {
            return Err(fail(
                "The server does not name a sign-in service.",
                BAD_GATEWAY,
            ));
        };
        let iu = url::Url::parse(&issuer)
            .map_err(|_| fail("The server does not name a sign-in service.", BAD_GATEWAY))?;
        let iorigin = iu.origin().ascii_serialization();
        let ipath = iu.path().strip_suffix('/').unwrap_or(iu.path()).to_owned();
        let as_candidates = [
            format!("{iorigin}/.well-known/oauth-authorization-server{ipath}"),
            format!("{iorigin}/.well-known/openid-configuration{ipath}"),
            format!(
                "{}/.well-known/openid-configuration",
                issuer.strip_suffix('/').unwrap_or(&issuer)
            ),
        ];
        let mut meta = None;
        for c in &as_candidates {
            if let Ok(Some(found)) = self.egress.get_json(c).await {
                if text(found.get("authorization_endpoint")).is_some()
                    && text(found.get("token_endpoint")).is_some()
                {
                    meta = Some(found);
                    break;
                }
            }
        }
        let Some(meta) = meta else {
            return Err(fail(
                "The server’s sign-in service did not describe itself (no OAuth metadata).",
                BAD_GATEWAY,
            ));
        };
        // RFC 8414 §3.3: the issuer it names must be the one it was asked for.
        if let Some(named) = meta.get("issuer").as_str() {
            if named.trim_end_matches('/') != issuer.trim_end_matches('/') {
                return Err(fail(
                    "The sign-in service names a different issuer than the one that was asked.",
                    BAD_GATEWAY,
                ));
            }
        }
        if let JValue::Arr(methods) = meta.get("code_challenge_methods_supported") {
            if !methods.iter().any(|m| m.as_str() == Some("S256")) {
                return Err(fail(
                    "The sign-in service does not support PKCE (S256).",
                    BAD_GATEWAY,
                ));
            }
        }
        let endpoint = |key: &str| text(meta.get(key));
        let authorization = endpoint("authorization_endpoint").unwrap_or_default();
        let token = endpoint("token_endpoint").unwrap_or_default();
        let registration = endpoint("registration_endpoint");
        let revocation = endpoint("revocation_endpoint");
        for (what, value) in [
            ("authorization", Some(&authorization)),
            ("token", Some(&token)),
            ("registration", registration.as_ref()),
        ] {
            if let Some(v) = value {
                if !self.egress.url_allowed(v).await {
                    return Err(fail(
                        format!(
                            "The sign-in service’s {what} address is not a public https address."
                        ),
                        BAD_GATEWAY,
                    ));
                }
            }
        }
        // Revocation is best effort and never a reason to refuse a service.
        let revocation = match revocation {
            Some(r) if self.egress.url_allowed(&r).await => Some(r),
            _ => None,
        };
        let methods: Vec<String> = match meta.get("token_endpoint_auth_methods_supported") {
            JValue::Arr(a) => a
                .iter()
                .filter_map(|m| m.as_str().map(str::to_owned))
                .collect(),
            _ => vec!["client_secret_basic".into()],
        };
        let secret_method = if methods.iter().any(|m| m == "client_secret_basic") {
            "client_secret_basic"
        } else if methods.iter().any(|m| m == "client_secret_post") {
            "client_secret_post"
        } else {
            "client_secret_basic"
        };
        let scopes = match prm.get("scopes_supported") {
            JValue::Arr(a) => a
                .iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .take(20)
                .collect(),
            _ => Vec::new(),
        };
        Ok(Client {
            resource: server_url.to_owned(),
            issuer,
            authorization_endpoint: authorization,
            token_endpoint: token,
            registration_endpoint: registration,
            revocation_endpoint: revocation,
            secret_method: secret_method.into(),
            scopes,
            client_id: String::new(),
            client_secret: None,
            redirect_uri: String::new(),
            token_auth: None,
            manual: false,
        })
    }

    /// core `clientFor`: discover and register noevia once per server, kept sealed for later
    /// sign-ins. A hand-registered app is used as it is.
    async fn client_for(
        self: &Arc<Self>,
        app: &Arc<App>,
        server: &str,
        server_url: &str,
        challenge: &str,
        redirect: &str,
    ) -> Result<Client, Fail> {
        let lock = lock_for(&self.client_locks, server);
        let _held = lock.lock().await;
        let name = server.to_owned();
        let known = self
            .blocking(app, move |me, a| me.read_client_row(a, &name))
            .await??
            .and_then(|(_, c)| c);
        if let Some(k) = known {
            if k.manual || k.redirect_uri == redirect {
                return Ok(k);
            }
        }
        let mut meta = self.discover(server_url, challenge).await?;
        let Some(registration) = meta.registration_endpoint.clone() else {
            return Err(Fail {
                needs_client: true,
                issuer: Some(meta.issuer.clone()),
                ..fail(
                    "This sign-in service needs an app registered by hand. An administrator registers one and enters its client ID.",
                    409,
                )
            });
        };
        let body = js_json::stringify(&JValue::obj([
            ("client_name", JValue::Str("noevia".into())),
            (
                "redirect_uris",
                JValue::Arr(vec![JValue::Str(redirect.into())]),
            ),
            (
                "grant_types",
                JValue::Arr(vec![
                    JValue::Str("authorization_code".into()),
                    JValue::Str("refresh_token".into()),
                ]),
            ),
            (
                "response_types",
                JValue::Arr(vec![JValue::Str("code".into())]),
            ),
            ("token_endpoint_auth_method", JValue::Str("none".into())),
        ]))
        .unwrap_or_default();
        let reply = self
            .egress
            .post(
                &registration,
                &[
                    ("content-type", "application/json".into()),
                    ("accept", "application/json".into()),
                ],
                body,
                Duration::from_secs(8),
            )
            .await
            .map_err(|_| {
                fail(
                    "The sign-in service did not answer the registration.",
                    BAD_GATEWAY,
                )
            })?;
        let registered = reply.json();
        let client_id = registered.as_ref().and_then(|r| text(r.get("client_id")));
        let (true, Some(client_id), Some(registered)) = (reply.ok(), client_id, registered) else {
            return Err(fail(
                format!(
                    "The sign-in service refused to register noevia ({}).",
                    reply.status
                ),
                BAD_GATEWAY,
            ));
        };
        meta.client_id = client_id;
        meta.client_secret = text(registered.get("client_secret"));
        meta.redirect_uri = redirect.to_owned();
        // The method the service registered the app for, else none without a secret.
        meta.token_auth = Some(if meta.client_secret.is_none() {
            "none".into()
        } else {
            text(registered.get("token_endpoint_auth_method"))
                .filter(|m| m == "client_secret_basic" || m == "client_secret_post")
                .unwrap_or_else(|| meta.secret_method.clone())
        });
        let sealed = self.seal(&meta.to_json(), None)?;
        let name = server.to_owned();
        self.blocking(app, move |me, a| {
            let w = a.writes.writer().map_err(|_| auth_error("transient"))?;
            w.write(|tx| tx.mcp_save_client(RustMcpCredentials::owned(), &name, &sealed, now_ms()))
                .map_err(|_| auth_error("transient"))?;
            me.publish(a);
            Ok::<(), Fail>(())
        })
        .await??;
        Ok(meta)
    }

    // ── the sign-in ──

    /// core `start`: the address to send this account to; the state ties the answer to them and this
    /// server.
    pub async fn start(
        self: &Arc<Self>,
        app: &Arc<App>,
        user: &str,
        server: &str,
        server_url: &str,
        challenge: &str,
        purpose: &str,
    ) -> Result<String, Fail> {
        let me = Arc::clone(self);
        let (app, user, server, server_url, challenge, purpose) = (
            Arc::clone(app),
            user.to_owned(),
            server.to_owned(),
            server_url.to_owned(),
            challenge.to_owned(),
            purpose.to_owned(),
        );
        // Registering an app is not undone by a client that goes away.
        tokio::spawn(async move {
            let redirect = {
                let (m, a) = (Arc::clone(&me), Arc::clone(&app));
                tokio::task::spawn_blocking(move || m.redirect_uri(&a))
                    .await
                    .map_err(|_| auth_error("transient"))??
            };
            let client = me
                .client_for(&app, &server, &server_url, &challenge, &redirect)
                .await?;
            let verifier = b64url(&random::<32>());
            let state = b64url(&random::<24>());
            {
                let now = now_ms();
                let mut pending = me.pending.lock().unwrap_or_else(|e| e.into_inner());
                pending.retain(|_, p| p.expires >= now);
                // Cap how many pending sign-ins one account can hold open: evict their oldest.
                let mut own: Vec<(&String, i64)> = pending
                    .iter()
                    .filter(|(_, p)| p.user == user)
                    .map(|(k, p)| (k, p.expires))
                    .collect();
                own.sort_by_key(|(_, e)| *e);
                let evict: Vec<String> = own
                    .iter()
                    .take(own.len().saturating_sub(MAX_PENDING_PER_USER - 1))
                    .map(|(k, _)| (*k).clone())
                    .collect();
                for k in evict {
                    pending.remove(&k);
                }
                if pending.len() >= MAX_PENDING {
                    return Err(fail(
                        "Too many sign-ins are waiting. Try again in a few minutes.",
                        429,
                    ));
                }
                pending.insert(
                    state.clone(),
                    Pending {
                        user: user.clone(),
                        server: server.clone(),
                        verifier: Zeroizing::new(verifier.clone()),
                        expires: now + STATE_TTL_MS,
                        purpose,
                        redirect: client.redirect_uri.clone(),
                        issuer: client.issuer.clone(),
                    },
                );
            }
            let challenge_s256 = b64url(&Sha256::digest(verifier.as_bytes()));
            let mut q = url::form_urlencoded::Serializer::new(String::new());
            q.append_pair("response_type", "code")
                .append_pair("client_id", &client.client_id)
                .append_pair("redirect_uri", &client.redirect_uri)
                .append_pair("state", &state)
                .append_pair("code_challenge", &challenge_s256)
                .append_pair("code_challenge_method", "S256")
                .append_pair("resource", &client.resource);
            if !client.scopes.is_empty() {
                q.append_pair("scope", &client.scopes.join(" "));
            }
            let sep = if client.authorization_endpoint.contains('?') {
                '&'
            } else {
                '?'
            };
            Ok(format!(
                "{}{sep}{}",
                client.authorization_endpoint,
                q.finish()
            ))
        })
        .await
        .map_err(|_| auth_error("transient"))?
    }

    /// core `tokenRequest`.
    async fn token_request(
        &self,
        client: &Client,
        params: &[(&str, &str)],
    ) -> Result<(Tokens, u16), (Fail, Option<String>)> {
        let mut pairs: Vec<(&str, &str)> = params.to_vec();
        pairs.push(("client_id", &client.client_id));
        pairs.push(("resource", &client.resource));
        let (body, headers) = form_request(client, pairs);
        let reply = self
            .egress
            .post(
                &client.token_endpoint,
                &headers,
                body,
                Duration::from_secs(10),
            )
            .await
            .map_err(|_| {
                (
                    fail("The sign-in service did not answer.", BAD_GATEWAY),
                    None,
                )
            })?;
        let body = reply.json();
        let error_code = body
            .as_ref()
            .and_then(|b| b.get("error").as_str().map(str::to_owned));
        let access = body.as_ref().and_then(|b| text(b.get("access_token")));
        let (true, Some(access), Some(body)) = (reply.ok(), access, body) else {
            let why = error_code
                .as_deref()
                .map(|e| format!(": {}", js_json::js_slice(e, 60)))
                .unwrap_or_default();
            return Err((
                fail(
                    format!(
                        "The sign-in service did not issue a token ({}{why}).",
                        reply.status
                    ),
                    BAD_GATEWAY,
                ),
                error_code,
            ));
        };
        let expires_at = match body.get("expires_in") {
            JValue::Num(n) if n.is_finite() => Some(now_ms() as f64 + n * 1000.0),
            JValue::Str(s) => s
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite())
                .map(|n| now_ms() as f64 + n * 1000.0),
            _ => None,
        };
        let refresh = text(body.get("refresh_token"))
            .or_else(|| {
                params
                    .iter()
                    .find(|(k, _)| *k == "refresh_token")
                    .map(|(_, v)| (*v).to_owned())
            })
            .map(Zeroizing::new);
        Ok((
            Tokens {
                access: Zeroizing::new(access),
                refresh,
                expires_at,
            },
            reply.status,
        ))
    }

    /// core `finish`: the browser came back. Only the same signed-in account that started can end it.
    pub async fn finish(
        self: &Arc<Self>,
        app: &Arc<App>,
        user: &str,
        state: &str,
        code: &str,
        iss: Option<&str>,
    ) -> Result<Finished, Fail> {
        let taken = {
            let now = now_ms();
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            match pending.get(state) {
                Some(p) if p.expires >= now => {
                    // Ownership before consuming: another signed-in account presenting this state
                    // must not be able to burn it for its owner.
                    if p.user != user {
                        return Err(fail(
                            "This sign-in was started by a different account.",
                            403,
                        ));
                    }
                    pending.remove(state)
                }
                _ => None,
            }
        };
        let Some(p) = taken else {
            return Err(fail(
                "This sign-in expired or was already used. Start again.",
                400,
            ));
        };
        let (me, app, user, code, iss) = (
            Arc::clone(self),
            Arc::clone(app),
            user.to_owned(),
            code.to_owned(),
            iss.map(str::to_owned),
        );
        // The code is spent once the exchange starts: the tokens must be kept even if the browser
        // tab is closed meanwhile.
        tokio::spawn(async move {
            let server = p.server.clone();
            let known = me
                .blocking(&app, move |m, a| m.read_client_row(a, &server))
                .await??
                .and_then(|(_, c)| c);
            let Some(client) = known else {
                return Err(fail("This server’s sign-in setup is missing. Start again.", 400));
            };
            // RFC 9207: a response that names its issuer must name the one that was asked.
            if let Some(iss) = iss.filter(|i| !i.is_empty()) {
                let expected = if p.issuer.is_empty() { &client.issuer } else { &p.issuer };
                if !expected.is_empty() && iss.trim_end_matches('/') != expected.trim_end_matches('/') {
                    return Err(fail(
                        "The answer came from a different sign-in service than the one that was asked. Start again.",
                        400,
                    ));
                }
            }
            if code.is_empty() {
                return Err(fail("The sign-in service did not send a code. Start again.", 400));
            }
            let (tokens, _) = me
                .token_request(
                    &client,
                    &[
                        ("grant_type", "authorization_code"),
                        ("code", &code),
                        ("redirect_uri", &p.redirect),
                        ("code_verifier", &p.verifier),
                    ],
                )
                .await
                .map_err(|(f, _)| f)?;
            let sealed = me.seal(&tokens.to_json(), Some(&user))?;
            let (u, s) = (user.clone(), p.server.clone());
            me.blocking(&app, move |m, a| {
                // A sign-in for a server removed meanwhile is not kept.
                if m.server(a, &s)?.is_none() {
                    return Err(fail("This server was removed while you were signing in.", 404));
                }
                let w = a.writes.writer().map_err(|_| auth_error("transient"))?;
                w.write(|tx| tx.mcp_save_token(RustMcpCredentials::owned(), &u, &s, &sealed, now_ms()))
                    .map_err(|_| auth_error("transient"))?;
                m.audit(a, Some(&u), "mcp.oauth.connect", &JValue::obj([("serverId", JValue::Str(s.clone()))]));
                m.publish(a);
                Ok(())
            })
            .await??;
            Ok(Finished { purpose: p.purpose })
        })
        .await
        .map_err(|_| auth_error("transient"))?
    }

    // ── the token ──

    /// A usable access token for this account and server, refreshed when close to expiry.
    /// `rejected` is the token a server just answered 401 to: it is refreshed past only while it is
    /// still the stored one.
    pub async fn session(
        self: &Arc<Self>,
        app: &Arc<App>,
        user: &str,
        server: &str,
        rejected: Option<&str>,
    ) -> Result<Issued, Fail> {
        let (u, s) = (user.to_owned(), server.to_owned());
        let row = self
            .blocking(app, move |me, a| me.read_token(a, &u, &s))
            .await??
            .ok_or_else(|| auth_error("disconnected"))?;
        let tokens = row.tokens.clone().ok_or_else(|| auth_error("reconnect"))?;
        let stale = rejected.is_some_and(|r| r == tokens.access.as_str());
        if !stale && !tokens.expiring(now_ms()) {
            return Ok(issued(&row, &tokens));
        }
        let (me, app, user, server, rejected) = (
            Arc::clone(self),
            Arc::clone(app),
            user.to_owned(),
            server.to_owned(),
            rejected.map(str::to_owned),
        );
        // A refresh spends a (rotating) refresh token: it finishes, and is stored, whatever the caller does.
        tokio::spawn(async move {
            me.refresh_task(&app, &user, &server, rejected.as_deref())
                .await
        })
        .await
        .map_err(|_| auth_error("transient"))?
    }

    async fn refresh_task(
        self: &Arc<Self>,
        app: &Arc<App>,
        user: &str,
        server: &str,
        rejected: Option<&str>,
    ) -> Result<Issued, Fail> {
        let lock = lock_for(&self.locks, &pair_key(user, server));
        let _held = lock.lock().await;
        let (u, s) = (user.to_owned(), server.to_owned());
        let row = self
            .blocking(app, move |me, a| me.read_token(a, &u, &s))
            .await??
            .ok_or_else(|| auth_error("disconnected"))?;
        let t = row.tokens.clone().ok_or_else(|| auth_error("reconnect"))?;
        let now = now_ms();
        let stale = rejected.is_some_and(|r| r == t.access.as_str());
        // Someone refreshed while this waited.
        if !stale && !t.expiring(now) {
            return Ok(issued(&row, &t));
        }
        let Some(refresh) = t.refresh.clone() else {
            return if !stale && !t.expired(now) {
                Ok(issued(&row, &t))
            } else {
                Err(auth_error("reconnect"))
            };
        };
        let name = server.to_owned();
        let client = self
            .blocking(app, move |me, a| me.read_client_row(a, &name))
            .await??
            .and_then(|(_, c)| c);
        let Some(client) = client else {
            return if !stale && !t.expired(now) {
                Ok(issued(&row, &t))
            } else {
                Err(auth_error("reconnect"))
            };
        };
        let outcome = match self
            .token_request(
                &client,
                &[("grant_type", "refresh_token"), ("refresh_token", &refresh)],
            )
            .await
        {
            Ok((next, _)) => Refreshed::New(next),
            Err((_, Some(code))) if code == "invalid_grant" => Refreshed::Rejected,
            Err(_) => Refreshed::Transient,
        };
        match outcome {
            Refreshed::New(next) => {
                let sealed = self.seal(&next.to_json(), Some(user))?;
                let (u, s, raw, stored) = (
                    user.to_owned(),
                    server.to_owned(),
                    row.raw.clone(),
                    sealed.clone(),
                );
                let changed = self
                    .blocking(app, move |_, a| {
                        let w = a.writes.writer().map_err(|_| auth_error("transient"))?;
                        w.write(|tx| {
                            tx.mcp_swap_token(
                                RustMcpCredentials::owned(),
                                &u,
                                &s,
                                &stored,
                                now_ms(),
                                &raw,
                            )
                        })
                        .map_err(|_| auth_error("transient"))
                    })
                    .await??;
                if changed == 0 {
                    // A disconnect or a newer sign-in won: use what is there now.
                    let (u, s) = (user.to_owned(), server.to_owned());
                    let now_row = self
                        .blocking(app, move |me, a| me.read_token(a, &u, &s))
                        .await??
                        .ok_or_else(|| auth_error("disconnected"))?;
                    let t = now_row
                        .tokens
                        .clone()
                        .ok_or_else(|| auth_error("reconnect"))?;
                    return Ok(issued(&now_row, &t));
                }
                Ok(Issued {
                    access: next.access.clone(),
                    expires_at: next.expires_at,
                    generation: generation_of(&sealed),
                })
            }
            Refreshed::Rejected => {
                self.strip(app, user, server, &row.raw).await?;
                Err(auth_error("reconnect"))
            }
            Refreshed::Transient => {
                if !stale && !t.expired(now) {
                    Ok(issued(&row, &t))
                } else {
                    Err(auth_error("transient"))
                }
            }
        }
    }

    /// Marks a sign-in as needing a new one: its refresh token goes and it is expired, so every
    /// reader (this broker, and a Core that owns the row after a rollback) reads it as dead. Only
    /// the sign-in named by `raw`.
    async fn strip(
        self: &Arc<Self>,
        app: &Arc<App>,
        user: &str,
        server: &str,
        raw: &str,
    ) -> Result<bool, Fail> {
        let Some(access) = self
            .open_json(raw, Some(user))
            .and_then(|v| Tokens::from_json(&v))
            .map(|t| t.access)
        else {
            return Ok(false);
        };
        let dead = Tokens {
            access,
            refresh: None,
            expires_at: Some(1.0),
        };
        let sealed = self.seal(&dead.to_json(), Some(user))?;
        let (u, s, raw) = (user.to_owned(), server.to_owned(), raw.to_owned());
        self.blocking(app, move |me, a| {
            let w = a.writes.writer().map_err(|_| auth_error("transient"))?;
            let n = w
                .write(|tx| {
                    tx.mcp_swap_token(RustMcpCredentials::owned(), &u, &s, &sealed, now_ms(), &raw)
                })
                .map_err(|_| auth_error("transient"))?;
            if n > 0 {
                me.audit(
                    a,
                    Some(&u),
                    "mcp.oauth.reconnect_needed",
                    &JValue::obj([("serverId", JValue::Str(s.clone()))]),
                );
                me.publish(a);
            }
            Ok(n > 0)
        })
        .await?
    }

    /// The second 401 on a fresh token: this sign-in (by its generation) needs a new one.
    pub async fn reject(
        self: &Arc<Self>,
        app: &Arc<App>,
        user: &str,
        server: &str,
        generation: &str,
    ) -> bool {
        let lock = lock_for(&self.locks, &pair_key(user, server));
        let _held = lock.lock().await;
        let (u, s) = (user.to_owned(), server.to_owned());
        let Ok(Ok(Some(row))) = self
            .blocking(app, move |me, a| me.read_token(a, &u, &s))
            .await
        else {
            return false;
        };
        if generation_of(&row.raw) != generation {
            return false;
        }
        self.strip(app, user, server, &row.raw)
            .await
            .unwrap_or(false)
    }

    /// An account's key headers for one call (core `userHeadersFor`).
    pub async fn key_headers(
        self: &Arc<Self>,
        app: &Arc<App>,
        user: &str,
        server: &str,
    ) -> Result<Vec<(String, String)>, Fail> {
        let (u, s) = (user.to_owned(), server.to_owned());
        match self
            .blocking(app, move |me, a| me.read_key(a, &u, &s))
            .await??
        {
            None => Err(Fail {
                code: Some("disconnected"),
                ..fail(NO_KEY, 401)
            }),
            Some(None) => Err(auth_error("reconnect")),
            Some(Some(headers)) if headers.is_empty() => Err(Fail {
                code: Some("disconnected"),
                ..fail(NO_KEY, 401)
            }),
            Some(Some(headers)) => Ok(headers),
        }
    }

    // ── what the pages list ──

    /// `GET /api/mcp-keys/servers`: the servers whose key each person supplies, and whether this
    /// account has added its own.
    pub async fn key_servers(self: &Arc<Self>, app: &Arc<App>, user: &str) -> Result<JValue, Fail> {
        let u = user.to_owned();
        self.blocking(app, move |me, a| {
            let held: Vec<String> = a
                .writes
                .writer()
                .ok()
                .and_then(|w| {
                    w.read(|r| {
                        r.rows(
                            "SELECT server_id FROM directory_mcp_user_keys WHERE user_id=?1",
                            [u.as_str()],
                            |row| row.get::<_, String>(0),
                        )
                    })
                    .ok()
                })
                .unwrap_or_default();
            let servers = me.servers(a)?;
            Ok(JValue::obj([(
                "servers",
                JValue::Arr(
                    servers
                        .into_iter()
                        .filter(|s| s.personal)
                        .map(|s| {
                            JValue::obj([
                                ("id", JValue::Str(s.id.clone())),
                                ("title", JValue::Str(s.title.clone())),
                                ("headers", s.declared.clone()),
                                ("hasKey", JValue::Bool(held.contains(&s.id))),
                            ])
                        })
                        .collect(),
                ),
            )]))
        })
        .await?
    }

    /// `GET /api/mcp-oauth/servers`: the sign-in servers and whether this account is connected
    /// (`connected`) or holds a sign-in no key can open any more (`needsReauth`).
    pub async fn oauth_servers(
        self: &Arc<Self>,
        app: &Arc<App>,
        user: &str,
    ) -> Result<JValue, Fail> {
        let u = user.to_owned();
        self.blocking(app, move |me, a| {
            let now = now_ms();
            let mut out = Vec::new();
            for s in me.servers(a)?.into_iter().filter(|s| s.oauth) {
                let state = match me.read_token(a, &u, &s.id)? {
                    None => "disconnected",
                    Some(Row {
                        tokens: Some(t), ..
                    }) if !t.dead(now) => "connected",
                    Some(_) => "needs-reauth",
                };
                out.push(JValue::obj([
                    ("id", JValue::Str(s.id.clone())),
                    ("title", JValue::Str(s.title.clone())),
                    ("connected", JValue::Bool(state == "connected")),
                    ("needsReauth", JValue::Bool(state == "needs-reauth")),
                ]));
            }
            Ok(JValue::obj([("servers", JValue::Arr(out))]))
        })
        .await?
    }

    /// One directory server by id.
    pub async fn find_server(
        self: &Arc<Self>,
        app: &Arc<App>,
        id: &str,
    ) -> Result<Option<Server>, Fail> {
        let id = id.to_owned();
        self.blocking(app, move |me, a| me.server(a, &id)).await?
    }

    // ── disconnect and forget ──

    /// core `disconnect` for an account's own sign-in, and the service is told (RFC 7009, best effort).
    pub async fn disconnect(
        self: &Arc<Self>,
        app: &Arc<App>,
        user: &str,
        server: &str,
    ) -> Result<(), Fail> {
        let lock = lock_for(&self.locks, &pair_key(user, server));
        let _held = lock.lock().await;
        let (u, s) = (user.to_owned(), server.to_owned());
        let revoke = self
            .blocking(app, move |me, a| {
                let before = me
                    .read_token(a, &u, &s)
                    .ok()
                    .flatten()
                    .and_then(|r| r.tokens);
                let client = me
                    .read_client_row(a, &s)
                    .ok()
                    .flatten()
                    .and_then(|(_, c)| c);
                let w = a.writes.writer().map_err(|_| auth_error("transient"))?;
                w.write(|tx| tx.mcp_delete_token(RustMcpCredentials::owned(), &u, &s))
                    .map_err(|_| auth_error("transient"))?;
                me.audit(
                    a,
                    Some(&u),
                    "mcp.oauth.disconnect",
                    &JValue::obj([("serverId", JValue::Str(s.clone()))]),
                );
                me.publish(a);
                Ok::<_, Fail>((before, client))
            })
            .await??;
        // Once the row is gone the service is asked to drop the grant; a failure changes nothing.
        if let (Some(tokens), Some(client)) = revoke {
            if client.revocation_endpoint.is_some() {
                let me = Arc::clone(self);
                tokio::spawn(async move { me.revoke(&client, &tokens).await });
            }
        }
        Ok(())
    }

    /// RFC 7009: the refresh token if there is one, else the access token. Never fails the caller.
    async fn revoke(&self, client: &Client, tokens: &Tokens) {
        let Some(endpoint) = client.revocation_endpoint.as_deref() else {
            return;
        };
        let (token, hint) = match &tokens.refresh {
            Some(r) => (r.to_string(), "refresh_token"),
            None => (tokens.access.to_string(), "access_token"),
        };
        let (body, headers) = form_request(
            client,
            vec![
                ("token", token.as_str()),
                ("token_type_hint", hint),
                ("client_id", client.client_id.as_str()),
            ],
        );
        let _ = self
            .egress
            .post(endpoint, &headers, body, Duration::from_secs(5))
            .await;
    }

    /// A removed server takes every account's sign-in, the registration and every account's key with it.
    pub async fn forget_server(self: &Arc<Self>, app: &Arc<App>, server: &str) -> Result<(), Fail> {
        let s = server.to_owned();
        self.blocking(app, move |me, a| {
            let w = a.writes.writer().map_err(|_| auth_error("transient"))?;
            w.write(|tx| {
                let cap = RustMcpCredentials::owned();
                tx.mcp_delete_tokens_of_server(cap, &s)?;
                tx.mcp_delete_client(cap, &s)?;
                tx.mcp_delete_keys_of_server(cap, &s)
            })
            .map_err(|_| auth_error("transient"))?;
            me.publish(a);
            Ok(())
        })
        .await?
    }

    /// A deleted account takes every one of its sign-ins and keys with it.
    pub async fn forget_user(self: &Arc<Self>, app: &Arc<App>, user: &str) -> Result<(), Fail> {
        let u = user.to_owned();
        self.blocking(app, move |me, a| {
            let w = a.writes.writer().map_err(|_| auth_error("transient"))?;
            w.write(|tx| {
                let cap = RustMcpCredentials::owned();
                tx.mcp_delete_tokens_of_user(cap, &u)?;
                tx.mcp_delete_keys_of_user(cap, &u)
            })
            .map_err(|_| auth_error("transient"))?;
            me.publish(a);
            Ok(())
        })
        .await?
    }

    // ── the administrator's hand-registered app ──

    /// core `setClient`: validated, checked against the service, stored sealed. Sign-ins made with
    /// another app are not valid for this one.
    pub async fn set_client(
        self: &Arc<Self>,
        app: &Arc<App>,
        server: &str,
        server_url: &str,
        client_id: &str,
        client_secret: &str,
        challenge: &str,
    ) -> Result<(), Fail> {
        let id = client_id.trim_matches(js_ws);
        let secret = client_secret.trim_matches(js_ws);
        if id.is_empty() || js_json::js_len(id) > 300 || id.chars().any(|c| js_ws(c) || c == '\0') {
            return Err(fail(
                "Enter the client ID the service gave you (one word, no spaces).",
                400,
            ));
        }
        if js_json::js_len(secret) > 1000 || secret.contains(['\r', '\n', '\0']) {
            return Err(fail("The client secret must be one line.", 400));
        }
        let (me, app, server, server_url, challenge, id, secret) = (
            Arc::clone(self),
            Arc::clone(app),
            server.to_owned(),
            server_url.to_owned(),
            challenge.to_owned(),
            id.to_owned(),
            secret.to_owned(),
        );
        tokio::spawn(async move {
            let meta = me.discover(&server_url, &challenge).await?;
            let redirect = {
                let (m, a) = (Arc::clone(&me), Arc::clone(&app));
                tokio::task::spawn_blocking(move || m.redirect_uri(&a))
                    .await
                    .map_err(|_| auth_error("transient"))??
            };
            let client = Client {
                client_id: id,
                token_auth: Some(if secret.is_empty() {
                    "none".into()
                } else {
                    meta.secret_method.clone()
                }),
                client_secret: Some(secret).filter(|s| !s.is_empty()),
                redirect_uri: redirect,
                manual: true,
                ..meta
            };
            let sealed = me.seal(&client.to_json(), None)?;
            let s = server.clone();
            me.blocking(&app, move |m, a| {
                let w = a.writes.writer().map_err(|_| auth_error("transient"))?;
                w.write(|tx| {
                    let cap = RustMcpCredentials::owned();
                    tx.mcp_save_client(cap, &s, &sealed, now_ms())?;
                    tx.mcp_delete_tokens_of_server(cap, &s)
                })
                .map_err(|_| auth_error("transient"))?;
                m.audit(
                    a,
                    None,
                    "mcp.oauth.client",
                    &JValue::obj([
                        ("serverId", JValue::Str(s.clone())),
                        ("manual", JValue::Bool(true)),
                    ]),
                );
                m.publish(a);
                Ok(())
            })
            .await?
        })
        .await
        .map_err(|_| auth_error("transient"))?
    }

    /// core `clientInfo`: what an administrator may see about the app, never the secret.
    pub async fn client_info(
        self: &Arc<Self>,
        app: &Arc<App>,
        server: &str,
    ) -> Result<JValue, Fail> {
        let s = server.to_owned();
        let found = self
            .blocking(app, move |me, a| me.read_client_row(a, &s))
            .await??;
        Ok(match found.and_then(|(_, c)| c) {
            None => JValue::Null,
            Some(c) => JValue::obj([
                ("manual", JValue::Bool(c.manual)),
                (
                    "clientId",
                    if c.manual {
                        JValue::Str(c.client_id)
                    } else {
                        JValue::Null
                    },
                ),
                ("hasSecret", JValue::Bool(c.client_secret.is_some())),
                ("redirectUri", JValue::Str(c.redirect_uri)),
                ("issuer", JValue::Str(c.issuer)),
            ]),
        })
    }

    // ── an account's own key ──

    /// The key check and its store as one piece of work (the caller awaits it). `values` is the
    /// request's `headers` object.
    pub async fn put_key(
        self: &Arc<Self>,
        app: &Arc<App>,
        user: &str,
        server: &Server,
        values: &JValue,
    ) -> Result<(), Fail> {
        let declared = server.declared_headers();
        let pending = check_header_values(&declared, values)?;
        let (me, app, user, server, values) = (
            Arc::clone(self),
            Arc::clone(app),
            user.to_owned(),
            server.clone(),
            values.clone(),
        );
        tokio::spawn(async move {
            // Proven against the server first: a key it refuses is never kept.
            let checked =
                mcp_egress::check_key(&me.egress, &server.url, &pending, &me.rpc_id).await;
            if let Err(why) = checked {
                return Err(fail(
                    format!(
                        "The server did not accept that key: {}",
                        js_json::js_slice(&why, 200)
                    ),
                    422,
                ));
            }
            me.blocking(&app, move |m, a| {
                // core setUserKey: the server must still be a personal one, and the key not empty.
                let Some(now_server) = m.server(a, &server.id)?.filter(|s| s.personal) else {
                    return Err(fail("No such server.", 404));
                };
                let headers = check_header_values(&now_server.declared_headers(), &values)?;
                if headers.is_empty() {
                    return Err(fail("Enter your key.", 400));
                }
                m.store_key(a, &user, &now_server.id, &headers)?;
                m.audit(
                    a,
                    Some(&user),
                    "mcp.directory.user-key",
                    &JValue::obj([
                        ("id", JValue::Str(now_server.id.clone())),
                        (
                            "keyHeaders",
                            JValue::Arr(
                                headers
                                    .iter()
                                    .map(|(k, _)| JValue::Str(k.clone()))
                                    .collect(),
                            ),
                        ),
                    ]),
                );
                m.publish(a);
                Ok(())
            })
            .await?
        })
        .await
        .map_err(|_| auth_error("transient"))?
    }

    /// Seals an account's key headers and saves them (blocking).
    pub fn store_key(
        &self,
        app: &App,
        user: &str,
        server: &str,
        headers: &[(String, String)],
    ) -> Result<(), Fail> {
        let plain = Zeroizing::new(
            js_json::stringify(&JValue::obj(
                headers
                    .iter()
                    .map(|(k, v)| (k.clone(), JValue::Str(v.clone()))),
            ))
            .unwrap_or_default(),
        );
        let sealed = self.seal(&plain, Some(user))?;
        let w = app.writes.writer().map_err(|_| auth_error("transient"))?;
        w.write(|tx| tx.mcp_save_key(RustMcpCredentials::owned(), user, server, &sealed, now_ms()))
            .map_err(|_| auth_error("transient"))?;
        Ok(())
    }

    pub async fn delete_key(
        self: &Arc<Self>,
        app: &Arc<App>,
        user: &str,
        server: &str,
    ) -> Result<(), Fail> {
        let (u, s) = (user.to_owned(), server.to_owned());
        self.blocking(app, move |me, a| {
            let w = a.writes.writer().map_err(|_| auth_error("transient"))?;
            w.write(|tx| tx.mcp_delete_key(RustMcpCredentials::owned(), &u, &s))
                .map_err(|_| auth_error("transient"))?;
            me.audit(
                a,
                Some(&u),
                "mcp.directory.user-key.remove",
                &JValue::obj([("id", JValue::Str(s.clone()))]),
            );
            me.publish(a);
            Ok(())
        })
        .await?
    }

    // ── the re-seal of a secrets rotation ──

    /// One pass over the three tables with the current and the previous key (secrets-rotate.cjs's
    /// rule per row: a row sealed under the previous key is `rotated`, a plaintext or older-format one
    /// `upgraded`, one no key opens `failed` and left exactly as it was). The keys are read from disk
    /// afresh and kept as the snapshot. A sign-in is re-sealed under its refresh lock, so a refresh
    /// never spends a token this pass then overwrites. `Err`: no usable keys.
    pub async fn reseal(self: &Arc<Self>, app: &Arc<App>) -> Result<ResealReport, Fail> {
        let fresh = server_auth::secrets::load_existing(
            &self.data_dir,
            self.previous_key_env.as_deref().map(String::as_str),
        )
        .map_err(|_| fail("credential store unavailable", 503))?;
        *self.keys.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(fresh));
        let mut report = ResealReport::default();
        // Tokens (bound), clients (unbound), keys (bound).
        let rows = self
            .blocking(app, |_, a| -> Result<_, Fail> {
                let w = a.writes.writer().map_err(|_| auth_error("transient"))?;
                let tokens = w.read(|r| r.rows("SELECT user_id, server_id FROM mcp_oauth_tokens ORDER BY user_id, server_id", [], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))).unwrap_or_default();
                let clients = w.read(|r| r.rows("SELECT server_id FROM mcp_oauth_clients ORDER BY server_id", [], |row| row.get::<_, String>(0))).unwrap_or_default();
                let keys = w.read(|r| r.rows("SELECT user_id, server_id FROM directory_mcp_user_keys ORDER BY user_id, server_id", [], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))).unwrap_or_default();
                Ok((tokens, clients, keys))
            })
            .await??;
        let (tokens, clients, keys) = rows;
        for (user, server) in tokens {
            let lock = lock_for(&self.locks, &pair_key(&user, &server));
            let _held = lock.lock().await;
            let (u, s) = (user.clone(), server.clone());
            let outcome = self
                .blocking(app, move |me, a| {
                    me.reseal_row(a, "mcp_oauth_tokens", &u, &s)
                })
                .await?;
            report.add("mcp_oauth_tokens", &[user, server], outcome);
        }
        for server in clients {
            let s = server.clone();
            let outcome = self
                .blocking(app, move |me, a| {
                    me.reseal_row(a, "mcp_oauth_clients", "", &s)
                })
                .await?;
            report.add("mcp_oauth_clients", &[server], outcome);
        }
        for (user, server) in keys {
            let (u, s) = (user.clone(), server.clone());
            let outcome = self
                .blocking(app, move |me, a| {
                    me.reseal_row(a, "directory_mcp_user_keys", &u, &s)
                })
                .await?;
            report.add("directory_mcp_user_keys", &[user, server], outcome);
        }
        let _ = self.blocking(app, |me, a| me.publish(a)).await;
        Ok(report)
    }

    fn reseal_row(&self, app: &App, table: &str, user: &str, server: &str) -> Resealed {
        let Ok(w) = app.writes.writer() else {
            return Resealed::Failed("the database could not be opened");
        };
        let read = w.read(|r| match table {
            "mcp_oauth_tokens" => r.row(
                "SELECT data_enc FROM mcp_oauth_tokens WHERE user_id=?1 AND server_id=?2",
                [user, server],
                |row| row.get::<_, String>(0),
            ),
            "directory_mcp_user_keys" => r.row(
                "SELECT headers_enc FROM directory_mcp_user_keys WHERE user_id=?1 AND server_id=?2",
                [user, server],
                |row| row.get::<_, String>(0),
            ),
            _ => r.row(
                "SELECT data_enc FROM mcp_oauth_clients WHERE server_id=?1",
                [server],
                |row| row.get::<_, String>(0),
            ),
        });
        let Ok(Some(raw)) = read else {
            // Deleted since the listing.
            return Resealed::Empty;
        };
        if raw.is_empty() {
            return Resealed::Empty;
        }
        let bound = table != "mcp_oauth_clients";
        let version = if raw.starts_with("enc:v2:") {
            2
        } else if raw.starts_with("enc:v1:") {
            1
        } else {
            0
        };
        if version == 2 && !bound {
            return Resealed::Failed("credential is bound to an unknown account");
        }
        let Some(keys) = self.keys() else {
            return Resealed::Failed("credential store unavailable");
        };
        let owner = bound.then_some(user);
        let Ok((plain, used)) = keys.open(&raw, owner) else {
            return Resealed::Failed("credential could not be opened");
        };
        let wanted = if bound { 2 } else { 1 };
        if used == KeyUsed::Current && version == wanted {
            return Resealed::Current;
        }
        let Ok(next) = keys.seal(&plain, owner, &random::<12>()) else {
            return Resealed::Failed("credential could not be sealed");
        };
        let cap = RustMcpCredentials::owned();
        let swapped = w.write(|tx| match table {
            "mcp_oauth_tokens" => tx.mcp_swap_token(cap, user, server, &next, now_ms(), &raw),
            "directory_mcp_user_keys" => tx.mcp_swap_key(cap, user, server, &next, &raw),
            _ => tx.mcp_swap_client(cap, server, &next, now_ms(), &raw),
        });
        match swapped {
            Ok(0) => Resealed::Failed("changed while rotating; run the rotation again"),
            Ok(_) if used == KeyUsed::Previous => Resealed::Rotated,
            Ok(_) => Resealed::Upgraded,
            Err(_) => Resealed::Failed("the row could not be written"),
        }
    }
}

// The reseal's report.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resealed {
    Current,
    Rotated,
    Upgraded,
    Empty,
    Failed(&'static str),
}

#[derive(Debug, Default)]
pub struct ResealReport {
    pub tables: Vec<(String, [usize; 5])>,
    pub failures: Vec<(String, Vec<String>, &'static str)>,
}

impl ResealReport {
    fn add(&mut self, table: &str, key: &[String], outcome: Resealed) {
        let at = match self.tables.iter().position(|(t, _)| t == table) {
            Some(i) => i,
            None => {
                self.tables.push((table.to_owned(), [0; 5]));
                self.tables.len() - 1
            }
        };
        let slot = match outcome {
            Resealed::Current => 0,
            Resealed::Rotated => 1,
            Resealed::Upgraded => 2,
            Resealed::Empty => 3,
            Resealed::Failed(why) => {
                self.failures.push((table.to_owned(), key.to_vec(), why));
                4
            }
        };
        if let Some((_, counts)) = self.tables.get_mut(at) {
            if let Some(n) = counts.get_mut(slot) {
                *n += 1;
            }
        }
    }

    /// `{ ok, tables: { name: { current, rotated, upgraded, empty, failed } }, failures }`. The three
    /// tables are always listed, so a pass over empty tables still names them.
    pub fn json(&self) -> JValue {
        let names = [
            "mcp_oauth_tokens",
            "mcp_oauth_clients",
            "directory_mcp_user_keys",
        ];
        let tables = names.iter().map(|name| {
            let counts = self
                .tables
                .iter()
                .find(|(t, _)| t == name)
                .map(|(_, c)| *c)
                .unwrap_or([0; 5]);
            let n = |i: usize| JValue::Num(counts.get(i).copied().unwrap_or(0) as f64);
            (
                (*name).to_owned(),
                JValue::obj([
                    ("current", n(0)),
                    ("rotated", n(1)),
                    ("upgraded", n(2)),
                    ("empty", n(3)),
                    ("failed", n(4)),
                ]),
            )
        });
        JValue::obj([
            ("ok", JValue::Bool(true)),
            ("tables", JValue::obj(tables)),
            (
                "failures",
                JValue::Arr(
                    self.failures
                        .iter()
                        .map(|(table, key, why)| {
                            JValue::obj([
                                ("table", JValue::Str(table.clone())),
                                (
                                    "ref",
                                    JValue::Arr(key.iter().cloned().map(JValue::Str).collect()),
                                ),
                                ("error", JValue::Str((*why).into())),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }
}

/// A form POST to the service: `pairs` as the form, and the app's own authentication added the way
/// its registration said (a secret in the form, or as Basic; an app without a secret sends neither).
/// Built whole before any await, so no part of it is held across one.
fn form_request(
    client: &Client,
    pairs: Vec<(&str, &str)>,
) -> (String, Vec<(&'static str, String)>) {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        form.append_pair(k, v);
    }
    let mut headers = vec![
        (
            "content-type",
            "application/x-www-form-urlencoded".to_owned(),
        ),
        ("accept", "application/json".to_owned()),
    ];
    if let Some(secret) = &client.client_secret {
        if client.auth_method() == "client_secret_post" {
            form.append_pair("client_secret", secret);
        } else if client.auth_method() != "none" {
            use base64::Engine as _;
            let basic = base64::engine::general_purpose::STANDARD.encode(format!(
                "{}:{}",
                uri_component(&client.client_id),
                uri_component(secret)
            ));
            headers.push(("authorization", format!("Basic {basic}")));
        }
    }
    (form.finish(), headers)
}

/// The `resource_metadata="…"` a `WWW-Authenticate` challenge hints (core's regex, case-insensitive).
fn resource_metadata_hint(challenge: &str) -> Option<String> {
    let lower = challenge.to_ascii_lowercase();
    let at = lower.find("resource_metadata=\"")? + "resource_metadata=\"".len();
    let rest = challenge.get(at..)?;
    let end = rest.find('"')?;
    Some(rest.get(..end)?.to_owned()).filter(|s| !s.is_empty())
}

/// Whether the resource a metadata document names is the server asked about (a trailing slash and
/// the default port do not count).
fn same_resource(named: &str, asked: &str) -> bool {
    let (Ok(a), Ok(b)) = (url::Url::parse(named), url::Url::parse(asked)) else {
        return false;
    };
    a.origin() == b.origin() && a.path().trim_end_matches('/') == b.path().trim_end_matches('/')
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn declared(name: &str, required: bool, template: Option<&str>) -> Declared {
        Declared {
            name: name.into(),
            required,
            template: template.map(str::to_owned),
        }
    }

    #[test]
    fn header_values_are_core_check_header_values() {
        let values = js_json::parse(r#"{"X-Api-Key":"  abc ","Other":"ignored"}"#).unwrap();
        let out = check_header_values(&[declared("X-Api-Key", true, None)], &values).unwrap();
        assert_eq!(out, vec![("X-Api-Key".to_string(), "abc".to_string())]);
        // A required one that is missing; an optional one that is skipped.
        let none = js_json::parse("{}").unwrap();
        let err = check_header_values(&[declared("X-Api-Key", true, None)], &none).unwrap_err();
        assert_eq!(
            (err.status, err.message.as_str()),
            (400, "Enter X-Api-Key.")
        );
        assert!(
            check_header_values(&[declared("X-Api-Key", false, None)], &none)
                .unwrap()
                .is_empty()
        );
        // One line, under 4 KB, no NUL.
        for bad in ["a\nb", "a\rb", "a\u{0}b"] {
            let v = JValue::obj([("K", JValue::Str(bad.into()))]);
            assert_eq!(
                check_header_values(&[declared("K", true, None)], &v)
                    .unwrap_err()
                    .message,
                "K must be a single line under 4 KB."
            );
        }
        let long = JValue::obj([("K", JValue::Str("x".repeat(4097)))]);
        assert!(check_header_values(&[declared("K", true, None)], &long).is_err());
        let edge = JValue::obj([("K", JValue::Str("x".repeat(4096)))]);
        assert!(check_header_values(&[declared("K", true, None)], &edge).is_ok());
        // Only the first {placeholder} of a template is replaced, by the value verbatim.
        let v = JValue::obj([("Authorization", JValue::Str("tok$&en".into()))]);
        let out = check_header_values(
            &[declared(
                "Authorization",
                true,
                Some("Bearer {token} {again}"),
            )],
            &v,
        )
        .unwrap();
        assert_eq!(out[0].1, "Bearer tok$&en {again}");
        // A non-string value is no value.
        let v = JValue::obj([("K", JValue::Num(5.0))]);
        assert_eq!(
            check_header_values(&[declared("K", true, None)], &v)
                .unwrap_err()
                .status,
            400
        );
    }

    #[test]
    fn the_challenge_hint_and_the_resource_match() {
        assert_eq!(
            resource_metadata_hint(
                r#"Bearer realm="x", Resource_Metadata="https://m.example/prm""#
            )
            .as_deref(),
            Some("https://m.example/prm")
        );
        assert_eq!(resource_metadata_hint("Bearer"), None);
        assert!(same_resource(
            "https://a.example/mcp/",
            "https://a.example/mcp"
        ));
        assert!(same_resource(
            "https://a.example:443/mcp",
            "https://a.example/mcp"
        ));
        assert!(!same_resource(
            "https://b.example/mcp",
            "https://a.example/mcp"
        ));
        assert!(!same_resource(
            "https://a.example/other",
            "https://a.example/mcp"
        ));
    }

    #[test]
    fn tokens_expire_inside_the_margin_and_die_without_a_refresh_token() {
        let t = |refresh: bool, expires: Option<f64>| Tokens {
            access: Zeroizing::new("a".into()),
            refresh: refresh.then(|| Zeroizing::new("r".into())),
            expires_at: expires,
        };
        let now = 1_000_000;
        assert!(!t(true, None).expiring(now));
        assert!(!t(true, Some(now as f64 + 61_000.0)).expiring(now));
        assert!(t(true, Some(now as f64 + 60_000.0)).expiring(now));
        assert!(t(true, Some(now as f64 - 1.0)).expired(now));
        assert!(!t(true, Some(now as f64 - 1.0)).dead(now));
        assert!(t(false, Some(now as f64 - 1.0)).dead(now));
        assert!(!t(false, Some(now as f64 + 5.0)).dead(now));
        // The stored JSON is core's three keys in core's order; an unset expiry is null.
        assert_eq!(
            &*t(true, Some(5.0)).to_json(),
            r#"{"accessToken":"a","refreshToken":"r","expiresAt":5}"#
        );
        assert_eq!(
            &*t(false, None).to_json(),
            r#"{"accessToken":"a","refreshToken":null,"expiresAt":null}"#
        );
    }

    #[test]
    fn a_client_row_round_trips_in_core_key_order() {
        let c = Client {
            resource: "https://s.example/mcp".into(),
            issuer: "https://as.example".into(),
            authorization_endpoint: "https://as.example/authorize".into(),
            token_endpoint: "https://as.example/token".into(),
            registration_endpoint: Some("https://as.example/register".into()),
            revocation_endpoint: None,
            secret_method: "client_secret_basic".into(),
            scopes: vec!["read".into()],
            client_id: "cid".into(),
            client_secret: None,
            redirect_uri: "https://noevia.example/api/mcp-oauth/callback".into(),
            token_auth: None,
            manual: false,
        };
        let json = c.to_json();
        assert_eq!(
            &*json,
            r#"{"resource":"https://s.example/mcp","issuer":"https://as.example","authorizationEndpoint":"https://as.example/authorize","tokenEndpoint":"https://as.example/token","registrationEndpoint":"https://as.example/register","secretMethod":"client_secret_basic","scopes":["read"],"clientId":"cid","clientSecret":null,"redirectUri":"https://noevia.example/api/mcp-oauth/callback"}"#
        );
        let back = Client::from_json(&js_json::parse(&json).unwrap()).unwrap();
        assert_eq!(back.client_id, "cid");
        assert!(!back.manual && back.client_secret.is_none());
        // A row core wrote for a registered app has no tokenAuth: the metadata's method is used.
        assert_eq!(back.auth_method(), "client_secret_basic");
    }

    #[test]
    fn small_encoders_match_the_js_ones() {
        assert_eq!(uri_component("a b/c:d~!'()*-_."), "a%20b%2Fc%3Ad~!'()*-_.");
        assert_eq!(generation_of("x").len(), 32);
        assert_ne!(generation_of("x"), generation_of("y"));
    }
}
