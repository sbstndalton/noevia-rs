//! `POST /api/chat` through the front (chat port S5, `rust-chat-turn`): the session gate, then the request
//! unread to Core's loopback `/internal/chat/turn`, and Core's answer back byte for byte and frame by frame.
//! Core is a loopback stand-in here that checks what the front sent and answers as Core's pipeline would (the
//! SSE stream with its keep-alive comments, the gate's 503, a crash mid-stream). No model runs; synthetic
//! data only. The real Core behind the same relay is core's rust-chat-turn.test.cjs and the corpus replay.
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
    /// SSE whose frames the test sends.
    Stream,
    /// SSE with Core's 5 s `: keep-alive` heartbeat and nothing else.
    Heartbeat,
    /// The gate's 503.
    Busy,
    /// Headers and one frame, then Core goes away.
    DieAfterHeaders,
    /// Core goes away before its headers.
    DieBeforeHeaders,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Gate {
    Open,
    Held,
}

#[derive(Default)]
struct Seen {
    /// "METHOD path" of every call.
    hits: Vec<String>,
    /// Headers of the last call.
    headers: HashMap<String, Vec<String>>,
    body_len: usize,
}

struct Core {
    addr: SocketAddr,
    turn: Mutex<Turn>,
    gate: Mutex<Gate>,
    seen: Mutex<Seen>,
    stream: Mutex<Option<http_body_util::channel::Sender<Bytes, std::io::Error>>>,
    /// Fires when a write to the client's stream failed: the front closed Core's socket.
    closed: Mutex<Option<oneshot::Sender<Instant>>>,
}

fn sse(status: u16, body: UpBody) -> Response<UpBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .header("connection", "keep-alive")
        .header("x-noevia-api", "1")
        .body(body)
        .unwrap()
}

