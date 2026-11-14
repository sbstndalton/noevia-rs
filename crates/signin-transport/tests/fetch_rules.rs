//! The fetch driver's rules that one recorded call cannot show: the process-wide session cache
//! across calls, an unreadable change signal, the cache after a rejected sign-in, the capped read
//! of a failed answer chunk by chunk, a stream read to its end, and no token in any Debug output.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;
use signin_transport::fetch::{self, read_capped, Answer, Body, Call, Outgoing, Upstream, Wire};
use signin_transport::request::{Protocol, Session};
use signin_transport::session::{BrokerAnswer, SessionCache};
use signin_transport::texts::Texts;
use std::collections::VecDeque;
use std::sync::Mutex;

const NOW: f64 = 1_800_000_000_000.0;
const TEXT_STREAM: [&str; 3] = [
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
];

fn texts() -> Texts {
    Texts {
        label: "Service".into(),
        disconnected: "Sign in first.".into(),
        reconnect: "Sign in again.".into(),
        transient: "Try again shortly.".into(),
        not_included: "Not included.".into(),
        source: "service".into(),
    }
}

fn protocol() -> Protocol {
    Protocol {
        account_header: "service-account-id".into(),
        region_header: "x-region".into(),
        user_agent: "qa-agent".into(),
    }
}

fn session(token: &str, generation: f64) -> Session {
    Session {
        access_token: token.into(),
        account_id: "acct-1".into(),
        restricted_region: false,
        expires_at: Some(NOW + 3_600_000.0),
        generation: JValue::Num(generation),
    }
}

struct Chunks(VecDeque<Result<Vec<u8>, String>>);

impl Body for Chunks {
    async fn chunk(&mut self) -> Option<Result<Vec<u8>, String>> {
        self.0.pop_front()
    }
}

fn chunks(parts: &[&[u8]]) -> Chunks {
    Chunks(parts.iter().map(|p| Ok(p.to_vec())).collect())
}

/// A broker that hands out the next session per ask, and a backend answering the statuses in turn.
struct Fake {
    signal: Option<String>,
    sessions: VecDeque<Session>,
    statuses: VecDeque<u16>,
    log: Vec<String>,
}

impl Fake {
    fn new(signal: Option<&str>, sessions: Vec<Session>, statuses: Vec<u16>) -> Self {
        Fake {
            signal: signal.map(str::to_string),
            sessions: sessions.into(),
            statuses: statuses.into(),
            log: Vec::new(),
        }
    }
}

impl Wire for Fake {
    type Body = Chunks;
    fn signal(&self) -> Option<String> {
        self.signal.clone()
    }
    async fn catalogue(&mut self, _user: &str) -> Option<JValue> {
        None
    }
    async fn session(&mut self, user: &str, rejected: Option<&str>) -> BrokerAnswer {
        self.log
            .push(format!("session {user} {}", rejected.unwrap_or("-")));
        match self.sessions.pop_front() {
            Some(s) => BrokerAnswer::Ok(s),
            None => BrokerAnswer::Unreachable,
        }
    }
    async fn reject(&mut self, user: &str, generation: &JValue) {
        self.log.push(format!(
            "reject {user} {}",
            js_json::stringify(generation).unwrap()
        ));
    }
    async fn send(&mut self, request: Outgoing<'_>) -> Result<Upstream<Chunks>, String> {
        let auth = request
            .headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        self.log.push(format!("send {auth}"));
        let status = self.statuses.pop_front().unwrap_or(200);
        let parts: Vec<&[u8]> = TEXT_STREAM.iter().map(|s| s.as_bytes()).collect();
        Ok(Upstream {
            status,
            labelled: false,
            body: Some(chunks(&parts)),
        })
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    for _ in 0..1_000_000 {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
    panic!("the fakes never wait");
}

fn call<'a>(t: &'a Texts, p: &'a Protocol, user: &'a str, body: &'a str, now: f64) -> Call<'a> {
    Call {
        texts: t,
        protocol: p,
        base_url: "https://backend.invalid/api",
        user,
        conversation: None,
        url: "http://x/v1/chat/completions",
        body,
        now_ms: now,
        reply_uuid: "00000000-0000-4000-8000-000000000000",
    }
}

const BODY: &str = r#"{"model":"m","messages":[{"role":"user","content":"q"}]}"#;
const STREAM_BODY: &str = r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"q"}]}"#;

