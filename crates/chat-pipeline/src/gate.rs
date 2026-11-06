//! One tool call of a round, from the exchange's checks to the result the model reads (core
//! chat.cjs:2280-2560 with tool-exchange.cjs): the per-turn result cache, the permission gate
//! (step 16: an earlier decline, oversized arguments, the external-provider rules, the account's
//! policy, the write target shown on the card, the provenance check), the approval card at the
//! front's registry (S9), the chat-wide grant redeemed at the front and its dispatch fence, the
//! re-checks after every await, the executor call, and the `tool_result` event.
//!
//! Every await is the caller's [`GateIo`]; [`run_call`] holds nothing across them that needs
//! releasing except the fence, whose `started()` is called on every path after it was redeemed
//! (also when the future is dropped: [`FenceGuard`]).
use crate::text::frame;
use js_json::{wtf16, JValue};
use std::collections::HashSet;

/// The account's tool policy for a call (`toolPolicy.mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Allow,
    Ask,
    Block,
}

/// The person's answer to the card (`awaitApproval`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
    Timeout,
    Aborted,
    Unavailable,
    /// Anything else: a decline (audited with its own word) that does not end the reply.
    Other(String),
}

impl Decision {
    pub fn from_js(s: &str) -> Decision {
        match s {
            "approve" => Decision::Approve,
            "deny" => Decision::Deny,
            "timeout" => Decision::Timeout,
            "aborted" => Decision::Aborted,
            "unavailable" => Decision::Unavailable,
            other => Decision::Other(other.to_owned()),
        }
    }
}

/// A project file edit's resolved target (`editTargets.resolveEditTarget`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditTarget {
    pub path: Option<String>,
    pub account: Option<String>,
    pub error: Option<String>,
}

/// Another write's target (`writeTargetFor`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteTarget {
    pub target: Option<String>,
    pub kind: Option<String>,
    pub error: Option<String>,
}

/// The front's redeemed grant (`redeemChatGrant`).
pub enum Redeemed<F> {
    Unavailable,
    Fence(F),
    Invalid,
}

/// The dispatch fence of a redeemed grant: `open()` before the call, `started()` once it went out
/// (or will not).
pub trait Fence {
    fn open(&self) -> bool;
    fn started(&self);
}

/// What the executor answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Executed {
    pub text: String,
    /// `outcome.notRun`: the executor could not run it (unreachable: fail closed).
    pub not_run: bool,
    /// `outcome.failed`.
    pub failed: bool,
}

/// One call as the round prepared it ([`crate::stream::Round::prepare_calls`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// `toolOffset + toolIndex`.
    pub index: usize,
    pub id: String,
    pub name: String,
    pub args: String,
    pub too_large: bool,
}

