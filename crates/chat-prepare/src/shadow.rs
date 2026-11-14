//! The native preparation beside core's, in shadow (track R8, slices P7a and P7b): for a turn core
//! handed to the native executor, the front builds the frame's `messages`, `chat` and `turn` itself
//! from the same request and the account's own files, and says whether they are core's, byte for
//! byte. Nothing uses the
//! native messages yet; the verdict is only logged (the front's `NOEVIA_CHAT_PREPARE` switch).
//!
//! Covered: a free-chat turn (no project, so the system message is the account, time and memory
//! parts) whose chat has no confirmed frame in the free list, whose history needs no toolbox
//! catalogue to replay (or the caller has the catalogue), on the local default provider (core
//! hands off nothing else). A confirmed chat frame in the free list gives its block when chat
//! framing is on ([`crate::frame::frame_part`]). Everything else is a [`Verdict::Skip`] with its
//! reason, read from core's frame and the request, before anything is built: a project turn
//! (`turn.projectId`, which is also a free chat's attachments project), a compaction, instruction
//! skills, a turn routed to the Code role (its route note), a stored frame when the flags are
//! unknown or the brain context may be on (its block is not ported), a free list that could not
//! be read.
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
use crate::{account, js, memory, prepare, PartKind, PrepareError, ProjectTurn, Turn};

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
    /// The account's free chat list (`free-chats.json`); `None` when it could not be read (the
    /// turn is skipped unless chat framing is off: Node's copy in memory may hold a frame).
    pub free_chats: Option<&'a JValue>,
    /// The stored record of `body.projectId` (`getProject(id)`, the account's `projects.json`), when
    /// the request names one and the caller found it; a project turn without it is skipped.
    pub project: Option<&'a JValue>,
    /// `serverTimeZone()`.
    pub time_zone: &'a str,
    /// `features.enabled('chatFraming')` and `brainContext.enabled()` (`brainContext` and
    /// `chatFraming`), `None` when the caller could not read them: a turn whose chat has a stored
    /// frame is then skipped.
    pub chat_framing: Option<bool>,
    pub brain_context: Option<bool>,
    /// `HISTORY_CAP`.
    pub history_cap: usize,
    /// `!!chatTitles` (the chat title store is wired) and `!!chatWorkspace` (the turn has the
    /// account's workspace): both true on a server.
    pub titles: bool,
    pub workspace: bool,
    /// The settings rows chat.cjs reads for the effort and the sampling presets, when the caller
    /// could read them; without them those three fields are not compared.
    pub settings: Option<Settings<'a>>,
    /// The history replay's `knownTool` and `carryOrphanTool` over the server's toolboxes, when the
    /// caller has them; without them a history that asks either is skipped.
    pub catalogue: Option<Catalogue<'a>>,
}

/// The settings table's `reasoning_effort_default` and `auto_sampling_presets_enabled` values
/// (`undefined`: no row; `null`: SQL NULL), see [`crate::route::RouteTurn`].
#[derive(Debug, Clone, Copy)]
pub struct Settings<'a> {
    pub effort_default: &'a JValue,
    pub sampling_presets: &'a JValue,
}

/// One of the replay's questions about a tool name.
pub type ToolQuestion<'a> = &'a (dyn Fn(&str) -> bool + Sync);

/// The replay's two questions about the server's toolboxes (see [`crate::ReplayOptions`]).
pub struct Catalogue<'a> {
    pub known_tool: ToolQuestion<'a>,
    pub carry_orphan_tool: ToolQuestion<'a>,
}

/// Where the native frame first differs from core's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Diff {
    /// The frame field: `messages`, `chat`, `turn`, `effort`, `thinkingOff` or `sampling`.
    pub field: &'static str,
    /// The message index (`messages.len()` of the shorter list when one is a prefix of the other;
    /// 0 for `chat` and `turn`).
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
    /// The log words: `match`, `skip <reason>`, `failed <reason>`,
    /// `diff field=<messages|chat|turn> message=<i> unit=<u> ours=<n> core=<n>`.
    pub fn line(&self) -> String {
        match self {
            Verdict::Match => "match".into(),
            Verdict::Skip(r) => format!("skip {r}"),
            Verdict::Failed(r) => format!("failed {r}"),
            Verdict::Diff(d) => format!(
                "diff field={} message={} unit={} ours={} core={}",
                d.field, d.message, d.unit, d.ours, d.core
            ),
        }
    }
}

