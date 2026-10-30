//! Shared harness of the MCP directory tests (sbstndalton/noevia#2202 round 2): a synthetic MCP server
//! and OAuth service on 127.0.0.1, the real front and its real internal store listener, a seeded
//! Node database. Copied from tests/mcp_credentials.rs (which keeps its own). No real server, account,
//! token or credential.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::{
    config::Config,
    project_store,
    serve::{serve, Limits},
    App,
};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
pub const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
pub const ADMIN: &str = "cowork_session=admin-token; cowork_csrf=csrf";
pub const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";
pub const KEY: [u8; 32] = [7; 32];
pub const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";

/// Golden vectors shared with core (server/rust-mcp-credentials.test.cjs): key [7;32], nonce
/// 0x01..0x0c, computed with Node's own `crypto.createCipheriv('aes-256-gcm')` (iv | tag | body,
/// AAD noevia:user:<id> for enc:v2). The JSON is the shape core's mcp-oauth.cjs and
/// directory-mcp.cjs store.
pub const TOKENS_JSON: &str = r#"{"accessToken":"synthetic-access","refreshToken":"synthetic-refresh","expiresAt":1800000000000}"#;
pub const TOKENS_GOLDEN: &str = "enc:v2:AQIDBAUGBwgJCgsMI17GHu5yPOtiRO5ls-yDyMBeif0Jj5FfsVFpe_uFmWKnnD9E2i5Gjh2M7fzPPTVtHCAWKmJhuTAK3BzxETwpBOoZEbPdEMh5qMaTYfmN8kPbxfh7anMZGQdGT1_M0DOT7uFhCuBnnCYrqRv3fLmq";
pub const CLIENT_JSON: &str = r#"{"resource":"https://mcp.example.test/mcp","issuer":"https://auth.example.test","authorizationEndpoint":"https://auth.example.test/authorize","tokenEndpoint":"https://auth.example.test/token","registrationEndpoint":"https://auth.example.test/register","secretMethod":"client_secret_basic","scopes":["read"],"clientId":"synthetic-client","clientSecret":null,"redirectUri":"https://noevia.example.test/api/mcp-oauth/callback"}"#;
pub const CLIENT_GOLDEN: &str = "enc:v1:AQIDBAUGBwgJCgsM6cDf01pcxElA-gdc2zMrzcBemvsZhZdehlsgJLfP1zSklmsfnSZRl1DE9P7BKCp7EHhRK3MopjYJlmS8Eyo0U7VJQPCRDNRorNzKY6SJ4UXWmPUhJzwMDRIBSV_M5WWd9rEsTrg43n9h-F-uI-eSBcvEEzXBY_WyJ8DDQ6IdgPzy64Ihm9gxYd26WkKTbb34IpjZki3w9Vg4IHD3jVhLhQdDp0t-uLoVLCbcq5QPOm7RRCnHLNm4yDTWmOkCcsHYQ1cg5faU4Ute5gIZdIGIRwmiTDiKDO-ItmAlR9cf98WqZjAXhpTmADcNlP1ogUI0JK_GngyghTtzudO0JtsYFbzSuEv-JtWwhmUyk3FuZsL-YgJRDENb9_EZHwoWVvtuokH7wmNqBxvlJSCqtQDzN9_5XFyXpXZ2HeQKNn4dq68SZRDxy-ra1DJ0651zZnyeUrIRno_c57uZFJPaw2-PrwLWrtaiyZO1hRqalhgjZ1mFpdSeH67PklPv578pTeiS6YZk8t6ulLYJm3Yl8DAMFuWWK5-3yt1zT1EM92Gu1C3w59slZPJI9FVy5HNbr6pAY2JwVwAIVAA";
pub const KEYS_JSON: &str = r#"{"X-Api-Key":"synthetic-key"}"#;
pub const KEYS_GOLDEN: &str =
    "enc:v2:AQIDBAUGBwgJCgsMkdk8PVlaYeAV80cSjk3YCsBesLMrmosBrlt7PK-F0Dm6kTlVxiJRyhXE9b3R";

pub fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn j(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|_| panic!("not JSON: {s}"))
}

pub async fn raw_text(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    extra: &str,
    body: &[u8],
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n", body.len()).as_bytes()).await.unwrap();
    let _ = s.write_all(body).await;
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(40), s.read_to_end(&mut bytes)).await;
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    let status = h.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, h.to_ascii_lowercase(), b.to_owned())
}

pub async fn raw(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    extra: &str,
    body: &[u8],
) -> (u16, serde_json::Value) {
    let (status, _, b) = raw_text(addr, method, path, cookie, extra, body).await;
    (
        status,
        serde_json::from_str(&b).unwrap_or(serde_json::Value::Null),
    )
}

