//! The front's side of MCP discovery (sbstndalton/noevia row T): the [`Backend`] the actor works in,
//! over the directory table, the credential broker and the two guarded HTTP clients, and the
//! operator's configuration (`MCP_SERVERS` / `MCP_SERVER_URL`, `ENABLED_TOOLBOXES`,
//! `MCP_NEXTCLOUD_ORIGINS`) read once, as core read it at boot.
//!
//! Credentials: discovery uses only service credentials (see the actor's docs). The in-process
//! server's discovery-only capability token is minted here exactly as core `mcp-internal.cjs`
//! `mintToken` does (`HMAC-SHA256` over the base64url payload, key `derive('mcp-internal-token')`), so
//! the Node listener that verifies it needs no change; it can list the catalogue and can never call.
use crate::mcp_broker::Mcp;
use crate::mcp_discovery_actor::{
    Actor, Backend, Ready, ServerSpec, Snapshot, Timing, Unavailable,
};
use crate::mcp_egress::Egress;
use crate::{identity, App};
use js_json::JValue;
use prompt_framing::js::units;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Weak};

/// How long a discovery-only capability token lives (core `mcpDiscoveryAuth`).
const DISCOVERY_TOKEN_TTL_MS: i64 = 120_000;
/// Listings that may run at once: every configured server is listed together, as core did.
const LIST_PLACES: usize = 64;

/// The actor over the running app.
pub type Discovery = Actor<AppBackend>;

/// What the app asks of discovery (an object-safe view of the actor, so a test can stand one in).
pub trait Source: Send + Sync {
    fn discover<'a>(
        &'a self,
        force: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Arc<Snapshot>, Unavailable>> + Send + 'a>,
    >;
    /// What is known now, with no discovery.
    fn current(&self) -> Arc<Snapshot>;
}

impl<B: Backend> Source for Actor<B> {
    fn discover<'a>(
        &'a self,
        force: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Arc<Snapshot>, Unavailable>> + Send + 'a>,
    > {
        Box::pin(async move {
            // The actor is held in an `Arc`; `discover` needs it.
            match self.me.upgrade() {
                Some(me) => me.discover(force).await,
                None => Err(Unavailable),
            }
        })
    }
    fn current(&self) -> Arc<Snapshot> {
        Actor::current(self)
    }
}

/// The operator's configuration and the actor.
pub struct Handle {
    pub source: Arc<dyn Source>,
    /// `ENABLED_TOOLBOXES` as `parseEnabledToolboxes` returns it (`None`: everything is offered).
    enabled_toolboxes: Option<Vec<Vec<u16>>>,
    /// `MCP_NEXTCLOUD_ORIGINS`, trailing slashes removed (core `parseNextcloudOrigins`).
    pub nextcloud_origins: Vec<String>,
}

impl Handle {
    /// A handle over `source` with the operator's `ENABLED_TOOLBOXES` (`None`: everything is offered) and
    /// `MCP_NEXTCLOUD_ORIGINS`.
    pub fn new(
        source: Arc<dyn Source>,
        enabled_toolboxes: Option<Vec<Vec<u16>>>,
        nextcloud_origins: Vec<String>,
    ) -> Handle {
        Handle {
            source,
            enabled_toolboxes,
            nextcloud_origins,
        }
    }

    /// `toolboxOffered(id)`.
    pub fn offered(&self, id: &str) -> bool {
        mcp_servers::toolbox_offered(self.enabled_toolboxes.as_deref(), &units(id))
    }
}

/// `parseNextcloudOrigins`.
pub fn parse_nextcloud_origins(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|x| x.trim().trim_end_matches('/').to_owned())
        .filter(|x| !x.is_empty())
        .collect()
}

fn units_of(value: &Option<String>) -> Option<Vec<u16>> {
    value
        .as_ref()
        .map(|v| v.encode_utf16().collect::<Vec<u16>>())
}

