//! Replays core's recorded transport (tests/fixtures/node-transport.v1.json, written by
//! tests/record_transport.cjs from core's own sign-in provider module) through this crate: every request
//! mapping, every event stream (whole and cut at every byte) and every full fetch, from the
//! catalogue read and the refresh-once rule to the bytes the chat loop receives.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;
use signin_transport::events::{parts_of, Part};
use signin_transport::fetch::{self, Answer, Body, Call, Outgoing, Upstream, Wire};
use signin_transport::reply::error_body;
use signin_transport::request::{self, Protocol, Refusal, Session};
use signin_transport::session::{BrokerAnswer, SessionCache};
use signin_transport::texts::Texts;
use std::sync::Mutex;

fn fixture() -> JValue {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/node-transport.v1.json"
    ))
    .unwrap();
    js_json::parse(&text).unwrap()
}

fn s(v: &JValue) -> String {
    v.as_str()
        .unwrap_or_else(|| panic!("not a string: {v:?}"))
        .to_string()
}

fn texts(f: &JValue) -> Texts {
    let t = f.get("texts");
    Texts {
        label: s(t.get("label")),
        disconnected: s(t.get("disconnected")),
        reconnect: s(t.get("reconnect")),
        transient: s(t.get("transient")),
        not_included: s(t.get("notIncluded")),
        source: s(t.get("source")),
    }
}

