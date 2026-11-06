//! The MCP tool executor (sbstndalton/noevia#2218, src/mcp_exec) against core's executor, step for step:
//! tests/fixtures/mcp-exec.v1.json is written by core's tools/gen-mcp-exec-fixtures.cjs from the frozen Node
//! executor over the real mcp.cjs transport, and every step below replays it over a synthetic MCP server on
//! 127.0.0.1 and fake credentials: the text the model is given, every request the server saw (method, tool,
//! arguments, the headers a credential decides) and every question the executor put to the broker.
//! Request ids come from a process-wide counter, so "request <n>" is read as "request N" on both sides. No real
//! server, account, token or credential.
#![cfg(feature = "rust-storage-reads")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::mcp_broker::{auth_error, Fail};
use noevia_server::mcp_egress::Egress;
use noevia_server::mcp_exec::{
    nextcloud_headers_for, parse_origins, Auth, Call, Credentials, DirServer, Executor, Fut,
    Operator, Settings,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const FIXTURE: &str = include_str!("fixtures/mcp-exec.v1.json");
const SEEN: [&str; 7] = [
    "authorization",
    "x-api-key",
    "x-cowork-user-id",
    "mcp-session-id",
    "mcp-protocol-version",
    "content-type",
    "accept",
];

/// The synthetic MCP server: behaviour per step, every request logged as the oracle logged it.
struct Fake {
    url: String,
    origin: String,
    behaviour: Arc<Mutex<Value>>,
    calls: Arc<AtomicUsize>,
    log: Arc<Mutex<Vec<Value>>>,
}

fn http_reply(status: u16, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// A stage answered with HTTP 200 and a JSON-RPC `error` (its code a number the server chose), or with a reply whose
/// id is a number the server chose: `{"rpcError":{..}}` / `{"replyId":n}` in the stage's behaviour.
fn protocol_reply(stage: Option<&Value>, id: &Value) -> Option<Vec<u8>> {
    let st = stage?;
    if let Some(e) = st.get("rpcError") {
        return Some(http_reply(
            200,
            &[("content-type", "application/json".to_owned())],
            json!({"jsonrpc":"2.0","id":id,"error":e})
                .to_string()
                .as_bytes(),
        ));
    }
    let wrong = st.get("replyId")?;
    Some(http_reply(
        200,
        &[("content-type", "application/json".to_owned())],
        rpc_reply(wrong, &json!({})).as_bytes(),
    ))
}

fn rpc_reply(id: &Value, result: &Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"result":result}).to_string()
}

impl Fake {
    async fn start() -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let fake = Fake {
            origin: format!("http://127.0.0.1:{port}"),
            url: format!("http://127.0.0.1:{port}/mcp"),
            behaviour: Arc::new(Mutex::new(json!({}))),
            calls: Arc::new(AtomicUsize::new(0)),
            log: Arc::new(Mutex::new(Vec::new())),
        };
        let (behaviour, calls, log) = (
            Arc::clone(&fake.behaviour),
            Arc::clone(&fake.calls),
            Arc::clone(&fake.log),
        );
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (behaviour, calls, log) =
                    (Arc::clone(&behaviour), Arc::clone(&calls), Arc::clone(&log));
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head, body) = loop {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).into_owned();
                        if let Some((h, b)) = text.split_once("\r\n\r\n") {
                            let want = h
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length: ")
                                        .and_then(|v| v.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if b.len() >= want {
                                break (h.to_owned(), b.to_owned());
                            }
                        }
                    };
                    let method = head.split(' ').next().unwrap_or("").to_owned();
                    let mut headers = serde_json::Map::new();
                    for line in head.lines().skip(1) {
                        if let Some((k, v)) = line.split_once(": ") {
                            let k = k.to_ascii_lowercase();
                            if SEEN.contains(&k.as_str()) {
                                headers.insert(k, Value::String(v.to_owned()));
                            }
                        }
                    }
                    let b = behaviour.lock().unwrap().clone();
                    if method == "DELETE" {
                        log.lock()
                            .unwrap()
                            .push(json!({"m":"DELETE","headers":headers}));
                        let status = b.get("del").and_then(Value::as_u64).unwrap_or(200) as u16;
                        let _ = socket.write_all(&http_reply(status, &[], b"")).await;
                        return;
                    }
                    let msg: Value = serde_json::from_str(&body).unwrap_or(json!({}));
                    let rpc = msg.get("method").cloned().unwrap_or(Value::Null);
                    let mut entry = json!({"m":"POST","rpc":rpc,"headers":headers});
                    if rpc == "tools/call" {
                        entry["tool"] = msg["params"]["name"].clone();
                        entry["args"] = msg["params"]["arguments"].clone();
                    }
                    log.lock().unwrap().push(entry);
                    let id = msg.get("id").cloned().unwrap_or(Value::Null);
                    // A server that does not answer one stage for a long while (a call that is dropped meanwhile).
                    if b.get("hold").and_then(Value::as_str) == rpc.as_str() {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    }
                    let json_type = ("content-type", "application/json".to_owned());
                    let out = match rpc.as_str().unwrap_or("") {
                        "initialize" => {
                            if let Some(out) = protocol_reply(b.get("init"), &id) {
                                out
                            } else if let Some(h) = b.get("init").and_then(|i| i.get("http")) {
                                http_reply(
                                    h["status"].as_u64().unwrap_or(500) as u16,
                                    &[],
                                    h.get("body")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .as_bytes(),
                                )
                            } else {
                                let mut hs = vec![json_type];
                                if let Some(sid) = b.get("sid").and_then(Value::as_str) {
                                    hs.push(("mcp-session-id", sid.to_owned()));
                                }
                                http_reply(
                                    200,
                                    &hs,
                                    rpc_reply(&id, &json!({"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"synthetic","version":"1"}})).as_bytes(),
                                )
                            }
                        }
                        "notifications/initialized" => http_reply(202, &[], b""),
                        "tools/call" => {
                            let list: Vec<Value> = match b.get("call") {
                                Some(Value::Array(a)) => a.clone(),
                                Some(v) => vec![v.clone()],
                                None => vec![
                                    json!({"result":{"content":[{"type":"text","text":"ok"}]}}),
                                ],
                            };
                            let at = calls.fetch_add(1, Ordering::SeqCst).min(list.len() - 1);
                            let c = &list[at];
                            if let Some(out) = protocol_reply(Some(c), &id) {
                                out
                            } else if let Some(h) = c.get("http") {
                                let mut hs = vec![];
                                if let Some(r) = h.get("retryAfter").and_then(Value::as_str) {
                                    hs.push(("retry-after", r.to_owned()));
                                }
                                http_reply(
                                    h["status"].as_u64().unwrap() as u16,
                                    &hs,
                                    h.get("body")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .as_bytes(),
                                )
                            } else if let Some(e) = c.get("rpcError") {
                                http_reply(
                                    200,
                                    &[json_type],
                                    json!({"jsonrpc":"2.0","id":id,"error":e})
                                        .to_string()
                                        .as_bytes(),
                                )
                            } else if let Some(r) = c.get("sse") {
                                http_reply(
                                    200,
                                    &[("content-type", "text/event-stream".to_owned())],
                                    format!("event: message\ndata: {}\n\n", rpc_reply(&id, r))
                                        .as_bytes(),
                                )
                            } else if c.get("sseOther").is_some() {
                                http_reply(
                                    200,
                                    &[("content-type", "text/event-stream".to_owned())],
                                    format!("event: message\ndata: {}\n\n", json!({"jsonrpc":"2.0","id":999,"method":"sampling/createMessage","params":{}})).as_bytes(),
                                )
                            } else if c.get("mismatch").is_some() {
                                let wrong = json!(id.as_i64().unwrap_or(0) + 1000);
                                http_reply(
                                    200,
                                    &[json_type],
                                    rpc_reply(
                                        &wrong,
                                        &json!({"content":[{"type":"text","text":"wrong id"}]}),
                                    )
                                    .as_bytes(),
                                )
                            } else if c.get("badJson").is_some() {
                                http_reply(200, &[json_type], b"{\"jsonrpc\":")
                            } else if c.get("huge").is_some() {
                                let text = "x".repeat(9 * 1024 * 1024);
                                http_reply(200, &[json_type], format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}").as_bytes())
                            } else if c.get("redirect").is_some() {
                                http_reply(
                                    302,
                                    &[("location", "http://127.0.0.1:1/elsewhere".to_owned())],
                                    b"",
                                )
                            } else {
                                http_reply(
                                    200,
                                    &[json_type],
                                    rpc_reply(&id, &c["result"]).as_bytes(),
                                )
                            }
                        }
                        "tools/list" => {
                            let list = b.get("list");
                            if let Some(out) = protocol_reply(list, &id) {
                                out
                            } else if let Some(h) = list.and_then(|l| l.get("http")) {
                                http_reply(
                                    h["status"].as_u64().unwrap() as u16,
                                    &[],
                                    h.get("body")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .as_bytes(),
                                )
                            } else {
                                http_reply(
                                    200,
                                    &[json_type],
                                    rpc_reply(&id, &json!({"tools":[]})).as_bytes(),
                                )
                            }
                        }
                        _ => http_reply(400, &[], b""),
                    };
                    let _ = socket.write_all(&out).await;
                });
            }
        });
        fake
    }
}

