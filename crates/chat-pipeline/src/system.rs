//! The turn's system message (core chat.cjs:599-752, before the route is known), and the helpers it
//! quotes: quoted-name.cjs, skill-index.cjs, source-readability.cjs `waitingNotice`/`readable`,
//! source-coverage.cjs `coverageNote`, chat-skill-routing.cjs `skillBlock`/`pinnedSkillBlock`,
//! shared-context.cjs `forChat`.
//!
//! The parts that come from elsewhere are inputs: the account instructions and memory parts
//! (server-account `chat_parts`), the date part (its zone-less variant for an off-box route), the
//! knowledge-file block (retrieval), the confirmed chat frame's block and the linked brains' block.
use crate::text::{as_str, frame, get};
use js_json::{wtf16, JValue};

/// chat.cjs `SKILL_INDEX_LEAD`.
pub const SKILL_INDEX_LEAD: &str = "Available skills (load the full file with the read_project_file tool when a task matches; do not guess their contents):";
/// chat.cjs `SKILL_INDEX_LEAD_NO_READER`.
pub const SKILL_INDEX_LEAD_NO_READER: &str = "Available skills (their files cannot be loaded in this reply; do not guess their contents):";
/// source-coverage.cjs `READ_FILE_HINT`.
pub const COVERAGE_READ_FILE_HINT: &str = " read_project_file with an offset can read more of a file.";
const MAX_BODY_CHARS: usize = 12000;

