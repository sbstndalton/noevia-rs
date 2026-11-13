//! Chat framing's steering (#739): noevia-core's `server/chat-frame-steering.cjs` and its call
//! site in `server/chat.cjs` (~745-755). A confirmed frame stored in the signed-in user's own
//! lists gives the "Chat frame" system-prompt block ([`frame_block`]) and a bias for the tool gate
//! ([`gate_bias`]); flag off, no frame or an unconfirmed frame give neither, and the turn is what
//! it was without framing.
//!
//! - [`stored_frame`]: `storedFrame({ enabled, chatId, projectId, project, freeChats })`. The frame
//!   is read only from the lists the caller loaded (the chat's project's `chats`, else the free
//!   list), never from the request body. It is `chat-framing.cjs` `normalizeFrame`, reused from
//!   `server_projects::frame::normalize` (the recording replays `normalizeFrame` itself through it).
//! - [`frame_block`]: `frameBlock(frame)`: the kind's answer shape, the project scope line when
//!   the chat lives in the project its frame names, and the tags as framed untrusted data
//!   (`frameUntrusted('chat tags', '', ...)`), at most 6 tags of 32 units after [`safe_tag`]; tags
//!   are dropped from the end until the block is at most 400 units, then it is cut to 400.
//! - [`gate_bias`]: `gateBias(frame)`, what the kind asks of the tool gate (a later slice uses it).
//! - [`chat_frame`] / [`frame_part`]: chat.cjs's rules around them (not for `compactOnly`, not in
//!   the Diary space, any throw while reading the lists is no frame) and the part a caller pushes
//!   into [`crate::Turn::trailing`] ahead of the brain block (not ported here).
//!
//! Strings are JS strings held as `js_json::wtf16` text: lengths and cuts count UTF-16 units, and
//! a tag cut through a surrogate pair keeps its high surrogate as JS does. [`safe_tag`] is
//! `String(t || '').normalize('NFKC').replace(/[^\p{L}\p{N}_-]+/gu, '').slice(0, 32)`: NFKC is
//! `prompt_framing::js::nfkc` (ICU4X, lone surrogates kept as they are), and `\p{L}`/`\p{N}` are
//! ICU4X's General_Category groups L and N (not `char::is_alphanumeric`, whose Alphabetic includes
//! marks). Both are the Unicode release Node's ICU states (recorded as `unicode` in the fixture);
//! a sweep of every code point through `frameBlock` (`tests/fixtures/node-frame.v1.json`, written
//! by `tests/record_frame.cjs`) is replayed against them. No differences were found: the
//! fixture's `differences` is empty.

use crate::{js, PartKind};
use icu_properties::props::{GeneralCategory, GeneralCategoryGroup};
use icu_properties::CodePointMapData;
use js_json::{wtf16, JValue};

/// `MAX_BLOCK_CHARS`.
pub const MAX_BLOCK_CHARS: usize = 400;
/// `MAX_TAGS` (steering's own, after `normalizeFrame`'s 20).
pub const MAX_TAGS: usize = 6;
/// `MAX_TAG_CHARS` (steering's own, after `normalizeFrame`'s 64).
pub const MAX_TAG_CHARS: usize = 32;

/// The `sysParts` entry the frame block is (chat.cjs pushes it after the stored-only line).
pub const FRAME_PART: PartKind = PartKind::Other("frameBlock");

/// `chat-framing.cjs` `KINDS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Search,
    Action,
    Idea,
    Question,
    Code,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Search,
        Kind::Action,
        Kind::Idea,
        Kind::Question,
        Kind::Code,
    ];

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Search => "search",
            Kind::Action => "action",
            Kind::Idea => "idea",
            Kind::Question => "question",
            Kind::Code => "code",
        }
    }

    /// `SHAPES[kind]`.
    pub fn shape(self) -> &'static str {
        match self {
            Kind::Search => "a lookup. Lead with the direct answer, then name the sources you used. Say so if nothing reliable was found.",
            Kind::Action => "a request to get something done. Use the offered tools for it; every change still waits for approval. Report what was done.",
            Kind::Idea => "brainstorming. Offer a few distinct options with brief trade-offs; do not run tools unless asked.",
            Kind::Question => "a question. Explain clearly and concisely, starting with the answer.",
            Kind::Code => "programming. Answer with working code in fenced blocks and a short explanation.",
        }
    }
}

/// `chat-framing.cjs` `SOURCES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Suggested,
    User,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Suggested => "suggested",
            Source::User => "user",
        }
    }
}

/// A frame in `normalizeFrame`'s canonical shape plus `storedFrame`'s `inProject`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub project_id: Option<String>,
    pub kind: Kind,
    pub tags: Vec<String>,
    pub links: Vec<String>,
    pub confirmed: bool,
    pub source: Source,
    /// The scope line names the chat's project: the chat is in that project's list, and the
    /// request's project, the project record's id and the frame's `projectId` are all the same.
    pub in_project: bool,
}