/// The oracle's credential fakes: queues whose last entry sticks, a storage row, a directory.
struct FakeCreds {
    /// Which credential question never gets an answer: "key", "session", "nextcloud", "url", "directory".
    hang: Mutex<Option<&'static str>>,
    /// How many sign-in questions were put (the retry's is the second).
    sessions: AtomicUsize,
    keys: Mutex<VecDeque<Value>>,
    oauth: Mutex<VecDeque<Value>>,
    log: Arc<Mutex<Vec<Value>>>,
    dir: Vec<DirServer>,
    allowed: bool,
    storage: Value,
}

impl FakeCreds {
    fn hanging(&self, question: &str) -> bool {
        *self.hang.lock().unwrap() == Some(question)
    }
}

async fn forever() {
    std::future::pending::<()>().await;
}

fn pop(queue: &Mutex<VecDeque<Value>>) -> Value {
    let mut q = queue.lock().unwrap();
    if q.len() > 1 {
        q.pop_front().unwrap()
    } else {
        q.front().cloned().unwrap_or(Value::Null)
    }
}

impl Credentials for FakeCreds {
    fn directory(&self) -> Fut<'_, Vec<DirServer>> {
        Box::pin(async move {
            if self.hanging("directory") {
                forever().await;
            }
            self.dir.clone()
        })
    }
    fn key_headers<'a>(
        &'a self,
        user: &'a str,
        server: &'a str,
    ) -> Fut<'a, Result<Vec<(String, String)>, Fail>> {
        Box::pin(async move {
            if self.hanging("key") {
                forever().await;
            }
            self.log.lock().unwrap().push(json!(["key", user, server]));
            match pop(&self.keys) {
                Value::Object(h) => Ok(h
                    .into_iter()
                    .map(|(k, v)| (k, v.as_str().unwrap().to_owned()))
                    .collect()),
                Value::String(s) if s == "transient" => Err(auth_error("transient")),
                _ => Err(auth_error("disconnected")),
            }
        })
    }
    fn session<'a>(
        &'a self,
        user: &'a str,
        server: &'a str,
        rejected: Option<&'a str>,
    ) -> Fut<'a, Result<(String, String), Fail>> {
        Box::pin(async move {
            let asked = self.sessions.fetch_add(1, Ordering::SeqCst);
            if self.hanging("session") || (self.hanging("session-retry") && asked >= 1) {
                forever().await;
            }
            self.log
                .lock()
                .unwrap()
                .push(json!(["session", user, server, rejected]));
            let next = pop(&self.oauth);
            if next.get("transient").is_some() {
                return Err(auth_error("transient"));
            }
            match (
                next.get("accessToken").and_then(Value::as_str),
                next.get("generation").and_then(Value::as_str),
            ) {
                (Some(a), Some(g)) => Ok((a.to_owned(), g.to_owned())),
                _ => Err(auth_error("disconnected")),
            }
        })
    }
    fn reject<'a>(&'a self, user: &'a str, server: &'a str, generation: &'a str) -> Fut<'a, bool> {
        Box::pin(async move {
            self.log
                .lock()
                .unwrap()
                .push(json!(["reject", user, server, generation]));
            true
        })
    }
    fn nextcloud<'a>(
        &'a self,
        user: &'a str,
        origins: &'a [String],
    ) -> Fut<'a, Option<Vec<(String, String)>>> {
        Box::pin(async move {
            if self.hanging("nextcloud") {
                forever().await;
            }
            let s = &self.storage;
            nextcloud_headers_for(
                matches!(s["kind"].as_str(), Some("nextcloud" | "webdav")),
                s["baseUrl"].as_str().unwrap_or(""),
                s["username"].as_str().unwrap_or(""),
                s["secret"].as_str().unwrap_or("").as_bytes(),
                user,
                origins,
            )
        })
    }
    fn url_allowed<'a>(&'a self, _url: &'a str) -> Fut<'a, bool> {
        Box::pin(async move {
            if self.hanging("url") {
                forever().await;
            }
            self.allowed
        })
    }
}

