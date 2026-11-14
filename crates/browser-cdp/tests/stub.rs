//! The engine against an in-process stub DevTools endpoint on 127.0.0.1: a scripted server side of
//! the WebSocket that records every message the adapter sends. Synthetic QA data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use base64::Engine as _;
use browser_cdp::CdpLaunch;
use browser_task::engine::{Launch, Page, Proxy, Request, Route, Socket};
use js_json::JValue;
use sha1::{Digest, Sha1};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D,
];

fn s(t: &str) -> JValue {
    JValue::Str(t.into())
}

fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::obj(pairs)
}

fn json(v: &JValue) -> String {
    js_json::stringify(v).unwrap()
}

/// What the script sends back for one message: replies and events, in order; `drop` ends the
/// connection instead.
#[derive(Default)]
struct Out {
    messages: Vec<String>,
    drop: bool,
}

impl Out {
    fn reply(&mut self, msg: &JValue, result: JValue) {
        let mut items = vec![("id", msg.get("id").clone()), ("result", result)];
        if let Some(sid) = msg.get("sessionId").as_str() {
            items.push(("sessionId", s(sid)));
        }
        self.messages.push(json(&obj(items)));
    }

    fn reply_to(&mut self, id: f64, session: Option<&str>, result: JValue) {
        let mut items = vec![("id", JValue::Num(id)), ("result", result)];
        if let Some(sid) = session {
            items.push(("sessionId", s(sid)));
        }
        self.messages.push(json(&obj(items)));
    }

    fn event(&mut self, session: Option<&str>, method: &str, params: JValue) {
        let mut items = vec![("method", s(method)), ("params", params)];
        if let Some(sid) = session {
            items.push(("sessionId", s(sid)));
        }
        self.messages.push(json(&obj(items)));
    }
}

type Script = Box<dyn FnMut(&JValue, &mut Out) + Send>;

struct Stub {
    port: u16,
    log: Arc<Mutex<Vec<JValue>>>,
    heads: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Stub {
    fn start(script: Script) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let (l, h) = (log.clone(), heads.clone());
        let thread = std::thread::spawn(move || serve(listener, script, l, h));
        Stub {
            port,
            log,
            heads,
            thread: Some(thread),
        }
    }

    /// Every message but the pong (which races the first call).
    fn messages(&self) -> Vec<JValue> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.get("method").as_str() != Some("(pong)"))
            .cloned()
            .collect()
    }

    fn methods(&self) -> Vec<String> {
        self.messages()
            .iter()
            .map(|m| m.get("method").as_str().unwrap().to_string())
            .collect()
    }

    fn sent(&self, method: &str) -> Vec<JValue> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.get("method").as_str() == Some(method))
            .cloned()
            .collect()
    }

    /// Waits (at most 5 s) until `n` messages of `method` were sent.
    fn wait_for(&self, method: &str, n: usize) -> Vec<JValue> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let found = self.sent(method);
            if found.len() >= n || Instant::now() > deadline {
                return found;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn join(mut self) {
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

fn read_head(sock: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        sock.read_exact(&mut b).unwrap();
        head.push(b[0]);
    }
    String::from_utf8(head).unwrap()
}

fn header(head: &str, name: &str) -> String {
    head.split("\r\n")
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.trim().eq_ignore_ascii_case(name))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_default()
}

/// A client frame (must be masked), unmasked.
fn read_frame(sock: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    sock.read_exact(&mut h).ok()?;
    assert!(h[0] & 0x80 != 0, "client frames are final");
    assert!(h[1] & 0x80 != 0, "client frames are masked");
    let len = match h[1] & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            sock.read_exact(&mut b).ok()?;
            u16::from_be_bytes(b) as usize
        }
        127 => {
            let mut b = [0u8; 8];
            sock.read_exact(&mut b).ok()?;
            u64::from_be_bytes(b) as usize
        }
        n => n as usize,
    };
    let mut mask = [0u8; 4];
    sock.read_exact(&mut mask).ok()?;
    let mut payload = vec![0u8; len];
    sock.read_exact(&mut payload).ok()?;
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    Some((h[0] & 0x0F, payload))
}

