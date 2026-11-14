//! The provider-row host around whole native turns (`chat_turn::native::run`): a scripted engine behind a fake
//! front host, the same turn run plain and through [`ProviderHost`].
//!
//! - Differential: for every answer shape (a streamed reply, a tool round then a reply, 401, 429 with
//!   retry-after, 500, a reset mid-stream, a refused connection, the client leaving mid-stream) the wrapped turn
//!   writes exactly the events of the plain one; only the requests' headers differ, and they are the row's
//!   `providerHeaders` (Content-Type, then the bearer of the key as it is at that request).
//! - Fail closed: a row changed, removed, re-shared or moved, a changed user, an unapproved member origin, an
//!   expired binding, a request to another address or model: nothing reaches the engine, and the turn ends with
//!   `${label} unreachable: <reason>`, exactly as the plain turn whose engine threw that message (chat.cjs's
//!   transport-error path, ~2151).
//! - Node's provider-row rules: no lease (a frame asking for one included: the lease token never leaves), no
//!   model manager, no draft evidence; usage and prefill still recorded; a redirect is `fetch failed`.
//! - Property: the SSE answer split at any byte positions (mid-line, mid-UTF-8) gives the same events.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use chat_turn::host::{
    Answer, EditTarget, EngineRequest, GateRefusal, Host, Ran, Read, Scrub, Skill, Target as ToolTarget,
    ToolRun,
};
use chat_turn::js;
use chat_turn::native::{self, Ended, Helpers, Process};
use chat_turn::prepared::Prepared;
use chat_upstream::binding::{self, Binding};
use chat_upstream::host::{self as phost, ProviderHost, Rows, RowsNow, Unsupported};
use chat_upstream::target::{self, Connection, Fetch, KeyUse, Target, Turn, User};
use js_json::JValue;
use proptest::prelude::*;

const NOW: f64 = 1_794_571_200_000.0;
const DEFAULT_ID: &str = "default";
const LABEL: &str = "QA Provider";

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

fn row(base: &str, shared: bool) -> JValue {
    js_json::parse(&format!(
        r#"{{"id":"qa-provider","label":"{LABEL}","baseUrl":"{base}","apiKey":"qa-key-sealed","shared":{shared}}}"#
    ))
    .unwrap()
}

fn member() -> User {
    User {
        id: Some("qa-member".into()),
        admin: false,
    }
}

/// The target as the cut resolves it (`target::resolve`, Node's order) for a member's own row.
fn resolved(r: &JValue) -> Target {
    let user = member();
    target::resolve(&Turn {
        row: r,
        default_id: DEFAULT_ID,
        user: Some(&user),
        member_origins: "https://qa-provider.example",
        machine: None,
        sign_in: false,
        sign_in_enabled: false,
        connection: Connection::Missing,
        workspace_user: Some("qa-member"),
        chat_id: Some("qa-chat-1"),
        space_id: None,
    })
    .unwrap()
    .unwrap()
}

fn bound(r: &JValue, t: &Target) -> Binding {
    Binding {
        user: "qa-member".into(),
        provider_id: t.provider_id.clone(),
        url: t.url.clone(),
        fingerprint: binding::fingerprint(r),
        model: "qa-model".into(),
        chat_key: Some("qa-chat-1".into()),
        expires_ms: NOW + 600_000.0,
    }
}

/// A prepared frame for a provider-row turn (shape as core writes it; rust-chat-native.cjs).
fn frame(url: &str, tools: bool, model_lease: bool) -> JValue {
    let tools = if tools {
        r#"[{"type":"function","function":{"name":"get_current_time","description":"Get the current date and time on the server.","parameters":{"type":"object","properties":{"timezone":{"type":"string"}},"required":[]}}}]"#
    } else {
        "[]"
    };
    js_json::parse(&format!(
        r#"{{"type":"prepared","v":1,"startedAt":{NOW},"model":"qa-model","role":null,"auto":false,"effort":"default","thinkingOff":false,"sampling":{{}},
"provider":{{"id":"qa-provider","label":"{LABEL}","localEngine":false,"capabilities":null}},
"upstreamUrl":"{url}","limitScope":"qa-scope",
"messages":[{{"role":"system","content":"QA system prompt."}},{{"role":"user","content":"QA hello ünï"}}],
"tools":{tools},"routes":[],"writes":[],"public":[],"automaticOnlyWrites":[],"editTools":[],"routeNote":"",
"templateGate":false,"managerEnabled":false,"modelLease":{model_lease},
"chat":{{"id":"qa-chat-1","contextId":"qa-chat-1","key":"qa-chat-1","grantScope":"project:qa-project","grantHolder":{{"kind":"project","id":"qa-project"}},"recentWrites":[],
"title":{{"chatId":"qa-chat-1","listId":"qa-project","firstMessage":"QA hello","firstReply":true,"enabled":false}}}},
"turn":{{"message":"QA hello ünï","spaceId":null,"projectId":"qa-project","declineEndsReply":true,"record":true,"streamIdleMs":120000,"compactOnly":false,"sources":{{"reply":[],"injected":[]}}}},
"skills":{{"loaded":[],"enabled":[],"revokedInHistory":[],"imageWarning":"","coverageWarning":""}}}}"#
    ))
    .unwrap()
}

