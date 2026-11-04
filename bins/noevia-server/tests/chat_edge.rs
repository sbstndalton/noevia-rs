//! The chat edge through the front (chat port S6, `rust-chat-edge`). The table is core's own routes/chat.cjs (and, for
//! the shared budget, routes/diary.cjs) as tests/chat-edge-oracle.cjs records it at the core in
//! tests/fixtures/chat-edge-oracle.core-ref: each refusal must be Node's byte for byte (status, content type, cache
//! control, body), and a request Node let through must reach the stand-in Core's loop (`/internal/chat/turn`), its
//! Cowork start (`/internal/chat/cowork`) or the Diary local exchange whole. The rest is what the oracle cannot see:
//! Core's gate taken before anything is counted and held through every refusal, callers counted in the order the gate
//! lets them in, the hold carried to Core, and the budget's place after authentication. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use serde_json::Value;
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-chat-edge-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const BUSY: &str = r#"{"error":"Inference is paused for maintenance.","code":"inferenceMaintenance","retryable":true,"retryAfter":3}"#;
const DIARY_BUSY: &str = r#"{"error":"Inference is paused for maintenance."}"#;
const LEASE: &str = "x-noevia-gate-lease";
const CAP: usize = 32 * 1024 * 1024;
const HOLD: &str = "POST /internal/chat/gate?hold=1";
const DIARY_HOLD: &str = "POST /internal/chat/gate?route=diary-local-exchange&hold=1";

type UpBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

/// The stand-in's inference gate, as Core's gate meets each call.
#[derive(Clone, Copy, PartialEq, Debug)]
enum GateMode {
    /// Lets every call in at once.
    Open,
    /// Refuses every call at once (the gate is held by maintenance).
    Refuse,
    /// Every call waits until the test lets it in or refuses it ([`Core::decide`]).
    Queue,
}

