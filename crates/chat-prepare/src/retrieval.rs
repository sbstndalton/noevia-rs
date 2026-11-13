//! A project turn's knowledge-file retrieval (slice P2b): what noevia-core's `server/chat.cjs`
//! computes around its two seams, the bounded wait for the project's file jobs (#1441,
//! `sourceJobs.settle`, chat.cjs ~469-485 at the pinned core) and the retrieval itself
//! (`rag.filesContext`, ~583-612), and what the rest of the turn reads of their answers: the meta
//! event's `sourcesNotReady`, the `sources` event (~1455), the executor's `injected-sources`
//! refusal input (~1704), and the prepared frame's `sources` and `skills.coverageWarning`
//! (~1736-1740).
//!
//! In chat.cjs's order:
//! 1. [`project_id`]: `body.projectId`, else a free chat's attachments shadow project
//!    (`diary-extras.cjs` `chatProjectId`, [`chat_project_id`]) when the store holds it.
//! 2. [`wait`]: whether the turn waits for the file jobs, and what it asks (`settle(workspace,
//!    projectId, { timeoutMs: 30000, signal })`). The wait is the caller's; its answer is
//!    [`Settled`]: aborted (the client left: the turn ends with nothing written) or the files that
//!    did not get ready, [`NotReady`] (the not-ready line and [`meta_not_ready`]).
//! 3. [`loads_project`]: whether `getProject(projectId)` is asked (the caller reads the record and
//!    answers 404 for a `body.projectId` it does not hold).
//! 4. [`multimodal`] (#1726/#1736) and [`request`]: what retrieval is asked: the project's id, the
//!    files it may read ([`readable`]: `instruction-skills.cjs` `sources`, the files that are
//!    neither skill candidates nor unreadable), the question (`body.message` as it is; chat.cjs
//!    derives no search query from the history here), the account, and the options (`attached`
//!    for a shadow project, `images` when multimodal retrieval is on). No request when the project
//!    has no files.
//! 5. [`retrieved`]: what chat.cjs keeps of the [`Answer`]: the block, and from the report
//!    (`onSources(placed, injected)`) the placed and injected entries, the files read, the reply's
//!    sources ([`build_sources`], `chat-sources.cjs`) and the partial reads ([`partial_sources`],
//!    `source-coverage.cjs`, #1513). rag.cjs calls the report inside a `try`, so a report that
//!    throws keeps what was set before the throw. A retrieval that throws (rag.cjs never does)
//!    fails the turn: [`RetrievalThrew`].
//! 6. [`sources_event`], [`injected_not_text`], [`frame_sources`] (`rust-chat-native.cjs`
//!    `injectedSources`, #1793) and [`coverage_warning`].
//!
//! The block, the not-ready list and the partial reads feed [`crate::ProjectTurn`]
//! (`files_block`, `not_ready`, `partial_reads`). The readable, waiting and stored-only sets of the
//! system message are [`crate::sources`]'s.
//!
//! Inputs are what chat.cjs holds: the request body (a `diary-extras` turn's after chat.cjs set its
//! `projectId` to [`DIARY_EXTRAS_PROJECT`]), and the project after `instruction-skills.cjs`
//! `snapshot`, each source whose text is in its own file (#1806) given its text in `content` when
//! the caller has it (a 64-hex `textRef`, a safe-integer `chars` that is its text's length as
//! source-text.cjs writes it, and no `content` reads as a missing text whose length is `chars`).
//! Strings are JS strings as `js_json::wtf16` holds them; lengths and cuts count UTF-16 units.
//!
//! Replayed byte for byte on `tests/fixtures/node-retrieval.v1.json` (written by
//! `tests/record_retrieval.cjs`, whole turns through `createChatHandler` with the two seams
//! stubbed). Named differences: none (the fixture's `differences` is empty).
//!
//! Later slices: the images multimodal retrieval finds (`onImages`, the asset plan and the
//! retrieved images; the executor refuses a turn with any, `retrieved-images`), and the
//! attribution after the reply (chat-turn's `sources`).

use crate::sources::{string_of, NotReady, Partial};
use crate::{js, skills, sources, Thrown};
use js_json::JValue;
use std::collections::HashMap;

