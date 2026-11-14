//! The in-app server's loopback listener over a real socket: core's `startInternalServer` request
//! rule (`server/mcp-internal.cjs`, its listener cases in `mcp-internal.test.cjs`) end to end, the
//! deadlines the Rust listener adds, and the loopback-only bind.
//!
//! Requests are written by hand over a `TcpStream` (no HTTP client), so every byte of the exchange is
//! the test's: method, request target, `Authorization`, body size, a body that stops arriving.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;
use mcp_internal::listener::{self, Deadlines, Other, Server};
use mcp_internal::project_tools::{self, EditPlan, Expect, Hit, Ports, Project, ProjectFile};
use mcp_internal::{mint, Definition, Handler, Invoke, Mint, MAX_BODY_BYTES};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

const KEY: &[u8] = b"qa-key-listener-0123456789abcdef";
const USER: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const PROJECT: &str = "proj-qa";
/// Every exchange's own deadline (the test fails rather than hangs).
const CLIENT_DEADLINE: Duration = Duration::from_secs(10);

/// A project with one text file; `boom` as the project id panics inside the port.
struct Fake {
    written: Mutex<Vec<(String, String)>>,
}

impl Ports for Fake {
    fn get_project(&self, id: &JValue) -> Option<Project> {
        if id.as_str() == Some("boom") {
            panic!("a port that fails hard");
        }
        (id.as_str() == Some(PROJECT)).then(|| Project {
            id: PROJECT.into(),
            files: vec![ProjectFile {
                name: "notes.md".into(),
                content: Some("alpha".into()),
                size: Some(5),
                ..ProjectFile::default()
            }],
        })
    }
    fn read_project_file(&self, _: &Project, _: &JValue) -> Result<Option<String>, String> {
        Ok(Some("alpha".into()))
    }
    fn rag_available(&self) -> bool {
        false
    }
    fn search(&self, _: &str, _: &str, _: &JValue) -> Result<Vec<Hit>, String> {
        Ok(vec![])
    }
    fn write_text_file(
        &self,
        _: &Project,
        name: &str,
        text: &str,
        _: Option<&Expect>,
    ) -> Result<(), String> {
        self.written
            .lock()
            .unwrap()
            .push((name.into(), text.into()));
        Ok(())
    }
    fn upload_path_for(&self, _: &Project, name: &str) -> String {
        name.into()
    }
    fn plan_edit(&self, _: &Project, _: &JValue) -> Result<EditPlan, String> {
        Err("not used here".into())
    }
    fn target_digest(&self, _: &str, _: Option<&str>) -> String {
        String::new()
    }
}

/// The catalogue: core's six project tools, then tools of another group: `other_echo` (a read), one
/// that fails with an empty message (`other_empty_error`) and one that takes 600 ms (`other_slow`).
fn definitions() -> Vec<Definition> {
    let mut defs = project_tools::definitions();
    for name in ["other_echo", "other_empty_error", "other_slow"] {
        defs.push(Definition {
            name: name.into(),
            description: Some("another group's tool".into()),
            schema: None,
            write: false,
        });
    }
    defs
}