async fn core_service(
    core: Arc<Core>,
    req: Request<Incoming>,
) -> Result<Response<UpBody>, std::io::Error> {
    let path = req.uri().path().to_string();
    let hold = req.uri().query() == Some("hold=1");
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
    {
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
    if path == "/internal/chat/gate" {
        return Ok(match *core.gate.lock().unwrap() {
            // Chat port S6: the front takes the gate for its call and holds it while this stays open.
            Gate::Open if hold => {
                let (mut tx, body) =
                    http_body_util::channel::Channel::<Bytes, std::io::Error>::new(1);
                tokio::spawn(async move {
                    let _ = tx.send_data(Bytes::from_static(b"held\n")).await;
                    while tx.send_data(Bytes::new()).await.is_ok() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                });
                Response::builder()
                    .status(200)
                    .header("x-noevia-gate-lease", "lease-1")
                    .body(body.boxed())
                    .unwrap()
            }
            Gate::Open => Response::builder()
                .status(204)
                .body(Full::new(Bytes::new()).map_err(|e| match e {}).boxed())
                .unwrap(),
            Gate::Held => {
                let mut r = json(503, BUSY.to_owned());
                r.headers_mut().insert("retry-after", "3".parse().unwrap());
                r
            }
        });
    }
    if path != "/internal/chat/turn" {
        return Ok(json(200, format!(r#"{{"proxied":"{method} {path}"}}"#)));
    }
    let mode = *core.turn.lock().unwrap();
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
        Turn::Stream | Turn::DieAfterHeaders => {
            let (mut tx, body) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(1);
            if mode == Turn::DieAfterHeaders {
                tokio::spawn(async move {
                    let _ = tx
                        .send_data(Bytes::from_static(b"data: {\"type\":\"meta\"}\n\n"))
                        .await;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let _ = tx
                        .send_data(Bytes::from_static(b"data: {\"type\":\"del"))
                        .await;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    tx.abort(std::io::Error::other("Core went away"));
                });
            } else {
                *core.stream.lock().unwrap() = Some(tx);
            }
            sse(200, body.boxed())
        }
        Turn::Heartbeat => {
            let (mut tx, body) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(1);
            let core = Arc::clone(&core);
            tokio::spawn(async move {
                // chat.cjs: setInterval(() => res.write(': keep-alive\n\n'), 5000) from the headers on.
                let mut beat = tokio::time::interval_at(
                    tokio::time::Instant::now() + Duration::from_secs(5),
                    Duration::from_secs(5),
                );
                loop {
                    beat.tick().await;
                    if tx
                        .send_data(Bytes::from_static(b": keep-alive\n\n"))
                        .await
                        .is_err()
                    {
                        if let Some(s) = core.closed.lock().unwrap().take() {
                            let _ = s.send(Instant::now());
                        }
                        return;
                    }
                }
            });
            sse(200, body.boxed())
        }
        Turn::DieBeforeHeaders => return Err(std::io::Error::other("Core went away")),
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
    assert_eq!(h["x-noevia-gate-lease"], serde_json::json!(["lease-1"]));
    assert_eq!(
        f.core.seen.lock().unwrap().hits,
        ["POST /internal/chat/gate", "POST /internal/chat/turn"]
    );
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
async fn the_event_stream_flushes_frame_by_frame_and_the_client_leaving_closes_cores_socket() {
    let f = front().await;
    *f.core.turn.lock().unwrap() = Turn::Stream;
    let (res, c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    assert_eq!(res.headers()["cache-control"], "no-cache, no-transform");
    assert_eq!(res.headers()["x-accel-buffering"], "no");
    let mut body = res.into_body();
    let mut tx = f.core.stream.lock().unwrap().take().unwrap();
    for frame in [
        "data: {\"type\":\"meta\",\"model\":\"m\"}\n\n",
        ": keep-alive\n\n",
        "data: {\"type\":\"delta\",\"text\":\"Hi\"}\n\n",
    ] {
        tx.send_data(Bytes::from(frame)).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("the frame was buffered, not flushed")
            .unwrap()
            .unwrap();
        assert_eq!(got.into_data().unwrap(), Bytes::from(frame));
    }
    // The browser goes away mid-stream: Core's next write fails within a second.
    let left = Instant::now();
    drop(body);
    drop(c);
    let closed = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tx
                .send_data(Bytes::from_static(
                    b"data: {\"type\":\"delta\",\"text\":\"more\"}\n\n",
                ))
                .await
                .is_err()
            {
                return Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Core's socket stayed open after the client left");
    assert!(closed - left < Duration::from_secs(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn keep_alive_comments_reach_the_client_at_most_5_5_s_apart() {
    let f = front().await;
    *f.core.turn.lock().unwrap() = Turn::Heartbeat;
    let (res, c) = call(&f, "POST", "/api/chat", &[], TURN.to_vec(), true).await;
    let mut last = Instant::now();
    assert_eq!(res.status(), 200);
    let mut body = res.into_body();
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
        assert!(
            last.elapsed() <= Duration::from_millis(5500),
            "{:?}",
            last.elapsed()
        );
        last = Instant::now();
    }
    // And a client that leaves during the heartbeat closes Core's socket: its next beat fails.
    let (tx, rx) = oneshot::channel();
    *f.core.closed.lock().unwrap() = Some(tx);
    drop(body);
    drop(c);
    tokio::time::timeout(Duration::from_secs(6), rx)
        .await
        .unwrap()
        .unwrap();
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
        // The frame Core left unfinished is dropped: the client reads whole frames only.
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

/// Review P2 (chat port S5): Core breaking inside a frame must reach the web client as the failure event,
/// read by the client's own stream reader (tests/web-stream-chat.cjs runs noevia-web's streamChat).
#[tokio::test(flavor = "multi_thread")]
async fn a_break_inside_a_frame_reaches_the_web_client_as_the_failure_event() {
    let Some(api) = web_api_ts() else { return };
    let f = front().await;
    *f.core.turn.lock().unwrap() = Turn::DieAfterHeaders;
    let harness = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/web-stream-chat.cjs");
    let base = format!("http://{}", f.addr);
    let (jar, csrf) = (f.jar.clone(), f.csrf.clone());
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
    assert_eq!(
        got["error"],
        serde_json::Value::Null,
        "the client failed: {got}"
    );
    assert_eq!(
        got["events"],
        serde_json::json!([
            {"type": "meta"},
            {"type": "error", "text": "The request could not be completed. Please retry."}
        ])
    );
    // Chat port S6: Core's gate is taken for the call first, and the turn carries its hold.
    assert_eq!(
        f.core.seen.lock().unwrap().hits,
        ["POST /internal/chat/gate", "POST /internal/chat/turn"]
    );
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
            let seen = f.core.seen.lock().unwrap();
            assert_eq!(seen.hits, ["POST /internal/chat/gate"]);
            assert_eq!(seen.headers["x-noevia-store-token"], [STORE_TOKEN]);
            assert!(
                !seen.headers.contains_key("x-noevia-chat-user")
                    && !seen.headers.contains_key("cookie")
            );
            assert_eq!(seen.body_len, 0);
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
            ["POST /internal/chat/gate"]
        );
    }
}