/// One scripted engine answer.
#[derive(Debug, Clone)]
enum Scripted {
    Throws(String),
    Status(u16, Vec<(String, String)>, String),
    /// The chunks, then how the stream ends.
    Stream(Vec<Vec<u8>>, Tail),
}

#[derive(Debug, Clone, Copy)]
enum Tail {
    End,
    Reset,
    Leave,
}

#[derive(Debug, Default)]
struct Body {
    reads: VecDeque<Read>,
    text: String,
}

/// The front's side, faked: a script of engine answers, an in-memory state store, and a log of the calls
/// the provider path must make or must not make.
#[derive(Default)]
struct Fake {
    script: Mutex<VecDeque<Scripted>>,
    requests: Mutex<Vec<EngineRequest>>,
    events: Mutex<Vec<String>>,
    left: Mutex<bool>,
    calls: Mutex<Vec<String>>,
    files: Mutex<BTreeMap<String, String>>,
}

impl Fake {
    fn new(script: Vec<Scripted>) -> Fake {
        Fake {
            script: Mutex::new(script.into()),
            ..Fake::default()
        }
    }
    fn call(&self, c: impl Into<String>) {
        self.calls.lock().unwrap().push(c.into());
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
    fn requests(&self) -> Vec<EngineRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Host for Fake {
    type Body = Body;

    fn emit(&self, event: &JValue) {
        if !*self.left.lock().unwrap() {
            self.events.lock().unwrap().push(js::stringify(event));
        }
    }
    fn left(&self) -> bool {
        *self.left.lock().unwrap()
    }
    fn now_ms(&self) -> f64 {
        NOW
    }
    async fn lease(&self, model: &str) -> Result<Option<Vec<(String, String)>>, GateRefusal> {
        self.call(format!("lease {model}"));
        Ok(Some(vec![("X-Noevia-Lease".into(), "qa-lease-1".into())]))
    }
    fn release_lease(&self) {}
    async fn health(&self) -> Option<JValue> {
        self.call("health");
        None
    }
    async fn load(&self, model: &str) -> bool {
        self.call(format!("load {model}"));
        false
    }
    async fn template_allows_tools(&self, _model: &str) -> bool {
        true
    }
    async fn request(&self, request: &EngineRequest) -> Result<Answer<Body>, String> {
        self.requests.lock().unwrap().push(request.clone());
        let next = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("no scripted answer for request {request:?}"));
        match next {
            Scripted::Throws(e) => Err(e),
            Scripted::Status(status, headers, text) => Ok(Answer {
                status,
                headers,
                body: Body {
                    text,
                    ..Body::default()
                },
            }),
            Scripted::Stream(chunks, tail) => {
                let mut reads: VecDeque<Read> = chunks.into_iter().map(Read::Chunk).collect();
                reads.push_back(match tail {
                    Tail::End => Read::End,
                    Tail::Reset => Read::Failed("terminated".into()),
                    Tail::Leave => Read::Aborted,
                });
                Ok(Answer {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    body: Body {
                        reads,
                        ..Body::default()
                    },
                })
            }
        }
    }
    async fn read(&self, body: &mut Body, _idle_ms: Option<u64>) -> Read {
        let r = body.reads.pop_front().unwrap_or(Read::End);
        if r == Read::Aborted {
            *self.left.lock().unwrap() = true;
        }
        r
    }
    async fn text(&self, body: Body, cap: usize) -> (String, bool) {
        let capped = body.text.len() > cap;
        (body.text, capped)
    }
    fn read_state(&self, id: &str) -> JValue {
        match self.files.lock().unwrap().get(id) {
            Some(t) => js_json::wtf16::parse(t).unwrap(),
            None => js::obj(Vec::<(String, JValue)>::new()),
        }
    }
    fn save_state(&self, id: &str, state: &JValue) -> Result<(), String> {
        self.files
            .lock()
            .unwrap()
            .insert(id.to_string(), js::stringify(state));
        Ok(())
    }
    fn mark_tools_unsupported(&self, model: &str) {
        self.call(format!("tools-unsupported {model}"));
    }
    async fn engine_busy(&self) -> bool {
        false
    }
    async fn title_auto_named(&self, _l: Option<&str>, _c: &str, _f: &str) -> bool {
        false
    }
    fn title_save(&self, _l: Option<&str>, _c: &str, _t: &str, _f: &str) {}
    fn record_usage(&self, model: &str, reported: &JValue) {
        self.call(format!("usage {model} {}", js::stringify(reported)));
    }
    fn user_id(&self) -> String {
        "qa-member".into()
    }
    fn uuid(&self) -> String {
        "qa-uuid-1".into()
    }
    fn tool_policy(&self, _name: &str, _write: bool) -> String {
        "allow".into()
    }
    async fn write_target(&self, _n: &str, _a: &str, _k: Option<&str>) -> ToolTarget {
        ToolTarget::None
    }
    async fn edit_target(&self, _p: Option<&str>, _n: &str, _a: &str) -> EditTarget {
        EditTarget::Failed
    }
    async fn permission_binding(
        &self,
        _c: Option<&str>,
        _s: &str,
        _h: &JValue,
    ) -> Result<JValue, ()> {
        Ok(JValue::Null)
    }
    async fn chat_wide_approved(
        &self,
        _c: Option<&str>,
        _s: &str,
        _b: &JValue,
        _e: &str,
    ) -> Option<String> {
        None
    }
    async fn open_approval(&self, _r: &JValue) -> bool {
        false
    }
    async fn await_approval(&self, _r: &JValue) -> String {
        "deny".into()
    }
    async fn revoke_grant(&self, _c: Option<&str>) {}
    fn audit(&self, event: &str, _d: &JValue) {
        self.call(format!("audit {event}"));
    }
    async fn execute_tool(&self, run: &ToolRun) -> Ran {
        self.call(format!("execute {}", run.name));
        Ran::Done {
            text: "QA time: 2026-11-14T11:00:00Z".into(),
            failed: false,
            not_run: false,
        }
    }
    async fn skills_revoked(&self, _p: &str, _l: &[Skill]) -> Option<Vec<(String, String)>> {
        Some(Vec::new())
    }
    async fn skills_read(
        &self,
        _p: &str,
        _c: Option<&str>,
        _k: &[Skill],
        _a: &str,
        _r: &str,
    ) -> Option<Vec<Skill>> {
        Some(Vec::new())
    }
    async fn skills_scrub(&self, _p: &str, _c: Option<&str>, _m: &[JValue]) -> Option<Scrub> {
        Some(Scrub::default())
    }
    fn prefill_sample(&self, model: &str, prompt: f64, _ms: f64) {
        self.call(format!("prefill {model} {prompt}"));
    }
    fn mtp_evidence(&self, model: &str, drafted: f64, accepted: f64) {
        self.call(format!("mtp {model} {drafted} {accepted}"));
    }
}

/// The account as of each request: the n-th `now()` reads `states[n]` (the last repeats), the n-th
/// `key()` reads `keys[n]` likewise.
struct ScriptRows {
    states: Vec<RowsNow>,
    keys: Vec<Option<String>>,
    asked: Mutex<(usize, usize)>,
}

impl ScriptRows {
    fn steady(r: &JValue) -> ScriptRows {
        ScriptRows::with(vec![now_of(r)], vec![Some("qa-key-1".into())])
    }
    fn with(states: Vec<RowsNow>, keys: Vec<Option<String>>) -> ScriptRows {
        ScriptRows {
            states,
            keys,
            asked: Mutex::new((0, 0)),
        }
    }
}

fn now_of(r: &JValue) -> RowsNow {
    RowsNow {
        user: "qa-member".into(),
        user_active: true,
        row: Some(r.clone()),
        member_own: true,
        origin_approved: true,
    }
}

impl Rows for ScriptRows {
    fn now(&self, _b: &Binding) -> RowsNow {
        let mut a = self.asked.lock().unwrap();
        let s = self.states[a.0.min(self.states.len() - 1)].clone();
        a.0 += 1;
        s
    }
    fn key(&self, provider_id: &str) -> Option<String> {
        assert_eq!(provider_id, "qa-provider");
        let mut a = self.asked.lock().unwrap();
        let k = self.keys[a.1.min(self.keys.len() - 1)].clone();
        a.1 += 1;
        k
    }
}

fn run_on<H: Host>(host: &H, frame: &JValue) -> Ended {
    let p = Prepared::read(frame).unwrap_or_else(|e| panic!("{e:?}"));
    let helpers = Helpers {
        clean_search_args: &|n, a| chat_turn::helpers::clean_search_args(n, a).into_owned(),
        announces_tool_action: &chat_turn::helpers::announces_tool_action,
        announced_note: chat_turn::helpers::ANNOUNCED_NOTE,
        reduce_tool_result: &|text, cap| chat_turn::reduce::reduce_tool_result(text, cap).text,
        source_links: &chat_turn::helpers::source_links,
        fallback_text: &|s| chat_turn::helpers::fallback_text(s),
        final_answer_note: chat_turn::helpers::FINAL_ANSWER_NOTE,
        tool_result_cap: 8000,
    };
    block_on(native::run(host, &Process::default(), &helpers, &p))
}

fn sse(frames: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in frames {
        out.extend_from_slice(format!("data: {f}\n\n").as_bytes());
    }
    out
}

fn reply_stream() -> Vec<u8> {
    sse(&[
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"QA "},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"réponse ✓ 🙂"},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":40,"completion_tokens":4,"total_tokens":44}}"#,
        "[DONE]",
    ])
}

