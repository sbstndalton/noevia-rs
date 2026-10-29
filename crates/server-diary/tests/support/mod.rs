//! A synthetic Diary sidecar on loopback and a synthetic tenant database. Nothing real is read.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use server_auth::{Credential, Identity, PublicUser, Role};
use server_diary::{Ctx, Diary, DiaryConfig, Limits};
use server_store::rusqlite::Connection;
use server_store::Store;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const ADMIN: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
pub const MEMBER: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
pub const OTHER: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
pub const TENANT_KEY: &str = "synthetic-tenant-key-for-tests";
pub const BEARER: &str = "synthetic-sidecar-bearer";
pub const SECRET: &str = "synthetic-storage-password";

#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub target: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap()
    }
    pub fn query(&self) -> &str {
        self.target.split_once('?').map_or("", |(_, q)| q)
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
    pub fn storage(&self) -> serde_json_lite::Value {
        let raw = self.header("x-cowork-storage").expect("storage header");
        let bytes = b64url_decode(raw);
        serde_json_lite::parse(std::str::from_utf8(&bytes).unwrap())
    }
}

/// A tiny JSON reader for the descriptor (the tests do not need serde_json's full surface, and
/// the key ORDER matters here, which `Vec<(String, Value)>` keeps).
pub mod serde_json_lite {
    pub type Value = js_json::JValue;
    pub fn parse(text: &str) -> Value {
        js_json::parse(text).expect("json")
    }
}

fn b64url_decode(text: &str) -> Vec<u8> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::new();
    for c in text.bytes() {
        let v = alphabet.iter().position(|a| *a == c).expect("base64url") as u32;
        bits = (bits << 6) | v;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push(((bits >> count) & 0xff) as u8);
        }
    }
    out
}

