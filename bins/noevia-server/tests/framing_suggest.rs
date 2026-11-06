//! `/api/chat-framing/suggest` through the front (chat port S4, `rust-chat-framing-suggest`): Node's gate
//! first, then every case of tests/fixtures/node-framing-suggest.v1.json (made by
//! tests/framing-suggest-oracle.cjs from core's own routes/chat-framing.cjs and http.cjs at
//! tests/fixtures/framing-suggest-oracle.core-ref) must be Node's answer byte for byte: status, content
//! type, cache control and body. The front's bridge to Node is a loopback stand-in that checks the
//! per-start secret and the signed-in user and applies the oracle's fake decision (no model runs), so
//! the body shows the message and chat id the front sent on. It refuses a body longer than the front
//! core's bridge limit (the fixture's `bridgeBodyLimit`, read from that core) with that core's 400, so a
//! public body the bridge could not take is caught. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use base64::Engine as _;
use js_json::raw;
use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use serde_json::Value;
use server_store::rusqlite::Connection;
use sha2::Digest as _;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-framing-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const PATH: &str = "/api/chat-framing/suggest";

fn fixture() -> Value {
    let path = std::env::var("NOEVIA_FRAMING_SUGGEST_FIXTURE").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/node-framing-suggest.v1.json"
        )
        .to_string()
    });
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// The oracle's fake decision.
    Decide,
    /// Node answers 502 (its bridge's failure).
    Fail,
    /// Node answers 200 with something that is not a JSON object.
    Garbage,
}

struct Node {
    addr: SocketAddr,
    mode: Arc<Mutex<Mode>>,
    /// (path, token, body) of each bridge call.
    seen: Arc<Mutex<Vec<(String, String, String)>>>,
}

async fn node(limit: usize) -> Node {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mode = Arc::new(Mutex::new(Mode::Decide));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (m, s) = (Arc::clone(&mode), Arc::clone(&seen));
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            let (m, s) = (Arc::clone(&m), Arc::clone(&s));
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 65536];
                let (head, body) = loop {
                    let n = c.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(split) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..split]).into_owned();
                        let want = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() - split - 4 >= want {
                            break (head, String::from_utf8(buf[split + 4..].to_vec()).unwrap());
                        }
                    }
                };
                let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
                let token = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("x-noevia-store-token: ")
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                s.lock().unwrap().push((path, token, body.clone()));
                let mode = *m.lock().unwrap();
                let (status, text) = match mode {
                    Mode::Fail => (
                        502,
                        r#"{"error":"The suggestion could not be made."}"#.to_owned(),
                    ),
                    Mode::Garbage => (200, "[]".to_owned()),
                    // Core's bridge: readJson(req, BRIDGE_BODY_LIMIT) throws 413, answered as its 400.
                    Mode::Decide if body.len() > limit => {
                        (400, r#"{"error":"invalid JSON"}"#.to_owned())
                    }
                    Mode::Decide => {
                        // The oracle's fake decision, over the body as JSON.parse reads it.
                        let Some(raw::Top::Object(fields)) = raw::read(&body) else {
                            panic!("the front sent {body}");
                        };
                        let value = |name: &str| {
                            fields
                                .iter()
                                .find(|f| raw::is_key(&f.key, name))
                                .map(|f| f.value.clone())
                                .unwrap()
                        };
                        (
                            200,
                            format!(
                                "{{\"frame\":{{\"kind\":\"question\",\"echo\":{},\"chatId\":{}}},\"reason\":null}}",
                                value("message"),
                                value("chatId")
                            ),
                        )
                    }
                };
                let out = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
                let _ = c.write_all(out.as_bytes()).await;
            });
        }
    });
    Node { addr, mode, seen }
}

struct Front {
    _dir: tempfile::TempDir,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
    node: Node,
}

async fn front() -> Front {
    front_with(noevia_server::chat_framing_suggest::BRIDGE_BODY_LIMIT).await
}

/// A front whose bridge takes bodies of at most `limit` bytes.
async fn front_with(limit: usize) -> Front {
    let node = node(limit).await;
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
    env.insert("NOEVIA_LEGACY_UPSTREAM", format!("http://{}", node.addr));
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
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic framing password","diaryEnabled":false}}"#
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
        addr,
        jar,
        csrf,
        user,
        node,
    }
}