/// The gate's collaborators and awaits.
#[allow(async_fn_in_trait)]
pub trait GateIo {
    type Binding;
    type Fence: Fence;
    /// `requestScope` workspace user.
    fn user(&self) -> Option<String>;
    /// The chat-wide grant's scope (`writeGrantScope`) and the chat id.
    fn chat_id(&self) -> Option<String>;
    /// `revokedSkills()` then `refuseForRevokedSkill`: the refusal, with its audit.
    fn revoked_skill_refusal(&self, name: &str) -> Option<String>;
    fn is_write(&self, name: &str) -> bool;
    fn mode(&self, name: &str, write: bool) -> Mode;
    /// `egressToolRefusal(user, name, args)`.
    fn egress_refusal(&self, name: &str, args: &str) -> Option<String>;
    /// `projectEditTool(name)` and its target.
    fn edit_tool(&self, name: &str) -> bool;
    fn resolve_edit(&self, args: &str) -> EditTarget;
    /// `writeTargetFor` is wired; its answer (`Err`: it threw).
    fn has_write_targets(&self) -> bool;
    async fn write_target(&self, name: &str, args: &str) -> Result<Option<WriteTarget>, ()>;
    /// #769: the provenance hits of a write (`None`: taint is off; `Err`: it could not be checked).
    fn provenance(&self, args: &str) -> Option<Result<Vec<JValue>, ()>>;
    /// `automaticOnlyWrites.has(name)` (#1550).
    fn automatic_only(&self, name: &str) -> bool;
    /// `pendingStoreWrites`, `storeFailed` and `permissionContext()`; `Ok(None)` without a chat id.
    async fn permission_context(&self) -> Result<Option<Self::Binding>, ()>;
    fn binding_matches(&self, a: &Option<Self::Binding>, b: &Option<Self::Binding>) -> bool;
    /// `chatWideApproved(user, chat, scope, binding)`.
    async fn chat_wide_approved(&self, binding: &Option<Self::Binding>) -> bool;
    /// #658: the same write already succeeded in this chat.
    fn repeat(&self, fingerprint: &str) -> bool;
    /// The MCP server the call goes to (`toolRoutes.get(name)`).
    fn route(&self, name: &str) -> Option<String>;
    fn new_approval_id(&self) -> String;
    async fn open_approval(&self, id: &str, binding: &Option<Self::Binding>) -> bool;
    async fn await_approval(&self, id: &str, binding: &Option<Self::Binding>) -> Decision;
    fn aborted(&self) -> bool;
    /// `currentWorkspace().userId === user && !revoked`.
    fn workspace_ok(&self, user: &Option<String>) -> bool;
    async fn revoke_grant(&self);
    fn has_redeem(&self) -> bool;
    async fn redeem(&self, binding: &Option<Self::Binding>) -> Redeemed<Self::Fence>;
    /// Whether [`GateIo::execute`] re-checks the grant `fence` at its own send boundary, after every
    /// preparation await (server lookup, credentials, address checks, MCP initialization, a retry),
    /// and answers `not_run` without sending when it closed (Node S9, chat.cjs:2456-2507). The gate
    /// checks the fence before it calls the executor, but a grant can close while the executor
    /// prepares; an executor that cannot carry the fence that far is never handed a fenced call
    /// (review P2.14-17 P1: fail closed).
    fn executor_fences_dispatch(&self) -> bool;
    /// The executor (the native MCP executor, mcp_exec.rs); `Err(message)`: it threw.
    async fn execute(
        &self,
        name: &str,
        args: &str,
        fence: Option<&Self::Fence>,
        edit_target: Option<&(String, Option<String>)>,
    ) -> Result<Executed, String>;
    fn audit(&self, event: &str, tool: &str, reason: Option<&str>);
    fn record_tool_use(&self, name: &str);
    /// `isPublicTool(name)`.
    fn public_tool(&self, name: &str) -> bool;
}

/// The turn's state the gate reads and writes across calls and rounds.
#[derive(Debug, Default)]
pub struct TurnTools {
    /// tool-exchange.cjs's per-turn results: (key, result, write).
    results: Vec<(Vec<u16>, String, bool)>,
    pub declined_writes: Vec<String>,
    pub not_run: HashSet<String>,
    pub not_approved: HashSet<String>,
    /// #658: (name, target) of writes that ran and succeeded.
    pub applied: Vec<(String, Option<String>)>,
    pub round_wrote: bool,
    /// `!spaceId?.startsWith('diary')`.
    pub decline_ends_reply: bool,
}

impl TurnTools {
    /// A turn's tool state; `decline_ends_reply`: `!spaceId?.startsWith('diary')` (#666).
    pub fn new(decline_ends_reply: bool) -> TurnTools {
        TurnTools {
            decline_ends_reply,
            ..TurnTools::default()
        }
    }
}

/// What one call left: the event, the model's message, and the write that succeeded (if any).
#[derive(Clone, Debug, PartialEq)]
pub struct Done {
    pub result: String,
    pub tool_result: JValue,
    /// `{role:'tool', tool_call_id, content}` (framed).
    pub message: JValue,
    /// The fingerprint of a write that ran and succeeded (`writesDone.record`).
    pub applied: Option<String>,
}

const CHECK_FAULT: &str = "ERROR: the tool call could not be checked; tool was not run.";

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn units(v: &str) -> Vec<u16> {
    v.encode_utf16().collect()
}

