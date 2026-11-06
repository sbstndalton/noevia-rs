//! The one executor of an MCP tool call (sbstndalton/noevia#2218 and the chat's tool runtime): core
//! `mcp-wiring.cjs` `executeMcpToolCall`, answer for answer. It routes the call (a name only one
//! server offers is unambiguous; one two offer is refused rather than guessed), keeps an approved
//! project-file edit from being routed anywhere but noevia's own server, attaches the account's own
//! credential (a Nextcloud connection, a key, a sign-in; a static service token; noevia's internal
//! capability token), applies the per-account rate-limit wait, runs one short MCP session
//! (connect, call, disconnect, with one retry after a refused sign-in) and returns the model's text.
//!
//! Like core, it **returns** a failure as text (`ERROR ...`, written for the model) and never throws.
//!
//! Two callers: deep research ([`crate::research_routes`]) calls it in process; the chat's tool
//! runtime (still Node) calls it over the private loopback with the store token
//! (`POST /v1/mcp/call`, core `rust-mcp-exec.cjs`), and **fails closed** when it cannot be reached.
//! Discovery stays where it is: the caller says which servers offer the name (`offers`).
pub mod limit;
pub mod reduce;
pub mod transport;

use crate::mcp_broker::Fail;
use crate::mcp_scrub::Secrets;
use crate::App;
use axum::body::Body;
use axum::http::{Response, StatusCode};
use base64::Engine;
use dispatch_permit::{DispatchPermit, GrantAuthority};
use js_json::JValue;
use limit::ToolRateLimit;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use transport::{CloseGuard, RpcError, SessionId, Transport};

/// core `TOOL_RESULT_CAP`.
const RESULT_CAP: usize = 8000;
const NEXTCLOUD_NEEDED: &str = "ERROR: this tool needs your Nextcloud account. Connect Nextcloud in Settings → Storage, then try again. (If it is already connected, the administrator has not listed its address in MCP_NEXTCLOUD_ORIGINS.)";
/// What a caller is told when this executor cannot be reached (core never had that case: its own process ran the call).
pub const UNAVAILABLE: &str = "tool execution is unavailable right now. Try again in a moment.";

/// How an MCP server authenticates (core `server.auth`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    None,
    Nextcloud,
    Internal,
    Bearer,
    Personal,
    Oauth,
    /// A directory server with a shared key an administrator typed.
    Directory,
}

impl Auth {
    /// `PER_USER_AUTH`: the servers whose credential is the account's own.
    fn per_user(self) -> bool {
        matches!(self, Auth::Nextcloud | Auth::Personal | Auth::Oauth)
    }
}