impl Frame {
    /// A frame object of exactly the canonical shape (`normalizeFrame`'s keys, string tags and
    /// links), with `inProject === true` read as [`Frame::in_project`]; `None` for anything else.
    pub fn from_value(v: &JValue) -> Option<Frame> {
        let strings = |key: &str| match v.get(key) {
            JValue::Arr(items) => items
                .iter()
                .map(|s| s.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>(),
            _ => None,
        };
        let project_id = match v.get("projectId") {
            JValue::Null => None,
            JValue::Str(s) => Some(s.clone()),
            _ => return None,
        };
        let source = match v.get("source").as_str()? {
            "suggested" => Source::Suggested,
            "user" => Source::User,
            _ => return None,
        };
        Some(Frame {
            project_id,
            kind: Kind::parse(v.get("kind").as_str()?)?,
            tags: strings("tags")?,
            links: strings("links")?,
            confirmed: v.get("confirmed").as_bool()?,
            source,
            in_project: v.get("inProject") == &JValue::Bool(true),
        })
    }

    /// `{ ...frame, inProject }` as `storedFrame` returns it (key order included).
    pub fn to_value(&self) -> JValue {
        let strings =
            |list: &[String]| JValue::Arr(list.iter().cloned().map(JValue::Str).collect());
        JValue::obj([
            (
                "projectId",
                self.project_id.clone().map_or(JValue::Null, JValue::Str),
            ),
            ("kind", JValue::from(self.kind.as_str())),
            ("tags", strings(&self.tags)),
            ("links", strings(&self.links)),
            ("confirmed", JValue::Bool(self.confirmed)),
            ("source", JValue::from(self.source.as_str())),
            ("inProject", JValue::Bool(self.in_project)),
        ])
    }
}

/// `storedFrame`'s argument. The values are what chat.cjs passes: `chatFramingEnabled() === true`,
/// `body.chatId` when it is a string, `projectId` (`body.projectId || null`, or a free chat's
/// context project) when it is a string (a non-string can never equal a frame's `projectId`), the
/// project record freshly read by `getProject(project.id)` (`Null` without a project) and the
/// user's free-chat list (`freeChats()`).
#[derive(Debug, Clone, Copy)]
pub struct Lookup<'a> {
    pub enabled: bool,
    pub chat_id: Option<&'a str>,
    pub project_id: Option<&'a str>,
    pub project: &'a JValue,
    pub free_chats: &'a JValue,
}

/// `(Array.isArray(list) ? list : []).find((c) => c && c.id === chatId)`.
fn find<'v>(list: &'v JValue, chat_id: &str) -> Option<&'v JValue> {
    match list {
        JValue::Arr(items) => items
            .iter()
            .find(|c| c.truthy() && c.get("id").as_str() == Some(chat_id)),
        _ => None,
    }
}

/// `storedFrame(input)`: the confirmed frame of the chat as stored in the user's own lists, or
/// `None`. The project's list is searched first; a chat found there is never looked up in the
/// free list, even when its entry has no frame.
pub fn stored_frame(input: &Lookup<'_>) -> Option<Frame> {
    let chat_id = input.chat_id.filter(|c| input.enabled && !c.is_empty())?;
    let in_list = if input.project.truthy() {
        find(input.project.get("chats"), chat_id)
    } else {
        None
    };
    let meta = in_list.or_else(|| find(input.free_chats, chat_id));
    let normalized =
        server_projects::frame::normalize(meta.map_or(&JValue::Undefined, |m| m.get("frame")))?;
    let frame = Frame::from_value(&normalized)?;
    if !frame.confirmed {
        return None;
    }
    let in_project = in_list.is_some()
        && input.project_id.is_some_and(|p| {
            !p.is_empty()
                && input.project.get("id").as_str() == Some(p)
                && frame.project_id.as_deref() == Some(p)
        });
    Some(Frame {
        in_project,
        ..frame
    })
}

/// A code point `/[\p{L}\p{N}_-]/u` matches. A lone surrogate (General_Category Cs) never does.
fn tag_char(c: char) -> bool {
    if wtf16::lone_unit(c).is_some() {
        return false;
    }
    if c == '_' || c == '-' {
        return true;
    }
    let gc: GeneralCategory = CodePointMapData::<GeneralCategory>::new().get(c);
    GeneralCategoryGroup::Letter.contains(gc) || GeneralCategoryGroup::Number.contains(gc)
}

