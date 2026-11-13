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
//!    The record the caller read goes through [`load`] (below) before anything reads it.
//! 4. [`multimodal`] (#1726/#1736) and [`request`]: what retrieval is asked: the project's id, the
//!    files it may read ([`readable`]: `instruction-skills.cjs` `sources`, the files that are
//!    neither skill candidates nor unreadable), the question (`body.message` as it is; chat.cjs
//!    derives no search query from the history here), the account, and the options (`attached`
//!    for a shadow project, `images` when multimodal retrieval is on). No request when the project
//!    has no files. The request holds copies of the files it names, so what [`retrieved`] reads
//!    is the snapshot the request was built from.
//! 5. [`retrieved`]: what chat.cjs keeps of the [`Answer`]: the block, and from the report
//!    (`onSources(placed, injected)`) the placed and injected entries, the files read, the reply's
//!    sources ([`build_sources`], `chat-sources.cjs`) and the partial reads ([`partial_sources`],
//!    `source-coverage.cjs`, #1513). rag.cjs calls the report inside a `try`, so a report that
//!    throws keeps what was set before the throw. It stops ([`Stopped`]) for a retrieval that
//!    threw (rag.cjs never does; the turn fails, [`Stopped::Threw`]) and for a request that asked
//!    for images ([`Stopped::Images`]: the turn stays in Node, see the named difference).
//! 6. [`sources_event`], [`injected_not_text`], [`frame_sources`] (`rust-chat-native.cjs`
//!    `injectedSources`, #1793) and [`coverage_warning`].
//!
//! The block, the not-ready list and the partial reads feed [`crate::ProjectTurn`]
//! (`files_block`, `not_ready`, `partial_reads`). The readable, waiting and stored-only sets of the
//! system message are [`crate::sources`]'s.
//!
//! Inputs are what chat.cjs holds: the request body (a `diary-extras` turn's after chat.cjs set its
//! `projectId` to [`DIARY_EXTRAS_PROJECT`]) and the project record as stored (projects.json, after
//! `instruction-skills.cjs` `snapshot`). Strings are JS strings as `js_json::wtf16` holds them;
//! lengths and cuts count UTF-16 units.
//!
//! The caller contract for a source whose text is in its own file (#1806): the caller passes the
//! stored record unchanged (no text inlined) and a lookup of the texts it could read, keyed by
//! `String(textRef)`. [`load`] then does what projects.cjs's `source-text.cjs` `attachFile` does:
//! a file object whose `String(textRef)` is 64 lowercase hex and that has no own `content` is
//! external ([`Source::external`]); its `content` is the text, or `""` when the lookup has none (a
//! missing text, as the accessor reads it). A file that holds a `content` of its own, even `""`, is
//! never external. An external source's length is its `chars` when that is a safe integer, else
//! its text's (`textLength`); the instruction-skill candidates are judged on the stored record
//! (an external source by its saved `skillHeader` flag, [`crate::skills`]).
//!
//! Replayed byte for byte on `tests/fixtures/node-retrieval.v1.json` (written by
//! `tests/record_retrieval.cjs`, whole turns through `createChatHandler` with the two seams
//! stubbed). One named difference, recorded in the fixture's `differences` and tested: a turn
//! whose request asks for images (multimodal retrieval on) is [`Stopped::Images`] and stays in
//! Node, also when retrieval finds none, until `onImages`, the asset plan and the retrieved images
//! (#1726) are ported; Node runs it (the executor refuses `retrieved-images` only when some were
//! found).
//!
//! Later slices: the images multimodal retrieval finds, and the attribution after the reply
//! (chat-turn's `sources`).

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

/// A project file as chat.cjs reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// The file, its `content` what Node reads (an external source's: its text, or `""`).
    pub file: JValue,
    /// `source-text.cjs` `isExternal`: its text is in its own file (set by [`load`] only).
    pub external: bool,
}

impl Source {
    /// A file that is never external (a value no loader attached a text to).
    pub fn plain(file: JValue) -> Source {
        Source {
            file,
            external: false,
        }
    }
}

/// A project record as `getProject` gives it (see the module doc's caller contract).
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    /// The record as stored.
    pub stored: JValue,
    /// The record with each external source's `content` set: what [`crate::ProjectTurn::project`]
    /// and every reader of `file.content` take.
    pub project: JValue,
    /// Per position of `project.files`: the source is external.
    pub external: Vec<bool>,
}

impl Loaded {
    /// `project.files[at]` with its flag.
    pub fn source(&self, at: usize) -> Option<Source> {
        let file = items(self.project.get("files")).get(at)?.clone();
        let external = self.external.get(at).copied().unwrap_or(false);
        Some(Source { file, external })
    }
}