fn tool_stream() -> Vec<u8> {
    sse(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"qa-call-1","type":"function","function":{"name":"get_current_time","arguments":"{}"}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ])
}

fn stream(bytes: Vec<u8>, tail: Tail) -> Scripted {
    Scripted::Stream(vec![bytes], tail)
}

/// The answer shapes of the differential, each with whether the frame offers the tool.
fn scenarios() -> Vec<(&'static str, bool, Vec<Scripted>)> {
    let half = reply_stream();
    let cut = half.len() / 2;
    vec![
        ("reply", false, vec![stream(reply_stream(), Tail::End)]),
        (
            "tool-then-reply",
            true,
            vec![stream(tool_stream(), Tail::End), stream(reply_stream(), Tail::End)],
        ),
        (
            "unauthorized",
            false,
            vec![Scripted::Status(
                401,
                vec![("content-type".into(), "application/json".into())],
                r#"{"error":{"message":"Incorrect API key provided: qa-k***","type":"invalid_request_error"}}"#.into(),
            )],
        ),
        (
            "rate-limited",
            false,
            vec![Scripted::Status(
                429,
                vec![("retry-after".into(), "7".into())],
                r#"{"error":{"message":"Rate limit reached","type":"requests"}}"#.into(),
            )],
        ),
        (
            "server-error",
            false,
            vec![Scripted::Status(500, Vec::new(), "upstream exploded".into())],
        ),
        (
            "reset-mid-stream",
            false,
            vec![Scripted::Stream(vec![half[..cut].to_vec()], Tail::Reset)],
        ),
        (
            "refused-connection",
            false,
            vec![Scripted::Throws("fetch failed".into())],
        ),
        (
            "client-leaves",
            false,
            vec![Scripted::Stream(vec![half[..cut].to_vec()], Tail::Leave)],
        ),
    ]
}

