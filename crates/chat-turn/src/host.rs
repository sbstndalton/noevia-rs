//! What the native turn asks of the process that runs it: the client's stream, the engine, the model
//! manager, the turn's lease and the account's stores. The front implements it for real turns; the
//! replay tests implement it from the recordings. Every method takes `&self`, so a turn can hold the
//! host across awaits; implementations keep their own interior state.

use std::future::Future;

use js_json::JValue;

/// One request to the engine (chat.cjs `requestWithEffort`'s fetch): JSON body, as written.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// The round's stream (`stream: true`), read chunk by chunk; else read whole.
    pub stream: bool,
    /// The whole request's deadline (the summary's 180 s, the fallback's 300 s, the title's 8 s); a
    /// streamed round has none, only the idle limit after its first output.
    pub timeout_ms: Option<u64>,
}

/// An engine answer: its status, headers (lower-case names) and body.
pub struct Answer<B> {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: B,
}

/// What a read of an engine answer's body gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    Chunk(Vec<u8>),
    End,
    /// No bytes for the idle limit after the first output (chat.cjs withIdleLimit, #1574).
    Idle,
    /// The client went away (the turn's own abort).
    Aborted,
    /// The stream broke; the error's message.
    Failed(String),
}

/// A refusal of the model's lease (the inference gate, noevia#2115): its public text, code and
/// whether trying again may help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRefusal {
    pub text: String,
    pub code: Option<String>,
    pub retryable: Option<bool>,
}

/// What a write would change, for its approval card (writeTargetFor, #659).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Nothing to show.
    None,
    Found {
        target: String,
        kind: Option<String>,
    },
    /// Refused before the card, with this reason; nothing is written.
    Refused(String),
}

/// The stored file a project edit tool would change (project-edit-target.cjs `resolveEditTarget`,
/// #648/#687), through core's `/internal/chat/tools/edit-target`, against the project as stored when asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    /// The stored path the edit writes (the card shows it), the storage account a move into the
    /// project folder is bound to (`account || null`), and the digest the call is pinned to
    /// (`targetDigest(path, account)`, 64 hex).
    Found {
        path: String,
        account: Option<String>,
        digest: String,
    },
    /// `resolveEditTarget`'s error, as it wrote it (the gate adds the final period).
    Refused(String),
    /// The bridge gave no readable answer (it could not be reached, or answered something else).
    /// `resolveEditTarget` never throws in chat.cjs, so this case is the native executor's own: the
    /// gate refuses the call as a target that could not be looked up (see `gate.rs`).
    Failed,
}

/// One call to run through the tool engines (core's `/internal/chat/tools/execute`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRun {
    pub project_id: Option<String>,
    pub name: String,
    pub args: String,
    pub allowed: Vec<String>,
    pub routes: Vec<(String, String)>,
    pub chat_key: Option<String>,
    pub exchange_key: String,
    /// An approved project file edit's pinned file (`editTarget`: [`EditTarget::Found`]'s digest).
    pub edit_target: Option<String>,
    /// A write run under "Allow for this chat" without a card: core redeems the grant and runs the call behind
    /// its dispatch fence, in one request (port contract S9, D17).
    pub grant: Option<GrantRun>,
}

/// The chat-wide grant a call runs under: core's single-use handle for it and the binding read last.
#[derive(Debug, Clone, PartialEq)]
pub struct GrantRun {
    pub handle: String,
    pub chat: String,
    pub scope: String,
    pub binding: JValue,
}

/// What core's execute bridge did with a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ran {
    /// The call ran (or its executor refused it at the outbound write: `not_run`, the fence closed).
    Done {
        text: String,
        failed: bool,
        not_run: bool,
    },
    /// The tool threw: its message.
    Threw(String),
    /// The grant was not redeemed (revoked, moved, expired, or the handle is not this call's): not run.
    Refused,
    /// The grant could not be checked at the front: not run.
    Unavailable,
    /// The call was stopped (the client left) after its grant was redeemed and before it ran: not run.
    Aborted,
}

/// An instruction skill as the prepared frame and core's skills bridges name it (#272): its stored file, the
/// SHA-256 of the version, and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub file: String,
    pub hash: String,
    pub name: String,
}

/// What core's round-0 scrub (#546, `/internal/chat/skills/scrub`) changed: the new content of each message it
/// replaced (by index into the messages sent), and the names of the skills it took out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scrub {
    pub changed: Vec<(usize, String)>,
    pub removed: Vec<String>,
}

