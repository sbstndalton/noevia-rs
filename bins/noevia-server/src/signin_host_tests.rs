//! The sign-in row's host over a recording inner host (no network): what it answers itself
//! (no lease, no manager, no draft evidence), what it hands to the inner host, and the binding's
//! checks before any request. Over real HTTP: signin_wire_tests.rs (a whole request and stream
//! through this host against a loopback backend, and the client leaving mid-read).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use super::*;
use crate::chatgpt_oauth_routes::{transport, Settings};
use signin_transport::session::BrokerAnswer;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// An inner host that records what reaches it and answers with neutral values.
pub(crate) struct FakeInner {
    pub(crate) left: AtomicBool,
    pub(crate) now: f64,
    pub(crate) calls: Mutex<Vec<String>>,
}

impl FakeInner {
    pub(crate) fn new(now: f64) -> FakeInner {
        FakeInner {
            left: AtomicBool::new(false),
            now,
            calls: Mutex::new(Vec::new()),
        }
    }
    fn log(&self, what: &str) {
        self.calls.lock().unwrap().push(what.to_owned());
    }
    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Host for FakeInner {
    type Body = ();

    fn emit(&self, _event: &JValue) {
        self.log("emit");
    }
    fn left(&self) -> bool {
        self.left.load(Ordering::SeqCst)
    }
    fn now_ms(&self) -> f64 {
        self.now
    }
    async fn lease(&self, _model: &str) -> Result<Option<Vec<(String, String)>>, GateRefusal> {
        self.log("lease");
        Ok(Some(vec![("x-noevia-lease".into(), "qa-lease".into())]))
    }
    fn release_lease(&self) {
        self.log("release_lease");
    }
    async fn health(&self) -> Option<JValue> {
        self.log("health");
        Some(JValue::obj([("ok", JValue::Bool(true))]))
    }
    async fn load(&self, _model: &str) -> bool {
        self.log("load");
        true
    }
    async fn template_allows_tools(&self, _model: &str) -> bool {
        self.log("template_allows_tools");
        true
    }
    async fn request(&self, _request: &EngineRequest) -> Result<Answer<()>, String> {
        self.log("request");
        Err("the inner host's engine".into())
    }
    async fn read(&self, _body: &mut (), _idle_ms: Option<u64>) -> Read {
        self.log("read");
        Read::End
    }
    async fn text(&self, _body: (), _cap: usize) -> (String, bool) {
        self.log("text");
        (String::new(), false)
    }
    fn read_state(&self, _id: &str) -> JValue {
        self.log("read_state");
        JValue::Obj(Vec::new())
    }
    fn save_state(&self, _id: &str, _state: &JValue) -> Result<(), String> {
        self.log("save_state");
        Ok(())
    }
    fn mark_tools_unsupported(&self, _model: &str) {
        self.log("mark_tools_unsupported");
    }
    async fn engine_busy(&self) -> bool {
        self.log("engine_busy");
        false
    }
    async fn title_auto_named(&self, _list: Option<&str>, _chat: &str, _first: &str) -> bool {
        self.log("title_auto_named");
        false
    }
    fn title_save(&self, _list: Option<&str>, _chat: &str, _title: &str, _first: &str) {
        self.log("title_save");
    }
    fn record_usage(&self, _model: &str, _reported: &JValue) {
        self.log("record_usage");
    }
    fn user_id(&self) -> String {
        "qa-user".into()
    }
    fn uuid(&self) -> String {
        "00000000-0000-4000-8000-000000000000".into()
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
        _chat_id: Option<&str>,
        _scope: &str,
        _holder: &JValue,
    ) -> Result<JValue, ()> {
        Err(())
    }
    async fn chat_wide_approved(
        &self,
        _chat_id: Option<&str>,
        _scope: &str,
        _binding: &JValue,
        _exchange_key: &str,
    ) -> Option<String> {
        None
    }
    async fn open_approval(&self, _request: &JValue) -> bool {
        false
    }
    async fn await_approval(&self, _request: &JValue) -> String {
        "deny".into()
    }
    async fn revoke_grant(&self, _chat_id: Option<&str>) {}
    fn audit(&self, _event: &str, _details: &JValue) {
        self.log("audit");
    }
    async fn execute_tool(&self, _run: &ToolRun) -> Ran {
        Ran::Unavailable
    }
    async fn skills_revoked(&self, _p: &str, _loaded: &[Skill]) -> Option<Vec<(String, String)>> {
        None
    }
    async fn skills_read(
        &self,
        _p: &str,
        _c: Option<&str>,
        _candidates: &[Skill],
        _args: &str,
        _result: &str,
    ) -> Option<Vec<Skill>> {
        None
    }
    async fn skills_scrub(&self, _p: &str, _c: Option<&str>, _m: &[JValue]) -> Option<Scrub> {
        None
    }
    fn prefill_sample(&self, _model: &str, _prompt: f64, _first_token_ms: f64) {
        self.log("prefill_sample");
    }
    fn mtp_evidence(&self, _model: &str, _drafted: f64, _accepted: f64) {
        self.log("mtp_evidence");
    }
    fn gate_record(&self, _event: &str, _tool: &str) {
        self.log("gate_record");
    }
    async fn supervise(&self, _r: i32, _o: &[JValue], _g: &str, _w: bool) -> String {
        String::new()
    }
    async fn condense(
        &self,
        _kind: &str,
        _message: &str,
        _tool: &str,
        _result_text: &str,
        _model: &str,
    ) -> Option<(String, JValue)> {
        None
    }
    async fn reasoner_trace(&self, _trace: &JValue, _chars: f64, _ms: f64) {}
}

/// A broker that is never reached in these tests (each refusal comes before the fetch).
#[derive(Clone, Default)]
struct NoBroker {
    asked: Arc<AtomicBool>,
}

impl Accounts for NoBroker {
    fn signal(&self) -> Option<String> {
        None
    }
    async fn session(&self, _user: &str, _rejected: Option<&str>) -> BrokerAnswer {
        self.asked.store(true, Ordering::SeqCst);
        BrokerAnswer::Unreachable
    }
    async fn reject(&self, _user: &str, _generation: &str) {
        self.asked.store(true, Ordering::SeqCst);
    }
    async fn catalogue(&self, _user: &str) -> Option<JValue> {
        self.asked.store(true, Ordering::SeqCst);
        None
    }
}

pub(crate) const URL: &str = "http://qa-signin.invalid/v1/chat/completions";
pub(crate) const MODEL: &str = "m-large";

fn engine(accounts: NoBroker) -> SigninEngine<NoBroker> {
    let settings = Settings::from_lookup(&|_: &str| None, None);
    let mut t = transport(&settings);
    // Never reached: every request below is refused before the fetch.
    t.base_url = "http://127.0.0.1:9/api".into();
    SigninEngine {
        accounts,
        client: crate::signin_wire::client().unwrap(),
        transport: t,
        user: "qa-user".into(),
        conversation: Some("chat-qa".into()),
    }
}

pub(crate) fn binding(expires_ms: f64) -> SigninBinding {
    SigninBinding {
        url: URL.into(),
        model: MODEL.into(),
        expires_ms,
    }
}

fn request(url: &str, body: &str) -> EngineRequest {
    EngineRequest {
        url: url.into(),
        headers: Vec::new(),
        body: body.into(),
        stream: true,
        timeout_ms: None,
    }
}

const BODY: &str =
    r#"{"model":"m-large","messages":[{"role":"user","content":"q"}],"stream":true}"#;

fn ok() -> Result<(), &'static str> {
    Ok(())
}

