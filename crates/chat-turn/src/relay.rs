//! The generation relay for one model round: turns what the upstream model streamed into the
//! events Node sends, and the partial record, in the order the turn's rules allow.
//!
//! This is a single round with no rounds loop, no approvals, no context compaction and no
//! final-answer pass: those are later steps. What it does follow from chat.cjs:
//! - `reasoning` and `delta` are relayed as they arrive; empty deltas are not sent (chat.cjs 2058).
//! - `telemetry streaming` once, at the first token of any kind (chat.cjs 1811).
//! - a `length` finish adds the token-budget warning (chat.cjs 2028).
//! - a provider failure sends `error` and ends the turn with no `done` (chat.cjs 2092, 2134).
//! - with no content but some reasoning, a fixed delta says so (chat.cjs 2449), then
//!   `telemetry complete` and `done` (chat.cjs 2451, 2460).

use crate::event::{Event, Phase, TimeToFirstToken};
use crate::partial::{Outcome, Partial};
use crate::sink::{OrderError, Turn};
use crate::tools::{cap_result, ToolCall, ToolDispatch, ToolOutcome};
use serde_json::Value;

/// chat.cjs 2028.
pub const LENGTH_WARNING: &str =
    "The model reached its thinking/answer token budget. This reply may be incomplete; try Low thinking or a narrower question.";

/// chat.cjs 2449.
pub const REASONING_ONLY_NOTE: &str =
    "\n\nThe model returned reasoning without a final answer. Try again or choose another model.";

/// One item from the upstream stream, already decoded from the provider's frames.
#[derive(Debug, Clone, PartialEq)]
pub enum Upstream {
    Reasoning(String),
    Delta(String),
    /// A complete tool call (the turn numbers it).
    Tool {
        name: String,
        args: Value,
    },
    /// The provider stopped on its token budget.
    Length,
    /// The provider failed; the text is what the client is shown.
    Fail(String),
}

/// What the relay produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Relayed {
    pub turn: Turn,
    pub partial: Partial,
    pub outcome: Outcome,
    pub content: String,
    pub reasoning: String,
}

/// Runs one round. `time_to_first_token` is seconds from the request to the first token, or `None`
/// when no token arrived.
pub fn relay<D, I>(
    model: &str,
    upstream: I,
    dispatch: &mut D,
    time_to_first_token: Option<f64>,
) -> Result<Relayed, OrderError>
where
    D: ToolDispatch,
    I: IntoIterator<Item = Upstream>,
{
    let mut run = Run {
        turn: Turn::resumed(),
        partial: Partial::new(),
        model,
        ttft: time_to_first_token,
        streaming_sent: false,
    };
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tools = 0usize;

    for item in upstream {
        match item {
            Upstream::Reasoning(text) => {
                run.first_token()?;
                reasoning.push_str(&text);
                run.emit(Event::Reasoning { text })?;
            }
            Upstream::Delta(text) => {
                if text.is_empty() {
                    continue;
                }
                run.first_token()?;
                content.push_str(&text);
                run.emit(Event::Delta { text })?;
            }
            Upstream::Tool { name, args } => {
                run.first_token()?;
                let index = tools;
                tools += 1;
                run.emit(Event::Tool {
                    index,
                    name: name.clone(),
                    args: args.clone(),
                })?;
                let call = ToolCall {
                    index,
                    name: name.clone(),
                    args,
                };
                let event = match dispatch.dispatch(&call) {
                    ToolOutcome::Result(text) => Event::ToolResult {
                        index,
                        name,
                        text: cap_result(&text),
                        not_run: false,
                    },
                    ToolOutcome::NotRun(text) => Event::ToolResult {
                        index,
                        name,
                        text: cap_result(&text),
                        not_run: true,
                    },
                };
                run.emit(event)?;
            }
            Upstream::Length => run.emit(Event::Warning {
                text: LENGTH_WARNING.to_string(),
            })?,
            Upstream::Fail(text) => {
                run.emit(Event::Error { text, code: None })?;
                return Ok(run.finish(Outcome::Failed, content, reasoning));
            }
        }
    }

    if content.is_empty() && !reasoning.trim().is_empty() {
        run.emit(Event::Delta {
            text: REASONING_ONLY_NOTE.to_string(),
        })?;
    }
    let ttft = match time_to_first_token {
        Some(s) => TimeToFirstToken::Seconds(s),
        None => TimeToFirstToken::Unknown,
    };
    run.emit(Event::Telemetry {
        phase: Phase::Complete,
        model: model.to_string(),
        time_to_first_token: ttft,
    })?;
    run.emit(Event::Done {
        model: model.to_string(),
    })?;
    Ok(run.finish(Outcome::Completed, content, reasoning))
}

struct Run<'m> {
    turn: Turn,
    partial: Partial,
    model: &'m str,
    ttft: Option<f64>,
    streaming_sent: bool,
}

impl Run<'_> {
    fn emit(&mut self, event: Event) -> Result<(), OrderError> {
        self.partial.observe(&event);
        self.turn.push(event)
    }

    /// Sends `telemetry streaming` once, before the first event of the reply.
    fn first_token(&mut self) -> Result<(), OrderError> {
        if self.streaming_sent {
            return Ok(());
        }
        self.streaming_sent = true;
        let ttft = match self.ttft {
            Some(s) => TimeToFirstToken::Seconds(s),
            None => TimeToFirstToken::Unknown,
        };
        let model = self.model.to_string();
        self.emit(Event::Telemetry {
            phase: Phase::Streaming,
            model,
            time_to_first_token: ttft,
        })
    }

    fn finish(self, outcome: Outcome, content: String, reasoning: String) -> Relayed {
        Relayed {
            turn: self.turn,
            partial: self.partial,
            outcome,
            content,
            reasoning,
        }
    }
}