pub enum Out {
    Full {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
        delay: Duration,
    },
    /// Sends `chunks`, then ends (or, with `forever`, keeps sending 64 KiB chunks until the
    /// client goes away). `sent` counts the bytes written.
    Stream {
        status: u16,
        headers: Vec<(&'static str, String)>,
        chunks: Vec<Vec<u8>>,
        forever: bool,
    },
    /// Sends `chunk`, then nothing more until the client goes away; `dropped` is set when the
    /// connection gives the body up.
    Stall {
        status: u16,
        headers: Vec<(&'static str, String)>,
        chunk: Vec<u8>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    },
    /// Closes the connection without an answer.
    Hangup,
}

/// [`Out::Stall`]'s body: one data frame, then pending until dropped.
struct StallBody {
    first: Option<Bytes>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

impl http_body::Body for StallBody {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, std::io::Error>>> {
        match self.first.take() {
            Some(chunk) => std::task::Poll::Ready(Some(Ok(http_body::Frame::data(chunk)))),
            None => std::task::Poll::Pending,
        }
    }
}

impl Drop for StallBody {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

impl Out {
    pub fn json(status: u16, body: &str) -> Out {
        Out::Full {
            status,
            headers: vec![("content-type", "application/json".into())],
            body: body.as_bytes().to_vec(),
            delay: Duration::ZERO,
        }
    }
    pub fn text(status: u16, body: &str) -> Out {
        Out::Full {
            status,
            headers: vec![("content-type", "text/plain".into())],
            body: body.as_bytes().to_vec(),
            delay: Duration::ZERO,
        }
    }
    pub fn zip(body: &[u8]) -> Out {
        Out::Full {
            status: 200,
            headers: vec![("content-type", "application/zip".into())],
            body: body.to_vec(),
            delay: Duration::ZERO,
        }
    }
    pub fn slow(mut self, wait: Duration) -> Out {
        if let Out::Full { delay, .. } = &mut self {
            *delay = wait;
        }
        self
    }
}

pub type Script = Arc<dyn Fn(&Seen, usize) -> Out + Send + Sync>;

pub struct Stub {
    pub addr: SocketAddr,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    /// Bytes an endless stream has written.
    pub streamed: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Stub {
    pub async fn start(script: impl Fn(&Seen, usize) -> Out + Send + Sync + 'static) -> Stub {
        let script: Script = Arc::new(script);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let streamed = Arc::new(AtomicUsize::new(0));
        let (seen2, streamed2) = (Arc::clone(&seen), Arc::clone(&streamed));
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (script, seen, streamed) = (
                    Arc::clone(&script),
                    Arc::clone(&seen2),
                    Arc::clone(&streamed2),
                );
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let (script, seen, streamed) = (
                            Arc::clone(&script),
                            Arc::clone(&seen),
                            Arc::clone(&streamed),
                        );
                        async move { answer(req, script, seen, streamed).await }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Stub {
            addr,
            seen,
            streamed,
            task,
        }
    }

    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

type Body = BoxBody<Bytes, std::io::Error>;

async fn answer(
    req: Request<Incoming>,
    script: Script,
    seen: Arc<Mutex<Vec<Seen>>>,
    streamed: Arc<AtomicUsize>,
) -> Result<Response<Body>, std::io::Error> {
    let (parts, body) = req.into_parts();
    let bytes = body
        .collect()
        .await
        .map_err(std::io::Error::other)?
        .to_bytes();
    let record = Seen {
        method: parts.method.to_string(),
        target: parts
            .uri
            .path_and_query()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_default(),
        headers: parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
            .collect(),
        body: bytes.to_vec(),
    };
    let index = {
        let mut all = seen.lock().unwrap();
        all.push(record.clone());
        all.len() - 1
    };
    match script(&record, index) {
        Out::Hangup => Err(std::io::Error::other("hangup")),
        Out::Stall {
            status,
            headers,
            chunk,
            dropped,
        } => {
            let mut res = Response::builder().status(status);
            for (k, v) in headers {
                res = res.header(k, v);
            }
            let body = StallBody {
                first: Some(Bytes::from(chunk)),
                dropped,
            };
            Ok(res.body(body.boxed()).unwrap())
        }
        Out::Full {
            status,
            headers,
            body,
            delay,
        } => {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let mut res = Response::builder().status(status);
            for (k, v) in headers {
                res = res.header(k, v);
            }
            Ok(res
                .body(Full::new(Bytes::from(body)).map_err(|e| match e {}).boxed())
                .unwrap())
        }
        Out::Stream {
            status,
            headers,
            chunks,
            forever,
        } => {
            let (mut tx, rx) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(2);
            tokio::spawn(async move {
                for c in chunks {
                    streamed.fetch_add(c.len(), Ordering::SeqCst);
                    if tx.send_data(Bytes::from(c)).await.is_err() {
                        return;
                    }
                }
                if forever {
                    loop {
                        streamed.fetch_add(65536, Ordering::SeqCst);
                        if tx.send_data(Bytes::from(vec![7u8; 65536])).await.is_err() {
                            return;
                        }
                    }
                }
            });
            let mut res = Response::builder().status(status);
            for (k, v) in headers {
                res = res.header(k, v);
            }
            Ok(res.body(rx.boxed()).unwrap())
        }
    }
}

/// A synthetic tenant database with three accounts and optionally a stored connection each.
pub struct World {
    pub dir: tempfile::TempDir,
    pub store: Arc<Store>,
}

pub struct Conn {
    pub kind: &'static str,
    pub base_url: &'static str,
    pub secret: String,
}

impl World {
    pub fn new(connections: &[(&str, Conn)]) -> World {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open(dir.path().join("cowork.db")).unwrap();
        db.execute_batch(include_str!(
            "../../../server-store/tests/fixtures/node-schema.sql"
        ))
        .unwrap();
        for (id, role) in [(ADMIN, "admin"), (MEMBER, "member"), (OTHER, "member")] {
            db.execute(
                "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)",
                [id, role],
            )
            .unwrap();
            db.execute(
                "INSERT INTO user_features(user_id,diary_enabled,updated_at) VALUES(?1,1,1)",
                [id],
            )
            .unwrap();
        }
        // A real secrets.key, so a stored credential is opened as the front opens it.
        let key = vec![9u8; 32];
        std::fs::write(dir.path().join("secrets.key"), &key).unwrap();
        for (id, c) in connections {
            let sealed = if c.secret.is_empty() {
                String::new()
            } else {
                secret_envelope::seal(&key, &[3u8; 12], c.secret.as_bytes(), Some(id.as_bytes()))
                    .unwrap()
            };
            db.execute(
                "INSERT INTO storage_connections(user_id,kind,base_url,username,secret,corpus_root,updated_at) VALUES(?1,?2,?3,'ada',?4,'/Diary',1)",
                [id, c.kind, c.base_url, sealed.as_str()],
            )
            .unwrap();
        }
        let store = Arc::new(Store::open(dir.path()).unwrap());
        World { dir, store }
    }

    pub fn ctx(&self, id: &str, diary: bool) -> Ctx {
        let role = if id == ADMIN {
            Role::Admin
        } else {
            Role::Member
        };
        Ctx {
            identity: Identity {
                user: PublicUser {
                    id: id.to_owned(),
                    username: id.to_owned(),
                    display_name: "Synthetic".into(),
                    role,
                    disabled: false,
                    diary_enabled: diary,
                    onboarded: true,
                },
                account_role: role,
                credential: Credential::Legacy,
            },
            store: Arc::clone(&self.store),
            data_dir: self.dir.path().to_owned(),
        }
    }

    /// `users/<id>/migration.json`, the legacy-owner marker.
    pub fn mark_legacy(&self, id: &str) {
        let dir = self.dir.path().join("users").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("migration.json"), "{}").unwrap();
    }
}

pub fn config(stub: &Stub, token: &str, key: &str, members: &'static str) -> DiaryConfig {
    DiaryConfig::new(stub.base(), token, key, move || members.to_owned())
}

/// A deployment that lists the synthetic cloud's origin in MEMBER_OUTBOUND_ORIGINS.
pub fn diary(stub: &Stub, key: &str) -> Diary {
    Diary::new(config(stub, BEARER, key, "https://cloud.example")).unwrap()
}

pub fn diary_with(stub: &Stub, key: &str, limits: Limits) -> Diary {
    Diary::with_limits(config(stub, BEARER, key, "https://cloud.example"), limits).unwrap()
}
