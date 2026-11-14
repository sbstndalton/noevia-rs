//! One MCP tool call, run by the front (track R5): core `mcp-wiring.cjs` `executeMcpToolCall` from the
//! moment a server is chosen. Node keeps choosing it (routing by server and tool name over the discovery
//! snapshot, which is this front's) and keeps the reducer that shortens a long result for the context
//! window (`tool-result-reduce.cjs`, the chat loop's). Everything between is here:
//!
//! - an approval for a project file edit is never sent to any server but noevia's own;
//! - the credential, per server mode, for this call only: the caller's own key or sign-in (the broker in
//!   this process; a refused token is refreshed past once, and a second refusal marks that sign-in), a
//!   directory server's shared key, the operator's service token, a capability token for the in-process
//!   server (user, project, write flag and approved edit target, signed with the same key core uses), or
//!   the Nextcloud pass-through, which Node supplies because it holds the storage credential;
//! - a directory server's address is checked again (public only), and its requests go through the
//!   guarded client; an operator's server through the operator client (`mcp_egress.rs`);
//! - the per-provider back-off after a 429 (`tool-rate-limit`), keyed per account where the credential is
//!   the account's own;
//! - connect, call and close (`mcp_egress::call_tool`), and the result or failure as the text the model
//!   gets, in core's words.
//!
//! Switch: `NOEVIA_RUST_MCP_CALLS=1` (off by default). Off, `POST /v1/mcp/call` is a 404 and Node runs the
//! call itself; on, Node asks here (core `mcp-wiring.cjs`).
//!
//! Proven against core: `tests/fixtures/mcp-call-node.v1.json` is recorded from core's real executor by
//! `tests/mcp-call-oracle.cjs` and replayed through [`execute`] by `tests/mcp_call_oracle.rs`.
//!
//! Named differences: when this front cannot mint a capability token (its secret keys are not loaded), the
//! in-process server's call answers [`UNAVAILABLE`] (core always holds its key). An error text is cut to
//! 300 UTF-16 units as core cuts it, except that a character whose two units straddle the cut is left out
//! whole (a Rust string cannot end in half a character; core's text ends in a lone surrogate there). The transport's own
//! differences are `mcp_egress.rs`'s (a non-JSON body's text; a connect-time refusal of a directory
//! server's address is `fetch failed`, where core's public-fetch names the host).
use crate::mcp_broker::{Fail, Mcp};
use crate::mcp_discovery_actor::ServerSpec;
use crate::mcp_egress::{CallFailure, Egress, RpcFailure};
use crate::App;
use axum::body::Body;
use axum::http::Response;
use js_json::JValue;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tool_rate_limit::RateLimit;

/// What a call answers when the front itself cannot make it.
pub const UNAVAILABLE: &str = "MCP tools are unavailable right now. Try again in a moment.";

/// Servers whose credential is the account's own: their back-off is per account (#1693, #1701).
const PER_USER_AUTH: [&str; 3] = ["nextcloud", "personal", "oauth"];

/// One call, as Node asks for it.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The signed-in account (core `authn.user.id`); empty when there is none.
    pub user: String,
    /// The workspace the call acts in (core `workspace.userId`), if any.
    pub workspace_user: Option<String>,
    pub server: String,
    pub name: String,
    pub arguments: JValue,
    /// The project the call belongs to (core `internalCallProject.id`).
    pub project: Option<String>,
    /// The approved project file edit's target (core `internalEditTarget`).
    pub edit_target: Option<String>,
    /// Whether the tool writes (core `isWriteTool(name)`; the approval gate has already let it through).
    pub write: bool,
    /// core `mcpAuthHeaders()` for this scope: the Nextcloud pass-through, or `None`.
    pub nextcloud: Option<Vec<(String, String)>>,
}

