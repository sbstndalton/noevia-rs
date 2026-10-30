//! Shared by tests/gdrive.rs and tests/offsite.rs (sbstndalton/noevia#2190 round 7): a synthetic
//! Google (device sign-in, token, revoke and a small Drive model) on 127.0.0.1, the real front and
//! its real internal store listener over a temporary data directory. No real account, token, Google
//! endpoint or off-site destination is ever contacted; the legacy upstream is dead.
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
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
pub const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
pub const C: &str = "cccccccc-cccc-cccc-cccc-cccccccccccc";
pub const ADMIN: &str = "cowork_session=admin-token; cowork_csrf=csrf";
pub const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";
pub const ADMIN2: &str = "cowork_session=admin2-token; cowork_csrf=csrf";
pub const SECRETS_KEY: [u8; 32] = [7; 32];
pub const BACKUP_KEY_HEX: &str = "0909090909090909090909090909090909090909090909090909090909090909";
pub const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";

pub fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn j(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|_| panic!("not JSON: {s}"))
}

pub struct Res {
    pub status: u16,
    pub head: String,
    pub text: String,
}

impl Res {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.text).unwrap_or(serde_json::Value::Null)
    }
}

pub async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    extra: &str,
    body: &[u8],
) -> Res {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n", body.len()).as_bytes()).await.unwrap();
    let _ = s.write_all(body).await;
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(60), s.read_to_end(&mut bytes)).await;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (h, b) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    Res {
        status: h
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        head: h.to_ascii_lowercase(),
        text: b.to_string(),
    }
}

/// A state-changing request from a signed-in browser (cookie, origin and CSRF header).
pub async fn send(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    body: &str,
) -> Res {
    let csrf = format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n");
    http(addr, method, path, cookie, &csrf, body.as_bytes()).await
}

pub async fn get(addr: std::net::SocketAddr, path: &str, cookie: &str) -> Res {
    http(addr, "GET", path, cookie, "", b"").await
}

/// One internal store call (`/v1/<op>`).
pub async fn store(
    addr: std::net::SocketAddr,
    op: &str,
    token: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("POST /v1/{op} HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{}: {token}\r\n\r\n{body}", body.len(), project_store::TOKEN_HEADER).as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(60), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (
        status,
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
}

// ---------------------------------------------------------------------------------------------
// A synthetic HTTP server with a replaceable handler.

#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: String,
    pub body: String,
    pub bytes: Vec<u8>,
}

pub type Handler = Arc<dyn Fn(&Seen) -> (Duration, Vec<u8>) + Send + Sync>;

#[derive(Clone)]
pub struct Upstream {
    pub addr: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    handler: Arc<Mutex<Handler>>,
    hit: Arc<tokio::sync::Notify>,
}

impl Upstream {
    /// Waits until the upstream has received `n` requests whose path starts with `path`.
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
        tokio::time::timeout(Duration::from_secs(30), waiting)
            .await
            .unwrap_or_else(|_| panic!("the upstream never saw request {n} of {path}"));
    }
    pub fn set(&self, f: impl Fn(&Seen) -> (Duration, Vec<u8>) + Send + Sync + 'static) {
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
}

pub fn reply(status: &str, body: &str) -> (Duration, Vec<u8>) {
    (
        Duration::ZERO,
        format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes(),
    )
}

/// A raw (binary) 200 reply.
pub fn bytes_reply(body: &[u8]) -> (Duration, Vec<u8>) {
    let mut out = format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    (Duration::ZERO, out)
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
                let mut chunk = [0u8; 8192];
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
                    bytes: buf[head_end..].to_vec(),
                };
                seen.lock().unwrap().push(request.clone());
                hit.notify_waiters();
                let f = handler.lock().unwrap().clone();
                let (delay, text) = f(&request);
                tokio::time::sleep(delay).await;
                let _ = s.write_all(&text).await;
                let _ = s.shutdown().await;
            });
        }
    });
    up
}

// ---------------------------------------------------------------------------------------------
// The front over a temporary data directory.

pub struct Front {
    pub addr: std::net::SocketAddr,
    pub store: std::net::SocketAddr,
    pub app: Arc<App>,
}

pub fn seed(dir: &std::path::Path) -> Connection {
    std::fs::write(dir.join("secrets.key"), SECRETS_KEY).unwrap();
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    // core tool-policy.cjs's own DDL (Node stays the migrator).
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS tool_policies(user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tool TEXT NOT NULL, mode TEXT NOT NULL CHECK(mode IN ('allow','ask','block')), updated_at INTEGER NOT NULL,
    PRIMARY KEY(user_id, tool));",
    )
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, t) in [
        (A, "admin", "admin-token"),
        (B, "member", "member-token"),
        (C, "admin", "admin2-token"),
    ] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(t), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
    }
    db
}

