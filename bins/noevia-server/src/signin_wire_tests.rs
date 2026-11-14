//! The sign-in transport in the front over real HTTP: core's recorded cases
//! (crates/signin-transport/tests/fixtures/node-transport.v1.json) replayed through the front's
//! client against a loopback stub backend, plus the texts, the signal, the broker mapping, the
//! redirect refusal and the abort.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use super::*;
use crate::chatgpt_oauth_routes::{transport, Settings};
use http_body_util::BodyExt;
use std::collections::VecDeque;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/signin-transport/tests/fixtures/node-transport.v1.json"
);

/// Every test's waits end by this.
const DEADLINE: Duration = Duration::from_secs(20);

fn fixture() -> JValue {
    js_json::parse(&std::fs::read_to_string(FIXTURE).unwrap()).unwrap()
}

fn s(v: &JValue) -> String {
    v.as_str()
        .unwrap_or_else(|| panic!("not a string: {v:?}"))
        .to_owned()
}

/// The recorder's scrub: product words become neutral words of the same length.
fn scrub(text: &str) -> String {
    [
        ("ChatGPT", "Service"),
        ("chatgpt", "service"),
        ("Codex", "Relay"),
        ("codex", "relay"),
        ("OpenAI", "Vendor"),
        ("openai", "vendor"),
    ]
    .iter()
    .fold(text.to_owned(), |t, (a, b)| t.replace(a, b))
}