/// One server of `MCP_SERVERS`, as the operator wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operator {
    pub id: String,
    pub url: String,
    pub auth: Auth,
    pub token_env: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub servers: Vec<Operator>,
    /// `MCP_NEXTCLOUD_ORIGINS`: the origins a Nextcloud credential may be forwarded to.
    pub nextcloud_origins: Vec<String>,
    /// `ENABLED_TOOLBOXES`: `None` offers every toolbox, else only these (and `core`, and `dir-*`).
    pub enabled_toolboxes: Option<Vec<String>>,
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Settings {
        let units = |s: String| s.encode_utf16().collect::<Vec<u16>>();
        let (list, single) = (
            get("MCP_SERVERS").map(units),
            get("MCP_SERVER_URL").map(units),
        );
        // A list the port cannot judge configures no server (core's host does the same).
        let servers = mcp_servers::parse_servers(list.as_deref(), single.as_deref())
            .map(|parsed| {
                parsed
                    .servers
                    .into_iter()
                    .map(|s| Operator {
                        id: String::from_utf16_lossy(&s.id),
                        url: String::from_utf16_lossy(&s.url),
                        auth: match s.auth {
                            mcp_servers::Auth::None => Auth::None,
                            mcp_servers::Auth::Nextcloud => Auth::Nextcloud,
                            mcp_servers::Auth::Internal => Auth::Internal,
                            mcp_servers::Auth::Bearer => Auth::Bearer,
                        },
                        token_env: s.token_env.map(|t| String::from_utf16_lossy(&t)),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let enabled_toolboxes = mcp_servers::parse_toolboxes(
            get("ENABLED_TOOLBOXES")
                .map(|v| v.encode_utf16().collect::<Vec<u16>>())
                .as_deref(),
        )
        .map(|ids| ids.iter().map(|id| String::from_utf16_lossy(id)).collect());
        Settings {
            enabled_toolboxes,
            servers,
            nextcloud_origins: parse_origins(&get("MCP_NEXTCLOUD_ORIGINS").unwrap_or_default()),
        }
    }
}

/// core `parseNextcloudOrigins`: comma separated, trimmed, trailing slashes cut.
pub fn parse_origins(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|x| {
            x.trim_matches(crate::mcp_broker::js_ws)
                .trim_end_matches('/')
                .to_owned()
        })
        .filter(|x| !x.is_empty())
        .collect()
}

/// core `mcpAuthHeaders`' decision, on the facts of an account's storage connection: only a Nextcloud or
/// WebDAV connection with a user name and a credential, and only to an origin the operator declared
/// (the credential must reach the one Nextcloud the MCP server talks to, nobody else).
pub fn nextcloud_headers_for(
    nextcloud_or_webdav: bool,
    base_url: &str,
    username: &str,
    secret: &[u8],
    user: &str,
    origins: &[String],
) -> Option<Vec<(String, String)>> {
    if !nextcloud_or_webdav || username.is_empty() || secret.is_empty() {
        return None;
    }
    let allowed = url::Url::parse(base_url)
        .map(|u| u.origin().ascii_serialization())
        .is_ok_and(|origin| origins.contains(&origin));
    if !allowed {
        eprintln!("[mcp] refusing to forward credentials for {base_url}: origin not in MCP_NEXTCLOUD_ORIGINS");
        return None;
    }
    let mut raw = zeroize::Zeroizing::new(Vec::<u8>::new());
    raw.extend_from_slice(username.as_bytes());
    raw.push(b':');
    raw.extend_from_slice(secret);
    let basic = base64::engine::general_purpose::STANDARD.encode(&*raw);
    Some(vec![
        ("Authorization".to_owned(), format!("Basic {basic}")),
        ("X-Cowork-User-ID".to_owned(), user.to_owned()),
    ])
}

/// A server as a call sees it.
#[derive(Debug, Clone)]
struct Server {
    id: String,
    url: String,
    auth: Auth,
    token_env: Option<String>,
    directory: bool,
    title: Option<String>,
    shared: Vec<(String, String)>,
}

/// A directory server (`directory_mcp_servers`) as a call needs it.
#[derive(Debug, Clone)]
pub struct DirServer {
    pub id: String,
    pub url: String,
    pub title: String,
    pub oauth: bool,
    pub personal: bool,
    /// The shared key an administrator typed, opened (empty when there is none).
    pub shared: Vec<(String, String)>,
}

pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Everything a call asks of the account's credentials and of the stores around it. The front's
/// own implementation is [`Real`]; the differential tests replay the Node oracle over a fake one.
pub trait Credentials: Send + Sync {
    /// The directory servers, oldest first; none when the table cannot be read (core fails closed).
    fn directory(&self) -> Fut<'_, Vec<DirServer>>;
    /// An account's own key headers for one call (the broker's `mcp/key`).
    fn key_headers<'a>(
        &'a self,
        user: &'a str,
        server: &'a str,
    ) -> Fut<'a, Result<Vec<(String, String)>, Fail>>;
    /// An access token and its generation (the broker's `mcp/session`); `rejected` is the token a server just refused.
    fn session<'a>(
        &'a self,
        user: &'a str,
        server: &'a str,
        rejected: Option<&'a str>,
    ) -> Fut<'a, Result<(String, String), Fail>>;
    /// The second refusal: this sign-in (by generation) needs a new one.
    fn reject<'a>(&'a self, user: &'a str, server: &'a str, generation: &'a str) -> Fut<'a, bool>;
    /// The account's Nextcloud connection as a server's headers, to the origins the operator declared.
    fn nextcloud<'a>(
        &'a self,
        user: &'a str,
        origins: &'a [String],
    ) -> Fut<'a, Option<Vec<(String, String)>>>;
    /// Whether a directory server's address still resolves only to public hosts.
    fn url_allowed<'a>(&'a self, url: &'a str) -> Fut<'a, bool>;
}

/// The front's credentials: the MCP broker, the directory table and the account's storage connection.
pub struct Real<'a>(pub &'a Arc<App>);