#[tokio::test]
async fn no_lease_no_manager_no_draft_evidence() {
    let inner = FakeInner::new(1_000.0);
    let host = SigninHost::new(&inner, engine(NoBroker::default()), binding(2_000.0), ok);
    assert_eq!(host.lease(MODEL).await, Ok(None));
    assert_eq!(host.health().await, None);
    assert!(!host.load(MODEL).await);
    host.mtp_evidence(MODEL, 4.0, 3.0);
    assert!(inner.calls().is_empty(), "{:?}", inner.calls());
}

#[tokio::test]
async fn the_rest_goes_to_the_inner_host() {
    let inner = FakeInner::new(1_000.0);
    let host = SigninHost::new(&inner, engine(NoBroker::default()), binding(2_000.0), ok);
    host.emit(&JValue::Null);
    host.release_lease();
    assert!(host.template_allows_tools(MODEL).await);
    let _ = host.read_state("qa");
    host.save_state("qa", &JValue::Null).unwrap();
    host.mark_tools_unsupported(MODEL);
    assert!(!host.engine_busy().await);
    assert!(!host.title_auto_named(None, "chat-qa", "q").await);
    host.title_save(None, "chat-qa", "t", "q");
    host.record_usage(MODEL, &JValue::Null);
    host.audit("qa", &JValue::Null);
    host.prefill_sample(MODEL, 10.0, 5.0);
    host.gate_record("qa", "tool");
    assert_eq!(host.user_id(), "qa-user");
    assert_eq!(host.now_ms(), 1_000.0);
    assert_eq!(
        inner.calls(),
        [
            "emit",
            "release_lease",
            "template_allows_tools",
            "read_state",
            "save_state",
            "mark_tools_unsupported",
            "engine_busy",
            "title_auto_named",
            "title_save",
            "record_usage",
            "audit",
            "prefill_sample",
            "gate_record",
        ]
    );
}

