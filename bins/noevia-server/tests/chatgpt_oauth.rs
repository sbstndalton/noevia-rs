//! Sign in with ChatGPT's token broker (sbstndalton/noevia#2190 round 5) through the real front and
//! the real internal store listener. OpenAI is a synthetic http server on 127.0.0.1 (the loopback
//! QA override NOEVIA_QA_CHATGPT_ISSUER / NOEVIA_QA_CHATGPT_CODEX_BASE); the legacy upstream is
//! dead, so a request that fell through to Node would fail. No real account, token or credential.
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
const DEVICE: &str = "/api/providers/chatgpt/device";
const POLL: &str = "/api/providers/chatgpt/device/poll";
const CANCEL: &str = "/api/providers/chatgpt/device/cancel";
const MODELS: &str = "/api/providers/chatgpt/models";

/// Golden vector shared with core (server/rust-chatgpt-oauth.test.cjs): the stored token JSON in
/// core's key order, sealed `enc:v2` for account A with key [7;32] and nonce 0x01..0x0c, computed
/// with Node's own `crypto.createCipheriv('aes-256-gcm')` (iv | tag | body, AAD noevia:user:<id>).
const TOKENS_JSON: &str = r#"{"accessToken":"synthetic-access","refreshToken":"synthetic-refresh","accountId":"acct-synthetic","fedRamp":false,"expiresAt":1800000000000,"email":"ada@example.test","plan":"plus","connectedAt":1700000000000}"#;
const TOKENS_GOLDEN: &str = "enc:v2:AQIDBAUGBwgJCgsMvIx2UeZ0MdLawavEpU9ZGcBeif0Jj5FfsVFpe_uFmWKnnD9E2i5Gjh2M7fzPPTVtHCAWKmJhuTAK3BzxETwpBOoZEbPdEMh5qMaTYfmN8kPbxfh7anMdAhRASFTL2COT7vI4WbMjgWVi91-vKf2-CI2YXjrKc4XpaNiVDbQP1qC4ptUwi4Y9a9mka1rUefilYdzGw2i0rQd6eTq-jREEkAFE4B8Zt7oEAyrKvttFdGOLRDzHYtS7izHOkalFMILFX0426a7T8VAf_AgRZYrOKl_yE27UT6vK5yR8GIlqqZGn";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn j(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|_| panic!("not JSON: {s}"))
}

fn jwt(claims: &str) -> String {
    format!(
        "h.{}.s",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims)
    )
}

async fn raw(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    extra: &str,
    body: &[u8],
) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n", body.len()).as_bytes()).await.unwrap();
    let _ = s.write_all(body).await;
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(40), s.read_to_end(&mut bytes)).await;
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    let status = h.split_whitespace().nth(1).unwrap().parse().unwrap();
    (
        status,
        serde_json::from_str(b).unwrap_or(serde_json::Value::Null),
    )
}

async fn post(
    addr: std::net::SocketAddr,
    path: &str,
    cookie: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    let csrf = format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n");
    raw(addr, "POST", path, cookie, &csrf, body.as_bytes()).await
}

async fn get(addr: std::net::SocketAddr, path: &str, cookie: &str) -> (u16, serde_json::Value) {
    raw(addr, "GET", path, cookie, "", b"").await
}

/// One internal store call (`/v1/chatgpt/<op>`).
async fn store(
    addr: std::net::SocketAddr,
    op: &str,
    token: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("POST /v1/chatgpt/{op} HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{}: {token}\r\n\r\n{body}", body.len(), project_store::TOKEN_HEADER).as_bytes()).await.unwrap();
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
// A synthetic OpenAI.

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    headers: String,
    body: String,
}

type Handler = Arc<dyn Fn(&Seen) -> (Duration, String) + Send + Sync>;

#[derive(Clone)]
struct Upstream {
    addr: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    handler: Arc<Mutex<Handler>>,
    /// Signalled whenever a request arrives: what the tests wait on instead of sleeping.
    hit: Arc<tokio::sync::Notify>,
}

impl Upstream {
    /// Waits until the upstream has received `n` requests whose path starts with `path`: that
    /// exchange is then in flight (a delayed handler answers only after its delay).
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
}

fn reply(status: &str, body: &str) -> (Duration, String) {
    (
        Duration::ZERO,
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

// ---------------------------------------------------------------------------------------------
// The front, its store listener and data directory.

struct Front {
    addr: std::net::SocketAddr,
    store: std::net::SocketAddr,
}

fn seed(dir: &std::path::Path) -> Connection {
    std::fs::write(dir.join("secrets.key"), KEY).unwrap();
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    // core chatgpt-oauth.cjs's own DDL (Node stays the migrator).
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY, data_enc TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'connected', updated_at INTEGER NOT NULL)",
    )
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, t) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(t), uid, hex("csrf"), now, now + 86400000),
        )
        .unwrap();
        // Core's workspace load makes the account's directory on its first request.
        std::fs::create_dir_all(dir.join("users").join(uid)).unwrap();
    }
    db
}

/// The rows of `users/<user>/providers.json`.
fn provider_rows(dir: &std::path::Path, user: &str) -> Vec<serde_json::Value> {
    match std::fs::read_to_string(dir.join("users").join(user).join("providers.json")) {
        Ok(t) => j(&t)["providers"].as_array().cloned().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

async fn front(dir: &std::path::Path, issuer: &str, flag: &str) -> Front {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        ("NOEVIA_FEATURE_CHATGPT_OAUTH", flag.to_string()),
        ("NOEVIA_QA_CHATGPT_ISSUER", issuer.to_string()),
        (
            "NOEVIA_QA_CHATGPT_CODEX_BASE",
            format!("{issuer}/backend-api/codex"),
        ),
    ]);
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
        Some(app),
        std::future::pending(),
    ));
    Front {
        addr,
        store: store_addr,
    }
}

fn open(value: &str, user: &str) -> serde_json::Value {
    let units: Vec<u16> = value.encode_utf16().collect();
    let (_, plain) = secret_envelope::open(&KEY, None, &units, Some(user.as_bytes())).unwrap();
    j(std::str::from_utf8(&plain).unwrap())
}