/// `diary-extras.cjs` `PROJECT_ID`.
pub const DIARY_EXTRAS_PROJECT: &str = "cowork-diary-extras";
/// chat.cjs `sourceWaitMs` (its default).
pub const SOURCE_WAIT_MS: u64 = 30000;
/// `chat-sources.cjs` `MAX_SOURCES`.
pub const MAX_SOURCES: usize = 12;
/// `chat-sources.cjs` `ATTRIBUTE_MAX`.
pub const ATTRIBUTE_MAX: usize = 24000;
/// `source-coverage.cjs` `WHOLE_MAX`.
pub const WHOLE_MAX: i64 = 2400;
const SNIPPET_MAX: usize = 200;
const NAME_MAX: usize = 300;
const PAGE_MAX: u32 = 100_000;
const MAX_SAFE: f64 = 9_007_199_254_740_991.0;

/// `Array.isArray(v) ? v : []`.
pub fn items(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(list) => list,
        _ => &[],
    }
}

/// `String(v || '')`.
fn string_or_empty(v: &JValue) -> Result<String, Thrown> {
    if v.truthy() {
        string_of(v)
    } else {
        Ok(String::new())
    }
}

/// `a === b` of two values from different JSON trees (no object is the other).
fn strict_eq(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Undefined, JValue::Undefined) | (JValue::Null, JValue::Null) => true,
        (JValue::Bool(x), JValue::Bool(y)) => x == y,
        (JValue::Num(x), JValue::Num(y)) => x == y,
        (JValue::Str(x), JValue::Str(y)) => x == y,
        _ => false,
    }
}

/// `diary-extras.cjs` `chatProjectId(chatId)`: a free chat's attachments project id.
pub fn chat_project_id(chat_id: &JValue) -> Option<String> {
    let id = chat_id.as_str()?;
    let safe = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    (!id.is_empty() && id.len() <= 100 && id.chars().all(safe))
        .then(|| format!("{}{id}", crate::project::CHAT_CONTEXT_PREFIX))
}

/// chat.cjs's `projectId`: `body.projectId || null`, else in the free space a chat's attachments
/// project when `stored(id)` (`getProject`) holds it.
pub fn project_id(body: &JValue, stored: &dyn Fn(&str) -> bool) -> JValue {
    let id = body.get("projectId");
    if id.truthy() {
        return id.clone();
    }
    if body.get("spaceId").as_str() == Some("free") && body.get("chatId").truthy() {
        if let Some(context) = chat_project_id(body.get("chatId")).filter(|c| stored(c)) {
            return JValue::Str(context);
        }
    }
    JValue::Null
}

/// What the turn asks the file-job wait (`sourceJobs.settle`).
#[derive(Debug, Clone, PartialEq)]
pub struct Wait {
    pub project_id: JValue,
    pub timeout_ms: u64,
}

/// `projectId && spaceId !== 'diary' && !body.compactOnly && chatWorkspace`: the turn waits.
/// `workspace`: chat.cjs could read the account's workspace.
pub fn wait(project_id: &JValue, body: &JValue, workspace: bool) -> Option<Wait> {
    let asks = project_id.truthy()
        && body.get("spaceId").as_str() != Some("diary")
        && !body.get("compactOnly").truthy()
        && workspace;
    asks.then(|| Wait {
        project_id: project_id.clone(),
        timeout_ms: SOURCE_WAIT_MS,
    })
}

/// What the wait answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// The client left while it waited: chat.cjs returns, the turn ends with nothing written.
    Aborted,
    /// The attached files whose jobs did not finish (`notReady`).
    Done(Vec<NotReady>),
}

/// The meta event's `sourcesNotReady`: the list, or `undefined` when empty.
pub fn meta_not_ready(not_ready: &[NotReady]) -> JValue {
    if not_ready.is_empty() {
        return JValue::Undefined;
    }
    let row = |f: &NotReady| {
        let reason = if f.failed { "failed" } else { "timeout" };
        JValue::obj([
            ("name", JValue::from(f.name.as_str())),
            ("reason", JValue::from(reason)),
        ])
    };
    JValue::Arr(not_ready.iter().map(row).collect())
}