struct Running {
    addr: SocketAddr,
    server: Arc<Server>,
    others: Arc<Mutex<Vec<String>>>,
    fake: Arc<Fake>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

async fn start(deadlines: Deadlines, path: Option<&str>) -> Running {
    let fake = Arc::new(Fake {
        written: Mutex::new(vec![]),
    });
    let others = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = Arc::clone(&others);
    let other: Arc<Other> = Arc::new(move |inv: &Invoke| {
        seen.lock().unwrap().push(inv.name.clone());
        match inv.name.as_str() {
            "other_slow" => {
                std::thread::sleep(Duration::from_millis(600));
                Ok(Some("slow answer".into()))
            }
            "other_echo" => Ok(Some(format!(
                "echo for {}",
                js_json::to_js_string(&inv.ctx.user_id).unwrap()
            ))),
            _ => Err(String::new()),
        }
    });
    let mut server = Server::new(Handler::new(KEY, definitions()), fake.clone(), other)
        .with_deadlines(deadlines);
    if let Some(path) = path {
        server = server.with_path(path);
    }
    let server = Arc::new(server);
    let socket = listener::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = socket.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(listener::run(socket, Arc::clone(&server), async move {
        let _ = stopped.await;
    }));
    Running {
        addr,
        server,
        others,
        fake,
        stop: Some(stop),
        task,
    }
}

fn token(write: bool, discovery: bool, jti: &str) -> String {
    mint(
        KEY,
        &Mint {
            uid: (!discovery).then_some(USER),
            pid: Some(PROJECT),
            cid: Some("chat-qa"),
            write,
            target: None,
            discovery,
            ttl_ms: None,
        },
        jti,
        listener::now_ms(),
    )
}

/// One parsed response: status, lower-cased headers, body text.
#[derive(Debug)]
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn parse(raw: &[u8]) -> Answer {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("a complete response head");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .map(|l| {
            let (k, v) = l.split_once(':').unwrap();
            (k.trim().to_ascii_lowercase(), v.trim().to_owned())
        })
        .collect();
    Answer {
        status,
        headers,
        body: body.to_owned(),
    }
}

/// Writes `raw`, then reads until the server closes (every request says `Connection: close`).
async fn exchange(addr: SocketAddr, raw: &[u8]) -> Vec<u8> {
    tokio::time::timeout(CLIENT_DEADLINE, async {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(raw).await.unwrap();
        let mut out = vec![];
        let _ = stream.read_to_end(&mut out).await;
        out
    })
    .await
    .expect("the listener answers within the client deadline")
}

fn request(method: &str, target: &str, auth: Option<&str>, body: &[u8]) -> Vec<u8> {
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(auth) = auth {
        head.push_str(&format!("Authorization: {auth}\r\n"));
    }
    head.push_str("\r\n");
    let mut raw = head.into_bytes();
    raw.extend_from_slice(body);
    raw
}

async fn post(addr: SocketAddr, token: &str, body: &str) -> Answer {
    parse(
        &exchange(
            addr,
            &request(
                "POST",
                "/mcp",
                Some(&format!("Bearer {token}")),
                body.as_bytes(),
            ),
        )
        .await,
    )
}

fn call(id: u32, name: &str, args: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_list_and_initialized_over_the_socket() {
    let run = start(Deadlines::default(), None).await;
    let t = token(false, true, "jti-init");

    let a = post(
        run.addr,
        &t,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
    )
    .await;
    assert_eq!(a.status, 200);
    assert_eq!(a.header("content-type"), Some("application/json"));
    assert_eq!(
        a.body,
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"noevia-internal","version":"1"}}}"#
    );
    assert_eq!(
        a.header("content-length"),
        Some(a.body.len().to_string().as_str())
    );

    // The discovery token lists (more than once: tools/list consumes nothing).
    for _ in 0..2 {
        let a = post(
            run.addr,
            &t,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        )
        .await;
        assert_eq!(a.status, 200);
        let tools = js_json::parse(&a.body).unwrap();
        let JValue::Arr(tools) = tools.get("result").get("tools").clone() else {
            panic!("a tool list: {}", a.body)
        };
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").as_str().unwrap())
            .collect();
        let mut want: Vec<&str> = project_tools::NAMES.to_vec();
        want.extend(["other_echo", "other_empty_error", "other_slow"]);
        assert_eq!(names, want);
    }

    // `notifications/initialized`: 202, an empty body.
    let a = post(
        run.addr,
        &t,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert_eq!(a.status, 202);
    assert_eq!(a.body, "");
    assert_eq!(a.header("content-length"), Some("0"));
    assert_eq!(a.header("content-type"), Some("application/json"));

    // ...but it may not call.
    let a = post(run.addr, &t, &call(3, "project_list_files", "{}")).await;
    assert_eq!(a.status, 403);
    assert_eq!(
        a.body,
        r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32002,"message":"this token may list tools but not call them"}}"#
    );
    assert!(run.others.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_request_rule_before_the_body() {
    let run = start(Deadlines::default(), None).await;
    let t = format!("Bearer {}", token(false, true, "jti-rule"));
    let list = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let not_found = r#"{"error":"not found"}"#;

    for (method, target) in [
        ("GET", "/mcp"),
        ("PUT", "/mcp"),
        ("POST", "/mcp/"),
        ("POST", "/other"),
        ("POST", "/MCP"),
        // `req.url` of an absolute-form target is the whole URL, so it is not the path.
        ("POST", "http://127.0.0.1/mcp"),
    ] {
        let a = parse(&exchange(run.addr, &request(method, target, Some(&t), list)).await);
        assert_eq!((method, target, a.status), (method, target, 404));
        assert_eq!(a.body, not_found);
        assert_eq!(a.header("content-type"), Some("application/json"));
    }

    // The query is ignored.
    let a = parse(&exchange(run.addr, &request("POST", "/mcp?x=1&y", Some(&t), list)).await);
    assert_eq!(a.status, 200);

    // Not JSON: 400 parse error with a null id.
    let a = parse(&exchange(run.addr, &request("POST", "/mcp", Some(&t), b"{nope")).await);
    assert_eq!(a.status, 400);
    assert_eq!(
        a.body,
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#
    );

    // No `Bearer ` (or another scheme): no token.
    for auth in [None, Some("Basic abc"), Some("bearer x"), Some("Bearer")] {
        let a = parse(&exchange(run.addr, &request("POST", "/mcp", auth, list)).await);
        assert_eq!(a.status, 401, "{auth:?}");
        assert_eq!(
            a.body,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"unauthorized: malformed"}}"#
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_path_is_the_only_path() {
    let run = start(Deadlines::default(), Some("/internal/mcp")).await;
    let t = format!("Bearer {}", token(false, true, "jti-path"));
    let list = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let a = parse(&exchange(run.addr, &request("POST", "/internal/mcp", Some(&t), list)).await);
    assert_eq!(a.status, 200);
    let a = parse(&exchange(run.addr, &request("POST", "/mcp", Some(&t), list)).await);
    assert_eq!(a.status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_body_limit_is_inclusive() {
    let run = start(Deadlines::default(), None).await;
    let t = format!("Bearer {}", token(false, true, "jti-limit"));
    let message = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    // Exactly MAX_BODY_BYTES (JSON padded with spaces) is read.
    let mut at = message.as_bytes().to_vec();
    at.resize(MAX_BODY_BYTES, b' ');
    let a = parse(&exchange(run.addr, &request("POST", "/mcp", Some(&t), &at)).await);
    assert_eq!(a.status, 200);
    // One byte more is 400 invalid request (core's intent; Node resets the socket, noevia#2783).
    at.push(b' ');
    let a = parse(&exchange(run.addr, &request("POST", "/mcp", Some(&t), &at)).await);
    assert_eq!(a.status, 400);
    assert_eq!(
        a.body,
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request"}}"#
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_run_once_with_identity_from_the_token() {
    let run = start(Deadlines::default(), None).await;

    // A project read.
    let t = token(false, false, "jti-read");
    let a = post(run.addr, &t, &call(1, "project_list_files", "{}")).await;
    assert_eq!(a.status, 200);
    let v = js_json::parse(&a.body).unwrap();
    let text = v.get("result").get("content");
    let JValue::Arr(items) = text else {
        panic!("{}", a.body)
    };
    let text = items[0].get("text").as_str().unwrap();
    assert!(text.contains("notes.md"), "{text}");
    assert_eq!(v.get("result").get("isError"), &JValue::Undefined);

    // The same token again: already used.
    let a = post(run.addr, &t, &call(2, "project_list_files", "{}")).await;
    assert_eq!(a.status, 401);
    assert_eq!(
        a.body,
        r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32004,"message":"this authorization has already been used"}}"#
    );

    // A write under a read token is refused before the guard (the token is not spent)...
    let t = token(false, false, "jti-write-unapproved");
    let a = post(
        run.addr,
        &t,
        &call(3, "project_create_file", r#"{"name":"new.md","text":"x"}"#),
    )
    .await;
    assert_eq!(a.status, 403);
    assert_eq!(
        a.body,
        r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32003,"message":"\"project_create_file\" changes data and was not approved"}}"#
    );
    assert!(run.fake.written.lock().unwrap().is_empty());
    // ...and still reads once.
    let a = post(run.addr, &t, &call(4, "project_list_files", "{}")).await;
    assert_eq!(a.status, 200);

    // An approved write reaches the port.
    let t = token(true, false, "jti-write");
    let a = post(
        run.addr,
        &t,
        &call(
            5,
            "project_create_file",
            r#"{"name":"new.md","text":"hello"}"#,
        ),
    )
    .await;
    assert_eq!(a.status, 200, "{}", a.body);
    assert_eq!(
        a.body,
        r#"{"jsonrpc":"2.0","id":5,"result":{"content":[{"type":"text","text":"Created \"new.md\" (5 chars)."}]}}"#
    );
    assert_eq!(
        run.fake.written.lock().unwrap().as_slice(),
        [("new.md".to_owned(), "hello".to_owned())]
    );

    // Outside the six: the caller's `other`, with the token's user.
    let t = token(false, false, "jti-other");
    let a = post(
        run.addr,
        &t,
        &call(6, "other_echo", r#"{"user":"someone-else"}"#),
    )
    .await;
    assert_eq!(
        a.body,
        format!(
            r#"{{"jsonrpc":"2.0","id":6,"result":{{"content":[{{"type":"text","text":"echo for {USER}"}}]}}}}"#
        )
    );
    // An empty error message reads `ERROR: Error`, as core's `(err && err.message) || err`.
    let t = token(false, false, "jti-other-error");
    let a = post(run.addr, &t, &call(7, "other_empty_error", "{}")).await;
    assert_eq!(
        a.body,
        r#"{"jsonrpc":"2.0","id":7,"result":{"content":[{"type":"text","text":"ERROR: Error"}],"isError":true}}"#
    );
    // Not in the catalogue: core's unknown-tool result, nothing called.
    let t = token(false, false, "jti-unknown");
    let a = post(run.addr, &t, &call(8, "no_such_tool", "{}")).await;
    assert_eq!(
        a.body,
        r#"{"jsonrpc":"2.0","id":8,"result":{"content":[{"type":"text","text":"ERROR: unknown tool \"no_such_tool\""}],"isError":true}}"#
    );
    assert_eq!(
        run.others.lock().unwrap().as_slice(),
        ["other_echo".to_owned(), "other_empty_error".to_owned()]
    );
    assert_eq!(run.server.handler().guard().size(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_that_panics_is_invalid_request_and_the_listener_lives_on() {
    let run = start(Deadlines::default(), None).await;
    let t = mint(
        KEY,
        &Mint {
            uid: Some(USER),
            pid: Some("boom"),
            cid: None,
            write: false,
            target: None,
            discovery: false,
            ttl_ms: None,
        },
        "jti-boom",
        listener::now_ms(),
    );
    let a = post(run.addr, &t, &call(1, "project_list_files", "{}")).await;
    assert_eq!(a.status, 400);
    assert_eq!(
        a.body,
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request"}}"#
    );
    let a = post(
        run.addr,
        &token(false, false, "jti-after"),
        &call(2, "project_list_files", "{}"),
    )
    .await;
    assert_eq!(a.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_wait_has_a_deadline() {
    let short = Duration::from_millis(300);
    let run = start(
        Deadlines {
            headers: short,
            body: short,
            connection: Duration::from_secs(2),
        },
        None,
    )
    .await;

    // A body that stops arriving: 400 invalid request after the body deadline.
    let started = Instant::now();
    let raw = b"POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nAuthorization: Bearer x\r\nContent-Length: 100\r\n\r\n{\"jsonrpc\"";
    let a = parse(&exchange(run.addr, raw).await);
    assert_eq!(a.status, 400);
    assert_eq!(
        a.body,
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request"}}"#
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    // A connection that never sends a head is closed (headers or connection deadline), never with a
    // tool answer.
    let started = Instant::now();
    let out = exchange(run.addr, b"").await;
    assert!(
        !String::from_utf8_lossy(&out).contains(" 200 "),
        "{}",
        String::from_utf8_lossy(&out)
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_alive_answers_each_request_in_turn() {
    let run = start(Deadlines::default(), None).await;
    let t = format!("Bearer {}", token(false, true, "jti-keep"));
    let one = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let two = br#"{"jsonrpc":"2.0","id":2,"method":"initialize"}"#;
    let mut raw = request("POST", "/mcp", Some(&t), one);
    // The first without `Connection: close`, so the second rides the same connection.
    let text = String::from_utf8(raw.clone())
        .unwrap()
        .replace("Connection: close\r\n", "");
    raw = text.into_bytes();
    raw.extend(request("POST", "/mcp", Some(&t), two));
    let out = String::from_utf8(exchange(run.addr, &raw).await).unwrap();
    assert_eq!(out.matches("HTTP/1.1 200 OK").count(), 2, "{out}");
    let first = out.find(r#""id":1"#).unwrap();
    let second = out.find(r#""id":2"#).unwrap();
    assert!(first < second);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binds_loopback_only_and_stops_on_shutdown() {
    for addr in ["0.0.0.0:0", "[::]:0", "10.0.0.1:0", "192.168.1.2:0"] {
        let err = listener::bind(addr.parse().unwrap()).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{addr}");
    }
    let mut run = start(Deadlines::default(), None).await;
    let addr = run.addr;
    run.stop.take().unwrap().send(()).unwrap();
    tokio::time::timeout(CLIENT_DEADLINE, &mut run.task)
        .await
        .expect("run returns after shutdown")
        .unwrap()
        .unwrap();
    // The socket is closed with it.
    assert!(TcpStream::connect(addr).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_in_flight_outlives_the_connection_deadline() {
    let run = start(
        Deadlines {
            connection: Duration::from_millis(300),
            ..Deadlines::default()
        },
        None,
    )
    .await;
    let t = token(false, false, "jti-slow");
    let a = post(run.addr, &t, &call(1, "other_slow", "{}")).await;
    assert_eq!(a.status, 200);
    assert_eq!(
        a.body,
        r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"slow answer"}]}}"#
    );
}