pub async fn write(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    let csrf = format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n");
    raw(addr, method, path, cookie, &csrf, body.as_bytes()).await
}

pub async fn get(addr: std::net::SocketAddr, path: &str, cookie: &str) -> (u16, serde_json::Value) {
    raw(addr, "GET", path, cookie, "", b"").await
}

/// One internal store call (`/v1/mcp/<op>`).
pub async fn store(
    addr: std::net::SocketAddr,
    op: &str,
    token: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("POST /v1/mcp/{op} HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{}: {token}\r\n\r\n{body}", body.len(), project_store::TOKEN_HEADER).as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(40), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (
        status,
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
}

// ---------------------------------------------------------------------------------------------
// A synthetic MCP server and OAuth service.

#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: String,
    pub body: String,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.lines().find_map(|l| {
            l.strip_prefix(&format!("{name}:"))
                .map(|v| v.trim().to_owned())
        })
    }
    pub fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }
}

pub type Handler = Arc<dyn Fn(&Seen) -> (Duration, String) + Send + Sync>;

#[derive(Clone)]
pub struct Upstream {
    pub addr: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    handler: Arc<Mutex<Handler>>,
    hit: Arc<tokio::sync::Notify>,
}

impl Upstream {
    pub fn set(&self, f: impl Fn(&Seen) -> (Duration, String) + Send + Sync + 'static) {
        *self.handler.lock().unwrap() = Arc::new(f);
    }
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
    pub fn count(&self, path: &str) -> usize {
        self.seen()
            .iter()
            .filter(|s| s.path.starts_with(path))
            .count()
    }
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub async fn wait_for(&self, path: &str, n: usize) {
        let waiting = async {
            loop {
                let notified = self.hit.notified();
                if self.count(path) >= n {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(20), waiting)
            .await
            .unwrap_or_else(|_| panic!("the upstream never saw request {n} of {path}"));
    }
}

pub fn reply(status: &str, body: &str) -> (Duration, String) {
    reply_with(status, "", body)
}

pub fn reply_with(status: &str, extra: &str, body: &str) -> (Duration, String) {
    (
        Duration::ZERO,
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}

pub async fn upstream() -> Upstream {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up = Upstream {
        addr: l.local_addr().unwrap(),
        seen: Arc::new(Mutex::new(Vec::new())),
        handler: Arc::new(Mutex::new(Arc::new(|_: &Seen| reply("500 Oops", "")))),
        hit: Arc::new(tokio::sync::Notify::new()),
    };
    let (seen, handler, hit) = (
        Arc::clone(&up.seen),
        Arc::clone(&up.handler),
        Arc::clone(&up.hit),
    );
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let (seen, handler, hit) = (Arc::clone(&seen), Arc::clone(&handler), Arc::clone(&hit));
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let Ok(n) = s.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let len = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    let Ok(n) = s.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let mut first = head.lines().next().unwrap_or("").split_whitespace();
                let request = Seen {
                    method: first.next().unwrap_or("").into(),
                    path: first.next().unwrap_or("").into(),
                    headers: head.to_ascii_lowercase(),
                    body: String::from_utf8_lossy(&buf[head_end..]).to_string(),
                };
                seen.lock().unwrap().push(request.clone());
                hit.notify_waiters();
                let f = handler.lock().unwrap().clone();
                let (delay, text) = f(&request);
                tokio::time::sleep(delay).await;
                let _ = s.write_all(text.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    up
}

/// The MCP half: `initialize`, the `initialized` notification, `tools/list`, the session close.
/// `want_key`, if set, is the `x-api-key` the server insists on.
pub fn mcp_reply(r: &Seen, want_key: Option<&str>) -> Option<(Duration, String)> {
    if r.path != "/mcp" {
        return None;
    }
    if let Some(k) = want_key {
        if r.header("x-api-key").as_deref() != Some(k) {
            return Some(reply("401 Unauthorized", r#"{"error":"bad key"}"#));
        }
    }
    if r.method == "DELETE" {
        return Some(reply("200 OK", ""));
    }
    let body = j(&r.body);
    let id = body["id"].clone();
    Some(match body["method"].as_str().unwrap_or("") {
        "initialize" => reply_with(
            "200 OK",
            "mcp-session-id: synthetic-session\r\n",
            &format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-06-18","serverInfo":{{"name":"synthetic"}}}}}}"#
            ),
        ),
        "notifications/initialized" => reply("202 Accepted", ""),
        "tools/list" => reply(
            "200 OK",
            &format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"one","inputSchema":{{"type":"object"}}}}]}}}}"#
            ),
        ),
        _ => reply("400 Bad", ""),
    })
}

