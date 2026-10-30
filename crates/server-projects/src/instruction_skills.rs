//! core instruction-skills.cjs, the half the HTTP routes use (sbstndalton/noevia#2191 round 9):
//! `inspect`, `list`, `listForClient`, `manifests`, `resolve`, `setSelection` and the metadata
//! index limit. The skills of a project are the files that look like one (`SKILL.md`, or a
//! front-matter block naming `name:` / `description:`), judged by [`crate::skills`]; this module
//! reads each candidate's text (a source whose text is in its own file is read from it, as
//! source-text.cjs's accessor does, a missing file reading as empty), checks the metadata block and
//! reports the skill's review status against `project.instructionSkills`.
//!
//! Chat keeps core's instruction-skills.cjs for a turn (pinned skills, the skill index): both
//! read the same projects.json, and this port is the HTTP surface only.
//!
//! Strings are measured as JS measures them: lengths in UTF-16 units ([`wtf16::len`]), a lone
//! surrogate hashing and counting as U+FFFD (what `crypto.update(string)` and `Buffer.byteLength`
//! do).

use crate::js::{is_space, trim};
use crate::skills::{self, Unsupported};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::path::Path;

/// instruction-skills.cjs MAX_BODY.
pub const MAX_BODY: usize = 32768;
const SCRIPTS_REFUSED: &str = "This skill bundles executable scripts, and chat never runs Skill scripts. Remove them or use the skill without them.";
/// The toolbox every project falls back to (`projectToolboxIds(project, ['core'])`).
const DEFAULT_BOX: &str = "core";
/// project-docs-default.cjs BOX.
const DOCS_BOX: &str = "project-docs";

/// What the HTTP routes answer: a status and a message (`{ error }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
}

fn refuse(status: u16, message: impl Into<String>) -> Refusal {
    Refusal {
        status,
        message: message.into(),
    }
}

/// The text as `crypto.createHash('sha256').update(String(content))` and `Buffer.byteLength` see
/// it: a mapped lone surrogate is U+FFFD.
fn utf8(s: &str) -> String {
    s.chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// instruction-skills.cjs `hash(content)`.
pub fn hash(content: &str) -> String {
    Sha256::digest(utf8(content).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `Buffer.byteLength(text)`.
fn byte_length(s: &str) -> usize {
    utf8(s).len()
}

/// The project a skill is read from, with the folder its sources' texts are in.
#[derive(Clone, Copy)]
pub struct View<'a> {
    pub project: &'a JValue,
    /// `users/<id>`.
    pub dir: &'a Path,
    /// The toolbox ids the server knows (`allToolboxes().map(b => b.id)`).
    pub known: &'a [String],
}

impl View<'_> {
    fn project_id(&self) -> &str {
        self.project.get("id").as_str().unwrap_or("")
    }

    fn files(&self) -> &[JValue] {
        match self.project.get("files") {
            JValue::Arr(items) => items,
            _ => &[],
        }
    }

    /// `String(file.content || '')`.
    fn content(&self, file: &JValue) -> String {
        crate::sources::text(self.dir, self.project_id(), file).unwrap_or_default()
    }
}

/// One `inspect(file, project)` result, in the object's key order.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub file: String,
    pub hash: String,
    pub content: String,
    pub valid: bool,
    pub error: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub requires: Vec<String>,
    /// Set (with `allowed_tools`) only when the block is valid.
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub status: &'static str,
    pub missing_tools: Vec<String>,
}

fn strings(items: &[String]) -> JValue {
    JValue::Arr(items.iter().map(|s| JValue::Str(s.clone())).collect())
}

impl Skill {
    /// The row as `listForClient` sends it before `id`, `origin` and `scripts` are added.
    fn row(&self) -> Vec<(String, JValue)> {
        let mut row = vec![
            ("file".to_string(), JValue::Str(self.file.clone())),
            ("hash".into(), JValue::Str(self.hash.clone())),
            ("content".into(), JValue::Str(self.content.clone())),
            ("valid".into(), JValue::Bool(self.valid)),
            ("error".into(), JValue::Str(self.error.clone())),
            ("name".into(), JValue::Str(self.name.clone())),
            ("description".into(), JValue::Str(self.description.clone())),
            ("version".into(), JValue::Str(self.version.clone())),
            ("requires".into(), strings(&self.requires)),
        ];
        if let (Some(license), Some(compat), Some(tools)) =
            (&self.license, &self.compatibility, &self.allowed_tools)
        {
            row.push(("license".into(), JValue::Str(license.clone())));
            row.push(("compatibility".into(), JValue::Str(compat.clone())));
            row.push(("allowedTools".into(), strings(tools)));
        }
        row.push(("status".into(), JValue::Str(self.status.to_string())));
        row.push(("missingTools".into(), strings(&self.missing_tools)));
        row
    }
}

fn owns(obj: &JValue, key: &str) -> bool {
    matches!(obj, JValue::Obj(items) if items.iter().any(|(k, _)| k == key))
}