/// `projectId && spaceId !== 'diary'`: chat.cjs reads the project record.
pub fn loads_project(project_id: &JValue, body: &JValue) -> bool {
    project_id.truthy() && body.get("spaceId").as_str() != Some("diary")
}

/// `multimodal-rag.cjs` `diaryProject(projectId)`: the Diary's extras project or a Diary chat's
/// attachments project (`/^cowork-chat-context-diary[-_]/i`).
pub fn diary_project(id: &JValue) -> Result<bool, Thrown> {
    let id = string_or_empty(id)?;
    const DIARY_CONTEXT: &str = "cowork-chat-context-diary";
    let n = DIARY_CONTEXT.len();
    let context = id
        .get(..n)
        .is_some_and(|head| head.eq_ignore_ascii_case(DIARY_CONTEXT))
        && matches!(id.as_bytes().get(n), Some(b'-' | b'_'));
    Ok(id == DIARY_EXTRAS_PROJECT || context)
}

/// chat.cjs's `multimodal` (#1726, #1736): images are retrieved too for a project turn that is not
/// `compactOnly`, not in a `diary…` space and not a Diary project, when `enabled`
/// (`multimodalEnabled() === true`).
pub fn multimodal(project: Option<&JValue>, body: &JValue, enabled: bool) -> Result<bool, Thrown> {
    let Some(project) = project else {
        return Ok(false);
    };
    if body.get("compactOnly").truthy()
        || body
            .get("spaceId")
            .as_str()
            .is_some_and(|s| s.starts_with("diary"))
        || diary_project(project.get("id"))?
    {
        return Ok(false);
    }
    Ok(enabled)
}

/// `instruction-skills.cjs` `sources(project)`: the positions in `project.files` of the files that
/// are neither instruction-skill candidates nor unreadable, in order.
pub fn readable(project: &JValue) -> Result<Vec<usize>, Thrown> {
    let candidates: Vec<usize> = skills::list(project)?.iter().map(|s| s.at).collect();
    let mut out = Vec::new();
    for (at, file) in items(project.get("files")).iter().enumerate() {
        if !candidates.contains(&at) && !sources::is_unreadable(file)? {
            out.push(at);
        }
    }
    Ok(out)
}

/// What chat.cjs asks `rag.filesContext(projectId, readable, message, userId, onSources, options)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Request<'a> {
    /// `project.id`.
    pub project_id: &'a JValue,
    /// [`readable`]: positions in `project.files`.
    pub readable: Vec<usize>,
    /// `body.message`.
    pub query: &'a str,
    /// `currentWorkspace().userId`.
    pub user: &'a str,
    /// `options.attached`: the project is a free chat's attachments ([`crate::project::is_chat_attachments`]).
    pub attached: bool,
    /// `options.images` (with an `onImages` hook): [`multimodal`].
    pub images: bool,
}

/// The retrieval request of a project turn, or `None` when its `files` is not a non-empty list.
/// `multimodal` is [`multimodal`]'s answer.
pub fn request<'a>(
    project: &'a JValue,
    message: &'a str,
    user: &'a str,
    multimodal: bool,
) -> Result<Option<Request<'a>>, Thrown> {
    let attached = crate::project::is_chat_attachments(project)?;
    if !crate::project::retrieval_ran(project) {
        return Ok(None);
    }
    Ok(Some(Request {
        project_id: project.get("id"),
        readable: readable(project)?,
        query: message,
        user,
        attached,
        images: multimodal,
    }))
}

/// What retrieval answered.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// It threw (rag.cjs never does).
    Threw,
    Answered {
        /// The block it returned (`null`: none).
        block: Option<String>,
        /// What it reported through `onSources(placed, injected)`, if it called it.
        reported: Option<(JValue, JValue)>,
    },
}

/// A retrieval that threw: handleChat rejects and the turn fails before its system message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrievalThrew;

