//! The MCP credential broker (sbstndalton/noevia#2202) through the real front and the real internal
//! store listener. The MCP server and its OAuth service are one synthetic http server on 127.0.0.1
//! (NOEVIA_QA_ALLOW_LOOPBACK_MCP=1); the legacy upstream is dead, so a request that fell through to
//! Node would fail. No real server, account, token or credential.
#![cfg(feature = "rust-storage-reads")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use base64::Engine as _;
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

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ADMIN: &str = "cowork_session=admin-token; cowork_csrf=csrf";
const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";
const KEY: [u8; 32] = [7; 32];
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";

/// Golden vectors shared with core (server/rust-mcp-credentials.test.cjs): key [7;32], nonce
/// 0x01..0x0c, computed with Node's own `crypto.createCipheriv('aes-256-gcm')` (iv | tag | body,
/// AAD noevia:user:<id> for enc:v2). The JSON is the shape core's mcp-oauth.cjs and
/// directory-mcp.cjs store.
const TOKENS_JSON: &str = r#"{"accessToken":"synthetic-access","refreshToken":"synthetic-refresh","expiresAt":1800000000000}"#;
const TOKENS_GOLDEN: &str = "enc:v2:AQIDBAUGBwgJCgsMI17GHu5yPOtiRO5ls-yDyMBeif0Jj5FfsVFpe_uFmWKnnD9E2i5Gjh2M7fzPPTVtHCAWKmJhuTAK3BzxETwpBOoZEbPdEMh5qMaTYfmN8kPbxfh7anMZGQdGT1_M0DOT7uFhCuBnnCYrqRv3fLmq";
const CLIENT_JSON: &str = r#"{"resource":"https://mcp.example.test/mcp","issuer":"https://auth.example.test","authorizationEndpoint":"https://auth.example.test/authorize","tokenEndpoint":"https://auth.example.test/token","registrationEndpoint":"https://auth.example.test/register","secretMethod":"client_secret_basic","scopes":["read"],"clientId":"synthetic-client","clientSecret":null,"redirectUri":"https://noevia.example.test/api/mcp-oauth/callback"}"#;
const CLIENT_GOLDEN: &str = "enc:v1:AQIDBAUGBwgJCgsM6cDf01pcxElA-gdc2zMrzcBemvsZhZdehlsgJLfP1zSklmsfnSZRl1DE9P7BKCp7EHhRK3MopjYJlmS8Eyo0U7VJQPCRDNRorNzKY6SJ4UXWmPUhJzwMDRIBSV_M5WWd9rEsTrg43n9h-F-uI-eSBcvEEzXBY_WyJ8DDQ6IdgPzy64Ihm9gxYd26WkKTbb34IpjZki3w9Vg4IHD3jVhLhQdDp0t-uLoVLCbcq5QPOm7RRCnHLNm4yDTWmOkCcsHYQ1cg5faU4Ute5gIZdIGIRwmiTDiKDO-ItmAlR9cf98WqZjAXhpTmADcNlP1ogUI0JK_GngyghTtzudO0JtsYFbzSuEv-JtWwhmUyk3FuZsL-YgJRDENb9_EZHwoWVvtuokH7wmNqBxvlJSCqtQDzN9_5XFyXpXZ2HeQKNn4dq68SZRDxy-ra1DJ0651zZnyeUrIRno_c57uZFJPaw2-PrwLWrtaiyZO1hRqalhgjZ1mFpdSeH67PklPv578pTeiS6YZk8t6ulLYJm3Yl8DAMFuWWK5-3yt1zT1EM92Gu1C3w59slZPJI9FVy5HNbr6pAY2JwVwAIVAA";
const KEYS_JSON: &str = r#"{"X-Api-Key":"synthetic-key"}"#;
const KEYS_GOLDEN: &str =
    "enc:v2:AQIDBAUGBwgJCgsMkdk8PVlaYeAV80cSjk3YCsBesLMrmosBrlt7PK-F0Dm6kTlVxiJRyhXE9b3R";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn j(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|_| panic!("not JSON: {s}"))
}

