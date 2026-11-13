//! Track R8 end to end: `POST /api/chat` through a real front whose executor switch is `native`, against
//! the recorded Node turns (`crates/chat-turn/tests/fixtures/node-turns.v1.json`). A fake Core on a local
//! port answers the turn with the recorded hand-off (`handoff.raw`, its `prepared` frame pointed at a fake
//! engine), the loopback bridges and the admission latch from the recording; the fake engine answers each
//! round with the recorded bytes. The client's SSE frames, the engine request bodies and the account's
//! state files must be the recording's.
//!
//! Clock and ids: the front runs on the real clock and real uuids, the recording on a fixed one. Before
//! comparing, a time field the front wrote (`updatedAt`, `observedAt`, `compactedAt`, `savedAt`) that lies
//! inside this test's run is read as the recording's fixed instant, a `timeToFirstToken`/`elapsedMs` under
//! a minute as 0, an `ap-<uuid v4>` id as the recorded one, and the signed-in user's id as `qa-user`.
//! Nothing else is rewritten. Synthetic data only; local ports only; no model runs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use bytes::Bytes;
use chat_turn::js;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use js_json::{wtf16, JValue};
use noevia_server::chat_native::Approvals;
use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use server_store::rusqlite::Connection;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const FIXTURE: &str = include_str!("../../../crates/chat-turn/tests/fixtures/node-turns.v1.json");
const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-chat-native-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const FIXED_NOW: f64 = 1_794_571_200_000.0;
const LEASE: &str = "qalease1";

/// The cases run end to end.
const CASES: &[&str] = &[
    "plain",
    "plain_split_frames",
    "astral_split_bytes",
    "no_project",
    "reasoning",
    "reasoning_only",
    "engine_http_500",
    "engine_stream_error",
    "empty_stream_fallback",
    "length_cut",
    "title",
    "title_renamed",
    "title_still_derived",
    "title_failed",
    "tool_read",
    "tool_streamed_args",
    "tool_approved",
    "tool_declined",
    "tool_approval_timeout",
    "tool_write_target_refused",
    "tool_unknown",
    "final_pass",
    "final_pass_no_answer",
    "fallback_fails_then_final_pass",
    "auto_fast",
    "context_compacted",
    "context_too_full",
    "thinking_cut",
    "thinking_cut_forced_fails",
    "model_loads",
    "gate_refuses",
    "template_refuses_tools",
    "continuation_compacted",
    "effort_rejected",
];

/// Recorded prepared cases this test does not run, and why.
const SKIPPED: &[(&str, &str)] = &[
    (
        "stopped_mid_stream",
        "`leave`: the client disconnects mid-stream; this harness reads the whole response",
    ),
    (
        "stopped_before_output",
        "`leave`: the client disconnects mid-stream; this harness reads the whole response",
    ),
    (
        "stopped_store_refuses",
        "`leave` plus a refusing transcript store, which the front's own store cannot be told to do",
    ),
    (
        "engine_unreachable",
        "`throws` with a recorded Node error text (ECONNREFUSED on qa-engine.invalid); a closed local socket gives the HTTP client's own wording",
    ),
    (
        "engine_stalls",
        "`stall` waits on the prepared stream-idle limit; the front's read honours it, but the recorded case also depends on Node's abort wording",
    ),
];

type UpBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

fn full(status: u16, ctype: &str, text: String) -> Response<UpBody> {
    Response::builder()
        .status(status)
        .header("content-type", ctype)
        .body(Full::new(Bytes::from(text)).map_err(|e| match e {}).boxed())
        .unwrap()
}

fn json(status: u16, v: &JValue) -> Response<UpBody> {
    full(status, "application/json", js::stringify(v))
}

fn empty(status: u16) -> Response<UpBody> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::new()).map_err(|e| match e {}).boxed())
        .unwrap()
}

fn arr(v: &JValue) -> Vec<JValue> {
    match v {
        JValue::Arr(a) => a.clone(),
        _ => Vec::new(),
    }
}