fn seal(plain: &str, user: &str, n: u8) -> String {
    secret_envelope::seal(&KEY, &[n; 12], plain.as_bytes(), Some(user.as_bytes())).unwrap()
}

fn token_row(db: &Connection, user: &str) -> Option<(String, String)> {
    db.query_row(
        "SELECT data_enc,state FROM chatgpt_oauth_tokens WHERE user_id=?1",
        [user],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

fn audits(db: &Connection, user: &str) -> Vec<String> {
    let mut st = db
        .prepare("SELECT action FROM audit_events WHERE target_user_id=?1 AND action LIKE 'chatgpt.%' ORDER BY id")
        .unwrap();
    st.query_map([user], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// A stored connected row for `user`, expiring at `expires`.
fn store_tokens(db: &Connection, user: &str, access: &str, refresh: &str, expires: i64, n: u8) {
    let json = format!(
        r#"{{"accessToken":"{access}","refreshToken":"{refresh}","accountId":"acct-synthetic","fedRamp":true,"expiresAt":{expires},"email":"ada@example.test","plan":"plus","connectedAt":1700000000000}}"#
    );
    db.execute(
        "INSERT OR REPLACE INTO chatgpt_oauth_tokens(user_id,data_enc,state,updated_at) VALUES(?1,?2,'connected',1)",
        (user, seal(&json, user, n)),
    )
    .unwrap();
}

#[test]
fn golden_vector_matches_node_sealing() {
    let keys = server_auth::secrets::SecretKeys::from_existing_bytes(
        zeroize::Zeroizing::new(KEY.to_vec()),
        None,
    )
    .unwrap();
    let nonce: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    assert_eq!(
        keys.seal(TOKENS_JSON, Some(A), &nonce).unwrap(),
        TOKENS_GOLDEN
    );
    let (plain, _) = keys.open(TOKENS_GOLDEN, Some(A)).unwrap();
    assert_eq!(plain.as_str(), TOKENS_JSON);
    assert!(
        keys.open(TOKENS_GOLDEN, Some(B)).is_err(),
        "bound to its owner"
    );
}

#[tokio::test]
async fn device_sign_in_through_the_front() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let issuer = up.base();
    let f = front(d.path(), &issuer, "1").await;
    let addr = f.addr;
    let id_token = jwt(
        r#"{"email":"ada@example.test","https://api.openai.com/auth":{"chatgpt_account_id":"acct-synthetic","chatgpt_plan_type":"plus"}}"#,
    );
    let granted = Arc::new(AtomicUsize::new(0));
    {
        let (granted, id_token) = (Arc::clone(&granted), id_token.clone());
        up.set(move |r: &Seen| match r.path.as_str() {
            "/api/accounts/deviceauth/usercode" => reply(
                "200 OK",
                r#"{"device_auth_id":"synthetic-device","user_code":"ABCD-EFGH","interval":"1"}"#,
            ),
            "/api/accounts/deviceauth/token" if granted.load(Ordering::SeqCst) == 0 => {
                reply("403 Forbidden", r#"{"error":"authorization_pending"}"#)
            }
            "/api/accounts/deviceauth/token" => reply(
                "200 OK",
                r#"{"authorization_code":"synthetic-code","code_verifier":"synthetic-verifier"}"#,
            ),
            "/oauth/token" => reply(
                "200 OK",
                &format!(
                    r#"{{"access_token":"synthetic-access-1","refresh_token":"synthetic-refresh-1","id_token":"{id_token}","expires_in":3600}}"#
                ),
            ),
            _ => reply("404 Not Found", ""),
        });
    }

    // The gate, then the flag.
    assert_eq!(raw(addr, "POST", DEVICE, "", "", b"{}").await.0, 401);
    assert_eq!(raw(addr, "POST", DEVICE, ADMIN, "", b"{}").await.0, 403);
    assert_eq!(get(addr, MODELS, "").await.0, 401);
    assert!(up.seen().is_empty(), "nothing outbound before the gate");

    // Start.
    let (s, started) = post(addr, DEVICE, ADMIN, "").await;
    assert_eq!(s, 200, "{started}");
    let login = started["loginId"].as_str().unwrap().to_string();
    assert_eq!(login.len(), 24);
    assert_eq!(started["userCode"], "ABCD-EFGH");
    assert_eq!(started["verificationUrl"], format!("{issuer}/codex/device"));
    assert_eq!(started["interval"], 1);
    let start_req = &up.seen()[0];
    assert_eq!(start_req.method, "POST");
    assert_eq!(
        start_req.body,
        r#"{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}"#
    );
    assert!(
        start_req.headers.contains("user-agent: noevia/"),
        "{}",
        start_req.headers
    );
    assert!(start_req.headers.contains("content-type: application/json"));

    // Too early: pending without a request; another account: 403; unknown: expired.
    let body = format!(r#"{{"loginId":"{login}"}}"#);
    assert_eq!(
        post(addr, POLL, ADMIN, &body).await,
        (200, j(r#"{"state":"pending","interval":1}"#))
    );
    assert_eq!(
        post(addr, POLL, MEMBER, &body).await,
        (
            403,
            j(r#"{"error":"This sign-in was started by a different account."}"#)
        )
    );
    assert_eq!(
        post(addr, POLL, ADMIN, r#"{"loginId":"nope"}"#).await,
        (200, j(r#"{"state":"expired"}"#))
    );
    assert_eq!(
        post(addr, POLL, ADMIN, "not json").await,
        (200, j(r#"{"state":"expired"}"#))
    );
    assert_eq!(up.count("/api/accounts/deviceauth/token"), 0);

    // Authorization pending upstream: still pending.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(
        post(addr, POLL, ADMIN, &body).await,
        (200, j(r#"{"state":"pending","interval":1}"#))
    );
    assert_eq!(up.count("/api/accounts/deviceauth/token"), 1);

    // Granted: the exchange (a form), the save, the answer.
    granted.store(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let (s, done) = post(addr, POLL, ADMIN, &body).await;
    assert_eq!(
        (s, done),
        (
            200,
            j(r#"{"state":"connected","account":{"email":"a…@example.test","plan":"plus"}}"#)
        )
    );
    let exchange = up
        .seen()
        .into_iter()
        .find(|r| r.path == "/oauth/token")
        .unwrap();
    assert!(exchange
        .headers
        .contains("content-type: application/x-www-form-urlencoded"));
    assert!(exchange.body.contains("grant_type=authorization_code"));
    assert!(exchange.body.contains("code_verifier=synthetic-verifier"));
    assert!(exchange.body.contains(&format!(
        "redirect_uri={}",
        url_enc(&format!("{issuer}/deviceauth/callback"))
    )));
    let (enc, state) = token_row(&db, A).unwrap();
    assert_eq!(state, "connected");
    assert!(enc.starts_with("enc:v2:"));
    let stored = open(&enc, A);
    assert_eq!(stored["accessToken"], "synthetic-access-1");
    assert_eq!(stored["refreshToken"], "synthetic-refresh-1");
    assert_eq!(stored["accountId"], "acct-synthetic");
    assert!(stored["connectedAt"].is_number());
    let keys: Vec<&String> = stored.as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 8);
    assert_eq!(
        audits(&db, A),
        ["chatgpt.oauth.start", "chatgpt.oauth.connect"]
    );
    // Round 6: the account's ChatGPT provider row is written in the same step as the tokens, by
    // the front (core's row, no credential in it).
    assert_eq!(
        provider_rows(d.path(), A),
        [j(
            r#"{"id":"chatgpt-oauth","kind":"chatgpt-oauth","label":"ChatGPT","baseUrl":"https://chatgpt.com/backend-api/codex","apiKey":"","defaultModel":"","shared":false,"external":true}"#
        )]
    );
    assert!(provider_rows(d.path(), B).is_empty());
    // #1838: the connect audit row carries only the short plan identifier.
    let detail: String = db
        .query_row(
            "SELECT detail FROM audit_events WHERE action='chatgpt.oauth.connect'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(detail, r#"{"plan":"plus"}"#);
    assert!(
        d.path().join("chatgpt-token-changes.json").exists(),
        "Node's change signal"
    );
    // The finished sign-in is gone.
    assert_eq!(
        post(addr, POLL, ADMIN, &body).await,
        (200, j(r#"{"state":"expired"}"#))
    );

    // Cancel: only the owner; a cancelled sign-in is gone.
    let (_, second) = post(addr, DEVICE, ADMIN, "{}").await;
    let body2 = format!(r#"{{"loginId":"{}"}}"#, second["loginId"].as_str().unwrap());
    assert_eq!(
        post(addr, CANCEL, MEMBER, &body2).await,
        (
            403,
            j(r#"{"error":"This sign-in was started by a different account."}"#)
        )
    );
    assert_eq!(
        post(addr, CANCEL, ADMIN, &body2).await,
        (200, j(r#"{"ok":true}"#))
    );
    assert_eq!(
        post(addr, CANCEL, ADMIN, &body2).await,
        (200, j(r#"{"ok":false}"#))
    );
    assert_eq!(
        post(addr, POLL, ADMIN, &body2).await,
        (200, j(r#"{"state":"expired"}"#))
    );

    // Five starts per ten minutes per account (the member's first five, then 429).
    for _ in 0..5 {
        assert_eq!(post(addr, DEVICE, MEMBER, "{}").await.0, 200);
    }
    assert_eq!(
        post(addr, DEVICE, MEMBER, "{}").await,
        (
            429,
            j(
                r#"{"error":"Too many ChatGPT sign-in attempts. Wait a few minutes and try again."}"#
            )
        )
    );

    // Upstream failures, in core's words.
    up.set(|r: &Seen| match r.path.as_str() {
        "/api/accounts/deviceauth/usercode" => reply("404 Not Found", ""),
        _ => reply("500 Oops", ""),
    });
    assert_eq!(
        post(addr, DEVICE, ADMIN, "{}").await,
        (
            502,
            j(r#"{"error":"ChatGPT device sign-in is not available right now."}"#)
        )
    );
    up.set(|_: &Seen| reply("200 OK", r#"{"device_auth_id":"x"}"#));
    assert_eq!(
        post(addr, DEVICE, ADMIN, "{}").await,
        (
            502,
            j(r#"{"error":"ChatGPT sign-in could not start (200)."}"#)
        )
    );
    up.set(|_: &Seen| {
        (
            Duration::ZERO,
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\n\r\n"
                .into(),
        )
    });
    assert_eq!(
        post(addr, DEVICE, ADMIN, "{}").await,
        (
            502,
            j(r#"{"error":"ChatGPT sign-in could not be reached. Try again."}"#)
        ),
        "a redirect is not followed"
    );
    drop(db);
}

fn url_enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[tokio::test]
async fn flag_off_is_node_404_before_anything() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let up = upstream().await;
    let f = front(d.path(), &up.base(), "0").await;
    let off = j(r#"{"error":"Sign in with ChatGPT is turned off on this server."}"#);
    assert_eq!(post(f.addr, DEVICE, ADMIN, "{}").await, (404, off.clone()));
    assert_eq!(post(f.addr, POLL, ADMIN, "{}").await, (404, off.clone()));
    assert_eq!(get(f.addr, MODELS, ADMIN).await, (404, off));
    assert!(up.seen().is_empty());
}

#[tokio::test]
async fn broker_hands_out_tokens_and_refreshes_once() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let f = front(d.path(), &up.base(), "1").await;
    let now = noevia_server::identity::now_ms();
    let user = format!(r#"{{"user":"{A}"}}"#);

    // The store secret first.
    assert_eq!(
        store(
            f.store,
            "session",
            "wrong-token-wrong-token-wrong-token",
            &user
        )
        .await
        .0,
        401
    );
    // No row: disconnected.
    assert_eq!(
        store(f.store, "session", STORE_TOKEN, &user).await,
        (
            401,
            j(
                r#"{"error":"ChatGPT is not connected. Sign in with ChatGPT in Settings → AI providers.","code":"disconnected","status":401}"#
            )
        )
    );
    assert_eq!(
        store(f.store, "connected", STORE_TOKEN, &user).await,
        (200, j(r#"{"exists":false}"#))
    );
    // Forgetting an account that never signed in changes nothing, so Node's signal stays put.
    assert_eq!(
        store(
            f.store,
            "forget",
            STORE_TOKEN,
            &format!(r#"{{"user":"{B}"}}"#)
        )
        .await,
        (200, j(r#"{"ok":true}"#))
    );
    assert!(!d.path().join("chatgpt-token-changes.json").exists());

    // A row that is not near expiry: handed out as stored, no refresh token in the answer.
    store_tokens(
        &db,
        A,
        "synthetic-access-0",
        "synthetic-refresh-0",
        now + 3_600_000,
        1,
    );
    let (s, got) = store(f.store, "session", STORE_TOKEN, &user).await;
    assert_eq!(s, 200, "{got}");
    assert_eq!(got["accessToken"], "synthetic-access-0");
    assert_eq!(got["accountId"], "acct-synthetic");
    assert_eq!(got["fedRamp"], true);
    assert!(got.get("refreshToken").is_none() && !got.to_string().contains("synthetic-refresh"));
    assert_eq!(up.count("/oauth/token"), 0);
    assert_eq!(
        store(f.store, "connected", STORE_TOKEN, &user).await,
        (200, j(r#"{"exists":true}"#))
    );

    // Inside the five-minute margin: one refresh for many concurrent callers (single flight).
    store_tokens(
        &db,
        A,
        "synthetic-access-0",
        "synthetic-refresh-0",
        now + 60_000,
        2,
    );
    up.set(|r: &Seen| match r.path.as_str() {
        "/oauth/token" => (
            Duration::from_millis(300),
            reply(
                "200 OK",
                r#"{"access_token":"synthetic-access-1","refresh_token":"synthetic-refresh-1","expires_in":3600}"#,
            )
            .1,
        ),
        _ => reply("404 Not Found", ""),
    });
    let calls: Vec<_> = (0..6)
        .map(|_| {
            let (addr, user) = (f.store, user.clone());
            tokio::spawn(async move { store(addr, "session", STORE_TOKEN, &user).await })
        })
        .collect();
    for c in calls {
        let (s, got) = c.await.unwrap();
        assert_eq!(s, 200, "{got}");
        assert_eq!(got["accessToken"], "synthetic-access-1");
    }
    assert_eq!(
        up.count("/oauth/token"),
        1,
        "one refresh, the rotated token spent once"
    );
    let refresh = up
        .seen()
        .into_iter()
        .find(|r| r.path == "/oauth/token")
        .unwrap();
    assert_eq!(
        j(&refresh.body),
        j(
            r#"{"grant_type":"refresh_token","refresh_token":"synthetic-refresh-0","client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}"#
        )
    );
    let stored = open(&token_row(&db, A).unwrap().0, A);
    assert_eq!(stored["refreshToken"], "synthetic-refresh-1");
    assert_eq!(
        stored["accountId"], "acct-synthetic",
        "kept from the previous sign-in"
    );
    assert_eq!(stored["connectedAt"], 1_700_000_000_000_i64);

    // A rejected token that is no longer stored: the stored one, no refresh.
    let (_, got) = store(
        f.store,
        "session",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}","rejected":"synthetic-access-0"}}"#),
    )
    .await;
    assert_eq!(got["accessToken"], "synthetic-access-1");
    assert_eq!(up.count("/oauth/token"), 1);

    // A transient refresh failure inside the margin keeps the still-valid token.
    store_tokens(
        &db,
        A,
        "synthetic-access-2",
        "synthetic-refresh-2",
        now + 60_000,
        3,
    );
    up.set(|_: &Seen| reply("500 Oops", ""));
    let (s, got) = store(f.store, "session", STORE_TOKEN, &user).await;
    assert_eq!(
        (s, got["accessToken"].clone()),
        (200, j(r#""synthetic-access-2""#))
    );
    // ... but the rejected token is refreshed and a 5xx is transient (503).
    let (s, got) = store(
        f.store,
        "session",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}","rejected":"synthetic-access-2"}}"#),
    )
    .await;
    assert_eq!((s, got["code"].clone()), (503, j(r#""transient""#)));

    // A refused refresh: reconnect needed, recorded once.
    up.set(|_: &Seen| reply("401 Unauthorized", r#"{"error":"invalid_grant"}"#));
    let (s, got) = store(
        f.store,
        "session",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}","rejected":"synthetic-access-2"}}"#),
    )
    .await;
    assert_eq!((s, got["code"].clone()), (401, j(r#""reconnect""#)));
    assert_eq!(token_row(&db, A).unwrap().1, "reconnect");
    assert_eq!(audits(&db, A), ["chatgpt.oauth.reconnect_needed"]);
    let (s, got) = store(f.store, "session", STORE_TOKEN, &user).await;
    assert_eq!((s, got["code"].clone()), (401, j(r#""reconnect""#)));

    // The second 401: only the generation that failed is marked.
    store_tokens(
        &db,
        A,
        "synthetic-access-3",
        "synthetic-refresh-3",
        now + 3_600_000,
        4,
    );
    let (_, got) = store(f.store, "session", STORE_TOKEN, &user).await;
    let generation = got["generation"].as_str().unwrap().to_string();
    store_tokens(
        &db,
        A,
        "synthetic-access-4",
        "synthetic-refresh-4",
        now + 3_600_000,
        5,
    );
    assert_eq!(
        store(
            f.store,
            "reject",
            STORE_TOKEN,
            &format!(r#"{{"user":"{A}","generation":"{generation}"}}"#)
        )
        .await,
        (200, j(r#"{"marked":false}"#)),
        "a newer sign-in is left alone"
    );
    let (_, got) = store(f.store, "session", STORE_TOKEN, &user).await;
    let generation = got["generation"].as_str().unwrap().to_string();
    assert_eq!(
        store(
            f.store,
            "reject",
            STORE_TOKEN,
            &format!(r#"{{"user":"{A}","generation":"{generation}"}}"#)
        )
        .await,
        (200, j(r#"{"marked":true}"#))
    );
    assert_eq!(token_row(&db, A).unwrap().1, "reconnect");

    // A row another account's key cannot open is reconnect, never another account's tokens.
    db.execute(
        "INSERT OR REPLACE INTO chatgpt_oauth_tokens(user_id,data_enc,state,updated_at) VALUES(?1,?2,'connected',1)",
        (B, seal(r#"{"accessToken":"x","accountId":"y"}"#, A, 9)),
    )
    .unwrap();
    let (s, got) = store(
        f.store,
        "session",
        STORE_TOKEN,
        &format!(r#"{{"user":"{B}"}}"#),
    )
    .await;
    assert_eq!((s, got["code"].clone()), (401, j(r#""reconnect""#)));

    // Disconnect and forget delete the row.
    assert_eq!(
        store(f.store, "disconnect", STORE_TOKEN, &user).await,
        (200, j(r#"{"ok":true}"#))
    );
    assert!(token_row(&db, A).is_none());
    assert_eq!(
        store(
            f.store,
            "forget",
            STORE_TOKEN,
            &format!(r#"{{"user":"{B}"}}"#)
        )
        .await,
        (200, j(r#"{"ok":true}"#))
    );
    assert!(token_row(&db, B).is_none());
    assert_eq!(store(f.store, "nope", STORE_TOKEN, &user).await.0, 404);
    assert_eq!(store(f.store, "session", STORE_TOKEN, "{}").await.0, 400);
}

#[tokio::test]
async fn catalogue_is_cached_throttled_and_shared() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let f = front(d.path(), &up.base(), "1").await;
    let now = noevia_server::identity::now_ms();
    store_tokens(
        &db,
        A,
        "synthetic-access-0",
        "synthetic-refresh-0",
        now + 3_600_000,
        1,
    );
    let catalogue = r#"{"models":[{"slug":"synthetic-model","default_reasoning_level":"medium","default_reasoning_summary":"auto","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"},{"effort":"Bad!"}]},{"slug":"hidden","visibility":"hide"},{"slug":"no-api","supported_in_api":false},{"slug":"second","supports_reasoning_summary_parameter":false}]}"#;
    up.set(move |r: &Seen| match r.path.as_str() {
        p if p.starts_with("/backend-api/codex/models") => {
            (Duration::from_millis(200), reply("200 OK", catalogue).1)
        }
        _ => reply("404 Not Found", ""),
    });
    // Concurrent first reads share one fetch.
    let calls: Vec<_> = (0..4)
        .map(|_| {
            let addr = f.addr;
            tokio::spawn(async move { get(addr, MODELS, ADMIN).await })
        })
        .collect();
    for c in calls {
        let (s, got) = c.await.unwrap();
        assert_eq!(s, 200, "{got}");
        assert_eq!(got["models"], j(r#"["synthetic-model","second"]"#));
    }
    assert_eq!(
        up.count("/backend-api/codex/models"),
        1,
        "one fetch in flight per account"
    );
    let fetch = up
        .seen()
        .into_iter()
        .find(|r| r.path.starts_with("/backend-api"))
        .unwrap();
    assert_eq!(
        fetch.path,
        "/backend-api/codex/models?client_version=0.155.0"
    );
    assert!(fetch
        .headers
        .contains("authorization: bearer synthetic-access-0"));
    assert!(fetch.headers.contains("chatgpt-account-id: acct-synthetic"));
    assert!(fetch.headers.contains("x-openai-fedramp: true"));
    // Cached; a refresh fetches; a second refresh within ten seconds is throttled.
    let (_, got) = get(f.addr, MODELS, ADMIN).await;
    assert_eq!(got["cached"], true);
    let (_, got) = get(f.addr, &format!("{MODELS}?refresh=1"), ADMIN).await;
    assert_eq!(
        (got["cached"].clone(), up.count("/backend-api/codex/models")),
        (j("false"), 2)
    );
    let (_, got) = get(f.addr, &format!("{MODELS}?refresh=1"), ADMIN).await;
    assert_eq!(got["throttled"], true);
    assert_eq!(got["cached"], true);
    assert_eq!(up.count("/backend-api/codex/models"), 2);
    // The chat's view: the catalogue entries with their reasoning levels.
    let (s, got) = store(
        f.store,
        "catalogue",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}"}}"#),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(
        got["models"],
        j(
            r#"[{"slug":"synthetic-model","effort":"medium","summary":"auto","summaryParam":true,"levels":["low","high"]},{"slug":"second","effort":null,"summary":null,"summaryParam":false,"levels":null}]"#
        )
    );
    // The credential channel has no `models` operation (the picker's list is the public route).
    let (s, _) = store(
        f.store,
        "models",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}","refresh":false}}"#),
    )
    .await;
    assert_eq!(s, 404);

    // An account with an upstream failure: Node's message, remembered for a minute.
    store_tokens(
        &db,
        B,
        "synthetic-access-b",
        "synthetic-refresh-b",
        now + 3_600_000,
        2,
    );
    up.set(|_: &Seen| {
        reply(
            "429 Too Many Requests",
            r#"{"error":{"message":"slow down"}}"#,
        )
    });
    let failed =
        j(r#"{"error":"ChatGPT is rate-limiting this account: slow down. Try again shortly."}"#);
    assert_eq!(get(f.addr, MODELS, MEMBER).await, (502, failed.clone()));
    assert_eq!(get(f.addr, MODELS, MEMBER).await, (502, failed));
    assert_eq!(
        up.count("/backend-api/codex/models"),
        3,
        "the failure is cached"
    );
    // Disconnected: Node's 401.
    assert_eq!(
        store(
            f.store,
            "disconnect",
            STORE_TOKEN,
            &format!(r#"{{"user":"{A}"}}"#)
        )
        .await
        .0,
        200
    );
    assert_eq!(
        get(f.addr, MODELS, ADMIN).await,
        (
            401,
            j(
                r#"{"error":"ChatGPT is not connected. Sign in with ChatGPT in Settings → AI providers."}"#
            )
        )
    );
}

/// A device sign-in for `cookie` up to the moment its poll is due (interval 1 s).
async fn started(addr: std::net::SocketAddr, cookie: &str) -> String {
    let (s, b) = post(addr, DEVICE, cookie, "{}").await;
    assert_eq!(s, 200, "{b}");
    format!(r#"{{"loginId":"{}"}}"#, b["loginId"].as_str().unwrap())
}

#[tokio::test]
async fn races_are_won_by_the_disconnect_and_the_cancel() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let f = front(d.path(), &up.base(), "1").await;
    let now = noevia_server::identity::now_ms();
    let user = format!(r#"{{"user":"{A}"}}"#);
    let id_token =
        jwt(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-synthetic"}}"#);
    up.set(move |r: &Seen| match r.path.as_str() {
        "/api/accounts/deviceauth/usercode" => reply(
            "200 OK",
            r#"{"device_auth_id":"synthetic-device","user_code":"ABCD-EFGH","interval":1}"#,
        ),
        "/api/accounts/deviceauth/token" => reply(
            "200 OK",
            r#"{"authorization_code":"synthetic-code","code_verifier":"synthetic-verifier"}"#,
        ),
        "/oauth/token" => (
            Duration::from_millis(700),
            reply(
                "200 OK",
                &format!(r#"{{"access_token":"synthetic-access-9","refresh_token":"synthetic-refresh-9","id_token":"{id_token}","expires_in":3600}}"#),
            )
            .1,
        ),
        _ => reply("404 Not Found", ""),
    });

    // A disconnect during an in-flight refresh wins at once; the refreshed tokens are not written.
    store_tokens(
        &db,
        A,
        "synthetic-access-0",
        "synthetic-refresh-0",
        now + 60_000,
        1,
    );
    let (addr, u) = (f.store, user.clone());
    let refreshing = tokio::spawn(async move { store(addr, "session", STORE_TOKEN, &u).await });
    up.wait_for("/oauth/token", 1).await;
    let t0 = std::time::Instant::now();
    assert_eq!(
        store(f.store, "disconnect", STORE_TOKEN, &user).await.0,
        200
    );
    assert!(
        t0.elapsed() < Duration::from_millis(400),
        "the disconnect does not wait for the refresh"
    );
    let (s, got) = refreshing.await.unwrap();
    assert_eq!((s, got["code"].clone()), (401, j(r#""disconnected""#)));
    assert!(
        token_row(&db, A).is_none(),
        "the refresh found no row to write"
    );

    // #454: a disconnect while the sign-in is being exchanged wins; nothing is stored.
    let body = started(f.addr, ADMIN).await;
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let (addr, b) = (f.addr, body.clone());
    let polling = tokio::spawn(async move { post(addr, POLL, ADMIN, &b).await });
    up.wait_for("/oauth/token", 2).await;
    assert_eq!(
        store(f.store, "disconnect", STORE_TOKEN, &user).await.0,
        200
    );
    assert_eq!(polling.await.unwrap(), (200, j(r#"{"state":"cancelled"}"#)));
    assert!(token_row(&db, A).is_none());
    assert!(
        provider_rows(d.path(), A).is_empty(),
        "no provider row for a sign-in the disconnect won"
    );
    // ... and so does a cancel.
    let body = started(f.addr, ADMIN).await;
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let (addr, b) = (f.addr, body.clone());
    let polling = tokio::spawn(async move { post(addr, POLL, ADMIN, &b).await });
    up.wait_for("/oauth/token", 3).await;
    assert_eq!(
        post(f.addr, CANCEL, ADMIN, &body).await,
        (200, j(r#"{"ok":true}"#))
    );
    assert_eq!(polling.await.unwrap(), (200, j(r#"{"state":"cancelled"}"#)));
    assert!(token_row(&db, A).is_none());
    assert_eq!(
        audits(&db, A)
            .iter()
            .filter(|a| *a == "chatgpt.oauth.cancelled")
            .count(),
        2
    );

    // At most three sign-ins pending per account: a fourth start drops the oldest.
    let first = started(f.addr, MEMBER).await;
    let second = started(f.addr, MEMBER).await;
    started(f.addr, MEMBER).await;
    started(f.addr, MEMBER).await;
    assert_eq!(
        post(f.addr, POLL, MEMBER, &first).await,
        (200, j(r#"{"state":"expired"}"#))
    );
    assert_eq!(
        post(f.addr, POLL, MEMBER, &second).await.1["state"],
        "pending"
    );
}

#[tokio::test]
async fn catalogue_failures_recover_and_never_replace_a_good_list() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let f = front(d.path(), &up.base(), "1").await;
    let now = noevia_server::identity::now_ms();
    store_tokens(
        &db,
        A,
        "synthetic-access-0",
        "synthetic-refresh-0",
        now + 3_600_000,
        1,
    );
    let good = r#"{"models":[{"slug":"synthetic-model"}]}"#;
    let down = Arc::new(AtomicUsize::new(1));
    {
        let down = Arc::clone(&down);
        up.set(move |_: &Seen| {
            if down.load(Ordering::SeqCst) == 1 {
                reply("503 Service Unavailable", r#"{"detail":"down"}"#)
            } else {
                reply("200 OK", good)
            }
        });
    }
    let failed = j(r#"{"error":"ChatGPT returned 503: down"}"#);
    let calls = || up.count("/backend-api/codex/models");
    // A failure is remembered for plain reads...
    assert_eq!(get(f.addr, MODELS, ADMIN).await, (502, failed.clone()));
    down.store(0, Ordering::SeqCst);
    assert_eq!(get(f.addr, MODELS, ADMIN).await, (502, failed.clone()));
    assert_eq!(calls(), 1);
    // ... and a refresh recovers from it; the failure was never reported as cached data.
    let (s, got) = get(f.addr, &format!("{MODELS}?refresh=1"), ADMIN).await;
    assert_eq!(
        (s, got["models"].clone(), got["cached"].clone()),
        (200, j(r#"["synthetic-model"]"#), j("false"))
    );
    assert_eq!(calls(), 2);

    // A failed refresh keeps the last good list for plain reads; only the refresh sees the error.
    store_tokens(
        &db,
        B,
        "synthetic-access-b",
        "synthetic-refresh-b",
        now + 3_600_000,
        2,
    );
    assert_eq!(get(f.addr, MODELS, MEMBER).await.0, 200);
    down.store(1, Ordering::SeqCst);
    assert_eq!(
        get(f.addr, &format!("{MODELS}?refresh=1"), MEMBER).await,
        (502, failed)
    );
    let (s, got) = get(f.addr, MODELS, MEMBER).await;
    assert_eq!((s, got["cached"].clone()), (200, j("true")));
    // Failing refreshes are throttled too: the next one inside ten seconds sends nothing.
    let before = calls();
    let (s, got) = get(f.addr, &format!("{MODELS}?refresh=1"), MEMBER).await;
    assert_eq!((s, got["throttled"].clone()), (200, j("true")));
    assert_eq!(calls(), before);

    // A disconnect during a catalogue fetch leaves no catalogue behind.
    down.store(0, Ordering::SeqCst);
    up.set(move |_: &Seen| (Duration::from_millis(500), reply("200 OK", good).1));
    store(
        f.store,
        "disconnect",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}"}}"#),
    )
    .await;
    store_tokens(
        &db,
        A,
        "synthetic-access-1",
        "synthetic-refresh-1",
        now + 3_600_000,
        3,
    );
    let addr = f.addr;
    let in_flight = up.count("/backend-api/codex/models") + 1;
    let fetching = tokio::spawn(async move { get(addr, MODELS, ADMIN).await });
    up.wait_for("/backend-api/codex/models", in_flight).await;
    store(
        f.store,
        "disconnect",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}"}}"#),
    )
    .await;
    let _ = fetching.await.unwrap();
    store_tokens(
        &db,
        A,
        "synthetic-access-2",
        "synthetic-refresh-2",
        now + 3_600_000,
        4,
    );
    let before = calls();
    let (s, got) = get(f.addr, MODELS, ADMIN).await;
    assert_eq!(
        (s, got["cached"].clone()),
        (200, j("false")),
        "fetched again for the new sign-in"
    );
    assert_eq!(calls(), before + 1);
}

// ---------------------------------------------------------------------------------------------
// A client that gives up must not strand the broker (sbstndalton/noevia#2190 round 5 review).

/// Waits (up to ten seconds) for a condition the broker reaches on its own.
async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let waiting = async {
        while !ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .unwrap_or_else(|_| panic!("never happened: {what}"));
}

/// Sends `request`, waits until `in_flight` says the upstream has the exchange, then closes the
/// socket without reading the answer: the caller went away mid-request.
async fn abandon(
    addr: std::net::SocketAddr,
    request: String,
    in_flight: impl std::future::Future<Output = ()>,
) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    in_flight.await;
    drop(s);
}

fn front_request(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> String {
    format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {ADMIN}\r\nOrigin: http://{addr}\r\nX-CSRF-Token: csrf\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
}

fn rotated(id_token: &str) -> String {
    format!(
        r#"{{"access_token":"synthetic-access-9","refresh_token":"synthetic-refresh-9","id_token":"{id_token}","expires_in":3600}}"#
    )
}

#[tokio::test]
async fn a_client_that_leaves_mid_exchange_still_gets_its_sign_in_saved() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let f = front(d.path(), &up.base(), "1").await;
    let id_token =
        jwt(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-synthetic"}}"#);
    up.set(move |r: &Seen| match r.path.as_str() {
        "/api/accounts/deviceauth/usercode" => reply(
            "200 OK",
            r#"{"device_auth_id":"synthetic-device","user_code":"ABCD-EFGH","interval":1}"#,
        ),
        "/api/accounts/deviceauth/token" => reply(
            "200 OK",
            r#"{"authorization_code":"synthetic-code","code_verifier":"synthetic-verifier"}"#,
        ),
        "/oauth/token" => (
            Duration::from_millis(700),
            reply("200 OK", &rotated(&id_token)).1,
        ),
        _ => reply("404 Not Found", ""),
    });
    let body = started(f.addr, ADMIN).await;
    // The poll interval (1 s) is real time: nothing to signal, so a margin.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    // The browser closes the tab while the authorization code is being exchanged.
    abandon(
        f.addr,
        front_request(f.addr, "POST", POLL, &body),
        up.wait_for("/oauth/token", 1),
    )
    .await;
    // The exchange finishes on its own: the one-time code is not burnt, the tokens are stored.
    eventually("the abandoned sign-in was saved", || {
        token_row(&db, A).is_some()
    })
    .await;
    let (raw, state) = token_row(&db, A).unwrap();
    assert_eq!(state, "connected");
    let tokens = open(&raw, A);
    assert_eq!(tokens["accessToken"], "synthetic-access-9");
    assert_eq!(tokens["refreshToken"], "synthetic-refresh-9");
    eventually("the connect was audited", || {
        audits(&db, A).iter().any(|a| a == "chatgpt.oauth.connect")
    })
    .await;
    assert_eq!(
        up.count("/oauth/token"),
        1,
        "the code was spent exactly once"
    );
    // Not stuck: the sign-in is settled, so a later poll is told so rather than left pending.
    assert_eq!(
        post(f.addr, POLL, ADMIN, &body).await,
        (200, j(r#"{"state":"expired"}"#))
    );
}

#[tokio::test]
async fn a_client_that_leaves_mid_poll_does_not_strand_the_polling_mark() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let up = upstream().await;
    let f = front(d.path(), &up.base(), "1").await;
    up.set(|r: &Seen| match r.path.as_str() {
        "/api/accounts/deviceauth/usercode" => reply(
            "200 OK",
            r#"{"device_auth_id":"synthetic-device","user_code":"ABCD-EFGH","interval":1}"#,
        ),
        // Still waiting for the person, and slow to say so.
        "/api/accounts/deviceauth/token" => (
            Duration::from_millis(700),
            reply("403 Forbidden", r#"{"error":"authorization_pending"}"#).1,
        ),
        _ => reply("404 Not Found", ""),
    });
    let body = started(f.addr, ADMIN).await;
    tokio::time::sleep(Duration::from_millis(1300)).await;
    abandon(
        f.addr,
        front_request(f.addr, "POST", POLL, &body),
        up.wait_for("/api/accounts/deviceauth/token", 1),
    )
    .await;
    // A later poll reaches the issuer again once the interval is up (a stranded mark would keep
    // answering `pending` from memory for the rest of the sign-in's life).
    let polled = async {
        while up.count("/api/accounts/deviceauth/token") < 2 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let (s, got) = post(f.addr, POLL, ADMIN, &body).await;
            assert_eq!((s, got["state"].clone()), (200, j(r#""pending""#)));
        }
    };
    tokio::time::timeout(Duration::from_secs(15), polled)
        .await
        .expect("a later poll was never let through: the polling mark is stuck");
}

#[tokio::test]
async fn a_client_that_leaves_mid_refresh_does_not_lose_the_rotated_token() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let f = front(d.path(), &up.base(), "1").await;
    let now = noevia_server::identity::now_ms();
    let id_token =
        jwt(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-synthetic"}}"#);
    up.set(move |r: &Seen| match r.path.as_str() {
        "/oauth/token" => (
            Duration::from_millis(700),
            reply("200 OK", &rotated(&id_token)).1,
        ),
        _ => reply("404 Not Found", ""),
    });
    // Inside the refresh-ahead window: the session call refreshes.
    store_tokens(
        &db,
        A,
        "synthetic-access-0",
        "synthetic-refresh-0",
        now + 60_000,
        1,
    );
    let user = format!(r#"{{"user":"{A}"}}"#);
    // Node's chat times out and drops the internal call while the issuer has rotated the token.
    let request = format!("POST /v1/chatgpt/session HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{}: {STORE_TOKEN}\r\n\r\n{user}", user.len(), project_store::TOKEN_HEADER);
    abandon(f.store, request, up.wait_for("/oauth/token", 1)).await;
    eventually("the rotated tokens were written", || {
        token_row(&db, A)
            .is_some_and(|(raw, _)| open(&raw, A)["accessToken"] == "synthetic-access-9")
    })
    .await;
    let (raw, state) = token_row(&db, A).unwrap();
    assert_eq!(state, "connected");
    assert_eq!(open(&raw, A)["refreshToken"], "synthetic-refresh-9");
    // The next caller uses them; no second refresh is spent.
    let (s, got) = store(f.store, "session", STORE_TOKEN, &user).await;
    assert_eq!(
        (s, got["accessToken"].clone()),
        (200, j(r#""synthetic-access-9""#))
    );
    assert_eq!(up.count("/oauth/token"), 1);
}

#[tokio::test]
async fn a_client_that_leaves_mid_fetch_does_not_waste_the_catalogue() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let f = front(d.path(), &up.base(), "1").await;
    let now = noevia_server::identity::now_ms();
    store_tokens(
        &db,
        A,
        "synthetic-access-0",
        "synthetic-refresh-0",
        now + 3_600_000,
        1,
    );
    up.set(|_: &Seen| {
        (
            Duration::from_millis(700),
            reply("200 OK", r#"{"models":[{"slug":"synthetic-model"}]}"#).1,
        )
    });
    abandon(
        f.addr,
        format!(
            "GET {MODELS} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nCookie: {ADMIN}\r\n\r\n",
            f.addr
        ),
        up.wait_for("/backend-api/codex/models", 1),
    )
    .await;
    // The fetch the account paid for is kept (or shared), not thrown away and repeated.
    let (s, got) = get(f.addr, MODELS, ADMIN).await;
    assert_eq!(
        (s, got["models"].clone()),
        (200, j(r#"["synthetic-model"]"#))
    );
    assert_eq!(up.count("/backend-api/codex/models"), 1);
}

#[tokio::test]
async fn tokens_of_deleted_accounts_are_swept_when_the_broker_starts() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let now = noevia_server::identity::now_ms();
    // Core deleted the account, then its "forget" call to the broker failed: the row stayed.
    store_tokens(&db, A, "a", "ra", now + 3_600_000, 1);
    store_tokens(&db, B, "b", "rb", now + 3_600_000, 2);
    store_tokens(&db, "cccccccc-cccc-cccc-cccc-cccccccccccc", "g", "rg", 1, 3);
    store_tokens(&db, "dddddddd-dddd-dddd-dddd-dddddddddddd", "h", "rh", 1, 4);
    let ghosts = || -> i64 {
        db.query_row(
            "SELECT count(*) FROM chatgpt_oauth_tokens WHERE user_id NOT IN (SELECT id FROM users)",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(ghosts(), 2);
    // With the sign-in switched off nothing public is served, but the credential channel is the
    // broker's too: its first call is the broker's first use.
    let f = front(d.path(), &up.base(), "0").await;
    let user = format!(r#"{{"user":"{A}"}}"#);
    let (s, got) = store(f.store, "connected", STORE_TOKEN, &user).await;
    assert_eq!((s, got["exists"].clone()), (200, j("true")));
    assert_eq!(ghosts(), 0, "the orphaned rows are deleted");
    assert!(token_row(&db, A).is_some() && token_row(&db, B).is_some());
    assert!(
        d.path().join("chatgpt-token-changes.json").exists(),
        "Node's token cache is told"
    );
}

#[tokio::test]
async fn the_sweep_also_runs_on_the_first_public_request() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let now = noevia_server::identity::now_ms();
    store_tokens(&db, A, "a", "ra", now + 3_600_000, 1);
    store_tokens(&db, "cccccccc-cccc-cccc-cccc-cccccccccccc", "g", "rg", 1, 3);
    let f = front(d.path(), &up.base(), "1").await;
    up.set(|_: &Seen| reply("200 OK", r#"{"models":[{"slug":"synthetic-model"}]}"#));
    assert_eq!(get(f.addr, MODELS, ADMIN).await.0, 200);
    assert!(token_row(&db, "cccccccc-cccc-cccc-cccc-cccccccccccc").is_none());
    assert!(token_row(&db, A).is_some());
}
