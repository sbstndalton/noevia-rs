//! The seam between a turn and the tool engines (MCP, browser, fetch, Drive, code tasks; R5 and
//! R7 build them). A turn hands a call to a [`ToolDispatch`] and relays what comes back. Approvals,
//! sandboxes and network policy live behind the trait, not here.

use serde_json::Value;

/// Node cuts every tool result shown to the client and to the model's next round to this many
/// characters (chat.cjs 1681, 2201, 2370). The draft counts Unicode scalars; Node counts UTF-16
/// units, so the two differ only for characters outside the Basic Multilingual Plane.
pub const RESULT_CAP_CHARS: usize = 300;

/// One tool call the model asked for, with the index the turn gave it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub index: usize,
    pub name: String,
    pub args: Value,
}

/// What a dispatcher answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutcome {
    /// The tool ran (or was refused with a message) and this is its text.
    Result(String),
    /// The turn did not run the call; the text says why (Node: "Not run: this step only writes
    /// the final answer.", chat.cjs 2166).
    NotRun(String),
}

/// Runs tool calls for a turn. Implemented by the engines; the turn never knows which one.
pub trait ToolDispatch {
    fn dispatch(&mut self, call: &ToolCall) -> ToolOutcome;
}

/// The text the turn keeps and shows: cut to [`RESULT_CAP_CHARS`] characters.
pub fn cap_result(text: &str) -> String {
    text.chars().take(RESULT_CAP_CHARS).collect()
}
