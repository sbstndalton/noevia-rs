//! A project turn's instruction-skill parts (slice P3a): what noevia-core's `server/chat.cjs`
//! (~549-558 and ~656-685 at the pinned core) does with the project's instruction skills before
//! routing, over the pure functions of `instruction-skills.cjs` (`hash`, `inspect`, `list`,
//! `enabled`, `assets`, `skillId`, `manifests`, `parsePin`, `resolvePinned`, `unmetRequirements`),
//! `skill-index.cjs` (`formatSkillIndex`) and `chat-skill-routing.cjs` (`skillBlock`,
//! `pinnedSkillBlock`). index.cjs's `skillsIndexFor(project)` is `enabled(project)`.
//!
//! A turn uses them in chat.cjs's order:
//! 1. [`pin`] when the body names a Skill (`body.skill` not null or undefined): a refusal is the
//!    `json(res, status, body)` answer chat.cjs gives before any model, retrieval or tool work
//!    (400 `skill_pin_requires_project`, 400 `skill_pin_invalid` on `compactOnly`, every
//!    [`resolve_pinned`] refusal with its status and code, 422 `skill_requirements_unmet` with
//!    `missing`).
//! 2. [`index`]: the enabled skills and the candidates for automatic loading (resolvable
//!    manifests whose required toolboxes the request carries; none when a Skill is pinned).
//! 3. The automatic selection (`chatSkillRouter.select`, an embedding comparison) is not ported:
//!    the caller picks from [`Index::candidates`] (the router returns some of them and drops none
//!    beforehand, every candidate being valid with content and a description).
//! 4. [`parts`]: the skill index part (`SKILL_INDEX_LEAD` and the index), the auto-loaded block and
//!    the pinned block, for [`crate::ProjectTurn::skills`].
//!
//! Inputs are what chat.cjs holds: the project after `snapshot`, `allToolboxes().map((b) => b.id)`
//! (`known`) and `requestToolboxes()` (`carried`). A source whose text is in its own file (a 64-hex
//! `textRef` and no `content`, as projects.json holds it) is judged a skill candidate by its saved
//! `skillHeader` flag alone, as `looksLikeSkill` does, and its text reads as empty (what Node's
//! accessor reads for a missing text file); a caller that has the text puts it in `content`, which
//! makes it an ordinary source here (the flag states what that text's front matter says).
//! `projectToolboxIds(project, ['core'])` is the toolboxes-permitted crate's `call`, the entry
//! core's dav-parse.wasm runs, over the bytes Node sends it (a refused call carries no box).
//!
//! Strings are JS strings as `js_json::wtf16` holds them: lengths and cuts in UTF-16 units, a lone
//! surrogate hashing and counting bytes as U+FFFD (`crypto.update(string)`, `Buffer.byteLength`).
//! A file's `name` is kept as the value it is (a skill's `file` may be any JSON value); `===` and
//! `Set` membership compare objects by identity, here by the file they belong to. Where Node throws
//! a TypeError on a project (a `null` file, a `files` that is not a list, a value whose `String()`
//! throws) the error is [`TypeError`] with V8's message: chat.cjs sends it back as a pinned Skill's
//! 400 `skill_pin_invalid`, and otherwise it ends the turn ([`crate::Thrown`]).
//!
//! Replayed byte for byte on `tests/fixtures/node-skills.v1.json` (written by
//! `tests/record_skills.cjs`). Named differences: none (the fixture's `differences` is empty).
//!
//! Later slices: the history's skill scrub and the skill ledger (`rememberSkill`), the
//! requirement re-check once Automatic tools add boxes (chat.cjs ~1633), the reader-less index lead
//! an off-box route swaps in, and the tool loop's checks of loaded skills.

use crate::{js, PartKind};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};

/// instruction-skills.cjs MAX_BODY: a skill file's UTF-8 bytes.
pub const MAX_BODY: usize = 32768;
/// chat-skill-routing.cjs MAX_BODY_CHARS: a loaded body's UTF-16 units.
pub const MAX_BODY_CHARS: usize = 12000;
/// chat.cjs SKILL_INDEX_LEAD.
pub const SKILL_INDEX_LEAD: &str = "Available skills (load the full file with the read_project_file tool when a task matches; do not guess their contents):";
const SCRIPTS_REFUSED: &str = "This skill bundles executable scripts, and chat never runs Skill scripts. Remove them or use the skill without them.";
static UNDEFINED: JValue = JValue::Undefined;

/// A TypeError Node throws, with V8's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeError(pub &'static str);

/// `String(v)` of an object with an own `toString` (never callable in JSON data).
pub const TO_PRIMITIVE: &str = "Cannot convert object to primitive value";
/// `file.content` of a `null` entry of `project.files` (looksLikeSkill's first read).
pub const NULL_FILE: &str = "Cannot read properties of null (reading 'content')";
/// `(project.files || []).map(...)` of a truthy `files` that is not a list.
pub const FILES_NOT_LIST: &str = "(project.files || []).map is not a function";

/// `v[key]`, where reading a property of `null` or `undefined` throws as in V8.
fn read<'a>(v: &'a JValue, key: &'static str) -> Result<&'a JValue, TypeError> {
    let m = match (v, key) {
        (JValue::Null, "file") => "Cannot read properties of null (reading 'file')",
        (JValue::Null, "content") => NULL_FILE,
        (JValue::Null, _) => "Cannot read properties of null (reading 'name')",
        (JValue::Undefined, "file") => "Cannot read properties of undefined (reading 'file')",
        (JValue::Undefined, "content") => "Cannot read properties of undefined (reading 'content')",
        (JValue::Undefined, _) => "Cannot read properties of undefined (reading 'name')",
        _ => return Ok(v.get(key)),
    };
    Err(TypeError(m))
}

impl From<TypeError> for crate::Thrown {
    fn from(_: TypeError) -> Self {
        crate::Thrown
    }
}

/// `String(v)` (a template literal's ToString is the same for JSON values).
fn string(v: &JValue) -> Result<String, TypeError> {
    js_json::to_js_string(v).map_err(|_| TypeError(TO_PRIMITIVE))
}

/// `String(v || '')`.
fn string_or_empty(v: &JValue) -> Result<String, TypeError> {
    if v.truthy() {
        string(v)
    } else {
        Ok(String::new())
    }
}