fn b64(text: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("bad base64"),
    };
    let bytes: Vec<u8> = text.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, &c)| {
            acc | (u32::from(val(c)) << (18 - 6 * i))
        });
        for i in 0..chunk.len().saturating_sub(1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

fn chunks_of(v: &JValue) -> Vec<Vec<u8>> {
    match v {
        JValue::Arr(items) => items.iter().map(|c| b64(&s(c))).collect(),
        _ => Vec::new(),
    }
}

/// A part in core's shape: `{ delta }`, `{ error, status? }`, `{ finish, usage }`.
fn part_json(p: &Part) -> JValue {
    match p {
        Part::Delta(d) => JValue::obj([("delta", d.clone())]),
        Part::Error { message, status } => {
            let mut o = vec![("error".to_string(), JValue::from(message.as_str()))];
            if let Some(st) = status {
                o.push(("status".to_string(), JValue::Num(f64::from(*st))));
            }
            JValue::Obj(o)
        }
        Part::Finish { finish, usage } => JValue::obj([
            ("finish", JValue::from(finish.as_str())),
            ("usage", usage.clone()),
        ]),
    }
}

fn json(v: &JValue) -> String {
    js_json::stringify(v).unwrap()
}

#[test]
fn requests_match_core() {
    let f = fixture();
    let JValue::Arr(cases) = f.get("requests") else {
        panic!()
    };
    assert_eq!(cases.len(), 140, "recorded requests");
    for c in cases {
        let body = js_json::parse(&s(c.get("body"))).unwrap();
        let out = json(&JValue::Obj(request::to_responses_request(&body)));
        assert_eq!(out, s(c.get("out")), "body {}", s(c.get("body")));
    }
}

#[test]
fn streams_match_core_whole_and_cut_everywhere() {
    let f = fixture();
    let t = texts(&f);
    let now = f.get("now").as_num().unwrap();
    let JValue::Obj(streams) = f.get("streams") else {
        panic!()
    };
    assert_eq!(streams.len(), 32, "recorded streams");
    let mut differences = 0;
    for (name, st) in streams {
        let chunks = chunks_of(st.get("chunks"));
        let mut want = json(st.get("parts"));
        if want.contains("\"thrown\":\"Invalid time value\"") {
            // Named difference (noevia bug, see the crate report): core throws while formatting a
            // reset time past JS's date range, which kills the reply; the port leaves the time out.
            assert_eq!(name, "errorFarDate");
            want = format!(
                "[{{\"error\":\"{}\",\"status\":429}}]",
                t.usage_limit("", "")
            );
            differences += 1;
        }
        let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
        let got = JValue::Arr(parts_of(&t, now, &refs).iter().map(part_json).collect());
        assert_eq!(json(&got), want, "stream {name}");
        // The same bytes as one chunk, and cut at every single position: same parts.
        let whole: Vec<u8> = chunks.concat();
        let one = JValue::Arr(parts_of(&t, now, &[&whole]).iter().map(part_json).collect());
        assert_eq!(json(&one), want, "stream {name} as one chunk");
        for cut in 0..=whole.len() {
            let (a, b) = whole.split_at(cut);
            let two = JValue::Arr(parts_of(&t, now, &[a, b]).iter().map(part_json).collect());
            assert_eq!(json(&two), want, "stream {name} cut at {cut}");
        }
    }
    assert_eq!(differences, 1);
}

struct Run {
    status: u16,
    content_type: Option<String>,
    provider_message: Option<String>,
    text: String,
    read_error: Option<String>,
    thrown: Option<String>,
}

fn session_of(v: &JValue) -> BrokerAnswer {
    if v.as_str() == Some("throw") {
        return BrokerAnswer::Unreachable;
    }
    if v.get("ok").as_bool() != Some(true) {
        return BrokerAnswer::Refused {
            error: v.get("error").as_str().map(str::to_string),
            status: v.get("status").as_num(),
            code: v.get("code").as_str().map(str::to_string),
        };
    }
    let x = v.get("session");
    BrokerAnswer::Ok(Session {
        access_token: s(x.get("accessToken")),
        account_id: s(x.get("accountId")),
        restricted_region: x.get("fedRamp").truthy(),
        expires_at: x.get("expiresAt").as_num(),
        generation: x.get("generation").clone(),
    })
}

/// A recorded backend body: its chunks, then the end or a failed read after `fail_after` chunks.
struct FakeBody {
    chunks: std::collections::VecDeque<Vec<u8>>,
    fail_after: Option<usize>,
    fail_message: String,
    read: usize,
}

impl Body for FakeBody {
    async fn chunk(&mut self) -> Option<Result<Vec<u8>, String>> {
        if self.fail_after == Some(self.read) {
            return Some(Err(self.fail_message.clone()));
        }
        self.read += 1;
        self.chunks.pop_front().map(Ok)
    }
}

/// The case's fake broker and backend, logging what core's fakes logged.
struct FakeWire {
    user_catalogue: JValue,
    sessions: Vec<JValue>,
    upstream: Vec<JValue>,
    default_stream: JValue,
    sent: Vec<JValue>,
    broker_calls: Vec<JValue>,
}

impl Wire for FakeWire {
    type Body = FakeBody;
    fn signal(&self) -> Option<String> {
        Some("sig-1".into())
    }
    async fn catalogue(&mut self, user: &str) -> Option<JValue> {
        self.broker_calls
            .push(JValue::Arr(vec!["catalogue".into(), user.into()]));
        match &self.user_catalogue {
            JValue::Undefined => Some(fixture_catalogue()),
            JValue::Str(x) if x == "throw" => None,
            c if c.get("ok").as_bool() == Some(true) => Some(c.get("models").clone()),
            _ => None,
        }
    }
    async fn session(&mut self, user: &str, rejected: Option<&str>) -> BrokerAnswer {
        self.broker_calls.push(JValue::Arr(vec![
            "session".into(),
            user.into(),
            rejected.map_or(JValue::Null, JValue::from),
        ]));
        session_of(&self.sessions.remove(0))
    }
    async fn reject(&mut self, user: &str, generation: &JValue) {
        self.broker_calls.push(JValue::Arr(vec![
            "reject".into(),
            user.into(),
            generation.clone(),
        ]));
    }
    async fn send(&mut self, request: Outgoing<'_>) -> Result<Upstream<FakeBody>, String> {
        self.sent.push(JValue::obj([
            ("url", JValue::from(request.url)),
            (
                "headers",
                JValue::Obj(
                    request
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), JValue::from(v.as_str())))
                        .collect(),
                ),
            ),
            ("body", JValue::from(request.body)),
            ("redirect", JValue::from("error")),
        ]));
        // core's fake backend answers the text stream once a case's list is used up.
        let u = if self.upstream.is_empty() {
            JValue::obj([
                ("status", JValue::Num(200.0)),
                ("chunks", self.default_stream.clone()),
            ])
        } else {
            self.upstream.remove(0)
        };
        if let Some(why) = u.get("throws").as_str() {
            return Err(why.to_string());
        }
        let status = u.get("status").as_num().map_or(200, |x| x as u16);
        let body = (!u.get("nobody").truthy()).then(|| FakeBody {
            chunks: chunks_of(u.get("chunks")).into(),
            fail_after: u.get("failAfter").as_num().map(|n| n as usize),
            fail_message: u.get("failMessage").as_str().unwrap_or("reset").to_string(),
            read: 0,
        });
        // core's fake backend: its content type (text/event-stream unless the case names one), and
        // the provider-message label when the case is labelled.
        let mut headers = vec![(
            "content-type".to_string(),
            u.get("contentType")
                .as_str()
                .unwrap_or("text/event-stream")
                .to_string(),
        )];
        let labelled = u.get("labelled").truthy();
        if labelled {
            headers.push(("x-noevia-provider-message".into(), "1".into()));
        }
        Ok(Upstream {
            status,
            labelled,
            headers,
            body,
        })
    }
}