/// `str.slice(0, n)` over UTF-16 units.
fn slice_units(v: &str, n: usize) -> String {
    let u = units(v);
    String::from_utf16_lossy(u.get(..n).unwrap_or(&u))
}

/// `String(x).replace(/\.?$/, '.')`.
fn with_period(v: &str) -> String {
    format!("{}.", v.strip_suffix('.').unwrap_or(v))
}

const BLOCKED: &str = "is blocked in this account's settings, so it was not run. Do not retry it; tell the user they can change it in Settings → Connectors.";

/// Calls `started()` when dropped unless already called: a fence redeemed for a call is closed on
/// every path, the future's drop included.
struct FenceGuard<'a, F: Fence>(Option<&'a F>);

impl<F: Fence> FenceGuard<'_, F> {
    fn started(&mut self) {
        if let Some(f) = self.0.take() {
            f.started();
        }
    }
}

impl<F: Fence> Drop for FenceGuard<'_, F> {
    fn drop(&mut self) {
        self.started();
    }
}

/// One call: `runTool(tc, gate)`, then the `tool_result` event and the model's tool message.
/// `allowed`: the call's name is in this turn's tool list. `reduce`: `reduceToolResult(result).text`.
pub async fn run_call<I: GateIo>(
    io: &I,
    st: &mut TurnTools,
    call: &Call,
    allowed: bool,
    reduce: &dyn Fn(&str) -> String,
    send: &mut dyn FnMut(JValue),
) -> Done {
    let mut ran = false;
    let mut card_target: Option<String> = None;
    let mut write_print: Option<String> = None;
    let result = exchange(
        io,
        st,
        call,
        allowed,
        send,
        &mut ran,
        &mut card_target,
        &mut write_print,
    )
    .await;
    let for_model = reduce(&result);
    let framed = frame("tool result", &call.name, &for_model);
    let not_error = !result.starts_with("ERROR")
        || result
            .get(5..)
            .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'));
    let mut applied = None;
    if ran && not_error {
        if let Some(fp) = write_print.clone() {
            st.applied.push((call.name.clone(), card_target.clone()));
            applied = Some(fp);
        }
    }
    let mut ev = vec![
        ("type", s("tool_result")),
        ("index", JValue::Num(call.index as f64)),
        ("name", s(&call.name)),
        ("text", s(&slice_units(&result, 300))),
    ];
    if applied.is_some() {
        ev.push(("applied", JValue::Bool(true)));
        if let Some(t) = &card_target {
            ev.push(("target", s(t)));
        }
    }
    if st.not_approved.contains(&result) {
        ev.push(("declined", JValue::Bool(true)));
    }
    if st.not_run.contains(&result) {
        ev.push(("notRun", JValue::Bool(true)));
    }
    if applied.is_none() && io.public_tool(&call.name) {
        ev.push(("public", JValue::Bool(true)));
    }
    let tool_result = obj(ev);
    let message = obj(vec![
        ("role", s("tool")),
        ("tool_call_id", s(&call.id)),
        ("content", s(&framed)),
    ]);
    Done {
        result,
        tool_result,
        message,
        applied,
    }
}

/// tool-exchange.cjs `run(call, execute)` around the gate.
#[allow(clippy::too_many_arguments)]
async fn exchange<I: GateIo>(
    io: &I,
    st: &mut TurnTools,
    call: &Call,
    allowed: bool,
    send: &mut dyn FnMut(JValue),
    ran: &mut bool,
    card_target: &mut Option<String>,
    write_print: &mut Option<String>,
) -> String {
    let aborted = io.aborted();
    let enabled = !aborted && allowed;
    let args = (enabled && !call.args.is_empty()).then(|| units(&call.args));
    let key = match tool_exchange::check(aborted, enabled, &units(&call.name), args.as_deref()) {
        Err(_) => return CHECK_FAULT.to_owned(),
        Ok(tool_exchange::Check::Answer(a)) => return String::from_utf16_lossy(&a),
        Ok(tool_exchange::Check::Run(key)) => key,
    };
    if let Some((_, r, _)) = st.results.iter().find(|(k, _, _)| *k == key) {
        return r.clone();
    }
    let write = io.is_write(&call.name);
    let result = match gate(io, st, call, write, send, ran, card_target, write_print).await {
        Ok(r) => r,
        Err(message) => tool_exchange::call_error(&units(&call.name), &units(&message))
            .map_or_else(
                |_| "ERROR: the tool failed and its error could not be read.".to_owned(),
                |u| String::from_utf16_lossy(&u),
            ),
    };
    st.results.push((key, result.clone(), write));
    result
}