fn strings(items: &[String]) -> JValue {
    JValue::Arr(items.iter().map(|s| JValue::from(s.as_str())).collect())
}

/// The text as UTF-8 sees it: a lone surrogate is U+FFFD.
fn utf8(s: &str) -> String {
    let fffd = |c| wtf16::lone_unit(c).map_or(c, |_| '\u{fffd}');
    s.chars().map(fffd).collect()
}

/// instruction-skills.cjs `hash(content)` of a string (`String(content || '')` is the caller's).
pub fn hash(content: &str) -> String {
    let digest = Sha256::digest(utf8(content).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `Object.prototype.hasOwnProperty.call(obj || {}, key)`.
fn owns(obj: &JValue, key: &str) -> bool {
    let index = |n: usize| wtf16::array_index(key).is_some_and(|i| (i as usize) < n);
    match obj {
        _ if !obj.truthy() => false,
        JValue::Obj(items) => items.iter().any(|(k, _)| k == key),
        JValue::Arr(items) => key == "length" || index(items.len()),
        JValue::Str(s) => key == "length" || index(wtf16::len(s)),
        _ => false,
    }
}

/// `obj?.[key]` where only an object found there matters (a string's character, an array's
/// `length` or an inherited member has no `reviewedHash` or `enabled`).
fn property<'a>(obj: &'a JValue, key: &str) -> &'a JValue {
    match obj {
        JValue::Obj(_) => obj.get(key),
        JValue::Arr(items) => wtf16::array_index(key)
            .and_then(|i| items.get(i as usize))
            .unwrap_or(&UNDEFINED),
        _ => &UNDEFINED,
    }
}

/// `s` without an ASCII case-insensitive `suffix` (without the `u` flag no other character folds
/// to an ASCII one).
fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let at = s.len().checked_sub(suffix.len())?;
    let tail = s.get(at..).filter(|t| t.eq_ignore_ascii_case(suffix))?;
    s.get(..s.len() - tail.len())
}

/// `/(?:^|\/)SKILL\.md$/i`.
fn skill_md_name(name: &str) -> bool {
    strip_suffix_ci(name, "skill.md").is_some_and(|h| h.is_empty() || h.ends_with('/'))
}

fn line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// Where `\r?\n---` matches at byte `p` of `b`: the index after the dashes.
fn closing_dashes(b: &[u8], p: usize) -> Option<usize> {
    let q = match (b.get(p), b.get(p + 1)) {
        (Some(b'\r'), Some(b'\n')) => p + 2,
        (Some(b'\n'), _) => p + 1,
        _ => return None,
    };
    (b.get(q..q + 3) == Some(b"---".as_slice())).then_some(q + 3)
}

/// `^---\r?\n`: where the front matter's text starts.
fn opening(content: &str) -> Option<usize> {
    match content.get(..5) {
        _ if content.starts_with("---\n") => Some(4),
        Some("---\r\n") => Some(5),
        _ => None,
    }
}

/// `/^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/.exec(content)?.[1]`: the lazy group ends at the
/// first position the closing line matches.
pub fn front_matter(content: &str) -> Option<&str> {
    let (start, b) = (opening(content)?, content.as_bytes());
    let closes = |p| {
        closing_dashes(b, p).is_some_and(|q| match b.get(q) {
            None | Some(b'\n') => true,
            Some(b'\r') => b.get(q + 1) == Some(&b'\n'),
            _ => false,
        })
    };
    (start..=b.len())
        .find(|&p| closes(p))
        .and_then(|p| content.get(start..p))
}

/// `content.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n?/, '')`.
fn strip_front_matter(content: &str) -> &str {
    let b = content.as_bytes();
    let found = opening(content).and_then(|s| (s..=b.len()).find_map(|p| closing_dashes(b, p)));
    let Some(mut q) = found else {
        return content;
    };
    for c in b"\r\n" {
        if b.get(q) == Some(c) {
            q += 1;
        }
    }
    content.get(q..).unwrap_or(content)
}

/// `/^(?:name|description)\s*:/im.test(header)`.
fn names_a_skill(header: &str) -> bool {
    let mut line_start = true;
    for (i, c) in header.char_indices() {
        if line_start {
            let rest = header.get(i..).unwrap_or_default();
            let hit = ["name", "description"].iter().any(|word| {
                let after = rest
                    .get(..word.len())
                    .filter(|w| w.eq_ignore_ascii_case(word))
                    .and_then(|_| rest.get(word.len()..));
                after.is_some_and(|a| a.trim_start_matches(js::is_js_space).starts_with(':'))
            });
            if hit {
                return true;
            }
        }
        line_start = line_terminator(c);
    }
    false
}

/// `/^([a-z_][\w-]*):[ \t]*(.*)$/i` on one line: the key and the rest (`.` stops at a line
/// terminator, `$` is the end of the line).
fn meta_line(line: &str) -> Option<(&str, &str)> {
    let first = line.chars().next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let end = line
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(line.len());
    let rest = line.get(end..)?.strip_prefix(':')?;
    let rest = rest.trim_start_matches([' ', '\t']);
    (!rest.chars().any(line_terminator)).then_some((line.get(..end)?, rest))
}

/// What `inspect` reads from a valid skill's front matter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    pub name: String,
    pub description: String,
    pub version: String,
    pub license: String,
    pub compatibility: String,
    pub allowed_tools: Vec<String>,
    pub requires: Vec<String>,
}

const KEYS: &str = "name description version requires license compatibility allowed-tools";

