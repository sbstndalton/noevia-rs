//! The native chat turn's preparation (track R8, phase 2), slice P1: the system message and the
//! replayed history of a turn without a project, as noevia-core's `server/chat.cjs` `handleChat`
//! builds them before routing (its `sysParts`, `msgs` and `wire`). Pure: the caller reads the
//! account files and the clock and passes their contents in.
//!
//! - [`account`]: `account-instructions.cjs` `read` and `systemPart` (style, advanced controls,
//!   response language, the custom text).
//! - [`time`]: `current-time-part.cjs` `systemPart`, `calendarFacts`, `fixedNow`.
//! - [`memory`]: `account-memory.cjs` `read` and `systemPart`.
//! - [`history`]: the filter, `HISTORY_CAP` cut and map of `body.history`, then
//!   `normalizeReplayHistory`.
//! - [`prepare`] / [`messages`]: the parts joined into the system message (`sysParts.join('\n\n')`)
//!   ahead of the replayed history and the new user message.
//!
//! Strings are JS strings held as `js_json::wtf16` text (a lone surrogate is one private-use
//! character). Everything is replayed byte for byte on Node's own output
//! (`tests/fixtures/node-prepare.v1.json`, written by `tests/record_prepare.cjs`).
//!
//! What a turn with a project adds (project name, goal, instructions and memories, shared code
//! context, knowledge-file excerpts, skills, framing and brain blocks, the stored-only and
//! not-ready source notes) and what happens to the wire once the route is known (off-box zone
//! and history filter, vision, route notes) are later slices.

pub mod account;
pub mod history;
pub mod js;
pub mod memory;
pub mod time;

pub use history::{Entry, ReplayOptions};
use js_json::JValue;

/// Node would throw a TypeError here (a settings value whose `String()` throws).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thrown;

/// Why [`prepare`] built nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareError {
    /// The account settings make Node throw ([`Thrown`]); a [`account::read`] record never does.
    Thrown,
    /// The instant's date in the zone is outside the years [`time::part`] states.
    TimeOutOfRange,
}

/// What a turn without a project is prepared from.
pub struct Turn<'a> {
    /// [`account::read`] of `account-instructions.json`.
    pub instructions: &'a JValue,
    /// [`memory::read`] of `account-memory.json`.
    pub memory: &'a JValue,
    /// `currentNow()`: [`time::FixedNow::now`] of the clock.
    pub now_ms: i64,
    /// `serverTimeZone()`: the process's resolved IANA zone ('UTC' when it has none).
    pub time_zone: &'a str,
    /// `body.history` as the client sent it.
    pub history: &'a JValue,
    /// `HISTORY_CAP` (1000 in index.cjs).
    pub history_cap: usize,
    /// `body.message`; `None` for `compactOnly` (the replay gets no new user turn).
    pub message: Option<&'a str>,
    pub replay: ReplayOptions<'a>,
}

/// A prepared turn: chat.cjs's `timePart`, `sys`, `msgs` and `wire` at the point the route is
/// chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub time_part: String,
    /// The system message's text.
    pub system: String,
    /// The replayed history and the new user message.
    pub history: Vec<JValue>,
    /// `[{ role: 'system', content: system }, ...history]`.
    pub messages: Vec<JValue>,
}

/// The system parts in chat.cjs's order (account part, time part, account memory), the system
/// message and the wire of a turn without a project.
pub fn prepare(turn: &Turn<'_>) -> Result<Prepared, PrepareError> {
    let history = history::replay_history(
        turn.history,
        turn.history_cap,
        turn.message,
        &turn.replay,
    );
    let mut parts = Vec::new();
    if let Some(p) = account::part(turn.instructions).map_err(|_| PrepareError::Thrown)? {
        parts.push(p);
    }
    let time_part =
        time::part(turn.now_ms, turn.time_zone, true).ok_or(PrepareError::TimeOutOfRange)?;
    parts.push(time_part.clone());
    if let Some(p) = memory::part(turn.memory, &JValue::Undefined) {
        parts.push(p);
    }
    let system = parts.join("\n\n");
    // `sys ? [system, ...msgs] : msgs`: the time part is never empty, so there is always one.
    let mut messages = Vec::with_capacity(history.len() + 1);
    messages.push(js::message("system", system.clone()));
    messages.extend(history.iter().cloned());
    Ok(Prepared {
        time_part,
        system,
        history,
        messages,
    })
}

/// [`prepare`]'s wire: the messages of the model request.
pub fn messages(turn: &Turn<'_>) -> Result<Vec<JValue>, PrepareError> {
    prepare(turn).map(|p| p.messages)
}