/// What chat.cjs keeps of the retrieval.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Retrieved {
    /// `filesBlock`.
    pub files_block: Option<String>,
    /// `replySources`: the `sources` event and the frame's `sources.reply`.
    pub reply_sources: Vec<JValue>,
    /// `partialReads` (#1513).
    pub partial_reads: Vec<Partial>,
    /// `placedEntries`, `injectedEntries`, `readableFiles` (#1793).
    pub placed: Vec<JValue>,
    pub injected: Vec<JValue>,
    pub readable: Vec<JValue>,
}

/// chat.cjs's state after the retrieval `request` (none: nothing was asked) answered.
pub fn retrieved(
    project: &JValue,
    request: Option<&Request<'_>>,
    answer: Answer,
) -> Result<Retrieved, RetrievalThrew> {
    let Some(request) = request else {
        return Ok(Retrieved::default());
    };
    let (block, reported) = match answer {
        Answer::Threw => return Err(RetrievalThrew),
        Answer::Answered { block, reported } => (block, reported),
    };
    let files = items(project.get("files"));
    let readable: Vec<JValue> = request
        .readable
        .iter()
        .filter_map(|at| files.get(*at).cloned())
        .collect();
    let mut out = match reported {
        Some((placed, injected)) => report(readable, &placed, &injected),
        None => Retrieved::default(),
    };
    out.files_block = block;
    Ok(out)
}

/// The `onSources` hook: each assignment in order; a throw (rag.cjs catches it) keeps the earlier
/// ones.
fn report(readable: Vec<JValue>, placed: &JValue, injected: &JValue) -> Retrieved {
    let mut out = Retrieved {
        placed: items(placed).to_vec(),
        injected: items(injected).to_vec(),
        readable,
        ..Retrieved::default()
    };
    let Ok(reply) = build_sources(items(placed), &out.readable) else {
        return out;
    };
    out.reply_sources = reply;
    let counted = if matches!(injected, JValue::Arr(_)) {
        injected
    } else {
        placed
    };
    if let JValue::Arr(counted) = counted {
        out.partial_reads = partial_sources(&out.readable, counted).unwrap_or_default();
    }
    out
}

/// `Math.round(x)`: the nearest integer, a tie toward +∞.
fn js_round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

/// `snippetOf(text)`: whitespace runs as one space, trimmed, at most 200 units (199 and `…`).
fn snippet_of(text: &JValue) -> Result<String, Thrown> {
    let mut flat = String::new();
    let mut space = false;
    for c in string_or_empty(text)?.chars() {
        if js::is_js_space(c) {
            if !space {
                flat.push(' ');
            }
            space = true;
        } else {
            flat.push(c);
            space = false;
        }
    }
    let flat = js::trim(&flat);
    Ok(if js::len(flat) > SNIPPET_MAX {
        let cut = js::slice(flat, SNIPPET_MAX - 1);
        format!("{}…", cut.trim_end_matches(js::is_js_space))
    } else {
        flat.to_string()
    })
}

/// `pageOf(body)`: a leading `[Page N]` marker (`/^\s*\[Page (\d{1,6})\](?:\n|$)/` over the
/// first 40 units) naming a page of 1..=100000.
fn page_of(body: &JValue) -> Result<Option<u32>, Thrown> {
    let head = js::slice(&string_or_empty(body)?, 40);
    let Some(rest) = head
        .trim_start_matches(js::is_js_space)
        .strip_prefix("[Page ")
    else {
        return Ok(None);
    };
    let n = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (digits, after) = rest.split_at(n);
    let closed = after
        .strip_prefix(']')
        .is_some_and(|tail| tail.is_empty() || tail.starts_with('\n'));
    if !(1..=6).contains(&n) || !closed {
        return Ok(None);
    }
    Ok(digits
        .parse::<u32>()
        .ok()
        .filter(|p| (1..=PAGE_MAX).contains(p)))
}

/// `buildSources`' `byName`: the readable files with a text name, the last of a name winning.
fn by_name(files: &[JValue]) -> Result<HashMap<&str, &JValue>, Thrown> {
    let mut out = HashMap::new();
    for f in files {
        if sources::is_unreadable(f)? || !f.truthy() {
            continue;
        }
        if let Some(name) = f.get("name").as_str() {
            out.insert(name, f);
        }
    }
    Ok(out)
}

