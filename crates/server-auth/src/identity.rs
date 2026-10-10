//! core device-auth.cjs `createRequestAuth` over auth.cjs `authenticate` / `csrfValid` /
//! `originValid` and device-auth.cjs `authenticate` / `browserOnly`, read-only.
//!
//! Verdicts are Node's. What Node also does and this does not (it never writes `cowork.db`):
//! a valid session's `last_seen_at` is not moved to now, a rejected session row is not deleted,
//! and a device grant's `last_used_at` is not touched. While Node serves every route it does all
//! three on each request, so idle expiry is unchanged; a Rust-owned route will need
//! `sessions`/`device_grants` in server-store's OWNED_TABLES (or a Node call) before it can
//! keep a session alive by itself (M3).

use crate::config::AuthConfig;
use crate::js;
use crate::request::{self, Creds};
use server_store::rusqlite::types::Value;
use server_store::{Reader, Store, StoreError};

/// auth.cjs `IDLE_MS`.
pub const IDLE_MS: f64 = 7.0 * 24.0 * 60.0 * 60.0 * 1000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    Member,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }
    fn parse(v: &Value) -> Option<Role> {
        match js::text(v)? {
            "admin" => Some(Role::Admin),
            "member" => Some(Role::Member),
            _ => None,
        }
    }
}

/// auth.cjs `publicUser(row)`. `role` is the effective role (`member` for a device token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicUser {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub role: Role,
    pub disabled: bool,
    pub diary_enabled: bool,
    pub onboarded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// A `cowork_session` cookie. `csrf_hash` is the row's, for [`csrf_valid`].
    Session { id_hash: String, csrf_hash: String },
    /// UI_AUTH_TOKEN with LEGACY_AUTH_COMPAT: acts as the oldest enabled admin.
    Legacy,
    /// A native-client access token (`nva_…`).
    Device {
        grant_id: String,
        client_name: String,
        expires_at: i64,
    },
}

/// A validated identity: Node's `authn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub user: PublicUser,
    /// The account's own role (`authn.accountRole`, set for device tokens only; equals
    /// `user.role` otherwise).
    pub account_role: Role,
    pub credential: Credential,
}

impl Identity {
    pub fn is_device(&self) -> bool {
        matches!(self.credential, Credential::Device { .. })
    }
}

fn get(row: &server_store::rusqlite::Row<'_>, name: &str) -> server_store::rusqlite::Result<Value> {
    row.get::<_, Value>(name)
}

struct UserRow {
    id: Value,
    username: Value,
    display_name: Value,
    role: Value,
    disabled_at: Value,
}

const USER_COLS: &str = "u.id AS id, u.username AS username, u.display_name AS display_name, u.role AS role, u.disabled_at AS disabled_at";

fn user_row(row: &server_store::rusqlite::Row<'_>) -> server_store::rusqlite::Result<UserRow> {
    Ok(UserRow {
        id: get(row, "id")?,
        username: get(row, "username")?,
        display_name: get(row, "display_name")?,
        role: get(row, "role")?,
        disabled_at: get(row, "disabled_at")?,
    })
}

/// `publicUser(row)`; `None` (refused) for a row Node could not have written.
fn public_user(r: &Reader<'_>, u: &UserRow) -> Result<Option<PublicUser>, StoreError> {
    let (Some(id), Some(username), Some(display_name), Some(role)) = (
        js::text(&u.id),
        js::text(&u.username),
        js::text(&u.display_name),
        Role::parse(&u.role),
    ) else {
        return Ok(None);
    };
    let feature = r.row(
        "SELECT diary_enabled, onboarded FROM user_features WHERE user_id=?1",
        [id],
        |row| Ok((row.get::<_, Value>(0)?, row.get::<_, Value>(1)?)),
    )?;
    let (diary_enabled, onboarded) = match feature {
        // `!!feature?.diary_enabled`, `!!(feature?.onboarded ?? 1)`.
        Some((d, o)) => (js::truthy(&d), matches!(o, Value::Null) || js::truthy(&o)),
        None => (false, true),
    };
    Ok(Some(PublicUser {
        id: id.to_string(),
        username: username.to_string(),
        display_name: display_name.to_string(),
        role,
        disabled: js::truthy(&u.disabled_at),
        diary_enabled,
        onboarded,
    }))
}

/// `a > b` in JS for the numbers [`js::number`] accepts; false otherwise.
fn gt(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| a > b)
}

/// The request gate's credential check, Node's `requestAuth.authenticate(req)`.
pub struct Authenticator {
    config: AuthConfig,
}

impl Authenticator {
    pub fn new(config: AuthConfig) -> Self {
        Authenticator { config }
    }