fn queue(v: Option<&Value>, none: Value) -> VecDeque<Value> {
    match v {
        Some(Value::Array(a)) => a.iter().cloned().collect(),
        Some(other) => VecDeque::from([other.clone()]),
        None => VecDeque::from([none]),
    }
}

/// `request 12` / `reply id 12` are written with N on both sides.
fn normal(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = ["request ", "reply id "]
        .iter()
        .filter_map(|p| rest.find(p).map(|i| (i, p.len())))
        .min()
    {
        let (at, len) = i;
        out.push_str(&rest[..at + len]);
        let tail = &rest[at + len..];
        let digits = tail.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 {
            out.push('N');
        }
        rest = &tail[digits..];
    }
    out.push_str(rest);
    out
}

fn call_of(c: &Value) -> Call {
    let s = |k: &str| c.get(k).and_then(Value::as_str).map(str::to_owned);
    Call {
        caller: s("caller"),
        workspace: s("workspace"),
        name: s("name").unwrap(),
        args: c
            .get("args")
            .map(|a| js_json::parse(&a.to_string()).unwrap()),
        server: s("server"),
        offers: c["offers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect(),
        edit_target: c
            .get("editTarget")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        internal_auth: s("internalAuth"),
    }
}

/// The scenarios whose failure text is the runtime's own (V8's JSON SyntaxError), not ours.
const INEXACT: [(&str, &str); 1] = [("reply-is-not-json", "ERROR calling t1: ")];

/// The deliberate departures from the Node oracle (sbstndalton/noevia#2218 review, credential leakage): a JSON-RPC
/// error is its code and never the server's message, which can echo the credential the request carried.
const FIXED: [(&str, &str); 1] = [("json-rpc-error", "ERROR calling t1: MCP error -32000")];

#[tokio::test]
async fn every_step_of_the_oracle_is_replayed() {
    let fake = Fake::start().await;
    let text = FIXTURE
        .replace("$FAKE", &fake.url)
        .replace("$ORIGIN", &fake.origin)
        .replace("$DEAD", "http://127.0.0.1:1/mcp");
    let doc: Value = serde_json::from_str(&text).unwrap();
    let egress = Egress::new(true).unwrap();
    let mut replayed = 0usize;
    for case in doc["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let operators: Vec<Operator> = case["servers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| !s["directory"].as_bool().unwrap_or(false))
            .map(|s| Operator {
                id: s["id"].as_str().unwrap().to_owned(),
                url: s["url"].as_str().unwrap().to_owned(),
                auth: match s["auth"].as_str().unwrap() {
                    "nextcloud" => Auth::Nextcloud,
                    "internal" => Auth::Internal,
                    "bearer" => Auth::Bearer,
                    _ => Auth::None,
                },
                token_env: s.get("tokenEnv").and_then(Value::as_str).map(str::to_owned),
            })
            .collect();
        // Bearer tokens are read from the process environment at call time.
        for (k, v) in case
            .get("env")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            std::env::set_var(k, v.as_str().unwrap());
        }
        for op in &operators {
            if let Some(k) = &op.token_env {
                if case.get("env").and_then(|e| e.get(k)).is_none() {
                    std::env::remove_var(k);
                }
            }
        }
        let dir: Vec<DirServer> = case["servers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["directory"].as_bool().unwrap_or(false))
            .map(|s| {
                let shared: Vec<(String, String)> = case["shared"][s["id"].as_str().unwrap()]
                    .as_object()
                    .map(|h| {
                        h.iter()
                            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                            .collect()
                    })
                    .unwrap_or_default();
                DirServer {
                    id: s["id"].as_str().unwrap().to_owned(),
                    url: s["url"].as_str().unwrap().to_owned(),
                    title: s["title"].as_str().unwrap_or("").to_owned(),
                    oauth: s["auth"] == "oauth",
                    personal: s["auth"] == "personal",
                    shared,
                }
            })
            .collect();
        let origins = parse_origins(
            &case["origins"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|v| v.as_str().unwrap())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default(),
        );
        let broker_log = Arc::new(Mutex::new(Vec::new()));
        let creds = FakeCreds {
            hang: Mutex::new(None),
            sessions: AtomicUsize::new(0),
            keys: Mutex::new(queue(case.get("keys"), Value::Null)),
            oauth: Mutex::new(queue(case.get("oauth"), Value::Null)),
            log: Arc::clone(&broker_log),
            dir,
            allowed: case
                .get("directoryAllowed")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            storage: case
                .get("storage")
                .cloned()
                .unwrap_or(json!({"kind":"local"})),
        };
        // One executor per scenario: its 429 wait lives across the steps of a scenario and nowhere else.
        let exec = Executor::new(
            Settings {
                servers: operators,
                nextcloud_origins: origins,
                enabled_toolboxes: None,
            },
            &egress,
        );
        for (n, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            *fake.behaviour.lock().unwrap() = step["mcp"].clone();
            fake.calls.store(0, Ordering::SeqCst);
            fake.log.lock().unwrap().clear();
            broker_log.lock().unwrap().clear();
            let call = call_of(&step["call"]);
            let got = if step.get("operator").is_some() {
                match exec.operator_server_for(&call.offers) {
                    Some(server) => {
                        let mut c = call.clone();
                        c.server = Some(server);
                        Ok(exec.run(&creds, c).await)
                    }
                    None => Err("Web search is not configured on this server."),
                }
            } else {
                Ok(exec.run(&creds, call).await)
            };
            let at = format!("{id} step {n}");
            match (
                &got,
                step["expect"].get("text"),
                step["expect"].get("throws"),
            ) {
                (Ok(g), Some(want), _) => {
                    let want = want.as_str().unwrap();
                    if let Some((_, prefix)) = INEXACT.iter().find(|(i, _)| *i == id) {
                        assert!(
                            g.starts_with(prefix) && want.starts_with(prefix),
                            "{at}: {g}"
                        );
                    } else {
                        let want = FIXED
                            .iter()
                            .find(|(i, _)| *i == id)
                            .map_or(want, |(_, t)| *t);
                        assert_eq!(normal(g), want, "{at}");
                    }
                }
                (Err(g), _, Some(want)) => assert_eq!(*g, want.as_str().unwrap(), "{at}"),
                (g, w, t) => panic!("{at}: got {g:?}, wanted {w:?} / {t:?}"),
            }
            let seen = Value::Array(fake.log.lock().unwrap().clone());
            assert_eq!(seen, step["requests"], "{at}: the requests the server saw");
            let asked = Value::Array(broker_log.lock().unwrap().clone());
            assert_eq!(asked, step["broker"], "{at}: what the broker was asked");
            replayed += 1;
        }
    }
    assert!(replayed >= 80, "{replayed}");
}

