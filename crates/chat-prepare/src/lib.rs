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
//! - [`SystemParts`]: chat.cjs's ordered `sysParts`, joined with `'\n\n'` into the system message.
//! - [`prepare`] / [`messages`]: the parts ahead of the replayed history and the new user message.
//!
//! Strings are JS strings held as `js_json::wtf16` text (a lone surrogate is one private-use
//! character). Everything is replayed byte for byte on Node's own output
//! (`tests/fixtures/node-prepare.v1.json`, written by `tests/record_prepare.cjs`).
//!
//! Named differences (recorded in the fixture's `differences`, each checked as documented here):
//! - An account file `wtf16::parse` refuses but `JSON.parse` reads (a number outside the f64
//!   range, a character of U+10F800..U+10FFFF, nesting past 128) reads as the empty record.
//! - The time part states only Gregorian years 1583-9999 ([`PrepareError::TimeOutOfRange`]
//!   otherwise); Node writes Julian days before 1583, and its `calendarFacts` puts years 0-99 in
//!   1900-1999 (`Date.UTC`), a Node bug only reachable through the contract-corpus pin.
//! - Zones: legacy ICU IDs outside the IANA database (`PST`, `AET`, `SystemV/AST4`) fall back to
//!   UTC here (`serverTimeZone()` resolves an IANA name, so a deployment does not reach this).
//! - Zone data: the day comes from chrono-tz's bundled IANA data, Node's from its ICU's, and the
//!   two releases differ (see [`time`]). A server whose zone changed between them states a
//!   different day near midnight; that is reachable on a deployment in such a zone.
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

/// Which `sysParts` entry a part is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Account,
    Time,
    Memory,
    /// A part a later slice adds, named by the chat.cjs block it ports (a project line, a frame
    /// block, ...).
    Other(&'static str),
}

/// chat.cjs's `sysParts`: the system message's parts in the order they were pushed. Empty parts
/// are never pushed (Node pushes only truthy ones).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemParts {
    parts: Vec<(PartKind, String)>,
}

impl SystemParts {
    /// `sysParts.push(text)` when `text` is not empty.
    pub fn push(&mut self, kind: PartKind, text: String) {
        if !text.is_empty() {
            self.parts.push((kind, text));
        }
    }

    pub fn parts(&self) -> &[(PartKind, String)] {
        &self.parts
    }

    /// The first part of `kind`.
    pub fn get(&self, kind: PartKind) -> Option<&str> {
        self.parts
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, t)| t.as_str())
    }

    /// `sysParts.join('\n\n')`, the system message (`sys`).
    pub fn text(&self) -> String {
        let texts: Vec<&str> = self.parts.iter().map(|(_, t)| t.as_str()).collect();
        texts.join("\n\n")
    }
}

/// What a turn is prepared from.
///
/// Preconditions (what P1 covers; anything else is a later slice, and calling this for it gives
/// a system message Node would not send):
/// - No project: after chat.cjs resolves `body.projectId` and a free chat's diary-extras context
///   project (`diaryExtras.chatProjectId`), `project` is null. A free chat with attachments has a
///   hidden shadow project (`cowork-chat-context-*`), so it is a project turn, not this case.
/// - Chat framing off for this turn: chat.cjs ~735-755 pushes the confirmed frame's block and the
///   linked brain block after the memory part (the P4 slice). Chat framing is on live, so a live
///   no-project turn with a confirmed frame is not covered yet; such blocks go in `trailing`
///   once ported.
/// - No Diary space and no `compactOnly` side effects beyond the replay (`message: None`).
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
    /// `carryOrphanTool` and `knownTool` (see [`ReplayOptions`]). The replay here is the one
    /// chat.cjs builds before the route is known, so `off_box` is always false: a route that
    /// leaves the box rebuilds it with [`history::replay_history`] and `off_box: true`
    /// (chat.cjs ~1265), a later slice.
    pub carry_orphan_tool: &'a (dyn Fn(&str) -> bool + Sync),
    pub known_tool: &'a (dyn Fn(&str) -> bool + Sync),
    /// Parts pushed after the memory part, in order (empty for P1).
    pub trailing: &'a [(PartKind, String)],
}

/// A prepared turn: chat.cjs's `sysParts`, `timePart`, `sys`, `msgs` and `wire` at the point the
/// route is chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub parts: SystemParts,
    /// `timePart`, kept because an off-box route swaps it for the zoneless one. Node does that with
    /// `sys.replace(timePart, ...)`, which replaces the first match in the joined text: if an
    /// earlier part (the user's custom text) holds the same text, that copy is replaced instead.
    pub time_part: String,
    /// The system message's text (`sys`).
    pub system: String,
    /// The replayed history and the new user message (`msgs`).
    pub history: Vec<JValue>,
    /// `[{ role: 'system', content: system }, ...history]` (`wire`).
    pub messages: Vec<JValue>,
}

/// The system parts in chat.cjs's order (account part, time part, account memory, then
/// `trailing`), the system message and the wire of a turn that meets [`Turn`]'s preconditions.
pub fn prepare(turn: &Turn<'_>) -> Result<Prepared, PrepareError> {
    let opts = ReplayOptions {
        carry_orphan_tool: turn.carry_orphan_tool,
        off_box: false,
        known_tool: turn.known_tool,
    };
    let history = history::replay_history(turn.history, turn.history_cap, turn.message, &opts);
    let mut parts = SystemParts::default();
    if let Some(p) = account::part(turn.instructions).map_err(|_| PrepareError::Thrown)? {
        parts.push(PartKind::Account, p);
    }
    let time_part =
        time::part(turn.now_ms, turn.time_zone, true).ok_or(PrepareError::TimeOutOfRange)?;
    parts.push(PartKind::Time, time_part.clone());
    // Without a project, `memoryPart` has the account's lines only (`project?.memories` is undefined).
    if let Some(p) = memory::part(turn.memory, &JValue::Undefined) {
        parts.push(PartKind::Memory, p);
    }
    for (kind, text) in turn.trailing {
        parts.push(*kind, text.clone());
    }
    let system = parts.text();
    // `sys ? [system, ...msgs] : msgs`: the time part is never empty, so there is always one.
    let mut messages = Vec::with_capacity(history.len() + 1);
    messages.push(js::message("system", system.clone()));
    messages.extend(history.iter().cloned());
    Ok(Prepared {
        parts,
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