fn write_frame(sock: &mut TcpStream, opcode: u8, payload: &[u8]) {
    let mut out = vec![0x80 | opcode];
    if payload.len() < 126 {
        out.push(payload.len() as u8);
    } else if payload.len() <= 0xFFFF {
        out.push(126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    let _ = sock.write_all(&out);
}

fn serve(
    listener: TcpListener,
    mut script: Script,
    log: Arc<Mutex<Vec<JValue>>>,
    heads: Arc<Mutex<Vec<String>>>,
) {
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let mut sock = loop {
            match listener.accept() {
                Ok((sock, _)) => break sock,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => panic!("no client: {e}"),
            }
        };
        sock.set_nonblocking(false).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let head = read_head(&mut sock);
        heads.lock().unwrap().push(head.clone());
        if !header(&head, "upgrade").eq_ignore_ascii_case("websocket") {
            // /json/version: the address it names is used for its path only.
            let body = format!("{{\"Browser\":\"QA/1.0\",\"webSocketDebuggerUrl\":\"ws://qa-browser.invalid:{port}/devtools/browser/qa-1\"}}");
            let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            sock.write_all(reply.as_bytes()).unwrap();
            continue;
        }
        let mut hash = Sha1::new();
        hash.update(header(&head, "sec-websocket-key").as_bytes());
        hash.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        let accept = base64::engine::general_purpose::STANDARD.encode(hash.finalize());
        let reply = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n");
        sock.write_all(reply.as_bytes()).unwrap();
        // A ping first: the adapter answers it.
        write_frame(&mut sock, 0x9, b"qa");
        while let Some((opcode, payload)) = read_frame(&mut sock) {
            match opcode {
                0x8 => break,
                0xA => {
                    log.lock().unwrap().push(obj(vec![
                        ("method", s("(pong)")),
                        ("params", s(&String::from_utf8_lossy(&payload))),
                    ]));
                    continue;
                }
                _ => {}
            }
            let msg = js_json::parse(std::str::from_utf8(&payload).unwrap()).unwrap();
            log.lock().unwrap().push(msg.clone());
            let mut out = Out::default();
            script(&msg, &mut out);
            if out.drop {
                let _ = sock.shutdown(Shutdown::Both);
                return;
            }
            for m in out.messages {
                write_frame(&mut sock, 0x1, m.as_bytes());
            }
        }
        return;
    }
}

fn attached(session: &str, target: &str, context: &str) -> JValue {
    obj(vec![
        ("sessionId", s(session)),
        (
            "targetInfo",
            obj(vec![
                ("targetId", s(target)),
                ("type", s("page")),
                ("browserContextId", s(context)),
                ("url", s("about:blank")),
                ("attached", JValue::Bool(true)),
            ]),
        ),
        ("waitingForDebugger", JValue::Bool(true)),
    ])
}

/// The answers every test shares: one context, one page `S1`/`T1`/`F1`; `{}` for the rest.
fn common(msg: &JValue, out: &mut Out) {
    let sid = msg.get("sessionId").as_str();
    match msg.get("method").as_str().unwrap() {
        "Target.createBrowserContext" => out.reply(msg, obj(vec![("browserContextId", s("CTX1"))])),
        "Target.createTarget" => {
            out.event(
                None,
                "Target.attachedToTarget",
                attached("S1", "T1", "CTX1"),
            );
            out.reply(msg, obj(vec![("targetId", s("T1"))]));
        }
        "Page.getFrameTree" => {
            let frame = format!("F{}", &sid.unwrap()[1..]);
            out.reply(
                msg,
                obj(vec![(
                    "frameTree",
                    obj(vec![(
                        "frame",
                        obj(vec![
                            ("id", s(&frame)),
                            ("loaderId", s("L0")),
                            ("url", s("about:blank")),
                        ]),
                    )]),
                )]),
            )
        }
        _ => out.reply(msg, obj(vec![])),
    }
}

fn proxy() -> Proxy {
    Proxy {
        server: "http://egress.qa.test:3128".into(),
        username: "qa-grant".into(),
        password: "qa-pass".into(),
    }
}

type Seen = Arc<Mutex<Vec<Request>>>;

fn gate(seen: &Seen) -> browser_task::engine::RouteGate {
    let seen = seen.clone();
    Arc::new(move |req: &Request| {
        seen.lock().unwrap().push(req.clone());
        if req.url.contains("blocked.invalid") {
            Route::Abort
        } else {
            Route::Continue
        }
    })
}

#[test]
fn drives_a_session_and_sends_the_expected_protocol() {
    let mut evaluations = 0;
    let mut navigation: Option<f64> = None;
    let stub = Stub::start(Box::new(move |msg, out| {
        let method = msg.get("method").as_str().unwrap();
        let params = msg.get("params");
        let sid = msg.get("sessionId").as_str();
        match method {
            "Page.navigate" => {
                navigation = msg.get("id").as_num();
                let request = obj(vec![
                    ("url", params.get("url").clone()),
                    ("method", s("GET")),
                ]);
                let id = if params.get("url").as_str().unwrap().contains("blocked") {
                    "R3"
                } else {
                    "R1"
                };
                out.event(
                    sid,
                    "Fetch.requestPaused",
                    obj(vec![
                        ("requestId", s(id)),
                        ("request", request),
                        ("frameId", s("F1")),
                        ("resourceType", s("Document")),
                    ]),
                );
            }
            "Fetch.continueRequest" if params.get("requestId").as_str() == Some("R1") => {
                out.reply(msg, obj(vec![]));
                out.event(
                    sid,
                    "Page.frameNavigated",
                    obj(vec![
                        (
                            "frame",
                            obj(vec![
                                ("id", s("F1")),
                                ("loaderId", s("L1")),
                                ("url", s("http://qa.test/")),
                                ("urlFragment", s("#top")),
                            ]),
                        ),
                        ("type", s("Navigation")),
                    ]),
                );
                let pixel = obj(vec![
                    ("url", s("http://blocked.invalid/pixel.png")),
                    ("method", s("GET")),
                ]);
                out.event(
                    sid,
                    "Fetch.requestPaused",
                    obj(vec![
                        ("requestId", s("R2")),
                        ("request", pixel),
                        ("frameId", s("F1")),
                        ("resourceType", s("Image")),
                    ]),
                );
                out.event(
                    sid,
                    "Page.loadEventFired",
                    obj(vec![("timestamp", JValue::Num(1.0))]),
                );
                out.reply_to(
                    navigation.take().unwrap(),
                    sid,
                    obj(vec![("frameId", s("F1")), ("loaderId", s("L1"))]),
                );
            }
            "Fetch.failRequest" if params.get("requestId").as_str() == Some("R3") => {
                out.reply(msg, obj(vec![]));
                let failed = obj(vec![
                    ("frameId", s("F1")),
                    ("loaderId", s("L3")),
                    ("errorText", s("net::ERR_BLOCKED_BY_CLIENT")),
                ]);
                out.reply_to(navigation.take().unwrap(), sid, failed);
            }
            "Runtime.evaluate" => {
                evaluations += 1;
                let result = if evaluations == 1 {
                    obj(vec![
                        ("type", s("object")),
                        ("subtype", s("null")),
                        ("value", JValue::Null),
                    ])
                } else {
                    obj(vec![
                        ("type", s("object")),
                        ("subtype", s("node")),
                        ("className", s("HTMLButtonElement")),
                        ("objectId", s("O1")),
                    ])
                };
                out.reply(msg, obj(vec![("result", result)]));
            }
            "Runtime.callFunctionOn" => {
                let f = params.get("functionDeclaration").as_str().unwrap();
                let value = if f.starts_with("function describeInPage(target) {") {
                    obj(vec![
                        ("tag", s("button")),
                        ("type", s("submit")),
                        ("role", s("")),
                        ("name", s("")),
                        ("text", s("Send")),
                        ("value", s("")),
                        ("inForm", JValue::Bool(true)),
                        ("formMethod", s("post")),
                        ("formAction", s("http://qa.test/done")),
                        ("frameOrigin", s("http://qa.test")),
                    ])
                } else {
                    obj(vec![
                        ("x", JValue::Num(60.0)),
                        ("y", JValue::Num(30.0)),
                        ("width", JValue::Num(100.0)),
                        ("height", JValue::Num(40.0)),
                    ])
                };
                out.reply(
                    msg,
                    obj(vec![(
                        "result",
                        obj(vec![("type", s("object")), ("value", value)]),
                    )]),
                );
            }
            "Page.captureScreenshot" => {
                out.reply(
                    msg,
                    obj(vec![(
                        "data",
                        s(&base64::engine::general_purpose::STANDARD.encode(PNG)),
                    )]),
                );
            }
            _ => common(msg, out),
        }
    }));

    let launch = CdpLaunch::new(format!("http://127.0.0.1:{}", stub.port));
    let browser = launch.launch().unwrap();
    let context = browser.new_context(Some(&proxy())).unwrap();
    let seen: Seen = Arc::default();
    context.route(gate(&seen)).unwrap();
    context
        .route_websocket(Arc::new(|_: &str| Socket::Connect))
        .unwrap();
    let hooked = Arc::new(AtomicUsize::new(0));
    let h = hooked.clone();
    context.on_page(Arc::new(move |page: Arc<dyn Page>| {
        assert_eq!(page.url(), "about:blank");
        h.fetch_add(1, Ordering::SeqCst);
    }));
    let page = context.new_page().unwrap();
    assert_eq!(
        hooked.load(Ordering::SeqCst),
        1,
        "the page hook runs before new_page returns"
    );
    assert_eq!(context.pages().len(), 1);
    assert_eq!(context.pages()[0].id(), page.id());

    page.goto("http://qa.test/", Duration::from_secs(5))
        .unwrap();
    assert_eq!(page.url(), "http://qa.test/#top");
    let refused = page
        .goto("http://blocked.invalid/", Duration::from_secs(5))
        .unwrap_err();
    assert_eq!(
        refused.message,
        "net::ERR_BLOCKED_BY_CLIENT at http://blocked.invalid/"
    );
    let asked: Vec<(String, bool)> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|r| (r.url.clone(), r.main_frame_navigation))
        .collect();
    assert_eq!(
        asked,
        [
            ("http://qa.test/".to_string(), true),
            ("http://blocked.invalid/pixel.png".into(), false),
            ("http://blocked.invalid/".into(), true)
        ]
    );

    let handle = page
        .locate("form > button[type=\"submit\"]", Duration::from_secs(5))
        .unwrap()
        .expect("found on the second poll");
    let facts = handle.describe().unwrap();
    assert_eq!(facts.get("tag").as_str(), Some("button"));
    assert_eq!(facts.get("inForm").as_bool(), Some(true));
    let keys: Vec<&str> = match &facts {
        JValue::Obj(items) => items.iter().map(|(k, _)| k.as_str()).collect(),
        _ => panic!("facts"),
    };
    assert_eq!(
        keys,
        [
            "tag",
            "type",
            "role",
            "name",
            "text",
            "value",
            "inForm",
            "formMethod",
            "formAction",
            "frameOrigin"
        ]
    );
    handle.click(Duration::from_secs(5)).unwrap();
    assert_eq!(&page.screenshot().unwrap(), PNG);
    handle.dispose().unwrap();
    context.close().unwrap();
    browser.close().unwrap();

    let pong = stub.sent("(pong)");
    assert_eq!(pong.len(), 1);
    assert_eq!(
        pong[0].get("params").as_str(),
        Some("qa"),
        "the ping's payload comes back"
    );
    let methods = stub.methods();
    let expected = [
        "Target.setAutoAttach",
        "Target.createBrowserContext",
        "Browser.setDownloadBehavior",
        "Target.createTarget",
        "Page.enable",
        "Network.enable",
        "Network.setBypassServiceWorker",
        "Fetch.enable",
        "Target.setAutoAttach",
        "Page.getFrameTree",
        "Runtime.runIfWaitingForDebugger",
        "Page.navigate",
        "Fetch.continueRequest",
        "Fetch.failRequest",
        "Page.navigate",
        "Fetch.failRequest",
        "Runtime.evaluate",
        "Runtime.evaluate",
        "Runtime.callFunctionOn",
        "Runtime.callFunctionOn",
        "Input.dispatchMouseEvent",
        "Input.dispatchMouseEvent",
        "Input.dispatchMouseEvent",
        "Page.captureScreenshot",
        "Runtime.releaseObject",
        "Target.disposeBrowserContext",
    ];
    assert_eq!(methods, expected);
    let log = stub.messages();
    let p = |i: usize| log[i].get("params").clone();
    let sid = |i: usize| log[i].get("sessionId").as_str().map(str::to_string);
    assert_eq!(
        json(&p(0)),
        r#"{"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}"#
    );
    assert_eq!(sid(0), None);
    assert_eq!(
        json(&p(1)),
        r#"{"disposeOnDetach":true,"proxyServer":"http://egress.qa.test:3128","proxyBypassList":"<-loopback>"}"#
    );
    assert_eq!(
        json(&p(2)),
        r#"{"behavior":"deny","browserContextId":"CTX1"}"#
    );
    assert_eq!(
        json(&p(3)),
        r#"{"url":"about:blank","browserContextId":"CTX1"}"#
    );
    for (i, method) in methods.iter().enumerate().take(11).skip(4) {
        assert_eq!(sid(i).as_deref(), Some("S1"), "{method}");
    }
    assert_eq!(json(&p(6)), r#"{"bypass":true}"#);
    assert_eq!(
        json(&p(7)),
        r#"{"patterns":[{"urlPattern":"*","requestStage":"Request"}],"handleAuthRequests":true}"#
    );
    assert_eq!(
        json(&p(8)),
        r#"{"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}"#
    );
    assert_eq!(json(&p(11)), r#"{"url":"http://qa.test/"}"#);
    assert_eq!(json(&p(12)), r#"{"requestId":"R1"}"#);
    assert_eq!(
        json(&p(13)),
        r#"{"requestId":"R2","errorReason":"BlockedByClient"}"#
    );
    assert_eq!(
        json(&p(15)),
        r#"{"requestId":"R3","errorReason":"BlockedByClient"}"#
    );
    assert_eq!(
        p(16).get("expression").as_str(),
        Some(r#"document.querySelector("form > button[type=\"submit\"]")"#)
    );
    assert_eq!(p(16).get("returnByValue").as_bool(), Some(false));
    let describe = p(18);
    assert!(describe
        .get("functionDeclaration")
        .as_str()
        .unwrap()
        .contains("frameOrigin: location.origin,"));
    assert_eq!(describe.get("objectId").as_str(), Some("O1"));
    assert_eq!(json(describe.get("arguments")), r#"[{"objectId":"O1"}]"#);
    assert_eq!(describe.get("returnByValue").as_bool(), Some(true));
    assert!(p(19)
        .get("functionDeclaration")
        .as_str()
        .unwrap()
        .contains("scrollIntoView({ block: 'center', inline: 'center'"));
    assert_eq!(
        json(&p(20)),
        r#"{"type":"mouseMoved","x":60,"y":30,"button":"none","buttons":0,"modifiers":0}"#
    );
    assert_eq!(
        json(&p(21)),
        r#"{"type":"mousePressed","x":60,"y":30,"button":"left","buttons":1,"modifiers":0,"clickCount":1}"#
    );
    assert_eq!(
        json(&p(22)),
        r#"{"type":"mouseReleased","x":60,"y":30,"button":"left","buttons":0,"modifiers":0,"clickCount":1}"#
    );
    assert_eq!(json(&p(23)), r#"{"format":"png"}"#);
    assert_eq!(json(&p(24)), r#"{"objectId":"O1"}"#);
    assert_eq!(json(&p(25)), r#"{"browserContextId":"CTX1"}"#);
    assert_eq!(sid(25), None);
    // /json/version first, then the upgrade to the path it named, on the configured host.
    let heads = stub.heads.lock().unwrap().clone();
    assert!(
        heads[0].starts_with("GET /json/version HTTP/1.1\r\n"),
        "{}",
        heads[0]
    );
    assert!(
        heads[1].starts_with("GET /devtools/browser/qa-1 HTTP/1.1\r\n"),
        "{}",
        heads[1]
    );
    for head in &heads {
        assert_eq!(header(head, "host"), format!("127.0.0.1:{}", stub.port));
        assert_eq!(header(head, "origin"), "");
    }
    stub.join();
}

#[test]
fn proxy_auth_popups_and_foreign_pages() {
    let stub = Stub::start(Box::new(move |msg, out| {
        let sid = msg.get("sessionId").as_str();
        if msg.get("method").as_str() == Some("Runtime.runIfWaitingForDebugger")
            && sid == Some("S1")
        {
            out.reply(msg, obj(vec![]));
            let challenge = |source: &str| {
                obj(vec![
                    ("source", s(source)),
                    ("origin", s("http://egress.qa.test:3128")),
                    ("scheme", s("basic")),
                ])
            };
            let request = obj(vec![("url", s("http://qa.test/")), ("method", s("GET"))]);
            for (id, source) in [("A1", "Proxy"), ("A2", "Server")] {
                out.event(
                    sid,
                    "Fetch.authRequired",
                    obj(vec![
                        ("requestId", s(id)),
                        ("request", request.clone()),
                        ("frameId", s("F1")),
                        ("authChallenge", challenge(source)),
                    ]),
                );
            }
            out.event(
                None,
                "Target.attachedToTarget",
                attached("S9", "T9", "SOMEONE-ELSE"),
            );
            out.event(
                None,
                "Target.attachedToTarget",
                attached("S2", "T2", "CTX1"),
            );
            // The same popup reported again (by the opener's own auto-attach) is set up once.
            out.event(sid, "Target.attachedToTarget", attached("S3", "T2", "CTX1"));
            return;
        }
        common(msg, out)
    }));
    let browser = CdpLaunch::new(format!(
        "ws://127.0.0.1:{}/devtools/browser/qa-1",
        stub.port
    ))
    .launch()
    .unwrap();
    let context = browser.new_context(Some(&proxy())).unwrap();
    let seen: Seen = Arc::default();
    context.route(gate(&seen)).unwrap();
    let pages: Arc<Mutex<Vec<Arc<dyn Page>>>> = Arc::default();
    let p = pages.clone();
    context.on_page(Arc::new(move |page: Arc<dyn Page>| {
        p.lock().unwrap().push(page)
    }));
    let first = context.new_page().unwrap();

    let auth = stub.wait_for("Fetch.continueWithAuth", 2);
    assert_eq!(auth.len(), 2);
    assert_eq!(
        json(auth[0].get("params")),
        r#"{"requestId":"A1","authChallengeResponse":{"response":"ProvideCredentials","username":"qa-grant","password":"qa-pass"}}"#
    );
    assert_eq!(
        json(auth[1].get("params")),
        r#"{"requestId":"A2","authChallengeResponse":{"response":"CancelAuth"}}"#
    );

    // The popup: set up, run, then handed to the hook.
    let deadline = Instant::now() + Duration::from_secs(5);
    while pages.lock().unwrap().len() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let hooked = pages.lock().unwrap().clone();
    assert_eq!(hooked.len(), 2);
    let popup = hooked
        .iter()
        .find(|p| p.id() != first.id())
        .cloned()
        .unwrap();
    assert!(hooked.iter().any(|p| p.id() == first.id()));
    assert_eq!(context.pages().len(), 2);
    let on = |session: &str| -> Vec<String> {
        stub.log
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.get("sessionId").as_str() == Some(session))
            .map(|m| m.get("method").as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        on("S2"),
        [
            "Page.enable",
            "Network.enable",
            "Network.setBypassServiceWorker",
            "Fetch.enable",
            "Target.setAutoAttach",
            "Page.getFrameTree",
            "Runtime.runIfWaitingForDebugger"
        ]
    );
    // Another context's page, and the duplicate report: run and let go.
    assert_eq!(on("S9"), ["Runtime.runIfWaitingForDebugger"]);
    assert_eq!(on("S3"), ["Runtime.runIfWaitingForDebugger"]);
    let detach: Vec<(Option<String>, String)> = stub
        .sent("Target.detachFromTarget")
        .iter()
        .map(|m| {
            (
                m.get("sessionId").as_str().map(str::to_string),
                json(m.get("params")),
            )
        })
        .collect();
    assert_eq!(
        detach,
        [
            (None, r#"{"sessionId":"S9"}"#.to_string()),
            (Some("S1".into()), r#"{"sessionId":"S3"}"#.into())
        ]
    );

    // The connection goes: the popup's close hook fires, the context has no open page.
    let closed = Arc::new(AtomicUsize::new(0));
    let c = closed.clone();
    popup.on_close(Arc::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
    }));
    assert!(!popup.is_closed());
    browser.close().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while closed.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    assert!(first.is_closed());
    assert!(context.pages().is_empty());
    stub.join();
}

#[test]
fn a_detached_page_is_closed() {
    let stub = Stub::start(Box::new(move |msg, out| {
        if msg.get("method").as_str() == Some("Target.createTarget") {
            // Here the page attaches after the reply.
            out.reply(msg, obj(vec![("targetId", s("T1"))]));
            out.event(
                None,
                "Target.attachedToTarget",
                attached("S1", "T1", "CTX1"),
            );
            return;
        }
        if msg.get("method").as_str() == Some("Page.captureScreenshot") {
            out.event(
                None,
                "Target.detachedFromTarget",
                obj(vec![("sessionId", s("S1")), ("targetId", s("T1"))]),
            );
            out.messages.push(json(&obj(vec![
                ("id", msg.get("id").clone()),
                (
                    "error",
                    obj(vec![
                        ("code", JValue::Num(-32001.0)),
                        ("message", s("Session with given id not found.")),
                    ]),
                ),
                ("sessionId", s("S1")),
            ])));
            return;
        }
        common(msg, out)
    }));
    let browser = CdpLaunch::new(format!(
        "ws://127.0.0.1:{}/devtools/browser/qa-1",
        stub.port
    ))
    .launch()
    .unwrap();
    let context = browser.new_context(None).unwrap();
    let page = context.new_page().unwrap();
    let closed = Arc::new(AtomicUsize::new(0));
    let c = closed.clone();
    page.on_close(Arc::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
    }));
    // The session detached: its waiting call fails then, before any answer.
    let error = page.screenshot().unwrap_err();
    assert_eq!(
        error.message,
        "Target page, context or browser has been closed"
    );
    assert!(page.is_closed());
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    // A hook registered once the page is closed (a popup closing early) still fires.
    let late = closed.clone();
    page.on_close(Arc::new(move || {
        late.fetch_add(10, Ordering::SeqCst);
    }));
    assert_eq!(closed.load(Ordering::SeqCst), 11);
    assert!(context.pages().is_empty());
    assert_eq!(
        page.body_text(Duration::from_secs(5)).unwrap_err().message,
        "Target page, context or browser has been closed"
    );
    // No route, so no Fetch; no proxy, so a context without one.
    assert!(stub.sent("Fetch.enable").is_empty());
    assert_eq!(
        json(stub.sent("Target.createBrowserContext")[0].get("params")),
        r#"{"disposeOnDetach":true}"#
    );
    browser.close().unwrap();
    stub.join();
}

#[test]
fn a_dropped_connection_fails_every_wait() {
    let stub = Stub::start(Box::new(move |msg, out| {
        if msg.get("method").as_str() == Some("Page.navigate") {
            out.drop = true;
            return;
        }
        common(msg, out)
    }));
    let browser = CdpLaunch::new(format!(
        "ws://127.0.0.1:{}/devtools/browser/qa-1",
        stub.port
    ))
    .launch()
    .unwrap();
    let context = browser.new_context(None).unwrap();
    let page = context.new_page().unwrap();
    let closed = Arc::new(AtomicUsize::new(0));
    let c = closed.clone();
    page.on_close(Arc::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
    }));
    let started = Instant::now();
    let error = page
        .goto("http://qa.test/", Duration::from_secs(20))
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "failed at once, not at the deadline"
    );
    assert_eq!(
        error.message,
        "Target page, context or browser has been closed"
    );
    // The reader marks the page closed, then runs its close hooks.
    let deadline = Instant::now() + Duration::from_secs(5);
    while closed.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(page.is_closed());
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    let started = Instant::now();
    assert!(page.locate("button", Duration::from_secs(20)).is_err());
    assert!(context.new_page().is_err());
    assert!(context.close().is_err());
    assert!(browser.new_context(None).is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
    stub.join();
}

#[test]
fn an_unanswered_call_times_out() {
    let stub = Stub::start(Box::new(move |msg, out| {
        if msg.get("method").as_str() != Some("Page.navigate") {
            common(msg, out)
        }
    }));
    let browser = CdpLaunch::new(format!(
        "ws://127.0.0.1:{}/devtools/browser/qa-1",
        stub.port
    ))
    .launch()
    .unwrap();
    let context = browser.new_context(None).unwrap();
    let page = context.new_page().unwrap();
    let started = Instant::now();
    let error = page
        .goto("http://qa.test/", Duration::from_millis(300))
        .unwrap_err();
    assert_eq!(error.message, "Timeout 300ms exceeded.");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!page.is_closed());
    browser.close().unwrap();
    stub.join();
}

#[test]
fn engine_errors() {
    let stub = Stub::start(Box::new(move |msg, out| {
        let params = msg.get("params");
        match msg.get("method").as_str().unwrap() {
            "Runtime.evaluate" if params.get("expression").as_str().unwrap().contains("a[") => {
                let thrown = obj(vec![
                    ("text", s("Uncaught")),
                    (
                        "exception",
                        obj(vec![
                            ("type", s("object")),
                            ("subtype", s("error")),
                            ("description", s("SyntaxError: Failed to execute 'querySelector' on 'Document': 'a[' is not a valid selector.\n    at <anonymous>:1:10")),
                        ]),
                    ),
                ]);
                out.reply(
                    msg,
                    obj(vec![
                        ("result", obj(vec![("type", s("object"))])),
                        ("exceptionDetails", thrown),
                    ]),
                );
            }
            "Runtime.evaluate" => {
                let node = obj(vec![
                    ("type", s("object")),
                    ("subtype", s("node")),
                    ("objectId", s("O7")),
                ]);
                out.reply(msg, obj(vec![("result", node)]));
            }
            "Runtime.callFunctionOn"
                if params
                    .get("functionDeclaration")
                    .as_str()
                    .unwrap()
                    .contains("isContentEditable") =>
            {
                let thrown = obj(vec![
                    ("text", s("Uncaught")),
                    ("exception", obj(vec![("description", s("Error: Element is not an <input>, <textarea> or [contenteditable] element\n    at fill"))])),
                ]);
                out.reply(
                    msg,
                    obj(vec![("result", obj(vec![])), ("exceptionDetails", thrown)]),
                );
            }
            "Runtime.callFunctionOn" => {
                let empty = obj(vec![
                    ("x", JValue::Num(0.0)),
                    ("y", JValue::Num(0.0)),
                    ("width", JValue::Num(0.0)),
                    ("height", JValue::Num(10.0)),
                ]);
                out.reply(msg, obj(vec![("result", obj(vec![("value", empty)]))]));
            }
            _ => common(msg, out),
        }
    }));
    let browser = CdpLaunch::new(format!(
        "ws://127.0.0.1:{}/devtools/browser/qa-1",
        stub.port
    ))
    .launch()
    .unwrap();
    let context = browser.new_context(None).unwrap();
    let page = context.new_page().unwrap();
    let error = page.locate("a[", Duration::from_secs(5)).err().unwrap();
    assert_eq!(error.message, "SyntaxError: Failed to execute 'querySelector' on 'Document': 'a[' is not a valid selector.");
    assert_eq!(
        page.keyboard_press("Hyper").unwrap_err().message,
        "Unknown key: \"Hyper\""
    );
    let handle = page
        .locate("#qa-label", Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(
        handle
            .fill("qa", Duration::from_secs(5))
            .unwrap_err()
            .message,
        "Element is not an <input>, <textarea> or [contenteditable] element"
    );
    assert_eq!(
        handle
            .click(Duration::from_millis(300))
            .unwrap_err()
            .message,
        "Element is not visible"
    );
    assert_eq!(
        handle
            .press("Hyper", Duration::from_secs(5))
            .unwrap_err()
            .message,
        "Unknown key: \"Hyper\""
    );
    assert!(
        stub.sent("Input.dispatchMouseEvent").is_empty(),
        "nothing was clicked"
    );
    let wait = Instant::now();
    page.wait_for_timeout(-5.0).unwrap();
    page.wait_for_timeout(f64::NAN).unwrap();
    page.wait_for_timeout(20.0).unwrap();
    assert!(wait.elapsed() < Duration::from_secs(1));
    let before = stub.methods().len();
    page.keyboard_press("Enter").unwrap();
    page.wheel(-250.0).unwrap();
    page.keyboard_press("Control+a").unwrap();
    let log = stub.messages();
    let sent: Vec<String> = log[before..]
        .iter()
        .map(|m| json(m.get("params")))
        .collect();
    assert_eq!(
        sent,
        [
            r#"{"type":"keyDown","modifiers":0,"windowsVirtualKeyCode":13,"code":"Enter","key":"Enter","autoRepeat":false,"location":0,"isKeypad":false,"text":"\r","unmodifiedText":"\r"}"#,
            r#"{"type":"keyUp","modifiers":0,"windowsVirtualKeyCode":13,"code":"Enter","key":"Enter","location":0}"#,
            r#"{"type":"mouseWheel","x":400,"y":300,"deltaX":0,"deltaY":-250}"#,
            r#"{"type":"rawKeyDown","modifiers":2,"windowsVirtualKeyCode":17,"code":"ControlLeft","key":"Control","autoRepeat":false,"location":1,"isKeypad":false}"#,
            r#"{"type":"rawKeyDown","modifiers":2,"windowsVirtualKeyCode":65,"code":"KeyA","key":"a","autoRepeat":false,"location":0,"isKeypad":false}"#,
            r#"{"type":"keyUp","modifiers":2,"windowsVirtualKeyCode":65,"code":"KeyA","key":"a","location":0}"#,
            r#"{"type":"keyUp","modifiers":0,"windowsVirtualKeyCode":17,"code":"ControlLeft","key":"Control","location":1}"#,
        ]
    );
    browser.close().unwrap();
    stub.join();
}

#[test]
fn refuses_tls_and_unreachable_engines() {
    for endpoint in [
        "wss://browser.qa.test:9222/devtools/browser/x",
        "https://browser.qa.test:9222",
    ] {
        let error = CdpLaunch::new(endpoint).launch().err().unwrap();
        assert!(
            error.message.contains("must be ws:// or http://"),
            "{}",
            error.message
        );
    }
    // A port nothing listens on.
    let free = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let error = CdpLaunch::new(format!("http://127.0.0.1:{free}"))
        .launch()
        .err()
        .unwrap();
    assert!(error.message.contains("unreachable"), "{}", error.message);
}

fn error_reply(msg: &JValue, out: &mut Out, text: &str) {
    let mut items = vec![
        ("id", msg.get("id").clone()),
        (
            "error",
            obj(vec![("code", JValue::Num(-32000.0)), ("message", s(text))]),
        ),
    ];
    if let Some(sid) = msg.get("sessionId").as_str() {
        items.push(("sessionId", s(sid)));
    }
    out.messages.push(json(&obj(items)));
}

fn child(
    session: &str,
    target: &str,
    kind: &str,
    context: Option<&str>,
    subtype: Option<&str>,
) -> JValue {
    let mut info = vec![
        ("targetId", s(target)),
        ("type", s(kind)),
        ("url", s("http://qa.test/w")),
    ];
    if let Some(c) = context {
        info.push(("browserContextId", s(c)));
    }
    if let Some(st) = subtype {
        info.push(("subtype", s(st)));
    }
    obj(vec![
        ("sessionId", s(session)),
        ("targetInfo", obj(info)),
        ("waitingForDebugger", JValue::Bool(true)),
    ])
}

/// Each child target is gated before it runs, and one that cannot be gated is never run.
#[test]
fn children_are_gated_before_they_run() {
    let stub = Stub::start(Box::new(move |msg, out| {
        let sid = msg.get("sessionId").as_str();
        let method = msg.get("method").as_str().unwrap();
        match (method, sid) {
            ("Runtime.runIfWaitingForDebugger", Some("S1")) => {
                out.reply(msg, obj(vec![]));
                // A popup, a frame and a browser-level service worker whose Fetch fails; a
                // worker, a shared worker and a prerendered page whose Fetch works.
                out.event(
                    None,
                    "Target.attachedToTarget",
                    attached("S2", "T2", "CTX1"),
                );
                out.event(
                    sid,
                    "Target.attachedToTarget",
                    child("S4", "FR4", "iframe", None, None),
                );
                out.event(
                    None,
                    "Target.attachedToTarget",
                    child("S8", "SW8", "service_worker", Some("CTX1"), None),
                );
                out.event(
                    sid,
                    "Target.attachedToTarget",
                    child("S5", "W5", "worker", None, None),
                );
                out.event(
                    None,
                    "Target.attachedToTarget",
                    child("S6", "SH6", "shared_worker", Some("CTX1"), None),
                );
                out.event(
                    sid,
                    "Target.attachedToTarget",
                    child("S7", "P7", "page", Some("CTX1"), Some("prerender")),
                );
            }
            ("Fetch.enable", Some("S2" | "S4" | "S8")) => {
                error_reply(msg, out, "QA: Fetch refused")
            }
            ("Runtime.runIfWaitingForDebugger", Some("S5")) => {
                out.reply(msg, obj(vec![]));
                let request = obj(vec![
                    ("url", s("http://blocked.invalid/w.js")),
                    ("method", s("GET")),
                ]);
                out.event(
                    sid,
                    "Fetch.requestPaused",
                    obj(vec![
                        ("requestId", s("W1")),
                        ("request", request),
                        ("resourceType", s("Script")),
                    ]),
                );
            }
            _ => common(msg, out),
        }
    }));
    let browser = CdpLaunch::new(format!(
        "ws://127.0.0.1:{}/devtools/browser/qa-1",
        stub.port
    ))
    .launch()
    .unwrap();
    let context = browser.new_context(None).unwrap();
    let seen: Seen = Arc::default();
    context.route(gate(&seen)).unwrap();
    let hooked = Arc::new(AtomicUsize::new(0));
    let h = hooked.clone();
    context.on_page(Arc::new(move |_: Arc<dyn Page>| {
        h.fetch_add(1, Ordering::SeqCst);
    }));
    let _page = context.new_page().unwrap();
    stub.wait_for("Target.closeTarget", 2);
    stub.wait_for("Fetch.failRequest", 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    while stub.sent("Runtime.runIfWaitingForDebugger").len() < 4 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    let on = |session: &str| -> Vec<String> {
        stub.messages()
            .iter()
            .filter(|m| m.get("sessionId").as_str() == Some(session))
            .map(|m| m.get("method").as_str().unwrap().to_string())
            .collect()
    };
    // The popup: never run, closed, never handed to the executor.
    assert_eq!(
        on("S2"),
        [
            "Page.enable",
            "Network.enable",
            "Network.setBypassServiceWorker",
            "Fetch.enable"
        ]
    );
    // The frame: never run; its page is closed.
    assert_eq!(on("S4"), ["Fetch.enable"]);
    let mut closed: Vec<String> = stub
        .sent("Target.closeTarget")
        .iter()
        .map(|m| json(m.get("params")))
        .collect();
    closed.sort();
    assert_eq!(closed, [r#"{"targetId":"T1"}"#, r#"{"targetId":"T2"}"#]);
    assert!(stub
        .sent("Target.closeTarget")
        .iter()
        .all(|m| m.get("sessionId").is_nullish()));
    // A browser-level worker that cannot be gated stays paused.
    assert_eq!(on("S8"), ["Fetch.enable"]);
    // Workers: Fetch before they run; their requests go to the gate, never as a navigation.
    assert_eq!(
        on("S5"),
        [
            "Fetch.enable",
            "Runtime.runIfWaitingForDebugger",
            "Fetch.failRequest"
        ]
    );
    assert_eq!(
        on("S6"),
        ["Fetch.enable", "Runtime.runIfWaitingForDebugger"]
    );
    let asked: Vec<(String, bool)> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|r| (r.url.clone(), r.main_frame_navigation))
        .collect();
    assert_eq!(asked, [("http://blocked.invalid/w.js".to_string(), false)]);
    // A prerendered page is routed like a frame, and is not a page of the context.
    assert_eq!(
        on("S7"),
        [
            "Fetch.enable",
            "Page.enable",
            "Network.enable",
            "Network.setBypassServiceWorker",
            "Target.setAutoAttach",
            "Runtime.runIfWaitingForDebugger"
        ]
    );
    assert_eq!(hooked.load(Ordering::SeqCst), 1);
    assert_eq!(context.pages().len(), 1);
    browser.close().unwrap();
    stub.join();
}

fn point(hit: &str) -> JValue {
    obj(vec![(
        "result",
        obj(vec![(
            "value",
            obj(vec![
                ("x", JValue::Num(60.0)),
                ("y", JValue::Num(30.0)),
                ("width", JValue::Num(100.0)),
                ("height", JValue::Num(40.0)),
                ("hit", s(hit)),
            ]),
        )]),
    )])
}

#[test]
fn click_waits_for_the_hit_target() {
    let mut measured = 0;
    let stub = Stub::start(Box::new(move |msg, out| {
        let params = msg.get("params");
        match msg.get("method").as_str().unwrap() {
            "Runtime.evaluate" => {
                let id = if params
                    .get("expression")
                    .as_str()
                    .unwrap()
                    .contains("#covered")
                {
                    "O2"
                } else {
                    "O1"
                };
                let node = obj(vec![
                    ("type", s("object")),
                    ("subtype", s("node")),
                    ("objectId", s(id)),
                ]);
                out.reply(msg, obj(vec![("result", node)]));
            }
            "Runtime.callFunctionOn" => {
                measured += 1;
                // O1 is covered for its first two looks; O2 always.
                let covered = params.get("objectId").as_str() == Some("O2") || measured <= 2;
                out.reply(
                    msg,
                    point(if covered {
                        "<div id=\"cover\">\u{2026}</div>"
                    } else {
                        ""
                    }),
                );
            }
            _ => common(msg, out),
        }
    }));
    let browser = CdpLaunch::new(format!(
        "ws://127.0.0.1:{}/devtools/browser/qa-1",
        stub.port
    ))
    .launch()
    .unwrap();
    let context = browser.new_context(None).unwrap();
    let page = context.new_page().unwrap();
    let button = page
        .locate("#send", Duration::from_secs(5))
        .unwrap()
        .unwrap();
    let started = Instant::now();
    button.click(Duration::from_secs(5)).unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "retried every 100 ms"
    );
    assert_eq!(stub.sent("Runtime.callFunctionOn").len(), 3);
    assert_eq!(stub.sent("Input.dispatchMouseEvent").len(), 3);
    let covered = page
        .locate("#covered", Duration::from_secs(5))
        .unwrap()
        .unwrap();
    let error = covered.click(Duration::from_millis(300)).unwrap_err();
    assert_eq!(
        error.message,
        "<div id=\"cover\">\u{2026}</div> intercepts pointer events"
    );
    assert_eq!(
        covered.hover(Duration::from_millis(0)).unwrap_err().message,
        error.message
    );
    assert_eq!(
        stub.sent("Input.dispatchMouseEvent").len(),
        3,
        "nothing pressed on the cover"
    );
    assert_eq!(
        button
            .set_input_files(std::path::Path::new("/tmp/qa.txt"), Duration::from_secs(5))
            .unwrap_err()
            .message,
        "Uploads need a directory shared with the browser engine; none is configured."
    );
    assert!(stub.sent("DOM.setFileInputFiles").is_empty());
    browser.close().unwrap();
    stub.join();
}

/// A document that replaces itself before its load (`location.replace`, a meta refresh) is waited
/// through to the load of the one that stays; a huge timeout does not overflow the clock.
#[test]
fn goto_waits_through_a_replacing_document() {
    let stub = Stub::start(Box::new(move |msg, out| {
        let sid = msg.get("sessionId").as_str();
        match msg.get("method").as_str().unwrap() {
            "Page.navigate" => {
                let frame = |loader: &str, url: &str| {
                    obj(vec![
                        (
                            "frame",
                            obj(vec![
                                ("id", s("F1")),
                                ("loaderId", s(loader)),
                                ("url", s(url)),
                            ]),
                        ),
                        ("type", s("Navigation")),
                    ])
                };
                out.event(
                    sid,
                    "Page.frameNavigated",
                    frame("L1", "http://qa.test/hop"),
                );
                out.reply(msg, obj(vec![("frameId", s("F1")), ("loaderId", s("L1"))]));
                out.event(
                    sid,
                    "Page.frameStartedLoading",
                    obj(vec![("frameId", s("F1"))]),
                );
                out.event(
                    sid,
                    "Page.frameNavigated",
                    frame("L2", "http://qa.test/stay"),
                );
                out.event(
                    sid,
                    "Page.loadEventFired",
                    obj(vec![("timestamp", JValue::Num(2.0))]),
                );
            }
            "Runtime.evaluate" => {
                let value = obj(vec![("type", s("string")), ("value", s("complete"))]);
                out.reply(msg, obj(vec![("result", value)]));
            }
            _ => common(msg, out),
        }
    }));
    let browser = CdpLaunch::new(format!(
        "ws://127.0.0.1:{}/devtools/browser/qa-1",
        stub.port
    ))
    .launch()
    .unwrap();
    let context = browser.new_context(None).unwrap();
    let page = context.new_page().unwrap();
    let started = Instant::now();
    page.goto("http://qa.test/hop", Duration::MAX).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(page.url(), "http://qa.test/stay");
    page.wait_for_load_state(Duration::MAX).unwrap();
    browser.close().unwrap();
    stub.join();
}

/// A message over the cap is read past and dropped; the connection stays up.
#[test]
fn an_oversize_message_is_dropped() {
    let stub = Stub::start(Box::new(move |msg, out| {
        if msg.get("method").as_str() == Some("Page.navigate") {
            out.messages.push(" ".repeat((64 << 20) + 1));
            return;
        }
        if msg.get("method").as_str() == Some("Page.captureScreenshot") {
            out.reply(
                msg,
                obj(vec![(
                    "data",
                    s(&base64::engine::general_purpose::STANDARD.encode(PNG)),
                )]),
            );
            return;
        }
        common(msg, out)
    }));
    let browser = CdpLaunch::new(format!(
        "ws://127.0.0.1:{}/devtools/browser/qa-1",
        stub.port
    ))
    .launch()
    .unwrap();
    let context = browser.new_context(None).unwrap();
    let page = context.new_page().unwrap();
    let error = page
        .goto("http://qa.test/", Duration::from_millis(500))
        .unwrap_err();
    assert_eq!(error.message, "Timeout 500ms exceeded.");
    assert!(!page.is_closed());
    assert_eq!(&page.screenshot().unwrap(), PNG);
    browser.close().unwrap();
    stub.join();
}