// ---------------------------------------------------------------------------------------------
// The front, its store listener and data directory.

pub struct Front {
    pub addr: std::net::SocketAddr,
    pub store: std::net::SocketAddr,
    pub app: Arc<App>,
}

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS mcp_oauth_clients(server_id TEXT PRIMARY KEY, data_enc TEXT NOT NULL, updated_at INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS mcp_oauth_tokens(user_id TEXT NOT NULL, server_id TEXT NOT NULL, data_enc TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY(user_id, server_id));
    CREATE TABLE IF NOT EXISTS directory_mcp_servers(id TEXT PRIMARY KEY, registry_name TEXT NOT NULL UNIQUE, title TEXT NOT NULL, url TEXT NOT NULL, added_by TEXT, added_at INTEGER NOT NULL, headers_enc TEXT, oauth INTEGER NOT NULL DEFAULT 0, personal INTEGER NOT NULL DEFAULT 0, declared_json TEXT NOT NULL DEFAULT '[]');
    CREATE TABLE IF NOT EXISTS directory_mcp_user_keys(user_id TEXT NOT NULL, server_id TEXT NOT NULL, headers_enc TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY(user_id, server_id));
    CREATE TABLE IF NOT EXISTS tool_policies(user_id TEXT NOT NULL, tool TEXT NOT NULL, mode TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY(user_id, tool));";

pub fn seed(dir: &std::path::Path) -> Connection {
    std::fs::write(dir.join("secrets.key"), KEY).unwrap();
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    db.execute_batch(DDL).unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, t) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(t), uid, hex("csrf"), now, now + 86400000),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("users").join(uid)).unwrap();
    }
    db
}

/// A directory server, as core's directory-mcp.cjs `add` writes it.
pub fn add_server(
    db: &Connection,
    id: &str,
    url: &str,
    oauth: bool,
    personal: bool,
    declared: &str,
) {
    db.execute(
        "INSERT INTO directory_mcp_servers(id,registry_name,title,url,added_by,added_at,oauth,personal,declared_json) VALUES(?1,?1,?2,?3,?4,1,?5,?6,?7)",
        (id, format!("Server {id}"), url, A, oauth as i64, personal as i64, declared),
    )
    .unwrap();
}

pub const DECLARED: &str =
    r#"[{"name":"X-Api-Key","required":true,"secret":true,"description":"","template":null}]"#;

pub async fn front(dir: &std::path::Path) -> Front {
    front_with(dir, true).await
}

pub async fn front_with(dir: &std::path::Path, qa_loopback: bool) -> Front {
    front_env(dir, qa_loopback, &[]).await
}

/// The front with extra environment (a synthetic registry: `NOEVIA_QA_MCP_REGISTRY`).
pub async fn front_env(
    dir: &std::path::Path,
    qa_loopback: bool,
    extra: &[(&str, String)],
) -> Front {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
    ]);
    if qa_loopback {
        env.insert("NOEVIA_QA_ALLOW_LOOPBACK_MCP", "1".to_string());
    }
    for (k, v) in extra {
        env.insert(k, v.clone());
    }
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    tokio::spawn(serve(
        listener,
        Arc::clone(&app),
        Limits::default(),
        std::future::pending(),
    ));
    let sl = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let store_addr = sl.local_addr().unwrap();
    let s = project_store::Store {
        data_dir: dir.to_path_buf(),
        token: STORE_TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: "".into(),
    };
    tokio::spawn(project_store::serve_with_context(
        sl,
        s,
        None,
        Some(Arc::clone(&app)),
        std::future::pending(),
    ));
    Front {
        addr,
        store: store_addr,
        app,
    }
}

pub fn open(value: &str, user: Option<&str>) -> serde_json::Value {
    let units: Vec<u16> = value.encode_utf16().collect();
    let (_, plain) = secret_envelope::open(&KEY, None, &units, user.map(str::as_bytes)).unwrap();
    j(std::str::from_utf8(&plain).unwrap())
}

pub fn seal(plain: &str, user: Option<&str>, n: u8) -> String {
    secret_envelope::seal(&KEY, &[n; 12], plain.as_bytes(), user.map(str::as_bytes)).unwrap()
}

pub fn token_row(db: &Connection, user: &str, server: &str) -> Option<String> {
    db.query_row(
        "SELECT data_enc FROM mcp_oauth_tokens WHERE user_id=?1 AND server_id=?2",
        [user, server],
        |r| r.get(0),
    )
    .ok()
}

