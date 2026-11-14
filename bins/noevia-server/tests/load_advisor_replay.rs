//! sbstndalton/noevia#2550 item 2: the native load advisor (`model_maint::load_advisor`) against
//! Node's. Each scenario in fixtures/load-advisor/scenarios.json was recorded from core's own
//! `createLoadAdvisor({ env }).judge` (record-node.cjs there) against a scripted stub decision
//! service; this replays it through `LoadAdvisor` against the same stub, serving the same bytes,
//! and compares what the stub received (method, path, content type, authorization, body, which
//! scripted reply) and what judge returned, byte for byte.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use js_json::{wtf16, JValue};
use noevia_server::model_maint::load_advisor::LoadAdvisor;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const FIXTURE: &str = include_str!("fixtures/load-advisor/scenarios.json");
const SOURCES: &str = include_str!("fixtures/load-advisor/SOURCES.sha256");
const PORT: &str = "<port>";

#[derive(Default)]
struct State {
    requests: Vec<JValue>,
    pending: usize,
    arrivals: usize,
    open: usize,
}

struct Stub {
    port: u16,
    state: Arc<Mutex<State>>,
    accept: Option<tokio::task::JoinHandle<()>>,
}

impl Stub {
    async fn start(spec: &JValue) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State::default()));
        if spec.get("closed") == &JValue::Bool(true) {
            drop(listener);
            return Stub {
                port,
                state,
                accept: None,
            };
        }
        let replies = match spec.get("replies") {
            JValue::Arr(r) => r.clone(),
            _ => Vec::new(),
        };
        let single = spec.get("singleWorker") == &JValue::Bool(true);
        let shared = Arc::clone(&state);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                shared.lock().unwrap().open += 1;
                let state = Arc::clone(&shared);
                let replies = replies.clone();
                tokio::spawn(async move {
                    serve(socket, &state, &replies, single).await;
                    state.lock().unwrap().open -= 1;
                });
            }
        });
        Stub {
            port,
            state,
            accept: Some(accept),
        }
    }

    fn requests(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }

    async fn settled(&self) {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            {
                let s = self.state.lock().unwrap();
                if s.open == 0 && s.pending == 0 {
                    return;
                }
            }
            assert!(Instant::now() < deadline, "the stub did not settle");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        if let Some(a) = self.accept.take() {
            a.abort();
        }
    }
}

fn json_reply(status: f64, body: &str) -> JValue {
    JValue::obj([
        ("status", JValue::Num(status)),
        ("body", JValue::from(body)),
    ])
}

/// record-node.cjs `responseBytes`.
fn response_bytes(r: &JValue) -> Vec<u8> {
    let body: Vec<u8> = if r.get("bodyRepeat").is_object() {
        let unit = r.get("bodyRepeat").get("text").as_str().unwrap();
        let count = r.get("bodyRepeat").get("count").as_num().unwrap() as usize;
        unit.repeat(count).into_bytes()
    } else if let Some(hex) = r.get("bodyHex").as_str() {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    } else {
        r.get("body")
            .as_str()
            .unwrap_or_default()
            .as_bytes()
            .to_vec()
    };
    let status = r.get("status").as_num().unwrap() as u16;
    let mut head = vec![format!("HTTP/1.1 {status} QA")];
    if let JValue::Arr(headers) = r.get("headers") {
        for h in headers {
            let JValue::Arr(pair) = h else {
                panic!("header {h:?}")
            };
            head.push(format!(
                "{}: {}",
                pair[0].as_str().unwrap(),
                pair[1].as_str().unwrap()
            ));
        }
    }
    if status != 204 {
        head.push(format!("Content-Length: {}", body.len()));
    }
    head.push("Connection: close".into());
    let mut out = format!("{}\r\n\r\n", head.join("\r\n")).into_bytes();
    if status != 204 {
        out.extend_from_slice(&body);
    }
    out
}

struct Request {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// record-node.cjs `parseRequest`.
fn parse_request(buf: &[u8]) -> Option<Request> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head: String = buf[..end].iter().map(|b| char::from(*b)).collect();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some(i) = line.find(':').filter(|i| *i > 0) {
            headers.insert(
                line[..i].trim().to_lowercase(),
                line[i + 1..].trim().to_owned(),
            );
        }
    }
    let length: usize = headers
        .get("content-length")
        .map(|v| v.parse().unwrap())
        .unwrap_or(0);
    if buf.len() < end + 4 + length {
        return None;
    }
    Some(Request {
        method,
        target,
        headers,
        body: buf[end + 4..end + 4 + length].to_vec(),
    })
}

async fn until_closed(socket: &mut TcpStream) {
    let mut sink = [0u8; 1024];
    let _ = tokio::time::timeout(Duration::from_secs(12), async {
        while matches!(socket.read(&mut sink).await, Ok(n) if n > 0) {}
    })
    .await;
}