/// `projectToolboxIds(project, ['core'])`, by the toolboxes-permitted crate (the only
/// implementation since #1071). A project it cannot project carries no box.
pub fn project_toolbox_ids(project: &JValue) -> Vec<String> {
    use toolboxes_permitted::{project_ids, Id, Project, Work};
    let id = |s: &str| -> Id { Some(s.encode_utf16().collect()) };
    let toolboxes = match project.get("toolboxes") {
        JValue::Arr(items) => Some(
            items
                .iter()
                .map(|i| i.as_str().and_then(|s| id(&utf8(s))))
                .collect::<Vec<Id>>(),
        ),
        _ => None,
    };
    let projected = Project {
        auto: project.get("toolsMode").as_str() == Some("auto"),
        docs_defaulted: project.get("docsToolboxDefaulted") == &JValue::Bool(true),
        toolboxes,
    };
    let docs: Vec<u16> = DOCS_BOX.encode_utf16().collect();
    let mut work = Work::for_request(4096);
    let present = if project.truthy() {
        Some(&projected)
    } else {
        None
    };
    match project_ids(present, &[id(DEFAULT_BOX)], &docs, &mut work) {
        Ok(ids) => ids
            .into_iter()
            .flatten()
            .filter_map(|units| String::from_utf16(&units).ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// `/\.md$/i`.
fn is_markdown(name: &str) -> bool {
    name.len() >= 3
        && name
            .get(name.len() - 3..)
            .is_some_and(|t| t.eq_ignore_ascii_case(".md"))
}

fn line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `/^([a-z_][\w-]*):[ \t]*(.*)$/i` on one line: the key and the rest.
fn meta_line(line: &str) -> Option<(&str, &str)> {
    let mut chars = line.char_indices();
    let (_, first) = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let mut end = first.len_utf8();
    for (i, c) in chars {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    let (key, rest) = (line.get(..end)?, line.get(end..)?);
    let rest = rest.strip_prefix(':')?;
    let rest = rest.trim_start_matches([' ', '\t']);
    // `.` does not match a line terminator, and `$` is the end of the line.
    (!rest.chars().any(line_terminator)).then_some((key, rest))
}

/// `header.split(/\r?\n/)`.
fn header_lines(header: &str) -> Vec<&str> {
    header
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

/// The checks `inspect` makes inside its try; the message is `err.message`.
#[derive(Debug)]
struct Checked {
    name: String,
    description: String,
    version: String,
    license: String,
    compatibility: String,
    allowed_tools: Vec<String>,
    requires: Vec<String>,
}

fn check(file_name: &str, content: &str) -> Result<Checked, String> {
    if !is_markdown(file_name) {
        return Err("Instruction skills must be Markdown files.".into());
    }
    if byte_length(content) > MAX_BODY {
        return Err("Skill exceeds the 32 KiB limit.".into());
    }
    let header = skills::front_matter(content)
        .filter(|h| byte_length(h) <= 2048)
        .ok_or("Use a closed frontmatter block of at most 2 KiB.")?;
    let mut meta: Vec<(String, String)> = Vec::new();
    for line in header_lines(header) {
        let t = trim(line);
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let (key, rest) =
            meta_line(line).ok_or("Use one scalar name: value per frontmatter line.")?;
        let key = key.to_ascii_lowercase();
        // The Agent Skills spec's plain-text fields are accepted (license, compatibility,
        // allowed-tools) so published skills install; they are informational here and grant nothing.
        if ![
            "name",
            "description",
            "version",
            "requires",
            "license",
            "compatibility",
            "allowed-tools",
        ]
        .contains(&key.as_str())
        {
            return Err(format!("Unsupported metadata field: {key}"));
        }
        if meta.iter().any(|(k, _)| *k == key) {
            return Err(format!("Duplicate metadata field: {key}"));
        }
        let mut value = trim(rest).to_string();
        if value.starts_with(['[', ']', '{', '|', '>', '&', '*', '!']) {
            return Err(
                "Lists, mappings, multiline values and YAML directives are not supported.".into(),
            );
        }
        if value.starts_with(['"', '\'']) {
            let quote = value.chars().next().unwrap_or('"');
            if value.chars().count() < 2 || !value.ends_with(quote) {
                return Err("Unclosed metadata quote.".into());
            }
            value = value
                .get(1..value.len() - 1)
                .unwrap_or_default()
                .to_string();
        }
        meta.push((key, value));
    }
    let get = |k: &str| -> String {
        meta.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let (name, description) = (get("name"), get("description"));
    if name.is_empty() || description.is_empty() {
        return Err("A non-empty name and description are required.".into());
    }
    let (version, license, compat, tools_text) = (
        get("version"),
        get("license"),
        get("compatibility"),
        get("allowed-tools"),
    );
    if wtf16::len(&name) > 160
        || wtf16::len(&description) > 1024
        || wtf16::len(&version) > 80
        || wtf16::len(&license) > 500
        || wtf16::len(&compat) > 500
        || wtf16::len(&tools_text) > 1024
    {
        return Err("Metadata exceeds its length limit (name 160, description 1024, version 80, license 500, compatibility 500, allowed-tools 1024).".into());
    }
    let requires: Vec<String> = get("requires")
        .split(',')
        .map(|x| trim(x).to_string())
        .filter(|x| !x.is_empty())
        .collect();
    let toolbox_id = |x: &str| {
        let mut chars = x.chars();
        x.chars().count() <= 80
            && chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    };
    if requires.len() > 12 || requires.iter().any(|x| !toolbox_id(x)) {
        return Err("requires must be a comma-separated list of existing toolbox IDs.".into());
    }
    // allowed-tools is reported, never honoured: core offers tools from the project's selection and
    // asks before every write whatever a skill declares. Bounded so a manifest stays small.
    let allowed_tools: Vec<String> = tools_text
        .split(is_space)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect();
    if allowed_tools.len() > 32 {
        return Err("allowed-tools lists at most 32 entries.".into());
    }
    Ok(Checked {
        name,
        description,
        version,
        license,
        compatibility: compat,
        allowed_tools,
        requires,
    })
}

/// `inspect(file, project)`: `None` for a file that is neither a skill candidate nor already
/// tracked. `Err` where this port would be judging a file of a shape Node never writes.
pub fn inspect(view: View<'_>, file: &JValue) -> Result<Option<Skill>, Unsupported> {
    let Some(name) = file.get("name").as_str() else {
        return Err(Unsupported);
    };
    let tracked = owns(view.project.get("instructionSkills"), name);
    if !skills::looks_like_skill(file, name)? && !tracked {
        return Ok(None);
    }
    let content = view.content(file);
    let digest = hash(&content);
    let mut skill = Skill {
        file: name.to_string(),
        hash: digest.clone(),
        content: content.clone(),
        valid: false,
        error: String::new(),
        name: String::new(),
        description: String::new(),
        version: String::new(),
        requires: Vec::new(),
        license: None,
        compatibility: None,
        allowed_tools: None,
        status: "invalid",
        missing_tools: Vec::new(),
    };
    match check(name, &content) {
        Ok(c) => {
            skill.valid = true;
            skill.name = c.name;
            skill.description = c.description;
            skill.version = c.version;
            skill.license = Some(c.license);
            skill.compatibility = Some(c.compatibility);
            skill.allowed_tools = Some(c.allowed_tools);
            skill.requires = c.requires;
        }
        Err(message) => skill.error = message,
    }
    let selection = view.project.get("instructionSkills").get(name);
    let reviewed = selection.get("reviewedHash");
    skill.status = if !skill.valid {
        "invalid"
    } else if !reviewed.truthy() {
        "review"
    } else if reviewed.as_str() != Some(digest.as_str()) {
        "updated"
    } else if selection.get("enabled").truthy() {
        "enabled"
    } else {
        "disabled"
    };
    let selected = project_toolbox_ids(view.project);
    skill.missing_tools = skill
        .requires
        .iter()
        .filter(|id| !selected.contains(id))
        .cloned()
        .collect();
    Ok(Some(skill))
}

/// `list(project)`: the project's skills in file order.
pub fn list(view: View<'_>) -> Result<Vec<Skill>, Unsupported> {
    let mut out = Vec::new();
    for file in view.files() {
        if let Some(skill) = inspect(view, file)? {
            out.push(skill);
        }
    }
    Ok(out)
}

/// A bundled asset of a skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub file: String,
    pub version: String,
    pub executable: bool,
}

/// instruction-skills.cjs EXECUTABLE.
const EXECUTABLE: &[&str] = &[
    "py",
    "pyw",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "psm1",
    "bat",
    "cmd",
    "js",
    "mjs",
    "cjs",
    "ts",
    "mts",
    "cts",
    "rb",
    "pl",
    "php",
    "lua",
    "r",
    "jar",
    "exe",
    "com",
    "dll",
    "so",
    "dylib",
    "bin",
    "wasm",
    "appimage",
    "run",
    "command",
    "applescript",
    "scpt",
    "vbs",
];

fn executable_name(name: &str) -> bool {
    name.rfind('.').is_some_and(|dot| {
        let ext = name.get(dot + 1..).unwrap_or("");
        EXECUTABLE.iter().any(|e| ext.eq_ignore_ascii_case(e))
    })
}

/// `/(?:^|\/)scripts\//i.test(rest)`.
fn in_scripts_folder(rest: &str) -> bool {
    let lower = rest.to_ascii_lowercase();
    lower.starts_with("scripts/") || lower.contains("/scripts/")
}

/// `assets(project, skillFile)`: files beside `<dir>/SKILL.md`, e.g. `<dir>/scripts/x.py`. A
/// root-level or non-SKILL.md skill owns no directory.
pub fn assets(view: View<'_>, skill_file: &str) -> Vec<Asset> {
    // `/^(.+)\/SKILL\.md$/i`: `.+` stops at a line terminator, and `$` is the end of the string.
    let Some(dir) = skill_file
        .len()
        .checked_sub("/SKILL.md".len())
        .filter(|&at| at > 0)
        .and_then(|at| {
            let tail = skill_file.get(at..)?;
            tail.eq_ignore_ascii_case("/skill.md")
                .then(|| skill_file.get(..at))?
        })
        .filter(|d| !d.chars().any(line_terminator))
    else {
        return Vec::new();
    };
    let prefix = format!("{dir}/");
    view.files()
        .iter()
        .filter_map(|f| {
            let name = f.get("name").as_str()?;
            (name != skill_file && name.starts_with(&prefix)).then(|| {
                let content = view.content(f);
                Asset {
                    file: name.to_string(),
                    version: hash(&content),
                    executable: in_scripts_folder(name.get(prefix.len()..).unwrap_or(""))
                        || executable_name(name)
                        || content.starts_with("#!"),
                }
            })
        })
        .collect()
}

/// `skillId(project, file)`: scoped to a project and a file name, never to display names or
/// mutable content.
pub fn skill_id(project: &JValue, file: &str) -> String {
    let id = project.get("id");
    let id = if id.truthy() {
        js_json::to_js_string(id).unwrap_or_default()
    } else {
        String::new()
    };
    let digest = hash(&format!("{id}\0{file}"));
    format!("skill_{}", digest.get(..32).unwrap_or(&digest))
}

/// One `manifests(project, known)` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub skill: Skill,
    pub id: String,
    pub origin: JValue,
    pub unsupported: Vec<String>,
    pub unselected: Vec<String>,
    pub assets: Vec<Asset>,
    pub scripts: Vec<String>,
    pub resolvable: bool,
}

impl Manifest {
    pub fn json(&self) -> JValue {
        let s = &self.skill;
        JValue::obj([
            ("schemaVersion", JValue::Num(1.0)),
            ("id", JValue::Str(self.id.clone())),
            ("file", JValue::Str(s.file.clone())),
            ("name", JValue::Str(s.name.clone())),
            ("description", JValue::Str(s.description.clone())),
            ("versionLabel", JValue::Str(s.version.clone())),
            ("version", JValue::Str(s.hash.clone())),
            ("status", JValue::Str(s.status.to_string())),
            ("valid", JValue::Bool(s.valid)),
            ("error", JValue::Str(s.error.clone())),
            ("origin", self.origin.clone()),
            (
                "compatibility",
                JValue::Str(s.compatibility.clone().unwrap_or_default()),
            ),
            (
                "license",
                JValue::Str(s.license.clone().unwrap_or_default()),
            ),
            (
                "requirements",
                JValue::obj([
                    ("toolboxes", strings(&s.requires)),
                    (
                        "allowedTools",
                        strings(s.allowed_tools.as_deref().unwrap_or(&[])),
                    ),
                    ("unsupportedToolboxes", strings(&self.unsupported)),
                    ("unselectedToolboxes", strings(&self.unselected)),
                    ("scripts", strings(&self.scripts)),
                ]),
            ),
            (
                "assets",
                JValue::Arr(
                    self.assets
                        .iter()
                        .map(|a| {
                            JValue::obj([
                                ("file", JValue::Str(a.file.clone())),
                                ("version", JValue::Str(a.version.clone())),
                                ("executable", JValue::Bool(a.executable)),
                            ])
                        })
                        .collect(),
                ),
            ),
            // Metadata declares needs only. Core chooses offered tools and approves each write.
            ("resolvable", JValue::Bool(self.resolvable)),
        ])
    }
}

/// `manifests(project, known)`.
pub fn manifests(view: View<'_>) -> Result<Vec<Manifest>, Unsupported> {
    let selected = project_toolbox_ids(view.project);
    let mut out = Vec::new();
    for skill in list(view)? {
        let file = view
            .files()
            .iter()
            .find(|f| f.get("name").as_str() == Some(skill.file.as_str()));
        let published = file.map(|f| f.get("skillOrigin")).filter(|o| {
            o.get("kind").as_str() == Some("published")
                && o.get("digest").as_str() == Some(skill.hash.as_str())
        });
        let origin = match published {
            Some(o) => o.clone(),
            None => {
                let attached = file.is_some_and(|f| {
                    let source = f.get("source");
                    source.truthy() && source != view.project.get("projectFolder")
                });
                JValue::obj([(
                    "kind",
                    JValue::from(if attached {
                        "attached-folder"
                    } else {
                        "project-file"
                    }),
                )])
            }
        };
        let unsupported: Vec<String> = skill
            .requires
            .iter()
            .filter(|id| !view.known.contains(id))
            .cloned()
            .collect();
        let unselected: Vec<String> = skill
            .requires
            .iter()
            .filter(|id| view.known.contains(id) && !selected.contains(id))
            .cloned()
            .collect();
        let bundled = assets(view, &skill.file);
        let scripts: Vec<String> = bundled
            .iter()
            .filter(|a| a.executable)
            .map(|a| a.file.clone())
            .collect();
        let resolvable = skill.status == "enabled" && unsupported.is_empty() && scripts.is_empty();
        out.push(Manifest {
            id: skill_id(view.project, &skill.file),
            skill,
            origin,
            unsupported,
            unselected,
            assets: bundled,
            scripts,
            resolvable,
        });
    }
    Ok(out)
}

/// `listForClient(project, known)`: the Sources list, with the portable id, origin and bundled
/// scripts so the web client shows where each skill came from and why it cannot be invoked.
pub fn list_for_client(view: View<'_>) -> Result<JValue, Unsupported> {
    let by_file = manifests(view)?;
    let rows = list(view)?;
    Ok(JValue::Arr(
        rows.iter()
            .map(|row| {
                let m = by_file.iter().find(|m| m.skill.file == row.file);
                let mut fields = row.row();
                fields.push((
                    "id".into(),
                    m.map_or(JValue::Undefined, |m| JValue::Str(m.id.clone())),
                ));
                fields.push((
                    "origin".into(),
                    m.map_or(JValue::Undefined, |m| m.origin.clone()),
                ));
                fields.push((
                    "scripts".into(),
                    strings(m.map_or(&[][..], |m| &m.scripts[..])),
                ));
                JValue::Obj(fields)
            })
            .collect(),
    ))
}

/// `list(project).map(({ content, ...rest }) => rest)` (the install's answer).
pub fn list_without_content(view: View<'_>) -> Result<JValue, Unsupported> {
    Ok(JValue::Arr(
        list(view)?
            .iter()
            .map(|s| {
                JValue::Obj(
                    s.row()
                        .into_iter()
                        .filter(|(k, _)| k != "content")
                        .collect(),
                )
            })
            .collect(),
    ))
}

/// `resolve(project, id, version, known)`: the manifest and the text of one enabled skill.
pub fn resolve(
    view: View<'_>,
    id: &str,
    version: Option<&str>,
) -> Result<(Manifest, String), Refusal> {
    let all = manifests(view).map_err(|_| refuse(500, "Internal error"))?;
    let manifest = all
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| refuse(404, "No such instruction skill in this project."))?;
    let version = version.unwrap_or("");
    let sha = version.len() == 64
        && version
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !sha {
        return Err(refuse(400, "A SHA-256 version is required."));
    }
    if manifest.skill.hash != version || manifest.skill.status != "enabled" {
        return Err(refuse(
            409,
            "The skill changed, is disabled, or awaits review.",
        ));
    }
    if !manifest.scripts.is_empty() {
        return Err(refuse(422, SCRIPTS_REFUSED));
    }
    if !manifest.resolvable {
        return Err(refuse(
            422,
            "This skill declares unsupported toolbox requirements.",
        ));
    }
    let content = manifest.skill.content.clone();
    Ok((manifest, content))
}

/// skill-index.cjs `formatSkillIndex(skills)` says "omitted" for these: whether the metadata of
/// `skills` does not fit the context limit (32 entries, 6,144 characters, a file name of at most
/// 500).
pub fn index_overflows(skills: &[&Skill]) -> bool {
    let (mut lines, mut size, mut omitted) = (0usize, 0usize, 0usize);
    for skill in skills {
        if skill.file.is_empty() || wtf16::len(&skill.file) > 500 {
            omitted += 1;
            continue;
        }
        let shown = |s: &str, n: usize| wtf16::slice(s, n);
        let name = if skill.name.is_empty() {
            &skill.file
        } else {
            &skill.name
        };
        let line = wtf16::stringify(&JValue::obj([
            ("file", JValue::Str(skill.file.clone())),
            ("name", JValue::Str(shown(name, 160))),
            ("description", JValue::Str(shown(&skill.description, 500))),
            ("version", JValue::Str(shown(&skill.version, 80))),
        ]))
        .unwrap_or_default();
        if lines >= 32 || size + wtf16::len(&line) + 1 > 6144 {
            omitted += 1;
            continue;
        }
        lines += 1;
        size += wtf16::len(&line) + 1;
    }
    omitted > 0
}

/// `setSelection(project, body)`: enables or disables one skill (the project changes in place).
/// `now` is `Date.now()` for `reviewedAt`.
pub fn set_selection(
    project: &mut JValue,
    dir: &Path,
    known: &[String],
    body: &JValue,
    now: i64,
) -> Result<(), Refusal> {
    let (Some(file), Some(enabled)) = (body.get("file").as_str(), body.get("enabled").as_bool())
    else {
        return Err(refuse(400, "File and enabled boolean required."));
    };
    let view = View {
        project,
        dir,
        known,
    };
    let rows = list(view).map_err(|_| refuse(500, "Internal error"))?;
    let skill = rows
        .iter()
        .find(|s| s.file == file)
        .ok_or_else(|| refuse(404, "No such instruction skill in this project."))?;
    if enabled && !skill.valid {
        return Err(refuse(400, skill.error.clone()));
    }
    if enabled && body.get("hash").as_str() != Some(skill.hash.as_str()) {
        return Err(refuse(
            409,
            "The file changed. Inspect the current version before enabling it.",
        ));
    }
    // #567: chat never runs Skill scripts, so a skill that bundles any can never be used there.
    // Refuse to enable it (rather than show "Enabled" for something no chat can load); disabling
    // stays allowed.
    if enabled {
        let scripts: Vec<String> = assets(view, file)
            .into_iter()
            .filter(|a| a.executable)
            .map(|a| a.file)
            .collect();
        if !scripts.is_empty() {
            return Err(refuse(
                422,
                format!("{SCRIPTS_REFUSED} Bundled scripts: {}.", scripts.join(", ")),
            ));
        }
    }
    let previous = project
        .get("instructionSkills")
        .get(file)
        .get("reviewedHash");
    let reviewed = if enabled || !previous.truthy() {
        JValue::Str(skill.hash.clone())
    } else {
        previous.clone()
    };
    let entry = JValue::obj([
        ("enabled", JValue::Bool(enabled)),
        ("reviewedHash", reviewed),
        ("reviewedAt", JValue::Num(now as f64)),
    ]);
    let mut selection: Vec<(String, JValue)> = match project.get("instructionSkills") {
        JValue::Obj(items) => items.clone(),
        _ => Vec::new(),
    };
    wtf16::set(&mut selection, file, entry);
    let mut next = project.clone();
    crate::store::set(
        &mut next,
        "instructionSkills",
        JValue::Obj(selection.clone()),
    );
    let next_view = View {
        project: &next,
        dir,
        known,
    };
    let after = list(next_view).map_err(|_| refuse(500, "Internal error"))?;
    let on: Vec<&Skill> = after.iter().filter(|s| s.status == "enabled").collect();
    if index_overflows(&on) {
        return Err(refuse(
            400,
            "Enabled skill metadata exceeds the context limit. Disable another skill first.",
        ));
    }
    crate::store::set(project, "instructionSkills", JValue::Obj(selection));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn p(text: &str) -> JValue {
        wtf16::parse(text).unwrap()
    }

    const GOOD: &str =
        "---\nname: Fixture\ndescription: A synthetic skill\nversion: 1.0\n---\nBody";

    fn project_with(files: &str, selection: &str) -> JValue {
        p(&format!(
            r#"{{"id":"proj-1","toolboxes":["core"],"files":{files},"instructionSkills":{selection}}}"#
        ))
    }

    fn file(name: &str, content: &str) -> String {
        let mut quoted = String::new();
        js_json::quote(content, &mut quoted);
        format!(r#"{{"name":"{name}","content":{quoted}}}"#)
    }

    fn view<'a>(project: &'a JValue, dir: &'a Path, known: &'a [String]) -> View<'a> {
        View {
            project,
            dir,
            known,
        }
    }

    #[test]
    fn a_valid_skill_is_in_review_until_it_is_enabled() {
        let project = project_with(&format!("[{}]", file("fixture/SKILL.md", GOOD)), "{}");
        let known = vec!["core".to_string()];
        let rows = list(view(&project, Path::new("/nonexistent"), &known)).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].valid, "{}", rows[0].error);
        assert_eq!(rows[0].status, "review");
        assert_eq!(rows[0].hash, hash(GOOD));
        assert_eq!(rows[0].name, "Fixture");
        assert_eq!(rows[0].version, "1.0");
    }

    #[test]
    fn the_metadata_rules_are_cores() {
        let bad = |content: &str| check("a/SKILL.md", content).unwrap_err();
        assert_eq!(
            bad("no front matter"),
            "Use a closed frontmatter block of at most 2 KiB."
        );
        assert_eq!(
            bad("---\nname: a\n---"),
            "A non-empty name and description are required."
        );
        assert_eq!(
            bad("---\nname: a\nwhat: b\n---"),
            "Unsupported metadata field: what"
        );
        assert_eq!(
            bad("---\nname: a\nName: b\n---"),
            "Duplicate metadata field: name"
        );
        assert_eq!(
            bad("---\nname: [a]\ndescription: b\n---"),
            "Lists, mappings, multiline values and YAML directives are not supported."
        );
        assert_eq!(
            bad("---\nname: \"a\ndescription: b\n---"),
            "Unclosed metadata quote."
        );
        assert_eq!(
            bad("---\nname: a\ndescription: b\nrequires: Core\n---"),
            "requires must be a comma-separated list of existing toolbox IDs."
        );
        assert_eq!(
            bad("---\nname: a\n  indented: b\n---"),
            "Use one scalar name: value per frontmatter line."
        );
        assert_eq!(bad(&format!("---\nname: {}\ndescription: b\n---", "n".repeat(161))), "Metadata exceeds its length limit (name 160, description 1024, version 80, license 500, compatibility 500, allowed-tools 1024).");
        assert_eq!(
            check("notes.txt", GOOD).unwrap_err(),
            "Instruction skills must be Markdown files."
        );
        assert_eq!(
            check("a/SKILL.md", &format!("{GOOD}{}", "x".repeat(MAX_BODY))).unwrap_err(),
            "Skill exceeds the 32 KiB limit."
        );
        let ok = check(
            "a/SKILL.md",
            "---\n# a comment\nname: 'Quoted'\ndescription: \"d\"\nrequires: core, project-docs\nallowed-tools: a  b\tc\n---\n",
        )
        .unwrap();
        assert_eq!(ok.name, "Quoted");
        assert_eq!(ok.description, "d");
        assert_eq!(ok.requires, vec!["core", "project-docs"]);
        assert_eq!(ok.allowed_tools, vec!["a", "b", "c"]);
        // CRLF files read the same.
        assert!(check("a/SKILL.md", "---\r\nname: a\r\ndescription: b\r\n---\r\n").is_ok());
    }

    #[test]
    fn selection_enables_only_the_current_hash_and_a_change_needs_review_again() {
        let mut project = project_with(&format!("[{}]", file("fixture/SKILL.md", GOOD)), "{}");
        let known = vec!["core".to_string()];
        let dir = Path::new("/nonexistent");
        let body = |enabled: bool, hash: &str| {
            p(&format!(
                r#"{{"file":"fixture/SKILL.md","enabled":{enabled},"hash":"{hash}"}}"#
            ))
        };
        assert_eq!(
            set_selection(&mut project, dir, &known, &body(true, "stale"), 5)
                .unwrap_err()
                .status,
            409
        );
        set_selection(&mut project, dir, &known, &body(true, &hash(GOOD)), 5).unwrap();
        let rows = list(view(&project, dir, &known)).unwrap();
        assert_eq!(rows[0].status, "enabled");
        assert_eq!(
            project
                .get("instructionSkills")
                .get("fixture/SKILL.md")
                .get("reviewedAt"),
            &JValue::Num(5.0)
        );
        set_selection(&mut project, dir, &known, &body(false, ""), 6).unwrap();
        assert_eq!(
            list(view(&project, dir, &known)).unwrap()[0].status,
            "disabled"
        );
        assert_eq!(
            set_selection(
                &mut project,
                dir,
                &known,
                &p(r#"{"file":"x.md","enabled":true}"#),
                7
            )
            .unwrap_err()
            .status,
            404
        );
        assert_eq!(
            set_selection(&mut project, dir, &known, &p("{}"), 7)
                .unwrap_err()
                .message,
            "File and enabled boolean required."
        );
        // The content changed after the review: updated, not enabled.
        let mut edited = project_with(
            &format!("[{}]", file("fixture/SKILL.md", &format!("{GOOD} changed"))),
            r#"{"fixture/SKILL.md":{"enabled":true,"reviewedHash":"0000"}}"#,
        );
        assert_eq!(
            list(view(&edited, dir, &known)).unwrap()[0].status,
            "updated"
        );
        edited = project_with("[]", r#"{"gone.md":{"enabled":true,"reviewedHash":"x"}}"#);
        assert!(list(view(&edited, dir, &known)).unwrap().is_empty());
    }

    #[test]
    fn a_skill_that_bundles_a_script_cannot_be_enabled_or_resolved() {
        let files = format!(
            "[{},{}]",
            file("tool/SKILL.md", GOOD),
            file("tool/scripts/run.txt", "echo")
        );
        let mut project = project_with(&files, "{}");
        let known = vec!["core".to_string()];
        let dir = Path::new("/nonexistent");
        let body = p(&format!(
            r#"{{"file":"tool/SKILL.md","enabled":true,"hash":"{}"}}"#,
            hash(GOOD)
        ));
        let refused = set_selection(&mut project, dir, &known, &body, 1).unwrap_err();
        assert_eq!(refused.status, 422);
        assert!(
            refused
                .message
                .ends_with("Bundled scripts: tool/scripts/run.txt."),
            "{}",
            refused.message
        );
        let shown = manifests(view(&project, dir, &known)).unwrap();
        assert_eq!(shown[0].scripts, vec!["tool/scripts/run.txt"]);
        assert!(!shown[0].resolvable);
        assert!(executable_name("a/B.PY") && !executable_name("a/b.pyx") && !executable_name("a"));
        assert!(
            in_scripts_folder("Scripts/x")
                && in_scripts_folder("a/scripts/x")
                && !in_scripts_folder("ascripts/x")
        );
    }

    #[test]
    fn resolve_follows_cores_order_of_refusals() {
        let known = vec!["core".to_string()];
        let dir = Path::new("/nonexistent");
        let project = project_with(
            &format!("[{}]", file("fixture/SKILL.md", GOOD)),
            &format!(
                r#"{{"fixture/SKILL.md":{{"enabled":true,"reviewedHash":"{}"}}}}"#,
                hash(GOOD)
            ),
        );
        let id = skill_id(&project, "fixture/SKILL.md");
        let v = view(&project, dir, &known);
        assert_eq!(resolve(v, "skill_nope", None).unwrap_err().status, 404);
        assert_eq!(resolve(v, &id, None).unwrap_err().status, 400);
        assert_eq!(
            resolve(v, &id, Some(&"0".repeat(64))).unwrap_err().status,
            409
        );
        let (manifest, content) = resolve(v, &id, Some(&hash(GOOD))).unwrap();
        assert_eq!(content, GOOD);
        assert!(manifest.resolvable);
        // An unknown toolbox makes it unresolvable.
        let needs = "---\nname: n\ndescription: d\nrequires: nothing-here\n---\n";
        let project = project_with(
            &format!("[{}]", file("n/SKILL.md", needs)),
            &format!(
                r#"{{"n/SKILL.md":{{"enabled":true,"reviewedHash":"{}"}}}}"#,
                hash(needs)
            ),
        );
        let id = skill_id(&project, "n/SKILL.md");
        assert_eq!(
            resolve(view(&project, dir, &known), &id, Some(&hash(needs)))
                .unwrap_err()
                .status,
            422
        );
    }

    #[test]
    fn the_skill_index_limit_is_the_formatters() {
        let many: Vec<Skill> = (0..33)
            .map(|i| Skill {
                file: format!("s{i}/SKILL.md"),
                hash: String::new(),
                content: String::new(),
                valid: true,
                error: String::new(),
                name: format!("n{i}"),
                description: "d".into(),
                version: String::new(),
                requires: vec![],
                license: None,
                compatibility: None,
                allowed_tools: None,
                status: "enabled",
                missing_tools: vec![],
            })
            .collect();
        let refs: Vec<&Skill> = many.iter().collect();
        assert!(!index_overflows(&refs[..32]));
        assert!(index_overflows(&refs));
    }

    #[test]
    fn a_skill_that_bundles_a_script_can_still_be_disabled_and_one_without_enables_normally() {
        let files = format!(
            "[{},{}]",
            file("tool/SKILL.md", GOOD),
            file("tool/scripts/run.sh", "echo")
        );
        let hash_of_good = hash(GOOD);
        let mut project = project_with(
            &files,
            &format!(r#"{{"tool/SKILL.md":{{"enabled":true,"reviewedHash":"{hash_of_good}"}}}}"#),
        );
        let known = vec!["core".to_string()];
        let dir = Path::new("/nonexistent");
        // Enabled before the script appeared: switching it off is allowed and keeps the reviewed hash.
        let off = p(r#"{"file":"tool/SKILL.md","enabled":false}"#);
        set_selection(&mut project, dir, &known, &off, 9).unwrap();
        assert_eq!(
            list(view(&project, dir, &known)).unwrap()[0].status,
            "disabled"
        );
        assert_eq!(
            project
                .get("instructionSkills")
                .get("tool/SKILL.md")
                .get("reviewedHash")
                .as_str(),
            Some(hash_of_good.as_str())
        );
        // The same call on a project without a script enables as usual.
        let mut plain = project_with(
            &format!(
                "[{},{}]",
                file("tool/SKILL.md", GOOD),
                file("tool/reference.md", "text")
            ),
            "{}",
        );
        let on = p(&format!(
            r#"{{"file":"tool/SKILL.md","enabled":true,"hash":"{hash_of_good}"}}"#
        ));
        set_selection(&mut plain, dir, &known, &on, 9).unwrap();
        assert_eq!(
            list(view(&plain, dir, &known)).unwrap()[0].status,
            "enabled"
        );
    }

    /// instruction-skills.test.cjs "catalogue overflow rejects enablement rather than silently omitting
    /// selected skills": twenty skills with the longest allowed name and description.
    #[test]
    fn enabling_past_the_index_limit_is_refused_and_leaves_the_selection_as_it_was() {
        let meta = format!(
            "name: {}
description: {}",
            "n".repeat(160),
            "d".repeat(500)
        );
        let content = format!(
            "---
{meta}
---
Body"
        );
        let items: Vec<String> = (0..20)
            .map(|i| file(&format!("skill-{i}.md"), &content))
            .collect();
        let mut project = project_with(&format!("[{}]", items.join(",")), "{}");
        let known = vec!["core".to_string()];
        let dir = Path::new("/nonexistent");
        let hash = hash(&content);
        let mut refused_at = None;
        for i in 0..20 {
            let body = p(&format!(
                r#"{{"file":"skill-{i}.md","enabled":true,"hash":"{hash}"}}"#
            ));
            let before = project.clone();
            if let Err(r) = set_selection(&mut project, dir, &known, &body, 1) {
                assert_eq!(
                    (r.status, r.message.as_str()),
                    (400, "Enabled skill metadata exceeds the context limit. Disable another skill first.")
                );
                assert_eq!(project, before, "the refused enable changed nothing");
                refused_at = Some(i);
                break;
            }
        }
        assert!(refused_at.is_some_and(|i| i > 0), "{refused_at:?}");
        assert_eq!(
            set_selection(
                &mut project,
                dir,
                &known,
                &p(r#"{"file":"../other.md","enabled":true,"hash":""}"#),
                1
            )
            .unwrap_err()
            .status,
            404
        );
    }

    #[test]
    fn the_client_list_carries_the_id_the_origin_and_the_bundled_scripts() {
        let digest = hash(GOOD);
        let origin = format!(
            r#"{{"kind":"published","publisher":"Synthetic","repository":"https://example.invalid/skills","sourceRef":"main","digest":"{digest}"}}"#
        );
        let mut quoted = String::new();
        js_json::quote(GOOD, &mut quoted);
        let files = format!(
            r#"[{{"name":"h/SKILL.md","content":{quoted},"skillOrigin":{origin}}},{}]"#,
            file("h/scripts/a.sh", "echo")
        );
        let project = project_with(&files, "{}");
        let known = vec!["core".to_string()];
        let dir = Path::new("/nonexistent");
        let JValue::Arr(rows) = list_for_client(view(&project, dir, &known)).unwrap() else {
            panic!()
        };
        assert_eq!(
            rows[0].get("id").as_str(),
            Some(skill_id(&project, "h/SKILL.md").as_str())
        );
        assert_eq!(
            rows[0].get("origin").get("kind").as_str(),
            Some("published")
        );
        assert_eq!(
            rows[0].get("origin").get("publisher").as_str(),
            Some("Synthetic")
        );
        assert_eq!(
            wtf16::stringify(rows[0].get("scripts")).unwrap(),
            r#"["h/scripts/a.sh"]"#
        );
        assert_eq!(rows[0].get("content").as_str(), Some(GOOD));
        // Edited bytes lose the published origin.
        let edited = project_with(&files.replace("Body", "Edited"), "{}");
        let JValue::Arr(rows) = list_for_client(view(&edited, dir, &known)).unwrap() else {
            panic!()
        };
        assert_eq!(
            rows[0].get("origin").get("kind").as_str(),
            Some("project-file")
        );
    }

    #[test]
    fn the_hash_and_length_treat_a_lone_surrogate_as_replacement() {
        let lone = wtf16::lone_char(0xd800).unwrap().to_string();
        assert_eq!(hash(&lone), hash("\u{fffd}"));
        assert_eq!(byte_length(&lone), 3);
    }
}