async fn refused(
    host: &SigninHost<'_, FakeInner, NoBroker, impl Fn() -> Result<(), &'static str> + Sync>,
    r: &EngineRequest,
) -> String {
    match host.request(r).await {
        Err(e) => e,
        Ok(_) => panic!("the request was sent"),
    }
}

#[tokio::test]
async fn the_binding_is_checked_before_anything_is_sent() {
    let broker = NoBroker::default();
    let inner = FakeInner::new(1_000.0);
    let good = request(URL, BODY);

    // At or past the expiry, and a NaN expiry.
    for expires in [1_000.0, 999.0, f64::NAN] {
        let host = SigninHost::new(&inner, engine(broker.clone()), binding(expires), ok);
        assert_eq!(refused(&host, &good).await, EXPIRED);
    }
    let host = SigninHost::new(&inner, engine(broker.clone()), binding(2_000.0), ok);
    // Another address, even one that differs only in its path.
    assert_eq!(
        refused(
            &host,
            &request("http://qa-other.invalid/v1/chat/completions", BODY)
        )
        .await,
        WRONG_ADDRESS
    );
    assert_eq!(
        refused(
            &host,
            &request("http://qa-signin.invalid/v1/completions", BODY)
        )
        .await,
        WRONG_ADDRESS
    );
    // Another model, no model, a body that is not an object, a body that does not parse.
    for body in [
        r#"{"model":"m-small","messages":[]}"#,
        r#"{"messages":[]}"#,
        r#"["m-large"]"#,
        "not json",
        r#"{"model":7}"#,
    ] {
        assert_eq!(
            refused(&host, &request(URL, body)).await,
            WRONG_MODEL,
            "{body}"
        );
    }
    // The caller's own check, with its text, before the address is looked at.
    let host = SigninHost::new(&inner, engine(broker.clone()), binding(2_000.0), || {
        Err("the signed-in account changed during this reply")
    });
    assert_eq!(
        refused(&host, &request("http://qa-other.invalid/x", BODY)).await,
        "the signed-in account changed during this reply"
    );
    assert!(!broker.asked.load(Ordering::SeqCst), "the broker was asked");
    assert!(
        !inner.calls().iter().any(|c| c == "request"),
        "the inner host's engine was used"
    );
}

#[tokio::test]
async fn a_client_that_already_left_sends_nothing() {
    let broker = NoBroker::default();
    let inner = FakeInner::new(1_000.0);
    inner.left.store(true, Ordering::SeqCst);
    let host = SigninHost::new(&inner, engine(broker.clone()), binding(2_000.0), ok);
    assert_eq!(
        refused(&host, &request(URL, BODY)).await,
        crate::signin_turn::ABORTED
    );
    assert!(!broker.asked.load(Ordering::SeqCst), "the broker was asked");
}

#[test]
fn the_binding_debug_has_no_secret_to_hide() {
    // The binding holds an address and a model only; the key never enters this host.
    let d = format!("{:?}", binding(1.0));
    assert!(d.contains(MODEL), "{d}");
}