// ── a call that is dropped, wherever it is (the chat's Stop, a client that left, research's cancel) ──

fn creds_for(storage: Value, keys: Value, oauth: Value, dir: Vec<DirServer>) -> Arc<FakeCreds> {
    Arc::new(FakeCreds {
        hang: Mutex::new(None),
        sessions: AtomicUsize::new(0),
        keys: Mutex::new(queue(Some(&keys), Value::Null)),
        oauth: Mutex::new(queue(Some(&oauth), Value::Null)),
        log: Arc::new(Mutex::new(Vec::new())),
        dir,
        allowed: true,
        storage,
    })
}

async fn until(what: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if what() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    false
}

fn requests_of(fake: &Fake, rpc: &str) -> usize {
    fake.log
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r["rpc"] == rpc)
        .count()
}

fn deletes_of(fake: &Fake) -> usize {
    fake.log
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r["m"] == "DELETE")
        .count()
}

fn executor(fake: &Fake, servers: Vec<Operator>) -> Arc<Executor> {
    let _ = fake;
    Arc::new(Executor::new(
        Settings {
            servers,
            nextcloud_origins: vec![],
            enabled_toolboxes: None,
        },
        &Egress::new(true).unwrap(),
    ))
}

fn plain(fake: &Fake) -> Operator {
    Operator {
        id: "s".into(),
        url: fake.url.clone(),
        auth: Auth::None,
        token_env: None,
    }
}

fn simple_call() -> Call {
    Call {
        caller: Some("u1".into()),
        workspace: Some("u1".into()),
        name: "t1".into(),
        args: Some(js_json::parse("{\"q\":1}").unwrap()),
        offers: vec!["s".into()],
        ..Default::default()
    }
}

