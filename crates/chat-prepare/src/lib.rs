//! The native chat turn's preparation (track R8, phase 2): the system message and the replayed
//! history of a turn, as noevia-core's `server/chat.cjs` `handleChat` builds them before routing
//! (its `sysParts`, `msgs` and `wire`). Slice P1: a turn without a project; slice P2a: what a
//! project turn adds that needs neither retrieval nor tools to be computed; slice P3a: its
//! instruction skills ([`skills`]). Pure: the caller reads the account files, the project record
//! and the clock and passes them in.
//!
//! - [`account`]: `account-instructions.cjs` `read` and `systemPart` (style, advanced controls,
//!   response language, the custom text).
//! - [`time`]: `current-time-part.cjs` `systemPart`, `calendarFacts`, `fixedNow`.
//! - [`memory`]: `account-memory.cjs` `read` and `systemPart`.
//! - [`history`]: the filter, `HISTORY_CAP` cut and map of `body.history`, then
//!   `normalizeReplayHistory`.
//! - [`names`]: `quoted-name.cjs` `quoteName` and `quotedNames`.
//! - [`skills`] (slice P3a): the instruction-skill parts, [`project::ProjectTurn::skills`].
//! - [`shared`]: `shared-context.cjs` `read` and `forChat` (the recent Code tasks part).
//! - [`sources`]: `source-readability.cjs` (`isUnreadable`, `isWaiting`, `readable`,
//!   `waitingNotice`), `source-coverage.cjs` `coverageNote`, and chat.cjs's not-ready and
//!   stored-only lines.
//! - [`project`]: the project parts in chat.cjs's order and a shadow project's knowledge block in
//!   front of the user's message.
//! - [`frame`] (slice P4a): `chat-frame-steering.cjs` `storedFrame`, `frameBlock` and `gateBias`,
//!   and chat.cjs's rules for reading the frame; [`frame::frame_part`] is the first
//!   [`Turn::trailing`] part (recorded in `tests/fixtures/node-frame.v1.json`, written by
//!   `tests/record_frame.cjs`).
//! - [`route`] (slice P5a): what a routed turn asks of the model besides its messages: the
//!   reasoning effort ([`effort`], `reasoning-effort.cjs`), the Fast route's thinking switch and
//!   the task-aware sampling presets ([`sampling`], `sampling-presets.cjs` in full), as chat.cjs
//!   ~1405-1430 computes them (recorded in `tests/fixtures/node-route.v1.json`, written by
//!   `tests/record_route.cjs`, which runs those lines of the pinned chat.cjs). `wantsAuto`,
//!   `routedRole` and the provider row come from [`roles`] (P5b) and the later routing slices. Named
//!   differences: two hosts Node's URL parser and the `url` crate read apart (a Unicode 16
//!   letter, a mixed-direction label), see [`effort::local_engine`]; every other recorded case
//!   (8,466 URL pairs and rows among them) replays byte for byte.
//! - [`roles`] (slice P5b): which model answers the turn and as which role, before P5a's lines:
//!   `wantsAuto`, the Automatic roles check (400/409), the classification and its fallbacks, the
//!   Creative move, the pre-load target, the Manual pick and its loaded-model default, and the 400
//!   and 409 refusals of a turn with no or a non-chat model. The classification is
//!   [`system_one`] (`system-one-router.cjs`) over [`router`] (`auto-router.cjs`). Recorded in
//!   `tests/fixtures/node-roles.v1.json`, written by `tests/record_roles.cjs`, which runs those
//!   modules and chat.cjs's own lines; no named differences. Routing modes, Inference machines, the
//!   egress and ChatGPT checks, the tool-capable move and the long-context profile are later slices.
//! - [`SystemParts`]: chat.cjs's ordered `sysParts`, joined with `'\n\n'` into the system message.
//! - [`prepare`] / [`messages`]: the parts ahead of the replayed history and the new user message.
//!
//! The order of `sysParts`: account part, time part, then without a project the memory part; with
//! one the [`project`] parts (name, goal, instructions, memory part with the project's memories,
//! shared Code tasks, knowledge-file block, not-ready line, waiting line, coverage note), its
//! skills ([`project::ProjectTurn::skills`]) and the stored-only line; then [`Turn::trailing`]
//! (the frame and brain blocks) in both cases.
//!
//! Strings are JS strings held as `js_json::wtf16` text (a lone surrogate is one private-use
//! character). Everything is replayed byte for byte on Node's own output
//! (`tests/fixtures/node-prepare.v1.json`, written by `tests/record_prepare.cjs`, and
//! `tests/fixtures/node-project.v1.json`, written by `tests/record_project.cjs`).
//!
//! Named differences (recorded in the fixtures' `differences`, each checked as documented here):
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
//! - Code tasks: `forChat` sorts by `(b.updatedAt || 0) - (a.updatedAt || 0)`. The code store
//!   writes a number; an `updatedAt` that is a non-empty string, an array or an object sorts here
//!   as 0, where Node coerces it (`ToNumber`; a `NaN` leaves the order to V8's sort internals).
//!
//! - A project record (a name, goal, memory or file name) holding a character of
//!   U+10F800..U+10FFFF: `JSON.parse` reads it, `wtf16::parse` refuses it, so the caller cannot
//!   hand such a record here (it reads as no project record); the reader of the record (P2b)
//!   decides, and keeps such a turn in Node. `NotReady.name` and `Partial.name` must come from
//!   wtf16 text, never from lossy Rust text.
//!
//! Node behaviour kept as is: a `null` entry in `project.files` makes the stored-only line throw
//! (`f.attachment?.state` of `null`), so such a turn is [`PrepareError::Thrown`] as in Node, where
//! retrieval's skill check throws on it even earlier.
//!
//! Later slices: the retrieval itself and `partialSources` (P2b, which pass `filesBlock` and
//! `partialReads` in here); the history's skill scrub and the automatic skill selection itself
//! (its picks go to [`skills::parts`]); the brain block
//! ([`Turn::trailing`], after the frame block) and the tool gate's use of [`frame::gate_bias`];
//! image assets; and what happens to the wire once the route is known (off-box zone and history
//! filter, the hybrid route's read-file hint, vision, route notes).