/// What the native turn asks of the process that runs it.
///
/// Duties of an implementation (Node does these through the response and its `chatSignal`):
/// - every future returns promptly once the client has left (`left()` true), as Node's abort ends each
///   request, wait and read; a request the client's leaving cut short must not count as answered;
/// - `emit` writes the event and also gives it to the turn record ([`crate::after::Saving::observe`])
///   with whether the client had left, and the 4 s partial saver writes `record.partial_entry()` with
///   the running marker while the turn runs (chat-partial.cjs);
/// - `permission_binding` fails (`Err`) when the workspace was revoked or is no longer the user's, as
///   chat.cjs's final check (`currentWorkspace().userId`, `revoked`) refuses then.
pub trait Host: Sync {
    /// An engine answer's body.
    type Body: Send;

    /// Writes one event to the client (dropped once the client has left).
    fn emit(&self, event: &JValue);
    /// The client went away.
    fn left(&self) -> bool;
    /// `Date.now()`.
    fn now_ms(&self) -> f64;

    /// The model's lease for the rest of the turn: the headers every engine request carries, or a
    /// refusal. `None`: no lease is taken (no gate configured).
    fn lease(
        &self,
        model: &str,
    ) -> impl Future<Output = Result<Option<Vec<(String, String)>>, GateRefusal>> + Send;
    /// Gives the model's lease back: the turn calls it once when it ends, before what it keeps is
    /// saved. The implementation also gives it back itself the moment the client leaves (Node's
    /// `close` handler, before the turn's audit, tool-use count and save), so a later call finds
    /// nothing to give back and does nothing.
    fn release_lease(&self);
    /// The model manager's `health()` body, or `None` when it did not answer ok.
    fn health(&self) -> impl Future<Output = Option<JValue>> + Send;
    /// The model manager's `load(model)`; whether it answered ok.
    fn load(&self, model: &str) -> impl Future<Output = bool> + Send;
    /// Whether the engine's chat template for `model` can take tools (chat-template-caps toolsGate);
    /// an unknown answer is yes.
    fn template_allows_tools(&self, model: &str) -> impl Future<Output = bool> + Send;

    /// One engine request: its answer, or the error it failed with before any answer (its deadline
    /// passing included). Dropping a body cancels it.
    fn request(
        &self,
        request: &EngineRequest,
    ) -> impl Future<Output = Result<Answer<Self::Body>, String>> + Send;
    /// The next read of a streamed body. `idle_ms`: the limit after the first output, if any.
    fn read(
        &self,
        body: &mut Self::Body,
        idle_ms: Option<u64>,
    ) -> impl Future<Output = Read> + Send;
    /// The rest of a body as text, at most `cap` bytes (http.cjs readCappedText); "" when it cannot be read.
    fn text(&self, body: Self::Body, cap: usize) -> impl Future<Output = (String, bool)> + Send;

    /// The account's context state file for `id` (`context-<sha256(id)>.json`), parsed; `{}` when absent.
    fn read_state(&self, id: &str) -> JValue;
    /// Writes it (atomically, mode 0600); `Err` with the reason when the workspace no longer stands.
    fn save_state(&self, id: &str, state: &JValue) -> Result<(), String>;
    /// Remembers that `model`'s chat template refused tools, so later turns send none (#1015).
    fn mark_tools_unsupported(&self, model: &str);
    /// Other inference is in flight on the local engine (model manager `inferenceBusy(1)`).
    fn engine_busy(&self) -> impl Future<Output = bool> + Send;
    /// The chat still carries the title derived from its first message (projects store `chatAutoNamed`).
    fn title_auto_named(
        &self,
        list: Option<&str>,
        chat: &str,
        first_message: &str,
    ) -> impl Future<Output = bool> + Send;
    /// Saves a generated title over the derived one (projects store `setChatTitle`).
    fn title_save(&self, list: Option<&str>, chat: &str, title: &str, first_message: &str);