/// Aborts the call while each MCP stage is unanswered: a session the server issued is closed, and nothing is sent
/// after the point where it stopped.
#[tokio::test]
async fn a_call_dropped_at_each_mcp_stage_closes_its_session_and_sends_nothing_after() {
    for (stage, sessions, calls) in [
        ("initialize", 0, 0),
        ("notifications/initialized", 1, 0),
        ("tools/call", 1, 1),
    ] {
        let fake = Fake::start().await;
        *fake.behaviour.lock().unwrap() = json!({"sid": "S-1", "hold": stage});
        let exec = executor(&fake, vec![plain(&fake)]);
        let creds = creds_for(json!({"kind":"local"}), Value::Null, Value::Null, vec![]);
        let task = tokio::spawn({
            let (exec, creds) = (Arc::clone(&exec), Arc::clone(&creds));
            async move { exec.run(&*creds, simple_call()).await }
        });
        assert!(
            until(|| requests_of(&fake, stage) == 1).await,
            "{stage}: the server never saw it"
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // The close is handed to a task of its own; give it a moment.
        assert!(
            until(|| deletes_of(&fake) >= sessions).await,
            "{stage}: the session was not closed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(deletes_of(&fake), sessions, "{stage}: one close, no more");
        assert_eq!(
            requests_of(&fake, "tools/call"),
            calls,
            "{stage}: nothing was sent after it stopped"
        );
        let log = fake.log.lock().unwrap().clone();
        if sessions == 1 {
            let close = log.iter().find(|r| r["m"] == "DELETE").unwrap();
            assert_eq!(close["headers"]["mcp-session-id"], "S-1", "{stage}");
        }
    }
}

/// Aborts the call while a credential, the address check, the directory or the retry's new sign-in is unanswered:
/// no request has left (or the one that did was closed), and the 429 wait is not charged to a call that never ran.
#[tokio::test]
async fn a_call_dropped_while_a_credential_is_asked_sends_nothing() {
    let dir = DirServer {
        id: "dir-o".into(),
        url: String::new(),
        title: "Dir O".into(),
        oauth: true,
        personal: false,
        shared: vec![],
    };
    for (question, server_id, auth) in [
        ("directory", "dir-o", Auth::None),
        ("session", "dir-o", Auth::None),
        ("key", "dir-p", Auth::None),
        ("nextcloud", "s", Auth::Nextcloud),
        ("url", "dir-o", Auth::None),
    ] {
        let fake = Fake::start().await;
        let exec = executor(
            &fake,
            vec![Operator {
                auth,
                ..plain(&fake)
            }],
        );
        let mut d = dir.clone();
        d.url = fake.url.clone();
        d.id = server_id.to_owned();
        d.personal = server_id == "dir-p";
        d.oauth = server_id == "dir-o";
        let creds = creds_for(
            json!({"kind":"nextcloud","baseUrl":"http://x","username":"a","secret":"b"}),
            json!({"X-Api-Key":"k"}),
            json!({"accessToken":"acc","generation":"g"}),
            vec![d],
        );
        *creds.hang.lock().unwrap() = Some(question);
        let task = tokio::spawn({
            let (exec, creds) = (Arc::clone(&exec), Arc::clone(&creds));
            async move {
                let mut call = simple_call();
                call.offers = vec![server_id.to_owned()];
                exec.run(&*creds, call).await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled(), "{question}");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            fake.log.lock().unwrap().is_empty(),
            "{question}: nothing was sent"
        );
    }
}

/// The retry after a refused sign-in: dropped while the broker is asked for the second token, the first
/// session is already closed and no second call is made.
#[tokio::test]
async fn a_call_dropped_while_the_retry_asks_the_broker_has_nothing_open() {
    let fake = Fake::start().await;
    *fake.behaviour.lock().unwrap() =
        json!({"sid": "S-1", "call": [{"http": {"status": 401, "body": "no"}}]});
    let exec = executor(&fake, vec![]);
    let dir = DirServer {
        id: "dir-o".into(),
        url: fake.url.clone(),
        title: "Dir O".into(),
        oauth: true,
        personal: false,
        shared: vec![],
    };
    let creds = creds_for(
        json!({"kind":"local"}),
        Value::Null,
        json!([{"accessToken":"acc-1","generation":"g1"}, {"accessToken":"acc-2","generation":"g2"}]),
        vec![dir],
    );
    // The second sign-in question (the retry's) is never answered.
    *creds.hang.lock().unwrap() = Some("session-retry");
    let task = tokio::spawn({
        let (exec, creds) = (Arc::clone(&exec), Arc::clone(&creds));
        async move {
            let mut call = simple_call();
            call.offers = vec!["dir-o".into()];
            exec.run(&*creds, call).await
        }
    });
    // The first session was refused and closed; the retry is waiting on the broker.
    assert!(until(|| deletes_of(&fake) == 1 && creds.sessions.load(Ordering::SeqCst) == 2).await);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        requests_of(&fake, "tools/call"),
        1,
        "no second call was made"
    );
    assert_eq!(deletes_of(&fake), 1, "nothing was left open");
}

/// A directory server's address is checked at connect time too, whatever the pre-check said (DNS rebinding).
#[tokio::test]
async fn a_directory_call_to_a_private_address_is_refused_even_when_the_check_passed() {
    let fake = Fake::start().await;
    let dir = DirServer {
        id: "dir-x".into(),
        url: fake.url.clone(),
        title: "Dir X".into(),
        oauth: false,
        personal: false,
        shared: vec![],
    };
    // The egress here is not the QA stand-in: a loopback address is private.
    let exec = Executor::new(Settings::default(), &Egress::new(false).unwrap());
    let creds = creds_for(json!({"kind":"local"}), Value::Null, Value::Null, vec![dir]);
    let mut call = simple_call();
    call.offers = vec!["dir-x".into()];
    let text = exec.run(&*creds, call).await;
    assert_eq!(text, "ERROR calling t1: fetch failed");
    assert!(
        fake.log.lock().unwrap().is_empty(),
        "the server received nothing"
    );
}

#[test]
fn operator_server_for_takes_the_one_operator_server_and_ignores_directory_twins() {
    let op = |id: &str| Operator {
        id: id.into(),
        url: "http://x".into(),
        auth: Auth::None,
        token_env: None,
    };
    let exec = Executor::new(
        Settings {
            servers: vec![op("a"), op("b")],
            ..Settings::default()
        },
        &Egress::new(true).unwrap(),
    );
    let ids = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        exec.operator_server_for(&ids(&["dir-x", "a"])),
        Some("a".into())
    );
    assert_eq!(
        exec.operator_server_for(&ids(&["a", "b"])),
        None,
        "two operator servers: ambiguous"
    );
    assert_eq!(
        exec.operator_server_for(&ids(&["dir-x"])),
        None,
        "never a directory server"
    );
    assert_eq!(exec.operator_server_for(&[]), None);
    // ENABLED_TOOLBOXES: core and dir-* always, the rest as listed.
    let limited = Executor::new(
        Settings {
            enabled_toolboxes: Some(vec!["files".into()]),
            ..Settings::default()
        },
        &Egress::new(true).unwrap(),
    );
    assert!(
        limited.toolbox_offered("core")
            && limited.toolbox_offered("dir-x")
            && limited.toolbox_offered("files")
    );
    assert!(!limited.toolbox_offered("web-search"));
    assert!(exec.toolbox_offered("web-search"));
}

#[test]
fn the_operators_servers_and_origins_are_read_as_core_reads_them() {
    let env = |k: &str| -> Option<String> {
        match k {
            "MCP_SERVERS" => Some("search|http://10.0.0.5:9000/mcp|bearer:SEARCH_TOKEN, nc|http://10.0.0.6/mcp|nextcloud, self|http://127.0.0.1:9100/mcp|internal".into()),
            "MCP_NEXTCLOUD_ORIGINS" => Some(" https://a.example/, http://10.0.0.1:11000 ,,".into()),
            "ENABLED_TOOLBOXES" => Some("web-search,files".into()),
            _ => None,
        }
    };
    let s = Settings::from_lookup(&env);
    assert_eq!(
        s.servers
            .iter()
            .map(|o| (o.id.as_str(), o.auth, o.token_env.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("search", Auth::Bearer, Some("SEARCH_TOKEN")),
            ("nc", Auth::Nextcloud, None),
            ("self", Auth::Internal, None)
        ]
    );
    assert_eq!(
        s.nextcloud_origins,
        ["https://a.example", "http://10.0.0.1:11000"]
    );
    assert_eq!(
        s.enabled_toolboxes,
        Some(vec!["web-search".to_owned(), "files".to_owned()])
    );
    let none = Settings::from_lookup(&|_| None);
    assert!(none.servers.is_empty() && none.enabled_toolboxes.is_none());
    // MCP_SERVER_URL alone is one Nextcloud server.
    let single = Settings::from_lookup(&|k| {
        (k == "MCP_SERVER_URL").then(|| "http://10.0.0.9/mcp".to_owned())
    });
    assert_eq!(single.servers.len(), 1);
    assert_eq!(single.servers[0].auth, Auth::Nextcloud);
}

// ── a server that repeats the credential it was sent (review: credential-bearing upstream errors) ──

const BEARER: &str = "SYNTH-bearer-tok-4f7a91c2";
const API_KEY: &str = "SYNTH-apikey-7d03be55";
const CUSTOM: &str = "SYNTH-custom-hdr-92ac11e0";
const OAUTH_ONE: &str = "SYNTH-oauth-first-31ab90";
const OAUTH_TWO: &str = "SYNTH-oauth-retry-6be2d4";
const NC_USER: &str = "synth-nc-user";
const NC_SECRET: &str = "SYNTH-nc-secret-a81f3c";

fn lines() -> (noevia_server::mcp_exec::LogSink, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    (
        Arc::new(move |l: &str| sink.lock().unwrap().push(l.to_owned())),
        seen,
    )
}

/// The behaviours of a server that echoes: the same hostile text in every place a server can put text.
fn echo_behaviours(echo: &str) -> Vec<(&'static str, Value, Option<&'static str>)> {
    vec![
        (
            "401 body",
            json!({"call":[{"http":{"status":401,"body":echo}}]}),
            Some("ERROR calling t1: tool call failed (HTTP 401)"),
        ),
        (
            "500 body at initialize",
            json!({"init":{"http":{"status":500,"body":echo}}}),
            Some("ERROR calling t1: tool call failed (HTTP 500)"),
        ),
        (
            "429 body",
            json!({"call":[{"http":{"status":429,"body":echo,"retryAfter":"1"}}]}),
            None,
        ),
        (
            "JSON-RPC error message",
            json!({"call":[{"rpcError":{"code":-32001,"message":echo}}]}),
            Some("ERROR calling t1: MCP error -32001"),
        ),
        (
            "JSON-RPC error with a text code",
            json!({"call":[{"rpcError":{"code":echo,"message":echo}}]}),
            Some("ERROR calling t1: MCP error"),
        ),
        (
            "tool error result",
            json!({"call":[{"result":{"isError":true,"content":[{"type":"text","text":echo}]}}]}),
            None,
        ),
        (
            "tool result",
            json!({"call":[{"result":{"content":[{"type":"text","text":echo}]}}]}),
            None,
        ),
        (
            "structured result",
            json!({"call":[{"result":{"structuredContent":{"note":echo}}}]}),
            None,
        ),
        (
            "content type name",
            json!({"call":[{"result":{"content":[{"type":echo}]}}]}),
            None,
        ),
    ]
}

/// `text` holds `secret` as a whole token (not touching a letter, digit or underscore).
fn has_token(text: &str, secret: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(secret).any(|(at, m)| {
        !text[..at].chars().next_back().is_some_and(word)
            && !text[at + m.len()..].chars().next().is_some_and(word)
    })
}

fn assert_clean(at: &str, text: &str, logs: &Mutex<Vec<String>>, secrets: &[&str]) {
    let log = logs.lock().unwrap().join("\n");
    for secret in secrets {
        assert!(
            !has_token(text, secret),
            "{at}: the model text repeats {secret}: {text}"
        );
        assert!(
            !has_token(&log, secret),
            "{at}: a log line repeats {secret}: {log}"
        );
    }
}

/// The credential values of one round of the echo matrix.
struct Cred {
    env: &'static str,
    bearer: String,
    api_key: String,
    custom: String,
    oauth_one: String,
    oauth_two: String,
    nc_user: String,
    nc_secret: String,
}

impl Cred {
    fn long() -> Cred {
        Cred {
            env: "SYNTH_ECHO_BEARER",
            bearer: BEARER.into(),
            api_key: API_KEY.into(),
            custom: CUSTOM.into(),
            oauth_one: OAUTH_ONE.into(),
            oauth_two: OAUTH_TWO.into(),
            nc_user: NC_USER.into(),
            nc_secret: NC_SECRET.into(),
        }
    }

    /// Every credential `n` bytes long (1..=8).
    fn short(n: usize, env: &'static str) -> Cred {
        let cut = |s: &str| s[..n].to_owned();
        Cred {
            env,
            bearer: cut("q7Z!2xyz"),
            api_key: cut("K5m$7def"),
            custom: cut("c3W%8ghi"),
            oauth_one: cut("A1b*4jkl"),
            oauth_two: cut("B2c+5mno"),
            nc_user: cut("u3Q%9pqr"),
            nc_secret: cut("P9x#4abc"),
        }
    }
}

async fn echo_matrix(c: &Cred) {
    let basic = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("{}:{}", c.nc_user, c.nc_secret),
    );
    // Each value stands as a token of its own in the server's text: that is how a short one is looked for.
    let all = format!(
        "Authorization: Bearer {} / Bearer {} / Bearer {} / Basic {basic} / x-api-key: {} / x-custom-auth: {} / user {} secret {}",
        c.bearer, c.oauth_one, c.oauth_two, c.api_key, c.custom, c.nc_user, c.nc_secret
    );
    std::env::set_var(c.env, &c.bearer);
    for kind in ["bearer", "personal key", "oauth retry", "nextcloud"] {
        for (what, behaviour, exact) in echo_behaviours(&all) {
            let fake = Fake::start().await;
            let mut behaviour = behaviour;
            let mut operators = vec![];
            let mut dir = vec![];
            let mut offers = "s".to_owned();
            let mut storage = json!({"kind":"local"});
            let mut keys = Value::Null;
            let mut oauth = Value::Null;
            let mut secrets: Vec<&str> = vec![];
            let mut origins = vec![];
            match kind {
                "bearer" => {
                    operators.push(Operator {
                        auth: Auth::Bearer,
                        token_env: Some(c.env.into()),
                        ..plain(&fake)
                    });
                    secrets.push(&c.bearer);
                }
                "personal key" => {
                    offers = "dir-p".into();
                    dir.push(DirServer {
                        id: "dir-p".into(),
                        url: fake.url.clone(),
                        title: "Dir P".into(),
                        oauth: false,
                        personal: true,
                        shared: vec![],
                    });
                    keys = json!({"X-Api-Key": c.api_key, "X-Custom-Auth": c.custom});
                    secrets.extend([c.api_key.as_str(), c.custom.as_str()]);
                }
                "oauth retry" => {
                    offers = "dir-o".into();
                    dir.push(DirServer {
                        id: "dir-o".into(),
                        url: fake.url.clone(),
                        title: "Dir O".into(),
                        oauth: true,
                        personal: false,
                        shared: vec![],
                    });
                    oauth = json!([
                        {"accessToken": c.oauth_one, "generation": "g1"},
                        {"accessToken": c.oauth_two, "generation": "g2"}
                    ]);
                    // The first answer is the refusal that makes the call retry with the second token, and
                    // it echoes both; the behaviour under test is what the retry gets.
                    if let Some(list) = behaviour.get("call").and_then(Value::as_array).cloned() {
                        let mut with = vec![json!({"http":{"status":401,"body":all}})];
                        with.extend(list);
                        behaviour["call"] = Value::Array(with);
                    }
                    secrets.extend([c.oauth_one.as_str(), c.oauth_two.as_str()]);
                }
                _ => {
                    operators.push(Operator {
                        auth: Auth::Nextcloud,
                        ..plain(&fake)
                    });
                    origins.push(fake.origin.clone());
                    storage = json!({"kind":"nextcloud","baseUrl":fake.origin,"username":c.nc_user,"secret":c.nc_secret});
                    secrets.extend([c.nc_secret.as_str(), c.nc_user.as_str(), basic.as_str()]);
                }
            }
            *fake.behaviour.lock().unwrap() = behaviour;
            let (sink, logs) = lines();
            let exec = Executor::new(
                Settings {
                    servers: operators,
                    nextcloud_origins: origins,
                    enabled_toolboxes: None,
                },
                &Egress::new(true).unwrap(),
            )
            .with_log(sink);
            let creds = creds_for(storage, keys, oauth, dir);
            let mut call = simple_call();
            call.offers = vec![offers];
            let text = exec.run(&*creds, call).await;
            let at = format!("{kind} / {what} / {} bytes", c.bearer.len());
            assert_clean(&at, &text, &logs, &secrets);
            if let Some(exact) = exact {
                assert_eq!(text, exact, "{at}");
            }
            // A result that is the tool's own words keeps them, minus the credential.
            if what.starts_with("tool") && !text.contains("(the tool returned no output)") {
                assert!(text.contains("[redacted]"), "{at}: {text}");
                assert!(
                    text.contains("Authorization:"),
                    "{at}: the rest of the text is kept"
                );
            }
        }
    }
}

#[tokio::test]
async fn nothing_a_server_sends_back_repeats_a_credential_the_call_carried() {
    echo_matrix(&Cred::long()).await;
}

/// Review round 2: a credential of any length is a credential. 1..=5 bytes (the bearer admission accepts any
/// nonempty token), the six-byte boundary the old threshold sat on, and the eight-byte one where the scrub
/// stops needing token boundaries.
#[tokio::test]
async fn a_short_credential_a_server_repeats_is_taken_out_whatever_its_length() {
    for n in 1..=8 {
        let env = [
            "SYNTH_SHORT_1",
            "SYNTH_SHORT_2",
            "SYNTH_SHORT_3",
            "SYNTH_SHORT_4",
            "SYNTH_SHORT_5",
            "SYNTH_SHORT_6",
            "SYNTH_SHORT_7",
            "SYNTH_SHORT_8",
        ][n - 1];
        echo_matrix(&Cred::short(n, env)).await;
    }
}

/// A word that merely contains a short credential is not one: the tool's own text survives.
#[tokio::test]
async fn a_short_credential_does_not_blank_the_words_that_contain_it() {
    let fake = Fake::start().await;
    *fake.behaviour.lock().unwrap() =
        json!({"call":[{"result":{"content":[{"type":"text","text":"cabin about ab ab."}]}}]});
    std::env::set_var("SYNTH_WORDS", "ab");
    let exec = executor(
        &fake,
        vec![Operator {
            auth: Auth::Bearer,
            token_env: Some("SYNTH_WORDS".into()),
            ..plain(&fake)
        }],
    );
    let creds = creds_for(json!({"kind":"local"}), Value::Null, Value::Null, vec![]);
    let text = exec.run(&*creds, simple_call()).await;
    assert_eq!(text, "cabin about [redacted] [redacted].");
}

// ── the key check and the listing (discovery) repeat no key either ──

/// What `check_key` and `list_tools` say when the server refuses, for a server that repeats the key it was sent.
#[tokio::test]
async fn a_key_check_and_a_listing_that_fail_do_not_repeat_the_key() {
    use noevia_server::mcp_egress::{check_key, list_tools};
    let basic = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        "kc-user:kc-pass9",
    );
    // (headers sent, the values the server will repeat)
    type Round = (Vec<(String, String)>, Vec<String>);
    let rounds: Vec<Round> = vec![
        (
            vec![("X-Api-Key".into(), "SYNTH-api-secret-1234".into())],
            vec!["SYNTH-api-secret-1234".into()],
        ),
        (
            vec![("X-Api-Key".into(), "q7Z!".into())],
            vec!["q7Z!".into()],
        ),
        (vec![("X-Api-Key".into(), "k".into())], vec!["k".into()]),
        (
            vec![("Authorization".into(), "Bearer q7Z!2".into())],
            vec!["q7Z!2".into(), "Bearer q7Z!2".into()],
        ),
        (
            vec![("Authorization".into(), format!("Basic {basic}"))],
            vec!["kc-pass9".into(), basic.clone()],
        ),
    ];
    for (headers, repeated) in rounds {
        let echo = format!("invalid key {} end", repeated.join(" and "));
        for (stage, behaviour) in [
            (
                "initialize",
                json!({"init":{"http":{"status":401,"body":echo}}}),
            ),
            (
                "tools/list",
                json!({"list":{"http":{"status":500,"body":echo}}}),
            ),
        ] {
            let fake = Fake::start().await;
            *fake.behaviour.lock().unwrap() = behaviour;
            let egress = Egress::new(true).unwrap();
            let counter = std::sync::atomic::AtomicI64::new(0);
            let checked = check_key(&egress, &fake.url, &headers, &counter)
                .await
                .unwrap_err();
            let listed = list_tools(&egress, &fake.url, &headers, &counter)
                .await
                .unwrap_err();
            for (entry, why) in [("check_key", &checked), ("list_tools", &listed)] {
                let at = format!("{entry} / {stage} / {headers:?}");
                for secret in &repeated {
                    assert!(!has_token(why, secret), "{at}: {why}");
                }
                // The format stays: `MCP <status>: <body>`, with the credential taken out.
                let status = if stage == "initialize" { "401" } else { "500" };
                assert!(
                    why.starts_with(&format!("MCP {status}: invalid key ")),
                    "{at}: {why}"
                );
                assert!(
                    why.contains("[redacted]") && why.ends_with(" end"),
                    "{at}: {why}"
                );
            }
        }
    }
}