#[test]
fn wrapped_turns_write_the_plain_turns_events() {
    let r = row("https://qa-provider.example", false);
    let t = resolved(&r);
    assert_eq!(t.url, "https://qa-provider.example/v1/chat/completions");
    assert!(t.member_own);
    for (name, tools, script) in scenarios() {
        let f = frame(&t.url, tools, false);
        let plain = Fake::new(script.clone());
        let ended_plain = run_on(&plain, &f);

        let inner = Fake::new(script);
        let rows = ScriptRows::steady(&r);
        let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
        let ended = run_on(&host, &f);

        assert_eq!(ended, ended_plain, "{name}: how the turn ended");
        assert_eq!(inner.events(), plain.events(), "{name}: the events");
        assert!(!inner.events().is_empty(), "{name}: wrote nothing");
        let (pr, wr) = (plain.requests(), inner.requests());
        assert_eq!(wr.len(), pr.len(), "{name}: request count");
        for (p, w) in pr.iter().zip(&wr) {
            assert_eq!(w.url, p.url, "{name}");
            assert_eq!(w.body, p.body, "{name}: the body is the executor's, unchanged");
            assert_eq!(w.stream, p.stream, "{name}");
            assert_eq!(w.timeout_ms, p.timeout_ms, "{name}");
            assert_eq!(
                w.headers,
                vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Authorization".to_string(), "Bearer qa-key-1".to_string()),
                ],
                "{name}: providerHeaders"
            );
        }
        // Usage and the prefill sample are recorded for any provider; the tool ran through the inner host.
        assert_eq!(inner.calls(), plain.calls(), "{name}: side calls");
        assert!(!inner.calls().iter().any(|c| c.starts_with("lease")
            || c == "health"
            || c.starts_with("load")
            || c.starts_with("mtp")));
    }
}