/// `chat-sources.cjs` `shape(...)` of a placed entry `e` of the file `f`.
fn shape(f: &JValue, name: &str, e: &JValue) -> Result<JValue, Thrown> {
    let excerpt = e.get("kind").as_str() == Some("excerpt");
    let page = if excerpt {
        page_of(e.get("body"))?
    } else {
        None
    };
    // `f.attachment && f.attachment.id`, then `String(id || file)`.
    let attachment = f.get("attachment");
    let id = if attachment.truthy() {
        attachment.get("id")
    } else {
        attachment
    };
    let id = if id.truthy() {
        string_of(id)?
    } else {
        name.to_string()
    };
    let mut item = vec![
        ("id", JValue::Str(js::slice(&id, NAME_MAX))),
        ("file", JValue::Str(js::slice(name, NAME_MAX))),
        ("snippet", JValue::Str(snippet_of(e.get("body"))?)),
        (
            "kind",
            JValue::from(if excerpt { "excerpt" } else { "file" }),
        ),
    ];
    if let Some(page) = page {
        item.push(("page", JValue::Num(f64::from(page))));
    }
    if let JValue::Num(score) = e.get("score") {
        if score.is_finite() {
            item.push(("score", JValue::Num(js_round(score * 1000.0) / 1000.0)));
        }
    }
    Ok(JValue::obj(item))
}

/// `chat-sources.cjs` `buildSources(entries, files)`: the source items of the entries whose file
/// is one of the readable `files`, in order, at most [`MAX_SOURCES`].
pub fn build_sources(entries: &[JValue], files: &[JValue]) -> Result<Vec<JValue>, Thrown> {
    let names = by_name(files)?;
    let mut out = Vec::new();
    for e in entries {
        if out.len() >= MAX_SOURCES {
            break;
        }
        if !e.truthy() {
            continue;
        }
        let Some((name, f)) = e.get("file").as_str().and_then(|n| names.get_key_value(n)) else {
            continue;
        };
        out.push(shape(f, name, e)?);
    }
    Ok(out)
}

/// `source-text.cjs` `textLength(f)`: a source's text length without reading a text in its file.
fn text_length(f: &JValue) -> Result<i64, Thrown> {
    let external = f.get("content") == &JValue::Undefined
        && f.get("textRef").as_str().is_some_and(|r| {
            r.len() == 64 && r.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        });
    if let (true, JValue::Num(chars)) = (external, f.get("chars")) {
        if chars.fract() == 0.0 && chars.abs() <= MAX_SAFE {
            // A safe integer: exact in i64.
            return Ok(*chars as i64);
        }
    }
    let len = js::len(&string_or_empty(f.get("content"))?);
    Ok(i64::try_from(len).unwrap_or(i64::MAX))
}

/// `source-coverage.cjs` `partialSources(files, placed)`: the files longer than [`WHOLE_MAX`] of
/// which `placed` holds less than the whole text.
pub fn partial_sources(files: &[JValue], placed: &[JValue]) -> Result<Vec<Partial>, Thrown> {
    let mut out = Vec::new();
    for f in files {
        let length = text_length(f)?;
        if length <= WHOLE_MAX {
            continue;
        }
        let mut read: i64 = 0;
        for p in placed {
            if p.truthy() && strict_eq(p.get("file"), f.get("name")) {
                let n = js::len(&string_or_empty(p.get("body"))?);
                read = read.saturating_add(i64::try_from(n).unwrap_or(i64::MAX));
            }
        }
        if read >= length {
            continue;
        }
        out.push(Partial {
            name: string_of(f.get("name"))?,
            length: length.unsigned_abs(),
            read: read.unsigned_abs(),
        });
    }
    Ok(out)
}

/// `source-coverage.cjs` `coverageWarning(partial)`: the warning the user sees with the reply.
pub fn coverage_warning(partial: &[Partial]) -> String {
    let one = |p: &Partial| {
        format!(
            "Only part of \"{}\" was read for this reply ({} of {} characters); answers about the rest may be wrong.",
            p.name,
            sources::grouped(p.read),
            sources::grouped(p.length)
        )
    };
    partial.iter().map(one).collect::<Vec<_>>().join(" ")
}