fn status_of<B>(a: &Answer<B>) -> u16 {
    match a {
        Answer::Error(r) => r.status,
        Answer::Passthrough(u) => u.status,
        Answer::Completion(_) | Answer::Stream(_) => 200,
        Answer::Thrown(_) => 0,
    }
}

#[test]
fn a_second_call_within_the_minute_reuses_the_session() {
    let (t, p) = (texts(), protocol());
    let cache = Mutex::new(SessionCache::default());
    let mut wire = Fake::new(Some("sig"), vec![session("tok-1", 1.0)], vec![]);
    for now in [NOW, NOW + 59_000.0] {
        let a = block_on(fetch::fetch(&mut wire, &cache, &call(&t, &p, "u1", BODY, now)));
        assert_eq!(status_of(&a), 200);
    }
    assert_eq!(
        wire.log,
        ["session u1 -", "send Bearer tok-1", "send Bearer tok-1"]
    );
    // After the minute the broker is asked again (and has nothing left: the transient refusal).
    let a = block_on(fetch::fetch(
        &mut wire,
        &cache,
        &call(&t, &p, "u1", BODY, NOW + 60_000.0),
    ));
    assert_eq!(status_of(&a), 503);
    assert_eq!(wire.log.last().unwrap(), "session u1 -");
}

#[test]
fn a_changed_or_unreadable_signal_asks_the_broker_every_time() {
    let (t, p) = (texts(), protocol());
    let cache = Mutex::new(SessionCache::default());
    let mut wire = Fake::new(
        None,
        vec![session("tok-1", 1.0), session("tok-2", 2.0)],
        vec![],
    );
    for _ in 0..2 {
        block_on(fetch::fetch(&mut wire, &cache, &call(&t, &p, "u1", BODY, NOW)));
    }
    assert!(cache.lock().unwrap().is_empty(), "nothing is cached without a signal");
    assert_eq!(wire.log[2], "session u1 -");

    let mut wire = Fake::new(
        Some("a"),
        vec![session("tok-1", 1.0), session("tok-2", 2.0)],
        vec![],
    );
    let cache = Mutex::new(SessionCache::default());
    block_on(fetch::fetch(&mut wire, &cache, &call(&t, &p, "u1", BODY, NOW)));
    wire.signal = Some("b".into());
    block_on(fetch::fetch(&mut wire, &cache, &call(&t, &p, "u1", BODY, NOW)));
    assert_eq!(wire.log[3], "send Bearer tok-2");
}

#[test]
fn a_401_skips_the_cache_and_the_second_forgets_the_session() {
    let (t, p) = (texts(), protocol());
    let cache = Mutex::new(SessionCache::default());
    // Warm the cache with tok-1.
    let mut wire = Fake::new(
        Some("sig"),
        vec![session("tok-1", 1.0), session("tok-2", 2.0)],
        vec![200, 401, 401],
    );
    block_on(fetch::fetch(&mut wire, &cache, &call(&t, &p, "u1", BODY, NOW)));
    assert_eq!(cache.lock().unwrap().len(), 1);
    let a = block_on(fetch::fetch(&mut wire, &cache, &call(&t, &p, "u1", BODY, NOW)));
    let Answer::Error(r) = a else { panic!() };
    assert_eq!((r.status, r.code.as_deref()), (401, Some("reconnect")));
    assert_eq!(
        wire.log,
        [
            "session u1 -",
            "send Bearer tok-1",
            "send Bearer tok-1",
            "session u1 tok-1",
            "send Bearer tok-2",
            "reject u1 2",
        ]
    );
    assert!(cache.lock().unwrap().is_empty());
}

