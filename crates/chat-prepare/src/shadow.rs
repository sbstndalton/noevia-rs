//! The native preparation beside core's, in shadow (track R8, slice P7a): for a turn core handed to
//! the native executor, the front builds the frame's `messages` itself from the same request and
//! the account's own files, and says whether they are core's, byte for byte. Nothing uses the
//! native messages yet; the verdict is only logged (the front's `NOEVIA_CHAT_PREPARE` switch).
//!
//! Covered: a free-chat turn (no project, so the system message is the account, time and memory
//! parts) whose chat has no confirmed frame in the free list, whose history needs no toolbox
//! catalogue to replay (or the caller has the catalogue), on the local default provider (core
//! hands off nothing else). Everything else is a [`Verdict::Skip`] with its reason, read from core's
//! frame and the request, before anything is built: a project turn (`turn.projectId`, which is
//! also a free chat's attachments project), a compaction, instruction skills, a confirmed frame
//! (its block, and the brain block behind its flag, are not built here).
//!
//! The clock is core's `startedAt` (the time part states only the day, so the two differ only for a
//! turn that started within milliseconds of midnight in the zone); the zone is the caller's
//! (`serverTimeZone()`: core's process zone, which the front reads the same way).
//!
//! Recorded in `tests/fixtures/node-shadow.v1.json`, written by `tests/record_shadow.cjs` (whole
//! free-chat turns through `createChatHandler` with the hand-off asked for).

use std::sync::atomic::{AtomicBool, Ordering};

use js_json::{wtf16, JValue};

use crate::frame::{self, FrameTurn, Lookup};
use crate::{account, js, memory, prepare, PrepareError, Turn};

/// What the shadow is built from.
pub struct ShadowTurn<'a> {
    /// The client's request body, as core read it.
    pub body: &'a JValue,
    /// Core's `prepared` frame for that request.
    pub frame: &'a JValue,
    /// The bytes of the account's `account-instructions.json` and `account-memory.json` (`None`:
    /// no file).
    pub instructions: Option<&'a [u8]>,
    pub memory: Option<&'a [u8]>,
    /// The account's free chat list (`free-chats.json`); `None` when it could not be read (Node
    /// fails open: no frame).
    pub free_chats: Option<&'a JValue>,
    /// `serverTimeZone()`.
    pub time_zone: &'a str,
    /// `HISTORY_CAP`.
    pub history_cap: usize,
    /// The history replay's `knownTool` and `carryOrphanTool` over the server's toolboxes, when the
    /// caller has them; without them a history that asks either is skipped.
    pub catalogue: Option<Catalogue<'a>>,
}

/// One of the replay's questions about a tool name.
pub type ToolQuestion<'a> = &'a (dyn Fn(&str) -> bool + Sync);

/// The replay's two questions about the server's toolboxes (see [`crate::ReplayOptions`]).
pub struct Catalogue<'a> {
    pub known_tool: ToolQuestion<'a>,
    pub carry_orphan_tool: ToolQuestion<'a>,
}

/// Where the native messages first differ from core's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Diff {
    /// The message index (`messages.len()` of the shorter list when one is a prefix of the other).
    pub message: usize,
    /// The first UTF-16 unit at which the two messages' JSON texts differ.
    pub unit: usize,
    /// How many messages each side has.
    pub ours: usize,
    pub core: usize,
}