#[test]
fn reply_turn_shape() {
    let r = row("https://qa-provider.example/v1/", false);
    let t = resolved(&r);
    let inner = Fake::new(vec![stream(reply_stream(), Tail::End)]);
    let rows = ScriptRows::steady(&r);
    let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
    assert_eq!(run_on(&host, &frame(&t.url, false, false)), Ended::Done);
    let events = inner.events();
    let text: String = events
        .iter()
        .filter_map(|e| {
            let v = js_json::parse(e).unwrap();
            (v.get("type").as_str() == Some("delta"))
                .then(|| v.get("text").as_str().unwrap_or("").to_string())
        })
        .collect();
    assert_eq!(text, "QA réponse ✓ 🙂", "events: {events:#?}");
    assert!(events.last().unwrap().contains(r#""type":"done""#), "{events:#?}");
    assert!(inner.calls().iter().any(|c| c.starts_with("usage qa-model")));
}

/// The key as it is at each request: rotated between the tool round and the reply, the reply carries the new
/// one; a key that cannot be unsealed (or `local`) sends no bearer.
#[test]
fn key_is_read_per_request() {
    let r = row("https://qa-provider.example", false);
    let t = resolved(&r);
    let inner = Fake::new(vec![
        stream(tool_stream(), Tail::End),
        stream(reply_stream(), Tail::End),
    ]);
    let rows = ScriptRows::with(vec![now_of(&r)], vec![Some("qa-key-1".into()), None]);
    let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
    assert_eq!(run_on(&host, &frame(&t.url, true, false)), Ended::Done);
    let reqs = inner.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].headers.len(), 2);
    assert_eq!(
        reqs[1].headers,
        vec![("Content-Type".to_string(), "application/json".to_string())]
    );
    for k in ["local", ""] {
        let inner = Fake::new(vec![stream(reply_stream(), Tail::End)]);
        let rows = ScriptRows::with(vec![now_of(&r)], vec![Some(k.into())]);
        let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
        run_on(&host, &frame(&t.url, false, false));
        assert_eq!(inner.requests()[0].headers.len(), 1, "key {k:?}");
    }
}

/// A frame that asks for the model's lease (a core bug, or a default-provider frame bound to a row): no lease
/// is taken and no lease header is sent to the provider.
#[test]
fn no_lease_leaves_the_server() {
    let r = row("https://qa-provider.example", false);
    let t = resolved(&r);
    let inner = Fake::new(vec![stream(reply_stream(), Tail::End)]);
    let rows = ScriptRows::steady(&r);
    let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
    assert_eq!(run_on(&host, &frame(&t.url, false, true)), Ended::Done);
    assert!(!inner.calls().iter().any(|c| c.starts_with("lease")));
    for req in inner.requests() {
        assert!(
            req.headers
                .iter()
                .all(|(k, _)| !k.eq_ignore_ascii_case("x-noevia-lease")),
            "{req:?}"
        );
    }
    // The plain host does take and send it: the frame really asked.
    let plain = Fake::new(vec![stream(reply_stream(), Tail::End)]);
    run_on(&plain, &frame(&t.url, false, true));
    assert!(plain.requests()[0]
        .headers
        .iter()
        .any(|(k, _)| k == "X-Noevia-Lease"));
}