/// Why a turn is not covered, read from core's frame and the request, or the chat frame block it
/// carries (`Ok(None)`: none).
fn covered(t: &ShadowTurn<'_>) -> Result<Option<(PartKind, String)>, &'static str> {
    let (body, frame) = (t.body, t.frame);
    let turn = frame.get("turn");
    if !turn.is_object() || !matches!(frame.get("messages"), JValue::Arr(_)) {
        return Err("frame");
    }
    if turn.get("compactOnly") == &JValue::Bool(true) || body.get("compactOnly").truthy() {
        return Err("compact");
    }
    let project = project_of(t)?;
    match project {
        // P7c: a project with no skills (frame.skills as chat.cjs builds it for none).
        Some(_) if text(frame.get("skills")) != NO_SKILLS => return Err("skills"),
        None if !frame.get("skills").is_nullish() => return Err("skills"),
        _ => {}
    }
    // An Automatic turn routed to the Code role carries chat.cjs's CODE_ROUTE_NOTE (not built here).
    if frame.get("role").as_str() == Some("code") {
        return Err("code-route");
    }
    let space = body.get("spaceId");
    if space.as_str().is_some_and(|s| s.starts_with("diary")) {
        return Err("diary");
    }
    // `spaceId?.startsWith(...)` throws for a truthy value that is not text (core refuses it first).
    if !space.is_nullish() && space.as_str().is_none() {
        return Err("space");
    }
    let Some(message) = body.get("message").as_str() else {
        return Err("message");
    };
    if turn.get("message").as_str() != Some(message) {
        return Err("message");
    }
    // chat.cjs reads the stored frame only with chat framing on; a confirmed one gives the frame
    // block, and the brain block when brainContext is on too (not built here).
    let chat_id = body.get("chatId").as_str();
    // Node reads its list from memory; a list the caller could not read may hold a frame.
    if t.free_chats.is_none() && t.chat_framing != Some(false) {
        return Err("free-list");
    }
    let stored = frame::chat_frame(&FrameTurn {
        compact_only: false,
        diary_space: space.as_str() == Some("diary"),
        lookup: t.free_chats.map(|free| Lookup {
            enabled: t.chat_framing != Some(false),
            chat_id,
            project_id: project.and_then(|p| p.get("id").as_str()),
            project: project.unwrap_or(&JValue::Null),
            free_chats: free,
        }),
    });
    match (stored, t.chat_framing, t.brain_context) {
        (None, _, _) => Ok(None),
        (Some(f), Some(true), Some(false)) => Ok(frame::frame_part(Some(&f))),
        _ => Err("frame"),
    }
}

/// `frame.skills` of a project turn with no instruction skills.
const NO_SKILLS: &str =
    r#"{"loaded":[],"enabled":[],"revokedInHistory":[],"imageWarning":"","coverageWarning":""}"#;

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_default()
}