/// `safeTag(t)` of a string tag: NFKC, then everything but letters, numbers, `_` and `-` removed,
/// then the first 32 units.
pub fn safe_tag(t: &str) -> String {
    let normalized = js::from_units(&prompt_framing::js::nfkc(&js::to_units(t)));
    let kept: String = normalized.chars().filter(|&c| tag_char(c)).collect();
    js::slice(&kept, MAX_TAG_CHARS)
}

/// `frameBlock(frame)`: the "Chat frame" block of a confirmed frame, or `''`.
pub fn frame_block(frame: Option<&Frame>) -> String {
    let Some(frame) = frame.filter(|f| f.confirmed) else {
        return String::new();
    };
    let head = format!(
        "Chat frame (set by the user): this chat is {}",
        frame.kind.shape()
    );
    let scope = if frame.in_project {
        " Stay within this project and its sources."
    } else {
        ""
    };
    // `[...new Set(tags.map(safeTag).filter(Boolean))].slice(0, MAX_TAGS)`.
    let mut tags: Vec<String> = Vec::new();
    for tag in frame.tags.iter().map(|t| safe_tag(t)) {
        if tags.len() == MAX_TAGS {
            break;
        }
        if !tag.is_empty() && !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    let build = |tags: &[String]| {
        if tags.is_empty() {
            format!("{head}{scope}")
        } else {
            format!(
                "{head}{scope} Stay within the topics tagged below.\n{}",
                js::frame_untrusted("chat tags", "", &tags.join(", "))
            )
        }
    };
    let mut block = build(&tags);
    while js::len(&block) > MAX_BLOCK_CHARS && tags.pop().is_some() {
        block = build(&tags);
    }
    js::slice(&block, MAX_BLOCK_CHARS)
}

/// `gateBias(frame)`: what a confirmed frame's kind asks of the tool gate. It only narrows or
/// prefers among the tools a request already offers; it never adds a tool, picks a write or
/// bypasses an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateBias {
    /// Gate rule kinds whose tools go first among the Stage 2 options, when offered.
    pub prefer: &'static [&'static str],
    /// Appended to the Stage 2 question.
    pub hint: Option<&'static str>,
    /// The gate forces nothing for this message.
    pub no_force: bool,
}

impl GateBias {
    /// The object `gateBias` returns (only the keys it sets, in its order).
    pub fn to_value(&self) -> JValue {
        let mut out: Vec<(String, JValue)> = Vec::new();
        if !self.prefer.is_empty() {
            let prefer = self.prefer.iter().map(|&k| JValue::from(k)).collect();
            out.push(("prefer".into(), JValue::Arr(prefer)));
        }
        if let Some(hint) = self.hint {
            out.push(("hint".into(), JValue::from(hint)));
        }
        if self.no_force {
            out.push(("noForce".into(), JValue::Bool(true)));
        }
        JValue::Obj(out)
    }
}

/// `gateBias(frame)`; `None` for no bias (no confirmed frame, or a question or code frame).
pub fn gate_bias(frame: Option<&Frame>) -> Option<GateBias> {
    let bias = |prefer: &'static [&'static str], hint: Option<&'static str>, no_force: bool| {
        Some(GateBias {
            prefer,
            hint,
            no_force,
        })
    };
    match frame.filter(|f| f.confirmed)?.kind {
        Kind::Search => bias(
            &["search", "drive"],
            Some("The user framed this chat as a lookup."),
            false,
        ),
        Kind::Action => bias(
            &[],
            Some("The user framed this chat as a request to get something done, so a tool is expected."),
            false,
        ),
        Kind::Idea => bias(&[], None, true),
        Kind::Question | Kind::Code => None,
    }
}

/// What chat.cjs knows when it looks for the frame of an ordinary space or project turn.
#[derive(Debug, Clone, Copy)]
pub struct FrameTurn<'a> {
    /// `body.compactOnly` (truthy): a compaction gets no frame.
    pub compact_only: bool,
    /// `spaceId === 'diary'`: the Diary space gets no frame.
    pub diary_space: bool,
    /// The lookup, or `None` when chat.cjs's `try` would catch a throw while building it (the
    /// flag read, `getProject` or `freeChats()` failed): fail open, no frame.
    pub lookup: Option<Lookup<'a>>,
}

/// chat.cjs's `chatFrame`: [`stored_frame`] unless the turn is a compaction, in the Diary space or
/// its lists could not be read.
pub fn chat_frame(turn: &FrameTurn<'_>) -> Option<Frame> {
    if turn.compact_only || turn.diary_space {
        return None;
    }
    stored_frame(turn.lookup.as_ref()?)
}

/// `if (frameBlock) sysParts.push(frameBlock)`: the part for [`crate::Turn::trailing`], first
/// (the brain block, a later slice, follows it).
pub fn frame_part(frame: Option<&Frame>) -> Option<(PartKind, String)> {
    let block = frame_block(frame);
    (!block.is_empty()).then_some((FRAME_PART, block))
}