#[derive(Default)]
struct Seen {
    /// "METHOD path[?query]" of every call.
    hits: Vec<String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Core {
    addr: SocketAddr,
    gate: Mutex<GateMode>,
    /// Calls waiting at a queued gate, in arrival order: their decision (true: let in).
    waiting: Mutex<Vec<tokio::sync::oneshot::Sender<bool>>>,
    /// Leases issued, in the order the gate let calls in.
    issued: Mutex<Vec<String>>,
    /// Leases whose hold closed (the front gave the gate back or its call adopted it).
    closed: Mutex<Vec<String>>,
    seen: Mutex<Seen>,
}

impl Core {
    /// Decides the oldest call waiting at a queued gate.
    async fn decide(&self, let_in: bool) {
        let tx = loop {
            {
                let mut w = self.waiting.lock().unwrap();
                if !w.is_empty() {
                    break w.remove(0);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        tx.send(let_in).unwrap();
    }

    async fn waiting(&self, n: usize) {
        while self.waiting.lock().unwrap().len() < n {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}

fn reply(status: u16, text: &str, extra: &[(&str, &str)]) -> Response<UpBody> {
    let mut b = Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .header("x-noevia-api", "1");
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    b.body(
        Full::new(Bytes::from(text.to_owned()))
            .map_err(|e| match e {})
            .boxed(),
    )
    .unwrap()
}

/// The gate's refusal as `query`'s route meets it.
fn refusal(query: &str) -> Response<UpBody> {
    if query.contains("route=diary-local-exchange") {
        reply(503, DIARY_BUSY, &[])
    } else {
        reply(503, BUSY, &[("retry-after", "3")])
    }
}

/// A held gate: 200 with a fresh lease, kept open until the front closes it.
fn held(core: &Arc<Core>) -> Response<UpBody> {
    let lease = {
        let mut issued = core.issued.lock().unwrap();
        let lease = format!("lease-{}", issued.len() + 1);
        issued.push(lease.clone());
        lease
    };
    let (mut tx, body) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(1);
    let (c, l) = (Arc::clone(core), lease.clone());
    tokio::spawn(async move {
        let _ = tx.send_data(Bytes::from_static(b"held\n")).await;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if tx.send_data(Bytes::new()).await.is_err() {
                c.closed.lock().unwrap().push(l);
                return;
            }
        }
    });
    Response::builder()
        .status(200)
        .header("content-type", "text/plain; charset=utf-8")
        .header(LEASE, lease)
        .body(body.boxed())
        .unwrap()
}

async fn core_service(
    core: Arc<Core>,
    req: Request<Incoming>,
) -> Result<Response<UpBody>, std::io::Error> {
    let target = req.uri().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let method = req.method().to_string();
    let headers = req
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect::<HashMap<_, _>>();
    let body = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    {
        let mut seen = core.seen.lock().unwrap();
        seen.hits.push(format!("{method} {target}"));
        seen.headers = headers.clone();
        seen.body = body.to_vec();
    }
    let lease = headers.get(LEASE).cloned();
    if path == "/internal/chat/gate" {
        let mode = *core.gate.lock().unwrap();
        let let_in = match mode {
            GateMode::Open => true,
            GateMode::Refuse => false,
            GateMode::Queue => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                core.waiting.lock().unwrap().push(tx);
                rx.await.unwrap_or(false)
            }
        };
        return Ok(match (let_in, query.contains("hold=1")) {
            (false, _) => refusal(&query),
            (true, true) => held(&core),
            (true, false) => Response::builder()
                .status(204)
                .body(Full::new(Bytes::new()).map_err(|e| match e {}).boxed())
                .unwrap(),
        });
    }
    Ok(match path.as_str() {
        "/internal/chat/turn" | "/internal/chat/cowork" => {
            let echo = serde_json::json!({
                "at": path,
                "user": headers.get("x-noevia-chat-user"),
                "token": headers.get("x-noevia-store-token"),
                "lease": lease,
                "bodyLen": body.len(),
            });
            reply(200, &echo.to_string(), &[])
        }
        _ => reply(
            200,
            &serde_json::json!({
                "proxied": format!("{method} {path}"),
                "lease": lease,
                "bodyLen": body.len(),
            })
            .to_string(),
            &[],
        ),
    })
}

async fn core() -> Arc<Core> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let core = Arc::new(Core {
        addr: listener.local_addr().unwrap(),
        gate: Mutex::new(GateMode::Open),
        waiting: Mutex::default(),
        issued: Mutex::default(),
        closed: Mutex::default(),
        seen: Mutex::default(),
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
    dir: tempfile::TempDir,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
    core: Arc<Core>,
}

#[derive(Clone, Copy)]
struct Account {
    admin: bool,
    diary: bool,
    harness: bool,
}

impl Front {
    fn db(&self) -> Connection {
        Connection::open(server_store::db_path(self.dir.path())).unwrap()
    }

    fn account(&self, a: Account) {
        let c = self.db();
        c.execute(
            "UPDATE users SET role=?1 WHERE id=?2",
            [if a.admin { "admin" } else { "member" }, &self.user],
        )
        .unwrap();
        c.execute(
            "INSERT OR REPLACE INTO user_features(user_id, diary_enabled, onboarded, updated_at) VALUES(?1, ?2, 1, 0)",
            server_store::rusqlite::params![self.user, i64::from(a.diary)],
        )
        .unwrap();
        c.execute(
            "INSERT OR REPLACE INTO settings VALUES('feature:codeHarness', ?1)",
            [if a.harness { "true" } else { "false" }],
        )
        .unwrap();
    }

    /// The lease of the last call the gate let in (JSON `null` when none was).
    fn last_lease(&self) -> Value {
        self.core
            .issued
            .lock()
            .unwrap()
            .last()
            .map_or(Value::Null, |l| Value::String(l.clone()))
    }

    /// Every issued hold has been closed (given back, or handed to its call).
    async fn holds_closed(&self) {
        for _ in 0..200 {
            if self.core.closed.lock().unwrap().len() == self.core.issued.lock().unwrap().len() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("a gate hold stayed open");
    }

    fn gate(&self, mode: GateMode) {
        *self.core.gate.lock().unwrap() = mode;
    }

    fn clear(&self) {
        self.core.seen.lock().unwrap().hits.clear();
    }

    fn hits(&self) -> Vec<String> {
        self.core.seen.lock().unwrap().hits.clone()
    }
}

async fn front(env_extra: &[(&'static str, &str)]) -> Front {
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
    for (k, v) in env_extra {
        env.insert(k, (*v).to_owned());
    }
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
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic chat edge password","diaryEnabled":false}}"#
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
    let f = Front {
        dir,
        addr,
        jar,
        csrf,
        user,
        core,
    };
    f.clear();
    f
}

struct Answer {
    status: u16,
    headers: hyper::HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Who {
    SignedIn,
    SignedOut,
    BadCsrf,
}

async fn send(f: &Front, method: &str, path: &str, body: Vec<u8>, who: Who) -> Answer {
    let stream = TcpStream::connect(f.addr).await.unwrap();
    let (mut send, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    let conn = tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "front.test")
        .header("origin", ORIGIN)
        .header("content-type", "application/json");
    if who != Who::SignedOut {
        let csrf = if who == Who::BadCsrf {
            "wrong"
        } else {
            &f.csrf
        };
        b = b.header("cookie", &f.jar).header("x-csrf-token", csrf);
    }
    let res = send
        .send_request(b.body(Full::new(Bytes::from(body))).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    conn.abort();
    Answer {
        status,
        headers,
        body,
    }
}

async fn turn(f: &Front, body: &[u8]) -> Answer {
    send(f, "POST", "/api/chat", body.to_vec(), Who::SignedIn).await
}

async fn exchange(f: &Front, body: &[u8]) -> Answer {
    send(
        f,
        "POST",
        "/api/diary/local-exchange",
        body.to_vec(),
        Who::SignedIn,
    )
    .await
}

/// A fixture body spec as bytes (chat-edge-oracle.cjs `bytesOf`).
fn bytes_of(spec: &Value) -> Vec<u8> {
    if let Some(t) = spec["text"].as_str() {
        return t.as_bytes().to_vec();
    }
    if let Some(h) = spec["hex"].as_str() {
        return (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
            .collect();
    }
    let n = usize::try_from(spec["big"].as_u64().unwrap()).unwrap();
    format!("{{\"message\":\"{}\"}}", "a".repeat(n - 14)).into_bytes()
}

fn fixture() -> Value {
    let path = std::env::var("NOEVIA_CHAT_EDGE_FIXTURE").unwrap_or_else(|_| {
        format!(
            "{}/tests/fixtures/node-chat-edge.v1.json",
            env!("CARGO_MANIFEST_DIR")
        )
    });
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn account_of(v: &Value) -> Account {
    Account {
        admin: v["role"] == "admin",
        diary: v["diary"] == true,
        harness: v["harness"] == true,
    }
}

/// One fixture step through the front: Node's refusal byte for byte, or the request where Node sent it.
async fn replay_step(f: &Front, id: &str, step: &Value) {
    let body = bytes_of(&step["body"]);
    let diary = step["route"] == "diary";
    f.clear();
    let got = if diary {
        exchange(f, &body).await
    } else {
        turn(f, &body).await
    };
    let node = &step["node"];
    let hits = f.hits();
    match node["pass"].as_str() {
        Some(at @ ("turn" | "cowork")) => {
            assert_eq!(got.status, 200, "{id}: {}", got.text());
            let echo = got.json();
            assert_eq!(echo["at"], format!("/internal/chat/{at}"), "{id}");
            assert_eq!(echo["user"], f.user.as_str(), "{id}");
            assert_eq!(echo["token"], STORE_TOKEN, "{id}");
            assert_eq!(
                echo["bodyLen"],
                body.len(),
                "{id}: the body reaches Core whole"
            );
            // The gate was taken first, and the turn carried its lease (one gate count).
            assert_eq!(
                hits,
                [HOLD.to_owned(), format!("POST /internal/chat/{at}")],
                "{id}"
            );
            assert_eq!(echo["lease"], f.last_lease(), "{id}");
        }
        Some("diary") => {
            assert_eq!(got.status, 200, "{id}");
            assert_eq!(
                got.json()["proxied"],
                "POST /api/diary/local-exchange",
                "{id}"
            );
            assert_eq!(hits, [DIARY_HOLD, "POST /api/diary/local-exchange"], "{id}");
            assert_eq!(got.json()["lease"], f.last_lease(), "{id}");
        }
        Some(other) => panic!("{id}: unknown pass {other}"),
        None => {
            let want = base64::engine::general_purpose::STANDARD
                .decode(node["body"].as_str().unwrap())
                .unwrap();
            assert_eq!(
                u64::from(got.status),
                node["status"].as_u64().unwrap(),
                "{id}: {}",
                got.text()
            );
            assert_eq!(got.text(), String::from_utf8_lossy(&want), "{id}");
            for h in ["content-type", "cache-control"] {
                assert_eq!(
                    got.headers.get(h).and_then(|v| v.to_str().ok()),
                    node["headers"][h].as_str(),
                    "{id}: {h}"
                );
            }
            assert_eq!(got.headers["x-noevia-api"], "1", "{id}");
            assert!(got.headers.get(LEASE).is_none(), "{id}");
            // Refused by the front with Core's gate held for it; nothing else reached Core.
            assert_eq!(hits, [if diary { DIARY_HOLD } else { HOLD }], "{id}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn routes_chat_cjs_answers_byte_for_byte_and_what_it_let_through_reaches_core_whole() {
    let table = fixture();
    // A budget no single case reaches, so each case meets the edge as on its own (the oracle's fresh limiter of 60).
    let f = front(&[("LLM_RATE_LIMIT", "100000")]).await;
    let mut seen = 0;
    for case in table["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        if id.starts_with("budget") {
            continue;
        }
        f.account(account_of(&case["account"]));
        for step in case["steps"].as_array().unwrap() {
            replay_step(&f, id, step).await;
        }
        seen += 1;
    }
    assert!(seen >= 60, "{seen}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_llm_budget_is_nodes_sixty_a_minute_shared_with_the_diary_local_exchange() {
    let table = fixture();
    assert_eq!(table["limit"], 60);
    let case = table["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"].as_str().unwrap().starts_with("budget"))
        .unwrap();
    // LLM_RATE_LIMIT unset: Node's default.
    let f = front(&[]).await;
    f.account(account_of(&case["account"]));
    for (i, step) in case["steps"].as_array().unwrap().iter().enumerate() {
        replay_step(&f, &format!("budget step {}", i + 1), step).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_the_gate_refused_is_never_counted_and_every_refusal_meets_the_gate_first() {
    let f = front(&[("LLM_RATE_LIMIT", "2")]).await;
    let member = Account {
        admin: false,
        diary: false,
        harness: false,
    };
    f.gate(GateMode::Refuse);
    let bodies: Vec<(Account, Vec<u8>)> = vec![
        (member, br#"{"message":"hi"}"#.to_vec()),
        (member, b"{nope".to_vec()),
        (member, b"[]".to_vec()),
        (member, br#"{"mode":"x"}"#.to_vec()),
        (member, br#"{"mode":"cowork","projectId":"p"}"#.to_vec()),
        (
            Account {
                admin: true,
                ..member
            },
            br#"{"mode":"cowork","projectId":"p"}"#.to_vec(),
        ),
        (member, br#"{"spaceId":"diary"}"#.to_vec()),
        (member, vec![b' '; CAP + 1]),
    ];
    for (account, body) in &bodies {
        f.account(*account);
        f.clear();
        let got = turn(&f, body).await;
        assert_eq!(got.status, 503, "{}", got.text());
        assert_eq!(got.headers["retry-after"], "3");
        assert_eq!(got.text(), BUSY);
        assert_eq!(f.hits(), [HOLD]);
    }
    // None of those was counted: the budget of 2 is whole, and the window opens with the first call let in.
    f.account(member);
    f.gate(GateMode::Open);
    for _ in 0..2 {
        assert_eq!(turn(&f, br#"{"message":"hi"}"#).await.status, 200);
    }
    let over = turn(&f, br#"{"message":"hi"}"#).await;
    assert_eq!(over.status, 429);
    assert_eq!(
        over.text(),
        r#"{"error":"Too many requests — the model endpoint is shared; wait a moment and try again"}"#
    );
    // Over the budget the gate still answers first.
    f.gate(GateMode::Refuse);
    assert_eq!(turn(&f, br#"{"message":"hi"}"#).await.text(), BUSY);
    f.gate(GateMode::Open);
    assert_eq!(turn(&f, br#"{"message":"hi"}"#).await.status, 429);
    // Every hold was closed: given back with the refusal, or handed to the turn that carried it.
    f.holds_closed().await;
}

/// Review P2-2: callers waiting at Core's gate are counted in the order the gate lets them in, and one
/// the gate refuses is never counted, whatever the order they arrived in.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_callers_are_counted_in_the_order_the_gate_lets_them_in() {
    let f = Arc::new(front(&[("LLM_RATE_LIMIT", "1")]).await);
    let spawn = |body: &'static [u8]| {
        let f = Arc::clone(&f);
        tokio::spawn(async move { turn(&f, body).await })
    };
    f.gate(GateMode::Queue);
    // A arrives first and B second; the gate lets B in first. Node counts B, then refuses A (429).
    let a = spawn(br#"{"message":"A"}"#);
    f.core.waiting(1).await;
    let b = spawn(br#"{"message":"B"}"#);
    f.core.waiting(2).await;
    // Neither was counted on arrival (a front that counted first would refuse B whatever the gate did).
    f.core.waiting.lock().unwrap().swap(0, 1);
    f.core.decide(true).await; // B
    let b = b.await.unwrap();
    assert_eq!(b.status, 200, "{}", b.text());
    assert_eq!(b.json()["bodyLen"], br#"{"message":"B"}"#.len());
    f.core.decide(true).await; // A
    let a = a.await.unwrap();
    assert_eq!(a.status, 429, "{}", a.text());
    f.holds_closed().await;

    // Refused at the gate while another is let in: Node counts only the one let in.
    let g = Arc::new(front(&[("LLM_RATE_LIMIT", "1")]).await);
    let spawn = |body: &'static [u8]| {
        let g = Arc::clone(&g);
        tokio::spawn(async move { turn(&g, body).await })
    };
    g.gate(GateMode::Queue);
    let a = spawn(br#"{"message":"A"}"#);
    g.core.waiting(1).await;
    let b = spawn(br#"{"message":"B"}"#);
    g.core.waiting(2).await;
    g.core.decide(false).await; // A refused by the gate
    g.core.decide(true).await; // B let in
    let (a, b) = (a.await.unwrap(), b.await.unwrap());
    assert_eq!((a.status, a.text().as_str()), (503, BUSY));
    assert_eq!(b.status, 200, "{}", b.text());
    // B used the one call; the next one is over.
    g.gate(GateMode::Open);
    assert_eq!(turn(&g, br#"{"message":"C"}"#).await.status, 429);

    // Many at once, the gate letting them in in reverse: exactly the first one it let in is counted within
    // a budget of 1, and the order of arrival plays no part.
    let h = Arc::new(front(&[("LLM_RATE_LIMIT", "1")]).await);
    h.gate(GateMode::Queue);
    let mut calls = Vec::new();
    for i in 0..5 {
        let h2 = Arc::clone(&h);
        calls.push(tokio::spawn(async move {
            turn(&h2, format!(r#"{{"message":"{i}"}}"#).as_bytes()).await
        }));
        h.core.waiting(i + 1).await;
    }
    h.core.waiting.lock().unwrap().reverse();
    for _ in 0..5 {
        h.core.decide(true).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let mut statuses = Vec::new();
    for c in calls {
        statuses.push(c.await.unwrap().status);
    }
    assert_eq!(statuses, [429, 429, 429, 429, 200]);
    h.holds_closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_budget_comes_after_authentication_and_before_the_body() {
    let f = front(&[("LLM_RATE_LIMIT", "1")]).await;
    assert_eq!(turn(&f, br#"{"message":"hi"}"#).await.status, 200);
    // Signed out, or without CSRF: Node's 401/403 before its budget (its gate probe only), nothing counted.
    for (who, status, body) in [
        (Who::SignedOut, 401, r#"{"error":"unauthorized"}"#),
        (Who::BadCsrf, 403, r#"{"error":"invalid CSRF token"}"#),
    ] {
        f.clear();
        let got = send(&f, "POST", "/api/chat", b"{}".to_vec(), who).await;
        assert_eq!((got.status, got.text().as_str()), (status, body));
        assert_eq!(f.hits(), ["POST /internal/chat/gate"]);
    }
    // Over the budget, a body over the cap or not JSON is still the 429: the body is never read.
    for body in [vec![b'a'; CAP + 1], b"{nope".to_vec()] {
        f.clear();
        let got = turn(&f, &body).await;
        assert_eq!(got.status, 429);
        assert_eq!(f.hits(), [HOLD]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_diary_local_exchange_shares_the_budget_after_nodes_own_checks() {
    let f = front(&[("LLM_RATE_LIMIT", "3")]).await;
    let on = Account {
        admin: false,
        diary: true,
        harness: false,
    };
    // Not counted and no hold taken: another method, signed out, CSRF, the add-on off (Node answers each
    // before its budget). A lease a client names is never passed on.
    f.account(on);
    for _ in 0..3 {
        f.clear();
        let got = send(
            &f,
            "GET",
            "/api/diary/local-exchange",
            Vec::new(),
            Who::SignedIn,
        )
        .await;
        assert_eq!(got.json()["proxied"], "GET /api/diary/local-exchange");
        let got = send(
            &f,
            "POST",
            "/api/diary/local-exchange",
            b"{}".to_vec(),
            Who::SignedOut,
        )
        .await;
        assert_eq!(got.json()["proxied"], "POST /api/diary/local-exchange");
        assert_eq!(got.json()["lease"], Value::Null);
        let got = send(
            &f,
            "POST",
            "/api/diary/local-exchange",
            b"{}".to_vec(),
            Who::BadCsrf,
        )
        .await;
        assert_eq!(got.json()["proxied"], "POST /api/diary/local-exchange");
        assert!(f.hits().iter().all(|h| !h.contains("/internal/chat/gate")));
    }
    f.account(Account { diary: false, ..on });
    for _ in 0..3 {
        assert_eq!(
            exchange(&f, b"{}").await.json()["proxied"],
            "POST /api/diary/local-exchange"
        );
    }
    // The gate refusing the exchange: its own 503 (no Retry-After), not counted.
    f.account(on);
    f.gate(GateMode::Refuse);
    for _ in 0..3 {
        f.clear();
        let got = exchange(&f, b"{}").await;
        assert_eq!((got.status, got.text().as_str()), (503, DIARY_BUSY));
        assert!(got.headers.get("retry-after").is_none());
        assert_eq!(f.hits(), [DIARY_HOLD]);
    }
    f.gate(GateMode::Open);
    // One budget: a chat turn and two exchanges use it up; then both routes refuse, in their own words.
    assert_eq!(turn(&f, br#"{"message":"hi"}"#).await.status, 200);
    for _ in 0..2 {
        let got = exchange(&f, b"{}").await;
        assert_eq!(got.status, 200);
        assert_eq!(
            got.json()["lease"],
            f.last_lease(),
            "the exchange carries its hold to Core"
        );
    }
    f.clear();
    let got = exchange(&f, b"{}").await;
    assert_eq!(
        (got.status, got.text().as_str()),
        (
            429,
            r#"{"error":"Please wait before sending another message"}"#
        )
    );
    assert_eq!(f.hits(), [DIARY_HOLD]);
    assert_eq!(turn(&f, br#"{"message":"hi"}"#).await.status, 429);
    // Spellings and siblings are not the local exchange.
    for path in [
        "/api/diary/x/../local-exchange",
        "/api/diary/local-exchange?x=1",
    ] {
        assert_eq!(
            send(&f, "POST", path, b"{}".to_vec(), Who::SignedIn)
                .await
                .status,
            429,
            "{path}"
        );
    }
    for path in ["/api/diary/local-exchange/", "/api/diary/local-exchangex"] {
        let got = send(&f, "POST", path, b"{}".to_vec(), Who::SignedIn).await;
        assert_eq!(got.status, 200, "{path}");
    }
    f.holds_closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cowork_start_goes_to_cores_cowork_relay_and_the_harness_env_overrides_the_setting() {
    let f = front(&[("NOEVIA_FEATURE_CODE_HARNESS", "on")]).await;
    f.account(Account {
        admin: true,
        diary: false,
        harness: false,
    });
    let body =
        br#"{"mode":"cowork","projectId":"p1","repository":"demo","message":"fix the test"}"#;
    let got = turn(&f, body).await;
    assert_eq!(got.status, 200, "{}", got.text());
    assert_eq!(got.json()["at"], "/internal/chat/cowork");
    assert_eq!(f.core.seen.lock().unwrap().body, body.to_vec());
    let off = front(&[("NOEVIA_FEATURE_CODE_HARNESS", "0")]).await;
    off.account(Account {
        admin: true,
        diary: false,
        harness: true,
    });
    assert_eq!(turn(&off, body).await.status, 409);
    // An invalid override stops startup, as Node does not start with one.
    let settings = |v: &'static str| {
        noevia_server::chat_edge::Settings::from_lookup(&move |k: &str| {
            (k == "NOEVIA_FEATURE_CODE_HARNESS").then(|| v.to_owned())
        })
    };
    assert!(settings("maybe").is_err());
    assert_eq!(settings(" YES ").unwrap().code_harness_env, Some(true));
    assert_eq!(settings("").unwrap().code_harness_env, None);
}

#[test]
fn the_oracle_core_is_pinned_and_the_capability_is_listed() {
    let pin = include_str!("fixtures/chat-edge-oracle.core-ref").trim();
    assert!(
        pin.len() == 40 && pin.bytes().all(|b| b.is_ascii_hexdigit()),
        "{pin}"
    );
    assert!(noevia_server::FEATURES.contains(&"rust-chat-edge"));
}