/// The turn's project (P7c), or why it is not covered: core's frame names the project the turn
/// ran in (`turn.projectId`); covered is the request's own stored project (not a free chat's
/// attachments project) with no knowledge files (no retrieval, no source waits or notes), no
/// sharing into Chat (no Code tasks part), no attached images (no vision step).
fn project_of<'a>(t: &ShadowTurn<'a>) -> Result<Option<&'a JValue>, &'static str> {
    let id = t.frame.get("turn").get("projectId");
    if id.is_nullish() {
        return if t.body.get("projectId").truthy() {
            Err("project")
        } else {
            Ok(None)
        };
    }
    if !t.body.get("projectId").truthy() {
        return Err("context-project");
    }
    let project = t
        .project
        .filter(|p| p.get("id") == id && t.body.get("projectId") == id)
        .ok_or("project-store")?;
    if matches!(project.get("files"), JValue::Arr(f) if !f.is_empty()) {
        return Err("files");
    }
    if crate::shared::read(project).0 {
        return Err("shared-context");
    }
    if matches!(project.get("assets"), JValue::Arr(a) if !a.is_empty()) {
        return Err("assets");
    }
    Ok(Some(project))
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
    let trailing: Vec<(PartKind, String)> = match covered(t) {
        Ok(part) => part.into_iter().collect(),
        Err(reason) => return Verdict::Skip(reason),
    };
    // Covered: the request's own stored project, or none.
    let project = project_of(t).ok().flatten();
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
        project: project.map(|project| ProjectTurn {
            project,
            code_tasks: &[],
            files_block: None,
            not_ready: &[],
            partial_reads: &[],
            skills: &[],
        }),
        trailing: &trailing,
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
                field: "messages",
                message: i,
                unit: first_difference(&a, &b),
                ours: ours.len(),
                core: core.len(),
            });
        }
    }
    if ours.len() == core.len() {
        let (chat, turn) = chat_and_turn(t, project);
        let mut fields = vec![("chat", chat), ("turn", turn)];
        if let Some(settings) = t.settings {
            fields.extend(route_fields(t, settings, project.unwrap_or(&JValue::Null)));
        }
        for (field, ours) in fields {
            let (a, b) = (text(&ours), text(t.frame.get(field)));
            if a != b {
                return Verdict::Diff(Diff {
                    field,
                    message: 0,
                    unit: first_difference(&a, &b),
                    ours: 1,
                    core: 1,
                });
            }
        }
    }
    if ours.len() != core.len() {
        return Verdict::Diff(Diff {
            field: "messages",
            message: ours.len().min(core.len()),
            unit: 0,
            ours: ours.len(),
            core: core.len(),
        });
    }
    Verdict::Match
}

fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// `a || b` of two JSON values.
fn or<'v>(a: &'v JValue, b: &'v JValue) -> &'v JValue {
    if a.truthy() {
        a
    } else {
        b
    }
}

