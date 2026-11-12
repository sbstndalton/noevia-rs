//! `POST /api/chat` through the front (chat port S5, `rust-chat-turn`): the session gate, then the request
//! unread to Core's loopback `/internal/chat/turn`, and Core's answer back: JSON byte for byte, and since chat
//! port S7 (`rust-chat-wire`) the turn's events, which Core sends as NDJSON, as the event stream the front
//! writes itself (headers, frames, the 5 s keep-alive, the end). Core is a loopback stand-in here that checks
//! what the front sent and answers as Core's pipeline would (NDJSON events, the gate's 503, a crash
//! mid-stream). tests/fixtures/node-chat-wire.v1.json is core's own rust-chat-wire.cjs writing the same turns
//! both ways (tests/chat-wire-oracle.cjs). No model runs; synthetic data only. The real Core behind the same
//! relay is core's rust-chat-turn.test.cjs and the corpus replay.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-chat-turn-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const FAILED: &str =
    "data: {\"type\":\"error\",\"text\":\"The request could not be completed. Please retry.\"}\n\n";
const BUSY: &str = r#"{"error":"Inference is paused for maintenance.","code":"inferenceMaintenance","retryable":true,"retryAfter":3}"#;

type UpBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Turn {
    /// JSON: what Core saw (path, headers, body length).
    Echo,
    /// NDJSON whose bytes the test sends.
    Stream,
    /// NDJSON: one line, then nothing (Core no longer writes keep-alive comments for the front).
    Silent,
    /// NDJSON headers at once, the first line 1.5 s later (the routing question's early stream).
    LateFirstLine,
    /// The gate's 503.
    Busy,
    /// Headers and one line, then Core goes away inside the next.
    DieAfterHeaders,
    /// Core goes away before its headers.
    DieBeforeHeaders,
    /// Core works without answering ([`Core::closed`] fires when the front closes its socket).
    Hang,
    /// NDJSON lines of 64 KiB as fast as the front takes them (a client that stopped reading backs
    /// them up).
    Flood,
    /// A JSON answer's headers, then its body never comes.
    SlowJson,
    /// A Core that writes its own event stream (not this front's pair).
    OwnEventStream,
    /// NDJSON: [`Core::script`]'s chunks, then the end (or a break when `broken`).
    Script,
}

/// Core's inference gate behind its admission lease route (chat port S8, core
/// inference-admission-lease.cjs `{gate}`).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Gate {
    /// 201 with [`LEASE`].
    Open,
    /// The gate's refusal after its wait: 503 `{error, code}`.
    Held,
    /// Node still waiting: no answer until the asker goes away ([`Core::ask_closed`] fires then).
    Waiting,
    /// A Core before chat port S8: the ask is not one it knows.
    OldCore,
}

/// The lease the stand-in grants.
const LEASE: &str = "fedcba9876543210fedcba9876543210";
const ADMISSION: &str = "/internal/inference-admission/lease";

#[derive(Default)]
struct Seen {
    /// "METHOD path" of every call.
    hits: Vec<String>,
    /// Headers of the last call.
    headers: HashMap<String, Vec<String>>,
    body_len: usize,
    /// Chat port S8: each admission ask's body and whether it carried the per-start secret.
    asks: Vec<(String, bool)>,
    /// Leases granted and not yet given back.
    live: i64,
    /// "METHOD path" of each renewal or release, in order.
    releases: Vec<String>,
}

type AdmissionReply = (u16, String, Option<String>, Option<String>, bool);

struct Core {
    addr: SocketAddr,
    turn: Mutex<Turn>,
    gate: Mutex<Gate>,
    seen: Mutex<Seen>,
    stream: Mutex<Option<http_body_util::channel::Sender<Bytes, std::io::Error>>>,
    /// Fires when Core's answer body is dropped: the front closed Core's socket.
    closed: Mutex<Option<oneshot::Sender<Instant>>>,
    /// [`Turn::Script`]: the NDJSON chunks, and whether Core breaks after them.
    script: Mutex<(Vec<Bytes>, bool)>,
    /// [`Gate::Waiting`]: fires when the asker went away while Node waited.
    ask_closed: Mutex<Option<oneshot::Sender<Instant>>>,
    /// Review P2-1: the lease's TTL as granted, and how renewals are answered.
    ttl_ms: Mutex<u64>,
    renew: Mutex<Renew>,
    /// Arbitrary admission response and retry metadata for bounded-body regressions.
    admission_reply: Mutex<Option<AdmissionReply>>,
}

/// How the stand-in answers a lease renewal.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Renew {
    /// 200: confirmed.
    Ok,
    /// 404: Node ended the lease.
    Gone,
    /// Never answers (Node too busy to hear it).
    Hang,
}

/// Fires its sender when dropped (hyper drops a pending service future when its connection closes).
struct OnDrop(Option<oneshot::Sender<Instant>>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() {
            let _ = s.send(Instant::now());
        }
    }
}

/// Core's answer body; fires [`Core::closed`] when hyper drops it (the connection closed).
struct Watched {
    inner: UpBody,
    core: Arc<Core>,
}

impl hyper::body::Body for Watched {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, std::io::Error>>> {
        std::pin::Pin::new(&mut self.inner).poll_frame(cx)
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        if let Some(s) = self.core.closed.lock().unwrap().take() {
            let _ = s.send(Instant::now());
        }
    }
}

fn ndjson(core: &Arc<Core>, body: UpBody) -> Response<UpBody> {
    let body = Watched {
        inner: body,
        core: Arc::clone(core),
    };
    Response::builder()
        .status(200)
        .header("content-type", "application/x-ndjson")
        .header("cache-control", "no-store")
        .header("x-noevia-api", "1")
        .header("set-cookie", "a=1; Path=/")
        .body(body.boxed())
        .unwrap()
}

