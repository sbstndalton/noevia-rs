//! GET /api/providers/{id}/models through the real front (sbstndalton/noevia#2190 round 5): the
//! stored rows read from disk, the per-account cache, a ChatGPT row through the token broker. The
//! providers are synthetic http servers on 127.0.0.1 and the legacy upstream is dead, so a request
//! that fell through to Node would answer 502. MEMBER_OUTBOUND_ORIGINS is never set in this binary,
//! so a member's own row is refused every origin. No real account, key or credential.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    dead_code
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

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn j(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|_| panic!("not JSON: {s}"))
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
    };
    let (seen, handler) = (Arc::clone(&up.seen), Arc::clone(&up.handler));
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let (seen, handler) = (Arc::clone(&seen), Arc::clone(&handler));
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
    }
    db
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

fn seal_v1(plain: &str, n: u8) -> String {
    secret_envelope::seal(&KEY, &[n; 12], plain.as_bytes(), None).unwrap()
}

fn rows(dir: &std::path::Path, file: &str, rows: &str) {
    let path = dir.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!(r#"{{"providers":{rows}}}"#)).unwrap();
}

#[tokio::test]
async fn provider_model_lists_follow_core() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let base = up.base();
    let f = front(d.path(), &base, "1").await;
    let addr = f.addr;
    let lists = |ids: &str| reply("200 OK", &format!(r#"{{"data":[{ids}]}}"#));
    up.set(move |r: &Seen| match r.path.as_str() {
        "/v1/models" if r.headers.contains("bearer sk-synthetic-1111") => {
            lists(r#"{"id":" syn-fast "},{"id":"syn-fast"},{"id":7},{"id":"syn-smart"}"#)
        }
        "/v1/models" if r.headers.contains("bearer sk-synthetic-9999") => {
            lists(r#"{"id":"syn-new"}"#)
        }
        "/team/v1/models" => lists(r#"{"id":"syn-team"}"#),
        "/denied/v1/models" => reply("401 Unauthorized", "{}"),
        p if p.starts_with("/backend-api/codex/models") => {
            reply("200 OK", r#"{"models":[{"slug":"gpt-synthetic"}]}"#)
        }
        _ => reply("404 Not Found", ""),
    });
    rows(
        d.path(),
        &format!("users/{A}/providers.json"),
        &format!(
            r#"[{{"id":"mine","label":"Mine","baseUrl":"{base}/v1","apiKey":"{}"}},{{"id":"denied","baseUrl":"{base}/denied/v1","apiKey":""}},{{"id":"far","baseUrl":"http://127.0.0.1:9/v1","apiKey":""}},{{"id":"none","baseUrl":"ftp://x"}}]"#,
            seal_v1("sk-synthetic-1111", 1)
        ),
    );
    rows(
        d.path(),
        &format!("users/{B}/providers.json"),
        &format!(r#"[{{"id":"own","baseUrl":"{base}/v1","apiKey":"local"}}]"#),
    );
    rows(
        d.path(),
        "shared-providers.json",
        &format!(r#"[{{"id":"team","baseUrl":"{base}/team/v1","apiKey":"","shared":true}}]"#),
    );

    // The gate first.
    assert_eq!(get(addr, "/api/providers/mine/models", "").await.0, 401);
    // The stored key, server-side, never in the answer; model ids as modelIdsFrom.
    let (s, got) = get(addr, "/api/providers/mine/models", ADMIN).await;
    assert_eq!(s, 200, "{got}");
    assert_eq!(got["models"], j(r#"["syn-fast","syn-smart"]"#));
    assert_eq!(got["cached"], false);
    assert!(!got.to_string().contains("sk-synthetic"));
    let sent = up
        .seen()
        .into_iter()
        .find(|r| r.path == "/v1/models")
        .unwrap();
    assert!(sent.headers.contains("content-type: application/json"));
    // Cached; a refresh fetches; a refresh within ten seconds is throttled.
    assert_eq!(
        get(addr, "/api/providers/mine/models", ADMIN).await.1["cached"],
        true
    );
    assert_eq!(up.count("/v1/models"), 1);
    let (_, got) = get(addr, "/api/providers/mine/models?refresh=1", ADMIN).await;
    assert_eq!(
        (got["cached"].clone(), up.count("/v1/models")),
        (j("false"), 2)
    );
    let (_, got) = get(addr, "/api/providers/mine/models?refresh=1", ADMIN).await;
    assert_eq!(
        (got["throttled"].clone(), up.count("/v1/models")),
        (j("true"), 2)
    );
    // A new key is a new list.
    rows(
        d.path(),
        &format!("users/{A}/providers.json"),
        &format!(
            r#"[{{"id":"mine","baseUrl":"{base}/v1","apiKey":"{}"}},{{"id":"denied","baseUrl":"{base}/denied/v1","apiKey":""}},{{"id":"far","baseUrl":"http://127.0.0.1:9/v1","apiKey":""}},{{"id":"none","baseUrl":"ftp://x"}}]"#,
            seal_v1("sk-synthetic-9999", 2)
        ),
    );
    let (_, got) = get(addr, "/api/providers/mine/models", ADMIN).await;
    assert_eq!(
        (got["models"].clone(), got["cached"].clone()),
        (j(r#"["syn-new"]"#), j("false"))
    );

    // Refusals in core's order and words.
    let e = |m: &str| serde_json::json!({ "error": m });
    let default = e("the default provider lists its models under Settings → Models");
    assert_eq!(
        get(addr, "/api/providers/default/models", ADMIN).await,
        (400, default.clone())
    );
    assert_eq!(
        get(addr, "/api/providers/lemonade/models", ADMIN).await,
        (400, default)
    );
    assert_eq!(
        get(addr, "/api/providers/ghost/models", ADMIN).await,
        (404, e("no such provider"))
    );
    assert_eq!(
        get(addr, "/api/providers/%E0%A4/models", ADMIN).await,
        (400, e("no such provider"))
    );
    assert_eq!(
        post(addr, "/api/providers/mine/models", ADMIN, "{}").await,
        (405, e("method not allowed"))
    );
    assert_eq!(
        get(addr, "/api/providers/none/models", ADMIN).await,
        (400, e("this provider has no address"))
    );
    assert_eq!(
        get(addr, "/api/providers/denied/models", ADMIN).await,
        (502, e("provider returned 401"))
    );
    assert_eq!(
        get(addr, "/api/providers/far/models", ADMIN).await,
        (502, e("The provider could not be reached."))
    );
    // A member's own row needs an approved origin; nothing is contacted. A shared row is used as it is.
    let before = up.seen().len();
    assert_eq!(
        get(addr, "/api/providers/own/models", MEMBER).await,
        (
            400,
            e("Provider origin is not approved for member connections; contact an administrator.")
        )
    );
    assert_eq!(up.seen().len(), before);
    assert_eq!(
        get(addr, "/api/providers/team/models", MEMBER).await.1["models"],
        j(r#"["syn-team"]"#)
    );
    // Another account never reads this account's rows.
    assert_eq!(get(addr, "/api/providers/mine/models", MEMBER).await.0, 404);

    // The ChatGPT row: the broker's catalogue; the token row stands for a row core has not listed.
    assert_eq!(
        get(addr, "/api/providers/chatgpt-oauth/models", ADMIN)
            .await
            .0,
        404
    );
    store_tokens(
        &db,
        A,
        "synthetic-access-0",
        "synthetic-refresh-0",
        noevia_server::identity::now_ms() + 3_600_000,
        1,
    );
    let (s, got) = get(addr, "/api/providers/chatgpt-oauth/models", ADMIN).await;
    assert_eq!((s, got["models"].clone()), (200, j(r#"["gpt-synthetic"]"#)));
    rows(
        d.path(),
        "shared-providers.json",
        r#"[{"id":"shared-gpt","kind":"chatgpt-oauth","shared":true}]"#,
    );
    assert_eq!(
        get(addr, "/api/providers/shared-gpt/models", ADMIN).await,
        (404, e("Sign in with ChatGPT is turned off on this server."))
    );
    // Shapes Node keeps: the broker's static route's other methods, a tail.
    assert_eq!(
        post(addr, "/api/providers/chatgpt/models", ADMIN, "{}")
            .await
            .0,
        502
    );
    assert_eq!(
        get(addr, "/api/providers/mine/models/x", ADMIN).await.0,
        502
    );
}

#[tokio::test]
async fn concurrent_lists_share_one_fetch_and_flag_off_hides_chatgpt() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let base = up.base();
    let f = front(d.path(), &base, "0").await;
    up.set(|_: &Seen| {
        (
            Duration::from_millis(300),
            reply("200 OK", r#"{"data":[{"id":"syn"}]}"#).1,
        )
    });
    rows(
        d.path(),
        &format!("users/{A}/providers.json"),
        &format!(
            r#"[{{"id":"mine","baseUrl":"{base}","apiKey":""}},{{"id":"chatgpt-oauth","kind":"chatgpt-oauth","external":true}}]"#
        ),
    );
    let calls: Vec<_> = (0..4)
        .map(|_| {
            let addr = f.addr;
            tokio::spawn(async move { get(addr, "/api/providers/mine/models", ADMIN).await })
        })
        .collect();
    for c in calls {
        let (s, got) = c.await.unwrap();
        assert_eq!((s, got["models"].clone()), (200, j(r#"["syn"]"#)));
    }
    assert_eq!(
        up.count("/v1/models"),
        1,
        "one upstream fetch for concurrent readers"
    );
    store_tokens(
        &db,
        A,
        "synthetic-access-0",
        "synthetic-refresh-0",
        noevia_server::identity::now_ms() + 3_600_000,
        1,
    );
    assert_eq!(
        get(f.addr, "/api/providers/chatgpt-oauth/models", ADMIN).await,
        (
            404,
            serde_json::json!({ "error": "Sign in with ChatGPT is turned off on this server." })
        )
    );
}