/// One request: status, the response head (lower case) and the body bytes.
async fn call(
    f: &Front,
    method: &str,
    path: &str,
    body: &[u8],
    auth: bool,
    csrf: bool,
) -> (u16, String, Vec<u8>) {
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\norigin: {ORIGIN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n", body.len());
    if auth {
        req.push_str(&format!("cookie: {}\r\n", f.jar));
    }
    if csrf {
        req.push_str(&format!("x-csrf-token: {}\r\n", f.csrf));
    }
    req.push_str("\r\n");
    let mut bytes = req.into_bytes();
    bytes.extend_from_slice(body);
    c.write_all(&bytes).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(60), c.read_to_end(&mut out))
        .await
        .unwrap()
        .unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(out[..split].to_vec()).unwrap();
    let status = head[9..12].parse().unwrap();
    (status, head.to_ascii_lowercase(), out[split + 4..].to_vec())
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        l.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix(": "))
            .map(str::trim_end)
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn body_of(spec: &Value) -> Vec<u8> {
    if spec.is_null() {
        return Vec::new();
    }
    if let Some(text) = spec.get("text").and_then(Value::as_str) {
        return text.as_bytes().to_vec();
    }
    if let Some(hex) = spec.get("hex").and_then(Value::as_str) {
        return (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
    }
    if let Some(n) = spec.get("chatId").and_then(Value::as_u64) {
        let byte = u8::from_str_radix(spec["byte"].as_str().unwrap(), 16).unwrap();
        let mut out = br#"{"message":"m","chatId":""#.to_vec();
        out.extend(std::iter::repeat_n(byte, n as usize));
        out.extend_from_slice(br#""}"#);
        return out;
    }
    let n = spec["big"].as_u64().unwrap() as usize;
    format!("{{\"message\":\"{}\"}}", "a".repeat(n)).into_bytes()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_oracle_case_is_nodes_answer_through_the_front() {
    let fx = fixture();
    assert_eq!(fx["v"], 1);
    let cases = fx["cases"].as_array().unwrap();
    assert!(cases.len() >= 53, "{}", cases.len());
    // The front core's own bridge limit: the one this front is built for.
    let limit = usize::try_from(fx["bridgeBodyLimit"].as_u64().unwrap()).unwrap();
    assert_eq!(
        limit,
        noevia_server::chat_framing_suggest::BRIDGE_BODY_LIMIT,
        "the front core's bridge limit"
    );
    let mut failures = Vec::new();
    for c in cases {
        // A fresh front per case: the budget is per process, as Node's was.
        let f = front_with(limit).await;
        let id = c["id"].as_str().unwrap();
        for (i, step) in c["steps"].as_array().unwrap().iter().enumerate() {
            let method = step["method"].as_str().unwrap();
            let body = body_of(&step["body"]);
            let (status, head, got) = call(&f, method, PATH, &body, true, true).await;
            let node = &step["node"];
            // A long reply is recorded as its SHA-256 and length.
            let (got, want) = match node["body"].as_str() {
                Some(b64) => (
                    got,
                    base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .unwrap(),
                ),
                None => (
                    format!("{} {}", got.len(), hex(&sha2::Sha256::digest(&got))).into_bytes(),
                    format!(
                        "{} {}",
                        node["bodyLength"],
                        node["bodySha256"].as_str().unwrap()
                    )
                    .into_bytes(),
                ),
            };
            let mut wrong = Vec::new();
            if u64::from(status) != node["status"].as_u64().unwrap() {
                wrong.push(format!("status {status} != {}", node["status"]));
            }
            for h in ["content-type", "cache-control"] {
                let expected = node["headers"][h].as_str().map(str::to_ascii_lowercase);
                if header(&head, h) != expected.as_deref() {
                    wrong.push(format!("{h} {:?} != {expected:?}", header(&head, h)));
                }
            }
            if got != want {
                wrong.push(format!(
                    "body {:?} != {:?}",
                    String::from_utf8_lossy(&got)
                        .chars()
                        .take(200)
                        .collect::<String>(),
                    String::from_utf8_lossy(&want)
                        .chars()
                        .take(200)
                        .collect::<String>()
                ));
            }
            if !wrong.is_empty() {
                failures.push(format!("{id}[{i}] {method}: {}", wrong.join("; ")));
            }
        }
        // Every bridge call carried the secret and the signed-in user, and only answered POSTs called it.
        let seen = f.node.seen.lock().unwrap().clone();
        let answered = c["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["node"]["status"] == 200)
            .count();
        assert_eq!(seen.len(), answered, "{id}: bridge calls");
        for (path, token, body) in seen {
            assert_eq!(path, "/internal/chat-framing/suggest", "{id}");
            assert_eq!(token, STORE_TOKEN.to_ascii_lowercase(), "{id}");
            let Some(raw::Top::Object(fields)) = raw::read(&body) else {
                panic!("{id}: {body}");
            };
            let keys: Vec<&str> = fields.iter().map(|f| f.key.as_str()).collect();
            assert_eq!(keys, ["\"user\"", "\"message\"", "\"chatId\""], "{id}");
            assert_eq!(fields[0].value, raw::quote(&f.user), "{id}");
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_gate_comes_first_and_a_failed_bridge_fails_open() {
    let f = front().await;
    let post = br#"{"message":"m"}"#;
    // Node's gate: signed out is its 401 (with the realm), whatever the method; a write without CSRF is its 403.
    for method in ["GET", "POST", "PUT"] {
        let (status, head, body) = call(&f, method, PATH, post, false, false).await;
        assert_eq!(status, 401, "{method}");
        assert_eq!(body, br#"{"error":"unauthorized"}"#);
        assert_eq!(
            header(&head, "www-authenticate"),
            Some("bearer realm=\"cowork\"")
        );
    }
    let (status, _, body) = call(&f, "POST", PATH, post, true, false).await;
    assert_eq!(
        (status, body.as_slice()),
        (403, &br#"{"error":"invalid CSRF token"}"#[..])
    );
    // A signed-in GET without CSRF is the 405 (the probe several core tests use).
    let (status, head, body) = call(&f, "GET", PATH, b"", true, false).await;
    assert_eq!(
        (status, body.as_slice()),
        (405, &br#"{"error":"method not allowed"}"#[..])
    );
    assert_eq!(header(&head, "x-noevia-api"), Some("1"));
    let (status, _, body) = call(&f, "HEAD", PATH, b"", true, false).await;
    assert_eq!(status, 405);
    assert!(body.is_empty());
    assert!(
        f.node.seen.lock().unwrap().is_empty(),
        "nothing reached Node"
    );
    // The bridge failing, or answering something that is not an object: the route's fail-open answer.
    for mode in [Mode::Fail, Mode::Garbage] {
        *f.node.mode.lock().unwrap() = mode;
        let (status, head, body) = call(&f, "POST", PATH, post, true, true).await;
        assert_eq!(status, 200);
        assert_eq!(body, br#"{"frame":null,"reason":"error"}"#);
        assert_eq!(header(&head, "content-type"), Some("application/json"));
    }
    // Other spellings are not this route: they go to Node as before.
    *f.node.mode.lock().unwrap() = Mode::Fail;
    let before = f.node.seen.lock().unwrap().len();
    let (status, _, _) = call(&f, "POST", "/api/chat-framing/suggest/", post, true, true).await;
    assert_eq!(status, 502);
    let seen = f.node.seen.lock().unwrap();
    assert_eq!(seen.len(), before + 1);
    assert_eq!(seen.last().unwrap().0, "/api/chat-framing/suggest/");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_committed_corpus_exchange_replays() {
    let f = front().await;
    let ex: Value = serde_json::from_str(include_str!(
        "../../../contracts/http/corpus/exchanges/000171-POST-api-chat-framing-suggest.json"
    ))
    .unwrap();
    let body = ex["request"]["body"]["json"].to_string();
    let (status, head, got) = call(&f, "POST", PATH, body.as_bytes(), true, true).await;
    assert_eq!(
        u64::from(status),
        ex["response"]["status"].as_u64().unwrap()
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&got).unwrap(),
        ex["response"]["body"]["json"]
    );
    for (k, v) in ex["response"]["headers"].as_object().unwrap() {
        assert_eq!(
            header(&head, k),
            v.as_str().map(str::to_ascii_lowercase).as_deref(),
            "{k}"
        );
    }
}

#[test]
fn the_oracle_core_is_pinned_and_the_capability_is_listed() {
    let pin = include_str!("fixtures/framing-suggest-oracle.core-ref").trim();
    assert!(
        pin.len() == 40 && pin.bytes().all(|b| b.is_ascii_hexdigit()),
        "{pin}"
    );
    assert!(noevia_server::FEATURES.contains(&"rust-chat-framing-suggest"));
}