/// Runs a future whose every await is ready at once (the fakes never wait).
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    for _ in 0..1_000_000 {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
    panic!("the fake wire never waits; a pending future is a bug");
}

/// What the chat loop reads from the answer: TextDecoder over every byte (a leading mark dropped).
fn decode_all(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    text.strip_prefix('\u{FEFF}')
        .map_or(text.clone(), str::to_string)
}

fn refused(r: &Refusal) -> Run {
    Run {
        status: r.status,
        content_type: Some("application/json".into()),
        provider_message: Some("1".into()),
        text: error_body(r),
        read_error: None,
        thrown: None,
    }
}

/// core's fetchFor through the crate's own driver ([`fetch::fetch`]), with the case's fakes.
fn run_fetch(
    t: &Texts,
    protocol: &Protocol,
    now: f64,
    input: &JValue,
    default_stream: &JValue,
) -> (Run, FakeWire, SessionCache) {
    let JValue::Arr(sessions) = input.get("sessions").clone() else {
        panic!()
    };
    let JValue::Arr(upstream) = input.get("upstream").clone() else {
        panic!()
    };
    let mut wire = FakeWire {
        user_catalogue: input.get("catalogue").clone(),
        sessions,
        upstream,
        default_stream: default_stream.clone(),
        sent: Vec::new(),
        broker_calls: Vec::new(),
    };
    let cache = Mutex::new(SessionCache::default());
    let url = input
        .get("url")
        .as_str()
        .unwrap_or("http://x/v1/chat/completions")
        .to_string();
    let body = s(input.get("body"));
    let conversation = input.get("conversation").as_str().map(str::to_string);
    let call = Call {
        texts: t,
        protocol,
        base_url: "https://backend.invalid/api/",
        user: "qa-user",
        conversation: conversation.as_deref(),
        url: &url,
        body: &body,
        now_ms: now,
        reply_uuid: "00000000-0000-4000-8000-000000000000",
    };
    let answer = block_on(fetch::fetch(&mut wire, &cache, &call));
    let run = match answer {
        Answer::Error(r) => refused(&r),
        Answer::Passthrough(mut u) => {
            let mut bytes = Vec::new();
            let mut read_error = None;
            if let Some(b) = u.body.as_mut() {
                loop {
                    match block_on(b.chunk()) {
                        Some(Ok(c)) => bytes.extend(c),
                        Some(Err(e)) => {
                            read_error = Some(e);
                            break;
                        }
                        None => break,
                    }
                }
            }
            let header = |name: &str| {
                u.headers
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            };
            Run {
                status: u.status,
                content_type: header("content-type"),
                provider_message: header("x-noevia-provider-message"),
                text: decode_all(&bytes),
                read_error,
                thrown: None,
            }
        }
        Answer::Completion(body) => Run {
            status: 200,
            content_type: Some("application/json".into()),
            provider_message: None,
            text: body,
            read_error: None,
            thrown: None,
        },
        Answer::Stream(mut st) => {
            let mut text = String::new();
            let mut read_error = None;
            while let Some(next) = block_on(st.next()) {
                match next {
                    Ok(frames) => text.push_str(&frames.concat()),
                    Err(e) => {
                        read_error = Some(e);
                        break;
                    }
                }
            }
            Run {
                status: 200,
                content_type: Some("text/event-stream".into()),
                provider_message: None,
                text,
                read_error,
                thrown: None,
            }
        }
        Answer::Thrown(m) => Run {
            status: 0,
            content_type: None,
            provider_message: None,
            text: String::new(),
            read_error: None,
            thrown: Some(m),
        },
    };
    let cache = cache.into_inner().unwrap();
    (run, wire, cache)
}