/// The clip is taken after the scrub: a key that crosses the 200-character cutoff leaves no prefix of itself.
#[tokio::test]
async fn a_key_crossing_the_clip_leaves_no_prefix() {
    use noevia_server::mcp_egress::check_key;
    let key = "SYNTH-api-secret-1234";
    for pad in [180, 190, 195, 199, 200] {
        let body = format!("{}{key}", "x".repeat(pad));
        let fake = Fake::start().await;
        *fake.behaviour.lock().unwrap() = json!({"init":{"http":{"status":401,"body":body}}});
        let counter = std::sync::atomic::AtomicI64::new(0);
        let why = check_key(
            &Egress::new(true).unwrap(),
            &fake.url,
            &[("X-Api-Key".into(), key.into())],
            &counter,
        )
        .await
        .unwrap_err();
        assert!(!why.contains("SYNTH"), "pad {pad}: {why}");
        assert!(why.starts_with("MCP 401: "), "{why}");
        assert!(why.chars().count() <= "MCP 401: ".len() + 200, "{why}");
    }
}

#[tokio::test]
async fn the_failure_log_line_is_the_status_and_nothing_the_server_said() {
    let fake = Fake::start().await;
    *fake.behaviour.lock().unwrap() = json!({"call":[{"http":{"status":502,"body":"hostile-body-text Bearer SYNTH-unlisted-0001"}}]});
    let (sink, logs) = lines();
    let exec = Executor::new(
        Settings {
            servers: vec![plain(&fake)],
            ..Settings::default()
        },
        &Egress::new(true).unwrap(),
    )
    .with_log(sink);
    let creds = creds_for(json!({"kind":"local"}), Value::Null, Value::Null, vec![]);
    let text = exec.run(&*creds, simple_call()).await;
    assert_eq!(text, "ERROR calling t1: tool call failed (HTTP 502)");
    let log = logs.lock().unwrap().clone();
    assert_eq!(log, ["[mcp] t1 failed: HTTP 502"]);
}