pub fn key_row(db: &Connection, user: &str, server: &str) -> Option<String> {
    db.query_row(
        "SELECT headers_enc FROM directory_mcp_user_keys WHERE user_id=?1 AND server_id=?2",
        [user, server],
        |r| r.get(0),
    )
    .ok()
}

pub fn audits(db: &Connection, like: &str) -> Vec<String> {
    let mut st = db
        .prepare("SELECT action FROM audit_events WHERE action LIKE ?1 ORDER BY id")
        .unwrap();
    st.query_map([like], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

pub fn state_file(dir: &std::path::Path) -> serde_json::Value {
    j(&std::fs::read_to_string(dir.join("mcp-credential-state.json")).unwrap())
}

pub fn now() -> i64 {
    noevia_server::identity::now_ms()
}

/// A stored sign-in for `user` on `server`.
pub fn store_tokens(
    db: &Connection,
    user: &str,
    server: &str,
    access: &str,
    refresh: Option<&str>,
    expires: Option<i64>,
    n: u8,
) {
    let refresh = refresh.map(|r| format!("\"{r}\"")).unwrap_or("null".into());
    let expires = expires.map(|e| e.to_string()).unwrap_or("null".into());
    let json =
        format!(r#"{{"accessToken":"{access}","refreshToken":{refresh},"expiresAt":{expires}}}"#);
    db.execute(
        "INSERT OR REPLACE INTO mcp_oauth_tokens VALUES(?1,?2,?3,1)",
        (user, server, seal(&json, Some(user), n)),
    )
    .unwrap();
}

/// A stored registered app for `server` pointing at the synthetic service.
pub fn store_client(db: &Connection, server: &str, base: &str, redirect: &str, extra: &str) {
    let json = format!(
        r#"{{"resource":"{base}/mcp","issuer":"{base}","authorizationEndpoint":"{base}/authorize","tokenEndpoint":"{base}/token","registrationEndpoint":"{base}/register","secretMethod":"client_secret_basic","scopes":[],"clientId":"synthetic-client","clientSecret":null,"redirectUri":"{redirect}"{extra}}}"#
    );
    db.execute(
        "INSERT OR REPLACE INTO mcp_oauth_clients VALUES(?1,?2,1)",
        (server, seal(&json, None, 9)),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------------------------

pub fn oauth_reply(r: &Seen, base: &str, counts: &Counts) -> Option<(Duration, String)> {
    let path = r.path.split('?').next().unwrap_or("");
    Some(match path {
        "/mcp" if !r.headers.contains("authorization: bearer") => reply_with(
            "401 Unauthorized",
            &format!("www-authenticate: Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\"\r\n"),
            r#"{"error":"auth"}"#,
        ),
        "/.well-known/oauth-protected-resource/mcp" => reply(
            "200 OK",
            &format!(r#"{{"resource":"{base}/mcp","authorization_servers":["{base}"],"scopes_supported":["read","write"]}}"#),
        ),
        "/.well-known/oauth-authorization-server" => reply(
            "200 OK",
            &format!(r#"{{"issuer":"{base}","authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","registration_endpoint":"{base}/register","revocation_endpoint":"{base}/revoke","code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"]}}"#),
        ),
        "/register" => {
            counts.register.fetch_add(1, Ordering::SeqCst);
            reply("201 Created", r#"{"client_id":"registered-client"}"#)
        }
        "/revoke" => {
            counts.revoke.fetch_add(1, Ordering::SeqCst);
            reply("200 OK", "")
        }
        "/token" => return None,
        _ => reply("404 Not Found", ""),
    })
}

#[derive(Default)]
pub struct Counts {
    pub register: AtomicUsize,
    pub revoke: AtomicUsize,
}

pub fn query(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

// ---------------------------------------------------------------------------------------------
// A configurable synthetic MCP server (and registry).

/// What the synthetic server does right now; tests change it between requests.
pub struct Synth {
    /// The `tools` array `tools/list` answers (JSON text).
    pub tools: Mutex<String>,
    /// The `x-api-key` the server insists on, if any.
    pub key: Mutex<Option<String>>,
    /// The registry listing `/v0/servers` answers (JSON text), if any.
    pub registry: Mutex<Option<String>>,
    /// How long `tools/list` takes.
    pub list_delay: Mutex<Duration>,
    /// Whether the server wants an OAuth sign-in (answers 401 with a challenge to an unauthenticated initialize).
    pub oauth: Mutex<bool>,
    pub lists: AtomicUsize,
    /// After the Nth `tools/list` is answered, the tools become these (a server that changes its mind).
    pub swap_after: Mutex<Option<(usize, String)>>,
}

pub const ECHO: &str = r#"[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]"#;

pub fn synth(up: &Upstream, tools: &str) -> Arc<Synth> {
    let s = Arc::new(Synth {
        tools: Mutex::new(tools.to_owned()),
        key: Mutex::new(None),
        registry: Mutex::new(None),
        list_delay: Mutex::new(Duration::ZERO),
        oauth: Mutex::new(false),
        lists: AtomicUsize::new(0),
        swap_after: Mutex::new(None),
    });
    let (me, base) = (Arc::clone(&s), up.base());
    up.set(move |r: &Seen| {
        let path = r.path.split('?').next().unwrap_or("");
        if path == "/v0/servers" {
            return match me.registry.lock().unwrap().clone() {
                Some(body) => reply("200 OK", &body),
                None => reply("404 Not Found", ""),
            };
        }
        if path != "/mcp" {
            return reply("404 Not Found", "");
        }
        if *me.oauth.lock().unwrap() && !r.headers.contains("authorization: bearer") {
            return reply_with(
                "401 Unauthorized",
                &format!("www-authenticate: Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\"\r\n"),
                r#"{"error":"auth"}"#,
            );
        }
        if let Some(k) = me.key.lock().unwrap().as_deref() {
            // The recorded request is lower-cased whole, values included.
            if r.header("x-api-key") != Some(k.to_ascii_lowercase()) {
                return reply("401 Unauthorized", r#"{"error":"bad key"}"#);
            }
        }
        if r.method == "DELETE" {
            return reply("200 OK", "");
        }
        let body = j(&r.body);
        let id = body["id"].clone();
        match body["method"].as_str().unwrap_or("") {
            "initialize" => reply_with(
                "200 OK",
                "mcp-session-id: synthetic-session\r\n",
                &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-06-18","serverInfo":{{"name":"synthetic"}}}}}}"#),
            ),
            "notifications/initialized" => reply("202 Accepted", ""),
            "tools/list" => {
                let n = me.lists.fetch_add(1, Ordering::SeqCst) + 1;
                let current = me.tools.lock().unwrap().clone();
                if let Some((after, next)) = me.swap_after.lock().unwrap().clone() {
                    if n >= after {
                        *me.tools.lock().unwrap() = next;
                    }
                }
                let (delay, text) = reply(
                    "200 OK",
                    &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":{current}}}}}"#),
                );
                (delay.max(*me.list_delay.lock().unwrap()), text)
            }
            "tools/call" => reply(
                "200 OK",
                &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"echo:{}"}}]}}}}"#, body["params"]["arguments"]["text"].as_str().unwrap_or("")),
            ),
            _ => reply("400 Bad", ""),
        }
    });
    s
}

/// A registry listing of one hosted server at `<base>/mcp`, with an optional key header.
pub fn registry_of(base: &str, name: &str, key_header: bool) -> String {
    let headers = if key_header {
        r#","headers":[{"name":"X-Api-Key","isRequired":true,"isSecret":true,"value":"{k}","description":"Your key"}]"#
    } else {
        ""
    };
    format!(
        r#"{{"servers":[{{"server":{{"name":"{name}","description":"A synthetic server","version":"1.0.0","repository":{{"url":"https://github.com/synthetic/server"}},"remotes":[{{"type":"streamable-http","url":"{base}/mcp"{headers}}}]}}}}]}}"#
    )
}

/// A snapshot as Node publishes it (core mcp-discovery-snapshot.cjs), fresh now.
pub fn write_snapshot(dir: &std::path::Path, body: serde_json::Value) {
    let mut doc = serde_json::json!({
        "version": 1, "kind": "mcp-discovery", "writtenAt": noevia_server::identity::now_ms(), "everyMs": 600000,
        "mcpEnabled": true, "harnessEnabled": false, "hasRepositories": false,
        "nextcloudConfigured": false, "nextcloudOrigins": [],
        "defaults": ["core"], "connectorBoxes": ["gdrive"], "boxes": [], "manifest": [], "servers": [],
    });
    for (k, v) in body.as_object().unwrap() {
        doc[k] = v.clone();
    }
    std::fs::write(dir.join("mcp-discovery.json"), doc.to_string()).unwrap();
}

/// The directory row `id` as a plain value, or None.
pub fn dir_row(db: &Connection, id: &str) -> Option<(String, String, Option<String>, i64, i64)> {
    db.query_row(
        "SELECT title, url, headers_enc, oauth, personal FROM directory_mcp_servers WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .ok()
}

pub fn dir_count(db: &Connection) -> i64 {
    db.query_row("SELECT count(*) FROM directory_mcp_servers", [], |r| {
        r.get(0)
    })
    .unwrap()
}