/// The shadow's answer. Nothing in it holds the turn's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The native messages are core's.
    Match,
    /// Not covered yet, with the reason.
    Skip(&'static str),
    /// The native preparation refused what core prepared: [`PrepareError`] as a reason.
    Failed(&'static str),
    Diff(Diff),
}

impl Verdict {
    /// The log words: `match`, `skip <reason>`, `failed <reason>`, `diff message=<i> unit=<u> ours=<n> core=<n>`.
    pub fn line(&self) -> String {
        match self {
            Verdict::Match => "match".into(),
            Verdict::Skip(r) => format!("skip {r}"),
            Verdict::Failed(r) => format!("failed {r}"),
            Verdict::Diff(d) => format!(
                "diff message={} unit={} ours={} core={}",
                d.message, d.unit, d.ours, d.core
            ),
        }
    }
}

/// Why a turn is not covered, read from core's frame and the request (`None`: covered).
fn uncovered(body: &JValue, frame: &JValue, free_chats: Option<&JValue>) -> Option<&'static str> {
    let turn = frame.get("turn");
    if !turn.is_object() || !matches!(frame.get("messages"), JValue::Arr(_)) {
        return Some("frame");
    }
    if !turn.get("projectId").is_nullish() || body.get("projectId").truthy() {
        return Some("project");
    }
    if turn.get("compactOnly") == &JValue::Bool(true) || body.get("compactOnly").truthy() {
        return Some("compact");
    }
    if !frame.get("skills").is_nullish() {
        return Some("skills");
    }
    let space = body.get("spaceId");
    if space.as_str().is_some_and(|s| s.starts_with("diary")) {
        return Some("diary");
    }
    let Some(message) = body.get("message").as_str() else {
        return Some("message");
    };
    if turn.get("message").as_str() != Some(message) {
        return Some("message");
    }
    // chat.cjs reads the stored frame with the flag; one that is there (confirmed) gives the frame
    // block when the flag is on, and the brain block behind its own flag: not built here.
    let chat_id = body.get("chatId").as_str();
    let stored = frame::chat_frame(&FrameTurn {
        compact_only: false,
        diary_space: space.as_str() == Some("diary"),
        lookup: free_chats.map(|free| Lookup {
            enabled: true,
            chat_id,
            project_id: None,
            project: &JValue::Null,
            free_chats: free,
        }),
    });
    if stored.is_some() {
        return Some("frame");
    }
    None
}

/// The first UTF-16 unit at which `a` and `b` differ (the shorter one's length when it is a prefix).
fn first_difference(a: &str, b: &str) -> usize {
    let (a, b) = (js::to_units(a), js::to_units(b));
    a.iter()
        .zip(&b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()))
}

/// Builds the messages natively and compares them with core's frame.
pub fn compare(t: &ShadowTurn<'_>) -> Verdict {
    if let Some(reason) = uncovered(t.body, t.frame, t.free_chats) {
        return Verdict::Skip(reason);
    }
    let Some(now) = t.frame.get("startedAt").as_num() else {
        return Verdict::Skip("frame");
    };
    let asked = AtomicBool::new(false);
    let unknown = |_: &str| {
        asked.store(true, Ordering::Relaxed);
        false
    };
    let (known_tool, carry_orphan_tool): (ToolQuestion<'_>, ToolQuestion<'_>) = match &t.catalogue {
        Some(c) => (c.known_tool, c.carry_orphan_tool),
        None => (&unknown, &unknown),
    };
    let instructions = account::read(t.instructions);
    let memory = memory::read(t.memory);
    let turn = Turn {
        instructions: &instructions,
        memory: &memory,
        now_ms: now as i64,
        time_zone: t.time_zone,
        history: t.body.get("history"),
        history_cap: t.history_cap,
        message: t.body.get("message").as_str(),
        carry_orphan_tool,
        known_tool,
        project: None,
        trailing: &[],
    };
    let prepared = match prepare(&turn) {
        Ok(p) => p,
        Err(PrepareError::Thrown) => return Verdict::Failed("thrown"),
        Err(PrepareError::TimeOutOfRange) => return Verdict::Failed("time"),
    };
    if asked.load(Ordering::Relaxed) {
        return Verdict::Skip("tool-history");
    }
    let JValue::Arr(core) = t.frame.get("messages") else {
        return Verdict::Skip("frame");
    };
    let ours = &prepared.messages;
    let text = |v: &JValue| wtf16::stringify(v).unwrap_or_default();
    for (i, (a, b)) in ours.iter().zip(core).enumerate() {
        let (a, b) = (text(a), text(b));
        if a != b {
            return Verdict::Diff(Diff {
                message: i,
                unit: first_difference(&a, &b),
                ours: ours.len(),
                core: core.len(),
            });
        }
    }
    if ours.len() != core.len() {
        return Verdict::Diff(Diff {
            message: ours.len().min(core.len()),
            unit: 0,
            ours: ours.len(),
            core: core.len(),
        });
    }
    Verdict::Match
}