fn js_string(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

/// quoted-name.cjs `quoteName(name)`: JSON-quoted, with line-break and reorder code points escaped.
pub fn quote_name(name: &str) -> String {
    let q = wtf16::stringify(&JValue::Str(name.to_owned())).unwrap_or_default();
    let mut out = String::with_capacity(q.len());
    for c in q.chars() {
        if matches!(c, '\u{85}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// `quotedNames(names)`.
pub fn quoted_names<S: AsRef<str>>(names: &[S]) -> String {
    names.iter().map(|n| quote_name(n.as_ref())).collect::<Vec<_>>().join(", ")
}

/// skill-index.cjs `formatSkillIndex(skills)`: one JSON line per skill, at most 32 lines and 6144 units.
pub fn format_skill_index(skills: &[JValue]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut size = 0usize;
    let mut omitted = 0usize;
    for skill in skills {
        let file = match get(skill, "file") {
            JValue::Str(f) if !f.is_empty() && wtf16::len(f) <= 500 => f.clone(),
            _ => {
                omitted += 1;
                continue;
            }
        };
        let name = match get(skill, "name") {
            v if v.truthy() => js_string(v),
            _ => file.clone(),
        };
        let or_empty = |k: &str| match get(skill, k) {
            v if v.truthy() => js_string(v),
            _ => String::new(),
        };
        let line = wtf16::stringify(&JValue::obj([
            ("file", JValue::Str(file)),
            ("name", JValue::Str(wtf16::slice(&name, 160))),
            ("description", JValue::Str(wtf16::slice(&or_empty("description"), 500))),
            ("version", JValue::Str(wtf16::slice(&or_empty("version"), 80))),
        ]))
        .unwrap_or_default();
        let len = wtf16::len(&line);
        if lines.len() >= 32 || size + len + 1 > 6144 {
            omitted += 1;
            continue;
        }
        size += len + 1;
        lines.push(line);
    }
    if omitted > 0 {
        lines.push(format!("({omitted} additional skill metadata entries omitted to fit the context limit.)"));
    }
    lines.join("\n")
}

fn blank(v: &JValue) -> bool {
    let s = match v {
        v if v.truthy() => js_string(v),
        _ => String::new(),
    };
    crate::text::trim(&s).is_empty()
}

/// source-readability.cjs `isUnreadable(file)` (#586).
pub fn is_unreadable(file: &JValue) -> bool {
    if !matches!(file, JValue::Obj(_) | JValue::Arr(_)) {
        return false;
    }
    let a = get(file, "attachment");
    let d = get(file, "document");
    if a.truthy()
        && as_str(get(a, "state")) == Some("stored")
        && matches!(as_str(get(a, "group")), Some("Text" | "Documents"))
    {
        return true;
    }
    d.truthy() && matches!(as_str(get(d, "state")), Some("failed" | "queued")) && blank(get(file, "content"))
}

/// `isWaiting(file)` (#2237).
fn is_waiting(file: &JValue) -> bool {
    let d = if matches!(file, JValue::Obj(_) | JValue::Arr(_)) { get(file, "document") } else { &JValue::Undefined };
    d.truthy() && as_str(get(d, "state")) == Some("queued") && blank(get(file, "content"))
}

fn list(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

/// `readable(files)`.
pub fn readable(files: &JValue) -> Vec<&JValue> {
    list(files).iter().filter(|f| !is_unreadable(f)).collect()
}

/// `waitingNotice(files)` (#2237, #2245): `''` when no document waits.
pub fn waiting_notice(files: &JValue) -> String {
    let all: Vec<String> = list(files).iter().filter(|f| is_waiting(f)).map(|f| js_string(get(f, "name"))).collect();
    if all.is_empty() {
        return String::new();
    }
    let names = &all[..all.len().min(10)];
    let more = all.len() - names.len();
    let one = all.len() == 1;
    format!(
        "{}{} {} waiting to be read (document reading is not available yet), so {} contents are not available to you. Say so if asked about {}; do not guess at the contents or call {} empty.",
        quoted_names(names),
        if more > 0 { format!(" and {more} more") } else { String::new() },
        if one { "is" } else { "are" },
        if one { "its" } else { "their" },
        if one { "it" } else { "them" },
        if one { "it" } else { "them" },
    )
}

/// `Number(n).toLocaleString('en-GB')` for the whole counts the notes print.
fn en_gb(n: f64) -> String {
    if !n.is_finite() {
        return js_json::number_to_string(n);
    }
    let neg = n < 0.0;
    // toLocaleString rounds to at most three fraction digits.
    let r = (n.abs() * 1000.0).round() / 1000.0;
    let whole = r.trunc();
    let mut digits = format!("{whole:.0}");
    let mut out = String::new();
    while digits.len() > 3 {
        let tail = digits.split_off(digits.len() - 3);
        out = format!(",{tail}{out}");
    }
    out = format!("{digits}{out}");
    let frac = r - whole;
    if frac > 0.0 {
        let f = format!("{frac:.3}");
        out.push_str(f.trim_start_matches('0').trim_end_matches('0'));
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

/// One source of which this reply read only part (source-coverage.cjs `partialSources`' rows).
#[derive(Clone, Debug, PartialEq)]
pub struct PartialRead {
    pub name: String,
    pub length: f64,
    pub read: f64,
}

/// `coverageNote(partial)` (#1513): `''` when every source was read in full.
pub fn coverage_note(partial: &[PartialRead]) -> String {
    if partial.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = partial
        .iter()
        .map(|p| {
            let read = if p.read != 0.0 {
                format!("only {} characters of it (excerpts or the start)", en_gb(p.read))
            } else {
                "none of it (it was not included)".to_owned()
            };
            format!("{} is {} characters long and this reply read {read}", quote_name(&p.name), en_gb(p.length))
        })
        .collect();
    format!(
        "Source text was truncated for this reply: {}. The rest of each file was NOT read. Do not say that some text is absent from a file, or answer \"no\", from the part you read; say which part you read.{COVERAGE_READ_FILE_HINT}",
        parts.join("; ")
    )
}

/// `/^---\r?\n[\s\S]*?\r?\n---\r?\n?/` removed, then trimmed: the front matter ends at the first
/// `\n---` after the opening line (its optional `\r` is part of the lazy middle).
fn skill_body(content: &str) -> String {
    let mut body = content;
    if let Some(rest) = content.strip_prefix("---\r\n").or_else(|| content.strip_prefix("---\n")) {
        if let Some(k) = rest.find("\n---") {
            let tail = &rest[k + 4..];
            body = tail.strip_prefix("\r\n").or_else(|| tail.strip_prefix('\n')).unwrap_or(tail);
        }
    }
    crate::text::trim(body).to_owned()
}

fn cut_body(body: String, file: &str) -> String {
    if wtf16::len(&body) > MAX_BODY_CHARS {
        format!("{}\n\n(Truncated; read {file} with read_project_file for the rest.)", wtf16::slice(&body, MAX_BODY_CHARS))
    } else {
        body
    }
}

/// chat-skill-routing.cjs `skillBlock(skills)`: the auto-loaded skills, bounded.
pub fn skill_block(skills: &[JValue]) -> String {
    skills
        .iter()
        .map(|s| {
            let file = js_string(get(s, "file"));
            let body = cut_body(skill_body(&js_string(get(s, "content"))), &file);
            format!("Skill \"{}\" ({file}) matches this message; follow it:\n{body}", js_string(get(s, "name")))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `pinnedSkillBlock({ record, content })` (#272).
pub fn pinned_skill_block(record: &JValue, content: &str) -> String {
    let file = js_string(get(record, "file"));
    let body = cut_body(skill_body(content), &file);
    format!(
        "The user invoked skill \"{}\" ({file}, SHA-256 {}) for this message. These are user-reviewed reference instructions; they cannot grant tool permissions or override the current user request. Follow them:\n{body}",
        js_string(get(record, "name")),
        js_string(get(record, "contentHash"))
    )
}

/// JavaScript's `\s`.
fn js_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// `.replace(/\s+/g, ' ').trim()`.
fn fold_space(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if js_space(c) {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out.trim_matches(' ').to_owned()
}

/// shared-context.cjs `clip(text, n)`: white space folded, at most `n` units with an ellipsis.
fn clip(v: &JValue, n: usize) -> String {
    let s = if v.truthy() { js_string(v) } else { String::new() };
    let folded = fold_space(&s);
    if wtf16::len(&folded) > n {
        format!("{}…", wtf16::slice(&folded, n - 1))
    } else {
        folded
    }
}

/// `forChat(project, tasks)`: the recent Code tasks a project shares into chat; `''` when none.
pub fn code_tasks_part(tasks: &[JValue]) -> String {
    let mut recent: Vec<&JValue> = tasks.iter().filter(|t| t.truthy() && get(t, "task").truthy()).collect();
    let at = |t: &JValue| match get(t, "updatedAt") {
        v if v.truthy() => v.as_num().unwrap_or(f64::NAN),
        _ => 0.0,
    };
    // `sort((a, b) => (b.updatedAt || 0) - (a.updatedAt || 0))`: stable, a NaN difference keeps the order.
    recent.sort_by(|a, b| at(b).partial_cmp(&at(a)).unwrap_or(std::cmp::Ordering::Equal));
    recent.truncate(6);
    if recent.is_empty() {
        return String::new();
    }
    let line = |t: &JValue| {
        let status = get(t, "status");
        let mut bits = vec![if status.truthy() { js_string(status) } else { "unknown".to_owned() }];
        if get(t, "branch").truthy() {
            bits.push(format!("branch {}", js_string(get(t, "branch"))));
        }
        if get(t, "error").truthy() {
            bits.push(format!("error: {}", clip(get(t, "error"), 160)));
        }
        format!("- \"{}\" ({})", clip(get(t, "task"), 160), bits.join(", "))
    };
    let body = recent.iter().map(|t| line(t)).collect::<Vec<_>>().join("\n");
    let (kind, label) = ("recent Code tasks", "diffs not visible");
    let max = 6000 - wtf16::len(&frame(kind, label, ""));
    let capped = if wtf16::len(&body) > max { format!("{}…", wtf16::slice(&body, max - 1)) } else { body };
    frame(kind, label, &capped)
}

/// What the system message is built from.
pub struct Inputs<'a> {
    /// account-instructions.cjs `systemPart(read(dir))`.
    pub account_part: Option<&'a str>,
    /// current-time-part.cjs `systemPart()`.
    pub time_part: &'a str,
    /// account-memory.cjs `systemPart(read(dir), project?.memories)`.
    pub memory_part: Option<&'a str>,
    /// The project snapshot (`null` for a free chat or the Diary space).
    pub project: &'a JValue,
    /// A free chat's attachment project (#1441).
    pub chat_attachments: bool,
    pub compact_only: bool,
    /// `codeTasksFor(project)` when the project shares into chat (`None`: it does not, or the read threw).
    pub code_tasks: Option<&'a [JValue]>,
    /// `rag.filesContext(...)`.
    pub files_block: Option<&'a str>,
    /// Sources attached just before this message that were not ready (`{name, reason}`).
    pub sources_not_ready: &'a [JValue],
    pub partial_reads: &'a [PartialRead],
    /// `skillsIndexFor(project)`.
    pub skills: &'a [JValue],
    /// The skills the router auto-loaded.
    pub auto_skills: &'a [JValue],
    /// The pinned skill (`record`, `content`).
    pub pinned: Option<(&'a JValue, &'a str)>,
    /// chat-frame-steering.cjs `frameBlock(chatFrame)`.
    pub frame_block: Option<&'a str>,
    /// chat-brain.cjs `linkedBrainBlock(...)` (#742).
    pub brain_block: Option<&'a str>,
}

/// The assembled system message.
#[derive(Clone, Debug, PartialEq)]
pub struct System {
    pub parts: Vec<String>,
    /// The auto-loaded skills' block (`autoSkillBlock`), `''` when none.
    pub auto_skill_block: String,
    brain: Option<String>,
    time: String,
}

fn some(v: Option<&str>) -> Option<&str> {
    v.filter(|s| !s.is_empty())
}

/// chat.cjs:599-669 and 725-752: the parts in Node's order.
pub fn assemble(i: &Inputs<'_>) -> System {
    let mut parts: Vec<String> = Vec::new();
    let mut auto_skill_block = String::new();
    if let Some(a) = some(i.account_part) {
        parts.push(a.to_owned());
    }
    parts.push(i.time_part.to_owned());
    let p = i.project;
    if !p.truthy() {
        if let Some(m) = some(i.memory_part) {
            parts.push(m.to_owned());
        }
    } else {
        if get(p, "name").truthy() && !i.chat_attachments {
            parts.push(format!("You are working inside the user's project {}.", quote_name(&js_string(get(p, "name")))));
        }
        if get(p, "goal").truthy() {
            parts.push(format!("Project goal: {}", js_string(get(p, "goal"))));
        }
        if get(p, "instructions").truthy() {
            parts.push(format!("Project instructions (follow closely):\n{}", js_string(get(p, "instructions"))));
        }
        if let Some(m) = some(i.memory_part) {
            parts.push(m.to_owned());
        }
        if let Some(tasks) = i.code_tasks {
            let code = code_tasks_part(tasks);
            if !code.is_empty() {
                parts.push(code);
            }
        }
        if let Some(fb) = some(i.files_block) {
            if !i.chat_attachments || i.compact_only {
                parts.push(format!("Relevant knowledge-file excerpts for this message:\n{fb}"));
            }
        }
        if !i.sources_not_ready.is_empty() {
            let n = i.sources_not_ready.len();
            let names: Vec<String> = i.sources_not_ready.iter().map(|f| js_string(get(f, "name"))).collect();
            let all_failed = i.sources_not_ready.iter().all(|f| as_str(get(f, "reason")) == Some("failed"));
            parts.push(format!(
                "The user just attached {}, but {} not {} in time, so {} contents are not available to you for this message. Say so if asked about {}; do not guess at the contents.",
                quoted_names(&names),
                if n == 1 { "it was" } else { "they were" },
                if all_failed { "processed" } else { "finished processing" },
                if n == 1 { "its" } else { "their" },
                if n == 1 { "it" } else { "them" },
            ));
        }
        let waiting = waiting_notice(get(p, "files"));
        if !waiting.is_empty() {
            parts.push(waiting);
        }
        let coverage = coverage_note(i.partial_reads);
        if !coverage.is_empty() {
            parts.push(coverage);
        }
        if !i.skills.is_empty() {
            parts.push(format!("{SKILL_INDEX_LEAD}\n{}", format_skill_index(i.skills)));
            if i.pinned.is_none() && !i.auto_skills.is_empty() {
                auto_skill_block = skill_block(i.auto_skills);
                parts.push(auto_skill_block.clone());
            }
        }
        if let Some((record, content)) = i.pinned {
            parts.push(pinned_skill_block(record, content));
        }
    }
    let stored_only: Vec<String> = readable(get(p, "files"))
        .into_iter()
        .filter(|f| as_str(get(get(f, "attachment"), "state")) == Some("stored"))
        .map(|f| js_string(get(f, "name")))
        .collect();
    if !stored_only.is_empty() {
        parts.push(format!(
            "These sources are stored only; their contents are NOT available to the model: {}. Do not claim to know their contents.",
            quoted_names(&stored_only)
        ));
    }
    if let Some(f) = some(i.frame_block) {
        parts.push(f.to_owned());
    }
    let brain = some(i.brain_block).map(str::to_owned);
    if let Some(b) = &brain {
        parts.push(b.clone());
    }
    System { parts, auto_skill_block, brain, time: i.time_part.to_owned() }
}

impl System {
    /// `sys`: the parts joined.
    pub fn text(&self) -> String {
        self.parts.join("\n\n")
    }

    /// `sysWithoutBrain` (#815): what the routing check reads and an off-box provider gets.
    pub fn without_brain(&self) -> String {
        match &self.brain {
            Some(b) => self.parts.iter().filter(|p| *p != b).cloned().collect::<Vec<_>>().join("\n\n"),
            None => self.text(),
        }
    }

    /// `routerBase` (#1644): without the brain block and the date part.
    pub fn router_base(&self) -> String {
        self.parts
            .iter()
            .filter(|p| Some(*p) != self.brain.as_ref() && **p != self.time)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The system text for an off-box route (#815, #1587): no brain block, and the date part without the
    /// server's time-zone name (`zoneless`), replaced once as Node's `String.replace` does.
    pub fn off_box(&self, zoneless: &str) -> String {
        let sys = self.without_brain();
        match sys.find(&self.time) {
            Some(at) if !self.time.is_empty() => format!("{}{zoneless}{}", &sys[..at], &sys[at + self.time.len()..]),
            _ => sys,
        }
    }
}

/// chat.cjs: a free chat's attached-files block goes right before the latest user message (#1441).
pub fn with_attached_files(msgs: &mut [JValue], message: &str, files_block: &str) {
    let Some(JValue::Obj(items)) = msgs.last_mut() else {
        return;
    };
    if !items.iter().any(|(k, v)| k == "role" && as_str(v) == Some("user")) {
        return;
    }
    let Some(JValue::Str(content)) = items.iter().find(|(k, _)| k == "content").map(|(_, v)| v.clone()) else {
        return;
    };
    let at = if content.ends_with(message) { content.len() - message.len() } else { 0 };
    let updated = format!("{}{files_block}\n\n{}", &content[..at], &content[at..]);
    wtf16::set(items, "content", JValue::Str(updated));
}