// ── a number the server chooses is as much an echo as a sentence (review r3: numeric upstream error paths) ──

/// A key of digits, or a word like `true`, repeated by the server as a JSON-RPC error code, a reply id or a malformed
/// `content`: the key check, the listing and the executor all take it out.
#[tokio::test]
async fn a_credential_that_is_a_number_is_not_echoed_by_a_protocol_error() {
    use noevia_server::mcp_egress::{check_key, list_tools};
    for key in ["12345", "7", "424242424242"] {
        let headers = [("X-Api-Key".to_owned(), key.to_owned())];
        for (stage, behaviour) in [
            (
                "initialize code",
                json!({"init":{"rpcError":{"code":key.parse::<i64>().unwrap(),"message":"no"}}}),
            ),
            (
                "initialize reply id",
                json!({"init":{"replyId":key.parse::<i64>().unwrap()}}),
            ),
            (
                "tools/list code",
                json!({"list":{"rpcError":{"code":key.parse::<i64>().unwrap(),"message":"no"}}}),
            ),
            (
                "tools/list reply id",
                json!({"list":{"replyId":key.parse::<i64>().unwrap()}}),
            ),
        ] {
            let fake = Fake::start().await;
            *fake.behaviour.lock().unwrap() = behaviour;
            let egress = Egress::new(true).unwrap();
            let counter = std::sync::atomic::AtomicI64::new(0);
            let checked = check_key(&egress, &fake.url, &headers, &counter)
                .await
                .unwrap_err();
            let listed = list_tools(&egress, &fake.url, &headers, &counter)
                .await
                .unwrap_err();
            for (entry, why) in [("check_key", &checked), ("list_tools", &listed)] {
                let at = format!("{entry} / {stage} / {key}: {why}");
                assert!(!has_token(why, key), "{at}");
                // The accepted shape stays, with the number redacted.
                assert!(
                    why.starts_with("MCP error [redacted]")
                        || why.starts_with("MCP: reply id [redacted] does not match request"),
                    "{at}"
                );
            }
        }
    }
}

