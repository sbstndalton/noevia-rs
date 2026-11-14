//! The native turn's [`Host`] for a turn bound to the sign-in row (p5-1 CONTINUE 1): it wraps the
//! front's own host and changes only what core's chat loop does differently for that row, so the
//! turn loop in `chat_turn::native` runs unchanged. The plain provider rows have chat-upstream's
//! own wrapper; this one differs only in the transport (the sign-in broker's fetch,
//! [`crate::signin_turn`], instead of a direct request with the row's key).
//!
//! Inert: nothing builds it yet. The caller (the native turn's plan for provider rows) may build
//! it only when core offered [`crate::signin_wire::CAPS_TOKEN`]; no core sends it yet.
//!
//! What it changes, each as core does it for the sign-in provider:
//! - every engine request (each round, its retries, the fallback, the summary and the title)
//!   goes through the sign-in fetch with only its address and body; no lease or row key is sent
//!   (the broker adds the account's own headers);
//! - before each request the turn's binding is checked: not past its expiry, the address and
//!   the model are the bound ones (the body must be a JSON object naming the bound model), and the
//!   caller's own check (the same active user) passes; a failure ends the request before
//!   anything is sent;
//! - no lease is taken, the local model manager is neither asked nor loaded, and no draft
//!   acceptance is recorded for the row's model (as for every non-default row);
//! - no idle limit on a stream (core applies it to the default provider and machines only).
//!
//! Everything else goes to the inner host as it is: the client's stream, the stores, the tool
//! bridges, usage, the prefill sample and the engine-busy signal.
//!
//! Named difference: whether the client left is read from the inner host every
//! [`LEFT_POLL`] while a request or read waits (the inner host gives a flag, not a signal).

use crate::signin_turn::{self, SigninEngine, TurnBody};
use crate::signin_wire::{Accounts, HttpBody};
use chat_turn::host::{
    Answer, EditTarget, EngineRequest, GateRefusal, Host, Ran, Read, Scrub, Skill,
    Target as ToolTarget, ToolRun,
};
use js_json::JValue;
use std::future::Future;
use std::time::Duration;

/// How often a waiting request or read looks at whether the client left.
pub const LEFT_POLL: Duration = Duration::from_millis(50);

/// The error a request ends with when the binding's time ran out (chat-upstream's text).
pub const EXPIRED: &str = "this reply ran past its time limit";
/// … when a request names another address than the bound one.
pub const WRONG_ADDRESS: &str = "the request named another address than this reply's provider";
/// … when a request names another model than the bound one, or its body is not a JSON object.
pub const WRONG_MODEL: &str = "the request named another model than this reply's";

/// What one turn on the sign-in row is bound to.
#[derive(Debug, Clone, PartialEq)]
pub struct SigninBinding {
    /// The address every request must name (the row's `/chat/completions`).
    pub url: String,
    /// The model every request's body must name.
    pub model: String,
    /// `Date.now()` past which no request is sent.
    pub expires_ms: f64,
}

/// The native turn's host for a sign-in row turn. `C` is the caller's re-check before each
/// request (the turn's user still active and still the sign-in's owner), with the error text.
pub struct SigninHost<'a, H, A, C> {
    inner: &'a H,
    engine: SigninEngine<A>,
    binding: SigninBinding,
    check: C,
}