/// Cases where core's fetch rejects on purpose-free TypeErrors and the port answers instead
/// (src/fetch.rs "Named differences"): name → the case whose answer the port gives.
/// Read failures after the terminal event (see fetch_path_matches_core).
const LATE: [&str; 2] = ["streamFailAfterFinish", "completeFailAfterError"];

const NAMED: [(&str, &str); 8] = [
    ("catalogueNullEntry", "catalogueNullEntryAnswer"),
    ("bodyNull", "bodyNull"),
    ("catalogueObject", "catalogueNull"),
    ("catalogueString", "catalogueString"),
    ("catalogueNumber", "catalogueNull"),
    ("catalogueNonArrayNoBody", "catalogueNonArrayNoBody"),
    ("noBody204", "noBody204"),
    ("noBody304", "noBody304"),
];

fn fixture_catalogue() -> JValue {
    js_json::parse(
        r#"[{"slug":"m-large","effort":"medium","summary":"detailed","summaryParam":true,"levels":["low","medium","high","xhigh"]},{"slug":"m-small","summaryParam":false},{"slug":"m-none","summary":"none","summaryParam":true,"levels":[]},{"slug":"m-auto","summaryParam":true,"effort":""}]"#,
    )
    .unwrap()
}

#[test]
fn fetch_path_matches_core() {
    let f = fixture();
    let t = texts(&f);
    let now = f.get("now").as_num().unwrap();
    let protocol = Protocol {
        account_header: "service-account-id".into(),
        region_header: "x-vendor-fedramp".into(),
        user_agent: s(f.get("userAgent")),
    };
    let default_stream = f.get("streams").get("text").get("chunks").clone();
    let JValue::Arr(cases) = f.get("fetch") else {
        panic!()
    };
    assert_eq!(cases.len(), 107, "recorded fetch cases");
    let by_name = |n: &str| {
        cases
            .iter()
            .find(|c| s(c.get("input").get("name")) == n)
            .unwrap_or_else(|| panic!("no case {n}"))
    };
    let mut checked = 0;
    let mut named = 0;
    let mut late = 0;
    let mut late_text = 0;
    let mut wrong: Vec<String> = Vec::new();
    let mut check = |ok: bool, what: String| {
        if !ok {
            wrong.push(what);
        }
    };
    for c in cases {
        let (input, want) = (c.get("input"), c.get("output"));
        let name = s(input.get("name"));
        let (got, wire, cache) = run_fetch(&t, &protocol, now, input, &default_stream);
        checked += 1;
        if LATE.contains(&name.as_str()) {
            // Named difference: a read failure after the reply's terminal event. Core's clean-up
            // cancels the already-failed body, the cancel rethrows, and the finished reply fails;
            // the port finishes it. Core's bytes are a prefix of the port's.
            late += 1;
            let failed =
                want.get("readError").as_str().is_some() || want.get("thrown").as_str().is_some();
            check(
                failed,
                format!("{name}: core no longer fails late ({})", json(want)),
            );
            check(
                got.thrown.is_none() && got.read_error.is_none(),
                format!(
                    "{name}: port failed {:?}",
                    got.thrown.as_ref().or(got.read_error.as_ref())
                ),
            );
            // A failed read leaves what came before it; nothing recorded counts as nothing read.
            let prefix = want.get("text").as_str().unwrap_or("");
            late_text += prefix.len();
            check(
                got.text.starts_with(prefix),
                format!("{name}: core's bytes are not a prefix"),
            );
            continue;
        }
        if let Some(thrown) = want.get("thrown").as_str() {
            if NAMED.iter().any(|(n, _)| *n == name) {
                // core rejects with a TypeError; the port answers (pinned below).
                check(
                    got.thrown.is_none(),
                    format!("{name}: the port answers ({thrown})"),
                );
                named += 1;
                continue;
            }
            check(
                got.thrown.as_deref() == Some(thrown),
                format!("{name} thrown: {:?} vs {thrown}", got.thrown),
            );
        } else {
            check(
                got.thrown.is_none(),
                format!("{name}: port threw {:?}", got.thrown),
            );
            check(
                Some(f64::from(got.status)) == want.get("status").as_num(),
                format!("{name} status {} ({})", got.status, got.text),
            );
            check(
                got.content_type.as_deref() == want.get("contentType").as_str(),
                format!("{name} content-type {:?}", got.content_type),
            );
            check(
                got.provider_message.as_deref() == want.get("providerMessage").as_str(),
                format!("{name} provider-message"),
            );
            check(
                got.text == s(want.get("text")),
                format!(
                    "{name} text:\n port {}\n core {}",
                    got.text,
                    s(want.get("text"))
                ),
            );
            check(
                got.read_error.as_deref() == want.get("readError").as_str(),
                format!("{name} read error {:?}", got.read_error),
            );
        }
        check(
            json(&JValue::Arr(wire.sent)) == json(want.get("sent")),
            format!("{name} sent"),
        );
        check(
            json(&JValue::Arr(wire.broker_calls)) == json(want.get("brokerCalls")),
            format!("{name} broker calls"),
        );
        // A rejected sign-in leaves nothing cached (core deletes the user's entry on the second 401).
        if json(want.get("brokerCalls")).contains("\"reject\"") {
            check(
                cache.is_empty(),
                format!("{name}: cache kept a rejected session"),
            );
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} cases differ:\n{}",
        wrong.len(),
        cases.len(),
        wrong.join("\n")
    );
    assert_eq!(late, LATE.len());
    assert!(late_text > 0, "core recorded no text before any late failure");
    assert_eq!(checked, cases.len());
    assert_eq!(named, NAMED.len());
    // The named differences answer as the port says they should.
    let run = |n: &str| run_fetch(&t, &protocol, now, by_name(n).get("input"), &default_stream).0;
    for (n, like) in NAMED {
        let got = run(n);
        match n {
            "noBody204" | "noBody304" => {
                assert_eq!(got.status, 502, "{n}");
                let code = &n[6..];
                assert_eq!(
                    got.text,
                    format!("{{\"error\":{{\"message\":\"Service returned {code}\"}}}}"),
                    "{n}"
                );
            }
            "catalogueString" => {
                // Unreadable list + an effort beyond the basic four: core's "could not be read" line.
                assert_eq!(got.status, 400, "{n}");
                assert!(got.text.contains("could not be read"), "{n}: {}", got.text);
            }
            "bodyNull" => {
                assert_eq!(got.status, 400, "{n}");
                assert!(got.text.contains("not valid JSON"), "{n}: {}", got.text);
            }
            "catalogueNonArrayNoBody" => {
                // Unreadable list: no allowlist, a basic effort goes through to the backend.
                assert_eq!(got.status, 200, "{n}: {}", got.text);
            }
            _ => {
                let want = run(like);
                assert_eq!(
                    (got.status, &got.text),
                    (want.status, &want.text),
                    "{n} answers as {like}"
                );
            }
        }
    }
}