/// The same for a call: a numeric or boolean credential the server repeats as `content`, as an error code or as a reply id.
#[tokio::test]
async fn an_executor_call_does_not_echo_a_credential_that_is_a_number_or_a_boolean() {
    for (token, env, content) in [
        ("12345", "SYNTH_NUM_TOKEN", json!(12345)),
        ("true", "SYNTH_BOOL_TOKEN", json!(true)),
        ("7", "SYNTH_ONE_TOKEN", json!(7)),
    ] {
        std::env::set_var(env, token);
        for (what, behaviour) in [
            ("content", json!({"call":[{"result":{"content":content}}]})),
            (
                "error code",
                json!({"call":[{"rpcError":{"code":content,"message":"no"}}]}),
            ),
        ] {
            let fake = Fake::start().await;
            *fake.behaviour.lock().unwrap() = behaviour;
            let (sink, logs) = lines();
            let exec = Executor::new(
                Settings {
                    servers: vec![Operator {
                        auth: Auth::Bearer,
                        token_env: Some(env.into()),
                        ..plain(&fake)
                    }],
                    ..Settings::default()
                },
                &Egress::new(true).unwrap(),
            )
            .with_log(sink);
            let creds = creds_for(json!({"kind":"local"}), Value::Null, Value::Null, vec![]);
            let text = exec.run(&*creds, simple_call()).await;
            assert!(text.starts_with("ERROR calling t1: "), "{what}: {text}");
            assert_clean(&format!("{what} / {token}"), &text, &logs, &[token]);
        }
        // A reply whose id is the credential.
        let fake = Fake::start().await;
        *fake.behaviour.lock().unwrap() = json!({"call":[{"replyId":12345}]});
        std::env::set_var("SYNTH_NUM_TOKEN", "12345");
        let exec = Executor::new(
            Settings {
                servers: vec![Operator {
                    auth: Auth::Bearer,
                    token_env: Some("SYNTH_NUM_TOKEN".into()),
                    ..plain(&fake)
                }],
                ..Settings::default()
            },
            &Egress::new(true).unwrap(),
        );
        let creds = creds_for(json!({"kind":"local"}), Value::Null, Value::Null, vec![]);
        let text = exec.run(&*creds, simple_call()).await;
        assert!(!has_token(&text, "12345"), "{text}");
    }
}