async fn core_service(
    core: Arc<Core>,
    req: Request<Incoming>,
) -> Result<Response<UpBody>, std::io::Error> {
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    let headers =
        req.headers()
            .iter()
            .fold(HashMap::<String, Vec<String>>::new(), |mut m, (k, v)| {
                m.entry(k.to_string())
                    .or_default()
                    .push(String::from_utf8_lossy(v.as_bytes()).into_owned());
                m
            });
    let body = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    let admission = path == ADMISSION || path.starts_with(&format!("{ADMISSION}/"));
    if !admission {
        let mut seen = core.seen.lock().unwrap();
        seen.hits.push(format!("{method} {path}"));
        seen.headers = headers.clone();
        seen.body_len = body.len();
    }
    let json = |status: u16, text: String| {
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .header("cache-control", "no-store")
            .header("x-noevia-api", "1")
            .body(Full::new(Bytes::from(text)).map_err(|e| match e {}).boxed())
            .unwrap()
    };
    if admission {
        let token = headers.get("x-noevia-store-token") == Some(&vec![STORE_TOKEN.to_owned()]);
        if path != ADMISSION {
            {
                let mut seen = core.seen.lock().unwrap();
                seen.releases.push(format!("{method} {path}"));
                if method == "DELETE" && path == format!("{ADMISSION}/{LEASE}") {
                    seen.live -= 1;
                }
            }
            if method == "POST" {
                let renew = *core.renew.lock().unwrap();
                if renew == Renew::Gone {
                    return Ok(json(404, r#"{"error":"Not found"}"#.into()));
                }
                if renew == Renew::Hang {
                    std::future::pending::<()>().await;
                }
            }
            return Ok(json(
                if method == "DELETE" { 204 } else { 200 },
                "{}".into(),
            ));
        }
        {
            let mut seen = core.seen.lock().unwrap();
            seen.hits.push(format!("{method} {path}"));
            seen.asks
                .push((String::from_utf8_lossy(&body).into_owned(), token));
        }
        if let Some((status, body, retry, code, chunked)) =
            core.admission_reply.lock().unwrap().clone()
        {
            let mut reply = json(status, body.clone());
            if chunked {
                // An unknown-size body forces HTTP/1 chunking, independent of its byte length.
                let (mut tx, stream) =
                    http_body_util::channel::Channel::<Bytes, std::io::Error>::new(1);
                tokio::spawn(async move {
                    for part in body.as_bytes().chunks(8192) {
                        if tx.send_data(Bytes::copy_from_slice(part)).await.is_err() {
                            break;
                        }
                    }
                });
                *reply.body_mut() = stream.boxed();
            }
            if let Some(retry) = retry {
                reply
                    .headers_mut()
                    .insert("retry-after", retry.parse().unwrap());
            }
            if let Some(code) = code {
                reply
                    .headers_mut()
                    .insert("x-noevia-admission-code", code.parse().unwrap());
            }
            return Ok(reply);
        }
        let gate = *core.gate.lock().unwrap();
        return Ok(match gate {
            Gate::Open => {
                core.seen.lock().unwrap().live += 1;
                let ttl = *core.ttl_ms.lock().unwrap();
                json(201, format!(r#"{{"lease":"{LEASE}","ttlMs":{ttl}}}"#))
            }
            Gate::Held => json(
                503,
                r#"{"error":"Inference is paused for maintenance.","code":"inferenceMaintenance"}"#
                    .into(),
            ),
            Gate::OldCore => json(400, r#"{"error":"A model is required."}"#.into()),
            Gate::Waiting => {
                let _closed = OnDrop(core.ask_closed.lock().unwrap().take());
                std::future::pending::<()>().await;
                unreachable!()
            }
        });
    }
    if path != "/internal/chat/turn" {
        return Ok(json(200, format!(r#"{{"proxied":"{method} {path}"}}"#)));
    }
    let mode = *core.turn.lock().unwrap();
    let channel = || http_body_util::channel::Channel::<Bytes, std::io::Error>::new(1);
    Ok(match mode {
        Turn::Echo => {
            let echo =
                serde_json::json!({ "path": path, "headers": headers, "bodyLen": body.len() });
            let mut r = json(200, echo.to_string());
            r.headers_mut()
                .append("set-cookie", "a=1; Path=/".parse().unwrap());
            r.headers_mut()
                .append("set-cookie", "b=2; Path=/".parse().unwrap());
            r
        }
        Turn::Busy => {
            let mut r = json(503, BUSY.to_owned());
            r.headers_mut().insert("retry-after", "3".parse().unwrap());
            r
        }
        Turn::Stream => {
            let (tx, body) = channel();
            *core.stream.lock().unwrap() = Some(tx);
            ndjson(&core, body.boxed())
        }
        Turn::Silent | Turn::LateFirstLine => {
            let (mut tx, body) = channel();
            let late = mode == Turn::LateFirstLine;
            tokio::spawn(async move {
                if late {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                }
                let first: &[u8] = if late {
                    b"{\"type\":\"route_pending\",\"id\":\"r1\",\"flag\":\"health\"}\n"
                } else {
                    b"{\"type\":\"meta\",\"model\":\"m\"}\n"
                };
                let _ = tx.send_data(Bytes::from_static(first)).await;
                // Held open (and silent) until the front lets go.
                std::future::pending::<()>().await;
                drop(tx);
            });
            ndjson(&core, body.boxed())
        }
        Turn::DieAfterHeaders => {
            let (mut tx, body) = channel();
            tokio::spawn(async move {
                let _ = tx
                    .send_data(Bytes::from_static(b"{\"type\":\"meta\"}\n"))
                    .await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = tx.send_data(Bytes::from_static(b"{\"type\":\"del")).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                tx.abort(std::io::Error::other("Core went away"));
            });
            ndjson(&core, body.boxed())
        }
        Turn::Script => {
            let (chunks, broken) = core.script.lock().unwrap().clone();
            let (mut tx, body) = channel();
            tokio::spawn(async move {
                for c in chunks {
                    if tx.send_data(c).await.is_err() {
                        return;
                    }
                }
                if broken {
                    // Inside its next line.
                    let _ = tx
                        .send_data(Bytes::from_static(b"{\"type\":\"delta\",\"te"))
                        .await;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    tx.abort(std::io::Error::other("Core went away"));
                }
            });
            ndjson(&core, body.boxed())
        }
        Turn::OwnEventStream => {
            let (mut tx, body) = channel();
            tokio::spawn(async move {
                let _ = tx
                    .send_data(Bytes::from_static(b"data: {\"type\":\"meta\"}\n\n"))
                    .await;
                std::future::pending::<()>().await;
                drop(tx);
            });
            let body = Watched {
                inner: body.boxed(),
                core: Arc::clone(&core),
            };
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache, no-transform")
                .body(body.boxed())
                .unwrap()
        }
        Turn::DieBeforeHeaders => return Err(std::io::Error::other("Core went away")),
        Turn::Hang => {
            let _closed = OnDrop(core.closed.lock().unwrap().take());
            std::future::pending::<()>().await;
            unreachable!()
        }
        Turn::Flood => {
            let (mut tx, body) = channel();
            tokio::spawn(async move {
                let line = format!(
                    "{{\"type\":\"delta\",\"text\":\"{}\"}}\n",
                    "x".repeat(64 * 1024)
                );
                let line = Bytes::from(line);
                while tx.send_data(line.clone()).await.is_ok() {}
            });
            ndjson(&core, body.boxed())
        }
        Turn::SlowJson => {
            let (tx, body) = channel();
            tokio::spawn(async move {
                std::future::pending::<()>().await;
                drop(tx);
            });
            let body = Watched {
                inner: body.boxed(),
                core: Arc::clone(&core),
            };
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .header("cache-control", "no-store")
                .body(body.boxed())
                .unwrap()
        }
    })
}

async fn core() -> Arc<Core> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let core = Arc::new(Core {
        addr: listener.local_addr().unwrap(),
        turn: Mutex::new(Turn::Echo),
        gate: Mutex::new(Gate::Open),
        seen: Mutex::default(),
        stream: Mutex::new(None),
        closed: Mutex::new(None),
        script: Mutex::new((Vec::new(), false)),
        ask_closed: Mutex::new(None),
        ttl_ms: Mutex::new(30_000),
        renew: Mutex::new(Renew::Ok),
        admission_reply: Mutex::new(None),
    });
    let c = Arc::clone(&core);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let c = Arc::clone(&c);
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |r| core_service(Arc::clone(&c), r));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    core
}

struct Front {
    _dir: tempfile::TempDir,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
    core: Arc<Core>,
}

async fn front() -> Front {
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let c = Connection::open(server_store::db_path(&data)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute(
        "INSERT INTO settings VALUES('setup_code_hash', ?1)",
        [server_auth::js::digest(CODE)],
    )
    .unwrap();
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", format!("http://{}", core.addr));
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    env.insert("NOEVIA_START_ID", START.into());
    env.insert("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into());
    let app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    let setup = format!(
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic chat turn password","diaryEnabled":false}}"#
    );
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("POST /api/setup/complete HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\norigin: {ORIGIN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{setup}", setup.len());
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("HTTP/1.1 201"), "{text}");
    let cookies: Vec<String> = text
        .lines()
        .filter_map(|l| {
            l.strip_prefix("set-cookie: ")
                .map(|v| v.split(';').next().unwrap().to_string())
        })
        .collect();
    let csrf = cookies[1].split_once('=').unwrap().1.to_string();
    let jar = cookies.join("; ");
    let user: String = c
        .query_row("SELECT id FROM users", [], |r| r.get(0))
        .unwrap();
    core.seen.lock().unwrap().hits.clear();
    Front {
        _dir: dir,
        addr,
        jar,
        csrf,
        user,
        core,
    }
}

struct Client {
    send: hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    conn: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.conn.abort();
    }
}

/// One request on its own connection; the connection lives as long as the returned client.
async fn call(
    f: &Front,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: Vec<u8>,
    signed_in: bool,
) -> (Response<Incoming>, Client) {
    let stream = TcpStream::connect(f.addr).await.unwrap();
    let (send, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
        .await
        .unwrap();
    let conn = tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut c = Client { send, conn };
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "front.test")
        .header("origin", ORIGIN)
        .header("content-type", "application/json");
    if signed_in {
        b = b.header("cookie", &f.jar).header("x-csrf-token", &f.csrf);
    }
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    let res = c
        .send
        .send_request(b.body(Full::new(Bytes::from(body))).unwrap())
        .await
        .unwrap();
    (res, c)
}

async fn text(res: Response<Incoming>) -> String {
    String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

const TURN: &[u8] = br#"{"chatId":null,"message":"Say hello.","history":[]}"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_reaches_cores_relay_with_the_secret_the_user_and_the_clients_own_request() {
    let f = front().await;
    let forged = [
        ("x-noevia-store-token", "forged"),
        ("x-noevia-chat-user", "someone-else"),
        ("x-noevia-relay-client-ip", "6.6.6.6"),
        ("x-noevia-client-ip", "6.6.6.6"),
        ("x-forwarded-for", "6.6.6.6"),
        ("x-noevia-chat-wire", "sse"),
        (
            "x-noevia-admission-lease",
            "0123456789abcdef0123456789abcdef",
        ),
        ("user-agent", "synthetic-agent"),
    ];
    let (res, _c) = call(
        &f,
        "POST",
        "/api/chat?ignored=1",
        &forged,
        TURN.to_vec(),
        true,
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers().get_all("set-cookie").iter().count(), 2);
    assert_eq!(res.headers()["x-noevia-api"], "1");
    let echo: serde_json::Value = serde_json::from_str(&text(res).await).unwrap();
    assert_eq!(echo["path"], "/internal/chat/turn");
    let h = &echo["headers"];
    assert_eq!(h["x-noevia-store-token"], serde_json::json!([STORE_TOKEN]));
    assert_eq!(h["x-noevia-chat-user"], serde_json::json!([f.user]));
    // Chat port S7: the front asks for its wire, whatever the client sent under that name.
    assert_eq!(h["x-noevia-chat-wire"], serde_json::json!(["ndjson/1"]));
    // The client's address, as the proxy would send it, under the relay's names only (no TRUST_PROXY).
    assert_eq!(
        h["x-noevia-relay-client-ip"],
        serde_json::json!(["127.0.0.1"])
    );
    assert!(h.get("x-noevia-relay-forwarded-for").is_none());
    assert!(
        h.get("x-noevia-client-ip").is_none() && h.get("x-forwarded-for").is_none(),
        "{h}"
    );
    // The client's own request rides along for Core's pipeline: credentials, CSRF, origin, agent, body.
    assert_eq!(h["cookie"], serde_json::json!([f.jar]));
    assert_eq!(h["x-csrf-token"], serde_json::json!([f.csrf]));
    assert_eq!(h["origin"], serde_json::json!([ORIGIN]));
    assert_eq!(h["user-agent"], serde_json::json!(["synthetic-agent"]));
    assert_eq!(h["host"], serde_json::json!(["front.test"]));
    assert_eq!(echo["bodyLen"], TURN.len());
    // Chat port S8: the turn carries the front's admission lease, whatever the client sent.
    assert_eq!(h["x-noevia-admission-lease"], serde_json::json!([LEASE]));
    {
        let seen = f.core.seen.lock().unwrap();
        assert_eq!(
            seen.hits,
            [
                format!("POST {ADMISSION}"),
                "POST /internal/chat/turn".into()
            ]
        );
        assert_eq!(seen.asks, [(r#"{"gate":"chat"}"#.to_owned(), true)]);
    }
    // A WHATWG spelling of the same path is the same route; other methods and paths stay Node's.
    let (res, _c) = call(&f, "POST", "/api/x/../chat", &[], TURN.to_vec(), true).await;
    assert_eq!(res.status(), 200);
    for (method, path) in [
        ("GET", "/api/chat"),
        ("PUT", "/api/chat"),
        ("POST", "/api/chat/"),
        ("POST", "/api/chatx"),
    ] {
        let (res, _c) = call(&f, method, path, &[], Vec::new(), true).await;
        let got = text(res).await;
        assert_eq!(
            got,
            format!(r#"{{"proxied":"{method} {path}"}}"#),
            "{method} {path}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_line_is_flushed_as_its_frame_and_the_client_leaving_closes_cores_socket_within_1_s() {
    let f = Arc::new(front().await);
    *f.core.turn.lock().unwrap() = Turn::Stream;
    let (closed_tx, closed_rx) = oneshot::channel();
    *f.core.closed.lock().unwrap() = Some(closed_tx);
    // The headers wait for the first event: the call is answered once Core sends its first line.
    let pending = tokio::spawn({
        let f = Arc::clone(&f);
        async move { call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await }
    });
    let mut tx = loop {
        if let Some(tx) = f.core.stream.lock().unwrap().take() {
            break tx;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !pending.is_finished(),
        "the headers went out before the first event"
    );
    tx.send_data(Bytes::from_static(b"{\"type\":\"meta\",\"model\":\"m\"}\n"))
        .await
        .unwrap();
    let (res, c) = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    assert_eq!(res.headers()["cache-control"], "no-cache, no-transform");
    assert_eq!(res.headers()["x-accel-buffering"], "no");
    assert_eq!(res.headers()["connection"], "keep-alive");
    // Core's own headers ride along; its type and caching do not.
    assert_eq!(res.headers()["x-noevia-api"], "1");
    assert_eq!(res.headers()["set-cookie"], "a=1; Path=/");
    assert_eq!(res.headers().get_all("cache-control").iter().count(), 1);
    let mut body = res.into_body();
    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        first.into_data().unwrap(),
        Bytes::from_static(b"data: {\"type\":\"meta\",\"model\":\"m\"}\n\n")
    );
    for (line, frame) in [
        (
            "{\"type\":\"delta\",\"text\":\"Hi\"}\n",
            "data: {\"type\":\"delta\",\"text\":\"Hi\"}\n\n",
        ),
        ("{\"type\":\"done\"}\n", "data: {\"type\":\"done\"}\n\n"),
    ] {
        tx.send_data(Bytes::from(line)).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("the frame was buffered, not flushed")
            .unwrap()
            .unwrap();
        assert_eq!(got.into_data().unwrap(), Bytes::from(frame));
    }
    // The browser goes away mid-stream: Core's socket closes (its body is dropped) within a second.
    let left = Instant::now();
    drop(body);
    drop(c);
    let closed = tokio::time::timeout(Duration::from_secs(1), closed_rx)
        .await
        .expect("Core's socket stayed open after the client left")
        .unwrap();
    assert!(closed - left < Duration::from_secs(1));
    // The socket's close is seen a moment before the upstream body's receiver is dropped, so the
    // channel may still take a line or two into its buffer; within the same second it refuses.
    let refused = tokio::time::timeout(Duration::from_secs(1), async {
        while tx
            .send_data(Bytes::from_static(b"{\"type\":\"delta\"}\n"))
            .await
            .is_ok()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        refused.is_ok(),
        "Core's body was still read after the client left"
    );
}

/// The headers on the wire, as a client reads them (chat.cjs's four, and no NDJSON type), sent with
/// the first event: for the routing question's early stream (`route_pending`) the head waits for it.
#[tokio::test(flavor = "multi_thread")]
async fn the_headers_are_chat_cjs_s_and_go_out_with_the_first_event() {
    let f = front().await;
    *f.core.turn.lock().unwrap() = Turn::LateFirstLine;
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    let req = format!(
        "POST /api/chat HTTP/1.1\r\nhost: front.test\r\norigin: {ORIGIN}\r\ncookie: {}\r\nx-csrf-token: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        f.jar,
        f.csrf,
        TURN.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    s.write_all(TURN).await.unwrap();
    let sent = Instant::now();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    let head_at = loop {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0);
        got.extend_from_slice(&buf[..n]);
        if got.windows(4).any(|w| w == b"\r\n\r\n") {
            break sent.elapsed();
        }
    };
    assert!(
        head_at >= Duration::from_millis(1400),
        "the headers went out before the first event ({head_at:?})"
    );
    while !String::from_utf8_lossy(&got).contains("route_pending") {
        let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        got.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&got).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let lines: Vec<String> = head.lines().map(str::to_ascii_lowercase).collect();
    assert_eq!(lines[0], "http/1.1 200 ok");
    for want in [
        "content-type: text/event-stream",
        "cache-control: no-cache, no-transform",
        "x-accel-buffering: no",
        "connection: keep-alive",
        "x-noevia-api: 1",
        "transfer-encoding: chunked",
    ] {
        assert_eq!(
            lines.iter().filter(|l| l.as_str() == want).count(),
            1,
            "{want} in {head}"
        );
    }
    assert!(!head.to_ascii_lowercase().contains("ndjson"), "{head}");
    assert!(!head.to_ascii_lowercase().contains("no-store"), "{head}");
    assert!(
        body.contains("data: {\"type\":\"route_pending\",\"id\":\"r1\",\"flag\":\"health\"}\n\n"),
        "{body}"
    );
}

/// The front's own heartbeat: Core sends no comments, and the client still gets `: keep-alive` every
/// 5 s (at most 5.5 s apart) from the headers on; the client leaving during it closes Core's socket.
#[tokio::test(flavor = "multi_thread")]
async fn the_fronts_keep_alive_reaches_the_client_every_5_s_from_the_headers() {
    let f = front().await;
    *f.core.turn.lock().unwrap() = Turn::Silent;
    let (res, c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    let mut last = Instant::now();
    assert_eq!(res.status(), 200);
    let mut body = res.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(
        first,
        Bytes::from_static(b"data: {\"type\":\"meta\",\"model\":\"m\"}\n\n")
    );
    for _ in 0..2 {
        let frame = tokio::time::timeout(Duration::from_millis(5500), body.frame())
            .await
            .expect("no keep-alive within 5.5 s")
            .unwrap()
            .unwrap();
        assert_eq!(
            frame.into_data().unwrap(),
            Bytes::from_static(b": keep-alive\n\n")
        );
        let gap = last.elapsed();
        assert!(
            gap <= Duration::from_millis(5500) && gap >= Duration::from_millis(4500),
            "{gap:?}"
        );
        last = Instant::now();
    }
    let (tx, rx) = oneshot::channel();
    *f.core.closed.lock().unwrap() = Some(tx);
    let left = Instant::now();
    drop(body);
    drop(c);
    let at = tokio::time::timeout(Duration::from_secs(1), rx)
        .await
        .expect("Core's socket stayed open after the client left")
        .unwrap();
    assert!(at - left < Duration::from_secs(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn cores_503_relays_with_retry_after_and_its_whole_body() {
    let f = front().await;
    *f.core.turn.lock().unwrap() = Turn::Busy;
    let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    assert_eq!(res.status(), 503);
    assert_eq!(res.headers()["retry-after"], "3");
    assert_eq!(res.headers()["content-type"], "application/json");
    assert_eq!(text(res).await, BUSY);
}

#[tokio::test(flavor = "multi_thread")]
async fn core_failing_after_its_headers_ends_with_the_error_frame_and_before_them_is_a_502() {
    let f = front().await;
    *f.core.turn.lock().unwrap() = Turn::DieAfterHeaders;
    let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    assert_eq!(res.status(), 200);
    let got = tokio::time::timeout(Duration::from_secs(5), text(res))
        .await
        .unwrap();
    assert_eq!(
        got,
        // The line Core left unfinished is dropped: the client reads whole frames only.
        format!("data: {{\"type\":\"meta\"}}\n\n{FAILED}")
    );
    *f.core.turn.lock().unwrap() = Turn::DieBeforeHeaders;
    let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    assert_eq!(res.status(), 502);
    assert_eq!(res.headers()["content-type"], "application/json");
    assert_eq!(res.headers()["x-noevia-api"], "1");
    assert_eq!(
        text(res).await,
        r#"{"error":"The server is not answering. Please retry."}"#
    );
}

/// Rollback safety at run time: a Core that writes its own event stream is not this front's pair (the
/// supervisor and assemble-release refuse the mix); its answer is dropped, which stops its turn.
#[tokio::test(flavor = "multi_thread")]
async fn a_core_writing_its_own_event_stream_is_refused_and_its_turn_stopped() {
    let f = front().await;
    *f.core.turn.lock().unwrap() = Turn::OwnEventStream;
    let (tx, rx) = oneshot::channel();
    *f.core.closed.lock().unwrap() = Some(tx);
    let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    assert_eq!(res.status(), 502);
    assert_eq!(
        text(res).await,
        r#"{"error":"The server is not answering. Please retry."}"#
    );
    tokio::time::timeout(Duration::from_secs(1), rx)
        .await
        .expect("the mismatched Core's turn kept running")
        .unwrap();
}

/// noevia-web's src/api.ts at contracts/http/web.ref (`NOEVIA_WEB_API_TS`; CI checks it out). Without it the
/// test says so and passes; with `NOEVIA_WEB_API_TS_REQUIRED` a missing file fails.
fn web_api_ts() -> Option<String> {
    let required = std::env::var("NOEVIA_WEB_API_TS_REQUIRED").is_ok();
    match std::env::var("NOEVIA_WEB_API_TS") {
        Ok(p) if !p.is_empty() && std::path::Path::new(&p).is_file() => Some(p),
        _ => {
            assert!(
                !required,
                "NOEVIA_WEB_API_TS_REQUIRED is set but NOEVIA_WEB_API_TS is not a file"
            );
            eprintln!("SKIPPED: NOEVIA_WEB_API_TS (noevia-web src/api.ts at contracts/http/web.ref) is not set");
            None
        }
    }
}

/// One turn read by the web client's own streamChat (tests/web-stream-chat.cjs): every event it yielded,
/// as JSON text, and what it threw.
async fn web_client(f: &Front, api: &str) -> (Vec<String>, serde_json::Value) {
    let harness = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/web-stream-chat.cjs");
    let base = format!("http://{}", f.addr);
    let (jar, csrf, api) = (f.jar.clone(), f.csrf.clone(), api.to_owned());
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(std::env::var("NOEVIA_NODE").unwrap_or_else(|_| "node".into()))
            .args(["--no-warnings", harness, &api, &base, ORIGIN, &jar, &csrf])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let events = got["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap().to_owned())
        .collect();
    (events, got["error"].clone())
}

/// Review P2 (chat port S5): Core breaking inside a line must reach the web client as the failure event,
/// read by the client's own stream reader.
#[tokio::test(flavor = "multi_thread")]
async fn a_break_inside_a_line_reaches_the_web_client_as_the_failure_event() {
    let Some(api) = web_api_ts() else { return };
    let f = front().await;
    *f.core.turn.lock().unwrap() = Turn::DieAfterHeaders;
    let (events, error) = web_client(&f, &api).await;
    assert_eq!(error, serde_json::Value::Null, "the client failed");
    assert_eq!(
        events,
        [
            r#"{"type":"meta"}"#,
            r#"{"type":"error","text":"The request could not be completed. Please retry."}"#
        ]
    );
    // Chat port S6: Core's gate is taken for the call first, and the turn carries its hold.
    assert_eq!(
        f.core.seen.lock().unwrap().hits,
        [
            format!("POST {ADMISSION}"),
            "POST /internal/chat/turn".into()
        ]
    );
}

// --- Core's own wire (tests/fixtures/node-chat-wire.v1.json, recorded by tests/chat-wire-oracle.cjs from core's
// rust-chat-wire.cjs at front-core.ref; CI regenerates it). ------------------------------------------------------

fn wire_fixture() -> serde_json::Value {
    let path = std::env::var("NOEVIA_CHAT_WIRE_FIXTURE").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/node-chat-wire.v1.json"
        )
        .to_owned()
    });
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The front's chunking of Core's NDJSON: whole, one byte at a time, and odd sizes that split lines.
fn chunked(ndjson: &str, size: usize) -> Vec<Bytes> {
    let b = ndjson.as_bytes();
    if size == 0 {
        return vec![Bytes::copy_from_slice(b)];
    }
    b.chunks(size).map(Bytes::copy_from_slice).collect()
}

/// The `type` of each event frame in `stream` (every event JSON.stringify writes starts with it).
fn event_types(stream: &str) -> Vec<String> {
    stream
        .split("\n\n")
        .filter_map(|f| f.lines().find_map(|l| l.strip_prefix("data: ")))
        .map(|d| {
            let rest = d
                .strip_prefix("{\"type\":\"")
                .unwrap_or_else(|| panic!("{d}"));
            rest[..rest.find('"').unwrap()].to_owned()
        })
        .collect()
}

/// Contract §1.3's order, as the client receives it.
fn assert_section_1_3_order(name: &str, types: &[String]) {
    let pos = |t: &str| types.iter().position(|x| x == t);
    let start = if types.first().map(String::as_str) == Some("route_pending") {
        // The early stream: the question (and a remembered answer) come before meta.
        let meta = pos("meta").unwrap_or_else(|| panic!("{name}: no meta after route_pending"));
        assert!(
            types[1..meta].iter().all(|t| t == "route_remembered"),
            "{name}: {types:?}"
        );
        meta
    } else {
        0
    };
    // The routing question is answered before meta is sent; a Diary turn's companion stream has no meta.
    let diary = types.first().map(String::as_str) == Some("status");
    if !diary {
        assert_eq!(types[start], "meta", "{name}: meta first, {types:?}");
    }
    // One completion: done or error. After done only the title; after error nothing.
    let end = types
        .iter()
        .position(|t| t == "done" || t == "error")
        .unwrap_or_else(|| panic!("{name}: no done or error, {types:?}"));
    let after = &types[end + 1..];
    if types[end] == "done" {
        assert!(
            after.iter().all(|t| t == "title"),
            "{name}: after done {after:?}"
        );
    } else {
        assert!(after.is_empty(), "{name}: after error {after:?}");
    }
    let body = &types[start..end];
    let first = |t: &str| body.iter().position(|x| x == t);
    // meta, then sources, then telemetry waiting; context before generating; complete just before done.
    // The first sources (item 2) precede telemetry waiting; a re-sent one (item 10) comes after the rounds.
    let output = ["delta", "reasoning", "tool", "preamble"]
        .iter()
        .filter_map(|t| first(t))
        .min();
    if let (Some(s), Some(w)) = (first("sources"), first("telemetry")) {
        if output.is_none_or(|o| s < o) {
            assert!(s < w, "{name}: sources before the first telemetry");
        }
    }
    // The context meter comes before any output of the model.
    if let Some(c) = first("context") {
        for t in ["delta", "reasoning", "tool", "preamble"] {
            assert!(first(t).is_none_or(|x| c < x), "{name}: context before {t}");
        }
    }
    for (i, t) in body.iter().enumerate() {
        match t.as_str() {
            "tool_pending" => assert!(
                body[..i].contains(&"tool".to_owned()),
                "{name}: tool before tool_pending"
            ),
            "tool_result" => assert!(
                body[..i].contains(&"tool".to_owned()),
                "{name}: tool before tool_result"
            ),
            "paused" => assert!(
                body[i + 1..].iter().all(|x| x == "telemetry"),
                "{name}: paused ends the rounds"
            ),
            _ => {}
        }
    }
    if types[end] == "done" && !diary && body.len() > 4 {
        assert_eq!(
            body.last().map(String::as_str),
            Some("telemetry"),
            "{name}: telemetry complete before done"
        );
    }
}

/// Byte for byte: for each turn core's rust-chat-wire.cjs writes, the front's event stream from Core's NDJSON
/// is the event stream Core itself writes (and after a break, Core's stream then the failure frame), however
/// Core's lines are split across reads; and every case keeps §1.3's order.
#[tokio::test(flavor = "multi_thread")]
async fn the_fronts_stream_is_cores_own_byte_for_byte_for_every_recorded_turn() {
    let fixture = wire_fixture();
    assert_eq!(fixture["wireHeader"], "x-noevia-chat-wire");
    assert_eq!(fixture["wire"], "ndjson/1");
    assert_eq!(fixture["ndjsonType"], "application/x-ndjson");
    assert_eq!(fixture["keepAlive"], ": keep-alive\n\n");
    assert_eq!(fixture["heartbeatMs"], 5000);
    let f = front().await;
    *f.core.turn.lock().unwrap() = Turn::Script;
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 10);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        assert_eq!(case["sseStatus"], 200);
        assert_eq!(
            case["sseHeaders"],
            serde_json::json!({"Content-Type":"text/event-stream","Cache-Control":"no-cache, no-transform","X-Accel-Buffering":"no","Connection":"keep-alive"}),
            "{name}"
        );
        assert_eq!(
            case["ndjsonHeaders"]["Content-Type"],
            "application/x-ndjson"
        );
        let broken = case["end"] == "break";
        let mut want = case["sse"].as_str().unwrap().to_owned();
        if broken {
            want.push_str(case["failed"].as_str().unwrap());
        }
        assert_eq!(case["failed"], FAILED);
        for size in [0, 1, 7, 64] {
            *f.core.script.lock().unwrap() =
                (chunked(case["ndjson"].as_str().unwrap(), size), broken);
            let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
            assert_eq!(res.status(), 200, "{name}");
            for (k, v) in case["sseHeaders"].as_object().unwrap() {
                assert_eq!(res.headers()[k.as_str()], v.as_str().unwrap(), "{name} {k}");
            }
            let got = tokio::time::timeout(Duration::from_secs(10), text(res))
                .await
                .unwrap();
            assert_eq!(got, want, "{name}, chunks of {size}");
        }
        assert_section_1_3_order(name, &event_types(&want));
        let types: Vec<String> = case["types"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect();
        if !types.contains(&"raw".to_owned()) {
            let mut sent = types.clone();
            if broken || case["end"] == "failed" {
                sent.push("error".into());
            }
            assert_eq!(event_types(&want), sent, "{name}");
        }
    }
}

/// The web client's real streamChat parses every recorded turn through the front, and yields exactly the
/// events a reader of Core's own stream gets.
#[tokio::test(flavor = "multi_thread")]
async fn the_web_client_reads_every_recorded_turn_through_the_front() {
    let Some(api) = web_api_ts() else { return };
    let fixture = wire_fixture();
    let f = front().await;
    *f.core.turn.lock().unwrap() = Turn::Script;
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        *f.core.script.lock().unwrap() = (
            chunked(case["ndjson"].as_str().unwrap(), 5),
            case["end"] == "break",
        );
        let (events, error) = web_client(&f, &api).await;
        assert_eq!(error, serde_json::Value::Null, "{name}: the client failed");
        let want: Vec<String> = case["client"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(events, want, "{name}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_request_meets_cores_gate_first_as_node_answered_it() {
    let f = front().await;
    // Signed out, and signed in without CSRF: Node's gate ran before authentication.
    for (signed_in, extra, status, body) in [
        (false, vec![], 401, r#"{"error":"unauthorized"}"#),
        (
            true,
            vec![("x-csrf-token", "wrong")],
            403,
            r#"{"error":"invalid CSRF token"}"#,
        ),
    ] {
        *f.core.gate.lock().unwrap() = Gate::Open;
        f.core.seen.lock().unwrap().hits.clear();
        let mut c = Vec::new();
        let (res, client) = call(&f, "POST", "/api/chat", &extra, TURN.to_vec(), signed_in).await;
        c.push(client);
        assert_eq!(res.status(), status);
        assert_eq!(text(res).await, body);
        {
            // The gate taken and given straight back, before the refusal: Node's order.
            let seen = f.core.seen.lock().unwrap();
            assert_eq!(seen.hits, [format!("POST {ADMISSION}")]);
            assert_eq!(
                seen.asks.last().unwrap(),
                &(r#"{"gate":"chat"}"#.to_owned(), true)
            );
            assert_eq!(seen.live, 0, "given back before the answer");
        }
        *f.core.gate.lock().unwrap() = Gate::Held;
        let (res, _c) = call(&f, "POST", "/api/chat", &extra, TURN.to_vec(), signed_in).await;
        assert_eq!(res.status(), 503);
        assert_eq!(res.headers()["retry-after"], "3");
        assert_eq!(text(res).await, BUSY);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_at_the_cap_reaches_core_whole_and_one_byte_more_is_the_fronts_413() {
    // Chat port S6: the front reads the body (chat_edge.rs); Core gets it whole or not at all.
    let f = front().await;
    let cap = noevia_server::chat_turn::STORED_HISTORY_BYTES as usize;
    let body = |len: usize| format!("{{\"message\":\"{}\"}}", "a".repeat(len - 14)).into_bytes();
    let (res, _c) = call(&f, "POST", "/api/chat", &[], body(cap), true).await;
    assert_eq!(res.status(), 200);
    let echo: serde_json::Value = serde_json::from_str(&text(res).await).unwrap();
    assert_eq!(echo["bodyLen"], cap);
    for len in [cap + 1, cap + 4096] {
        f.core.seen.lock().unwrap().hits.clear();
        let (res, _c) = call(&f, "POST", "/api/chat", &[], body(len), true).await;
        assert_eq!(res.status(), 413, "{len}");
        assert_eq!(
            text(res).await,
            r#"{"error":"This chat is too large to continue; start a new chat."}"#
        );
        assert_eq!(
            f.core.seen.lock().unwrap().hits,
            [format!("POST {ADMISSION}")]
        );
    }
}

// Chat port S8 (chat_admission.rs, `rust-chat-admission`): the front takes Core's inference gate with an
// admission lease; the lease is the turn's one gate count, held until the answer has ended.

/// Waits (at most 2 s) until `ok` holds for what Core saw.
async fn until(f: &Front, what: &str, ok: impl Fn(&Seen) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ok(&f.core.seen.lock().unwrap()) {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_held_gate_is_answered_by_the_front_as_index_cjs_answered_it_and_no_turn_runs() {
    // chat-admission-1896: the gate's refusal after its bounded wait, retryable, with its words and code.
    let f = front().await;
    *f.core.gate.lock().unwrap() = Gate::Held;
    let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    assert_eq!(res.status(), 503);
    assert_eq!(res.headers()["retry-after"], "3");
    assert_eq!(res.headers()["x-noevia-api"], "1");
    assert_eq!(res.headers()["content-type"], "application/json");
    assert_eq!(res.headers()["cache-control"], "no-store");
    assert_eq!(text(res).await, BUSY);
    let seen = f.core.seen.lock().unwrap();
    assert_eq!(
        seen.hits,
        [format!("POST {ADMISSION}")],
        "no turn, nothing else"
    );
    assert_eq!(seen.live, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_call_is_never_counted_on_the_budget() {
    // Node's order: the gate first, then the `llm:` budget. With a limit of 60, sixty refusals and then a
    // turn: the turn is let in (none of the refusals was counted).
    let f = front().await;
    *f.core.gate.lock().unwrap() = Gate::Held;
    for _ in 0..60 {
        let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
        assert_eq!(res.status(), 503);
    }
    *f.core.gate.lock().unwrap() = Gate::Open;
    let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    assert_eq!(res.status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_lease_is_the_turns_one_count_until_its_answer_ends_then_it_is_given_back() {
    // chat-gate-lease-2115 as the front holds it: one lease for the whole turn, never a second ask, and
    // given back once the turn has ended.
    let f = Arc::new(front().await);
    *f.core.turn.lock().unwrap() = Turn::Stream;
    let pending = tokio::spawn({
        let f = Arc::clone(&f);
        async move { call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await }
    });
    let mut tx = loop {
        if let Some(tx) = f.core.stream.lock().unwrap().take() {
            break tx;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    tx.send_data(Bytes::from_static(b"{\"type\":\"meta\",\"model\":\"m\"}\n"))
        .await
        .unwrap();
    let (res, _c) = pending.await.unwrap();
    let mut body = res.into_body();
    body.frame().await.unwrap().unwrap();
    for line in [
        "{\"type\":\"delta\",\"text\":\"Hi\"}\n",
        "{\"type\":\"done\"}\n",
        "{\"type\":\"title\",\"chatId\":\"c\",\"title\":\"T\"}\n",
    ] {
        tx.send_data(Bytes::from(line)).await.unwrap();
        body.frame().await.unwrap().unwrap();
        let seen = f.core.seen.lock().unwrap();
        assert_eq!(seen.live, 1, "held while the turn runs, the title included");
        assert!(seen.releases.iter().all(|r| !r.starts_with("DELETE")));
        assert_eq!(seen.asks.len(), 1, "one lease for the whole turn");
    }
    drop(tx);
    while body.frame().await.is_some() {}
    until(&f, "given back once the answer ended", |s| s.live == 0).await;
    assert_eq!(
        f.core.seen.lock().unwrap().releases,
        [format!("DELETE {ADMISSION}/{LEASE}")]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_client_leaving_mid_turn_gives_the_lease_back_within_1_s() {
    // chat-gate-lease-2115's Stop: however the turn ends, its lease is given back.
    let f = front().await;
    *f.core.turn.lock().unwrap() = Turn::Silent;
    let (res, c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    let mut body = res.into_body();
    body.frame().await.unwrap().unwrap();
    assert_eq!(f.core.seen.lock().unwrap().live, 1);
    drop(body);
    drop(c);
    let left = Instant::now();
    until(&f, "the lease outlived the client", |s| s.live == 0).await;
    assert!(left.elapsed() < Duration::from_secs(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_client_leaving_during_the_wait_ends_nodes_wait_and_holds_nothing() {
    // chat-admission-1896: the wait ends when the client goes away. Core sees its ask closed, no turn and no
    // budget charge follow, and nothing is held.
    let f = front().await;
    *f.core.gate.lock().unwrap() = Gate::Waiting;
    let (closed_tx, closed_rx) = oneshot::channel();
    *f.core.ask_closed.lock().unwrap() = Some(closed_tx);
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    let req = format!(
        "POST /api/chat HTTP/1.1\r\nhost: front.test\r\norigin: {ORIGIN}\r\ncookie: {}\r\nx-csrf-token: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        f.jar,
        f.csrf,
        TURN.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    s.write_all(TURN).await.unwrap();
    until(&f, "the front asked for the gate", |seen| {
        seen.asks.len() == 1
    })
    .await;
    drop(s);
    let left = Instant::now();
    let at = tokio::time::timeout(Duration::from_secs(1), closed_rx)
        .await
        .expect("Node kept waiting after the client left")
        .unwrap();
    assert!(at - left < Duration::from_secs(1));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let seen = f.core.seen.lock().unwrap();
    assert_eq!(seen.hits, [format!("POST {ADMISSION}")], "no turn");
    assert_eq!(seen.live, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_core_that_does_not_know_the_gate_ask_is_the_proxys_502_and_no_turn_runs() {
    // A Core that is not this front's pair (an S7 Core answers `{gate}` with 400 `A model is required.`)
    // never lets a turn through, signed in or not.
    let f = front().await;
    *f.core.gate.lock().unwrap() = Gate::OldCore;
    let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    assert_eq!(res.status(), 502);
    assert_eq!(
        text(res).await,
        r#"{"error":"The server is not answering. Please retry."}"#
    );
    assert_eq!(
        f.core.seen.lock().unwrap().hits,
        [format!("POST {ADMISSION}")]
    );
    // A refusal of the front's own stands when the gate gives no answer of its own.
    let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), false).await;
    assert_eq!(res.status(), 401);
}

// Review P2-1: a lease Node ended (or stopped hearing renewals for) stops the call it covered, while
// Core works and while its events stream: Core no longer counts it, so maintenance may begin.

/// A front whose grants have a short TTL (renewed every 100 ms) that Node stops confirming.
async fn losing_front() -> Front {
    let f = front().await;
    *f.core.ttl_ms.lock().unwrap() = 300;
    *f.core.renew.lock().unwrap() = Renew::Gone;
    f
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lease_lost_while_core_works_is_the_gates_503_and_cores_socket_closes() {
    let f = losing_front().await;
    *f.core.turn.lock().unwrap() = Turn::Hang;
    let (closed_tx, closed_rx) = oneshot::channel();
    *f.core.closed.lock().unwrap() = Some(closed_tx);
    let (res, _c) = tokio::time::timeout(
        Duration::from_secs(5),
        call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true),
    )
    .await
    .expect("the call outlived its lease");
    assert_eq!(res.status(), 503);
    assert_eq!(res.headers()["retry-after"], "3");
    assert_eq!(
        text(res).await,
        r#"{"error":"Model configuration is being applied. Try again shortly.","code":"inferenceMaintenance","retryable":true,"retryAfter":3}"#
    );
    tokio::time::timeout(Duration::from_secs(1), closed_rx)
        .await
        .expect("Core's turn kept running")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lease_lost_mid_stream_ends_the_turn_with_the_failure_frame_and_closes_cores_socket() {
    let f = losing_front().await;
    *f.core.turn.lock().unwrap() = Turn::Silent;
    let (closed_tx, closed_rx) = oneshot::channel();
    *f.core.closed.lock().unwrap() = Some(closed_tx);
    let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    assert_eq!(res.status(), 200);
    let got = tokio::time::timeout(Duration::from_secs(5), text(res))
        .await
        .expect("the stream outlived its lease");
    assert!(got.starts_with("data: {\"type\":\"meta\""), "{got}");
    assert!(got.ends_with(FAILED), "{got}");
    tokio::time::timeout(Duration::from_secs(1), closed_rx)
        .await
        .expect("Core's turn kept running")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lease_node_keeps_confirming_lets_a_long_turn_run() {
    // Control: renewed (every 100 ms) and confirmed, the same silent turn is still streaming after 1 s.
    let f = front().await;
    *f.core.ttl_ms.lock().unwrap() = 300;
    *f.core.turn.lock().unwrap() = Turn::Silent;
    let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    let mut body = res.into_body();
    body.frame().await.unwrap().unwrap();
    let next = tokio::time::timeout(Duration::from_secs(1), body.frame()).await;
    assert!(
        next.is_err(),
        "the turn ended though its lease was renewed: {next:?}"
    );
    assert!(f
        .core
        .seen
        .lock()
        .unwrap()
        .releases
        .iter()
        .any(|r| r.ends_with("/renew")));
}

// S8 re-review: loss is seen whatever the client does, and in time.

/// A front whose grants have a 600 ms TTL and whose renewals Node never answers.
async fn unheard_front() -> Front {
    let f = front().await;
    *f.core.ttl_ms.lock().unwrap() = 600;
    *f.core.renew.lock().unwrap() = Renew::Hang;
    f
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_stopped_reading_does_not_keep_a_turn_whose_lease_was_lost() {
    // P2-2: the client reads nothing at all; Core floods its events, which back up. Losing the lease
    // still closes Core's socket at once.
    let f = losing_front().await;
    *f.core.turn.lock().unwrap() = Turn::Flood;
    let (closed_tx, closed_rx) = oneshot::channel();
    *f.core.closed.lock().unwrap() = Some(closed_tx);
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    let req = format!(
        "POST /api/chat HTTP/1.1\r\nhost: front.test\r\norigin: {ORIGIN}\r\ncookie: {}\r\nx-csrf-token: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        f.jar,
        f.csrf,
        TURN.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    s.write_all(TURN).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), closed_rx)
        .await
        .expect("a stalled reader kept Core's turn running after the lease was lost")
        .unwrap();
    drop(s);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_json_answer_whose_lease_is_lost_ends_with_an_error_before_nodes_ttl() {
    // The JSON path, with renewals Node never answers: the loss is seen before Node's TTL ends.
    let f = unheard_front().await;
    *f.core.turn.lock().unwrap() = Turn::SlowJson;
    let (closed_tx, closed_rx) = oneshot::channel();
    *f.core.closed.lock().unwrap() = Some(closed_tx);
    let started = Instant::now();
    let (res, _c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "application/json");
    let read = tokio::time::timeout(Duration::from_secs(3), res.into_body().collect())
        .await
        .expect("the JSON answer outlived its lease");
    assert!(read.is_err(), "the answer ended as if complete: {read:?}");
    tokio::time::timeout(Duration::from_secs(1), closed_rx)
        .await
        .expect("Core's call kept running")
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lease_lost_after_cores_headers_and_before_its_first_event_is_the_gates_503() {
    // Core's NDJSON headers at once and its first event after 1.5 s; Node never answers the renewals,
    // so the lease is lost (before its 600 ms TTL) while the front waits for that event.
    let f = unheard_front().await;
    *f.core.turn.lock().unwrap() = Turn::LateFirstLine;
    let (closed_tx, closed_rx) = oneshot::channel();
    *f.core.closed.lock().unwrap() = Some(closed_tx);
    let (res, _c) = tokio::time::timeout(
        Duration::from_secs(3),
        call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true),
    )
    .await
    .unwrap();
    assert_eq!(
        res.status(),
        503,
        "the first event went out under a lost lease"
    );
    assert_eq!(res.headers()["retry-after"], "3");
    tokio::time::timeout(Duration::from_secs(1), closed_rx)
        .await
        .expect("Core's turn kept running")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_70042_byte_refusal_keeps_its_complete_message_code_and_retry_hints() {
    for chunked in [false, true] {
        let f = front().await;
        let reason = "r".repeat(70_000);
        let refusal = format!(r#"{{"error":"{reason}","code":"inferenceMaintenance"}}"#);
        assert_eq!(refusal.len(), 70_042);
        *f.core.admission_reply.lock().unwrap() = Some((503, refusal, None, None, chunked));
        let (res, _client) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
        assert_eq!(res.status(), 503, "chunked {chunked}");
        assert_eq!(res.headers()["retry-after"], "3");
        let got: serde_json::Value = serde_json::from_str(&text(res).await).unwrap();
        assert_eq!(got["error"], reason);
        assert_eq!(got["code"], "inferenceMaintenance");
        assert_eq!(got["retryable"], true);
        assert_eq!(got["retryAfter"], 3);
        assert!(f
            .core
            .seen
            .lock()
            .unwrap()
            .hits
            .iter()
            .all(|h| !h.contains("/internal/chat/turn")));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_over_one_mib_refusal_keeps_status_header_code_and_retry_semantics() {
    for chunked in [false, true] {
        let f = front().await;
        let reason = "r".repeat(1024 * 1024);
        let refusal = format!(r#"{{"error":"{reason}","code":"inferenceMaintenance"}}"#);
        *f.core.admission_reply.lock().unwrap() = Some((
            503,
            refusal,
            Some("11".into()),
            Some("engineHeld".into()),
            chunked,
        ));
        let (res, _client) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
        assert_eq!(res.status(), 503, "chunked {chunked}");
        assert_eq!(res.headers()["retry-after"], "11");
        let got: serde_json::Value = serde_json::from_str(&text(res).await).unwrap();
        assert_eq!(
            got["error"],
            "Model configuration is being applied. Try again shortly."
        );
        assert_eq!(got["code"], "engineHeld");
        assert_eq!(got["retryable"], true);
        assert_eq!(got["retryAfter"], 11);
        assert!(f
            .core
            .seen
            .lock()
            .unwrap()
            .hits
            .iter()
            .all(|h| !h.contains("/internal/chat/turn")));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_over_one_mib_non_refusal_fails_closed() {
    for chunked in [false, true] {
        let f = front().await;
        *f.core.admission_reply.lock().unwrap() =
            Some((200, " ".repeat(1024 * 1024 + 1), None, None, chunked));
        let (res, _client) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
        assert_eq!(res.status(), 502, "chunked {chunked}");
        assert!(f
            .core
            .seen
            .lock()
            .unwrap()
            .hits
            .iter()
            .all(|h| !h.contains("/internal/chat/turn")));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lease_lost_while_the_client_stalls_its_upload_is_the_gates_503_at_once() {
    // Review P2 (S5–S9): a body that stops arriving part-way is read under the lease's loss too.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let f = losing_front().await;
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    let head = format!(
        "POST /api/chat HTTP/1.1\r\nhost: front.test\r\norigin: {ORIGIN}\r\ncontent-type: application/json\r\ncookie: {}\r\nx-csrf-token: {}\r\ncontent-length: {}\r\n\r\n",
        f.jar,
        f.csrf,
        TURN.len() + 64
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(&TURN[..TURN.len() / 2]).await.unwrap();
    // ... and the rest never comes.
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    tokio::time::timeout(Duration::from_secs(5), async {
        while !got.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = s.read(&mut buf).await.unwrap();
            assert!(n > 0, "the front closed without answering");
            got.extend_from_slice(&buf[..n]);
        }
    })
    .await
    .expect("a stalled upload kept the request alive after its lease was lost");
    let head = String::from_utf8_lossy(&got);
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(
        !f.core
            .seen
            .lock()
            .unwrap()
            .hits
            .iter()
            .any(|h| h.contains("/internal/chat/")),
        "a turn reached Core"
    );
}