fn b64(text: &str) -> Vec<u8> {
    let val = |c: u8| -> u32 {
        u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("bad base64"),
        })
    };
    let bytes: Vec<u8> = text.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &c)| acc | (val(c) << (18 - 6 * i)));
        for i in 0..chunk.len().saturating_sub(1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

/// The fixture's texts and header names (already scrubbed).
fn fixture_transport(f: &JValue, base_url: &str) -> Transport {
    let t = f.get("texts");
    Transport {
        texts: Texts {
            label: s(t.get("label")),
            disconnected: s(t.get("disconnected")),
            reconnect: s(t.get("reconnect")),
            transient: s(t.get("transient")),
            not_included: s(t.get("notIncluded")),
            source: s(t.get("source")),
        },
        protocol: Protocol {
            account_header: "service-account-id".into(),
            region_header: "x-vendor-fedramp".into(),
            user_agent: s(f.get("userAgent")),
        },
        base_url: base_url.to_owned(),
    }
}

#[test]
fn texts_and_header_names_are_cores() {
    let f = fixture();
    let settings = Settings::from_lookup(&|_: &str| None, Some("0"));
    let ours = transport(&settings);
    let want = fixture_transport(&f, "");
    let t = &ours.texts;
    let scrubbed = Texts {
        label: scrub(&t.label),
        disconnected: scrub(&t.disconnected),
        reconnect: scrub(&t.reconnect),
        transient: scrub(&t.transient),
        not_included: scrub(&t.not_included),
        source: scrub(&t.source),
    };
    assert_eq!(scrubbed, want.texts);
    assert_eq!(
        scrub(&ours.protocol.account_header),
        want.protocol.account_header
    );
    assert_eq!(
        scrub(&ours.protocol.region_header),
        want.protocol.region_header
    );
    // The recorder ran core without a package version ("0"); the front's settings were built so too.
    assert_eq!(scrub(&ours.protocol.user_agent), want.protocol.user_agent);
    assert!(!ours.base_url.ends_with('/'));
    // A loopback QA backend keeps its path, without the trailing slash.
    let qa = Settings::from_lookup(
        &|k: &str| {
            (k == "NOEVIA_QA_CHATGPT_CODEX_BASE").then(|| "http://127.0.0.1:9/x/".to_owned())
        },
        None,
    );
    assert_eq!(transport(&qa).base_url, "http://127.0.0.1:9/x");
}

#[test]
fn the_caps_token_is_exact() {
    assert!(offered("a,signin-transport-v1"));
    assert!(offered(" signin-transport-v1 , b"));
    assert!(!offered(""));
    assert!(!offered("signin-transport-v10"));
    assert!(!offered("signin-transport"));
}

#[test]
fn file_signal_is_cores() {
    use std::os::unix::fs::MetadataExt;
    let dir = std::env::temp_dir().join(format!("qa-signin-signal-{}", uuid_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("changes.json");
    assert_eq!(file_signal(&file).as_deref(), Some(""), "missing file");
    std::fs::write(&file, "{}").unwrap();
    let m = std::fs::metadata(&file).unwrap();
    let first = file_signal(&file).unwrap();
    assert_eq!(
        first,
        format!(
            "{}:2:{}",
            m.ino(),
            i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec())
        )
    );
    // An atomic replace (the broker's write) moves the inode: the signal moves.
    let tmp = dir.join("changes.json.tmp");
    std::fs::write(&tmp, "{}").unwrap();
    std::fs::rename(&tmp, &file).unwrap();
    assert_ne!(file_signal(&file).unwrap(), first);
    // A path through a file is not "missing": the signal cannot be read.
    assert_eq!(file_signal(&file.join("x")), None);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn broker_answers_map_as_cores_store_client() {
    let e = Fail {
        status: 401,
        message: "again".into(),
        code: Some("reconnect"),
    };
    assert_eq!(
        refusal_of(&e),
        BrokerAnswer::Refused {
            error: Some("again".into()),
            status: Some(401.0),
            code: Some("reconnect".into()),
        }
    );
    let s = session_of("tok", "acct", true, Some(f64::INFINITY), "cipher");
    assert_eq!(s.expires_at, None, "a non-finite expiry is null");
    assert!(s.restricted_region);
    assert_eq!(s.generation, JValue::Str(generation_of("cipher")));
    assert!(!format!("{s:?}").contains("tok\""), "{s:?}");
}

// ── the stub backend ────────────────────────────────────────────────────────

/// One recorded backend answer.
#[derive(Clone)]
struct Scripted {
    status: u16,
    chunks: Vec<Vec<u8>>,
    labelled: bool,
    content_type: String,
    nobody: bool,
    fail_after: Option<usize>,
    throws: bool,
    /// Sent as is instead (redirect tests).
    raw: Option<String>,
    /// Hold the connection open after this many chunks until the client goes away (abort test).
    hold_after: Option<usize>,
}

impl Scripted {
    fn of(u: &JValue) -> Scripted {
        Scripted {
            status: u.get("status").as_num().map_or(200, |x| x as u16),
            chunks: match u.get("chunks") {
                JValue::Arr(items) => items.iter().map(|c| b64(&s(c))).collect(),
                _ => Vec::new(),
            },
            labelled: u.get("labelled").truthy(),
            content_type: u
                .get("contentType")
                .as_str()
                .unwrap_or("text/event-stream")
                .to_owned(),
            nobody: u.get("nobody").truthy(),
            fail_after: u.get("failAfter").as_num().map(|n| n as usize),
            throws: u.get("throws").as_str().is_some(),
            raw: None,
            hold_after: None,
        }
    }
}

/// What the stub received: the request head (lower-case header names) and body.
#[derive(Debug, Clone)]
struct Received {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

#[derive(Default)]
struct StubState {
    script: VecDeque<Scripted>,
    /// Answered once the script is used up (core's fake: the text stream).
    default: Option<Scripted>,
    received: Vec<Received>,
    /// Set when a held connection saw the client go away.
    client_gone: bool,
}

type Stub = Arc<Mutex<StubState>>;

async fn read_request(sock: &mut TcpStream) -> Option<Received> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let path = lines.next()?.split(' ').nth(1)?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Some(Received {
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

async fn answer(sock: &mut TcpStream, u: &Scripted, stub: &Stub) {
    if u.throws {
        // The connection closes before any answer: the client's send fails.
        return;
    }
    if let Some(raw) = &u.raw {
        let _ = sock.write_all(raw.as_bytes()).await;
        return;
    }
    let mut head = format!(
        "HTTP/1.1 {} QA\r\ncontent-type: {}\r\nconnection: close\r\n",
        u.status, u.content_type
    );
    if u.labelled {
        head.push_str("x-noevia-provider-message: 1\r\n");
    }
    if u.nobody || matches!(u.status, 204 | 304) {
        head.push_str("\r\n");
        let _ = sock.write_all(head.as_bytes()).await;
        return;
    }
    head.push_str("transfer-encoding: chunked\r\n\r\n");
    if sock.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for (i, c) in u.chunks.iter().enumerate() {
        if u.fail_after == Some(i) {
            // Cut mid-body: the chunked body never ends, the client's read fails.
            return;
        }
        if u.hold_after == Some(i) {
            break;
        }
        if c.is_empty() {
            continue;
        }
        let frame = [
            format!("{:x}\r\n", c.len()).into_bytes(),
            c.clone(),
            b"\r\n".to_vec(),
        ]
        .concat();
        if sock.write_all(&frame).await.is_err() {
            return;
        }
        let _ = sock.flush().await;
        // Let each chunk arrive as its own read where the network allows.
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    if u.fail_after == Some(u.chunks.len()) {
        return;
    }
    if u.hold_after.is_some() {
        // Wait (bounded) for the client to go away.
        let mut tmp = [0u8; 64];
        if let Ok(Ok(0) | Err(_)) = tokio::time::timeout(DEADLINE, sock.read(&mut tmp)).await {
            stub.lock().unwrap().client_gone = true;
        }
        return;
    }
    let _ = sock.write_all(b"0\r\n\r\n").await;
}

/// A loopback backend answering its script in order; its address.
async fn stub(state: StubState) -> (String, Stub) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stub: Stub = Arc::new(Mutex::new(state));
    let st = Arc::clone(&stub);
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let st = Arc::clone(&st);
            tokio::spawn(async move {
                let Some(req) = read_request(&mut sock).await else {
                    return;
                };
                let next = {
                    let mut g = st.lock().unwrap();
                    g.received.push(req);
                    g.script.pop_front().or_else(|| g.default.clone())
                };
                if let Some(u) = next {
                    answer(&mut sock, &u, &st).await;
                }
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{addr}/api/"), stub)
}

// ── the fake broker ─────────────────────────────────────────────────────────

/// The case's broker answers, logging calls as core's fake did (generation as core's store client
/// sends it: a string).
struct FakeAccounts {
    catalogue: JValue,
    sessions: Mutex<VecDeque<JValue>>,
    calls: Mutex<Vec<JValue>>,
}

fn session_answer(v: &JValue) -> BrokerAnswer {
    if v.as_str() == Some("throw") {
        return BrokerAnswer::Unreachable;
    }
    if v.get("ok").as_bool() != Some(true) {
        return BrokerAnswer::Refused {
            error: v.get("error").as_str().map(str::to_owned),
            status: v.get("status").as_num(),
            code: v.get("code").as_str().map(str::to_owned),
        };
    }
    let x = v.get("session");
    BrokerAnswer::Ok(Session {
        access_token: s(x.get("accessToken")),
        account_id: s(x.get("accountId")),
        restricted_region: x.get("fedRamp").truthy(),
        expires_at: x.get("expiresAt").as_num(),
        generation: JValue::Str(js_string(x.get("generation"))),
    })
}

/// `String(v)` for the fixture's generations (numbers and strings).
fn js_string(v: &JValue) -> String {
    match v {
        JValue::Str(s) => s.clone(),
        JValue::Num(n) => js_json::stringify(&JValue::Num(*n)).unwrap(),
        other => js_json::stringify(other).unwrap(),
    }
}

impl Accounts for FakeAccounts {
    fn signal(&self) -> Option<String> {
        Some("sig-1".into())
    }
    async fn session(&self, user: &str, rejected: Option<&str>) -> BrokerAnswer {
        self.calls.lock().unwrap().push(JValue::Arr(vec![
            "session".into(),
            user.into(),
            rejected.map_or(JValue::Null, JValue::from),
        ]));
        let next = self.sessions.lock().unwrap().pop_front();
        session_answer(&next.unwrap_or(JValue::Str("throw".into())))
    }
    async fn reject(&self, user: &str, generation: &str) {
        self.calls.lock().unwrap().push(JValue::Arr(vec![
            "reject".into(),
            user.into(),
            generation.into(),
        ]));
    }
    async fn catalogue(&self, user: &str) -> Option<JValue> {
        self.calls
            .lock()
            .unwrap()
            .push(JValue::Arr(vec!["catalogue".into(), user.into()]));
        match &self.catalogue {
            JValue::Undefined => Some(
                js_json::parse(
                    r#"[{"slug":"m-large","effort":"medium","summary":"detailed","summaryParam":true,"levels":["low","medium","high","xhigh"]},{"slug":"m-small","summaryParam":false},{"slug":"m-none","summary":"none","summaryParam":true,"levels":[]},{"slug":"m-auto","summaryParam":true,"effort":""}]"#,
                )
                .unwrap(),
            ),
            c if c.get("ok").as_bool() == Some(true) => Some(c.get("models").clone()),
            _ => None,
        }
    }
}

/// What the chat loop read from the front's reply.
#[derive(Debug, Default)]
struct Read {
    status: u16,
    content_type: Option<String>,
    provider_message: Option<String>,
    text: String,
    read_error: bool,
    thrown: Option<String>,
}

async fn read_reply(answer: Answer<HttpBody>) -> Read {
    let res = match into_response(answer) {
        Ok(r) => r,
        Err(m) => {
            return Read {
                thrown: Some(m),
                ..Read::default()
            }
        }
    };
    let header = |n: &str| {
        res.headers()
            .get(n)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
    };
    let mut out = Read {
        status: res.status().as_u16(),
        content_type: header("content-type"),
        provider_message: header("x-noevia-provider-message"),
        ..Read::default()
    };
    let mut body = res.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(f) => {
                if let Some(d) = f.data_ref() {
                    bytes.extend_from_slice(d);
                }
            }
            Err(_) => {
                out.read_error = true;
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    out.text = text
        .strip_prefix('\u{FEFF}')
        .map_or(text.clone(), str::to_owned);
    out
}

/// One recorded case through the front's client against the stub.
async fn run_case(f: &JValue, input: &JValue) -> (Read, Vec<Received>, Vec<JValue>, bool) {
    let default_stream = Scripted::of(&JValue::obj([
        ("status", JValue::Num(200.0)),
        ("chunks", f.get("streams").get("text").get("chunks").clone()),
    ]));
    let JValue::Arr(upstream) = input.get("upstream").clone() else {
        panic!()
    };
    let JValue::Arr(sessions) = input.get("sessions").clone() else {
        panic!()
    };
    let (base, stub) = stub(StubState {
        script: upstream.iter().map(Scripted::of).collect(),
        default: Some(default_stream),
        ..StubState::default()
    })
    .await;
    let transport = fixture_transport(f, &base);
    let mut wire = SigninWire {
        accounts: FakeAccounts {
            catalogue: input.get("catalogue").clone(),
            sessions: Mutex::new(sessions.into()),
            calls: Mutex::new(Vec::new()),
        },
        client: client().unwrap(),
    };
    let cache = Mutex::new(SessionCache::default());
    let url = input
        .get("url")
        .as_str()
        .unwrap_or("http://x/v1/chat/completions")
        .to_owned();
    let body = s(input.get("body"));
    let conversation = input.get("conversation").as_str().map(str::to_owned);
    let call = Call {
        texts: &transport.texts,
        protocol: &transport.protocol,
        base_url: &transport.base_url,
        user: "qa-user",
        conversation: conversation.as_deref(),
        url: &url,
        body: &body,
        now_ms: f.get("now").as_num().unwrap(),
        reply_uuid: "00000000-0000-4000-8000-000000000000",
    };
    let read = tokio::time::timeout(DEADLINE, async {
        let answer = fetch::fetch(&mut wire, &cache, &call).await;
        read_reply(answer).await
    })
    .await
    .expect("a case finished within the deadline");
    let received = stub.lock().unwrap().received.clone();
    let calls = wire.accounts.calls.lock().unwrap().clone();
    let cache_empty = cache.into_inner().unwrap().is_empty();
    (read, received, calls, cache_empty)
}

/// Cases the HTTP wire cannot express as core's fake did, or whose port answer is a named
/// difference already pinned by the crate's replay.
fn skipped(input: &JValue) -> Option<&'static str> {
    let name = s(input.get("name"));
    // Named differences and late failures (crates/signin-transport/tests/node_replay.rs).
    if matches!(
        name.as_str(),
        "catalogueObject"
            | "catalogueString"
            | "catalogueNumber"
            | "catalogueNonArrayNoBody"
            | "catalogueNullEntry"
            | "bodyNull"
            | "noBody204"
            | "noBody304"
            | "streamFailAfterFinish"
            | "completeFailAfterError"
    ) {
        return Some("named");
    }
    // A null body with a status that has a body on the wire: HTTP sends an empty body instead.
    let JValue::Arr(up) = input.get("upstream") else {
        return None;
    };
    if up.iter().any(|u| u.get("nobody").truthy()) {
        return Some("no null body over HTTP");
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recorded_cases_over_real_http_match_core() {
    let f = fixture();
    let JValue::Arr(cases) = f.get("fetch").clone() else {
        panic!()
    };
    assert_eq!(cases.len(), 107);
    let mut wrong = Vec::new();
    let (mut ran, mut skips) = (0, 0);
    for c in &cases {
        let (input, want) = (c.get("input"), c.get("output"));
        let name = s(input.get("name"));
        if skipped(input).is_some() {
            skips += 1;
            continue;
        }
        ran += 1;
        let (got, received, calls, cache_empty) = run_case(&f, input).await;
        let mut check = |ok: bool, what: String| {
            if !ok {
                wrong.push(format!("{name}: {what}"));
            }
        };
        let failure_case = match input.get("upstream") {
            JValue::Arr(up) => up.iter().any(|u| {
                u.get("failAfter").as_num().is_some() || u.get("throws").as_str().is_some()
            }),
            _ => false,
        };
        if let Some(thrown) = want.get("thrown").as_str() {
            // core's fake rejects with its own text; over HTTP the client's texts are fetch's.
            let sends = match input.get("upstream") {
                JValue::Arr(up) => up.iter().any(|u| u.get("throws").as_str().is_some()),
                _ => false,
            };
            let expected = if sends { SEND_FAILED } else { READ_FAILED };
            check(
                got.thrown.as_deref() == Some(expected),
                format!("thrown {:?}, core {thrown}", got.thrown),
            );
        } else {
            check(got.thrown.is_none(), format!("thrown {:?}", got.thrown));
            check(
                f64::from(got.status) == want.get("status").as_num().unwrap_or(-1.0),
                format!("status {} vs {}", got.status, json(want.get("status"))),
            );
            check(
                got.content_type.as_deref() == want.get("contentType").as_str(),
                format!("content type {:?}", got.content_type),
            );
            check(
                got.provider_message.as_deref() == want.get("providerMessage").as_str(),
                format!("provider message {:?}", got.provider_message),
            );
            check(
                got.read_error == want.get("readError").as_str().is_some(),
                format!("read error {}", got.read_error),
            );
            // A failed answer's line can quote the failure's own text (core's fake's message
            // against fetch's): compared only where no failure text can appear.
            if !(failure_case && got.status != 200) {
                check(
                    got.text == s(want.get("text")),
                    format!("text\n port {}\n core {}", got.text, s(want.get("text"))),
                );
            }
        }
        // What reached the backend: core's requests, header by header, byte for byte.
        let JValue::Arr(sent) = want.get("sent").clone() else {
            panic!()
        };
        check(
            received.len() == sent.len(),
            format!("{} requests, core {}", received.len(), sent.len()),
        );
        for (r, w) in received.iter().zip(&sent) {
            check(r.path == "/api/responses", format!("path {}", r.path));
            check(r.body == s(w.get("body")), "body".into());
            if let JValue::Obj(h) = w.get("headers") {
                for (k, v) in h {
                    let ours = r
                        .headers
                        .iter()
                        .find(|(n, _)| n == k)
                        .map(|(_, x)| x.as_str());
                    check(ours == v.as_str(), format!("header {k}: {ours:?}"));
                }
            }
        }
        let want_calls: Vec<JValue> = match want.get("brokerCalls") {
            JValue::Arr(items) => items
                .iter()
                .map(|c| match c {
                    JValue::Arr(x) if x.first().and_then(JValue::as_str) == Some("reject") => {
                        JValue::Arr(vec![
                            x[0].clone(),
                            x[1].clone(),
                            JValue::Str(js_string(&x[2])),
                        ])
                    }
                    other => other.clone(),
                })
                .collect(),
            _ => Vec::new(),
        };
        check(
            json(&JValue::Arr(calls.clone())) == json(&JValue::Arr(want_calls.clone())),
            format!(
                "broker calls {} vs {}",
                json(&JValue::Arr(calls)),
                json(&JValue::Arr(want_calls))
            ),
        );
        if json(want.get("brokerCalls")).contains("\"reject\"") {
            check(cache_empty, "cache kept a rejected session".into());
        }
    }
    assert!(
        wrong.is_empty(),
        "{} differences over {ran} cases:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    assert!(ran >= 90, "ran {ran}, skipped {skips}");
    assert_eq!(ran + skips, cases.len());
}

fn json(v: &JValue) -> String {
    js_json::stringify(v).unwrap_or_default()
}

fn qa_accounts(sessions: usize) -> FakeAccounts {
    let ok = JValue::obj([
        ("ok", JValue::Bool(true)),
        (
            "session",
            JValue::obj([
                ("accessToken", JValue::from("tok-1")),
                ("accountId", JValue::from("acct-1")),
                ("fedRamp", JValue::Bool(false)),
                ("expiresAt", JValue::Num(4_000_000_000_000.0)),
                ("generation", JValue::from("g-1")),
            ]),
        ),
    ]);
    FakeAccounts {
        catalogue: JValue::Undefined,
        sessions: Mutex::new(std::iter::repeat_n(ok, sessions).collect()),
        calls: Mutex::new(Vec::new()),
    }
}

const QA_BODY: &str =
    r#"{"model":"m-large","messages":[{"role":"user","content":"q"}],"stream":true}"#;

#[tokio::test]
async fn a_redirect_is_refused_and_not_followed() {
    let (base, stub) = stub(StubState {
        script: VecDeque::from([Scripted {
            raw: Some(
                "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:9/elsewhere\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    .into(),
            ),
            ..Scripted::of(&JValue::Undefined)
        }]),
        ..StubState::default()
    })
    .await;
    let settings = Settings::from_lookup(&|_: &str| None, None);
    let mut transport = transport(&settings);
    transport.base_url = base.trim_end_matches('/').to_owned();
    let mut wire = SigninWire {
        accounts: qa_accounts(1),
        client: client().unwrap(),
    };
    let cache = Mutex::new(SessionCache::default());
    let answer = tokio::time::timeout(
        DEADLINE,
        fetch_for(
            &mut wire,
            &cache,
            &transport,
            "qa-user",
            None,
            "http://x/v1/chat/completions",
            QA_BODY,
        ),
    )
    .await
    .unwrap();
    let read = read_reply(answer).await;
    assert_eq!(read.thrown.as_deref(), Some(SEND_FAILED));
    assert_eq!(
        stub.lock().unwrap().received.len(),
        1,
        "the redirect was not followed"
    );
}

#[tokio::test]
async fn dropping_the_reply_closes_the_backend_connection() {
    let f = fixture();
    let mut held = Scripted::of(&JValue::obj([
        ("status", JValue::Num(200.0)),
        ("chunks", f.get("streams").get("text").get("chunks").clone()),
    ]));
    held.hold_after = Some(1);
    let (base, stub) = stub(StubState {
        script: VecDeque::from([held]),
        ..StubState::default()
    })
    .await;
    let transport = fixture_transport(&f, base.trim_end_matches('/'));
    let mut wire = SigninWire {
        accounts: qa_accounts(1),
        client: client().unwrap(),
    };
    let cache = Mutex::new(SessionCache::default());
    let answer = fetch_for(
        &mut wire,
        &cache,
        &transport,
        "qa-user",
        Some("chat-1"),
        "http://x/v1/chat/completions",
        QA_BODY,
    )
    .await;
    let res = into_response(answer).unwrap();
    assert_eq!(res.status(), 200);
    let mut body = res.into_body();
    // The role chunk and the first part arrive while the backend holds the rest.
    let first = tokio::time::timeout(DEADLINE, body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(first.data_ref().unwrap()).into_owned();
    assert!(text.contains("\"role\":\"assistant\""), "{text}");
    // The turn is aborted: the reply is dropped, and with it the backend connection.
    drop(body);
    let gone = tokio::time::timeout(DEADLINE, async {
        loop {
            if stub.lock().unwrap().client_gone {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "the backend never saw the client go away");
}

#[tokio::test]
async fn the_cache_spans_calls_through_the_wire() {
    let f = fixture();
    let (base, stub) = stub(StubState {
        default: Some(Scripted::of(&JValue::obj([
            ("status", JValue::Num(200.0)),
            ("chunks", f.get("streams").get("text").get("chunks").clone()),
        ]))),
        ..StubState::default()
    })
    .await;
    let transport = fixture_transport(&f, base.trim_end_matches('/'));
    let mut wire = SigninWire {
        accounts: qa_accounts(1),
        client: client().unwrap(),
    };
    let cache = Mutex::new(SessionCache::default());
    for _ in 0..3 {
        let answer = tokio::time::timeout(
            DEADLINE,
            fetch_for(
                &mut wire,
                &cache,
                &transport,
                "qa-user",
                None,
                "http://x/v1/chat/completions",
                QA_BODY,
            ),
        )
        .await
        .unwrap();
        let read = read_reply(answer).await;
        assert_eq!(read.status, 200);
        assert!(read.text.ends_with("data: [DONE]\n\n"), "{}", read.text);
    }
    // One broker session ask for three calls inside the minute; three backend calls.
    let calls = wire.accounts.calls.lock().unwrap().clone();
    let asks = calls
        .iter()
        .filter(|c| json(c).contains("\"session\""))
        .count();
    assert_eq!(asks, 1, "{}", json(&JValue::Arr(calls)));
    assert_eq!(stub.lock().unwrap().received.len(), 3);
}