/// The operator's servers (`parseMcpServers(env)`); a list the parser will not judge configures none.
fn operator_servers(settings: &crate::mcp_broker::Settings) -> Vec<ServerSpec> {
    let list = units_of(&settings.servers);
    let single = units_of(&settings.server_url);
    let Ok(parsed) = mcp_servers::parse_servers(list.as_deref(), single.as_deref()) else {
        return Vec::new();
    };
    parsed
        .servers
        .iter()
        .map(|s| ServerSpec {
            id: String::from_utf16_lossy(&s.id),
            url: String::from_utf16_lossy(&s.url),
            auth: s.auth.name().to_owned(),
            directory: false,
            title: None,
            added_by: None,
            token_env: s.token_env.as_ref().map(|e| String::from_utf16_lossy(e)),
        })
        .collect()
}

fn timing(settings: &crate::mcp_broker::Settings) -> Timing {
    let mut timing = Timing::default();
    if let Some(fail) = settings.discovery_fail_ttl_ms {
        timing.fail_ttl_ms = fail as i64;
    }
    timing
}

impl Handle {
    /// A handle over `source` with the operator's configuration.
    pub fn configured(settings: &crate::mcp_broker::Settings, source: Arc<dyn Source>) -> Handle {
        Handle::new(
            source,
            mcp_servers::parse_toolboxes(units_of(&settings.enabled_toolboxes).as_deref()),
            parse_nextcloud_origins(&settings.nextcloud_origins),
        )
    }
}

/// The handle for this app, made on first use. `None`: no data directory or no credential broker, so
/// nothing can be discovered (every caller fails closed).
pub fn handle(app: &Arc<App>) -> Option<&Arc<Handle>> {
    app.mcp_discovery
        .get_or_init(|| {
            app.mcp.as_ref()?;
            let settings = &app.config.mcp;
            let guarded = Egress::for_discovery(settings.qa_loopback, LIST_PLACES)?;
            let open = Egress::operator(LIST_PLACES)?;
            let backend = Arc::new(AppBackend {
                app: Arc::downgrade(app),
                operator: operator_servers(settings),
                guarded,
                open,
            });
            Some(Arc::new(Handle::configured(
                settings,
                Actor::new(backend, timing(settings)),
            )))
        })
        .as_ref()
}

/// Starts the first listing in the background (no waiting: a server still starting is retried by the
/// cadence).
pub fn warm(app: &Arc<App>) {
    let Some(handle) = handle(app) else {
        return;
    };
    let handle = Arc::clone(handle);
    tokio::spawn(async move {
        let _ = handle.source.discover(false).await;
    });
}

/// What the actor works in.
pub struct AppBackend {
    app: Weak<App>,
    operator: Vec<ServerSpec>,
    /// Directory servers: public addresses only, checked again at connect time.
    guarded: Arc<Egress>,
    /// The operator's own servers: any address the operator configured.
    open: Arc<Egress>,
}

impl AppBackend {
    fn app(&self) -> Option<Arc<App>> {
        self.app.upgrade()
    }

    fn mcp(&self) -> Option<(Arc<App>, Arc<Mcp>)> {
        let app = self.app()?;
        let mcp = app.mcp.as_ref().map(Arc::clone)?;
        Some((app, mcp))
    }
}