impl Credentials for Real<'_> {
    fn directory(&self) -> Fut<'_, Vec<DirServer>> {
        Box::pin(async move {
            let Some(mcp) = self.0.mcp.as_ref().map(Arc::clone) else {
                return Vec::new();
            };
            let app = Arc::clone(self.0);
            tokio::task::spawn_blocking(move || {
                mcp.dir_rows(&app)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|row| DirServer {
                        shared: mcp.shared_headers(&row),
                        id: row.id,
                        url: row.url,
                        title: row.title,
                        oauth: row.oauth,
                        personal: row.personal,
                    })
                    .collect()
            })
            .await
            .unwrap_or_default()
        })
    }

    fn key_headers<'a>(
        &'a self,
        user: &'a str,
        server: &'a str,
    ) -> Fut<'a, Result<Vec<(String, String)>, Fail>> {
        Box::pin(async move {
            match self.0.mcp.as_ref().map(Arc::clone) {
                Some(mcp) => mcp.key_headers(self.0, user, server).await,
                None => Err(crate::mcp_broker::auth_error("transient")),
            }
        })
    }

    fn session<'a>(
        &'a self,
        user: &'a str,
        server: &'a str,
        rejected: Option<&'a str>,
    ) -> Fut<'a, Result<(String, String), Fail>> {
        Box::pin(async move {
            match self.0.mcp.as_ref().map(Arc::clone) {
                Some(mcp) => mcp
                    .session(self.0, user, server, rejected)
                    .await
                    .map(|t| (t.access.to_string(), t.generation)),
                None => Err(crate::mcp_broker::auth_error("transient")),
            }
        })
    }

    fn reject<'a>(&'a self, user: &'a str, server: &'a str, generation: &'a str) -> Fut<'a, bool> {
        Box::pin(async move {
            match self.0.mcp.as_ref().map(Arc::clone) {
                Some(mcp) => mcp.reject(self.0, user, server, generation).await,
                None => false,
            }
        })
    }

    fn nextcloud<'a>(
        &'a self,
        user: &'a str,
        origins: &'a [String],
    ) -> Fut<'a, Option<Vec<(String, String)>>> {
        Box::pin(async move {
            let (app, user, origins) = (Arc::clone(self.0), user.to_owned(), origins.to_vec());
            tokio::task::spawn_blocking(move || storage_headers(&app, &user, &origins))
                .await
                .ok()
                .flatten()
        })
    }

    fn url_allowed<'a>(&'a self, url: &'a str) -> Fut<'a, bool> {
        Box::pin(async move {
            match self.0.mcp.as_ref() {
                Some(m) => m.egress().url_allowed(url).await,
                None => false,
            }
        })
    }
}

/// One call to run.
#[derive(Debug, Clone, Default)]
pub struct Call {
    /// The account the call is for: core `authn.user.id` (its own key, its own sign-in, its rate-limit key).
    pub caller: Option<String>,
    /// core `workspace.userId` (its Nextcloud connection).
    pub workspace: Option<String>,
    pub name: String,
    pub args: Option<JValue>,
    /// The server the offering box named (core's `serverId`), or none.
    pub server: Option<String>,
    /// Every server that offers the name, in declaration order.
    pub offers: Vec<String>,
    /// core `scope.internalEditTarget`: the call was approved as a project-file edit.
    pub edit_target: bool,
    /// The `Authorization` header of noevia's capability token for this call, minted by the process
    /// that owns the key; used only if this call resolves to the `internal` server.
    pub internal_auth: Option<String>,
    /// The authoritative dispatch permit of a call under a chat write grant (sbstndalton/noevia-rs#425): asked at the
    /// outbound `tools/call`, after every await that prepared it, and again before an OAuth retry. `None`: the call
    /// carries no grant, and nothing is asked.
    pub permit: Option<Arc<dyn DispatchPermit>>,
}

/// What a call came to, for a caller that must keep the grant's started/settlement semantics (the chat gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The text the model is given.
    pub text: String,
    /// The permit refused the first `tools/call`: nothing was sent, nothing changed.
    pub not_run: bool,
    /// At least one `tools/call` went out (the permit was spent for it): the grant's `started()`. A retry the permit
    /// then refused leaves this true: the first send happened, though the server refused it.
    pub started: bool,
    /// The text is an error for the model (`ERROR ...`).
    pub failed: bool,
}

/// The text a call answers when the permit refused it: the same words the chat gate gives a permission it could not check.
pub fn not_run_text(name: &str) -> String {
    format!("ERROR: the permission for {name} could not be checked, so it was not run. Nothing was changed. Ask again.")
}

/// Wraps the call's permit to know what it was asked and answered.
#[derive(Debug)]
struct Counting {
    inner: Option<Arc<dyn DispatchPermit>>,
    sent: AtomicUsize,
    denied: AtomicBool,
}

impl DispatchPermit for Counting {
    fn authorize(&self) -> bool {
        let ok = self.inner.as_ref().is_none_or(|p| p.authorize());
        if ok {
            self.sent.fetch_add(1, Ordering::SeqCst);
        } else {
            self.denied.store(true, Ordering::SeqCst);
        }
        ok
    }
}

/// Where the executor's diagnostic lines go (stderr unless a test listens).
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

pub struct Executor {
    log: Option<LogSink>,
    settings: Settings,
    operator: Option<Transport>,
    public: Transport,
    limit: ToolRateLimit,
    ids: AtomicI64,
    /// The authority over chat write grants, for a call that arrives over the private bridge with a fence (none: such a
    /// call is refused, fail closed).
    grants: std::sync::RwLock<Option<Arc<dyn GrantAuthority>>>,
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor").finish_non_exhaustive()
    }
}

/// `String(err.message).slice(0, 300)`.
fn cut(message: &str) -> String {
    js_json::js_slice(message, 300)
}

impl Executor {
    pub fn new(settings: Settings, egress: &Arc<crate::mcp_egress::Egress>) -> Executor {
        Executor {
            log: None,
            settings,
            operator: Transport::operator(),
            public: Transport::public(egress),
            limit: ToolRateLimit::default(),
            ids: AtomicI64::new(0),
            grants: std::sync::RwLock::new(None),
        }
    }