impl<'a, H, A, C> SigninHost<'a, H, A, C>
where
    H: Host,
    A: Accounts + Clone,
    C: Fn() -> Result<(), &'static str> + Sync,
{
    pub fn new(inner: &'a H, engine: SigninEngine<A>, binding: SigninBinding, check: C) -> Self {
        SigninHost {
            inner,
            engine,
            binding,
            check,
        }
    }

    /// The binding's checks for one request, in order: expiry, the caller's check, the address,
    /// the model.
    pub fn admit(&self, request: &EngineRequest) -> Result<(), &'static str> {
        // Not before the expiry (NaN included) is expired.
        if self.inner.now_ms().partial_cmp(&self.binding.expires_ms)
            != Some(std::cmp::Ordering::Less)
        {
            return Err(EXPIRED);
        }
        (self.check)()?;
        if request.url != self.binding.url {
            return Err(WRONG_ADDRESS);
        }
        let model = js_json::wtf16::parse(&request.body)
            .ok()
            .and_then(|v| match &v {
                JValue::Obj(_) => v.get("model").as_str().map(str::to_string),
                _ => None,
            });
        if model.as_deref() != Some(self.binding.model.as_str()) {
            return Err(WRONG_MODEL);
        }
        Ok(())
    }

    /// Resolves once the inner host says the client left.
    fn left_signal(&self) -> impl Future<Output = ()> + Send + '_ {
        async move {
            while !self.inner.left() {
                tokio::time::sleep(LEFT_POLL).await;
            }
        }
    }
}