    pub fn config(&self) -> &AuthConfig {
        &self.config
    }

    /// `features.enabled('nativeClientAuth')`, read from the settings Node keeps.
    pub fn native_client_auth(&self, r: &Reader<'_>) -> Result<bool, StoreError> {
        let stored = setting(r, "feature:nativeClientAuth")?;
        Ok(self.config.native_client_auth(stored.as_deref()))
    }

    /// `Ok(None)` is Node's `null` (401 on a non-public /api/ route). A store error fails closed:
    /// callers answer 503, never "signed in".
    pub fn authenticate(
        &self,
        store: &Store,
        creds: &Creds,
        now_ms: i64,
    ) -> Result<Option<Identity>, StoreError> {
        store.read(|r| self.authenticate_in(r, creds, now_ms))
    }

    pub fn authenticate_in(
        &self,
        r: &Reader<'_>,
        creds: &Creds,
        now_ms: i64,
    ) -> Result<Option<Identity>, StoreError> {
        let bearer = request::bearer_token(creds);
        if !bearer.is_empty() && self.native_client_auth(r)? {
            if request::has_session_cookie(creds) {
                return Ok(None);
            }
            return device(r, bearer, now_ms);
        }
        self.session_or_legacy(r, creds, now_ms)
    }

    /// auth.cjs `authenticate`.
    fn session_or_legacy(
        &self,
        r: &Reader<'_>,
        creds: &Creds,
        now_ms: i64,
    ) -> Result<Option<Identity>, StoreError> {
        let now = now_ms as f64;
        if let Some(raw) = creds.cookie("cowork_session").filter(|v| !v.is_empty()) {
            let sql = format!(
                "SELECT s.id_hash AS id_hash, s.csrf_hash AS csrf_hash, s.expires_at AS expires_at, s.last_seen_at AS last_seen_at, {USER_COLS}
                 FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.id_hash=?1"
            );
            let found = r.row(&sql, [js::digest(&raw)], |row| {
                Ok((
                    user_row(row)?,
                    get(row, "id_hash")?,
                    get(row, "csrf_hash")?,
                    get(row, "expires_at")?,
                    get(row, "last_seen_at")?,
                ))
            })?;
            if let Some((u, id_hash, csrf_hash, expires_at, last_seen_at)) = found {
                let live = !js::truthy(&u.disabled_at)
                    && gt(js::number(&expires_at), now)
                    && gt(js::number(&last_seen_at).map(|l| l + IDLE_MS), now);
                if live {
                    if let (Some(user), Some(id_hash), Some(csrf_hash)) = (
                        public_user(r, &u)?,
                        js::text(&id_hash),
                        js::text(&csrf_hash),
                    ) {
                        return Ok(Some(Identity {
                            account_role: user.role,
                            user,
                            credential: Credential::Session {
                                id_hash: id_hash.to_string(),
                                csrf_hash: csrf_hash.to_string(),
                            },
                        }));
                    }
                }
            }
            // Node deletes the row here; read-only, it stays (it is refused again next time).
        }
        let cfg = &self.config;
        if cfg.legacy_compat && !cfg.legacy_token.is_empty() && user_count(r)? > 0 {
            // timingSafeEqual after a length check in Node; here the digests are compared in
            // constant time, so neither the length nor the first difference leaks.
            if js::ct_eq(request::legacy_supplied(creds), &cfg.legacy_token) {
                let sql = format!(
                    "SELECT {USER_COLS} FROM users u WHERE u.role='admin' AND u.disabled_at IS NULL ORDER BY u.created_at LIMIT 1"
                );
                if let Some(u) = r.row(&sql, [], user_row)? {
                    if let Some(user) = public_user(r, &u)? {
                        return Ok(Some(Identity {
                            account_role: user.role,
                            user,
                            credential: Credential::Legacy,
                        }));
                    }
                }
            }
        }
        Ok(None)
    }

    /// `requestAuth.csrfValid(req, authn)`.
    pub fn csrf_valid(&self, creds: &Creds, authn: &Identity) -> bool {
        match &authn.credential {
            Credential::Legacy => true,
            Credential::Device { .. } => !request::has_session_cookie(creds),
            Credential::Session { csrf_hash, .. } => {
                let value = creds.csrf.as_deref().unwrap_or("");
                let cookie = creds.cookie("cowork_csrf").unwrap_or_default();
                // value && value === cookie && digest(value) === csrf_hash, with both
                // comparisons constant-time and both always evaluated.
                let same = js::ct_eq(value, &cookie);
                let stored = {
                    use subtle::ConstantTimeEq;
                    bool::from(js::digest(value).as_bytes().ct_eq(csrf_hash.as_bytes()))
                };
                !value.is_empty() & same & stored
            }
        }
    }