fn unreachable_event(reason: &str) -> String {
    js::stringify(&js::obj([
        ("type", js::s("error")),
        ("text", js::s(target::unreachable_text(LABEL, reason))),
    ]))
}

/// A refused request is exactly a transport error with that message: the wrapped turn's events and end equal
/// the plain turn's whose engine threw `reason` at the same request (`before`: the answers before it).
fn same_as_thrown(inner: &Fake, ended: &Ended, f: &JValue, before: Vec<Scripted>, reason: &str) {
    let mut script = before;
    script.push(Scripted::Throws(reason.to_string()));
    let plain = Fake::new(script);
    let ended_plain = run_on(&plain, f);
    assert_eq!(ended, &ended_plain, "{reason}: how the turn ended");
    assert_eq!(inner.events(), plain.events(), "{reason}: the events");
    assert!(
        inner.events().contains(&unreachable_event(reason)),
        "{reason}: {:#?}",
        inner.events()
    );
}

/// Every re-check failure before the first round: nothing is sent, the turn ends on the transport error.
#[test]
fn recheck_failures_send_nothing() {
    let r = row("https://qa-provider.example", false);
    let t = resolved(&r);
    let gone = RowsNow {
        row: None,
        ..now_of(&r)
    };
    let other_id = RowsNow {
        row: Some(js_json::parse(r#"{"id":"qa-other","baseUrl":"https://qa-provider.example"}"#).unwrap()),
        ..now_of(&r)
    };
    let moved = RowsNow {
        row: Some(row("https://qa-elsewhere.example", false)),
        ..now_of(&r)
    };
    let shared = RowsNow {
        row: Some(row("https://qa-provider.example", true)),
        ..now_of(&r)
    };
    let user = RowsNow {
        user: "qa-admin".into(),
        ..now_of(&r)
    };
    let inactive = RowsNow {
        user_active: false,
        ..now_of(&r)
    };
    let origin = RowsNow {
        origin_approved: false,
        ..now_of(&r)
    };
    let not_own = RowsNow {
        member_own: false,
        origin_approved: false,
        ..now_of(&r)
    };
    for (name, state, reason) in [
        ("gone", gone, Some(phost::ROW_GONE)),
        ("other id", other_id, Some(phost::ROW_GONE)),
        ("moved", moved, Some(phost::ROW_CHANGED)),
        ("re-shared", shared, Some(phost::ROW_CHANGED)),
        ("user", user, Some(phost::USER_CHANGED)),
        ("inactive", inactive, Some(phost::USER_CHANGED)),
        ("origin", origin, Some(phost::ORIGIN_REFUSED)),
        // Not a member's own row any more (an admin's turn): the origin list does not apply.
        ("not own", not_own, None),
    ] {
        let script = vec![stream(reply_stream(), Tail::End)];
        let inner = Fake::new(script);
        let rows = ScriptRows::with(vec![state], vec![Some("qa-key-1".into())]);
        let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
        let f = frame(&t.url, false, false);
        let ended = run_on(&host, &f);
        match reason {
            Some(reason) => {
                assert!(inner.requests().is_empty(), "{name}: sent");
                same_as_thrown(&inner, &ended, &f, Vec::new(), reason);
                // The key is never unsealed for a request that is not sent.
                assert_eq!(rows.asked.lock().unwrap().1, 0, "{name}: key read");
            }
            None => assert_eq!(ended, Ended::Done, "{name}"),
        }
    }
}

/// The row changed during the tool round: the reply request is not sent.
#[test]
fn row_changed_between_rounds() {
    let r = row("https://qa-provider.example", false);
    let t = resolved(&r);
    let inner = Fake::new(vec![
        stream(tool_stream(), Tail::End),
        stream(reply_stream(), Tail::End),
    ]);
    let moved = RowsNow {
        row: Some(row("https://qa-elsewhere.example", false)),
        ..now_of(&r)
    };
    let rows = ScriptRows::with(vec![now_of(&r), moved], vec![Some("qa-key-1".into())]);
    let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
    let f = frame(&t.url, true, false);
    let ended = run_on(&host, &f);
    assert_eq!(inner.requests().len(), 1);
    assert!(inner.calls().contains(&"execute get_current_time".to_string()));
    same_as_thrown(
        &inner,
        &ended,
        &f,
        vec![stream(tool_stream(), Tail::End)],
        phost::ROW_CHANGED,
    );
}

#[test]
fn expired_binding_sends_nothing() {
    let r = row("https://qa-provider.example", false);
    let t = resolved(&r);
    for expires in [NOW, NOW - 1.0, f64::NAN] {
        let inner = Fake::new(vec![stream(reply_stream(), Tail::End)]);
        let rows = ScriptRows::steady(&r);
        let mut b = bound(&r, &t);
        b.expires_ms = expires;
        let host = ProviderHost::new(&inner, &rows, t.clone(), b, DEFAULT_ID).unwrap();
        let f = frame(&t.url, false, false);
        let ended = run_on(&host, &f);
        assert!(inner.requests().is_empty());
        same_as_thrown(&inner, &ended, &f, Vec::new(), phost::EXPIRED);
    }
}

/// A frame whose address or model is not the binding's: refused at the first request.
#[test]
fn wrong_address_or_model() {
    let r = row("https://qa-provider.example", false);
    let t = resolved(&r);
    let inner = Fake::new(vec![stream(reply_stream(), Tail::End)]);
    let rows = ScriptRows::steady(&r);
    let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
    let f = frame("http://qa-engine.invalid/v1/chat/completions", false, false);
    let ended = run_on(&host, &f);
    assert!(inner.requests().is_empty());
    same_as_thrown(&inner, &ended, &f, Vec::new(), phost::WRONG_ADDRESS);

    let inner = Fake::new(vec![stream(reply_stream(), Tail::End)]);
    let mut b = bound(&r, &t);
    b.model = "qa-other-model".into();
    let host = ProviderHost::new(&inner, &rows, t.clone(), b, DEFAULT_ID).unwrap();
    let f = frame(&t.url, false, false);
    let ended = run_on(&host, &f);
    assert!(inner.requests().is_empty());
    same_as_thrown(&inner, &ended, &f, Vec::new(), phost::WRONG_MODEL);

    // Bodies that are not an object, or have no text model, are refused by `outgoing` itself.
    let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
    for body in ["[]", "null", "\"qa-model\"", r#"{"model":7}"#, r#"{"messages":[]}"#, "{"] {
        let req = EngineRequest {
            url: t.url.clone(),
            headers: Vec::new(),
            body: body.into(),
            stream: true,
            timeout_ms: None,
        };
        assert_eq!(host.outgoing(&req), Err(phost::WRONG_MODEL), "{body}");
    }
}

/// undici refuses every redirect status under `redirect: 'error'` (`fetch failed`); other 3xx are answers.
#[test]
fn redirects_are_refused() {
    let r = row("https://qa-provider.example", false);
    let t = resolved(&r);
    for status in [301u16, 302, 303, 307, 308] {
        let script = vec![Scripted::Status(
            status,
            vec![("location".into(), "http://10.0.0.1/".into())],
            String::new(),
        )];
        let inner = Fake::new(script);
        let rows = ScriptRows::steady(&r);
        let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
        let f = frame(&t.url, false, false);
        let ended = run_on(&host, &f);
        assert_eq!(inner.requests().len(), 1);
        // Node's fetch rejects (`fetch failed`) where the wrapper refuses the answer: the same turn.
        same_as_thrown(&inner, &ended, &f, Vec::new(), phost::REDIRECT);
    }
    for status in [300u16, 304] {
        let script = vec![Scripted::Status(status, Vec::new(), "QA".into())];
        let plain = Fake::new(script.clone());
        run_on(&plain, &frame(&t.url, false, false));
        let inner = Fake::new(script);
        let rows = ScriptRows::steady(&r);
        let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
        run_on(&host, &frame(&t.url, false, false));
        assert_eq!(inner.events(), plain.events(), "{status}");
    }
}

/// The call site's deadline is kept; each site's headers are the same `providerHeaders`.
#[test]
fn deadlines_and_call_sites() {
    let r = row("https://qa-provider.example", false);
    let t = resolved(&r);
    let inner = Fake::new(Vec::new());
    let rows = ScriptRows::steady(&r);
    let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
    for (stream, timeout) in [
        (true, None),
        (false, Some(300_000)),
        (false, Some(180_000)),
        (false, Some(8_000)),
        (false, None),
    ] {
        let req = EngineRequest {
            url: t.url.clone(),
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("X-Noevia-Lease".into(), "qa-lease-9".into()),
                ("Authorization".into(), "Bearer qa-forged".into()),
            ],
            body: r#"{"model":"qa-model","stream":false}"#.into(),
            stream,
            timeout_ms: timeout,
        };
        let out = host.outgoing(&req).unwrap();
        assert_eq!(out.timeout_ms, timeout);
        assert_eq!(out.stream, stream);
        assert_eq!(out.body, req.body);
        assert_eq!(
            out.headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Authorization".to_string(), "Bearer qa-key-1".to_string()),
            ]
        );
        // The plan's Debug never prints a header value.
        assert!(!format!("{:?}", chat_upstream::plan::request(&t, chat_upstream::plan::Call::Round, Some("qa-key-1"))).contains("qa-key-1"));
    }
}