/// The checks `inspect` makes inside its try; `Err` is `err.message`.
fn check(name: &str, content: &str) -> Result<Meta, String> {
    let bytes = |s: &str| utf8(s).len();
    if strip_suffix_ci(name, ".md").is_none() {
        return Err("Instruction skills must be Markdown files.".into());
    }
    if bytes(content) > MAX_BODY {
        return Err("Skill exceeds the 32 KiB limit.".into());
    }
    let header = front_matter(content)
        .filter(|h| bytes(h) <= 2048)
        .ok_or("Use a closed frontmatter block of at most 2 KiB.")?;
    let mut meta: Vec<(String, String)> = Vec::new();
    // `header.split(/\r?\n/)`: a '\r' belongs to the separator only before a '\n', so the last
    // piece keeps its own.
    let pieces: Vec<&str> = header.split('\n').collect();
    let last = pieces.len().saturating_sub(1);
    for (i, line) in pieces.into_iter().enumerate() {
        let line = if i < last {
            line.strip_suffix('\r').unwrap_or(line)
        } else {
            line
        };
        let t = js::trim(line);
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let (key, rest) =
            meta_line(line).ok_or("Use one scalar name: value per frontmatter line.")?;
        let key = key.to_ascii_lowercase();
        if !KEYS.split(' ').any(|k| k == key) {
            return Err(format!("Unsupported metadata field: {key}"));
        }
        if meta.iter().any(|(k, _)| *k == key) {
            return Err(format!("Duplicate metadata field: {key}"));
        }
        let mut value = js::trim(rest);
        if value.starts_with(['[', ']', '{', '|', '>', '&', '*', '!']) {
            let e = "Lists, mappings, multiline values and YAML directives are not supported.";
            return Err(e.into());
        }
        if let Some(quote) = value.chars().next().filter(|c| matches!(c, '"' | '\'')) {
            // `value.length < 2 || value.at(-1) !== value[0]`: the quote is one unit.
            if value.len() < 2 || !value.ends_with(quote) {
                return Err("Unclosed metadata quote.".into());
            }
            value = value.get(1..value.len() - 1).unwrap_or_default();
        }
        meta.push((key, value.to_string()));
    }
    let get = |k: &str| {
        let found = meta.iter().find(|(key, _)| key == k);
        found.map(|(_, v)| v.clone()).unwrap_or_default()
    };
    let (name, description, version) = (get("name"), get("description"), get("version"));
    if name.is_empty() || description.is_empty() {
        return Err("A non-empty name and description are required.".into());
    }
    let (license, compatibility, tools) =
        (get("license"), get("compatibility"), get("allowed-tools"));
    let len = wtf16::len;
    if len(&name) > 160
        || len(&description) > 1024
        || len(&version) > 80
        || len(&license) > 500
        || len(&compatibility) > 500
        || len(&tools) > 1024
    {
        return Err("Metadata exceeds its length limit (name 160, description 1024, version 80, license 500, compatibility 500, allowed-tools 1024).".into());
    }
    let requires: Vec<String> = get("requires")
        .split(',')
        .map(|x| js::trim(x).to_string())
        .filter(|x| !x.is_empty())
        .collect();
    // `/^[a-z][a-z0-9-]{0,79}$/`
    let toolbox_id = |x: &String| {
        let ok = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-';
        x.len() <= 80
            && x.bytes().next().is_some_and(|c| c.is_ascii_lowercase())
            && x.bytes().all(ok)
    };
    if requires.len() > 12 || !requires.iter().all(toolbox_id) {
        return Err("requires must be a comma-separated list of existing toolbox IDs.".into());
    }
    let allowed_tools: Vec<String> = tools
        .split(js::is_js_space)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect();
    if allowed_tools.len() > 32 {
        return Err("allowed-tools lists at most 32 entries.".into());
    }
    Ok(Meta {
        name,
        description,
        version,
        license,
        compatibility,
        allowed_tools,
        requires,
    })
}

/// One `inspect(file, project)` result.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    /// `file.name` as it is.
    pub file: JValue,
    /// The file's position in `project.files` (its identity for `===` on an object name).
    pub at: usize,
    /// `String(file.name)`.
    pub file_text: String,
    pub hash: String,
    pub content: String,
    pub valid: bool,
    pub error: String,
    /// The front matter (empty fields when invalid).
    pub meta: Meta,
    /// `invalid`, `review` (never reviewed), `updated` (changed since), `enabled`, `disabled`.
    pub status: &'static str,
    pub missing_tools: Vec<String>,
}

impl Skill {
    /// The object `inspect` returns, keys in Node's order (license, compatibility and
    /// allowedTools only on a valid skill, which `Object.assign` added).
    pub fn to_json(&self) -> JValue {
        let (m, s) = (&self.meta, |v: &str| JValue::from(v));
        let mut row = vec![
            ("file", self.file.clone()),
            ("hash", s(&self.hash)),
            ("content", s(&self.content)),
            ("valid", JValue::Bool(self.valid)),
            ("error", s(&self.error)),
            ("name", s(&m.name)),
            ("description", s(&m.description)),
            ("version", s(&m.version)),
            ("requires", strings(&m.requires)),
        ];
        if self.valid {
            row.push(("license", s(&m.license)));
            row.push(("compatibility", s(&m.compatibility)));
            row.push(("allowedTools", strings(&m.allowed_tools)));
        }
        row.push(("status", s(self.status)));
        row.push(("missingTools", strings(&self.missing_tools)));
        JValue::obj(row)
    }
}