pub mod account;
pub mod effort;
pub mod frame;
pub mod history;
pub mod js;
pub mod memory;
pub mod names;
pub mod project;
pub mod roles;
pub mod route;
pub mod router;
pub mod sampling;
pub mod shared;
pub mod skills;
pub mod sources;
pub mod system_one;
pub mod time;

pub use history::{Entry, ReplayOptions};
use js_json::JValue;
pub use project::ProjectTurn;

/// Node would throw a TypeError here (a value whose `String()` throws, a property read of `null`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thrown;

/// Why [`prepare`] built nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareError {
    /// The account settings or the project make Node throw ([`Thrown`]); an [`account::read`]
    /// record never does.
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
    ProjectName,
    ProjectGoal,
    ProjectInstructions,
    /// `shared-context.cjs` `forChat`.
    SharedCode,
    /// The knowledge-file excerpts block.
    Knowledge,
    NotReady,
    Waiting,
    Coverage,
    StoredOnly,
    /// A part a later slice adds, named by the chat.cjs block it ports (a frame block, the brain
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
/// Preconditions (anything else is a later slice, and calling this for it gives a system message
/// Node would not send):
/// - `project` is what chat.cjs resolves from `body.projectId` and a free chat's diary-extras
///   context project (`diaryExtras.chatProjectId`): `None` when that is null. A free chat with
///   attachments has a hidden shadow project (`cowork-chat-context-*`), so it is a project turn.
///   [`ProjectTurn`] lists what a project turn must not hold yet (image assets); its skills come
///   from [`skills::parts`] in [`ProjectTurn::skills`].
/// - Chat framing: chat.cjs ~745-765 pushes the confirmed frame's block and the linked brain
///   block after everything else. The frame block is [`frame::frame_part`] of
///   [`frame::chat_frame`], passed in `trailing`; the brain block (`features.brainContext`, off by
///   default) is not ported yet, so a turn with it on is not covered.
/// - No Diary space and no `compactOnly` side effects beyond the replay and the parts
///   (`message: None`).
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
    /// The project, its tasks and what retrieval and the source jobs found (`None`: no project).
    pub project: Option<ProjectTurn<'a>>,
    /// Parts pushed after everything above (the frame and brain blocks), in order.
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

/// The system parts in chat.cjs's order (see the crate doc), the system message and the wire of a
/// turn that meets [`Turn`]'s preconditions.
pub fn prepare(turn: &Turn<'_>) -> Result<Prepared, PrepareError> {
    let opts = ReplayOptions {
        carry_orphan_tool: turn.carry_orphan_tool,
        off_box: false,
        known_tool: turn.known_tool,
    };
    let mut history = history::replay_history(turn.history, turn.history_cap, turn.message, &opts);
    let mut parts = SystemParts::default();
    if let Some(p) = account::part(turn.instructions).map_err(|_| PrepareError::Thrown)? {
        parts.push(PartKind::Account, p);
    }
    let time_part =
        time::part(turn.now_ms, turn.time_zone, true).ok_or(PrepareError::TimeOutOfRange)?;
    parts.push(PartKind::Time, time_part.clone());
    // `memoryPart` holds the project's memories too (`project?.memories`; undefined without one).
    let project_memories = turn
        .project
        .as_ref()
        .map_or(&JValue::Undefined, |p| p.project.get("memories"));
    let memory_part = memory::part(turn.memory, project_memories);
    match &turn.project {
        None => {
            if let Some(p) = memory_part {
                parts.push(PartKind::Memory, p);
            }
        }
        Some(p) => {
            let compact = turn.message.is_none();
            for (kind, text) in
                project::parts(p, memory_part, compact).map_err(|_| PrepareError::Thrown)?
            {
                parts.push(kind, text);
            }
            let block = p
                .files_block
                .filter(|b| project::retrieval_ran(p.project) && !b.is_empty());
            if let (Some(block), Some(message), true) = (
                block,
                turn.message,
                project::is_chat_attachments(p.project).map_err(|_| PrepareError::Thrown)?,
            ) {
                project::attach_files_block(&mut history, block, message);
            }
            for (kind, text) in p.skills {
                parts.push(*kind, text.clone());
            }
            if let Some(line) =
                sources::stored_only(p.project.get("files")).map_err(|_| PrepareError::Thrown)?
            {
                parts.push(PartKind::StoredOnly, line);
            }
        }
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