#[test]
fn only_direct_keyed_rows_are_wrapped() {
    let r = row("https://qa-provider.example", false);
    let t = resolved(&r);
    let b = bound(&r, &t);
    let inner = Fake::new(Vec::new());
    let rows = ScriptRows::steady(&r);
    let mk = |t: Target, b: Binding, d: &str| {
        ProviderHost::new(&inner, &rows, t, b, d).err()
    };
    assert_eq!(mk(t.clone(), b.clone(), DEFAULT_ID), None);
    assert_eq!(
        mk(t.clone(), b.clone(), "qa-provider"),
        Some(Unsupported::DefaultProvider)
    );
    let machine = Target {
        fetch: Fetch::Machine { id: "qa-machine".into() },
        key: KeyUse::None,
        ..t.clone()
    };
    assert_eq!(mk(machine, b.clone(), DEFAULT_ID), Some(Unsupported::Transport));
    let broker = Target {
        fetch: Fetch::Broker {
            owner: "qa-member".into(),
            conversation: Some("qa-chat-1".into()),
        },
        key: KeyUse::None,
        ..t.clone()
    };
    assert_eq!(mk(broker, b.clone(), DEFAULT_ID), Some(Unsupported::Transport));
    let keyless = Target {
        key: KeyUse::None,
        ..t.clone()
    };
    assert_eq!(mk(keyless, b.clone(), DEFAULT_ID), Some(Unsupported::Transport));
    let mut other = b.clone();
    other.provider_id = "qa-other".into();
    assert_eq!(mk(t.clone(), other, DEFAULT_ID), Some(Unsupported::NotBound));
    let mut moved = b;
    moved.url = "https://qa-elsewhere.example/v1/chat/completions".into();
    assert_eq!(mk(t, moved, DEFAULT_ID), Some(Unsupported::NotBound));
}