async fn serve(mut socket: TcpStream, state: &Mutex<State>, replies: &[JValue], single: bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let req = loop {
        if let Some(r) = parse_request(&buf) {
            break r;
        }
        match socket.read(&mut chunk).await {
            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            _ => return,
        }
    };
    let reply = {
        let mut s = state.lock().unwrap();
        let index = s.arrivals;
        s.arrivals += 1;
        let busy = single && s.pending > 0;
        let reply = if busy {
            json_reply(503.0, r#"{"error":"busy"}"#)
        } else {
            replies
                .get(index)
                .cloned()
                .unwrap_or_else(|| json_reply(500.0, r#"{"error":"unscripted"}"#))
        };
        let header = |k: &str| {
            req.headers
                .get(k)
                .map_or(JValue::Null, |v| JValue::from(v.as_str()))
        };
        let record = JValue::obj([
            ("method", JValue::from(req.method.as_str())),
            ("path", JValue::from(req.target.as_str())),
            ("contentType", header("content-type")),
            ("authorization", header("authorization")),
            (
                "body",
                JValue::from(String::from_utf8(req.body.clone()).expect("UTF-8 body")),
            ),
            (
                "reply",
                if busy {
                    JValue::from("busy")
                } else {
                    JValue::Num(index as f64)
                },
            ),
        ]);
        s.requests.push(record);
        s.pending += 1;
        reply
    };
    let finish = || state.lock().unwrap().pending -= 1;
    if reply.get("reset") == &JValue::Bool(true) {
        finish();
        drop(socket);
        return;
    }
    if reply.get("hang") == &JValue::Bool(true) {
        until_closed(&mut socket).await;
        finish();
        return;
    }
    let delay = reply.get("delayMs").as_num().unwrap_or(0.0);
    tokio::time::sleep(Duration::from_millis(delay as u64)).await;
    finish();
    if socket.write_all(&response_bytes(&reply)).await.is_ok() {
        let _ = socket.shutdown().await;
        until_closed(&mut socket).await;
    }
}

fn input(sc: &JValue, i: usize) -> JValue {
    match sc.get("inputs") {
        JValue::Arr(items) => items[i].clone(),
        _ => panic!("no inputs"),
    }
}

async fn replay(sc: &JValue) -> (JValue, Vec<String>) {
    let stub = Stub::start(sc.get("stub")).await;
    let env: HashMap<String, String> = match sc.get("env") {
        JValue::Obj(items) => items
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.as_str().unwrap().replace(PORT, &stub.port.to_string()),
                )
            })
            .collect(),
        _ => HashMap::new(),
    };
    let advisor = Arc::new(LoadAdvisor::from_lookup(&|k: &str| env.get(k).cloned()));
    let results = match sc.get("calls").as_str().unwrap() {
        "single" => vec![advisor.judge(&input(sc, 0)).await],
        "sequential" => {
            let a = advisor.judge(&input(sc, 0)).await;
            let b = advisor.judge(&input(sc, 1)).await;
            vec![a, b]
        }
        "concurrent" => {
            // Node asks the second question while the first is open (judge sets it before its
            // first await); here the first is open once the stub has its request.
            let first = Arc::clone(&advisor);
            let a_input = input(sc, 0);
            let a = tokio::spawn(async move { first.judge(&a_input).await });
            let deadline = Instant::now() + Duration::from_secs(5);
            while stub.requests() == 0 {
                assert!(
                    Instant::now() < deadline,
                    "the first question never arrived"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let b = advisor.judge(&input(sc, 1)).await;
            vec![a.await.unwrap(), b]
        }
        other => panic!("calls {other}"),
    };
    stub.settled().await;
    let requests = JValue::Arr(stub.state.lock().unwrap().requests.clone());
    (
        requests,
        results
            .iter()
            .map(|r| wtf16::stringify(r).unwrap())
            .collect(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_load_advisor_matches_node() {
    let doc = wtf16::parse(FIXTURE).expect("fixture parses");
    assert_eq!(doc.get("version").as_num(), Some(1.0));
    // The recorded sources are the ones SOURCES.sha256 lists.
    let JValue::Obj(sources) = doc.get("sourceSha256") else {
        panic!("no sourceSha256")
    };
    let listed: Vec<String> = SOURCES.lines().map(str::to_owned).collect();
    let recorded: Vec<String> = sources
        .iter()
        .map(|(f, h)| format!("{}  {f}", h.as_str().unwrap()))
        .collect();
    assert_eq!(listed, recorded, "SOURCES.sha256 and the fixture disagree");

    let JValue::Arr(scenarios) = doc.get("scenarios") else {
        panic!("no scenarios")
    };
    assert!(scenarios.len() >= 90, "{} scenarios", scenarios.len());
    let mut failures = Vec::new();
    for sc in scenarios {
        let name = sc.get("name").as_str().unwrap();
        let (requests, results) = replay(sc).await;
        let want_requests = wtf16::stringify(sc.get("requests")).unwrap();
        let got_requests = wtf16::stringify(&requests).unwrap();
        if got_requests != want_requests {
            failures.push(format!(
                "{name}: requests\n  node: {want_requests}\n  rust: {got_requests}"
            ));
        }
        let want: Vec<String> = match sc.get("results") {
            JValue::Arr(r) => r.iter().map(|s| s.as_str().unwrap().to_owned()).collect(),
            _ => panic!("{name}: no results"),
        };
        if results != want {
            failures.push(format!(
                "{name}: results\n  node: {want:?}\n  rust: {results:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} scenarios differ:\n{}",
        failures.len(),
        scenarios.len(),
        failures.join("\n")
    );
}