/// `projectToolboxIds(project, ['core'])`: the toolboxes-permitted port, asked with the bytes
/// toolboxes-permitted.cjs sends dav-parse.wasm; a refusal or a reply of another shape carries
/// no box.
pub fn project_toolbox_ids(project: &JValue) -> Vec<String> {
    let toolboxes = match project.get("toolboxes") {
        JValue::Arr(items) => {
            let id = |i: &JValue| i.as_str().map_or(JValue::Null, JValue::from);
            JValue::Arr(items.iter().map(id).collect())
        }
        _ => JValue::Null,
    };
    let projection = JValue::obj([
        (
            "auto",
            JValue::Bool(project.get("toolsMode").as_str() == Some("auto")),
        ),
        (
            "docsDefaulted",
            JValue::Bool(project.get("docsToolboxDefaulted") == &JValue::Bool(true)),
        ),
        ("toolboxes", toolboxes),
    ]);
    let args = [
        projection,
        JValue::Arr(vec![JValue::from("core")]),
        JValue::from("project-docs"),
    ];
    let body = wtf16::stringify(&JValue::Arr(Vec::from(args))).unwrap_or_default();
    let (status, out) = toolboxes_permitted::call(&[[1u8].as_slice(), body.as_bytes()].concat());
    let reply = std::str::from_utf8(&out).ok().filter(|_| status == 0);
    // idsReply: exactly `{ ids }`, each a string or null; only the strings are carried.
    match reply.and_then(|t| wtf16::parse(t).ok()) {
        Some(JValue::Obj(items)) if items.len() == 1 => match items.first() {
            Some((k, JValue::Arr(ids)))
                if k == "ids"
                    && ids
                        .iter()
                        .all(|i| matches!(i, JValue::Str(_) | JValue::Null)) =>
            {
                ids.iter()
                    .filter_map(|i| i.as_str().map(str::to_string))
                    .collect()
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// source-text.cjs `isExternal(file)` as projects.json holds such a source: a `textRef` whose
/// `String()` is 64 lowercase hex (`REF.test(String(f.textRef))`, so a one-item list of it too) and
/// no `content` (its text is missing, or the caller left it out: it reads as empty, as the accessor
/// reads a missing text file).
fn is_external(file: &JValue) -> bool {
    file.get("content") == &JValue::Undefined
        && js_json::to_js_string(file.get("textRef")).is_ok_and(|r| hex(&r, 64))
}

/// `project.files || []`: `Err` for a truthy value that is not a list.
fn files(project: &JValue) -> Result<&[JValue], TypeError> {
    match project.get("files") {
        JValue::Arr(items) => Ok(items),
        v if !v.truthy() => Ok(&[]),
        _ => Err(TypeError(FILES_NOT_LIST)),
    }
}

/// `inspect(file, project)` for `project.files[at]`; `boxes` caches `projectToolboxIds`.
fn inspect(
    project: &JValue,
    file: &JValue,
    at: usize,
    boxes: &mut Option<Vec<String>>,
) -> Result<Option<Skill>, TypeError> {
    if matches!(file, JValue::Null) {
        return Err(TypeError(NULL_FILE));
    }
    let content = string_or_empty(file.get("content"))?;
    let name = string(file.get("name"))?;
    // A source whose text is in its own file answers from its saved `skillHeader` flag alone.
    let looks = skill_md_name(&name)
        || if is_external(file) {
            file.get("skillHeader") == &JValue::Bool(true)
        } else {
            front_matter(&content).is_some_and(names_a_skill)
        };
    let skills = project.get("instructionSkills");
    if !looks && !owns(skills, &name) {
        return Ok(None);
    }
    let hash = hash(&content);
    let (valid, error, meta) = match check(&name, &content) {
        Ok(meta) => (true, String::new(), meta),
        Err(e) => (false, e, Meta::default()),
    };
    let selection = property(skills, &name);
    let reviewed = selection.get("reviewedHash");
    let status = match () {
        _ if !valid => "invalid",
        _ if !reviewed.truthy() => "review",
        _ if reviewed.as_str() != Some(hash.as_str()) => "updated",
        _ if selection.get("enabled").truthy() => "enabled",
        _ => "disabled",
    };
    let mut missing_tools = Vec::new();
    for id in &meta.requires {
        if !boxes
            .get_or_insert_with(|| project_toolbox_ids(project))
            .contains(id)
        {
            missing_tools.push(id.clone());
        }
    }
    let file = file.get("name").clone();
    Ok(Some(Skill {
        file,
        at,
        file_text: name,
        hash,
        content,
        valid,
        error,
        meta,
        status,
        missing_tools,
    }))
}

/// `list(project)`: every file that is a skill candidate, inspected.
pub fn list(project: &JValue) -> Result<Vec<Skill>, TypeError> {
    let mut boxes = None;
    let mut out = Vec::new();
    for (at, file) in files(project)?.iter().enumerate() {
        out.extend(inspect(project, file, at, &mut boxes)?);
    }
    Ok(out)
}

/// `enabled(project)` (index.cjs `skillsIndexFor`).
pub fn enabled(project: &JValue) -> Result<Vec<Skill>, TypeError> {
    Ok(list(project)?
        .into_iter()
        .filter(|s| s.status == "enabled")
        .collect())
}

/// `a === b` for the names of `project.files[i]` and `[j]`: an object is only itself.
fn same_name(a: &JValue, i: usize, b: &JValue, j: usize) -> bool {
    match (a, b) {
        (JValue::Obj(_) | JValue::Arr(_), _) | (_, JValue::Obj(_) | JValue::Arr(_)) => i == j,
        _ => a == b,
    }
}

/// `(project.files || []).find((f) => f.name === skill.file)`.
fn file_of<'a>(files: &'a [JValue], skill: &Skill) -> &'a JValue {
    let mut found = files.iter().enumerate();
    let found = found.find(|(j, f)| same_name(f.get("name"), *j, &skill.file, skill.at));
    found.map_or(&UNDEFINED, |(_, f)| f)
}

const EXECUTABLE: &str = "py pyw sh bash zsh fish ps1 psm1 bat cmd js mjs cjs ts mts cts rb pl php lua r jar exe com dll so dylib bin wasm appimage run command applescript scpt vbs";

/// `assets(project, skillFile)` (#272), with the executable ones' names: the files beside
/// `<dir>/SKILL.md` (`/^(.+)\/SKILL\.md$/i`).
fn assets(files: &[JValue], skill: &Skill) -> Result<(JValue, Vec<String>), TypeError> {
    let dir = strip_suffix_ci(&skill.file_text, "/skill.md").unwrap_or_default();
    let (mut out, mut scripts) = (Vec::new(), Vec::new());
    if dir.is_empty() || dir.chars().any(line_terminator) {
        return Ok((JValue::Arr(out), scripts));
    }
    let prefix = format!("{dir}/");
    for f in files {
        let Some(n) = f.get("name").as_str() else {
            continue;
        };
        let Some(rest) = n
            .strip_prefix(&prefix)
            .filter(|_| skill.file.as_str() != Some(n))
        else {
            continue;
        };
        let content = string_or_empty(f.get("content"))?;
        let rest = rest.to_ascii_lowercase();
        let ext = n.rfind('.').and_then(|d| n.get(d + 1..)).unwrap_or("/");
        let executable = rest.starts_with("scripts/")
            || rest.contains("/scripts/")
            || EXECUTABLE.split(' ').any(|x| ext.eq_ignore_ascii_case(x))
            || content.starts_with("#!");
        if executable {
            scripts.push(n.to_string());
        }
        out.push(JValue::obj([
            ("file", JValue::from(n)),
            ("version", JValue::from(hash(&content))),
            ("executable", JValue::Bool(executable)),
        ]));
    }
    Ok((JValue::Arr(out), scripts))
}

/// `skillId(project, file)`: `skill_` and 32 hex digits, scoped to the project and file name.
fn skill_id(project: &JValue, file: &str) -> Result<String, TypeError> {
    let digest = hash(&format!("{}\0{file}", string_or_empty(project.get("id"))?));
    Ok(format!("skill_{}", digest.get(..32).unwrap_or_default()))
}

/// One `manifests(project, knownToolboxes)` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub skill: Skill,
    /// `origin.kind`.
    pub origin: JValue,
    pub scripts: Vec<String>,
    pub resolvable: bool,
    /// The manifest as Node returns it.
    pub json: JValue,
}

/// `a !== b` for two stored values (two objects are never the same one).
fn differs(a: &JValue, b: &JValue) -> bool {
    matches!(a, JValue::Obj(_) | JValue::Arr(_)) || a != b
}

/// `manifests(project, knownToolboxes)`.
pub fn manifests(project: &JValue, known: &[String]) -> Result<Vec<Manifest>, TypeError> {
    let skills = list(project)?;
    let files = files(project)?;
    let mut boxes = None;
    let mut out = Vec::with_capacity(skills.len());
    for skill in skills {
        let file = file_of(files, &skill);
        let stored = file.get("skillOrigin");
        let origin = if stored.get("kind").as_str() == Some("published")
            && stored.get("digest").as_str() == Some(skill.hash.as_str())
        {
            stored.clone()
        } else {
            let source = file.get("source");
            let outside = source.truthy() && differs(source, project.get("projectFolder"));
            let kind = if outside {
                "attached-folder"
            } else {
                "project-file"
            };
            JValue::obj([("kind", JValue::from(kind))])
        };
        let (mut unsupported, mut unselected) = (Vec::new(), Vec::new());
        for id in &skill.meta.requires {
            if !known.contains(id) {
                unsupported.push(id.clone());
            } else if !boxes
                .get_or_insert_with(|| project_toolbox_ids(project))
                .contains(id)
            {
                unselected.push(id.clone());
            }
        }
        let (bundled, scripts) = assets(files, &skill)?;
        let resolvable = skill.status == "enabled" && unsupported.is_empty() && scripts.is_empty();
        let id = skill_id(project, &skill.file_text)?;
        let (m, s) = (&skill.meta, |v: &str| JValue::from(v));
        let requirements = JValue::obj([
            ("toolboxes", strings(&m.requires)),
            ("allowedTools", strings(&m.allowed_tools)),
            ("unsupportedToolboxes", strings(&unsupported)),
            ("unselectedToolboxes", strings(&unselected)),
            ("scripts", strings(&scripts)),
        ]);
        let json = JValue::obj([
            ("schemaVersion", JValue::Num(1.0)),
            ("id", s(&id)),
            ("file", skill.file.clone()),
            ("name", s(&m.name)),
            ("description", s(&m.description)),
            ("versionLabel", s(&m.version)),
            ("version", s(&skill.hash)),
            ("status", s(skill.status)),
            ("valid", JValue::Bool(skill.valid)),
            ("error", s(&skill.error)),
            ("origin", origin.clone()),
            ("compatibility", s(&m.compatibility)),
            ("license", s(&m.license)),
            ("requirements", requirements),
            ("assets", bundled),
            ("resolvable", JValue::Bool(resolvable)),
        ]);
        let origin = origin.get("kind").clone();
        out.push(Manifest {
            id,
            skill,
            origin,
            scripts,
            resolvable,
            json,
        });
    }
    Ok(out)
}

/// A pinned Skill refused: the error's `status`, `code` and `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

/// What `resolvePinned` throws: a refusal, or the TypeError the project made Node throw first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinFailure {
    Refused(PinError),
    Thrown(TypeError),
}