    /// auth.cjs `originValid(req)`. Node holds `origin` and the trusted set in memory from boot
    /// (and updates them on Settings → Web address); this reads the same settings rows each time:
    /// `public_origin_admin || PUBLIC_ORIGIN || public_origin`, and ADDITIONAL_TRUSTED_ORIGINS plus
    /// `previous_origins`. Where the two could differ (an origin renamed out of the five kept, until
    /// Node restarts; first-run setup choosing an address other than PUBLIC_ORIGIN, see
    /// [`CurrentOrigin::Ambiguous`]) this one refuses and Node accepts, never the reverse.
    pub fn origin_valid(&self, r: &Reader<'_>, creds: &Creds) -> Result<bool, StoreError> {
        let current = self.current_origin(r)?;
        let supplied = creds.origin.as_deref().unwrap_or("");
        match &current {
            CurrentOrigin::Unset => return Ok(true),
            _ if supplied.is_empty() => return Ok(true),
            CurrentOrigin::Exact(o) if o == supplied => return Ok(true),
            _ => {}
        }
        if self.config.additional_origins.iter().any(|o| o == supplied) {
            return Ok(true);
        }
        Ok(previous_origins(r)?.iter().any(|o| o == supplied))
    }

    /// The origin Node's `originValid` compares with, as far as the database can tell.
    pub fn current_origin(&self, r: &Reader<'_>) -> Result<CurrentOrigin, StoreError> {
        if let Some(a) = setting(r, "public_origin_admin")?.filter(|s| !s.is_empty()) {
            return Ok(CurrentOrigin::Exact(a));
        }
        let setup = setting(r, "public_origin")?.unwrap_or_default();
        if !self.config.public_origin.is_empty() {
            if !setup.is_empty() && setup != self.config.public_origin {
                return Ok(CurrentOrigin::Ambiguous);
            }
            return Ok(CurrentOrigin::Exact(self.config.public_origin.clone()));
        }
        if setup.is_empty() {
            return Ok(CurrentOrigin::Unset);
        }
        Ok(CurrentOrigin::Exact(setup))
    }
}

/// What [`Authenticator::current_origin`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentOrigin {
    /// No address anywhere: Node's `originValid` accepts every request.
    Unset,
    /// The one address Node compares with.
    Exact(String),
    /// No `public_origin_admin`, PUBLIC_ORIGIN set, and a first-run setup address that differs.
    /// Node compares with the setup address from setup until it restarts and with PUBLIC_ORIGIN
    /// after; nothing in the database says which, so neither matches (only an empty Origin,
    /// ADDITIONAL_TRUSTED_ORIGINS and `previous_origins` are accepted).
    Ambiguous,
}

/// `JSON.parse(setting('previous_origins') || '[]').filter(o => typeof o === 'string')`, `[]` when
/// it does not parse.
fn previous_origins(r: &Reader<'_>) -> Result<Vec<String>, StoreError> {
    let raw = setting(r, "previous_origins")?.filter(|s| !s.is_empty());
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    Ok(match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    })
}

fn setting(r: &Reader<'_>, key: &str) -> Result<Option<String>, StoreError> {
    let v = r.row("SELECT value FROM settings WHERE key=?1", [key], |row| {
        row.get::<_, Value>(0)
    })?;
    Ok(v.as_ref().and_then(js::text).map(str::to_string))
}

fn user_count(r: &Reader<'_>) -> Result<i64, StoreError> {
    Ok(
        r.row("SELECT count(*) FROM users", [], |row| row.get::<_, i64>(0))?
            .unwrap_or(0),
    )
}