fn base64(text: &str) -> Vec<u8> {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for b in text.bytes().filter(|b| *b != b'=') {
        acc = (acc << 6) | ABC.iter().position(|x| *x == b).unwrap() as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// What the fakes answer for the case under test, and what they saw.
#[derive(Default)]
struct Script {
    case: JValue,
    engine: String,
    started: f64,
    loaded: bool,
    round: usize,
    bridge: VecDeque<JValue>,
    /// "METHOD path" of every call to the fake Core.
    calls: Vec<String>,
    handoff_header: Option<String>,
    engine_bodies: Vec<String>,
    engine_leases: Vec<Option<String>>,
    asked: Vec<JValue>,
    /// Engine streams held open by a `stall`.
    stalled: Vec<http_body_util::channel::Sender<Bytes, std::io::Error>>,
}

type Shared = Arc<Mutex<Script>>;

async fn listen<F, Fut>(f: F) -> SocketAddr
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = Result<Response<UpBody>, std::io::Error>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let f = f.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |r| f.clone()(r));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

async fn read_body(req: Request<Incoming>) -> (String, String, HashMap<String, String>, String) {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let headers = req
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    (
        method,
        path,
        headers,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

async fn core_service(
    s: Shared,
    req: Request<Incoming>,
) -> Result<Response<UpBody>, std::io::Error> {
    let hold = req.uri().query() == Some("hold=1");
    let (method, path, headers, body) = read_body(req).await;
    let mut sc = s.lock().unwrap();
    sc.calls.push(format!("{method} {path}"));
    let input = sc.case.get("input").clone();
    if path == "/internal/chat/gate" {
        if hold {
            let (mut tx, b) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(1);
            tokio::spawn(async move {
                let _ = tx.send_data(Bytes::from_static(b"held\n")).await;
                while tx.send_data(Bytes::new()).await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
            return Ok(Response::builder()
                .status(200)
                .header("x-noevia-gate-lease", "lease-1")
                .body(b.boxed())
                .unwrap());
        }
        return Ok(empty(204));
    }
    if let Some(rest) = path.strip_prefix("/internal/inference-admission/lease/") {
        if method == "PUT" {
            let refusal = input.get("leaseRefusal");
            if refusal.is_object() {
                return Ok(json(
                    503,
                    &js::obj([
                        ("error", refusal.get("publicMessage").clone()),
                        ("code", refusal.get("code").clone()),
                    ]),
                ));
            }
            return Ok(json(
                201,
                &js::obj([
                    ("lease", js::s(rest)),
                    ("ttlMs", js::n(30_000.0)),
                    ("headers", js::obj([("x-noevia-lease", js::s(LEASE))])),
                ]),
            ));
        }
        return Ok(if rest.ends_with("/renew") {
            empty(200)
        } else {
            empty(204)
        });
    }
    let ok = |v: JValue| Ok(json(200, &v));
    match path.as_str() {
        "/internal/chat/turn" => {
            sc.handoff_header = headers.get("x-noevia-chat-handoff").cloned();
            let mut lines: Vec<String> = arr(sc.case.get("handoff").get("raw"))
                .iter()
                .map(|l| l.as_str().unwrap().to_string())
                .collect();
            let last = lines.pop().unwrap();
            let mut prepared =
                wtf16::parse(last.strip_prefix("data: ").unwrap().trim_end()).unwrap();
            js::set(&mut prepared, "upstreamUrl", js::s(sc.engine.clone()));
            js::set(&mut prepared, "startedAt", js::n(sc.started));
            lines.push(format!("data: {}\n\n", js::stringify(&prepared)));
            let (mut tx, b) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(64);
            tokio::spawn(async move {
                for l in lines {
                    if tx.send_data(Bytes::from(l)).await.is_err() {
                        return;
                    }
                }
                // Core's stream ends once the turn is handed off (rust-chat-native.cjs).
            });
            Ok(Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache, no-transform")
                .header("x-accel-buffering", "no")
                .body(b.boxed())
                .unwrap())
        }
        "/internal/chat/native/health" => {
            let model = input
                .get("project")
                .get("model")
                .as_str()
                .unwrap_or("qa-model")
                .to_string();
            let rows = if sc.loaded {
                vec![js::obj([
                    ("model_name", js::s(model)),
                    ("loaded", JValue::Bool(true)),
                    (
                        "recipe_options",
                        js::obj([("ctx_size", input.get("ctxSize").clone())]),
                    ),
                ])]
            } else {
                Vec::new()
            };
            ok(js::obj([
                ("ok", JValue::Bool(true)),
                (
                    "body",
                    js::obj([("all_models_loaded", JValue::Arr(rows))]),
                ),
            ]))
        }
        "/internal/chat/native/load" => {
            sc.loaded = true;
            ok(js::obj([("ok", JValue::Bool(true))]))
        }
        "/internal/chat/native/tools-allowed" => ok(js::obj([("allowed", JValue::Bool(true))])),
        "/internal/chat/native/busy" => ok(js::obj([("busy", JValue::Bool(false))])),
        "/internal/chat/native/title-auto-named" => {
            let named = input.get("titles").get("autoNamed") != &JValue::Bool(false);
            ok(js::obj([("autoNamed", JValue::Bool(named))]))
        }
        "/internal/chat/tools/target" | "/internal/chat/tools/execute" => {
            let sent = wtf16::parse(&body).unwrap();
            let op = if path.ends_with("target") {
                "target"
            } else {
                "execute"
            };
            let next = sc
                .bridge
                .pop_front()
                .unwrap_or_else(|| panic!("an unrecorded bridge call: {op} {body}"));
            assert_eq!(
                (
                    next.get("op").as_str(),
                    next.get("name").as_str(),
                    next.get("args").as_str()
                ),
                (Some(op), sent.get("name").as_str(), sent.get("args").as_str()),
                "bridge call"
            );
            if op == "target" {
                let found = next.get("found");
                ok(if found.is_object() {
                    found.clone()
                } else {
                    js::obj(Vec::<(String, JValue)>::new())
                })
            } else {
                ok(js::obj([
                    ("text", next.get("text").clone()),
                    (
                        "failed",
                        JValue::Bool(next.get("failed") == &JValue::Bool(true)),
                    ),
                ]))
            }
        }
        "/internal/chat/usage"
        | "/internal/chat/prefill"
        | "/internal/chat/native/title-save"
        | "/internal/chat/native/tools-unsupported" => Ok(empty(204)),
        _ => Ok(json(404, &js::obj([("error", js::s("not found"))]))),
    }
}

async fn engine_service(
    s: Shared,
    req: Request<Incoming>,
) -> Result<Response<UpBody>, std::io::Error> {
    let (_, _, headers, body) = read_body(req).await;
    let (round, chunks) = {
        let mut sc = s.lock().unwrap();
        sc.engine_bodies.push(body);
        sc.engine_leases.push(headers.get("x-noevia-lease").cloned());
        let i = sc.round;
        sc.round += 1;
        let input = sc.case.get("input").clone();
        let round = arr(input.get("rounds"))
            .get(i)
            .cloned()
            .unwrap_or_else(|| panic!("no engine round {}", i + 1));
        let chunks = arr(input.get("chunks")).get(i).cloned().unwrap_or(JValue::Null);
        (round, chunks)
    };
    if round.get("throws").as_str().is_some() {
        // The connection closes without an answer.
        return Err(std::io::Error::other("the engine went away"));
    }
    if let Some(status) = round.get("status").as_num().filter(|n| *n != 200.0) {
        let text = round.get("body").as_str().unwrap_or("").to_string();
        return Ok(full(status as u16, "application/json", text));
    }
    if round.get("json").is_object() {
        return Ok(json(200, round.get("json")));
    }
    let (mut tx, b) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(64);
    let s2 = Arc::clone(&s);
    tokio::spawn(async move {
        for c in arr(&chunks) {
            match &c {
                JValue::Str(m) if m == "wait" => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                JValue::Str(m) if m == "stall" => {
                    s2.lock().unwrap().stalled.push(tx);
                    return;
                }
                JValue::Str(_) => return,
                _ => {
                    let bytes = match c.get("text").as_str() {
                        Some(t) => t.as_bytes().to_vec(),
                        None => base64(c.get("b64").as_str().unwrap()),
                    };
                    if tx.send_data(Bytes::from(bytes)).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    Ok(Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(b.boxed())
        .unwrap())
}

/// The approval registry the test installs: native_turns.rs's binding, never chat-wide, and the case's
/// decision.
struct TestApprovals(Arc<Mutex<Option<Shared>>>);

impl Approvals for TestApprovals {
    fn binding(&self, user: &str, chat: &str, scope: &str, _holder: &JValue) -> Result<JValue, ()> {
        let context = js::obj([
            ("canonicalChatId", js::s(chat)),
            ("holder", js::obj([("kind", js::s("absent"))])),
            ("generation", js::s("0")),
            ("start", js::s(START)),
            ("tombstoned", JValue::Bool(false)),
        ]);
        Ok(js::obj([
            ("user", js::s(user)),
            ("canonicalChatId", js::s(chat)),
            ("scope", js::s(scope)),
            ("context", context),
        ]))
    }

    fn chat_wide(&self, _user: &str, _chat: Option<&str>, _scope: &str, _b: &JValue) -> bool {
        false
    }

    fn ask(
        &self,
        _user: &str,
        request: &JValue,
    ) -> Pin<Box<dyn std::future::Future<Output = String> + Send>> {
        let script = self.0.lock().unwrap().clone().unwrap();
        let decision = {
            let mut sc = script.lock().unwrap();
            sc.asked.push(request.clone());
            sc.case
                .get("input")
                .get("decision")
                .as_str()
                .unwrap_or("approve")
                .to_string()
        };
        Box::pin(async move { decision })
    }

    fn revoke(&self, _user: &str, _chat: Option<&str>) {}
}

struct Front {
    _dir: tempfile::TempDir,
    data: std::path::PathBuf,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
}

async fn front(core: SocketAddr) -> Front {
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
    env.insert("NOEVIA_LEGACY_UPSTREAM", format!("http://{core}"));
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    env.insert("NOEVIA_START_ID", START.into());
    env.insert("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into());
    env.insert("NOEVIA_CHAT_EXECUTOR", "native".into());
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
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic chat native password","diaryEnabled":false}}"#
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
    Front {
        _dir: dir,
        data,
        addr,
        jar,
        csrf,
        user,
    }
}

/// POST /api/chat and the whole SSE body.
async fn chat(f: &Front, body: &str) -> (u16, String) {
    let stream = TcpStream::connect(f.addr).await.unwrap();
    let (mut send, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    let conn = tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/chat")
        .header("host", "front.test")
        .header("origin", ORIGIN)
        .header("content-type", "application/json")
        .header("cookie", &f.jar)
        .header("x-csrf-token", &f.csrf)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let res = send.send_request(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = tokio::time::timeout(Duration::from_secs(60), res.into_body().collect())
        .await
        .expect("the turn did not end within 60 s")
        .unwrap()
        .to_bytes();
    conn.abort();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// The `data:` frames, keep-alives and other comments removed.
fn frames(sse: &str) -> Vec<String> {
    sse.split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .map(str::to_string)
        .collect()
}

fn is_uuid_v4(s: &str) -> bool {
    let b = s.as_bytes();
    s.len() == 36
        && [8, 13, 18, 23].iter().all(|i| b[*i] == b'-')
        && b[14] == b'4'
        && s.bytes().all(|c| c == b'-' || c.is_ascii_hexdigit())
}

/// The clock, uuid and user-id rewrites the module doc names, on what the front produced.
fn settle(v: &mut JValue, user: &str, window: (f64, f64), recorded_ap: &str) {
    match v {
        JValue::Obj(pairs) => {
            for (k, x) in pairs.iter_mut() {
                if let JValue::Num(n) = x {
                    let at = matches!(
                        k.as_str(),
                        "updatedAt" | "observedAt" | "compactedAt" | "savedAt" | "startedAt"
                    );
                    if at && *n >= window.0 && *n <= window.1 {
                        *n = FIXED_NOW;
                    }
                    if matches!(k.as_str(), "timeToFirstToken" | "elapsedMs")
                        && *n >= 0.0
                        && *n < 60_000.0
                    {
                        *n = 0.0;
                    }
                } else {
                    settle(x, user, window, recorded_ap);
                }
            }
        }
        JValue::Arr(items) => items
            .iter_mut()
            .for_each(|x| settle(x, user, window, recorded_ap)),
        JValue::Str(s) => {
            if s == user {
                *s = "qa-user".into();
            } else if s.strip_prefix("ap-").is_some_and(is_uuid_v4) && !recorded_ap.is_empty() {
                *s = recorded_ap.to_string();
            }
        }
        _ => {}
    }
}

/// The recorded `ap-...` id of the case, if any (one approval card per recorded case).
fn recorded_ap(case: &JValue) -> String {
    arr(case.get("approvals"))
        .first()
        .and_then(|a| a.get("id").as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The state files the turn left in the account's directory: `context-*.json` and `chat-partials/*`.
fn state_files(dir: &std::path::Path) -> BTreeMap<String, JValue> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("context-") && name.ends_with(".json") {
            let text = std::fs::read_to_string(e.path()).unwrap();
            out.insert(name, wtf16::parse(&text).unwrap());
        } else if name == "chat-partials" {
            for p in std::fs::read_dir(e.path()).unwrap().flatten() {
                let n = p.file_name().to_string_lossy().into_owned();
                let text = std::fs::read_to_string(p.path()).unwrap();
                out.insert(format!("chat-partials/{n}"), wtf16::parse(&text).unwrap());
            }
        }
    }
    out
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64
}

#[tokio::test(flavor = "multi_thread")]
async fn the_native_executor_answers_as_the_recorded_node_turns() {
    let fixture = wtf16::parse(FIXTURE).unwrap();
    let cases = arr(fixture.get("cases"));
    // Every recorded prepared case is either run or named as skipped.
    for c in cases
        .iter()
        .filter(|c| c.get("handoff").get("prepared").is_object())
    {
        let name = c.get("name").as_str().unwrap();
        assert!(
            CASES.contains(&name) || SKIPPED.iter().any(|(n, _)| *n == name),
            "{name}: neither run nor skipped"
        );
    }
    for (name, why) in SKIPPED {
        eprintln!("SKIPPED {name}: {why}");
    }
    // The front's admission authority needs a Node start to issue leases under.
    engine_admission::authority::global()
        .node_session("qa-chat-native-boot", 1)
        .unwrap();
    let current: Arc<Mutex<Option<Shared>>> = Arc::default();
    noevia_server::chat_native::install_approvals(Arc::new(TestApprovals(Arc::clone(&current))));

    let mut failures = Vec::new();
    for name in CASES {
        let case = cases
            .iter()
            .find(|c| c.get("name").as_str() == Some(*name))
            .unwrap_or_else(|| panic!("{name}: not in the recording"))
            .clone();
        if let Err(e) = run_case(&fixture, &case, &current).await {
            eprintln!("DIFFERS {name}: {e}");
            failures.push(format!("{name}: {e}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

async fn run_case(
    fixture: &JValue,
    case: &JValue,
    current: &Arc<Mutex<Option<Shared>>>,
) -> Result<(), String> {
    let script: Shared = Arc::new(Mutex::new(Script {
        case: case.clone(),
        loaded: case.get("input").get("loaded") != &JValue::Bool(false),
        bridge: arr(case.get("bridge")).into(),
        ..Script::default()
    }));
    *current.lock().unwrap() = Some(Arc::clone(&script));
    let s = Arc::clone(&script);
    let core = listen(move |r| core_service(Arc::clone(&s), r)).await;
    let s = Arc::clone(&script);
    let engine = listen(move |r| engine_service(Arc::clone(&s), r)).await;
    let f = front(core).await;
    let t0 = now_ms();
    {
        let mut sc = script.lock().unwrap();
        sc.engine = format!("http://{engine}/v1/chat/completions");
        sc.started = t0;
        sc.calls.clear();
    }
    let body = js::stringify(case.get("input").get("body"));
    let (status, sse) = chat(&f, &body).await;
    // The turn's after-work (saving, the lease back) runs as the stream ends.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t1 = now_ms();
    let window = (t0, t1);
    let ap = recorded_ap(case);
    if status != 200 {
        return Err(format!("status {status}: {sse}"));
    }
    let sc = script.lock().unwrap();
    if sc.handoff_header.as_deref() != Some(STORE_TOKEN) {
        return Err(format!(
            "the turn's hand-off header was {:?}",
            sc.handoff_header
        ));
    }

    // (a) The client's frames, before and after the cut.
    let want: Vec<String> = arr(case.get("events")).iter().map(js::stringify).collect();
    let got: Vec<String> = frames(&sse)
        .iter()
        .map(|t| {
            let mut v = wtf16::parse(t).unwrap_or_else(|_| js::s(format!("<unparsed> {t}")));
            settle(&mut v, &f.user, window, &ap);
            js::stringify(&v)
        })
        .collect();
    if got != want {
        let i = got
            .iter()
            .zip(&want)
            .position(|(a, b)| a != b)
            .unwrap_or(got.len().min(want.len()));
        return Err(format!(
            "event {i} of {} (recorded {}):\n front: {}\n node:  {}",
            got.len(),
            want.len(),
            got.get(i).map_or("<none>", String::as_str),
            want.get(i).map_or("<none>", String::as_str)
        ));
    }

    // (b) The engine request bodies.
    let recorded = arr(case.get("requests"));
    if sc.engine_bodies.len() != recorded.len() {
        return Err(format!(
            "{} engine requests, recorded {}",
            sc.engine_bodies.len(),
            recorded.len()
        ));
    }
    for (i, (sent, r)) in sc.engine_bodies.iter().zip(&recorded).enumerate() {
        let mut want = r.get("body").clone();
        if let Some(key) = want.get("tools").get("$tools").as_str() {
            let tools = fixture.get("tools").get(key).clone();
            js::set(&mut want, "tools", tools);
        }
        let want = js::stringify(&want);
        if *sent != want {
            return Err(format!(
                "engine request {i} body:\n front: {sent}\n node:  {want}"
            ));
        }
        if sc.engine_leases[i].as_deref() != Some(LEASE) {
            return Err(format!(
                "engine request {i} lease header: {:?}",
                sc.engine_leases[i]
            ));
        }
    }

    // Every recorded bridge answer was asked for, and the approval cards are the recording's.
    if !sc.bridge.is_empty() {
        return Err(format!("{} bridge answers never asked for", sc.bridge.len()));
    }
    let asked: Vec<String> = sc
        .asked
        .iter()
        .map(|a| {
            let mut a = a.clone();
            settle(&mut a, &f.user, window, &ap);
            js::stringify(&a)
        })
        .collect();
    let want: Vec<String> = arr(case.get("approvals")).iter().map(js::stringify).collect();
    if asked != want {
        return Err(format!(
            "approval requests:\n front: {asked:?}\n node:  {want:?}"
        ));
    }

    // (c) The account's state files.
    let dir = server_projects::store::user_dir(&f.data, &f.user).unwrap();
    let got: BTreeMap<String, String> = state_files(&dir)
        .into_iter()
        .map(|(k, mut v)| {
            settle(&mut v, &f.user, window, &ap);
            (k, js::stringify(&v))
        })
        .collect();
    let want: BTreeMap<String, String> = match case.get("files") {
        JValue::Obj(files) => files
            .iter()
            .map(|(k, v)| (k.clone(), js::stringify(v)))
            .collect(),
        _ => BTreeMap::new(),
    };
    if got != want {
        return Err(format!(
            "workspace files:\n front: {got:?}\n node:  {want:?}"
        ));
    }
    Ok(())
}