impl Request {
    /// The request body of `POST /v1/mcp/call`.
    pub fn from_json(body: &JValue) -> Result<Request, &'static str> {
        let text = |key: &str, max: usize| match body.get(key) {
            JValue::Str(s) if !s.is_empty() && s.len() <= max => Some(s.clone()),
            _ => None,
        };
        let optional = |key: &str| match body.get(key) {
            JValue::Str(s) => Ok(Some(s.clone())),
            JValue::Null | JValue::Undefined => Ok(None),
            _ => Err("invalid call"),
        };
        let server = text("server", 256).ok_or("invalid server")?;
        let name = text("name", 256).ok_or("invalid name")?;
        let user = optional("user")?.unwrap_or_default();
        let nextcloud = match body.get("nextcloud") {
            JValue::Null | JValue::Undefined => None,
            JValue::Obj(items) => Some(
                items
                    .iter()
                    .map(|(k, v)| match v {
                        JValue::Str(v) => Ok((k.clone(), v.clone())),
                        _ => Err("invalid nextcloud"),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            _ => return Err("invalid nextcloud"),
        };
        Ok(Request {
            user,
            workspace_user: optional("workspaceUser")?,
            server,
            name,
            arguments: body.get("arguments").clone(),
            project: optional("project")?,
            edit_target: optional("editTarget")?,
            write: body.get("write").as_bool() == Some(true),
            nextcloud,
        })
    }
}

/// What the model gets: `text`, and whether Node's reducer is to shorten it (a successful result's text).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub text: String,
    pub reduce: bool,
}

impl Answer {
    fn plain(text: String) -> Answer {
        Answer {
            text,
            reduce: false,
        }
    }
}

/// A broker's answer for one call's credential.
#[derive(Clone, Debug, PartialEq)]
pub enum Credential<T> {
    Ready(T),
    /// The account has to sign in or add a key (none, or one that no longer works).
    Absent,
    /// The broker cannot say right now: "try again", never "sign in again".
    Transient,
}

/// An access token for one call and the generation of the sign-in it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub access: String,
    pub generation: String,
}

/// Why a round ended without a result: the HTTP status and Retry-After when there was an answer, and
/// core's error message.
#[derive(Clone, Debug, PartialEq)]
pub struct Failed {
    pub http_status: Option<u16>,
    pub retry_after: Option<String>,
    pub message: String,
}

/// What a call needs from the process it runs in.
pub trait Backend: Send + Sync {
    fn now_ms(&self) -> f64;
    /// The configured servers: the operator's, then the directory's.
    fn servers(&self) -> impl Future<Output = Vec<ServerSpec>> + Send;
    fn key_headers(
        &self,
        user: &str,
        server: &str,
    ) -> impl Future<Output = Credential<Vec<(String, String)>>> + Send;
    /// `rejected`: the token the server just refused (refreshed past only while it is still the stored one).
    fn oauth_session(
        &self,
        user: &str,
        server: &str,
        rejected: Option<&str>,
    ) -> impl Future<Output = Credential<Session>> + Send;
    /// The second refusal of a fresh token: this sign-in (by its generation) needs a new one.
    fn reject(&self, user: &str, server: &str, generation: &str)
        -> impl Future<Output = ()> + Send;
    fn directory_headers(&self, id: &str) -> impl Future<Output = Vec<(String, String)>> + Send;
    /// The value of a static-token environment variable, if set and not empty.
    fn static_token(&self, env: &str) -> Option<String>;
    /// core `mintToken(key, { uid, pid, w, t })`, or `None` when the key is not available.
    fn internal_token(
        &self,
        uid: Option<&str>,
        pid: Option<&str>,
        write: bool,
        target: Option<&str>,
    ) -> Option<String>;
    /// Whether a directory server's address is still a public host.
    fn url_public(&self, url: &str) -> impl Future<Output = bool> + Send;
    /// Track R14: the in-app server's result for one of its project write tools, answered by this
    /// front (inapp_writes.rs, behind NOEVIA_RUST_INAPP_WRITES), or `None` for a call to the
    /// server as before. `uid`/`pid`/`target` are what the call's token carries.
    fn inapp_write(
        &self,
        _uid: &str,
        _pid: Option<&str>,
        _name: &str,
        _arguments: &JValue,
        _target: Option<&str>,
        _write: bool,
    ) -> impl Future<Output = Option<JValue>> + Send {
        std::future::ready(None)
    }
    /// One round: connect, call, close.
    fn call(
        &self,
        server: &ServerSpec,
        headers: &[(String, String)],
        name: &str,
        arguments: &JValue,
    ) -> impl Future<Output = Result<JValue, Failed>> + Send;
}