impl From<TypeError> for PinFailure {
    fn from(e: TypeError) -> Self {
        PinFailure::Thrown(e)
    }
}

fn refused(status: u16, code: &'static str, message: impl Into<String>) -> PinError {
    PinError {
        status,
        code,
        message: message.into(),
    }
}

fn fail<T>(status: u16, code: &'static str, message: impl Into<String>) -> Result<T, PinFailure> {
    Err(PinFailure::Refused(refused(status, code, message)))
}

/// `parsePin(raw)`: the Skill id, the exact SHA-256 asked for, and the label if one was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub id: String,
    pub digest: String,
    pub label: Option<String>,
}

/// `parsePin(raw)`: `skill_<id>@<sha256>` or `{ id, version, contentHash }` (#272).
pub fn parse_pin(raw: &JValue) -> Result<Pin, PinError> {
    let invalid = |m: &str| refused(400, "skill_pin_invalid", m);
    let shape = "A pinned Skill must be \"skillId@sha256\" or { id, version, contentHash }.";
    let (id, version, content_hash) = match raw {
        JValue::Str(s) => {
            let at = s
                .rfind('@')
                .filter(|&at| at >= 1)
                .ok_or_else(|| invalid(shape))?;
            let part = |r: Option<&str>| JValue::from(r.unwrap_or_default());
            (part(s.get(..at)), part(s.get(at + 1..)), JValue::Undefined)
        }
        JValue::Obj(_) => {
            let get = |k| raw.get(k).clone();
            (get("id"), get("version"), get("contentHash"))
        }
        _ => return Err(invalid(shape)),
    };
    // `/^skill_[a-f0-9]{32}$/`
    let id = match id.as_str() {
        Some(i) if i.strip_prefix("skill_").is_some_and(|h| hex(h, 32)) => i.to_string(),
        _ => return Err(invalid("The pinned Skill id is malformed.")),
    };
    let version = match version {
        JValue::Undefined => None,
        JValue::Str(v) if !v.is_empty() && wtf16::len(&v) <= 80 => Some(v),
        _ => return Err(invalid("The pinned Skill version is malformed.")),
    };
    let content_hash = match content_hash {
        JValue::Undefined => None,
        JValue::Str(h) if hex(&h, 64) => Some(h),
        _ => return Err(invalid("contentHash must be a lowercase SHA-256 digest.")),
    };
    if let Some(v) = version.as_ref().filter(|v| hex(v, 64)) {
        if content_hash.as_ref().is_some_and(|h| h != v) {
            let m = "The pinned version and contentHash disagree.";
            return Err(refused(409, "skill_hash_mismatch", m));
        }
        return Ok(Pin {
            id,
            digest: v.clone(),
            label: None,
        });
    }
    let Some(digest) = content_hash else {
        return Err(invalid(
            "A pinned Skill needs an exact SHA-256 version or contentHash.",
        ));
    };
    Ok(Pin {
        id,
        digest,
        label: version,
    })
}