/// Splits `bytes` at the given positions (sorted, deduplicated, inside the text).
fn split(bytes: &[u8], cuts: &[usize]) -> Vec<Vec<u8>> {
    let mut at: Vec<usize> = cuts.iter().map(|c| c % (bytes.len() + 1)).collect();
    at.sort_unstable();
    at.dedup();
    let mut out = Vec::new();
    let mut from = 0;
    for c in at {
        out.push(bytes[from..c].to_vec());
        from = c;
    }
    out.push(bytes[from..].to_vec());
    out.retain(|c| !c.is_empty());
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// The reply's SSE bytes in any chunking (mid-line, mid-UTF-8, one byte at a time) give the events of the
    /// whole answer in one chunk, through the wrapper.
    #[test]
    fn sse_chunking_does_not_change_events(cuts in proptest::collection::vec(0usize..4096, 0..24), bytewise in any::<bool>()) {
        let r = row("https://qa-provider.example", false);
        let t = resolved(&r);
        let f = frame(&t.url, false, false);
        let whole = Fake::new(vec![stream(reply_stream(), Tail::End)]);
        run_on(&whole, &f);
        let bytes = reply_stream();
        let chunks = if bytewise { bytes.iter().map(|b| vec![*b]).collect() } else { split(&bytes, &cuts) };
        let inner = Fake::new(vec![Scripted::Stream(chunks, Tail::End)]);
        let rows = ScriptRows::steady(&r);
        let host = ProviderHost::new(&inner, &rows, t.clone(), bound(&r, &t), DEFAULT_ID).unwrap();
        prop_assert_eq!(run_on(&host, &f), Ended::Done);
        prop_assert_eq!(inner.events(), whole.events());
    }
}