/// The signal of the directory table: changes when a row, a flag or a sealed key does, and is
/// `"unreadable"` while the table cannot be read.
fn signal_of(rows: Option<&[crate::mcp_directory::Row]>) -> String {
    let Some(rows) = rows else {
        return "unreadable".into();
    };
    let mut h = Sha256::new();
    for r in rows {
        for part in [
            r.id.as_str(),
            r.registry_name.as_str(),
            r.title.as_str(),
            r.url.as_str(),
            r.added_by.as_deref().unwrap_or("\u{0}"),
            r.headers_enc.as_deref().unwrap_or("\u{0}"),
        ] {
            h.update((part.len() as u64).to_be_bytes());
            h.update(part.as_bytes());
        }
        h.update(r.added_at.to_be_bytes());
        h.update([u8::from(r.oauth), u8::from(r.personal)]);
        let declared = js_json::stringify(&r.declared).unwrap_or_default();
        h.update((declared.len() as u64).to_be_bytes());
        h.update(declared.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The directory's servers in `MCP_SERVERS` shape (core `directoryMcp.asServers()`).
fn specs_of(mcp: &Mcp, rows: &[crate::mcp_directory::Row]) -> Vec<ServerSpec> {
    rows.iter()
        .map(|r| ServerSpec {
            id: r.id.clone(),
            url: r.url.clone(),
            auth: if r.oauth {
                "oauth"
            } else if r.personal {
                "personal"
            } else if !mcp.key_header_names(r).is_empty() {
                "directory"
            } else {
                "none"
            }
            .to_owned(),
            directory: true,
            title: Some(r.title.clone()),
            added_by: r.added_by.clone(),
            token_env: None,
        })
        .collect()
}

fn b64url(bytes: &[u8]) -> String {
    secret_envelope::encode_base64url(bytes)
}

/// HMAC-SHA256 (RFC 2104).
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    let digest;
    let key = if key.len() > 64 {
        digest = Sha256::digest(key);
        digest.as_slice()
    } else {
        key
    };
    for (slot, byte) in block.iter_mut().zip(key) {
        *slot = *byte;
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// core `mcp-internal.cjs` `mintToken(key, { discovery: true, ttlMs })` with no user and no project.
pub fn mint_discovery_token(key: &[u8], now_ms: i64, jti: &str) -> String {
    let payload = format!(
        r#"{{"v":1,"uid":null,"pid":null,"cid":null,"jti":"{jti}","exp":{},"w":0,"d":1}}"#,
        now_ms + DISCOVERY_TOKEN_TTL_MS
    );
    let body = b64url(payload.as_bytes());
    let signature = hmac_sha256(key, body.as_bytes());
    format!("{body}.{}", b64url(&signature))
}

/// A random version-4 UUID, as `crypto.randomUUID()`.
fn uuid_v4() -> String {
    let mut b = crate::mcp_broker::random::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

impl Backend for AppBackend {
    fn now_ms(&self) -> i64 {
        identity::now_ms()
    }

    async fn diary_mcp_write(&self) -> bool {
        let Some(app) = self.app() else {
            return false;
        };
        tokio::task::spawn_blocking(move || {
            let feature = server_features::Feature::DiaryMcpWrite;
            let spec = server_features::spec(feature);
            let index = server_features::Feature::ALL
                .iter()
                .position(|f| *f == feature);
            let env = index.and_then(|i| app.config.feature_context_env.get(i).copied().flatten());
            let stored = app.writes.writer().ok().and_then(|w| {
                w.read(|r| {
                    r.row(
                        "SELECT CASE WHEN typeof(value)='text' AND value IN ('true','false') THEN value ELSE NULL END FROM settings WHERE key=?1",
                        [spec.setting],
                        |row| row.get::<_, Option<String>>(0),
                    )
                })
                .ok()
                .flatten()
                .flatten()
            });
            server_features::evaluate(feature, env, stored.as_deref(), true).enabled
        })
        .await
        .unwrap_or(false)
    }

    async fn servers(&self) -> Vec<ServerSpec> {
        let mut out = self.operator.clone();
        let Some((app, mcp)) = self.mcp() else {
            return out;
        };
        // A table that cannot be read is no directory servers, never the last list.
        let specs = tokio::task::spawn_blocking(move || {
            mcp.dir_rows(&app).ok().map(|rows| specs_of(&mcp, &rows))
        })
        .await
        .ok()
        .flatten();
        out.extend(specs.unwrap_or_default());
        out
    }

    async fn configuration(&self) -> Option<(String, Vec<ServerSpec>)> {
        let mut servers = self.operator.clone();
        let Some((app, mcp)) = self.mcp() else {
            return Some(("unreadable".into(), servers));
        };
        // Identity and specifications come from the same database observation.
        let observed = tokio::task::spawn_blocking(move || {
            let rows = mcp.dir_rows(&app).ok();
            let signal = signal_of(rows.as_deref());
            let specs = rows
                .as_ref()
                .map(|rows| specs_of(&mcp, rows))
                .unwrap_or_default();
            (signal, specs)
        })
        .await
        .ok()?;
        servers.extend(observed.1);
        Some((observed.0, servers))
    }

    async fn directory_signal(&self) -> String {
        let Some((app, mcp)) = self.mcp() else {
            return "unreadable".into();
        };
        let rows = tokio::task::spawn_blocking(move || mcp.dir_rows(&app).ok())
            .await
            .ok()
            .flatten();
        signal_of(rows.as_deref())
    }

    async fn account_ready(&self, user: &str) -> Ready {
        let Some((app, mcp)) = self.mcp() else {
            return Ready::default();
        };
        let user = user.to_owned();
        tokio::task::spawn_blocking(move || mcp.ready_for(&app, &user).ok())
            .await
            .ok()
            .flatten()
            .map(|r| Ready {
                signed_in: r.signed_in,
                keys: r.keys,
            })
            .unwrap_or_default()
    }

    fn static_token(&self, env: &str) -> Option<String> {
        std::env::var(env).ok().filter(|v| !v.is_empty())
    }

    async fn directory_headers(&self, id: &str) -> Vec<(String, String)> {
        let Some((app, mcp)) = self.mcp() else {
            return Vec::new();
        };
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            mcp.dir_rows(&app)
                .ok()
                .and_then(|rows| rows.into_iter().find(|r| r.id == id))
                .map(|row| mcp.shared_headers(&row))
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    }

    fn internal_discovery_token(&self) -> Option<String> {
        let (_, mcp) = self.mcp()?;
        let keys = mcp.keys()?;
        let (key, _) = keys.derive_with(|secret| {
            google_token_seal::derive_secret_key(secret, "mcp-internal-token")
        });
        Some(mint_discovery_token(
            key.as_slice(),
            identity::now_ms(),
            &uuid_v4(),
        ))
    }

    async fn user_disabled(&self, user: &str) -> bool {
        let Some(app) = self.app() else {
            // Unknown: do not trust a credential on a guess.
            return true;
        };
        let user = user.to_owned();
        tokio::task::spawn_blocking(move || {
            let found = app.writes.writer().ok().and_then(|w| {
                w.read(|r| {
                    r.row(
                        "SELECT disabled_at FROM users WHERE id=?1",
                        [user.as_str()],
                        |row| row.get::<_, server_store::rusqlite::types::Value>(0),
                    )
                })
                .ok()
                .flatten()
            });
            found.is_some_and(|v| server_auth::js::truthy(&v))
        })
        .await
        .unwrap_or(true)
    }

    async fn oauth_token(&self, user: &str, server: &str) -> Result<Option<String>, String> {
        let Some((app, mcp)) = self.mcp() else {
            return Err(
                "MCP sign-ins and keys are unavailable right now. Try again in a moment.".into(),
            );
        };
        if user.is_empty() {
            return Ok(None);
        }
        match mcp.session(&app, user, server, None).await {
            Ok(issued) => Ok(Some(issued.access.to_string())),
            Err(e) if matches!(e.code, Some("disconnected" | "reconnect")) => Ok(None),
            Err(e) => Err(e.message),
        }
    }

    async fn key_headers(
        &self,
        user: &str,
        server: &str,
    ) -> Result<Option<Vec<(String, String)>>, String> {
        let Some((app, mcp)) = self.mcp() else {
            return Err(
                "MCP sign-ins and keys are unavailable right now. Try again in a moment.".into(),
            );
        };
        if user.is_empty() {
            return Ok(None);
        }
        match mcp.key_headers(&app, user, server).await {
            Ok(headers) => Ok(Some(headers)),
            Err(e) if matches!(e.code, Some("disconnected" | "reconnect")) => Ok(None),
            Err(e) => Err(e.message),
        }
    }

    async fn url_public(&self, url: &str) -> bool {
        self.guarded.url_allowed(url).await
    }

    async fn list_tools(
        &self,
        server: &ServerSpec,
        headers: &[(String, String)],
    ) -> Result<Vec<JValue>, String> {
        let Some((_, mcp)) = self.mcp() else {
            return Err("fetch failed".into());
        };
        let egress = if server.directory {
            &self.guarded
        } else {
            &self.open
        };
        crate::mcp_egress::list_tools(egress, &server.url, headers, &mcp.rpc_id).await
    }
}