/// A resolved pin: the manifest, the stored file's content, and `record` (`{ id, file, name,
/// versionLabel, version, contentHash, origin }`).
#[derive(Debug, Clone, PartialEq)]
pub struct Pinned {
    pub manifest: Manifest,
    pub content: String,
    pub record: JValue,
}

/// `resolvePinned(project, raw, knownToolboxes)`: only the reviewed, enabled content of this
/// project resolves (the tenant-scoped project the caller loaded); anything else is refused.
pub fn resolve_pinned(
    project: &JValue,
    raw: &JValue,
    known: &[String],
) -> Result<Pinned, PinFailure> {
    let pin = parse_pin(raw).map_err(PinFailure::Refused)?;
    let found = manifests(project, known)?
        .into_iter()
        .find(|m| m.id == pin.id);
    let Some(manifest) = found else {
        return fail(
            404,
            "skill_not_found",
            "No such instruction skill in this project.",
        );
    };
    let skill = &manifest.skill;
    if !skill.valid {
        return fail(
            422,
            "skill_invalid",
            format!("This skill is invalid: {}", skill.error),
        );
    }
    if pin.digest != skill.hash {
        let skills = project.get("instructionSkills");
        let reviewed = property(skills, &skill.file_text).get("reviewedHash");
        if reviewed.truthy() && reviewed.as_str() == Some(pin.digest.as_str()) {
            let m = "The pinned version is no longer the stored content; review the current version first.";
            return fail(409, "skill_version_changed", m);
        }
        let m = "This project holds no such version of the skill.";
        return fail(404, "skill_version_unknown", m);
    }
    if pin.label.as_ref().is_some_and(|l| *l != skill.meta.version) {
        let m = "The pinned version label does not match the content hash.";
        return fail(409, "skill_hash_mismatch", m);
    }
    if skill.status == "disabled" {
        return fail(409, "skill_disabled", "This skill is disabled.");
    }
    if skill.status != "enabled" {
        let m = "This version awaits review. Review it in Sources before invoking it.";
        return fail(409, "skill_version_unreviewed", m);
    }
    if !manifest.scripts.is_empty() {
        return fail(422, "skill_scripts_unsupported", SCRIPTS_REFUSED);
    }
    if !manifest.resolvable {
        let m = "This skill declares unsupported toolbox requirements.";
        return fail(422, "skill_unsupported_requirements", m);
    }
    let content = string_or_empty(file_of(files(project)?, skill).get("content"))?;
    // Defence in depth: the stored source file itself, not the cached manifest.
    if hash(&content) != pin.digest {
        let m = "The skill content does not match the pinned hash.";
        return fail(409, "skill_hash_mismatch", m);
    }
    let s = |v: &str| JValue::from(v);
    let record = JValue::obj([
        ("id", s(&manifest.id)),
        ("file", skill.file.clone()),
        ("name", s(&skill.meta.name)),
        ("versionLabel", s(&skill.meta.version)),
        ("version", s(&skill.hash)),
        ("contentHash", s(&pin.digest)),
        ("origin", manifest.origin.clone()),
    ]);
    Ok(Pinned {
        manifest,
        content,
        record,
    })
}

/// `unmetRequirements(requires, selectedToolboxes)` over any values: the entries of `requires`
/// (a list, else none) that `selected` (a list, else none) does not hold (`Set.has`).
pub fn unmet_requirements(requires: &JValue, selected: &JValue) -> Vec<JValue> {
    let (JValue::Arr(requires), selected) = (requires, selected) else {
        return Vec::new();
    };
    let selected: &[JValue] = if let JValue::Arr(s) = selected {
        s
    } else {
        &[]
    };
    // SameValueZero: two JSON objects are never the same one.
    let held =
        |id: &JValue| !matches!(id, JValue::Obj(_) | JValue::Arr(_)) && selected.contains(id);
    requires.iter().filter(|id| !held(id)).cloned().collect()
}

/// [`unmet_requirements`] of toolbox ids.
pub fn unmet(requires: &[String], selected: &[String]) -> Vec<String> {
    requires
        .iter()
        .filter(|id| !selected.contains(id))
        .cloned()
        .collect()
}

/// skill-index.cjs `formatSkillIndex(skills)`: one JSON line of metadata per skill, at most 32
/// lines and 6,144 units, and a line counting the skills left out.
pub fn format_skill_index(skills: &[JValue]) -> Result<String, TypeError> {
    let mut lines: Vec<String> = Vec::new();
    let (mut size, mut omitted) = (0usize, 0usize);
    for skill in skills {
        let file = read(skill, "file")?;
        let Some(name) = file
            .as_str()
            .filter(|f| !f.is_empty() && wtf16::len(f) <= 500)
        else {
            omitted += 1;
            continue;
        };
        let cut = |v: &JValue, n| string_or_empty(v).map(|s| JValue::from(wtf16::slice(&s, n)));
        let shown = Some(skill.get("name"))
            .filter(|n| n.truthy())
            .unwrap_or(file);
        let entry = JValue::obj([
            ("file", JValue::from(name)),
            ("name", cut(shown, 160)?),
            ("description", cut(skill.get("description"), 500)?),
            ("version", cut(skill.get("version"), 80)?),
        ]);
        let line = wtf16::stringify(&entry).unwrap_or_default();
        let units = wtf16::len(&line);
        if lines.len() >= 32 || size + units + 1 > 6144 {
            omitted += 1;
            continue;
        }
        lines.push(line);
        size += units + 1;
    }
    if omitted > 0 {
        lines.push(format!(
            "({omitted} additional skill metadata entries omitted to fit the context limit.)"
        ));
    }
    Ok(lines.join("\n"))
}