/// `REF.test(String(f.textRef))`: the text file's name; `Err` where `String()` throws.
fn text_ref(f: &JValue) -> Result<Option<String>, Thrown> {
    let r = string_of(f.get("textRef"))?;
    let hex = r.len() == 64 && r.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    Ok(hex.then_some(r))
}

/// projects.cjs's `attachAll` of one record (`source-text.cjs` `attachFile` of each of its files):
/// `texts(String(textRef))` is the text of a source the caller could read. `Err` where `attachFile`
/// throws (a `textRef` whose `String()` throws: Node cannot load the record).
pub fn load(stored: &JValue, texts: &dyn Fn(&str) -> Option<String>) -> Result<Loaded, Thrown> {
    let mut project = stored.clone();
    let mut external = Vec::new();
    if let JValue::Obj(fields) = &mut project {
        for (key, value) in fields.iter_mut() {
            let (true, JValue::Arr(files)) = (key == "files", value) else {
                continue;
            };
            for f in files.iter_mut() {
                let reference = match f {
                    JValue::Obj(_) => text_ref(f)?,
                    _ => None,
                };
                let JValue::Obj(fields) = f else {
                    external.push(false);
                    continue;
                };
                let own = fields.iter().any(|(k, _)| k == "content");
                match reference {
                    Some(r) if !own => {
                        let text = texts(&r).unwrap_or_default();
                        fields.push(("content".to_string(), JValue::Str(text)));
                        external.push(true);
                    }
                    _ => external.push(false),
                }
            }
        }
    }
    Ok(Loaded {
        stored: stored.clone(),
        project,
        external,
    })
}

