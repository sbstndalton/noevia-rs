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

/// One call to run through the tool engines (core's `/internal/chat/tools/execute`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRun {
    pub project_id: Option<String>,
    pub name: String,
    pub args: String,
    pub allowed: Vec<String>,
    pub routes: Vec<(String, String)>,
    pub chat_key: Option<String>,
    pub exchange_key: String,
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
    /// The chat's permission binding for `scope` (chat.cjs `permissionContext`): `null` for a turn with
    /// no chat id, `Err` when it cannot be verified (store failing, chat moved, deleted or held elsewhere).
    fn permission_binding(
        &self,
        chat_id: Option<&str>,
        scope: &str,
        holder: &JValue,
    ) -> impl Future<Output = Result<JValue, ()>> + Send;
    /// "Allow for this chat" holds for this chat, scope and binding.
    fn chat_wide_approved(&self, chat_id: Option<&str>, scope: &str, binding: &JValue) -> bool;
    /// Waits for the person's answer to an approval card: "approve", "deny", "timeout" or "aborted".
    fn await_approval(&self, request: &JValue) -> impl Future<Output = String> + Send;
    /// Ends the chat's "Allow for this chat" grant.
    fn revoke_grant(&self, chat_id: Option<&str>);
    /// One audit line for the signed-in user (`authService.audit(event, user, user, details)`).
    fn audit(&self, event: &str, details: &JValue);
    /// Runs one call: its text and whether it failed, or the error it threw.
    fn execute_tool(
        &self,
        run: &ToolRun,
    ) -> impl Future<Output = Result<(String, bool), String>> + Send;

    /// `prefill.recordSample(model, prompt, ms)`.
    fn prefill_sample(&self, model: &str, prompt: f64, first_token_ms: f64);
    /// The model's draft acceptance as evidence (model manager `recordEvidence`).
    fn mtp_evidence(&self, model: &str, drafted: f64, accepted: f64);
}