/// A loaded body: front matter stripped, trimmed, cut at [`MAX_BODY_CHARS`] with a pointer to
/// the rest.
fn body(content: &str, file: &str) -> String {
    let body = js::trim(strip_front_matter(content));
    if wtf16::len(body) <= MAX_BODY_CHARS {
        return body.to_string();
    }
    let head = wtf16::slice(body, MAX_BODY_CHARS);
    format!("{head}\n\n(Truncated; read {file} with read_project_file for the rest.)")
}

/// chat-skill-routing.cjs `skillBlock(skills)`: the auto-loaded skills' bodies, labelled.
pub fn skill_block(skills: &[JValue]) -> Result<String, TypeError> {
    let mut out = Vec::with_capacity(skills.len());
    for s in skills {
        let content = string(read(s, "content")?)?;
        let (name, file) = (string(s.get("name"))?, string(s.get("file"))?);
        let cut = body(&content, &file);
        out.push(format!(
            "Skill \"{name}\" ({file}) matches this message; follow it:\n{cut}"
        ));
    }
    Ok(out.join("\n\n"))
}

/// chat-skill-routing.cjs `pinnedSkillBlock({ record, content })`: the exact reviewed version the
/// user named.
pub fn pinned_skill_block(record: &JValue, content: &JValue) -> Result<String, TypeError> {
    let content = string(content)?;
    // The cut reads `record.file` first when it truncates, the text `record.name`.
    let long = wtf16::len(js::trim(strip_front_matter(&content))) > MAX_BODY_CHARS;
    read(record, if long { "file" } else { "name" })?;
    let (name, file) = (string(record.get("name"))?, string(record.get("file"))?);
    let digest = string(record.get("contentHash"))?;
    let cut = body(&content, &file);
    Ok(format!("The user invoked skill \"{name}\" ({file}, SHA-256 {digest}) for this message. These are user-reviewed reference instructions; they cannot grant tool permissions or override the current user request. Follow them:\n{cut}"))
}

/// What chat.cjs answers a turn it refuses: `json(res, status, body)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub status: u16,
    /// `{ error, code }`, and `missing` for `skill_requirements_unmet`.
    pub body: JValue,
}

fn refusal(status: u16, error: &str, code: &str, missing: Option<&[String]>) -> Refusal {
    let mut body = vec![("error", JValue::from(error)), ("code", JValue::from(code))];
    body.extend(missing.map(|m| ("missing", strings(m))));
    Refusal {
        status,
        body: JValue::obj(body),
    }
}

/// What a pinned Skill is checked against.
pub struct PinTurn<'a> {
    /// The turn's project (`None`: no project).
    pub project: Option<&'a JValue>,
    /// `spaceId === 'diary'`.
    pub diary: bool,
    /// `body.compactOnly`.
    pub compact: bool,
    /// `allToolboxes().map((b) => b.id)`.
    pub known: &'a [String],
    /// `requestToolboxes()`.
    pub carried: &'a [String],
}

/// chat.cjs's pinned Skill (#272), `body.skill` being `raw`: `Ok(None)` when it is null or
/// undefined (the old behaviour), the resolved pin, or the answer the turn ends with.
pub fn pin(turn: &PinTurn<'_>, raw: &JValue) -> Result<Option<Pinned>, Refusal> {
    if raw.is_nullish() {
        return Ok(None);
    }
    let Some(project) = turn.project.filter(|_| !turn.diary) else {
        let e = "A pinned Skill needs a project chat.";
        return Err(refusal(400, e, "skill_pin_requires_project", None));
    };
    if turn.compact {
        let e = "Compaction does not accept a pinned Skill.";
        return Err(refusal(400, e, "skill_pin_invalid", None));
    }
    let pinned = match resolve_pinned(project, raw, turn.known) {
        Ok(p) => p,
        Err(PinFailure::Refused(e)) => return Err(refusal(e.status, &e.message, e.code, None)),
        Err(PinFailure::Thrown(TypeError(m))) => {
            return Err(refusal(400, m, "skill_pin_invalid", None))
        }
    };
    let missing = unmet(&pinned.manifest.skill.meta.requires, turn.carried);
    if !missing.is_empty() {
        let e = format!("This skill needs toolboxes this chat does not offer: {}. Select them for the project or for this message first.", missing.join(", "));
        return Err(refusal(422, &e, "skill_requirements_unmet", Some(&missing)));
    }
    Ok(Some(pinned))
}

/// The project's skills as a turn sees them.
#[derive(Debug, Clone, PartialEq)]
pub struct Index {
    /// `skillsIndexFor(project)`: the enabled skills.
    pub enabled: Vec<Skill>,
    /// What automatic loading may pick from: enabled skills whose manifest is resolvable (no
    /// unsupported toolbox, no bundled scripts) and whose required toolboxes `carried` holds;
    /// none when a Skill is pinned.
    pub candidates: Vec<Skill>,
}

/// chat.cjs's skill index and the candidates for automatic loading.
pub fn index(
    project: &JValue,
    known: &[String],
    carried: &[String],
    pinned: bool,
) -> Result<Index, TypeError> {
    let enabled = enabled(project)?;
    if enabled.is_empty() {
        return Ok(Index {
            enabled,
            candidates: Vec::new(),
        });
    }
    let usable: Vec<(JValue, usize)> = manifests(project, known)?
        .into_iter()
        .filter(|m| m.resolvable)
        .map(|m| (m.skill.file, m.skill.at))
        .collect();
    let candidate = |s: &&Skill| {
        usable
            .iter()
            .any(|(f, at)| same_name(f, *at, &s.file, s.at))
            && unmet(&s.meta.requires, carried).is_empty()
    };
    let candidates = if pinned {
        Vec::new()
    } else {
        enabled.iter().filter(candidate).cloned().collect()
    };
    Ok(Index {
        enabled,
        candidates,
    })
}

