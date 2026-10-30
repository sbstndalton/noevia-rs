//! Shared helpers of the model maintenance tests (tests/model_reload.rs and its siblings): the front
//! on an ephemeral port over a seeded cowork.db, an HTTP caller, and a scripted fake engine (router
//! and inference gate on one port, as in production). Synthetic data only.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

pub const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
pub const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
pub const ADMIN: &str = "cowork_session=admin-token; cowork_csrf=csrf";
pub const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";

pub fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The suffix Core gives its state files: the first 16 hex digits of the base URL's sha256.
pub fn suffix(base: &str) -> String {
    hex(base).chars().take(16).collect()
}

/// A request to the front: (status, lower-cased headers, body).
pub async fn call(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: &[u8],
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let cookie = cookie
        .map(|c| format!("Cookie: {c}\r\n"))
        .unwrap_or_default();
    let csrf = if method == "GET" {
        String::new()
    } else {
        format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n")
    };
    s.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{cookie}{csrf}Content-Length: {}\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let _ = s.write_all(body).await;
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), s.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        h.to_ascii_lowercase(),
        b.to_string(),
    )
}

/// An admin and a member with sessions.
pub fn seed_database(dir: &Path) -> Connection {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, token) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
    }
    db
}

/// The front over `dir` with the extra environment; the legacy upstream is dead, so a proxied
/// request answers 502 and only the native handler can answer anything else.
pub async fn start(
    dir: &Path,
    extra: &[(String, String)],
) -> (std::net::SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut env: HashMap<String, String> = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM".into(), "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR".into(), dir.display().to_string()),
        ("TRUST_PROXY".into(), "true".into()),
        ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH".into(), "1".into()),
        ("PUBLIC_ORIGIN".into(), format!("http://{addr}")),
    ]);
    for (k, v) in extra {
        env.insert(k.clone(), v.clone());
    }
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    // main.rs starts the evidence inbox's drain loop; so does a front under test.
    app.config.maint.spawn_inbox();
    let (stop, rx) = oneshot::channel();
    tokio::spawn(serve(
        listener,
        Arc::clone(&app),
        Limits::default(),
        async {
            let _ = rx.await;
        },
    ));
    (addr, stop)
}

/// A request the fake engine received.
#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub target: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

/// What the fake engine answers.
pub enum Answer {
    Reply(u16, String),
    /// Close the connection without answering.
    Drop,
    /// Answer after a pause.
    After(Duration, u16, String),
}

pub type Handler = Arc<dyn Fn(&Req) -> Answer + Send + Sync>;

/// A fake engine: every request goes to `handler`; returns the port and the request log.
pub async fn engine(handler: Handler) -> (u16, Arc<Mutex<Vec<Req>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (port, seen, _task) = engine_on(listener, handler, port).await;
    (port, seen)
}

/// Ends the fake engine's accept loop when dropped (so its port can be bound again).
pub struct Stop(pub tokio::task::JoinHandle<()>);

impl Drop for Stop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn engine_on(
    listener: TcpListener,
    handler: Handler,
    port: u16,
) -> (u16, Arc<Mutex<Vec<Req>>>, tokio::task::JoinHandle<()>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&log);
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let head_end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let first = lines.next().unwrap_or("").to_string();
                let mut parts = first.split(' ');
                let method = parts.next().unwrap_or("").to_string();
                let target = parts.next().unwrap_or("").to_string();
                let headers: HashMap<String, String> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let want: usize = headers
                    .get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = buf[head_end + 4..].to_vec();
                while body.len() < want {
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                    }
                }
                let req = Req {
                    method,
                    target,
                    headers,
                    body: String::from_utf8_lossy(&body).to_string(),
                };
                log.lock().unwrap().push(req.clone());
                let mut answer = handler(&req);
                if let Answer::After(wait, code, text) = answer {
                    tokio::time::sleep(wait).await;
                    answer = Answer::Reply(code, text);
                }
                match answer {
                    Answer::Drop | Answer::After(..) => {}
                    Answer::Reply(code, text) => {
                        let reply = if code == 204 {
                            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_string()
                        } else {
                            format!(
                                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                                text.len()
                            )
                        };
                        let _ = s.write_all(reply.as_bytes()).await;
                    }
                }
            });
        }
    });
    (port, seen, task)
}