/// A front with Google at `google` (the synthetic server's base) and the given extra environment.
pub async fn front(dir: &std::path::Path, google: &str, extra: &[(&str, String)]) -> Front {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut env: HashMap<String, String> = HashMap::from([
        (
            "NOEVIA_LEGACY_UPSTREAM".to_string(),
            "http://127.0.0.1:9".to_string(),
        ),
        ("UI_DATA_DIR".to_string(), dir.display().to_string()),
        ("PUBLIC_ORIGIN".to_string(), format!("http://{addr}")),
        (
            "GOOGLE_OAUTH_CLIENT_ID".to_string(),
            "synthetic-client".to_string(),
        ),
        (
            "GOOGLE_OAUTH_CLIENT_SECRET".to_string(),
            "synthetic-secret".to_string(),
        ),
        ("GOOGLE_OAUTH_BASE_URL".to_string(), google.to_string()),
        (
            "GOOGLE_DRIVE_API_BASE_URL".to_string(),
            format!("{google}/drive/v3"),
        ),
        (
            "GOOGLE_DRIVE_UPLOAD_BASE_URL".to_string(),
            format!("{google}/upload/drive/v3"),
        ),
    ]);
    for (k, v) in extra {
        env.insert((*k).to_string(), v.clone());
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

pub fn jwt(claims: &str) -> String {
    use base64::Engine as _;
    format!(
        "h.{}.s",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims)
    )
}

/// Node's `gdrive.cjs` sealer, written with the same algorithm as the golden vector, so a test can
/// plant a token file exactly as Node would have saved it (account key = secrets.cjs derive).
pub fn account_sealer() -> google_token_seal::Sealer {
    let key = google_token_seal::derive_secret_key(&SECRETS_KEY, google_token_seal::USER_LABEL);
    google_token_seal::Sealer::new(key.as_slice()).unwrap()
}

pub fn backup_sealer() -> google_token_seal::Sealer {
    google_token_seal::Sealer::new(&[9u8; 32]).unwrap()
}

pub fn plant_account_file(dir: &std::path::Path, user: &str, record: &str) {
    let d = dir.join("google-drive-users");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join(format!("{user}.sealed")),
        account_sealer().seal(record.as_bytes(), &[3; 12]).unwrap(),
    )
    .unwrap();
}

pub fn read_account_file(dir: &std::path::Path, user: &str) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(
        dir.join("google-drive-users")
            .join(format!("{user}.sealed")),
    )
    .ok()?;
    Some(j(std::str::from_utf8(
        account_sealer().open(&text).unwrap().as_slice(),
    )
    .unwrap()))
}

pub fn read_backup_file(dir: &std::path::Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(dir.join("google-drive.sealed")).ok()?;
    Some(j(std::str::from_utf8(
        backup_sealer().open(&text).unwrap().as_slice(),
    )
    .unwrap()))
}

pub fn state_file(dir: &std::path::Path) -> serde_json::Value {
    j(
        &std::fs::read_to_string(dir.join("gdrive-connections.json"))
            .unwrap_or_else(|_| "null".into()),
    )
}

/// The synthetic Google's OAuth behaviour: `granted` flips the device grant; `refreshes` counts
/// refresh-token grants and numbers the access tokens.
pub struct Google {
    pub granted: Arc<std::sync::atomic::AtomicUsize>,
    pub refreshes: Arc<std::sync::atomic::AtomicUsize>,
}

pub fn install_oauth(up: &Upstream, email: &str, refresh_delay: Duration) -> Google {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let g = Google {
        granted: Arc::new(AtomicUsize::new(0)),
        refreshes: Arc::new(AtomicUsize::new(0)),
    };
    let (granted, refreshes) = (Arc::clone(&g.granted), Arc::clone(&g.refreshes));
    let id_token = jwt(&format!(r#"{{"email":"{email}"}}"#));
    up.set(move |r: &Seen| match r.path.as_str() {
        "/device/code" => reply("200 OK", r#"{"device_code":"dev-1","user_code":"ABCD-EFGH","verification_url":"https://www.google.com/device","expires_in":1800,"interval":1}"#),
        "/token" if r.body.contains("grant_type=urn") => {
            if granted.load(Ordering::SeqCst) == 0 {
                reply("428 Precondition Required", r#"{"error":"authorization_pending"}"#)
            } else {
                reply("200 OK", &format!(r#"{{"access_token":"access-0","refresh_token":"refresh-0","expires_in":3600,"id_token":"{id_token}"}}"#))
            }
        }
        "/token" if r.body.contains("grant_type=refresh_token") => {
            let n = refreshes.fetch_add(1, Ordering::SeqCst) + 1;
            let (_, text) = reply("200 OK", &format!(r#"{{"access_token":"access-{n}","expires_in":3600}}"#));
            (refresh_delay, text)
        }
        "/revoke" => reply("200 OK", "{}"),
        _ => reply("404 Not Found", ""),
    });
    g
}