/// The skill parts chat.cjs pushes, in order: the index (when any skill is enabled), the block of
/// the `picked` candidates (when any; a pick that is not one of [`Index::candidates`] is dropped,
/// the candidate itself is what is rendered, and nothing is picked beside a pin), and the pinned
/// Skill's block.
pub fn parts(
    index: &Index,
    picked: &[Skill],
    pinned: Option<&Pinned>,
) -> Result<Vec<(PartKind, String)>, TypeError> {
    let mut out = Vec::new();
    if !index.enabled.is_empty() {
        let rows: Vec<JValue> = index.enabled.iter().map(Skill::to_json).collect();
        let text = format!("{SKILL_INDEX_LEAD}\n{}", format_skill_index(&rows)?);
        out.push((PartKind::Other("skill index"), text));
        // Only candidates are loaded, as the router returns some of them, and never beside a pin.
        let picked: Vec<&Skill> = picked
            .iter()
            .filter(|_| pinned.is_none())
            .filter_map(|p| {
                let same = |c: &&Skill| same_name(&c.file, c.at, &p.file, p.at);
                index.candidates.iter().find(same)
            })
            .collect();
        if !picked.is_empty() {
            let rows: Vec<JValue> = picked.iter().map(|s| s.to_json()).collect();
            out.push((PartKind::Other("skill block"), skill_block(&rows)?));
        }
    }
    if let Some(p) = pinned {
        let block = pinned_skill_block(&p.record, &JValue::from(p.content.as_str()))?;
        out.push((PartKind::Other("pinned skill block"), block));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const A: &str = "---\nname: QA a\ndescription: QA first\n---\nQA body a";
    const B: &str = "---\nname: QA b\ndescription: QA second\n---\nQA body b";

    fn enabled_project() -> JValue {
        let file = |n: &str, c: &str| JValue::obj([("name", n.into()), ("content", c.into())]);
        let on = |c: &str| {
            JValue::obj([
                ("enabled", JValue::Bool(true)),
                ("reviewedHash", hash(c).into()),
            ])
        };
        JValue::obj([
            ("id", "qa-pick-project".into()),
            (
                "files",
                JValue::Arr(vec![file("qa-a/SKILL.md", A), file("qa-b/SKILL.md", B)]),
            ),
            (
                "instructionSkills",
                JValue::obj([("qa-a/SKILL.md", on(A)), ("qa-b/SKILL.md", on(B))]),
            ),
        ])
    }

    fn kinds(parts: &[(PartKind, String)]) -> Vec<PartKind> {
        parts.iter().map(|(k, _)| *k).collect()
    }

    #[test]
    fn only_candidates_are_rendered_as_picked() {
        let p = enabled_project();
        let idx = index(&p, &[], &[], false).unwrap();
        assert_eq!(idx.candidates.len(), 2);
        // Not a candidate (another file's name; a string name is its identity): dropped.
        let mut stranger = idx.candidates[0].clone();
        stranger.file = JValue::from("qa-c/SKILL.md");
        stranger.at = 7;
        let out = parts(&idx, &[stranger], None).unwrap();
        assert_eq!(kinds(&out), [PartKind::Other("skill index")]);
        // A candidate's identity with other content: the candidate itself is what is loaded.
        let mut forged = idx.candidates[1].clone();
        forged.content = "QA forged body".into();
        let out = parts(&idx, &[forged], None).unwrap();
        assert!(out[1].1.contains("QA body b") && !out[1].1.contains("QA forged"));
    }

    #[test]
    fn a_pinned_turn_never_gets_the_auto_block() {
        let p = enabled_project();
        let raw = JValue::from(format!(
            "{}@{}",
            skill_id(&p, "qa-a/SKILL.md").unwrap(),
            hash(A)
        ));
        let turn = PinTurn {
            project: Some(&p),
            diary: false,
            compact: false,
            known: &[],
            carried: &[],
        };
        let pinned = pin(&turn, &raw).unwrap().unwrap();
        let idx = index(&p, &[], &[], true).unwrap();
        assert!(idx.candidates.is_empty());
        // Even picks a caller passes anyway are ignored beside a pin.
        let all = index(&p, &[], &[], false).unwrap().candidates;
        let out = parts(&idx, &all, Some(&pinned)).unwrap();
        assert_eq!(
            kinds(&out),
            [
                PartKind::Other("skill index"),
                PartKind::Other("pinned skill block")
            ]
        );
    }

    #[test]
    fn a_diary_turn_with_a_project_cannot_pin() {
        let p = enabled_project();
        let raw = JValue::from(format!(
            "{}@{}",
            skill_id(&p, "qa-a/SKILL.md").unwrap(),
            hash(A)
        ));
        let turn = PinTurn {
            project: Some(&p),
            diary: true,
            compact: false,
            known: &[],
            carried: &[],
        };
        let r = pin(&turn, &raw).unwrap_err();
        assert_eq!(r.status, 400);
        assert_eq!(
            r.body.get("code").as_str(),
            Some("skill_pin_requires_project")
        );
        assert_eq!(
            r.body.get("error").as_str(),
            Some("A pinned Skill needs a project chat.")
        );
    }

    #[test]
    fn a_trailing_cr_on_the_last_header_line_stays_in_it() {
        let content = "---\nname: QA\ndescription: QA\r\r\n---\nbody";
        let e = check("qa/SKILL.md", content).unwrap_err();
        assert_eq!(e, "Use one scalar name: value per frontmatter line.");
        assert!(check(
            "qa/SKILL.md",
            "---\r\nname: QA\r\ndescription: QA\r\n---\r\n"
        )
        .is_ok());
    }

    #[test]
    fn front_matter_ends_at_the_first_closing_line() {
        assert_eq!(
            front_matter("---\nname: a\n---\nbody\n---\n"),
            Some("name: a")
        );
        assert_eq!(front_matter("---\r\nname: a\r\n---"), Some("name: a"));
        assert_eq!(front_matter("---\n\n---"), Some(""));
        assert_eq!(front_matter("---\n---\n"), None);
        assert_eq!(front_matter("---\nname: a\n----\n"), None);
        assert_eq!(strip_front_matter("---\na\n---\r\nbody"), "body");
        assert_eq!(strip_front_matter("---\na\n----x"), "-x");
        assert_eq!(strip_front_matter("no header"), "no header");
    }

    #[test]
    fn owns_reads_own_keys_as_js_does() {
        let arr = wtf16::parse("[1,2]").unwrap();
        assert!(owns(&arr, "1") && owns(&arr, "length") && !owns(&arr, "2"));
        assert!(!owns(&JValue::from(""), "length"));
        assert!(owns(&JValue::from("ab"), "1") && !owns(&JValue::from("ab"), "01"));
        assert!(!owns(&JValue::Num(5.0), "0"));
    }
}