/// device-auth.cjs `authenticate(req)`.
fn device(r: &Reader<'_>, raw: &str, now_ms: i64) -> Result<Option<Identity>, StoreError> {
    let at = now_ms as f64;
    let row = r.row(
        "SELECT t.kind AS kind, t.expires_at AS token_expires_at, g.id AS grant_id, g.user_id AS user_id,
                g.client_name AS client_name, g.expires_at AS grant_expires_at
         FROM device_tokens t JOIN device_grants g ON g.id=t.grant_id WHERE t.token_hash=?1",
        [js::digest(raw)],
        |row| {
            Ok((
                get(row, "kind")?,
                get(row, "token_expires_at")?,
                get(row, "grant_id")?,
                get(row, "user_id")?,
                get(row, "client_name")?,
                get(row, "grant_expires_at")?,
            ))
        },
    )?;
    let Some((kind, token_expires_at, grant_id, user_id, client_name, grant_expires_at)) = row
    else {
        return Ok(None);
    };
    if js::text(&kind) != Some("access")
        || !gt(js::number(&token_expires_at), at)
        || !gt(js::number(&grant_expires_at), at)
    {
        return Ok(None);
    }
    let sql = format!("SELECT {USER_COLS} FROM users u WHERE u.id=?1");
    let Some(u) = r.row(&sql, [&user_id], user_row)? else {
        return Ok(None);
    };
    if js::truthy(&u.disabled_at) {
        return Ok(None);
    }
    let (Some(account), Some(grant_id), Some(client_name), Value::Integer(expires_at)) = (
        public_user(r, &u)?,
        js::text(&grant_id),
        js::text(&client_name),
        grant_expires_at,
    ) else {
        return Ok(None);
    };
    // Node touches device_grants.last_used_at here (at most once a minute); read-only, it does not.
    Ok(Some(Identity {
        account_role: account.role,
        user: PublicUser {
            role: Role::Member,
            ..account
        },
        credential: Credential::Device {
            grant_id: grant_id.to_string(),
            client_name: client_name.to_string(),
            expires_at,
        },
    }))
}

/// device-auth.cjs `BROWSER_ONLY`: prefixes on a segment boundary, any method.
pub const BROWSER_ONLY: &[&str] = &[
    "/api/admin",
    "/api/auth/passkeys",
    "/api/auth/sessions",
    "/api/auth/devices",
    "/api/auth/device/lookup",
    "/api/auth/device/approve",
    "/api/profile/app-passwords",
    "/api/profile/diary-connectors",
    "/api/profile/sharing",
    "/api/integrations/storage/nextcloud",
    "/api/mcp-keys",
    "/api/mcp-oauth",
    "/api/providers/chatgpt",
];
const BROWSER_ONLY_EXACT: &[&str] = &["/api/profile"];
const BROWSER_ONLY_WRITES: &[&str] = &[
    "/api/integrations/storage",
    "/api/integrations/storage/test",
];
const BROWSER_ONLY_WRITE_PREFIXES: &[&str] = &["/api/connectors/"];

/// device-auth.cjs `browserOnly(pathname, method)`. `pathname` is the WHATWG-parsed path Node
/// routes on.
pub fn browser_only_path(pathname: &str, method: &str) -> bool {
    if BROWSER_ONLY_EXACT.contains(&pathname) {
        return true;
    }
    let m = method.to_uppercase();
    let write = m != "GET" && m != "HEAD";
    if write
        && (BROWSER_ONLY_WRITES.contains(&pathname)
            || BROWSER_ONLY_WRITE_PREFIXES
                .iter()
                .any(|p| pathname.starts_with(p)))
    {
        return true;
    }
    BROWSER_ONLY.iter().any(|p| {
        pathname == *p
            || pathname
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// `requestAuth.browserOnly(authn, pathname, method)`.
pub fn browser_only(authn: &Identity, pathname: &str, method: &str) -> bool {
    authn.is_device() && browser_only_path(pathname, method)
}

/// Why core index.cjs refuses a signed-in /api/ request before any route runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 401 `{"error":"unauthorized"}` (http.cjs `unauthorized`).
    Unauthorized,
    /// 403 `{"error":"invalid CSRF token"}`.
    Csrf,
    /// 403 `{"error":"This needs a signed-in browser session.","code":"browser_session_required"}`.
    BrowserOnly,
}

impl Authenticator {
    /// core index.cjs handleRequestScoped's gate for an /api/ path outside `publicAuthRoutes`:
    /// no identity is 401; a write (not GET/HEAD/OPTIONS) needs a valid origin and CSRF; a
    /// device token is refused on browser-only paths.
    pub fn gate(
        &self,
        store: &Store,
        creds: &Creds,
        pathname: &str,
        now_ms: i64,
    ) -> Result<Result<Identity, Refusal>, StoreError> {
        store.read(|r| self.gate_in(r, creds, pathname, now_ms))
    }

    pub fn gate_in(
        &self,
        r: &Reader<'_>,
        creds: &Creds,
        pathname: &str,
        now_ms: i64,
    ) -> Result<Result<Identity, Refusal>, StoreError> {
        let Some(authn) = self.authenticate_in(r, creds, now_ms)? else {
            return Ok(Err(Refusal::Unauthorized));
        };
        let method = creds.method.as_str();
        if !matches!(method, "GET" | "HEAD" | "OPTIONS")
            && (!self.origin_valid(r, creds)? || !self.csrf_valid(creds, &authn))
        {
            return Ok(Err(Refusal::Csrf));
        }
        if browser_only(&authn, pathname, method) {
            return Ok(Err(Refusal::BrowserOnly));
        }
        Ok(Ok(authn))
    }
}