impl<H, A, C> Host for SigninHost<'_, H, A, C>
where
    H: Host,
    A: Accounts + Clone,
    C: Fn() -> Result<(), &'static str> + Sync,
{
    type Body = TurnBody<HttpBody>;

    fn emit(&self, event: &JValue) {
        self.inner.emit(event);
    }
    fn left(&self) -> bool {
        self.inner.left()
    }
    fn now_ms(&self) -> f64 {
        self.inner.now_ms()
    }

    /// No lease for a provider row.
    fn lease(
        &self,
        _model: &str,
    ) -> impl Future<Output = Result<Option<Vec<(String, String)>>, GateRefusal>> + Send {
        std::future::ready(Ok(None))
    }
    fn release_lease(&self) {
        self.inner.release_lease();
    }
    /// The local model manager is not this row's: no health.
    fn health(&self) -> impl Future<Output = Option<JValue>> + Send {
        std::future::ready(None)
    }
    /// … and no load.
    fn load(&self, _model: &str) -> impl Future<Output = bool> + Send {
        std::future::ready(false)
    }
    fn template_allows_tools(&self, model: &str) -> impl Future<Output = bool> + Send {
        self.inner.template_allows_tools(model)
    }

    async fn request(&self, request: &EngineRequest) -> Result<Answer<Self::Body>, String> {
        self.admit(request).map_err(str::to_string)?;
        if self.inner.left() {
            return Err(signin_turn::ABORTED.to_owned());
        }
        self.engine.request(request, self.left_signal()).await
    }
    /// No idle limit for this row's stream.
    async fn read(&self, body: &mut Self::Body, _idle_ms: Option<u64>) -> Read {
        signin_turn::read(body, self.left_signal()).await
    }
    async fn text(&self, body: Self::Body, cap: usize) -> (String, bool) {
        signin_turn::text(body, cap, self.left_signal()).await
    }

    fn read_state(&self, id: &str) -> JValue {
        self.inner.read_state(id)
    }
    fn save_state(&self, id: &str, state: &JValue) -> Result<(), String> {
        self.inner.save_state(id, state)
    }
    fn mark_tools_unsupported(&self, model: &str) {
        self.inner.mark_tools_unsupported(model);
    }
    fn engine_busy(&self) -> impl Future<Output = bool> + Send {
        self.inner.engine_busy()
    }
    fn title_auto_named(
        &self,
        list: Option<&str>,
        chat: &str,
        first_message: &str,
    ) -> impl Future<Output = bool> + Send {
        self.inner.title_auto_named(list, chat, first_message)
    }
    fn title_save(&self, list: Option<&str>, chat: &str, title: &str, first_message: &str) {
        self.inner.title_save(list, chat, title, first_message);
    }
    fn record_usage(&self, model: &str, reported: &JValue) {
        self.inner.record_usage(model, reported);
    }
    fn user_id(&self) -> String {
        self.inner.user_id()
    }
    fn uuid(&self) -> String {
        self.inner.uuid()
    }
    fn tool_policy(&self, name: &str, write: bool) -> String {
        self.inner.tool_policy(name, write)
    }
    fn write_target(
        &self,
        name: &str,
        args: &str,
        chat_key: Option<&str>,
    ) -> impl Future<Output = ToolTarget> + Send {
        self.inner.write_target(name, args, chat_key)
    }
    fn edit_target(
        &self,
        project_id: Option<&str>,
        name: &str,
        args: &str,
    ) -> impl Future<Output = EditTarget> + Send {
        self.inner.edit_target(project_id, name, args)
    }
    fn permission_binding(
        &self,
        chat_id: Option<&str>,
        scope: &str,
        holder: &JValue,
    ) -> impl Future<Output = Result<JValue, ()>> + Send {
        self.inner.permission_binding(chat_id, scope, holder)
    }
    fn chat_wide_approved(
        &self,
        chat_id: Option<&str>,
        scope: &str,
        binding: &JValue,
        exchange_key: &str,
    ) -> impl Future<Output = Option<String>> + Send {
        self.inner
            .chat_wide_approved(chat_id, scope, binding, exchange_key)
    }
    fn open_approval(&self, request: &JValue) -> impl Future<Output = bool> + Send {
        self.inner.open_approval(request)
    }
    fn await_approval(&self, request: &JValue) -> impl Future<Output = String> + Send {
        self.inner.await_approval(request)
    }
    fn revoke_grant(&self, chat_id: Option<&str>) -> impl Future<Output = ()> + Send {
        self.inner.revoke_grant(chat_id)
    }
    fn audit(&self, event: &str, details: &JValue) {
        self.inner.audit(event, details);
    }
    fn execute_tool(&self, run: &ToolRun) -> impl Future<Output = Ran> + Send {
        self.inner.execute_tool(run)
    }
    fn skills_revoked(
        &self,
        project_id: &str,
        loaded: &[Skill],
    ) -> impl Future<Output = Option<Vec<(String, String)>>> + Send {
        self.inner.skills_revoked(project_id, loaded)
    }
    fn skills_read(
        &self,
        project_id: &str,
        chat_id: Option<&str>,
        candidates: &[Skill],
        args: &str,
        result: &str,
    ) -> impl Future<Output = Option<Vec<Skill>>> + Send {
        self.inner
            .skills_read(project_id, chat_id, candidates, args, result)
    }
    fn skills_scrub(
        &self,
        project_id: &str,
        chat_id: Option<&str>,
        messages: &[JValue],
    ) -> impl Future<Output = Option<Scrub>> + Send {
        self.inner.skills_scrub(project_id, chat_id, messages)
    }
    fn prefill_sample(&self, model: &str, prompt: f64, first_token_ms: f64) {
        self.inner.prefill_sample(model, prompt, first_token_ms);
    }
    /// Draft acceptance is the local model manager's evidence: not recorded for this row.
    fn mtp_evidence(&self, _model: &str, _drafted: f64, _accepted: f64) {}
    fn gate_record(&self, event: &str, tool: &str) {
        self.inner.gate_record(event, tool);
    }
    fn supervise(
        &self,
        round: i32,
        outputs: &[JValue],
        goal: &str,
        wrote: bool,
    ) -> impl Future<Output = String> + Send {
        self.inner.supervise(round, outputs, goal, wrote)
    }
    fn condense(
        &self,
        kind: &str,
        message: &str,
        tool: &str,
        result_text: &str,
        model: &str,
    ) -> impl Future<Output = Option<(String, JValue)>> + Send {
        self.inner.condense(kind, message, tool, result_text, model)
    }
    fn reasoner_trace(
        &self,
        trace: &JValue,
        answer_chars: f64,
        answer_ms: f64,
    ) -> impl Future<Output = ()> + Send {
        self.inner.reasoner_trace(trace, answer_chars, answer_ms)
    }
}

#[cfg(test)]
#[path = "signin_host_tests.rs"]
pub(crate) mod tests;