    /// Where a bridge call's fence is validated: set once by whatever owns the chat grants. Until then a call that
    /// carries a fence is not run.
    pub fn set_grants(&self, authority: Arc<dyn GrantAuthority>) {
        *self.grants.write().unwrap_or_else(|e| e.into_inner()) = Some(authority);
    }

    fn grants(&self) -> Option<Arc<dyn GrantAuthority>> {
        self.grants
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// This executor with its own lookup for "does this directory server still resolve only to public addresses" in
    /// place of the egress's, on the transport that sends to directory servers (a test that holds the lookup open).
    #[must_use]
    pub fn with_address_check(mut self, check: transport::AddressCheck) -> Executor {
        self.public = self.public.with_address_check(check);
        self
    }

    /// The same executor writing its diagnostic lines to `sink` instead of stderr.
    #[must_use]
    pub fn with_log(mut self, sink: LogSink) -> Executor {
        self.log = Some(sink);
        self
    }

    fn say(&self, line: &str) {
        match &self.log {
            Some(sink) => sink(line),
            None => eprintln!("{line}"),
        }
    }

    /// core `createToolboxOffered`: `core` and `dir-*` always, else as `ENABLED_TOOLBOXES` says.
    pub fn toolbox_offered(&self, id: &str) -> bool {
        id == "core"
            || id.starts_with("dir-")
            || self
                .settings
                .enabled_toolboxes
                .as_ref()
                .is_none_or(|ids| ids.iter().any(|x| x == id))
    }

    /// core `operatorServerFor`: the one operator (non-directory) server among `offers`, for noevia's own fixed
    /// callers (deep research). A directory server offering the same name neither takes the call nor makes
    /// it ambiguous. `None` when no operator server, or more than one, offers it.
    pub fn operator_server_for(&self, offers: &[String]) -> Option<String> {
        let mut ids = offers
            .iter()
            .filter(|id| self.settings.servers.iter().any(|s| &s.id == *id));
        match (ids.next(), ids.next()) {
            (Some(only), None) => Some(only.clone()),
            _ => None,
        }
    }

    /// core `routeMcpTool`: the server a call means, or the text that refuses it.
    fn route(call: &Call) -> Result<String, String> {
        let name = &call.name;
        if let Some(id) = call.server.as_deref() {
            return if call.offers.iter().any(|o| o == id) {
                Ok(id.to_owned())
            } else {
                Err(format!(
                    "ERROR: unknown tool \"{name}\" on MCP server \"{id}\""
                ))
            };
        }
        if call.offers.len() > 1 {
            let ids: Vec<String> = call.offers.iter().map(|id| format!("\"{id}\"")).collect();
            eprintln!(
                "[mcp] refused a call to \"{name}\" without a server: offered by {}",
                ids.join(", ")
            );
            return Err(format!(
                "ERROR: tool \"{name}\" is offered by more than one MCP server, so it was not run."
            ));
        }
        call.offers
            .first()
            .cloned()
            .ok_or_else(|| format!("ERROR: unknown tool \"{name}\""))
    }

    /// The server with this id: a directory server shadows an operator one of the same id (core's
    /// `Map` is built operator-first, so the later entry wins).
    async fn find(&self, creds: &dyn Credentials, id: &str) -> Option<Server> {
        if id.starts_with("dir-") {
            if let Some(row) = creds.directory().await.into_iter().find(|r| r.id == id) {
                let auth = if row.oauth {
                    Auth::Oauth
                } else if row.personal {
                    Auth::Personal
                } else if !row.shared.is_empty() {
                    Auth::Directory
                } else {
                    Auth::None
                };
                return Some(Server {
                    id: row.id,
                    url: row.url,
                    auth,
                    token_env: None,
                    directory: true,
                    title: Some(row.title),
                    shared: row.shared,
                });
            }
        }
        self.settings
            .servers
            .iter()
            .find(|s| s.id == id)
            .map(|s| Server {
                id: s.id.clone(),
                url: s.url.clone(),
                auth: s.auth,
                token_env: s.token_env.clone(),
                directory: false,
                title: None,
                shared: Vec::new(),
            })
    }

    /// core `credentialTrouble` for a broker that could not say: temporary, not "sign in again".
    fn trouble(name: &str, server: &Server, oauth: bool) -> String {
        format!(
            "ERROR: {name} could not check your {} for {} right now. This is temporary: try again in a moment. Do not sign in again.",
            if oauth { "sign-in" } else { "key" },
            server.title.as_deref().unwrap_or("this server")
        )
    }

    /// core `executeMcpToolCall` over the front's own credentials. Never fails: a failure is the text the model is given.
    pub async fn execute(&self, app: &Arc<App>, call: Call) -> String {
        self.run(&Real(app), call).await
    }

    /// [`Executor::execute`] with what the call came to ([`Outcome`]).
    pub async fn execute_outcome(&self, app: &Arc<App>, call: Call) -> Outcome {
        self.dispatch(&Real(app), call).await
    }

    /// [`Executor::execute`] over any [`Credentials`].
    pub async fn run(&self, creds: &dyn Credentials, call: Call) -> String {
        self.dispatch(creds, call).await.text
    }

    /// A call over any [`Credentials`], with its [`Outcome`]. If the call holds a permit it is asked immediately before each
    /// outbound `tools/call` -- after the server lookup, the credentials, the address checks and the MCP handshake, and
    /// again before an OAuth retry -- and a refusal means that request is never made.
    pub async fn dispatch(&self, creds: &dyn Credentials, mut call: Call) -> Outcome {
        let counting = Arc::new(Counting {
            inner: call.permit.take(),
            sent: AtomicUsize::new(0),
            denied: AtomicBool::new(false),
        });
        let guarded = counting.inner.is_some();
        call.permit = guarded.then(|| Arc::clone(&counting) as Arc<dyn DispatchPermit>);
        let name = call.name.clone();
        let text = self.run_text(creds, call).await;
        let started = counting.sent.load(Ordering::SeqCst) > 0;
        let not_run = guarded && counting.denied.load(Ordering::SeqCst) && !started;
        Outcome {
            failed: text.starts_with("ERROR"),
            text: if not_run { not_run_text(&name) } else { text },
            not_run,
            started,
        }
    }

    async fn run_text(&self, creds: &dyn Credentials, call: Call) -> String {
        let name = call.name.clone();
        let known = match Self::route(&call) {
            Ok(id) => id,
            Err(text) => return text,
        };
        let Some(server) = self.find(creds, &known).await else {
            return format!("ERROR: tool \"{name}\" belongs to MCP server \"{known}\", which is no longer configured");
        };
        // An approval for a project file edit names a stored project file, and only noevia's own server
        // edits project files (#865 review).
        if call.edit_target && server.auth != Auth::Internal {
            eprintln!(
                "[mcp] refused \"{name}\": approved as a project file edit but routed to MCP server \"{}\"",
                server.id
            );
            return format!("ERROR: {name} was approved as a project file edit, but \"{name}\" was offered by MCP server \"{}\"; it was not run.", server.id);
        }

        // Credentials are per server, and an account's own never goes to another's.
        let caller = call.caller.clone();
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut oauth: Option<(String, String)> = None; // (access token, generation)
        match server.auth {
            Auth::Personal => {
                let got = match caller.as_deref() {
                    None => Ok(Vec::new()),
                    Some(user) => creds.key_headers(user, &server.id).await.or_else(|e| {
                        if signed_out(&e) {
                            Ok(Vec::new())
                        } else {
                            Err(e)
                        }
                    }),
                };
                match got {
                    Err(_) => return Self::trouble(&name, &server, false),
                    Ok(h) if h.is_empty() => {
                        return format!("ERROR: {name} needs your own key for {}: Plugins → Connected → Add key.", server.title.as_deref().unwrap_or("this server"));
                    }
                    Ok(h) => headers = h,
                }
            }
            Auth::Oauth => {
                let got = match caller.as_deref() {
                    None => Ok(None),
                    Some(user) => Self::oauth_session(creds, user, &server.id, None).await,
                };
                match got {
                    Err(_) => return Self::trouble(&name, &server, true),
                    Ok(None) => {
                        return format!("ERROR: {name} needs you to sign in to {} first: Plugins → Connected → Sign in.", server.title.as_deref().unwrap_or("this server"));
                    }
                    Ok(Some(session)) => {
                        headers =
                            vec![("Authorization".to_owned(), format!("Bearer {}", session.0))];
                        oauth = Some(session);
                    }
                }
            }
            Auth::Directory => headers = server.shared.clone(),
            Auth::Bearer => {
                let token = server
                    .token_env
                    .as_deref()
                    .and_then(|k| std::env::var(k).ok())
                    .filter(|t| !t.is_empty());
                match token {
                    Some(t) => headers = vec![("Authorization".to_owned(), format!("Bearer {t}"))],
                    None => {
                        return format!(
                            "ERROR: {name} needs {}, which is not configured on this deployment.",
                            server.token_env.as_deref().unwrap_or("undefined")
                        );
                    }
                }
            }
            Auth::Internal => match call.internal_auth.clone() {
                Some(h) => headers = vec![("Authorization".to_owned(), h)],
                None => {
                    return format!("ERROR: {name} needs a signed-in session and there is none.")
                }
            },
            Auth::Nextcloud => {
                let got = match call.workspace.as_deref() {
                    Some(user) => {
                        creds
                            .nextcloud(user, &self.settings.nextcloud_origins)
                            .await
                    }
                    None => None,
                };
                match got {
                    Some(h) => headers = h,
                    None => return NEXTCLOUD_NEEDED.to_owned(),
                }
            }
            Auth::None => {}
        }

        if server.directory && !creds.url_allowed(&server.url).await {
            return format!("ERROR: {name} was not run: its server's address no longer resolves to a public host.");
        }
        // #1693: a provider that answered 429 is not called again inside its wait. Per server, and per account
        // for every server that sends the account's own credentials.
        let rate_key = format!(
            "{}\u{0}{}",
            server.id,
            if server.auth.per_user() {
                caller.clone().unwrap_or_default()
            } else {
                String::new()
            }
        );
        let cooling = self.limit.remaining(&rate_key);
        if cooling > 0.0 {
            return limit::cooling_down_text(&name, cooling);
        }
        let transport = if server.directory {
            self.public.clone()
        } else {
            match self.operator.clone() {
                Some(t) => t,
                None => return format!("ERROR calling {name}: fetch failed"),
            }
        };
        let args = call.args.clone().unwrap_or(JValue::Undefined);
        let mut secrets = Secrets::default();
        secrets.add_headers(&headers);
        let permit = call.permit.clone();
        let mut result = self
            .call_once(
                &transport,
                &server.url,
                &headers,
                &name,
                &args,
                permit.as_deref(),
            )
            .await;
        // A server that refuses the token it was just given: ask the broker for one past it and try once
        // more. A second refusal marks THAT sign-in as needing a new one.
        if let (Err(first), Some(session), Some(user)) = (&result, oauth.clone(), caller.clone()) {
            if first.http_status() == Some(401) {
                result = self
                    .retry_oauth(
                        creds,
                        &transport,
                        &server,
                        &user,
                        &name,
                        &args,
                        first.clone(),
                        session,
                        &mut secrets,
                        permit.as_deref(),
                    )
                    .await;
            }
        }
        // Whatever the server sent, no credential this call carried (also the retry's) leaves with it.
        match result {
            Ok(value) => self.finish(&name, &rate_key, &value, &secrets),
            Err(err) => secrets.scrub(&self.failed(&name, &rate_key, &err)),
        }
    }

    async fn oauth_session(
        creds: &dyn Credentials,
        user: &str,
        server: &str,
        rejected: Option<&str>,
    ) -> Result<Option<(String, String)>, Fail> {
        match creds.session(user, server, rejected).await {
            Ok(t) => Ok(Some(t)),
            Err(e) if signed_out(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The second half of core's retry: the broker's answer decides, and a broker that cannot say leaves the 401 as the answer.
    #[allow(clippy::too_many_arguments)]
    async fn retry_oauth(
        &self,
        creds: &dyn Credentials,
        transport: &Transport,
        server: &Server,
        user: &str,
        name: &str,
        args: &JValue,
        first: RpcError,
        session: (String, String),
        secrets: &mut Secrets,
        permit: Option<&dyn DispatchPermit>,
    ) -> Result<JValue, RpcError> {
        let next = match Self::oauth_session(creds, user, &server.id, Some(&session.0)).await {
            Ok(Some(next)) => next,
            // Down, or no usable sign-in left (the broker has already marked the refused one): the 401 stands.
            Ok(None) | Err(_) => return Err(first),
        };
        if next.0 == session.0 {
            // The broker's best is the very token that was refused: that sign-in is no good.
            creds.reject(user, &server.id, &session.1).await;
            return Err(first);
        }
        let headers = vec![("Authorization".to_owned(), format!("Bearer {}", next.0))];
        secrets.add_headers(&headers);
        let again = self
            .call_once(transport, &server.url, &headers, name, args, permit)
            .await;
        // The grant closed while the retry's sign-in was fetched: the retry is not made, and the first answer (the
        // server's refusal of the first send) stands.
        if matches!(again, Err(RpcError::NotRun)) {
            return Err(first);
        }
        if let Err(e) = &again {
            if e.http_status() == Some(401) {
                creds.reject(user, &server.id, &next.1).await;
            }
        }
        again
    }

    /// One MCP session: connect, call, disconnect (closed whatever happened, also when this future is dropped).
    async fn call_once(
        &self,
        transport: &Transport,
        url: &str,
        headers: &[(String, String)],
        name: &str,
        args: &JValue,
        permit: Option<&dyn DispatchPermit>,
    ) -> Result<JValue, RpcError> {
        let session = SessionId::default();
        let _guard = CloseGuard::new(transport, url, headers, &session);
        transport.connect(url, headers, &session, &self.ids).await?;
        let result = transport
            .call_tool(url, headers, &session, &self.ids, name, args, permit)
            .await;
        transport
            .disconnect(url, headers, &session, transport::DISCONNECT_TIMEOUT)
            .await;
        result
    }

    /// The text of a result that came back.
    fn finish(&self, name: &str, rate_key: &str, result: &JValue, secrets: &Secrets) -> String {
        let text = match result_to_text(result) {
            Ok(t) => secrets.scrub(&t),
            // The runtime's own complaint about a malformed `content` names the value the server sent (a number, a
            // boolean): through the same scrub as everything else, before it is cut.
            Err(message) => {
                return secrets.scrub(&format!("ERROR calling {name}: {}", cut(&message)))
            }
        };
        let is_error = result.get("isError").truthy();
        if is_error && limit::is_rate_limited_text(&text) {
            self.say(&format!("[mcp] {name} rate limited (tool error)"));
            return limit::rate_limited_text(name, self.limit.hit(rate_key, None));
        }
        if !is_error {
            self.limit.clear(rate_key);
        }
        if text.is_empty() {
            return "(the tool returned no output)".to_owned();
        }
        reduce::reduce_tool_result(&text, RESULT_CAP).text
    }

    /// core's catch: a failed call is information the model can act on or relay. An HTTP failure is a neutral
    /// line with the status and a JSON-RPC failure its code: neither carries the server's own text.
    fn failed(&self, name: &str, rate_key: &str, err: &RpcError) -> String {
        match err {
            RpcError::Http {
                status: 429,
                retry_after,
                ..
            } => {
                self.say(&format!("[mcp] {name} rate limited (HTTP 429)"));
                let wait = limit::parse_retry_after(retry_after.as_deref(), limit::now_ms());
                limit::rate_limited_text(name, self.limit.hit(rate_key, wait))
            }
            RpcError::Http { status, .. } => {
                // The status only: a server's body can repeat the credential it was sent.
                self.say(&format!("[mcp] {name} failed: HTTP {status}"));
                format!("ERROR calling {name}: tool call failed (HTTP {status})")
            }
            RpcError::Msg(message) => format!("ERROR calling {name}: {}", cut(message)),
            RpcError::NotRun => not_run_text(name),
        }
    }
}

/// The broker's "the account has to (re)do something": no sign-in or key, or one that no longer works.
fn signed_out(e: &Fail) -> bool {
    e.status == 401 && matches!(e.code, Some("disconnected" | "reconnect"))
}

/// core mcp.cjs `resultToText`. `Err` is the TypeError core would have thrown out of its `for...of`.
pub fn result_to_text(result: &JValue) -> Result<String, String> {
    if !result.truthy() {
        return Ok(String::new());
    }
    let mut parts: Vec<String> = Vec::new();
    match result.get("content") {
        JValue::Arr(items) => {
            for c in items {
                if !c.truthy() {
                    continue;
                }
                if c.get("type").as_str() == Some("text") {
                    if let Some(t) = c.get("text").as_str() {
                        parts.push(t.to_owned());
                        continue;
                    }
                }
                if c.get("type").truthy() {
                    let kind = js_json::to_js_string(c.get("type")).unwrap_or_default();
                    parts.push(format!("[{kind} content omitted]"));
                }
            }
        }
        JValue::Str(_) | JValue::Undefined | JValue::Null => {}
        other if !other.truthy() => {}
        JValue::Num(n) => {
            return Err(format!(
                "number {} is not iterable (cannot read property Symbol(Symbol.iterator))",
                js_json::number_to_string(*n)
            ))
        }
        JValue::Bool(b) => {
            return Err(format!(
                "boolean {b} is not iterable (cannot read property Symbol(Symbol.iterator))"
            ))
        }
        JValue::Obj(_) => {
            return Err(
                "object is not iterable (cannot read property Symbol(Symbol.iterator))".to_owned(),
            )
        }
    }
    let mut text = parts
        .join("\n")
        .trim_matches(crate::mcp_broker::js_ws)
        .to_owned();
    if text.is_empty() && result.get("structuredContent").truthy() {
        text = js_json::stringify(result.get("structuredContent")).unwrap_or_default();
    }
    if result.get("isError").truthy() {
        text = format!(
            "ERROR from tool: {}",
            if text.is_empty() {
                "(no detail)"
            } else {
                &text
            }
        );
    }
    Ok(text)
}

/// The account's own storage connection as a Nextcloud server's headers (core `mcpAuthHeaders`).
fn storage_headers(
    app: &Arc<App>,
    user: &str,
    origins: &[String],
) -> Option<Vec<(String, String)>> {
    use server_storage_reads::{owner_secret, protocol::Kind};
    let data_dir = app.config.data_dir.as_ref()?;
    let previous = std::env::var("SECRETS_KEY_PREVIOUS")
        .ok()
        .map(zeroize::Zeroizing::new);
    let keys =
        owner_secret::load_existing_keys(data_dir, previous.as_deref().map(String::as_str)).ok();
    let store = app.identity.store_blocking().ok()?;
    // The account is the one the caller authenticated (the research request's own, or the one the
    // store-token channel names for the chat): the same trust as the broker's `mcp/key`.
    let identity = server_auth::Identity {
        user: server_auth::PublicUser {
            id: user.to_owned(),
            username: String::new(),
            display_name: String::new(),
            role: server_auth::Role::Member,
            disabled: false,
            diary_enabled: false,
            onboarded: false,
        },
        account_role: server_auth::Role::Member,
        credential: server_auth::Credential::Legacy,
    };
    let projected = owner_secret::project_connection_with(&store, &identity, keys.as_ref()).ok()?;
    let c = projected.connection();
    nextcloud_headers_for(
        matches!(c.kind, Kind::Nextcloud | Kind::WebDav),
        &c.base_url,
        &c.username,
        &c.secret,
        user,
        origins,
    )
}

// ── the private loopback: `POST /v1/mcp/call` ─────────────────────────────────────────────────

fn reply(status: u16, body: &JValue) -> Response<Body> {
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

fn refuse(status: u16, message: &str) -> Response<Body> {
    reply(
        status,
        &JValue::obj([("error", JValue::Str(message.into()))]),
    )
}

fn text_of(body: &JValue, key: &str, max: usize) -> Option<String> {
    match body.get(key) {
        JValue::Str(s) if !s.is_empty() && s.len() <= max => Some(s.clone()),
        _ => None,
    }
}

/// `POST /v1/mcp/call` after the store secret was checked. The body is
/// `{ name, args?, server?, offers[], caller?, workspace?, editTarget?, internalAuth?, fence? }`; the answer is
/// `{ text }` (and `notRun`, `started`, `failed` for a fenced call). A caller that goes away drops this future, which drops the call and closes its session.
pub async fn internal_call(app: Option<Arc<App>>, raw: &[u8]) -> Response<Body> {
    let Some(app) = app else {
        return refuse(503, "unavailable");
    };
    let Some(exec) = app.mcp_exec.as_ref().map(Arc::clone) else {
        return refuse(503, "unavailable");
    };
    let body = match js_json::parse(&String::from_utf8_lossy(raw)) {
        Ok(b @ JValue::Obj(_)) => b,
        _ => return refuse(400, "invalid JSON"),
    };
    let Some(name) = text_of(&body, "name", 256) else {
        return refuse(400, "invalid name");
    };
    let offers: Option<Vec<String>> = match body.get("offers") {
        JValue::Arr(items) if items.len() <= 64 => items
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 256)
                    .map(str::to_owned)
            })
            .collect(),
        _ => None,
    };
    let Some(offers) = offers else {
        return refuse(400, "invalid offers");
    };
    let call = Call {
        caller: text_of(&body, "caller", 256),
        workspace: text_of(&body, "workspace", 256),
        name,
        args: match body.get("args") {
            JValue::Undefined => None,
            v => Some(v.clone()),
        },
        server: text_of(&body, "server", 256),
        offers,
        edit_target: body.get("editTarget").truthy(),
        internal_auth: text_of(&body, "internalAuth", 4096),
        permit: None,
    };
    // A call under a chat write grant names it: `fence: { grant, generation }`, the grant's identity and the revocation
    // generation it was redeemed at. It is validated here against the authority over grants, and the permit it yields is
    // asked at the outbound `tools/call`. With no authority to ask, or a grant that is no longer at that generation, the
    // call is not run (a fenced write fails closed); a malformed fence is a 400.
    let permit = match body.get("fence") {
        JValue::Undefined | JValue::Null => None,
        fence @ JValue::Obj(_) => {
            let generation = match fence.get("generation") {
                JValue::Num(n) if n.fract() == 0.0 && (0.0..9.0e15).contains(n) => Some(*n as u64),
                _ => None,
            };
            let (Some(grant), Some(generation)) = (text_of(fence, "grant", 256), generation) else {
                return refuse(400, "invalid fence");
            };
            let name = call.name.clone();
            match exec
                .grants()
                .and_then(|authority| dispatch_permit::permit_for(&authority, &grant, generation))
            {
                Some(p) => Some(p),
                None => return not_run_reply(&name),
            }
        }
        _ => return refuse(400, "invalid fence"),
    };
    let fenced = permit.is_some();
    let outcome = exec.execute_outcome(&app, Call { permit, ..call }).await;
    let mut fields = vec![("text", JValue::Str(outcome.text))];
    if fenced {
        fields.push(("notRun", JValue::Bool(outcome.not_run)));
        fields.push(("started", JValue::Bool(outcome.started)));
        fields.push(("failed", JValue::Bool(outcome.failed)));
    }
    reply(200, &JValue::obj(fields))
}

/// The answer to a fenced call that was not run, and could not have been.
fn not_run_reply(name: &str) -> Response<Body> {
    reply(
        200,
        &JValue::obj([
            ("text", JValue::Str(not_run_text(name))),
            ("notRun", JValue::Bool(true)),
            ("started", JValue::Bool(false)),
            ("failed", JValue::Bool(true)),
        ]),
    )
}