/// `instruction-skills.cjs` `sources(project)`: the positions in `project.files` of the files that
/// are neither instruction-skill candidates nor unreadable, in order.
pub fn readable(loaded: &Loaded) -> Result<Vec<usize>, Thrown> {
    let files = items(loaded.project.get("files"));
    let mut candidate = vec![false; files.len()];
    for skill in skills::list(&loaded.stored)? {
        if let Some(c) = candidate.get_mut(skill.at) {
            *c = true;
        }
    }
    let mut out = Vec::new();
    for (at, (file, skill)) in files.iter().zip(candidate).enumerate() {
        if !skill && !sources::is_unreadable(file)? {
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
    /// Those files, copied from the record the request was built from.
    pub files: Vec<Source>,
    /// `body.message`.
    pub query: &'a str,
    /// `currentWorkspace().userId`.
    pub user: &'a str,
    /// `options.attached`: the project is a free chat's attachments ([`crate::project::is_chat_attachments`]).
    pub attached: bool,
    /// `options.images` (with an `onImages` hook): [`multimodal`]. A request with images keeps the
    /// turn in Node ([`Stopped::Images`]): a caller checks this before it asks retrieval.
    pub images: bool,
}

/// The retrieval request of a project turn, or `None` when its `files` is not a non-empty list.
/// `multimodal` is [`multimodal`]'s answer.
pub fn request<'a>(
    loaded: &'a Loaded,
    message: &'a str,
    user: &'a str,
    multimodal: bool,
) -> Result<Option<Request<'a>>, Thrown> {
    let project = &loaded.project;
    let attached = crate::project::is_chat_attachments(project)?;
    if !crate::project::retrieval_ran(project) {
        return Ok(None);
    }
    let readable = readable(loaded)?;
    let files = readable
        .iter()
        .filter_map(|at| loaded.source(*at))
        .collect();
    Ok(Some(Request {
        project_id: project.get("id"),
        readable,
        files,
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

/// Why [`retrieved`] gives no state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// Retrieval threw: handleChat rejects and the turn fails before its system message.
    Threw,
    /// The request asked for images (multimodal retrieval): what retrieval finds of them is a later
    /// slice, so the turn stays in Node (the named difference in the module doc).
    Images,
}

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
    pub readable: Vec<Source>,
}

/// chat.cjs's state after the retrieval `request` (none: nothing was asked) answered.
pub fn retrieved(request: Option<&Request<'_>>, answer: Answer) -> Result<Retrieved, Stopped> {
    let Some(request) = request else {
        return Ok(Retrieved::default());
    };
    if request.images {
        return Err(Stopped::Images);
    }
    let (block, reported) = match answer {
        Answer::Threw => return Err(Stopped::Threw),
        Answer::Answered { block, reported } => (block, reported),
    };
    let mut out = match reported {
        Some((placed, injected)) => report(request.files.clone(), &placed, &injected),
        None => Retrieved::default(),
    };
    out.files_block = block;
    Ok(out)
}

/// The `onSources` hook: each assignment in order; a throw (rag.cjs catches it) keeps the earlier
/// ones.
fn report(readable: Vec<Source>, placed: &JValue, injected: &JValue) -> Retrieved {
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
fn by_name(files: &[Source]) -> Result<HashMap<&str, &JValue>, Thrown> {
    let mut out = HashMap::new();
    for f in files.iter().map(|s| &s.file) {
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
pub fn build_sources(entries: &[JValue], files: &[Source]) -> Result<Vec<JValue>, Thrown> {
    let names = by_name(files)?;
    let mut out = Vec::new();
    for e in entries {
        if out.len() >= MAX_SOURCES {
            break;
        }
        out.extend(item_of(&names, e)?);
    }
    Ok(out)
}

/// The item `buildSources` makes of one entry, if its file is readable.
fn item_of(names: &HashMap<&str, &JValue>, e: &JValue) -> Result<Option<JValue>, Thrown> {
    if !e.truthy() {
        return Ok(None);
    }
    match e.get("file").as_str().and_then(|n| names.get_key_value(n)) {
        Some((name, f)) => shape(f, name, e).map(Some),
        None => Ok(None),
    }
}

/// `source-text.cjs` `textLength(f)`: a source's text length without reading a text in its file.
fn text_length(source: &Source) -> Result<i64, Thrown> {
    let f = &source.file;
    if let (true, JValue::Num(chars)) = (source.external, f.get("chars")) {
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
pub fn partial_sources(files: &[Source], placed: &[JValue]) -> Result<Vec<Partial>, Thrown> {
    let mut out = Vec::new();
    for source in files {
        let f = &source.file;
        let length = text_length(source)?;
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
    readable: &[Source],
    reply: &[JValue],
) -> Result<JValue, Thrown> {
    // `new Set(placed.map((e) => e && e.file))`.
    let already: Vec<&JValue> = placed
        .iter()
        .map(|e| if e.truthy() { e.get("file") } else { e })
        .collect();
    // Node builds the name map for every entry (`buildSources([e], readable)`); built once here, at
    // the first entry that needs it, so it throws where Node's first such call would.
    let mut names = None;
    let mut out = Vec::new();
    for e in injected {
        if !e.truthy()
            || e.get("kind").as_str() != Some("file")
            || already.iter().any(|a| strict_eq(a, e.get("file")))
        {
            continue;
        }
        let text = js::slice(&string_or_empty(e.get("body"))?, ATTRIBUTE_MAX);
        if names.is_none() {
            names = Some(by_name(readable)?);
        }
        let item = match &names {
            Some(n) => item_of(n, e)?,
            None => None,
        };
        let item = item.unwrap_or(JValue::Null);
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
    fn a_request_for_images_stays_in_node() {
        let stored =
            js_json::wtf16::parse(r#"{"id":"qa-p","files":[{"name":"QA a.txt","content":"QA"}]}"#)
                .unwrap();
        let loaded = load(&stored, &|_| None).unwrap();
        let answer = || Answer::Answered {
            block: Some("QA".into()),
            reported: None,
        };
        let asked = request(&loaded, "QA", "qa-user", true).unwrap().unwrap();
        assert_eq!(retrieved(Some(&asked), answer()), Err(Stopped::Images));
        assert_eq!(retrieved(Some(&asked), Answer::Threw), Err(Stopped::Images));
        let plain = request(&loaded, "QA", "qa-user", false).unwrap().unwrap();
        assert!(retrieved(Some(&plain), answer()).is_ok());
    }

    #[test]
    fn only_a_file_without_its_own_content_is_external() {
        let hex = "a".repeat(64);
        let stored = js_json::wtf16::parse(&format!(
            r#"{{"files":[{{"textRef":"{hex}","chars":9}},{{"textRef":"{hex}","content":"","chars":9}},{{"textRef":["{hex}"]}},{{"textRef":"{}"}},"QA"]}}"#,
            hex.to_uppercase()
        ))
        .unwrap();
        let loaded = load(&stored, &|_| None).unwrap();
        assert_eq!(loaded.external, [true, false, true, false, false]);
        let first = loaded.source(0).unwrap();
        assert_eq!(first.file.get("content").as_str(), Some(""));
        assert_eq!(text_length(&first).unwrap(), 9);
        assert_eq!(text_length(&loaded.source(1).unwrap()).unwrap(), 0);
        let throws = js_json::wtf16::parse(r#"{"files":[{"textRef":{"toString":1}}]}"#).unwrap();
        assert_eq!(load(&throws, &|_| None), Err(Thrown));
    }

    #[test]
    fn a_report_that_throws_keeps_what_was_set_before() {
        let files = vec![Source::plain(JValue::obj([(
            "name",
            JValue::from("QA a.txt"),
        )]))];
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
