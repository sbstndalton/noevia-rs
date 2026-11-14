//! The native turn's [`Host`] for a turn bound to a provider row (council p3, p1-1 CONTINUE step 2): it wraps
//! the front's own host and changes only what chat.cjs does differently for a non-default provider row, so the
//! turn loop in `chat_turn::native` runs unchanged.
//!
//! What it changes, each as Node does it (chat.cjs, lines as of the oracle core):
//! - every engine request (each round, its retries, the fallback, the context summary and the title) goes to
//!   the bound address only, with `providerHeaders(provider)` rebuilt from the row's key as it is NOW
//!   (`upstreamHeaders`, ~1405; [`crate::plan::request`]), never with the local engine's lease headers: a lease
//!   token is the local gate's and must not leave the server;
//! - before each request the binding is re-checked ([`crate::binding::recheck`]: same active user, the row
//!   still there and unchanged, the member origin still approved) and its expiry read; a failure ends the
//!   request before anything is sent (fail closed; Node keeps the captured row, a named difference);
//! - the body's `model` must be the bound model (a body naming another model is refused, not sent);
//! - a redirect answer (301/302/303/307/308) is the error `fetch failed`, as undici's `redirect: 'error'`
//!   rejects; the front's client must not follow redirects itself (see [`ProviderHost`]);
//! - no lease is taken (`provider.id === DEFAULT_PROVIDER_ID` gates `acquireLease`, ~1883), the local model
//!   manager is not asked (`manager: provider.id===DEFAULT_PROVIDER_ID ? modelManager : null`), and no draft
//!   acceptance evidence is recorded for the row's model (~2003).
//!
//! What it leaves to the inner host as Node does for every provider: the client's stream, the stores, the
//! tool bridges, usage (`recordUsage` is not provider-gated, ~2002), the prefill sample (~2008) and the
//! engine-busy signal (the title still waits for the local engine, `provider.machine || !engineBusy()`).
//!
//! Only [`Fetch::Direct`] targets that send the row's key are taken here; an Inference machine and the
//! sign-in broker carry their own transport and stay refused ([`crate::refusal`]).

use std::future::Future;

use chat_turn::host::{
    Answer, EditTarget, EngineRequest, GateRefusal, Host, Ran, Read, Scrub, Skill, Target as ToolTarget,
    ToolRun,
};
use js_json::JValue;

use crate::binding::{recheck, Binding, Now, Recheck};
use crate::plan::{self, Call};
use crate::target::{Fetch, KeyUse, Target};

/// The error a request ends with when the user is no longer the binding's or no longer active.
pub const USER_CHANGED: &str = "the signed-in account changed during this reply";
/// … when the row was deleted or is no longer the user's to use.
pub const ROW_GONE: &str = "the provider was removed during this reply";
/// … when the row's address or sharing changed.
pub const ROW_CHANGED: &str = "the provider settings changed during this reply";
/// … when a member's own row's origin is no longer approved.
pub const ORIGIN_REFUSED: &str = "the provider address is no longer approved for members";
/// … when the binding's time ran out.
pub const EXPIRED: &str = "this reply ran past its time limit";
/// … when a request names another address than the bound one.
pub const WRONG_ADDRESS: &str = "the request named another address than this reply's provider";
/// … when a request names another model than the bound one, or its body is not a JSON object.
pub const WRONG_MODEL: &str = "the request named another model than this reply's";
/// undici's message for a refused redirect (`redirect: 'error'`; the cause is not part of `err.message`).
pub const REDIRECT: &str = "fetch failed";

/// The redirect statuses fetch acts on.
const REDIRECTS: [u16; 5] = [301, 302, 303, 307, 308];

/// What the wrapper reads of the account as of one request. Implemented by the front over its provider
/// store (provider_files.rs) and the account store; the tests implement it from a script.
pub trait Rows: Sync {
    /// The facts the re-check reads, as of now.
    fn now(&self, binding: &Binding) -> RowsNow;
    /// The bound row's key as unsealed now (`None`: no key, or it could not be unsealed). Called once per
    /// request after the re-check passed; the value goes into one request's headers and nowhere else.
    fn key(&self, provider_id: &str) -> Option<String>;
}

/// [`Now`], owned.
#[derive(Debug, Clone, PartialEq)]
pub struct RowsNow {
    pub user: String,
    pub user_active: bool,
    /// The row with the bound id, looked up as this user (exactly, not the default fallback).
    pub row: Option<JValue>,
    pub member_own: bool,
    pub origin_approved: bool,
}

/// Why a target cannot be wrapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// The default provider: the turn runs on the inner host as it is.
    DefaultProvider,
    /// An Inference machine or the sign-in broker: their own transport (later slices).
    Transport,
    /// The binding is not the target's (provider id or address differ).
    NotBound,
}