async fn raw_text(
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

async fn raw(
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

async fn write(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    let csrf = format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n");
    raw(addr, method, path, cookie, &csrf, body.as_bytes()).await
}

async fn get(addr: std::net::SocketAddr, path: &str, cookie: &str) -> (u16, serde_json::Value) {
    raw(addr, "GET", path, cookie, "", b"").await
}

/// One internal store call (`/v1/mcp/<op>`).
async fn store(
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
struct Seen {
    method: String,
    path: String,
    headers: String,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<String> {
        self.headers.lines().find_map(|l| {
            l.strip_prefix(&format!("{name}:"))
                .map(|v| v.trim().to_owned())
        })
    }
    fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }
}

type Handler = Arc<dyn Fn(&Seen) -> (Duration, String) + Send + Sync>;

#[derive(Clone)]
struct Upstream {
    addr: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    handler: Arc<Mutex<Handler>>,
    hit: Arc<tokio::sync::Notify>,
}

impl Upstream {
    fn set(&self, f: impl Fn(&Seen) -> (Duration, String) + Send + Sync + 'static) {
        *self.handler.lock().unwrap() = Arc::new(f);
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
    fn count(&self, path: &str) -> usize {
        self.seen()
            .iter()
            .filter(|s| s.path.starts_with(path))
            .count()
    }
    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }
    async fn wait_for(&self, path: &str, n: usize) {
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

fn reply(status: &str, body: &str) -> (Duration, String) {
    reply_with(status, "", body)
}

fn reply_with(status: &str, extra: &str, body: &str) -> (Duration, String) {
    (
        Duration::ZERO,
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}

async fn upstream() -> Upstream {
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
fn mcp_reply(r: &Seen, want_key: Option<&str>) -> Option<(Duration, String)> {
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

struct Front {
    addr: std::net::SocketAddr,
    store: std::net::SocketAddr,
    _app: Arc<App>,
}

const DDL: &str = "CREATE TABLE IF NOT EXISTS mcp_oauth_clients(server_id TEXT PRIMARY KEY, data_enc TEXT NOT NULL, updated_at INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS mcp_oauth_tokens(user_id TEXT NOT NULL, server_id TEXT NOT NULL, data_enc TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY(user_id, server_id));
    CREATE TABLE IF NOT EXISTS directory_mcp_servers(id TEXT PRIMARY KEY, registry_name TEXT NOT NULL UNIQUE, title TEXT NOT NULL, url TEXT NOT NULL, added_by TEXT, added_at INTEGER NOT NULL, headers_enc TEXT, oauth INTEGER NOT NULL DEFAULT 0, personal INTEGER NOT NULL DEFAULT 0, declared_json TEXT NOT NULL DEFAULT '[]');
    CREATE TABLE IF NOT EXISTS directory_mcp_user_keys(user_id TEXT NOT NULL, server_id TEXT NOT NULL, headers_enc TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY(user_id, server_id));";

fn seed(dir: &std::path::Path) -> Connection {
    std::fs::write(dir.join("secrets.key"), KEY).unwrap();
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
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
fn add_server(db: &Connection, id: &str, url: &str, oauth: bool, personal: bool, declared: &str) {
    db.execute(
        "INSERT INTO directory_mcp_servers(id,registry_name,title,url,added_by,added_at,oauth,personal,declared_json) VALUES(?1,?1,?2,?3,?4,1,?5,?6,?7)",
        (id, format!("Server {id}"), url, A, oauth as i64, personal as i64, declared),
    )
    .unwrap();
}

const DECLARED: &str =
    r#"[{"name":"X-Api-Key","required":true,"secret":true,"description":"","template":null}]"#;

async fn front(dir: &std::path::Path) -> Front {
    front_with(dir, true).await
}

async fn front_with(dir: &std::path::Path, qa_loopback: bool) -> Front {
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
        _app: app,
    }
}

fn open(value: &str, user: Option<&str>) -> serde_json::Value {
    let units: Vec<u16> = value.encode_utf16().collect();
    let (_, plain) = secret_envelope::open(&KEY, None, &units, user.map(str::as_bytes)).unwrap();
    j(std::str::from_utf8(&plain).unwrap())
}

fn seal(plain: &str, user: Option<&str>, n: u8) -> String {
    secret_envelope::seal(&KEY, &[n; 12], plain.as_bytes(), user.map(str::as_bytes)).unwrap()
}

fn token_row(db: &Connection, user: &str, server: &str) -> Option<String> {
    db.query_row(
        "SELECT data_enc FROM mcp_oauth_tokens WHERE user_id=?1 AND server_id=?2",
        [user, server],
        |r| r.get(0),
    )
    .ok()
}

fn key_row(db: &Connection, user: &str, server: &str) -> Option<String> {
    db.query_row(
        "SELECT headers_enc FROM directory_mcp_user_keys WHERE user_id=?1 AND server_id=?2",
        [user, server],
        |r| r.get(0),
    )
    .ok()
}

fn audits(db: &Connection, like: &str) -> Vec<String> {
    let mut st = db
        .prepare("SELECT action FROM audit_events WHERE action LIKE ?1 ORDER BY id")
        .unwrap();
    st.query_map([like], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn state_file(dir: &std::path::Path) -> serde_json::Value {
    j(&std::fs::read_to_string(dir.join("mcp-credential-state.json")).unwrap())
}

fn now() -> i64 {
    noevia_server::identity::now_ms()
}

/// A stored sign-in for `user` on `server`.
fn store_tokens(
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
fn store_client(db: &Connection, server: &str, base: &str, redirect: &str, extra: &str) {
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

#[test]
fn golden_vectors_match_node_sealing() {
    let keys = server_auth::secrets::SecretKeys::from_existing_bytes(
        zeroize::Zeroizing::new(KEY.to_vec()),
        None,
    )
    .unwrap();
    let nonce: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    for (plain, golden, user) in [
        (TOKENS_JSON, TOKENS_GOLDEN, Some(A)),
        (CLIENT_JSON, CLIENT_GOLDEN, None),
        (KEYS_JSON, KEYS_GOLDEN, Some(A)),
    ] {
        assert_eq!(keys.seal(plain, user, &nonce).unwrap(), golden);
        let (opened, _) = keys.open(golden, user).unwrap();
        assert_eq!(opened.as_str(), plain);
    }
    assert!(
        keys.open(TOKENS_GOLDEN, Some(B)).is_err(),
        "bound to its owner"
    );
    assert!(
        keys.open(KEYS_GOLDEN, Some(B)).is_err(),
        "bound to its owner"
    );
}

#[tokio::test]
async fn a_key_is_proven_against_the_server_then_kept_sealed_for_its_owner() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    up.set(|r: &Seen| mcp_reply(r, Some("good-key")).unwrap_or_else(|| reply("404 Not Found", "")));
    add_server(
        &db,
        "dir-keyed",
        &format!("{}/mcp", up.base()),
        false,
        true,
        DECLARED,
    );
    add_server(
        &db,
        "dir-shared",
        &format!("{}/mcp", up.base()),
        false,
        false,
        "[]",
    );
    let f = front(d.path()).await;
    let a = f.addr;
    let (st, body) = get(a, "/api/mcp-keys/servers", MEMBER).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        body["servers"].as_array().unwrap().len(),
        1,
        "only the personal server"
    );
    assert_eq!(body["servers"][0]["id"], "dir-keyed");
    assert_eq!(body["servers"][0]["headers"][0]["name"], "X-Api-Key");
    assert_eq!(body["servers"][0]["hasKey"], false);

    // A server that refuses the key: 422, nothing stored, nothing published.
    let (st, body) = write(
        a,
        "PUT",
        "/api/mcp-keys/dir-keyed",
        MEMBER,
        r#"{"headers":{"X-Api-Key":"wrong"}}"#,
    )
    .await;
    assert_eq!(st, 422, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("The server did not accept that key: MCP 401:"),
        "{body}"
    );
    assert_eq!(key_row(&db, B, "dir-keyed"), None);
    // The input checks come first, in core's words.
    for (path, payload, status, message) in [
        ("/api/mcp-keys/dir-keyed", "{", 400, "invalid JSON"),
        ("/api/mcp-keys/dir-keyed", "[]", 400, "invalid JSON"),
        (
            "/api/mcp-keys/dir-keyed",
            r#"{"headers":{}}"#,
            400,
            "Enter X-Api-Key.",
        ),
        (
            "/api/mcp-keys/dir-keyed",
            r#"{"headers":{"X-Api-Key":"a\nb"}}"#,
            400,
            "X-Api-Key must be a single line under 4 KB.",
        ),
        (
            "/api/mcp-keys/dir-shared",
            r#"{"headers":{}}"#,
            404,
            "No such server.",
        ),
        (
            "/api/mcp-keys/dir-missing",
            r#"{"headers":{}}"#,
            404,
            "No such server.",
        ),
    ] {
        let (st, body) = write(a, "PUT", path, MEMBER, payload).await;
        assert_eq!(
            (st, body["error"].as_str().unwrap_or("")),
            (status, message),
            "{path} {payload}"
        );
    }
    assert_eq!(
        up.count("/mcp"),
        1,
        "only the wrong key reached the server: its initialize answered 401"
    );

    // The good key: proven (initialize, initialized, tools/list, close), then sealed enc:v2 for B.
    let before = up.count("/mcp");
    let (st, body) = write(
        a,
        "PUT",
        "/api/mcp-keys/dir-keyed",
        MEMBER,
        r#"{"headers":{"X-Api-Key":"good-key"}}"#,
    )
    .await;
    assert_eq!((st, body), (200, j(r#"{"ok":true}"#)));
    let calls: Vec<String> = up
        .seen()
        .iter()
        .skip(before)
        .map(|s| {
            format!(
                "{} {}",
                s.method,
                serde_json::from_str::<serde_json::Value>(&s.body)
                    .ok()
                    .and_then(|b| b["method"].as_str().map(str::to_owned))
                    .unwrap_or_default()
            )
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            "POST initialize",
            "POST notifications/initialized",
            "POST tools/list",
            "DELETE "
        ]
    );
    let stored = key_row(&db, B, "dir-keyed").unwrap();
    assert!(stored.starts_with("enc:v2:"), "bound to the owner");
    assert_eq!(open(&stored, Some(B)), j(r#"{"X-Api-Key":"good-key"}"#));
    assert!(!stored.contains("good-key"));
    assert_eq!(
        audits(&db, "mcp.directory.user-key%"),
        vec!["mcp.directory.user-key"]
    );
    // Only the owner has it; the readiness file says so and carries no secret.
    assert_eq!(
        get(a, "/api/mcp-keys/servers", MEMBER).await.1["servers"][0]["hasKey"],
        true
    );
    assert_eq!(
        get(a, "/api/mcp-keys/servers", ADMIN).await.1["servers"][0]["hasKey"],
        false
    );
    let state = state_file(d.path());
    assert_eq!(state["keys"][B], j(r#"["dir-keyed"]"#));
    let published = std::fs::read_to_string(d.path().join("mcp-credential-state.json")).unwrap();
    assert!(!published.contains("good-key") && !published.contains("enc:"));
    // A second PUT replaces it.
    up.set(|r: &Seen| {
        mcp_reply(r, Some("better-key")).unwrap_or_else(|| reply("404 Not Found", ""))
    });
    let (st, _) = write(
        a,
        "PUT",
        "/api/mcp-keys/dir-keyed",
        MEMBER,
        r#"{"headers":{"X-Api-Key":"better-key"}}"#,
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(
        open(&key_row(&db, B, "dir-keyed").unwrap(), Some(B)),
        j(r#"{"X-Api-Key":"better-key"}"#)
    );

    // The internal channel hands the headers for one call; another account has none.
    let (st, body) = store(
        f.store,
        "key",
        STORE_TOKEN,
        &format!(r#"{{"user":"{B}","server":"dir-keyed"}}"#),
    )
    .await;
    assert_eq!(
        (st, body),
        (200, j(r#"{"headers":{"X-Api-Key":"better-key"}}"#))
    );
    let (st, body) = store(
        f.store,
        "key",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}","server":"dir-keyed"}}"#),
    )
    .await;
    assert_eq!(
        (st, body["code"].as_str()),
        (401, Some("disconnected")),
        "{body}"
    );
    assert_eq!(
        store(
            f.store,
            "key",
            "wrong-token",
            &format!(r#"{{"user":"{B}","server":"dir-keyed"}}"#)
        )
        .await
        .0,
        401
    );

    // Delete: always ok, even for a server that is not there; the row and the readiness go.
    let (st, body) = write(a, "DELETE", "/api/mcp-keys/dir-keyed", MEMBER, "").await;
    assert_eq!((st, body), (200, j(r#"{"ok":true}"#)));
    assert_eq!(key_row(&db, B, "dir-keyed"), None);
    assert_eq!(state_file(d.path())["keys"], j("{}"));
    assert_eq!(
        write(a, "DELETE", "/api/mcp-keys/dir-nope", MEMBER, "")
            .await
            .0,
        200
    );
    assert_eq!(audits(&db, "mcp.directory.user-key.remove").len(), 2);
}

#[tokio::test]
async fn the_routes_keep_the_sessions_gate() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    add_server(
        &db,
        "dir-keyed",
        "http://127.0.0.1:9/mcp",
        false,
        true,
        DECLARED,
    );
    let f = front(d.path()).await;
    let a = f.addr;
    // Not signed in: refused by the front, not sent on to the dead upstream.
    assert_eq!(
        raw(a, "GET", "/api/mcp-keys/servers", "", "", b"").await.0,
        401
    );
    assert_eq!(
        raw(a, "GET", "/api/mcp-oauth/servers", "", "", b"").await.0,
        401
    );
    assert_eq!(
        raw_text(
            a,
            "GET",
            "/api/mcp-oauth/callback?state=x&code=y",
            "",
            "",
            b""
        )
        .await
        .0,
        401
    );
    // A write without the CSRF token is refused.
    assert_eq!(
        raw(a, "PUT", "/api/mcp-keys/dir-keyed", MEMBER, "", b"{}")
            .await
            .0,
        403
    );
    assert_eq!(
        raw(a, "DELETE", "/api/mcp-oauth/dir-x", MEMBER, "", b"")
            .await
            .0,
        403
    );
    assert_eq!(
        raw(a, "POST", "/api/mcp-oauth/dir-x/connect", MEMBER, "", b"")
            .await
            .0,
        403
    );
    // Methods and shapes that were never core's answers are Node's (here a dead upstream).
    let (st, _) = raw(
        a,
        "POST",
        "/api/mcp-keys/servers",
        MEMBER,
        &format!("Origin: http://{a}\r\nX-CSRF-Token: csrf\r\n"),
        b"{}",
    )
    .await;
    assert_ne!(st, 200);
    assert_eq!(
        write(a, "PUT", "/api/mcp-keys/Dir-Keyed", MEMBER, "{}")
            .await
            .0,
        502
    );
    // The store refuses a caller without the secret.
    assert_eq!(
        store(f.store, "session", "", r#"{"user":"u","server":"s"}"#)
            .await
            .0,
        401
    );
    assert_eq!(store(f.store, "nope", STORE_TOKEN, "{}").await.0, 404);
    assert_eq!(store(f.store, "session", STORE_TOKEN, "[]").await.0, 400);
    assert_eq!(
        store(f.store, "session", STORE_TOKEN, r#"{"user":"u"}"#)
            .await
            .0,
        400
    );
}

/// The OAuth half of the synthetic service. `counts` observe the token endpoint.
fn oauth_reply(r: &Seen, base: &str, counts: &Counts) -> Option<(Duration, String)> {
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
struct Counts {
    register: AtomicUsize,
    revoke: AtomicUsize,
}

fn query(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

#[tokio::test]
async fn an_oauth_sign_in_end_to_end_with_pkce_state_and_a_sealed_token() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let base = up.base();
    let counts = Arc::new(Counts::default());
    {
        let (base, counts) = (base.clone(), Arc::clone(&counts));
        up.set(move |r: &Seen| {
            if let Some(answer) = oauth_reply(r, &base, &counts) {
                return answer;
            }
            // /token
            let form = r.form();
            if form.get("grant_type").map(String::as_str) == Some("authorization_code") {
                reply(
                    "200 OK",
                    r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600}"#,
                )
            } else {
                reply("400 Bad Request", r#"{"error":"unsupported_grant_type"}"#)
            }
        });
    }
    add_server(&db, "dir-oauth", &format!("{base}/mcp"), true, false, "[]");
    let f = front(d.path()).await;
    let a = f.addr;
    let (st, body) = get(a, "/api/mcp-oauth/servers", MEMBER).await;
    assert_eq!(st, 200);
    assert_eq!(
        body,
        j(
            r#"{"servers":[{"id":"dir-oauth","title":"Server dir-oauth","connected":false,"needsReauth":false}]}"#
        )
    );
    assert_eq!(
        write(a, "POST", "/api/mcp-oauth/dir-missing/connect", MEMBER, "").await,
        (404, j(r#"{"error":"No such sign-in server."}"#))
    );

    // Connect: discovery from the 401 challenge, one registration, and the authorisation address.
    let (st, body) = write(a, "POST", "/api/mcp-oauth/dir-oauth/connect", MEMBER, "").await;
    assert_eq!(st, 200, "{body}");
    let sign_in = body["signIn"].as_str().unwrap().to_owned();
    assert!(
        sign_in.starts_with(&format!("{base}/authorize?")),
        "{sign_in}"
    );
    let q = query(&sign_in);
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], "registered-client");
    assert_eq!(
        q["redirect_uri"],
        format!("http://{a}/api/mcp-oauth/callback")
    );
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["resource"], format!("{base}/mcp"));
    assert_eq!(q["scope"], "read write");
    assert_eq!(q["code_challenge"].len(), 43);
    assert!(q["state"].len() >= 32);
    assert_eq!(counts.register.load(Ordering::SeqCst), 1);
    // The registered app is kept sealed (unbound, enc:v1) in core's shape.
    let client_row: String = db
        .query_row(
            "SELECT data_enc FROM mcp_oauth_clients WHERE server_id='dir-oauth'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(client_row.starts_with("enc:v1:"));
    let client = open(&client_row, None);
    assert_eq!(client["clientId"], "registered-client");
    assert_eq!(
        client["redirectUri"],
        format!("http://{a}/api/mcp-oauth/callback")
    );
    assert_eq!(client["issuer"], base);
    // A second sign-in reuses it.
    let (st, body2) = write(a, "POST", "/api/mcp-oauth/dir-oauth/connect", MEMBER, "").await;
    assert_eq!(st, 200, "{body2}");
    assert_eq!(counts.register.load(Ordering::SeqCst), 1);
    let second = query(body2["signIn"].as_str().unwrap());
    assert_ne!(second["state"], q["state"]);

    // The state is bound to the account that started it: another account is refused and cannot
    // burn it for its owner.
    let state = q["state"].clone();
    let (st, _, page) = raw_text(
        a,
        "GET",
        &format!("/api/mcp-oauth/callback?state={state}&code=c1"),
        ADMIN,
        "",
        b"",
    )
    .await;
    assert_eq!(st, 400);
    assert!(page.contains("started by a different account"), "{page}");
    // A service that names another issuer is refused (RFC 9207), and that spends the state.
    let (st, _, page) = raw_text(
        a,
        "GET",
        &format!(
            "/api/mcp-oauth/callback?state={}&code=c1&iss=https%3A%2F%2Fevil.example",
            second["state"]
        ),
        MEMBER,
        "",
        b"",
    )
    .await;
    assert_eq!(st, 400);
    assert!(page.contains("different sign-in service"), "{page}");
    assert_eq!(
        up.count("/token"),
        0,
        "no code was spent on a refused answer"
    );

    // The owner completes it: the code is exchanged with the verifier that matches the challenge.
    let (st, headers, page) = raw_text(
        a,
        "GET",
        &format!("/api/mcp-oauth/callback?state={state}&code=c1"),
        MEMBER,
        "",
        b"",
    )
    .await;
    assert_eq!(st, 200, "{page}");
    assert!(
        page.contains("Signed in") && page.contains("Your account is connected."),
        "{page}"
    );
    assert!(
        headers.contains("content-type: text/html; charset=utf-8")
            && headers.contains("cache-control: no-store")
    );
    assert!(headers.contains("content-security-policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'"));
    let exchange = up.seen().into_iter().find(|s| s.path == "/token").unwrap();
    let form = exchange.form();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "c1");
    assert_eq!(form["client_id"], "registered-client");
    assert_eq!(
        form["redirect_uri"],
        format!("http://{a}/api/mcp-oauth/callback")
    );
    assert_eq!(form["resource"], format!("{base}/mcp"));
    let verifier = &form["code_verifier"];
    assert_eq!(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes())),
        q["code_challenge"]
    );
    assert!(
        !exchange.headers.contains("authorization:"),
        "an app without a secret sends none"
    );
    // Sealed enc:v2 for its owner, in core's three-key shape; never in the readiness file.
    let stored = token_row(&db, B, "dir-oauth").unwrap();
    assert!(stored.starts_with("enc:v2:") && !stored.contains("access-1"));
    let tokens = open(&stored, Some(B));
    assert_eq!(
        (
            tokens["accessToken"].as_str(),
            tokens["refreshToken"].as_str()
        ),
        (Some("access-1"), Some("refresh-1"))
    );
    assert!(tokens["expiresAt"].as_i64().unwrap() > now() + 3_000_000);
    assert_eq!(audits(&db, "mcp.oauth.%"), vec!["mcp.oauth.connect"]);
    assert_eq!(state_file(d.path())["oauth"][B]["dir-oauth"], "connected");
    assert!(
        !std::fs::read_to_string(d.path().join("mcp-credential-state.json"))
            .unwrap()
            .contains("access-1")
    );
    // The state is single use.
    let (st, _, page) = raw_text(
        a,
        "GET",
        &format!("/api/mcp-oauth/callback?state={state}&code=c1"),
        MEMBER,
        "",
        b"",
    )
    .await;
    assert_eq!(st, 400);
    assert!(page.contains("expired or was already used"), "{page}");

    // Listed as connected for B only; Node's chat gets the access token over the store.
    assert_eq!(
        get(a, "/api/mcp-oauth/servers", MEMBER).await.1["servers"][0]["connected"],
        true
    );
    assert_eq!(
        get(a, "/api/mcp-oauth/servers", ADMIN).await.1["servers"][0]["connected"],
        false
    );
    let (st, body) = store(
        f.store,
        "session",
        STORE_TOKEN,
        &format!(r#"{{"user":"{B}","server":"dir-oauth"}}"#),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["accessToken"], "access-1");
    assert!(
        body.get("refreshToken").is_none(),
        "a refresh token never leaves the broker"
    );
    assert_eq!(body["generation"].as_str().unwrap().len(), 32);
    let (st, body) = store(
        f.store,
        "session",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}","server":"dir-oauth"}}"#),
    )
    .await;
    assert_eq!((st, body["code"].as_str()), (401, Some("disconnected")));

    // Disconnect: the row goes, the service is told (RFC 7009), the readiness follows.
    let (st, body) = write(a, "DELETE", "/api/mcp-oauth/dir-oauth", MEMBER, "").await;
    assert_eq!((st, body), (200, j(r#"{"ok":true}"#)));
    assert_eq!(token_row(&db, B, "dir-oauth"), None);
    up.wait_for("/revoke", 1).await;
    let revoked = up
        .seen()
        .into_iter()
        .find(|s| s.path == "/revoke")
        .unwrap()
        .form();
    assert_eq!(
        (
            revoked["token"].as_str(),
            revoked["token_type_hint"].as_str()
        ),
        ("refresh-1", "refresh_token")
    );
    assert_eq!(state_file(d.path())["oauth"], j("{}"));
    assert!(audits(&db, "mcp.oauth.%").contains(&"mcp.oauth.disconnect".to_string()));
}

#[tokio::test]
async fn a_refresh_is_single_flight_rotates_the_token_and_never_double_spends() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let base = up.base();
    add_server(&db, "dir-oauth", &format!("{base}/mcp"), true, false, "[]");
    let f = front(d.path()).await;
    let redirect_uri = format!("http://{}/api/mcp-oauth/callback", f.addr);
    store_client(&db, "dir-oauth", &base, &redirect_uri, "");
    let refreshes = Arc::new(AtomicUsize::new(0));
    {
        let refreshes = Arc::clone(&refreshes);
        up.set(move |r: &Seen| {
            if r.path != "/token" {
                return reply("404 Not Found", "");
            }
            let form = r.form();
            // A rotating service: a refresh token works once.
            if form["grant_type"] == "refresh_token" && form["refresh_token"] == "refresh-0" {
                let n = refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                let (_, text) = reply("200 OK", &format!(r#"{{"access_token":"access-{n}","refresh_token":"refresh-{n}","expires_in":3600}}"#));
                return (Duration::from_millis(250), text);
            }
            reply("400 Bad Request", r#"{"error":"invalid_grant"}"#)
        });
    }
    store_tokens(
        &db,
        B,
        "dir-oauth",
        "access-old",
        Some("refresh-0"),
        Some(now() + 10_000),
        3,
    );
    let body = format!(r#"{{"user":"{B}","server":"dir-oauth"}}"#);
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let (addr, body) = (f.store, body.clone());
            tokio::spawn(async move { store(addr, "session", STORE_TOKEN, &body).await })
        })
        .collect();
    for h in handles {
        let (st, ans) = h.await.unwrap();
        assert_eq!(
            (st, ans["accessToken"].as_str()),
            (200, Some("access-1")),
            "{ans}"
        );
    }
    assert_eq!(
        refreshes.load(Ordering::SeqCst),
        1,
        "one refresh for six waiters"
    );
    assert_eq!(up.count("/token"), 1);
    let tokens = open(&token_row(&db, B, "dir-oauth").unwrap(), Some(B));
    assert_eq!(
        (
            tokens["accessToken"].as_str(),
            tokens["refreshToken"].as_str()
        ),
        (Some("access-1"), Some("refresh-1")),
        "the rotated refresh token is the stored one"
    );
    // Fresh now: no more calls to the service.
    let (st, ans) = store(f.store, "session", STORE_TOKEN, &body).await;
    assert_eq!((st, ans["accessToken"].as_str()), (200, Some("access-1")));
    assert_eq!(up.count("/token"), 1);
}

#[tokio::test]
async fn a_refresh_the_caller_abandons_is_still_stored() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let base = up.base();
    add_server(&db, "dir-oauth", &format!("{base}/mcp"), true, false, "[]");
    let f = front(d.path()).await;
    store_client(
        &db,
        "dir-oauth",
        &base,
        &format!("http://{}/api/mcp-oauth/callback", f.addr),
        "",
    );
    up.set(|r: &Seen| {
        if r.path == "/token" {
            let (_, text) = reply(
                "200 OK",
                r#"{"access_token":"access-new","refresh_token":"refresh-new","expires_in":3600}"#,
            );
            return (Duration::from_millis(400), text);
        }
        reply("404 Not Found", "")
    });
    store_tokens(
        &db,
        B,
        "dir-oauth",
        "access-old",
        Some("refresh-old"),
        Some(now() - 1000),
        3,
    );
    // Ask, then hang up before the service has answered.
    let body = format!(r#"{{"user":"{B}","server":"dir-oauth"}}"#);
    let mut s = TcpStream::connect(f.store).await.unwrap();
    s.write_all(format!("POST /v1/mcp/session HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{}: {STORE_TOKEN}\r\n\r\n{body}", body.len(), project_store::TOKEN_HEADER).as_bytes()).await.unwrap();
    up.wait_for("/token", 1).await;
    drop(s);
    let rotated = async {
        loop {
            let row = token_row(&db, B, "dir-oauth").map(|r| open(&r, Some(B)));
            if row
                .as_ref()
                .is_some_and(|t| t["refreshToken"] == "refresh-new")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), rotated)
        .await
        .expect("the rotated refresh token was lost with the caller");
}

#[tokio::test]
async fn a_revoked_grant_needs_a_new_sign_in_and_a_flaky_service_does_not() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let base = up.base();
    add_server(&db, "dir-oauth", &format!("{base}/mcp"), true, false, "[]");
    let f = front(d.path()).await;
    let a = f.addr;
    store_client(
        &db,
        "dir-oauth",
        &base,
        &format!("http://{a}/api/mcp-oauth/callback"),
        "",
    );
    let mode = Arc::new(Mutex::new("revoked"));
    {
        let mode = Arc::clone(&mode);
        up.set(move |r: &Seen| {
            if r.path != "/token" {
                return reply("404 Not Found", "");
            }
            match *mode.lock().unwrap() {
                "revoked" => reply("400 Bad Request", r#"{"error":"invalid_grant"}"#),
                "down" => reply("503 Service Unavailable", ""),
                _ => reply("200 OK", r#"{"access_token":"access-2","expires_in":3600}"#),
            }
        });
    }
    let body = format!(r#"{{"user":"{B}","server":"dir-oauth"}}"#);
    // Flaky: a token still inside its life is kept (never "disconnected"); an expired one is a 503.
    *mode.lock().unwrap() = "down";
    store_tokens(
        &db,
        B,
        "dir-oauth",
        "access-1",
        Some("refresh-1"),
        Some(now() + 20_000),
        3,
    );
    let (st, ans) = store(f.store, "session", STORE_TOKEN, &body).await;
    assert_eq!(
        (st, ans["accessToken"].as_str()),
        (200, Some("access-1")),
        "{ans}"
    );
    store_tokens(
        &db,
        B,
        "dir-oauth",
        "access-1",
        Some("refresh-1"),
        Some(now() - 5_000),
        3,
    );
    let (st, ans) = store(f.store, "session", STORE_TOKEN, &body).await;
    assert_eq!(
        (st, ans["code"].as_str()),
        (503, Some("transient")),
        "{ans}"
    );
    assert!(
        token_row(&db, B, "dir-oauth").is_some(),
        "a flaky service never costs the sign-in"
    );
    assert_eq!(
        open(&token_row(&db, B, "dir-oauth").unwrap(), Some(B))["refreshToken"],
        "refresh-1"
    );

    // The service says the grant is gone: the account must sign in again, and it is told so.
    *mode.lock().unwrap() = "revoked";
    let (st, ans) = store(f.store, "session", STORE_TOKEN, &body).await;
    assert_eq!(
        (st, ans["code"].as_str()),
        (401, Some("reconnect")),
        "{ans}"
    );
    let dead = open(&token_row(&db, B, "dir-oauth").unwrap(), Some(B));
    assert!(
        dead["refreshToken"].is_null() && dead["expiresAt"] == 1,
        "{dead}"
    );
    let seen = up.count("/token");
    let (st, ans) = store(f.store, "session", STORE_TOKEN, &body).await;
    assert_eq!((st, ans["code"].as_str()), (401, Some("reconnect")));
    assert_eq!(
        up.count("/token"),
        seen,
        "a dead sign-in is not tried again"
    );
    let (_, list) = get(a, "/api/mcp-oauth/servers", MEMBER).await;
    assert_eq!(
        (
            list["servers"][0]["connected"].as_bool(),
            list["servers"][0]["needsReauth"].as_bool()
        ),
        (Some(false), Some(true))
    );
    assert_eq!(state_file(d.path())["oauth"][B]["dir-oauth"], "reauth");
    assert!(audits(&db, "mcp.oauth.%").contains(&"mcp.oauth.reconnect_needed".to_string()));

    // A fresh sign-in replaces it. A token the MCP server answered 401 to is refreshed past once,
    // and only while it is still the stored one.
    *mode.lock().unwrap() = "ok";
    store_tokens(
        &db,
        B,
        "dir-oauth",
        "access-1",
        Some("refresh-1"),
        Some(now() + 3_600_000),
        4,
    );
    let (st, ans) = store(
        f.store,
        "session",
        STORE_TOKEN,
        &format!(r#"{{"user":"{B}","server":"dir-oauth","rejected":"access-1"}}"#),
    )
    .await;
    assert_eq!(
        (st, ans["accessToken"].as_str()),
        (200, Some("access-2")),
        "{ans}"
    );
    let (st, ans) = store(
        f.store,
        "session",
        STORE_TOKEN,
        &format!(r#"{{"user":"{B}","server":"dir-oauth","rejected":"access-1"}}"#),
    )
    .await;
    assert_eq!(
        (st, ans["accessToken"].as_str()),
        (200, Some("access-2")),
        "an older rejected token does not refresh again"
    );
    // The second 401 on the new token names its generation and marks THAT sign-in.
    let generation = ans["generation"].as_str().unwrap().to_owned();
    let (st, ans) = store(
        f.store,
        "reject",
        STORE_TOKEN,
        &format!(
            r#"{{"user":"{B}","server":"dir-oauth","generation":"{}"}}"#,
            "0".repeat(32)
        ),
    )
    .await;
    assert_eq!(
        (st, ans["marked"].as_bool()),
        (200, Some(false)),
        "a stale generation marks nothing"
    );
    let (st, ans) = store(
        f.store,
        "reject",
        STORE_TOKEN,
        &format!(r#"{{"user":"{B}","server":"dir-oauth","generation":"{generation}"}}"#),
    )
    .await;
    assert_eq!((st, ans["marked"].as_bool()), (200, Some(true)));
    assert_eq!(state_file(d.path())["oauth"][B]["dir-oauth"], "reauth");
}

#[tokio::test]
async fn an_oauth_service_is_a_stranger_and_is_held_to_the_public_https_rules() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let base = up.base();
    add_server(&db, "dir-oauth", &format!("{base}/mcp"), true, false, "[]");
    let f = front(d.path()).await;
    let a = f.addr;
    let counts = Arc::new(Counts::default());
    // What the synthetic service's metadata says is varied per case.
    let meta = Arc::new(Mutex::new(String::new()));
    let prm = Arc::new(Mutex::new(None::<String>));
    {
        let (base, counts, meta, prm) = (
            base.clone(),
            Arc::clone(&counts),
            Arc::clone(&meta),
            Arc::clone(&prm),
        );
        up.set(move |r: &Seen| {
            let path = r.path.split('?').next().unwrap_or("");
            if path == "/.well-known/oauth-authorization-server" {
                return reply("200 OK", &meta.lock().unwrap());
            }
            if path.starts_with("/.well-known/oauth-protected-resource") {
                return match prm.lock().unwrap().clone() {
                    Some(text) if text.starts_with("302 ") => {
                        reply_with("302 Found", "location: /elsewhere\r\n", "")
                    }
                    Some(text) => reply("200 OK", &text),
                    None => reply("404 Not Found", ""),
                };
            }
            oauth_reply(r, &base, &counts).unwrap_or_else(|| reply("404 Not Found", ""))
        });
    }
    let connect =
        || async { write(a, "POST", "/api/mcp-oauth/dir-oauth/connect", MEMBER, "").await };
    let good = |extra: &str| {
        format!(
            r#"{{"issuer":"{base}","authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","registration_endpoint":"{base}/register","code_challenge_methods_supported":["S256"]{extra}}}"#
        )
    };

    // Endpoints that are not public https addresses are refused, whichever of them it is.
    for (field, value, what) in [
        ("token_endpoint", "https://10.0.0.5/token", "token"),
        (
            "authorization_endpoint",
            "https://192.168.1.2/authorize",
            "authorization",
        ),
        (
            "registration_endpoint",
            "http://example.test/register",
            "registration",
        ),
        ("token_endpoint", "https://localhost/token", "token"),
        ("token_endpoint", "https://169.254.169.254/token", "token"),
        (
            "token_endpoint",
            "https://metadata.google.internal/token",
            "token",
        ),
    ] {
        let mut m = j(&good(""));
        m[field] = value.into();
        *meta.lock().unwrap() = m.to_string();
        let (st, body) = connect().await;
        assert_eq!(st, 502, "{field} {value}: {body}");
        assert_eq!(
            body["error"],
            format!("The sign-in service’s {what} address is not a public https address."),
            "{field} {value}"
        );
    }
    // PKCE S256 is required; the issuer named must be the issuer asked.
    *meta.lock().unwrap() = good("").replace(r#"["S256"]"#, r#"["plain"]"#);
    assert_eq!(
        connect().await.1["error"],
        "The sign-in service does not support PKCE (S256)."
    );
    *meta.lock().unwrap() = good("").replace(
        &format!(r#""issuer":"{base}""#),
        r#""issuer":"https://evil.example""#,
    );
    assert_eq!(
        connect().await.1["error"],
        "The sign-in service names a different issuer than the one that was asked."
    );
    // A resource document that names another resource is refused.
    *meta.lock().unwrap() = good("");
    *prm.lock().unwrap() = Some(format!(
        r#"{{"resource":"https://other.example/mcp","authorization_servers":["{base}"]}}"#
    ));
    assert_eq!(
        connect().await.1["error"],
        "The server's sign-in information names a different resource."
    );
    // No registration endpoint: an administrator must register an app by hand, and members are told so.
    *prm.lock().unwrap() = None;
    *meta.lock().unwrap() = good("").replace(
        &format!(r#""registration_endpoint":"{base}/register","#),
        "",
    );
    let (st, body) = connect().await;
    assert_eq!(
        (st, body["error"].as_str()),
        (
            409,
            Some(
                "An administrator has to finish setting this server up before anyone can sign in."
            )
        )
    );
    assert_eq!(counts.register.load(Ordering::SeqCst), 0);
    // A redirect is never followed: the 302 reads as "no resource document", and the target is not asked.
    *prm.lock().unwrap() = Some("302 ".into());
    *meta.lock().unwrap() = good("");
    let (st, body) = connect().await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(up.count("/elsewhere"), 0);
    // And one that is fine, for the record, registers once.
    assert_eq!(counts.register.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_administrators_calls_start_set_an_app_forget_and_store_a_key() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let base = up.base();
    let counts = Arc::new(Counts::default());
    {
        let (base, counts) = (base.clone(), Arc::clone(&counts));
        up.set(move |r: &Seen| {
            oauth_reply(r, &base, &counts)
                .unwrap_or_else(|| reply("200 OK", r#"{"access_token":"a","expires_in":60}"#))
        });
    }
    let url = format!("{base}/mcp");
    add_server(&db, "dir-oauth", &url, true, false, "[]");
    add_server(&db, "dir-keyed", &url, false, true, DECLARED);
    let f = front(d.path()).await;
    let s = f.store;
    let call =
        |op: &'static str, body: String| async move { store(s, op, STORE_TOKEN, &body).await };

    // start: for the directory's own address only; purpose add is carried to the page.
    let (st, body) = call(
        "start",
        format!(r#"{{"user":"{A}","server":"dir-oauth","serverUrl":"{url}","purpose":"add"}}"#),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    assert!(body["signIn"]
        .as_str()
        .unwrap()
        .starts_with(&format!("{base}/authorize?")));
    let state = query(body["signIn"].as_str().unwrap())["state"].clone();
    assert_eq!(
        call(
            "start",
            format!(
                r#"{{"user":"{A}","server":"dir-oauth","serverUrl":"https://other.example/mcp"}}"#
            )
        )
        .await
        .0,
        404
    );
    assert_eq!(
        call(
            "start",
            format!(r#"{{"user":"{A}","server":"dir-keyed","serverUrl":"{url}"}}"#)
        )
        .await
        .0,
        404,
        "not a sign-in server"
    );
    let (st, _, page) = raw_text(
        f.addr,
        "GET",
        &format!("/api/mcp-oauth/callback?state={state}&code=c"),
        ADMIN,
        "",
        b"",
    )
    .await;
    assert_eq!(st, 200, "{page}");
    assert!(page.contains("The server is added."), "{page}");

    // client: a hand-registered app; its secret never comes back; older sign-ins are dropped.
    store_tokens(&db, B, "dir-oauth", "x", Some("y"), None, 5);
    let (st, body) = call("client", format!(r#"{{"server":"dir-oauth","serverUrl":"{url}","clientId":"manual-id","clientSecret":"manual-secret"}}"#)).await;
    assert_eq!((st, body), (200, j(r#"{"ok":true}"#)));
    assert_eq!(
        token_row(&db, B, "dir-oauth"),
        None,
        "sign-ins made with another app are not valid for this one"
    );
    let (st, body) = call("client-info", r#"{"server":"dir-oauth"}"#.into()).await;
    assert_eq!(st, 200);
    assert_eq!(body["client"]["manual"], true);
    assert_eq!(body["client"]["clientId"], "manual-id");
    assert_eq!(body["client"]["hasSecret"], true);
    assert!(!body.to_string().contains("manual-secret"));
    let row: String = db
        .query_row(
            "SELECT data_enc FROM mcp_oauth_clients WHERE server_id='dir-oauth'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let client = open(&row, None);
    assert_eq!(
        (
            client["manual"].as_bool(),
            client["clientSecret"].as_str(),
            client["tokenAuth"].as_str()
        ),
        (
            Some(true),
            Some("manual-secret"),
            Some("client_secret_basic")
        )
    );
    for (payload, message) in [
        (
            r#"{"clientId":"has space"}"#,
            "Enter the client ID the service gave you (one word, no spaces).",
        ),
        (
            r#"{"clientId":"ok","clientSecret":"a\nb"}"#,
            "The client secret must be one line.",
        ),
    ] {
        let (st, body) = call(
            "client",
            format!(
                r#"{{"server":"dir-oauth","serverUrl":"{url}",{}"#,
                &payload[1..]
            ),
        )
        .await;
        assert_eq!((st, body["error"].as_str()), (400, Some(message)));
    }
    // A manual app is used as it is: the next sign-in does not register.
    let registered = counts.register.load(Ordering::SeqCst);
    let (st, _) = call(
        "start",
        format!(r#"{{"user":"{A}","server":"dir-oauth","serverUrl":"{url}"}}"#),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(counts.register.load(Ordering::SeqCst), registered);
    assert_eq!(audits(&db, "mcp.oauth.client").len(), 1);

    // key-set: the adding administrator's key for a personal server, sealed for them.
    let (st, body) = call(
        "key-set",
        format!(r#"{{"user":"{A}","server":"dir-keyed","headers":{{"X-Api-Key":"admin-key"}}}}"#),
    )
    .await;
    assert_eq!((st, body), (200, j(r#"{"ok":true}"#)));
    assert_eq!(
        open(&key_row(&db, A, "dir-keyed").unwrap(), Some(A)),
        j(r#"{"X-Api-Key":"admin-key"}"#)
    );
    assert_eq!(
        call(
            "key-set",
            format!(r#"{{"user":"{A}","server":"dir-oauth","headers":{{"K":"v"}}}}"#)
        )
        .await
        .0,
        404,
        "not a personal server"
    );
    assert_eq!(
        call(
            "key-set",
            format!(r#"{{"user":"{A}","server":"dir-keyed","headers":{{"K":"a\nb"}}}}"#)
        )
        .await
        .0,
        400
    );
    assert_eq!(
        call(
            "key-set",
            format!(r#"{{"user":"{A}","server":"dir-keyed","headers":{{}}}}"#)
        )
        .await
        .0,
        400
    );

    // forget: a removed server takes every account's sign-in, its app and every key; an account
    // takes its own; a call that names both or neither is refused.
    store_tokens(&db, A, "dir-oauth", "x", Some("y"), None, 5);
    store_tokens(&db, B, "dir-oauth", "x", Some("y"), None, 5);
    store_tokens(&db, B, "dir-other", "x", Some("y"), None, 5);
    db.execute(
        "INSERT INTO directory_mcp_user_keys VALUES(?1,'dir-keyed','x',1)",
        [B],
    )
    .unwrap();
    assert_eq!(
        call(
            "forget",
            format!(r#"{{"user":"{B}","server":"dir-oauth"}}"#)
        )
        .await
        .0,
        400
    );
    assert_eq!(call("forget", "{}".into()).await.0, 400);
    assert_eq!(call("forget", format!(r#"{{"user":"{B}"}}"#)).await.0, 200);
    assert_eq!(token_row(&db, B, "dir-oauth"), None);
    assert_eq!(token_row(&db, B, "dir-other"), None);
    assert_eq!(key_row(&db, B, "dir-keyed"), None);
    assert!(token_row(&db, A, "dir-oauth").is_some());
    assert_eq!(
        call("forget", r#"{"server":"dir-oauth"}"#.into()).await.0,
        200
    );
    assert_eq!(token_row(&db, A, "dir-oauth"), None);
    let clients: i64 = db
        .query_row(
            "SELECT count(*) FROM mcp_oauth_clients WHERE server_id='dir-oauth'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(clients, 0);
    let keys: i64 = db
        .query_row("SELECT count(*) FROM directory_mcp_user_keys", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        keys, 1,
        "the keys of another server are not touched... until it is forgotten"
    );
    assert_eq!(
        call("forget", r#"{"server":"dir-keyed"}"#.into()).await.0,
        200
    );
    let keys: i64 = db
        .query_row("SELECT count(*) FROM directory_mcp_user_keys", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(keys, 0);
}

#[tokio::test]
async fn a_rotation_reseals_the_three_tables_under_the_new_key_and_loses_nothing() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    add_server(
        &db,
        "dir-oauth",
        "http://127.0.0.1:9/mcp",
        true,
        true,
        DECLARED,
    );
    // The previous key sealed these; the current one is secrets.key.
    let old = [9u8; 32];
    let seal_old = |plain: &str, user: Option<&str>| {
        secret_envelope::seal(&old, &[1; 12], plain.as_bytes(), user.map(str::as_bytes)).unwrap()
    };
    db.execute(
        "INSERT INTO mcp_oauth_tokens VALUES(?1,'dir-oauth',?2,1)",
        (A, seal_old(TOKENS_JSON, Some(A))),
    )
    .unwrap();
    let current = seal(TOKENS_JSON, Some(B), 4);
    db.execute(
        "INSERT INTO mcp_oauth_tokens VALUES(?1,'dir-oauth',?2,1)",
        (B, &current),
    )
    .unwrap(); // already current
    db.execute(
        "INSERT INTO mcp_oauth_clients VALUES('dir-oauth',?1,1)",
        [seal_old(CLIENT_JSON, None)],
    )
    .unwrap();
    db.execute(
        "INSERT INTO directory_mcp_user_keys VALUES(?1,'dir-oauth',?2,1)",
        (A, seal_old(KEYS_JSON, Some(A))),
    )
    .unwrap();
    db.execute("INSERT INTO directory_mcp_user_keys VALUES(?1,'dir-oauth','enc:v2:AAAA-not-a-sealed-value',1)", [B]).unwrap(); // no key opens it
    add_server(
        &db,
        "dir-b",
        "http://127.0.0.1:9/mcp",
        false,
        true,
        DECLARED,
    );
    db.execute(
        "INSERT INTO directory_mcp_user_keys VALUES(?1,'dir-b','',1)",
        [A],
    )
    .unwrap(); // empty
    std::fs::write(d.path().join("secrets.key.previous"), old).unwrap();
    let f = front(d.path()).await;
    let (st, body) = store(f.store, "reseal", STORE_TOKEN, "{}").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(
        body["tables"]["mcp_oauth_tokens"],
        j(r#"{"current":1,"rotated":1,"upgraded":0,"empty":0,"failed":0}"#)
    );
    assert_eq!(
        body["tables"]["mcp_oauth_clients"],
        j(r#"{"current":0,"rotated":1,"upgraded":0,"empty":0,"failed":0}"#)
    );
    assert_eq!(
        body["tables"]["directory_mcp_user_keys"],
        j(r#"{"current":0,"rotated":1,"upgraded":0,"empty":1,"failed":1}"#)
    );
    assert_eq!(body["failures"].as_array().unwrap().len(), 1);
    assert_eq!(body["failures"][0]["table"], "directory_mcp_user_keys");
    assert_eq!(
        body["failures"][0]["ref"],
        j(&format!(r#"["{B}","dir-oauth"]"#))
    );
    assert!(
        !body.to_string().contains("synthetic"),
        "no secret in the report"
    );
    // Re-sealed under the current key with the same plaintext; the unopenable row is untouched.
    assert_eq!(
        open(&token_row(&db, A, "dir-oauth").unwrap(), Some(A)),
        j(TOKENS_JSON)
    );
    assert_eq!(
        token_row(&db, B, "dir-oauth").as_deref(),
        Some(current.as_str())
    );
    let client: String = db
        .query_row("SELECT data_enc FROM mcp_oauth_clients", [], |r| r.get(0))
        .unwrap();
    assert!(client.starts_with("enc:v1:"));
    assert_eq!(open(&client, None), j(CLIENT_JSON));
    assert_eq!(
        open(&key_row(&db, A, "dir-oauth").unwrap(), Some(A)),
        j(KEYS_JSON)
    );
    assert_eq!(
        key_row(&db, B, "dir-oauth").as_deref(),
        Some("enc:v2:AAAA-not-a-sealed-value")
    );
    // Idempotent: a second pass finds every opened row current.
    let (_, again) = store(f.store, "reseal", STORE_TOKEN, "{}").await;
    assert_eq!(
        again["tables"]["mcp_oauth_tokens"],
        j(r#"{"current":2,"rotated":0,"upgraded":0,"empty":0,"failed":0}"#)
    );
    // A rotation while no usable keys exist changes nothing and says so.
    std::fs::remove_file(d.path().join("secrets.key")).unwrap();
    let (st, body) = store(f.store, "reseal", STORE_TOKEN, "{}").await;
    assert_eq!(
        (st, body["error"].as_str()),
        (503, Some("credential store unavailable"))
    );
}

#[tokio::test]
async fn the_start_sweep_removes_what_deleted_accounts_and_servers_left_and_publishes_readiness() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    add_server(
        &db,
        "dir-oauth",
        "http://127.0.0.1:9/mcp",
        true,
        false,
        "[]",
    );
    store_tokens(&db, A, "dir-oauth", "x", Some("y"), None, 5);
    store_tokens(&db, "deleted-account", "dir-oauth", "x", Some("y"), None, 5);
    store_tokens(&db, A, "dir-removed", "x", Some("y"), None, 5);
    // A sign-in whose key no longer opens it is "reauth", not absent.
    db.execute(
        "INSERT INTO mcp_oauth_tokens VALUES(?1,'dir-oauth',?2,1)",
        (
            B,
            seal("{\"accessToken\":\"x\"}", Some(B), 6).replace("enc:v2:", "enc:v2:AAAA"),
        ),
    )
    .unwrap();
    db.execute(
        "INSERT INTO directory_mcp_user_keys VALUES('deleted-account','dir-oauth','x',1)",
        [],
    )
    .unwrap();
    let f = front(d.path()).await;
    f._app.mcp.as_ref().unwrap().sweep_once(&f._app).await;
    assert!(token_row(&db, A, "dir-oauth").is_some());
    assert_eq!(token_row(&db, "deleted-account", "dir-oauth"), None);
    assert_eq!(token_row(&db, A, "dir-removed"), None);
    assert_eq!(key_row(&db, "deleted-account", "dir-oauth"), None);
    let state = state_file(d.path());
    assert_eq!(state["oauth"][A]["dir-oauth"], "connected");
    assert_eq!(state["oauth"][B]["dir-oauth"], "reauth");
    // Republished only when it changes: an unchanged state does not touch the file.
    let before = std::fs::metadata(d.path().join("mcp-credential-state.json")).unwrap();
    let mcp = f._app.mcp.as_ref().unwrap();
    let app = Arc::clone(&f._app);
    let m = Arc::clone(mcp);
    tokio::task::spawn_blocking(move || m.publish(&app))
        .await
        .unwrap();
    let after = std::fs::metadata(d.path().join("mcp-credential-state.json")).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        (before.ino(), before.mtime_nsec()),
        (after.ino(), after.mtime_nsec())
    );
    assert_eq!(after.mode() & 0o777, 0o600);
}

#[tokio::test]
async fn a_key_check_the_caller_abandons_is_still_stored() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    up.set(|r: &Seen| {
        let (_, text) = mcp_reply(r, None).unwrap_or_else(|| reply("404 Not Found", ""));
        let slow = r.body.contains("initialize");
        (
            if slow {
                Duration::from_millis(400)
            } else {
                Duration::ZERO
            },
            text,
        )
    });
    add_server(
        &db,
        "dir-keyed",
        &format!("{}/mcp", up.base()),
        false,
        true,
        DECLARED,
    );
    let f = front(d.path()).await;
    let body = r#"{"headers":{"X-Api-Key":"k"}}"#;
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    s.write_all(format!("PUT /api/mcp-keys/dir-keyed HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nCookie: {MEMBER}\r\nOrigin: http://{}\r\nX-CSRF-Token: csrf\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", f.addr, f.addr, body.len()).as_bytes()).await.unwrap();
    up.wait_for("/mcp", 1).await;
    drop(s);
    let stored = async {
        loop {
            if key_row(&db, B, "dir-keyed").is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), stored)
        .await
        .expect("the proven key was lost with the caller");
    assert_eq!(
        open(&key_row(&db, B, "dir-keyed").unwrap(), Some(B)),
        j(r#"{"X-Api-Key":"k"}"#)
    );
}

#[tokio::test]
async fn a_loopback_stand_in_is_a_qa_switch_and_never_reached_in_a_deployment() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let base = up.base();
    let counts = Arc::new(Counts::default());
    {
        let (base, counts) = (base.clone(), Arc::clone(&counts));
        up.set(move |r: &Seen| {
            mcp_reply(r, None)
                .or_else(|| oauth_reply(r, &base, &counts))
                .unwrap_or_else(|| reply("404 Not Found", ""))
        });
    }
    add_server(&db, "dir-oauth", &format!("{base}/mcp"), true, false, "[]");
    add_server(
        &db,
        "dir-keyed",
        &format!("{base}/mcp"),
        false,
        true,
        DECLARED,
    );
    // Without NOEVIA_QA_ALLOW_LOOPBACK_MCP the loopback is private like any other: not one request leaves.
    let f = front_with(d.path(), false).await;
    let a = f.addr;
    let (st, body) = write(a, "POST", "/api/mcp-oauth/dir-oauth/connect", MEMBER, "").await;
    assert_eq!(st, 502, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("did not describe itself"),
        "{body}"
    );
    let (st, body) = write(
        a,
        "PUT",
        "/api/mcp-keys/dir-keyed",
        MEMBER,
        r#"{"headers":{"X-Api-Key":"k"}}"#,
    )
    .await;
    assert_eq!(st, 422, "{body}");
    assert_eq!(
        body["error"],
        "The server did not accept that key: its address no longer resolves to a public host"
    );
    assert_eq!(key_row(&db, B, "dir-keyed"), None);
    assert!(
        up.seen().is_empty(),
        "the stand-in was never asked: {:?}",
        up.seen()
    );
}

#[tokio::test]
async fn oauth_metadata_is_small_by_contract() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let base = up.base();
    add_server(&db, "dir-oauth", &format!("{base}/mcp"), true, false, "[]");
    let f = front(d.path()).await;
    let a = f.addr;
    let pad = Arc::new(Mutex::new(0usize));
    {
        let (base, pad) = (base.clone(), Arc::clone(&pad));
        up.set(move |r: &Seen| match r.path.as_str() {
            "/.well-known/oauth-authorization-server" => reply(
                "200 OK",
                &format!(
                    r#"{{"pad":"{}","issuer":"{base}","authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","registration_endpoint":"{base}/register","code_challenge_methods_supported":["S256"]}}"#,
                    "x".repeat(*pad.lock().unwrap())
                ),
            ),
            "/register" => reply("201 Created", r#"{"client_id":"c"}"#),
            _ => reply("404 Not Found", ""),
        });
    }
    // Under the cap it signs in; over it the body is no metadata at all, and nothing is registered.
    *pad.lock().unwrap() = 100 * 1024;
    assert_eq!(
        write(a, "POST", "/api/mcp-oauth/dir-oauth/connect", MEMBER, "")
            .await
            .0,
        200
    );
    *pad.lock().unwrap() = 300 * 1024;
    db.execute("DELETE FROM mcp_oauth_clients", []).unwrap();
    let registered = up.count("/register");
    let (st, body) = write(a, "POST", "/api/mcp-oauth/dir-oauth/connect", MEMBER, "").await;
    assert_eq!(st, 502, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("did not describe itself"),
        "{body}"
    );
    assert_eq!(up.count("/register"), registered);
}

#[tokio::test]
async fn keys_sealed_before_the_move_are_read_and_a_copied_row_is_not() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    add_server(
        &db,
        "dir-keyed",
        "http://127.0.0.1:9/mcp",
        false,
        true,
        DECLARED,
    );
    // A key core sealed before user binding shipped (enc:v1) still opens; an account's bound row
    // copied under another account's id does not; and the readiness says each account has a key
    // (a row is a key: whether it opens is the call's answer).
    db.execute(
        "INSERT INTO directory_mcp_user_keys VALUES(?1,'dir-keyed',?2,1)",
        (B, seal(r#"{"X-Api-Key":"legacy"}"#, None, 1)),
    )
    .unwrap();
    db.execute(
        "INSERT INTO directory_mcp_user_keys VALUES(?1,'dir-keyed',?2,1)",
        (A, KEYS_GOLDEN),
    )
    .unwrap();
    let f = front(d.path()).await;
    let ask = |user: &str| format!(r#"{{"user":"{user}","server":"dir-keyed"}}"#);
    let (st, body) = store(f.store, "key", STORE_TOKEN, &ask(B)).await;
    assert_eq!(
        (st, body),
        (200, j(r#"{"headers":{"X-Api-Key":"legacy"}}"#))
    );
    let (st, body) = store(f.store, "key", STORE_TOKEN, &ask(A)).await;
    assert_eq!(
        (st, body["headers"]["X-Api-Key"].as_str()),
        (200, Some("synthetic-key")),
        "{body}"
    );
    // Copy A's bound row to B: it opens for no one but A.
    db.execute(
        "UPDATE directory_mcp_user_keys SET headers_enc=?1 WHERE user_id=?2",
        (KEYS_GOLDEN, B),
    )
    .unwrap();
    let (st, body) = store(f.store, "key", STORE_TOKEN, &ask(B)).await;
    assert_eq!(
        (st, body["code"].as_str()),
        (401, Some("reconnect")),
        "{body}"
    );
    assert_eq!(state_file(d.path())["keys"][B], j(r#"["dir-keyed"]"#));
    // A replacement by its owner re-seals it bound (enc:v2).
    let up = upstream().await;
    up.set(|r: &Seen| mcp_reply(r, None).unwrap_or_else(|| reply("404 Not Found", "")));
    db.execute(
        "UPDATE directory_mcp_servers SET url=?1",
        [format!("{}/mcp", up.base())],
    )
    .unwrap();
    let (st, body) = write(
        f.addr,
        "PUT",
        "/api/mcp-keys/dir-keyed",
        MEMBER,
        r#"{"headers":{"X-Api-Key":"fresh"}}"#,
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let stored = key_row(&db, B, "dir-keyed").unwrap();
    assert!(stored.starts_with("enc:v2:"));
    assert_eq!(open(&stored, Some(B)), j(r#"{"X-Api-Key":"fresh"}"#));
}

#[tokio::test]
async fn a_deployment_that_never_used_these_credentials_gets_no_file() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    add_server(
        &db,
        "dir-oauth",
        "http://127.0.0.1:9/mcp",
        true,
        true,
        DECLARED,
    );
    let f = front(d.path()).await;
    // Everything a signed-out front does: the sweep, the lists. Node reads a missing file as "nobody
    // is ready", the same as the empty state, so none is written until something is held.
    assert_eq!(get(f.addr, "/api/mcp-oauth/servers", MEMBER).await.0, 200);
    assert_eq!(get(f.addr, "/api/mcp-keys/servers", MEMBER).await.0, 200);
    assert!(!d.path().join("mcp-credential-state.json").exists());
    // Holding one makes it; letting it go leaves the file, empty (a signal that it changed).
    store_tokens(&db, B, "dir-oauth", "x", Some("y"), None, 5);
    assert_eq!(
        write(f.addr, "DELETE", "/api/mcp-oauth/dir-other", MEMBER, "")
            .await
            .0,
        200
    );
    assert_eq!(state_file(d.path())["oauth"][B]["dir-oauth"], "connected");
    assert_eq!(
        write(f.addr, "DELETE", "/api/mcp-oauth/dir-oauth", MEMBER, "")
            .await
            .0,
        200
    );
    assert_eq!(state_file(d.path())["oauth"], j("{}"));
}