    /// `recordUsage(workspace, model, reported)`.
    fn record_usage(&self, model: &str, reported: &JValue);
    /// The signed-in user (the turn's workspace's).
    fn user_id(&self) -> String;
    /// `crypto.randomUUID()`.
    fn uuid(&self) -> String;
    /// The account's tool policy (Settings → Connectors): "allow", "ask" or "block".
    fn tool_policy(&self, name: &str, write: bool) -> String;
    /// What a write would change (core bridge).
    fn write_target(
        &self,
        name: &str,
        args: &str,
        chat_key: Option<&str>,
    ) -> impl Future<Output = Target> + Send;
    /// The stored file a project edit tool call would change (core bridge), for the turn's project.
    fn edit_target(
        &self,
        project_id: Option<&str>,
        name: &str,
        args: &str,
    ) -> impl Future<Output = EditTarget> + Send;
    /// The chat's permission binding for `scope` (chat.cjs `permissionContext`): `null` for a turn with
    /// no chat id, `Err` when it cannot be verified (store failing, chat moved, deleted or held elsewhere).
    fn permission_binding(
        &self,
        chat_id: Option<&str>,
        scope: &str,
        holder: &JValue,
    ) -> impl Future<Output = Result<JValue, ()>> + Send;
    /// "Allow for this chat" holds for this chat, scope and binding: core's single-use handle for the grant
    /// (`/internal/chat/tools/grant`, Node's `chatWideApproved`), or `None`.
    /// The handle is bound to the turn's exchange (`exchange_key`), whose execute calls alone may redeem it.
    fn chat_wide_approved(
        &self,
        chat_id: Option<&str>,
        scope: &str,
        binding: &JValue,
        exchange_key: &str,
    ) -> impl Future<Output = Option<String>> + Send;
    /// Opens the card's question at the front's approval registry (S9 `openApproval`): false when it could not.
    fn open_approval(&self, request: &JValue) -> impl Future<Output = bool> + Send;
    /// Waits for the person's answer to an approval card: "approve", "deny", "timeout", "aborted" or
    /// "unavailable".
    fn await_approval(&self, request: &JValue) -> impl Future<Output = String> + Send;
    /// Ends the chat's "Allow for this chat" grant (Node's `revokeChatGrant`), before the turn goes on.
    fn revoke_grant(&self, chat_id: Option<&str>) -> impl Future<Output = ()> + Send;
    /// One audit line for the signed-in user (`authService.audit(event, user, user, details)`).
    fn audit(&self, event: &str, details: &JValue);
    /// Runs one call (and redeems its grant first, when it carries one).
    fn execute_tool(&self, run: &ToolRun) -> impl Future<Output = Ran> + Send;

    /// The loaded skills that are gone, disabled, changed or no longer reviewed in the project as stored now
    /// (core's `/internal/chat/skills/revoked`): `(file, name)` each; `None` when the bridge gave no readable answer.
    fn skills_revoked(
        &self,
        project_id: &str,
        loaded: &[Skill],
    ) -> impl Future<Output = Option<Vec<(String, String)>>> + Send;
    /// Which of `candidates` one successful tool call (`args`, `result`) read; core records each in the account's
    /// skill ledger for the chat (`/internal/chat/skills/read`). `None` when the bridge gave no readable answer.
    fn skills_read(
        &self,
        project_id: &str,
        chat_id: Option<&str>,
        candidates: &[Skill],
        args: &str,
        result: &str,
    ) -> impl Future<Output = Option<Vec<Skill>>> + Send;
    /// The round-0 scrub of `messages` (each `{role, content}`, or `null` for one the scrub does not read) against
    /// the project as stored now (`/internal/chat/skills/scrub`). `None` when the bridge gave no readable answer.
    fn skills_scrub(
        &self,
        project_id: &str,
        chat_id: Option<&str>,
        messages: &[JValue],
    ) -> impl Future<Output = Option<Scrub>> + Send;

    /// `prefill.recordSample(model, prompt, ms)`.
    fn prefill_sample(&self, model: &str, prompt: f64, first_token_ms: f64);
    /// The model's draft acceptance as evidence (model manager `recordEvidence`).
    fn mtp_evidence(&self, model: &str, drafted: f64, accepted: f64);

    /// The tool gate's log (tool-gate.cjs `record`): `gate.retry` or `gate.miss` for the required tool, `prefetch.failed`
    /// for a prefetch that did not run or failed.
    fn gate_record(&self, event: &str, tool: &str);

    /// Step supervision after a tool round (chat.cjs `superviseNextStep`, asked of core's supervisor):
    /// `continue`, `verify` or `escalate`. `outputs` are the round's last tool and assistant messages as the
    /// supervisor reads them; `goal` the turn's message as it reads it (its first 1500 UTF-16 units). No answer is
    /// `continue`, as the supervisor falls back.
    fn supervise(
        &self,
        round: i32,
        outputs: &[JValue],
        goal: &str,
        wrote: bool,
    ) -> impl Future<Output = String> + Send;

    /// #740 the framing reasoner on the gate's prefetched read (chat.cjs `prefetchTool`'s `condense`, asked of core):
    /// the packet as the tool message's content and the trace the reply's opt-in trace records
    /// (`{kind, tool, packet, timings}`), or None (any skip: the raw framed result goes on).
    fn condense(
        &self,
        kind: &str,
        message: &str,
        tool: &str,
        result_text: &str,
        model: &str,
    ) -> impl Future<Output = Option<(String, JValue)>> + Send;

    /// #740 after the reply: the person's opt-in trace of the handed-on packet (`reasoningTraces.append`; core checks
    /// the opt-in). `answer_chars` the last round's content in UTF-16 units, `answer_ms` since the packet was handed.
    /// Awaited before `done`, as chat.cjs appends before it.
    fn reasoner_trace(
        &self,
        trace: &JValue,
        answer_chars: f64,
        answer_ms: f64,
    ) -> impl Future<Output = ()> + Send;
}