/// rust-chat-native.cjs `preparedTurn`'s `chat` and `turn` of a covered free-chat turn, from what
/// chat.cjs passes it: `chatId = body.chatId || null`, `contextId` and `key` `chatId || spaceId`,
/// the free space's grant scope and holder, the fingerprints of the history's applied writes
/// (recent-writes.cjs, in first-seen order), the title facts (chat-title.cjs), and the turn's
/// message, space, record flag (#679), idle limit (createChatHandler's default 120 s) and its
/// empty sources (no retrieval without a project).
fn chat_and_turn(t: &ShadowTurn<'_>, project: Option<&JValue>) -> (JValue, JValue) {
    let body = t.body;
    let null = JValue::Null;
    // A project turn without a chat id gets a new one (chat.cjs: `p-${Date.now()}-${Math.random()...}`):
    // core's, read from its frame (the random part cannot be made again here).
    let chat_id = match project {
        Some(_) if !body.get("chatId").truthy() => t.frame.get("chat").get("id"),
        _ => or(body.get("chatId"), &null),
    };
    let project_id = project.map_or(&null, |p| p.get("id"));
    let space = body.get("spaceId");
    let space_text = space.as_str().filter(|s| !s.is_empty());
    let mut writes: Vec<JValue> = Vec::new();
    for h in crate::history::map_history(body.get("history"), t.history_cap) {
        if h.applied {
            let fp = chat_turn::gate::fingerprint(
                h.name.as_deref().unwrap_or(""),
                h.target.as_deref(),
                h.args.as_deref().unwrap_or(""),
            );
            let fp = JValue::from(fp);
            if !writes.contains(&fp) {
                writes.push(fp);
            }
        }
    }
    let message = body.get("message").as_str().unwrap_or_default();
    let history = body.get("history");
    let diary = space.as_str().is_some_and(|s| s.starts_with("diary"));
    let chat = obj(vec![
        ("id", chat_id.clone()),
        ("contextId", or(or(chat_id, space), &null).clone()),
        ("key", or(or(chat_id, space), &null).clone()),
        (
            "grantScope",
            JValue::from(match project_id.as_str() {
                // `${body.projectId ? 'project' : 'context'}:${project.id}`: a covered project is the request's.
                Some(id) => format!("project:{id}"),
                None => format!("space:{}", space_text.unwrap_or("free")),
            }),
        ),
        (
            "grantHolder",
            match project {
                Some(_) => obj(vec![
                    ("kind", JValue::from("project")),
                    ("id", project_id.clone()),
                ]),
                None => obj(vec![("kind", JValue::from("free"))]),
            },
        ),
        ("recentWrites", JValue::Arr(writes)),
        (
            "title",
            obj(vec![
                (
                    "chatId",
                    JValue::from(body.get("chatId").as_str().unwrap_or("")),
                ),
                ("listId", project_id.clone()),
                (
                    "firstMessage",
                    JValue::from(chat_turn::title::first_user_message(history, message)),
                ),
                (
                    "firstReply",
                    JValue::Bool(chat_turn::title::first_reply(history)),
                ),
                ("enabled", JValue::Bool(t.titles)),
            ]),
        ),
    ]);
    let record = body.get("chatId").as_str().is_some_and(|c| !c.is_empty())
        && !diary
        && !body.get("compactOnly").truthy()
        && t.workspace;
    let turn = obj(vec![
        ("message", JValue::from(message)),
        ("spaceId", or(space, &null).clone()),
        ("projectId", project_id.clone()),
        ("declineEndsReply", JValue::Bool(!diary)),
        ("record", JValue::Bool(record)),
        ("streamIdleMs", JValue::Num(STREAM_IDLE_MS)),
        ("compactOnly", JValue::Bool(false)),
        (
            "sources",
            obj(vec![
                ("reply", JValue::Arr(Vec::new())),
                ("injected", JValue::Arr(Vec::new())),
            ]),
        ),
    ]);
    (chat, turn)
}

/// chat.cjs `createChatHandler`'s `streamIdleMs` default (index.cjs passes none).
const STREAM_IDLE_MS: f64 = 120_000.0;

/// chat.cjs's effort, Fast thinking switch and sampling presets (~1405-1430, [`crate::route`]) of a
/// covered turn: the account default effort, lowered on an Automatic Fast route; thinking off on an
/// Automatic Fast route at the account default effort on the local engine; the presets from the
/// role and the message (no project, so no explicit sampling). Which role answers (`role`, `auto`)
/// is core's: the classification is a decision call; what follows from it is compared. The local
/// engine verdict is core's too (`provider.localEngine`: a handed-off turn is on the default
/// provider).
fn route_fields(
    t: &ShadowTurn<'_>,
    settings: Settings<'_>,
    project: &JValue,
) -> Vec<(&'static str, JValue)> {
    use crate::{effort, route, sampling};
    let frame = t.frame;
    let auto = frame.get("auto") == &JValue::Bool(true);
    let role = frame.get("role").as_str();
    // chat.cjs: the project's own effort is kept; the account default is lowered on a Fast route.
    let project_effort = project.get("reasoningEffort");
    let resolved = effort::resolve_effort(project, settings.effort_default);
    let chosen = if effort::valid_effort(project_effort) {
        resolved
    } else {
        effort::effort_for_route(resolved, role.filter(|_| auto))
    };
    let thinking_off = effort::fast_route_thinking_off(auto, role, project_effort)
        && frame.get("provider").get("localEngine") == &JValue::Bool(true);
    let presets = sampling::select(
        role,
        t.body.get("message").as_str().unwrap_or_default(),
        project.get("sampling"),
        route::sampling_auto_enabled(settings.sampling_presets),
    );
    vec![
        ("effort", JValue::from(chosen.as_str())),
        ("thinkingOff", JValue::Bool(thinking_off)),
        ("sampling", presets.params_value()),
    ]
}
