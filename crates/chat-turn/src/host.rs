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
    /// The model manager's `health()` body, or `None` when it did not answer ok.
    fn health(&self) -> impl Future<Output = Option<JValue>> + Send;
    /// The model manager's `load(model)`; whether it answered ok.
    fn load(&self, model: &str) -> impl Future<Output = bool> + Send;
    /// Whether the engine's chat template for `model` can take tools (chat-template-caps toolsGate);
    /// an unknown answer is yes.
    fn template_allows_tools(&self, model: &str) -> impl Future<Output = bool> + Send;

    /// One engine request: its status and body, or the error it failed with before any answer.
    fn request(
        &self,
        request: &EngineRequest,
    ) -> impl Future<Output = Result<(u16, Self::Body), String>> + Send;
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
    /// Writes it (atomically, mode 0600).
    fn save_state(&self, id: &str, state: &JValue);

    /// `recordUsage(workspace, model, reported)`.
    fn record_usage(&self, model: &str, reported: &JValue);
    /// `prefill.recordSample(model, prompt, ms)`.
    fn prefill_sample(&self, model: &str, prompt: f64, first_token_ms: f64);
    /// The model's draft acceptance as evidence (model manager `recordEvidence`).
    fn mtp_evidence(&self, model: &str, drafted: f64, accepted: f64);
}