#[test]
fn other_users_keep_their_sessions() {
    let (t, p) = (texts(), protocol());
    let cache = Mutex::new(SessionCache::default());
    let mut wire = Fake::new(
        Some("sig"),
        vec![session("tok-a", 1.0), session("tok-b", 2.0), session("tok-b2", 3.0)],
        vec![200, 401, 401],
    );
    block_on(fetch::fetch(&mut wire, &cache, &call(&t, &p, "a", BODY, NOW)));
    block_on(fetch::fetch(&mut wire, &cache, &call(&t, &p, "b", BODY, NOW)));
    assert_eq!(cache.lock().unwrap().len(), 1, "only a's session is left");
    let a = block_on(fetch::fetch(&mut wire, &cache, &call(&t, &p, "a", BODY, NOW)));
    assert_eq!(status_of(&a), 200);
    assert_eq!(wire.log.last().unwrap(), "send Bearer tok-a");
}

#[test]
fn a_stream_reads_to_its_end_once() {
    let (t, p) = (texts(), protocol());
    let cache = Mutex::new(SessionCache::default());
    let mut wire = Fake::new(Some("sig"), vec![session("tok-1", 1.0)], vec![]);
    let a = block_on(fetch::fetch(
        &mut wire,
        &cache,
        &call(&t, &p, "u1", STREAM_BODY, NOW),
    ));
    let Answer::Stream(mut st) = a else { panic!() };
    let mut frames = Vec::new();
    while let Some(next) = block_on(st.next()) {
        frames.extend(next.unwrap());
    }
    assert_eq!(frames.len(), 6, "{frames:?}"); // role, Hel, lo, finish, usage, [DONE]
    assert_eq!(frames.last().unwrap(), "data: [DONE]\n\n");
    assert!(block_on(st.next()).is_none(), "closed stays closed");
}

#[test]
fn the_capped_read_follows_core() {
    let cap = 8;
    let read = |parts: Vec<Result<Vec<u8>, String>>| block_on(read_capped(&mut Chunks(parts.into()), cap));
    assert_eq!(read(vec![]).unwrap(), "");
    assert_eq!(read(vec![Ok(b"abc".to_vec()), Ok(b"de".to_vec())]).unwrap(), "abcde");
    // Cut inside the chunk that crosses the cap.
    assert_eq!(read(vec![Ok(b"abcdef".to_vec()), Ok(b"ghij".to_vec())]).unwrap(), "abcdefgh");
    // A character cut at the cap is dropped; an invalid byte before it is kept as U+FFFD.
    assert_eq!(
        read(vec![Ok(b"\xffabcde".to_vec()), Ok(b"z\xc3\xa9".to_vec())]).unwrap(),
        "\u{FFFD}abcdez"
    );
    // Exactly at the cap with more on offer (even an empty chunk): capped, the cut mark dropped.
    assert_eq!(read(vec![Ok(b"abcdefg\xc3".to_vec()), Ok(Vec::new())]).unwrap(), "abcdefg");
    // Exactly at the cap and then the end: not capped, the replacement stays.
    assert_eq!(read(vec![Ok(b"abcdefg\xc3".to_vec())]).unwrap(), "abcdefg\u{FFFD}");
    // A failed read before the cap fails the whole read; after the cap it does not matter.
    assert!(read(vec![Ok(b"ab".to_vec()), Err("reset".into())]).is_err());
    assert_eq!(read(vec![Ok(b"abcdefgh".to_vec()), Err("reset".into())]).unwrap(), "abcdefgh");
    // A leading byte-order mark is text here (Buffer#toString keeps it).
    assert_eq!(read(vec![Ok(b"\xef\xbb\xbfa".to_vec())]).unwrap(), "\u{FEFF}a");
}

#[test]
fn no_token_in_debug_output() {
    let h = vec![("authorization".to_string(), "Bearer secret-tok".to_string())];
    let out = Outgoing {
        url: "https://backend.invalid/api/responses",
        headers: &h,
        body: "{}",
    };
    let shown = format!("{out:?} {:?}", session("secret-tok", 1.0));
    assert!(!shown.contains("secret-tok"), "{shown}");
    assert!(shown.contains("authorization"), "{shown}");
}