const NOT_ITERABLE: &str = "is not iterable (cannot read property Symbol(Symbol.iterator))";

/// core mcp.cjs `resultToText`: the text parts joined, other typed parts named, trimmed; the structured
/// content's JSON when there is no text; `isError` prefixed. `Err` is what V8 throws for a `content` that
/// cannot be iterated (or a part `type` that cannot become a string), which core's executor turns into
/// the call's error text.
pub fn result_to_text(result: &JValue) -> Result<String, String> {
    if !result.truthy() {
        return Ok(String::new());
    }
    let content = result.get("content");
    let parts: &[JValue] = if !content.truthy() {
        &[]
    } else {
        match content {
            JValue::Arr(items) => items,
            // A string iterates as characters, none of which has a `type`.
            JValue::Str(_) => &[],
            JValue::Num(n) => {
                return Err(format!(
                    "number {} {NOT_ITERABLE}",
                    js_json::number_to_string(*n)
                ))
            }
            JValue::Bool(_) => return Err(format!("boolean true {NOT_ITERABLE}")),
            _ => return Err(format!("object {NOT_ITERABLE}")),
        }
    };
    let mut out: Vec<String> = Vec::new();
    for c in parts {
        if !c.truthy() {
            continue;
        }
        let kind = c.get("type");
        match (kind, c.get("text")) {
            (JValue::Str(k), JValue::Str(t)) if k == "text" => out.push(t.clone()),
            _ if kind.truthy() => {
                let k = js_json::to_js_string(kind)
                    .map_err(|_| "Cannot convert object to primitive value".to_string())?;
                out.push(format!("[{k} content omitted]"));
            }
            _ => {}
        }
    }
    let joined: Vec<u16> = out.join("\n").encode_utf16().collect();
    let mut text = String::from_utf16_lossy(policy_leaves::js_trim(&joined));
    let structured = result.get("structuredContent");
    if text.is_empty() && structured.truthy() {
        text = js_json::stringify(structured).unwrap_or_default();
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

fn bearer(token: &str) -> Vec<(String, String)> {
    vec![("Authorization".into(), format!("Bearer {token}"))]
}

fn title_of(server: &ServerSpec) -> &str {
    server
        .title
        .as_deref()
        .filter(|t| !t.is_empty())
        .unwrap_or("this server")
}

/// core `credentialTrouble` for a broker that cannot say right now.
fn trouble(name: &str, server: &ServerSpec) -> String {
    format!(
        "ERROR: {name} could not check your {} for {} right now. This is temporary: try again in a moment. Do not sign in again.",
        if server.auth == "oauth" { "sign-in" } else { "key" },
        title_of(server)
    )
}

fn limited<T>(limits: &Mutex<RateLimit>, f: impl FnOnce(&mut RateLimit) -> T) -> T {
    let mut held = limits.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut held)
}

/// core `executeMcpToolCall(name, args, signal, serverId)` from the routed server on (see the module docs).
pub async fn execute<B: Backend>(b: &B, limits: &Mutex<RateLimit>, req: &Request) -> Answer {
    let name = req.name.as_str();
    // core `new Map(MCP_SERVERS.map(...))`: of two servers with one id, the later (the directory's) wins.
    let servers = b.servers().await;
    let Some(server) = servers.iter().rev().find(|s| s.id == req.server).cloned() else {
        return Answer::plain(format!(
            "ERROR: tool \"{name}\" belongs to MCP server \"{}\", which is no longer configured",
            req.server
        ));
    };
    if req.edit_target.as_deref().is_some_and(|t| !t.is_empty()) && server.auth != "internal" {
        return Answer::plain(format!("ERROR: {name} was approved as a project file edit, but \"{name}\" was offered by MCP server \"{}\"; it was not run.", server.id));
    }
    let user = req.user.as_str();
    let mut session: Option<Session> = None;
    // Track R14: the in-process server's call identity (the token's uid, pid and edit pin).
    let mut inapp_write: Option<(String, Option<String>, Option<String>)> = None;
    let headers: Vec<(String, String)> = match server.auth.as_str() {
        "personal" => {
            let needs = || {
                Answer::plain(format!(
                    "ERROR: {name} needs your own key for {}: Plugins → Connected → Add key.",
                    title_of(&server)
                ))
            };
            if user.is_empty() {
                return needs();
            }
            match b.key_headers(user, &server.id).await {
                Credential::Ready(h) if !h.is_empty() => h,
                Credential::Ready(_) | Credential::Absent => return needs(),
                Credential::Transient => return Answer::plain(trouble(name, &server)),
            }
        }
        "oauth" => {
            let needs = || {
                Answer::plain(format!(
                    "ERROR: {name} needs you to sign in to {} first: Plugins → Connected → Sign in.",
                    title_of(&server)
                ))
            };
            if user.is_empty() {
                return needs();
            }
            match b.oauth_session(user, &server.id, None).await {
                Credential::Ready(s) => {
                    let h = bearer(&s.access);
                    session = Some(s);
                    h
                }
                Credential::Absent => return needs(),
                Credential::Transient => return Answer::plain(trouble(name, &server)),
            }
        }
        "directory" => b.directory_headers(&server.id).await,
        "bearer" => match server.token_env.as_deref().and_then(|e| b.static_token(e)) {
            Some(token) => bearer(&token),
            None => {
                return Answer::plain(format!(
                    "ERROR: {name} needs {}, which is not configured on this deployment.",
                    server.token_env.as_deref().unwrap_or("undefined")
                ))
            }
        },
        "internal" => {
            let Some(workspace) = req.workspace_user.as_deref() else {
                return Answer::plain(format!(
                    "ERROR: {name} needs a signed-in session and there is none."
                ));
            };
            let target = req
                .edit_target
                .as_deref()
                .filter(|t| req.write && !t.is_empty());
            let uid = Some(workspace).filter(|u| !u.is_empty());
            let pid = req.project.as_deref().filter(|p| !p.is_empty());
            inapp_write = uid.map(|u| (u.to_string(), pid.map(str::to_string), target.map(str::to_string)));
            match b.internal_token(uid, pid, req.write, target) {
                Some(token) => bearer(&token),
                None => return Answer::plain(format!("ERROR calling {name}: {UNAVAILABLE}")),
            }
        }
        "nextcloud" => match &req.nextcloud {
            Some(h) if !h.is_empty() => h.clone(),
            _ => return Answer::plain("ERROR: this tool needs your Nextcloud account. Connect Nextcloud in Settings → Storage, then try again. (If it is already connected, the administrator has not listed its address in MCP_NEXTCLOUD_ORIGINS.)".into()),
        },
        _ => Vec::new(),
    };
    if server.directory && !b.url_public(&server.url).await {
        return Answer::plain(format!(
            "ERROR: {name} was not run: its server's address no longer resolves to a public host."
        ));
    }
    let rate_key = format!(
        "{}\u{0}{}",
        server.id,
        if PER_USER_AUTH.contains(&server.auth.as_str()) {
            user
        } else {
            ""
        }
    );
    let cooling = limited(limits, |l| l.remaining(&rate_key, b.now_ms()));
    if cooling > 0.0 {
        return Answer::plain(tool_rate_limit::cooling_down_text(name, cooling));
    }
    let native = match &inapp_write {
        Some((uid, pid, target)) => {
            b.inapp_write(
                uid,
                pid.as_deref(),
                name,
                &req.arguments,
                target.as_deref(),
                req.write,
            )
            .await
        }
        None => None,
    };
    let mut outcome = match native {
        Some(result) => Ok(result),
        None => b.call(&server, &headers, name, &req.arguments).await,
    };
    // A server that refuses the token it was just given: ask the broker for one past it and try once more.
    if let (Err(first), Some(old)) = (&outcome, &session) {
        if first.http_status == Some(401) {
            match b.oauth_session(user, &server.id, Some(&old.access)).await {
                // The broker could not say, or there is no sign-in left (it marked the refused one
                // itself): the 401 stands, and nothing is marked here.
                Credential::Transient | Credential::Absent => {}
                Credential::Ready(next) if next.access == old.access => {
                    b.reject(user, &server.id, &old.generation).await;
                }
                Credential::Ready(next) => {
                    let again = b
                        .call(&server, &bearer(&next.access), name, &req.arguments)
                        .await;
                    if matches!(&again, Err(f) if f.http_status == Some(401)) {
                        b.reject(user, &server.id, &next.generation).await;
                    }
                    outcome = again;
                }
            }
        }
    }
    match outcome {
        Ok(result) => {
            let text = match result_to_text(&result) {
                Ok(text) => text,
                Err(message) => {
                    return Answer::plain(format!(
                        "ERROR calling {name}: {}",
                        js_json::js_slice(&message, 300)
                    ))
                }
            };
            let is_error = result.truthy() && result.get("isError").truthy();
            if is_error && tool_rate_limit::is_rate_limited_text(&text) {
                let wait = limited(limits, |l| l.hit(&rate_key, None, b.now_ms()));
                return Answer::plain(tool_rate_limit::rate_limited_text(name, wait));
            }
            if !is_error {
                limited(limits, |l| l.clear(&rate_key));
            }
            if text.is_empty() {
                return Answer::plain("(the tool returned no output)".into());
            }
            Answer { text, reduce: true }
        }
        Err(f) => match f.http_status {
            Some(429) => {
                let now = b.now_ms();
                let after = tool_rate_limit::parse_retry_after(f.retry_after.as_deref(), now);
                let wait = limited(limits, |l| l.hit(&rate_key, after, now));
                Answer::plain(tool_rate_limit::rate_limited_text(name, wait))
            }
            Some(status) => Answer::plain(format!(
                "ERROR calling {name}: tool call failed (HTTP {status})"
            )),
            None => Answer::plain(format!(
                "ERROR calling {name}: {}",
                js_json::js_slice(&f.message, 300)
            )),
        },
    }
}

/// core `mcp-internal.cjs` `mintToken(key, { uid, pid, w, t })` for one call (30 s), with `jti` and `now`
/// given: `base64url(JSON payload) "." base64url(HMAC-SHA256(key, that))`.
pub fn mint_call_token(
    key: &[u8],
    uid: Option<&str>,
    pid: Option<&str>,
    write: bool,
    target: Option<&str>,
    now_ms: i64,
    jti: &str,
) -> String {
    let id = |v: Option<&str>| v.map_or(JValue::Null, |s| JValue::Str(s.to_owned()));
    let mut pairs = vec![
        ("v", JValue::Num(1.0)),
        ("uid", id(uid)),
        ("pid", id(pid)),
        ("cid", JValue::Null),
        ("jti", JValue::Str(jti.to_owned())),
        ("exp", JValue::Num((now_ms + 30_000) as f64)),
        ("w", JValue::Num(if write { 1.0 } else { 0.0 })),
        ("d", JValue::Num(0.0)),
    ];
    if let Some(t) = target.filter(|t| !t.is_empty()) {
        pairs.push(("t", JValue::Str(t.to_owned())));
    }
    let payload = js_json::stringify(&JValue::obj(pairs)).unwrap_or_default();
    crate::mcp_discovery_backend::sign_token(key, &payload)
}

// ── in the running front ──

/// What every call shares: the two clients and the back-off.
pub struct Calls {
    /// Directory servers: public addresses only, checked again at connect time.
    guarded: Arc<Egress>,
    /// The operator's own servers: any address the operator configured.
    open: Arc<Egress>,
    limits: Mutex<RateLimit>,
}

/// The shared clients, made on the first call. A client that could not be built is tried again on the next
/// call rather than remembered as missing.
fn calls_of(app: &App, mcp: &Mcp) -> Option<Arc<Calls>> {
    if let Some(calls) = mcp.calls.get() {
        return Some(Arc::clone(calls));
    }
    let made = Arc::new(Calls {
        guarded: Egress::for_discovery(app.config.mcp.qa_loopback, 1)?,
        open: Egress::operator(1)?,
        limits: Mutex::new(RateLimit::default()),
    });
    // Two first calls at once: one set wins and both use it.
    Some(Arc::clone(mcp.calls.get_or_init(|| made)))
}

/// The broker's refusal as a credential: "sign in / add a key" (core `needsSignIn`), anything else
/// "try again" (core `brokerUnavailable`).
fn credential<T>(answer: Result<T, Fail>) -> Credential<T> {
    match answer {
        Ok(v) => Credential::Ready(v),
        Err(e) if e.status == 401 && matches!(e.code, Some("disconnected" | "reconnect")) => {
            Credential::Absent
        }
        Err(_) => Credential::Transient,
    }
}

/// [`Backend`] over this front: the broker, the directory table, the clients.
struct AppBackend {
    app: Arc<App>,
    mcp: Arc<Mcp>,
    calls: Arc<Calls>,
    servers: Vec<ServerSpec>,
    /// Fires when the caller (Node's request) has gone.
    stop: tokio::sync::watch::Receiver<bool>,
}

impl Backend for AppBackend {
    fn now_ms(&self) -> f64 {
        crate::identity::now_ms() as f64
    }

    async fn inapp_write(
        &self,
        uid: &str,
        pid: Option<&str>,
        name: &str,
        arguments: &JValue,
        target: Option<&str>,
        write: bool,
    ) -> Option<JValue> {
        if !crate::inapp_writes::enabled() || !write {
            return None;
        }
        let app = Arc::clone(&self.app);
        let (uid, pid, name, arguments, target) = (
            uid.to_string(),
            pid.map(str::to_string),
            name.to_string(),
            arguments.clone(),
            target.map(str::to_string),
        );
        match tokio::task::spawn_blocking(move || {
            crate::inapp_writes::answer(&app, &uid, pid.as_deref(), &name, &arguments, target.as_deref(), write)
        })
        .await
        {
            Ok(answer) => answer,
            // The write may have begun: never let core's server write again (inapp_writes.rs).
            Err(_) => Some(edit_target_lookup::inapp::error_result(
                "noevia could not finish this write, so it cannot tell whether it was saved. Outcome unknown: sync this project's Sources to check whether it was saved before trying again.",
            )),
        }
    }

    async fn servers(&self) -> Vec<ServerSpec> {
        self.servers.clone()
    }

    async fn key_headers(&self, user: &str, server: &str) -> Credential<Vec<(String, String)>> {
        credential(self.mcp.key_headers(&self.app, user, server).await)
    }

    async fn oauth_session(
        &self,
        user: &str,
        server: &str,
        rejected: Option<&str>,
    ) -> Credential<Session> {
        credential(self.mcp.session(&self.app, user, server, rejected).await).map(|t| Session {
            access: t.access.to_string(),
            generation: t.generation,
        })
    }

    async fn reject(&self, user: &str, server: &str, generation: &str) {
        let _ = self.mcp.reject(&self.app, user, server, generation).await;
    }

    async fn directory_headers(&self, id: &str) -> Vec<(String, String)> {
        let (app, mcp, id) = (Arc::clone(&self.app), Arc::clone(&self.mcp), id.to_owned());
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

    fn static_token(&self, env: &str) -> Option<String> {
        std::env::var(env).ok().filter(|v| !v.is_empty())
    }

    fn internal_token(
        &self,
        uid: Option<&str>,
        pid: Option<&str>,
        write: bool,
        target: Option<&str>,
    ) -> Option<String> {
        let key = crate::mcp_discovery_backend::internal_token_key(&self.mcp)?;
        let jti = crate::mcp_discovery_backend::uuid_v4();
        Some(mint_call_token(
            &key,
            uid,
            pid,
            write,
            target,
            crate::identity::now_ms(),
            &jti,
        ))
    }

    async fn url_public(&self, url: &str) -> bool {
        self.calls.guarded.url_allowed(url).await
    }

    async fn call(
        &self,
        server: &ServerSpec,
        headers: &[(String, String)],
        name: &str,
        arguments: &JValue,
    ) -> Result<JValue, Failed> {
        let egress = if server.directory {
            &self.calls.guarded
        } else {
            &self.calls.open
        };
        let mut stop = self.stop.clone();
        let stopped = async move {
            // A dropped sender (the request went away) ends the wait as a stop does.
            let _ = stop.wait_for(|gone| *gone).await;
        };
        crate::mcp_egress::call_tool(
            egress,
            &server.url,
            headers,
            name,
            arguments,
            &self.mcp.rpc_id,
            stopped,
        )
        .await
        .map_err(|f| Failed {
            http_status: match &f {
                CallFailure::Rpc(RpcFailure::Http { status, .. }) => Some(*status),
                _ => None,
            },
            retry_after: match &f {
                CallFailure::Rpc(RpcFailure::Http { retry_after, .. }) => retry_after.clone(),
                _ => None,
            },
            message: f.text(),
        })
    }
}

impl<T> Credential<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Credential<U> {
        match self {
            Credential::Ready(v) => Credential::Ready(f(v)),
            Credential::Absent => Credential::Absent,
            Credential::Transient => Credential::Transient,
        }
    }
}

/// `POST /v1/mcp/call` (after the store secret): one call (see the module docs) -> 200 `{ text, reduce }`.
/// 404 while `NOEVIA_RUST_MCP_CALLS` is off (mcp_routes.rs answers that before anything else); 400 for a
/// body that is not a call; 503 when the front cannot build its clients. The call runs in its own task:
/// the request going away stops it, and its session is still closed.
pub async fn op(app: &Arc<App>, mcp: &Arc<Mcp>, body: &JValue) -> Response<Body> {
    use crate::mcp_routes::{internal, internal_error};
    if !app.config.mcp.calls {
        return internal_error(404, "not found");
    }
    let request = match Request::from_json(body) {
        Ok(r) => r,
        Err(why) => return internal_error(400, why),
    };
    let Some(calls) = calls_of(app, mcp) else {
        return internal_error(503, "unavailable");
    };
    let servers = crate::mcp_discovery_backend::server_specs(app).await;
    let (gone, stop) = tokio::sync::watch::channel(false);
    let backend = AppBackend {
        app: Arc::clone(app),
        mcp: Arc::clone(mcp),
        calls,
        servers,
        stop,
    };
    let task = tokio::spawn(async move {
        let answer = execute(&backend, &backend.calls.limits, &request).await;
        drop(backend);
        answer
    });
    // Held until the answer is in: dropping this handler (the caller went away) drops `gone`.
    let answer = task.await;
    drop(gone);
    match answer {
        Ok(a) => internal(
            200,
            &JValue::obj([
                ("text", JValue::Str(a.text)),
                ("reduce", JValue::Bool(a.reduce)),
            ]),
        ),
        Err(_) => internal_error(500, "the call failed"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_read_strictly() {
        let body = js_json::parse(r#"{"server":"s","name":"t","user":"u","workspaceUser":null,"arguments":{"q":1},"write":true,"nextcloud":{"Authorization":"Basic x"}}"#).unwrap();
        let r = Request::from_json(&body).unwrap();
        assert_eq!(
            (r.user.as_str(), r.workspace_user, r.write),
            ("u", None, true)
        );
        assert_eq!(
            r.nextcloud,
            Some(vec![("Authorization".to_string(), "Basic x".to_string())])
        );
        for bad in [
            r#"{"name":"t"}"#,
            r#"{"server":"s"}"#,
            r#"{"server":"s","name":"t","user":5}"#,
            r#"{"server":"s","name":"t","nextcloud":{"a":1}}"#,
            r#"{"server":"s","name":"t","nextcloud":"x"}"#,
        ] {
            assert!(
                Request::from_json(&js_json::parse(bad).unwrap()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_part_type_that_cannot_become_text_fails_as_v8_does() {
        let r = js_json::parse(r#"{"content":[{"type":{"toString":1}}]}"#).unwrap();
        assert_eq!(
            result_to_text(&r),
            Err("Cannot convert object to primitive value".into())
        );
    }
}