/// The `sources` event: `replySources`, sent when there are any and the turn is not `compactOnly`.
pub fn sources_event(retrieved: &Retrieved, compact: bool) -> Option<&[JValue]> {
    (!retrieved.reply_sources.is_empty() && !compact).then_some(retrieved.reply_sources.as_slice())
}

/// The executor's `injected-sources` refusal: an injected whole file whose name is not text.
pub fn injected_not_text(injected: &[JValue]) -> bool {
    injected.iter().any(|e| {
        e.truthy() && e.get("kind").as_str() == Some("file") && e.get("file").as_str().is_none()
    })
}

/// `rust-chat-native.cjs` `injectedSources({ injected, placed, readable, reply })`: the reply's
/// sources, and each injected whole file not among the placed ones with the part of its text the
/// attribution reads and the item [`build_sources`] gives it (`null` when its file is not readable).
pub fn injected_sources(
    injected: &[JValue],
    placed: &[JValue],
    readable: &[JValue],
    reply: &[JValue],
) -> Result<JValue, Thrown> {
    // `new Set(placed.map((e) => e && e.file))`.
    let already: Vec<&JValue> = placed
        .iter()
        .map(|e| if e.truthy() { e.get("file") } else { e })
        .collect();
    let mut out = Vec::new();
    for e in injected {
        if !e.truthy()
            || e.get("kind").as_str() != Some("file")
            || already.iter().any(|a| strict_eq(a, e.get("file")))
        {
            continue;
        }
        let text = js::slice(&string_or_empty(e.get("body"))?, ATTRIBUTE_MAX);
        let item = build_sources(std::slice::from_ref(e), readable)?
            .into_iter()
            .next()
            .unwrap_or(JValue::Null);
        out.push(JValue::obj([
            ("file", e.get("file").clone()),
            ("text", JValue::Str(text)),
            ("item", item),
        ]));
    }
    Ok(JValue::obj([
        ("reply", JValue::Arr(reply.to_vec())),
        ("injected", JValue::Arr(out)),
    ]))
}

/// The prepared frame's `sources` of a turn's retrieval ([`injected_sources`]).
pub fn frame_sources(retrieved: &Retrieved) -> Result<JValue, Thrown> {
    injected_sources(
        &retrieved.injected,
        &retrieved.placed,
        &retrieved.readable,
        &retrieved.reply_sources,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn math_round_ties_go_up() {
        for (x, want) in [
            (2.5, 3.0),
            (-2.5, -2.0),
            (-0.5, 0.0),
            (0.49999999999999994, 0.0),
            (7.0, 7.0),
        ] {
            assert_eq!(js_round(x), want, "{x}");
        }
    }

    #[test]
    fn page_markers() {
        let page = |s: &str| page_of(&JValue::from(s)).unwrap();
        assert_eq!(page("[Page 4]\nQA"), Some(4));
        assert_eq!(page(" \n[Page 12]"), Some(12));
        assert_eq!(page("[Page 1234567]\nQA"), None);
        assert_eq!(page("[Page 0]\nQA"), None);
        assert_eq!(page("[Page 4] QA"), None);
    }

    #[test]
    fn the_diary_space_neither_waits_nor_loads() {
        let body = JValue::obj([("spaceId", JValue::from("diary"))]);
        let id = JValue::from("qa-project");
        assert_eq!(wait(&id, &body, true), None);
        assert!(!loads_project(&id, &body));
        assert!(wait(&id, &JValue::obj([("message", JValue::from("QA"))]), true).is_some());
        assert_eq!(wait(&id, &JValue::Null, false), None);
    }

    #[test]
    fn a_report_that_throws_keeps_what_was_set_before() {
        let files = vec![JValue::obj([("name", JValue::from("QA a.txt"))])];
        let bad = JValue::obj([("toString", JValue::Num(1.0))]);
        let placed = JValue::Arr(vec![JValue::obj([
            ("file", JValue::from("QA a.txt")),
            ("body", bad),
        ])]);
        let r = report(files, &placed, &JValue::Arr(Vec::new()));
        assert_eq!(r.placed.len(), 1);
        assert!(r.reply_sources.is_empty() && r.partial_reads.is_empty());
    }
}