/// The native turn's host for a provider-row turn. `H` is the front's host; its client must refuse redirects
/// (`reqwest::redirect::Policy::none()`), so that a 3xx comes back here as an answer and is refused as Node
/// refuses it, instead of being followed to an address nobody approved.
pub struct ProviderHost<'a, H, R> {
    inner: &'a H,
    rows: &'a R,
    target: Target,
    binding: Binding,
}

impl<'a, H: Host, R: Rows> ProviderHost<'a, H, R> {
    /// The wrapper for `target` (resolved by [`crate::target::resolve`] at the cut) bound by `binding`.
    pub fn new(
        inner: &'a H,
        rows: &'a R,
        target: Target,
        binding: Binding,
        default_id: &str,
    ) -> Result<Self, Unsupported> {
        if target.provider_id == default_id {
            return Err(Unsupported::DefaultProvider);
        }
        if target.fetch != Fetch::Direct || target.key != KeyUse::RowKey {
            return Err(Unsupported::Transport);
        }
        if binding.provider_id != target.provider_id || binding.url != target.url {
            return Err(Unsupported::NotBound);
        }
        Ok(ProviderHost {
            inner,
            rows,
            target,
            binding,
        })
    }

    /// The request as it goes out, or the error it ends with before anything is sent.
    pub fn outgoing(&self, request: &EngineRequest) -> Result<EngineRequest, &'static str> {
        // Not before the expiry (NaN included) is expired, as `binding::open` reads it.
        if self.inner.now_ms().partial_cmp(&self.binding.expires_ms) != Some(std::cmp::Ordering::Less) {
            return Err(EXPIRED);
        }
        let now = self.rows.now(&self.binding);
        let checked = recheck(
            &self.binding,
            &Now {
                user: &now.user,
                user_active: now.user_active,
                row: now.row.as_ref(),
                member_own: now.member_own,
                origin_approved: now.origin_approved,
            },
        );
        match checked {
            Recheck::Ok => {}
            Recheck::UserChanged => return Err(USER_CHANGED),
            Recheck::RowGone => return Err(ROW_GONE),
            Recheck::RowChanged => return Err(ROW_CHANGED),
            Recheck::OriginRefused => return Err(ORIGIN_REFUSED),
        }
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
        let call = call_of(request);
        let key = self.rows.key(&self.target.provider_id);
        let planned = plan::request(&self.target, call, key.as_deref());
        Ok(EngineRequest {
            url: planned.url,
            headers: planned.headers,
            body: request.body.clone(),
            stream: request.stream,
            // The executor's own deadline for this call site (the title's 8 s included); the plan's is the
            // same for the round, the fallback and the summary.
            timeout_ms: request.timeout_ms,
        })
    }
}

/// Which call site a request is, from what the executor set: a stream is a round, 300 s the fallback,
/// anything else with a deadline the summary's rules (the title is built the same way; its deadline is kept).
fn call_of(request: &EngineRequest) -> Call {
    match (request.stream, request.timeout_ms) {
        (true, _) | (false, None) => Call::Round,
        (false, Some(300_000)) => Call::Fallback,
        (false, Some(_)) => Call::Summary,
    }
}

impl<H: Host, R: Rows> Host for ProviderHost<'_, H, R> {
    type Body = H::Body;

    fn emit(&self, event: &JValue) {
        self.inner.emit(event);
    }
    fn left(&self) -> bool {
        self.inner.left()
    }
    fn now_ms(&self) -> f64 {
        self.inner.now_ms()
    }

    /// No lease for a provider row (chat.cjs takes one only for the default provider).
    fn lease(
        &self,
        _model: &str,
    ) -> impl Future<Output = Result<Option<Vec<(String, String)>>, GateRefusal>> + Send {
        std::future::ready(Ok(None))
    }
    fn release_lease(&self) {
        self.inner.release_lease();
    }
    /// The local model manager is not this row's (`manager: null`): no health.
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
        let outgoing = self.outgoing(request).map_err(str::to_string)?;
        let answer = self.inner.request(&outgoing).await?;
        if REDIRECTS.contains(&answer.status) {
            // Dropping the body cancels it.
            return Err(REDIRECT.to_string());
        }
        Ok(answer)
    }
    fn read(
        &self,
        body: &mut Self::Body,
        idle_ms: Option<u64>,
    ) -> impl Future<Output = Read> + Send {
        self.inner.read(body, idle_ms)
    }
    fn text(&self, body: Self::Body, cap: usize) -> impl Future<Output = (String, bool)> + Send {
        self.inner.text(body, cap)
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
    /// Draft acceptance is the local model manager's evidence: not recorded for a provider row.
    fn mtp_evidence(&self, _model: &str, _drafted: f64, _accepted: f64) {}
}