/// The gate (chat.cjs:2286-2531). `Err(message)`: the executor threw.
#[allow(clippy::too_many_arguments)]
async fn gate<I: GateIo>(
    io: &I,
    st: &mut TurnTools,
    call: &Call,
    write: bool,
    send: &mut dyn FnMut(JValue),
    ran: &mut bool,
    card_target: &mut Option<String>,
    write_print: &mut Option<String>,
) -> Result<String, String> {
    let name = call.name.as_str();
    let args = call.args.as_str();
    let user = io.user();
    let chat = io.chat_id();
    if let Some(r) = io.revoked_skill_refusal(name) {
        return Ok(r);
    }
    if !st.declined_writes.is_empty() && write {
        io.audit("tool.denied", name, Some("earlier-decline"));
        let r = format!("ERROR: {name} was not run because an earlier write in this reply was declined. Nothing was changed.");
        st.not_run.insert(r.clone());
        return Ok(r);
    }
    if call.too_large {
        io.audit("tool.denied", name, Some("arguments-too-large"));
        return Ok(format!("ERROR: {name} was not run: its name, id or arguments were larger than about 8 million characters, which is not a real tool call. Nothing was changed."));
    }
    if let Some(r) = io.egress_refusal(name, args) {
        io.audit("tool.denied", name, Some("external-provider-diary"));
        return Ok(r);
    }
    let permission = io.mode(name, write);
    if permission == Mode::Block {
        io.audit("tool.denied", name, Some("blocked"));
        return Ok(format!("ERROR: {name} {BLOCKED}"));
    }
    let mut edit: Option<(String, Option<String>)> = None;
    let mut target_kind: Option<String> = None;
    if io.edit_tool(name) {
        let resolved = io.resolve_edit(args);
        if let Some(e) = resolved.error {
            io.audit("tool.denied", name, Some("edit-target"));
            return Ok(format!(
                "ERROR: {} {name} was not run and nothing was changed.",
                with_period(&e)
            ));
        }
        let path = resolved.path.unwrap_or_default();
        *card_target = Some(path.clone());
        edit = Some((path, resolved.account));
    } else if io.has_write_targets() && write {
        let resolved = match io.write_target(name, args).await {
            Ok(t) => t,
            Err(()) => Some(WriteTarget {
                error: Some("The file this would change could not be looked up".into()),
                ..WriteTarget::default()
            }),
        };
        if let Some(e) = resolved.as_ref().and_then(|t| t.error.as_ref()) {
            io.audit("tool.denied", name, Some("write-target"));
            return Ok(format!(
                "ERROR: {} {name} was not run and nothing was changed.",
                with_period(e)
            ));
        }
        if let Some(t) = resolved
            .as_ref()
            .and_then(|t| t.target.clone())
            .filter(|t| !t.is_empty())
        {
            *card_target = Some(t);
            target_kind = resolved.and_then(|t| t.kind);
        }
    }
    *write_print = write.then(|| {
        crate::writes::fingerprint(
            &s(name),
            &card_target.as_deref().map_or(JValue::Null, s),
            &s(args),
        )
    });
    let mut provenance: Vec<JValue> = Vec::new();
    if write {
        if let Some(checked) = io.provenance(args) {
            let unchecked = || {
                vec![obj(vec![
                    ("field", JValue::Null),
                    ("source", JValue::Null),
                    ("unchecked", JValue::Bool(true)),
                ])]
            };
            provenance = checked.unwrap_or_else(|()| unchecked());
        }
    }
    if !provenance.is_empty() {
        io.audit("tool.provenance", name, None);
    }
    let route = io.route(name);
    let shown = (user.clone(), chat.clone(), route.clone());
    let mut binding: Option<I::Binding> = None;
    let mut used_grant = false;
    if permission == Mode::Ask {
        match io.permission_context().await {
            Ok(b) => binding = b,
            Err(()) => return Ok("ERROR: the chat permission context could not be verified; the tool was not run. Nothing was changed. Ask again.".into()),
        }
        used_grant = io.chat_wide_approved(&binding).await;
    }
    let asked_per_call = permission == Mode::Ask
        && (!used_grant || !provenance.is_empty() || io.automatic_only(name));
    if asked_per_call {
        let id = io.new_approval_id();
        let repeat = write_print.as_deref().is_some_and(|fp| io.repeat(fp));
        let not_asked = |st: &mut TurnTools| {
            let r = format!("ERROR: the approval for {name} could not be asked, so it was not run. Nothing was changed. Ask again.");
            io.audit("tool.denied", name, Some("unavailable"));
            st.not_run.insert(r.clone());
            r
        };
        if !io.open_approval(&id, &binding).await {
            return Ok(not_asked(st));
        }
        let mut ev = vec![
            ("type", s("tool_pending")),
            ("id", s(&id)),
            ("index", JValue::Num(call.index as f64)),
            ("name", s(name)),
            ("args", s(args)),
        ];
        if let Some(t) = card_target.as_deref() {
            ev.push(("target", s(t)));
        }
        if let Some(k) = target_kind.as_deref().filter(|k| !k.is_empty()) {
            ev.push(("targetKind", s(k)));
        }
        if let Some(r) = &route {
            ev.push(("server", s(r)));
        }
        if repeat {
            ev.push(("repeatOf", JValue::Bool(true)));
        }
        if !provenance.is_empty() {
            ev.push(("provenance", JValue::Arr(provenance.clone())));
        }
        send(obj(ev));
        let decision = io.await_approval(&id, &binding).await;
        if decision != Decision::Approve {
            if decision == Decision::Unavailable {
                return Ok(not_asked(st));
            }
            if decision == Decision::Aborted {
                let r = format!("ERROR: the reply was stopped before {name} was approved, so it was not run. Nothing was changed.");
                io.audit("tool.denied", name, Some("aborted"));
                st.not_run.insert(r.clone());
                return Ok(r);
            }
            let (r, reason) = if decision == Decision::Timeout {
                (format!("ERROR: the user did not respond in time, so {name} was not run. Ask before trying again."), "timeout")
            } else {
                let reason = match &decision {
                    Decision::Other(word) => word.as_str(),
                    _ => "deny",
                };
                (format!("ERROR: the user declined to run {name}. Do not retry it; ask what they would prefer."), reason)
            };
            io.audit("tool.denied", name, Some(reason));
            st.not_approved.insert(r.clone());
            if decision == Decision::Deny && st.decline_ends_reply {
                st.declined_writes.push(name.to_owned());
            }
            return Ok(r);
        }
    }
    if io.aborted() {
        return Ok("ERROR: exchange cancelled; tool was not run.".into());
    }
    if io.mode(name, write) == Mode::Block {
        io.audit("tool.denied", name, Some("blocked"));
        return Ok(format!("ERROR: {name} {BLOCKED}"));
    }
    if let Some(r) = io.revoked_skill_refusal(name) {
        return Ok(r);
    }
    if let Some((path, account)) = &edit {
        let again = io.resolve_edit(args);
        if again.path.as_deref() != Some(path.as_str()) || again.account != *account {
            io.audit("tool.denied", name, Some("edit-target-changed"));
            let quoted = |p: &str| wtf16::stringify(&s(p)).unwrap_or_default();
            let what = if again.error.is_some() {
                "can no longer be edited".to_owned()
            } else {
                format!(
                    "is no longer the file this name refers to (it now means {})",
                    again.path.as_deref().map_or("undefined".to_owned(), quoted)
                )
            };
            return Ok(format!("ERROR: the project's files changed after approval, so {} {what}. {name} was not run and nothing was changed. Ask again if the edit is still wanted.", quoted(path)));
        }
    }
    if io.has_write_targets() && write && edit.is_none() {
        let now = io.write_target(name, args).await;
        let (target, kind, error) = match now {
            Ok(Some(t)) => (
                t.target.filter(|x| !x.is_empty()),
                t.kind,
                t.error.is_some(),
            ),
            Ok(None) => (None, None, false),
            Err(()) => (None, None, true),
        };
        if error || target != *card_target || kind != target_kind {
            io.revoke_grant().await;
            return Ok(
                "ERROR: the write target changed after approval; the tool was not run. Ask again."
                    .into(),
            );
        }
    }
    let mut redeem_unavailable = false;
    let mut fence: Option<I::Fence> = None;
    if permission == Mode::Ask {
        let checked: Result<(), ()> = async {
            let final_binding = io.permission_context().await?;
            if (chat.as_deref().is_some_and(|c| !c.is_empty())
                && !io.binding_matches(&binding, &final_binding))
                || (io.user(), io.chat_id(), io.route(name)) != shown
                || !io.workspace_ok(&user)
            {
                return Err(());
            }
            if used_grant && !asked_per_call {
                if io.has_redeem() {
                    match io.redeem(&final_binding).await {
                        Redeemed::Unavailable => redeem_unavailable = true,
                        Redeemed::Fence(f) => fence = Some(f),
                        Redeemed::Invalid => return Err(()),
                    }
                } else if !io.chat_wide_approved(&final_binding).await {
                    return Err(());
                }
            }
            Ok(())
        }
        .await;
        if checked.is_err() {
            io.revoke_grant().await;
            return Ok("ERROR: the chat or permission target changed after approval; the tool was not run. Nothing was changed. Ask again.".into());
        }
    }
    let mut guard = FenceGuard(fence.as_ref());
    let not_checked = |st: &mut TurnTools, guard: &mut FenceGuard<'_, I::Fence>| {
        guard.started();
        let r = format!("ERROR: the permission for {name} could not be checked, so it was not run. Nothing was changed. Ask again.");
        io.audit("tool.denied", name, Some("unavailable"));
        st.not_run.insert(r.clone());
        r
    };
    if redeem_unavailable {
        return Ok(not_checked(st, &mut guard));
    }
    if let Some((path, account)) = &edit {
        let now = io.resolve_edit(args);
        if now.path.as_deref() != Some(path.as_str()) || now.account != *account {
            guard.started();
            io.revoke_grant().await;
            return Ok(
                "ERROR: the project target changed before dispatch; tool was not run. Ask again."
                    .into(),
            );
        }
    }
    if io.aborted() {
        guard.started();
        return Ok("ERROR: exchange cancelled; tool was not run.".into());
    }
    let now = io.mode(name, write);
    if now == Mode::Block
        || (permission != Mode::Ask && io.mode(name, write) == Mode::Ask)
        || io.revoked_skill_refusal(name).is_some()
    {
        guard.started();
        return Ok("ERROR: permission changed before dispatch; tool was not run.".into());
    }
    if fence.as_ref().is_some_and(|f| !f.open() || !io.executor_fences_dispatch()) {
        return Ok(not_checked(st, &mut guard));
    }
    // markWriteAttempt: a write drops the cached results of reads.
    if write {
        st.results.retain(|(_, _, w)| *w);
        st.round_wrote = true;
    }
    let executed = io.execute(name, args, fence.as_ref(), edit.as_ref()).await;
    guard.started();
    let executed = executed?;
    if executed.not_run {
        st.not_run.insert(executed.text.clone());
        io.audit("tool.denied", name, Some("unavailable"));
        return Ok(executed.text);
    }
    *ran = true;
    io.record_tool_use(name);
    if write {
        io.audit("tool.write", name, None);
    }
    if executed.failed {
        // A failed call is not an applied write (`outcome.failed !== true`).
        *write_print = None;
    }
    Ok(executed.text)
}
