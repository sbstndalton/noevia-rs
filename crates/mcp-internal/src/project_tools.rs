//! The project tools of noevia-core's in-app MCP server (`server/mcp-internal-tools.cjs`
//! `createInternalTools`, the "Project documents" and "Project writes" groups) in Rust: what
//! `project_list_files`, `project_read_file`, `project_search`, `project_create_file`,
//! `project_append_file` and `project_replace_text` do, given the server's ports.
//!
//! The Diary group of the same module is not here: it stays core's (no Diary work while the storage
//! move holds). [`definitions`] is therefore only the project half of core's catalogue; a front that
//! serves the in-app server needs the Diary half from core before it may answer `tools/list`.
//!
//! Identity comes only from the verified token's [`Ctx`], never from the arguments (core's rule): no
//! schema here declares a user, project or tenant field.
//!
//! The ports are the host's: the project as stored, the read path of the built-in `read_project_file`
//! tool, the retrieval search, the text write, and project-edit-target's `planEdit`/`targetDigest`/
//! `uploadPathFor` (the `project-edit-target` crate in the host). Tests use fakes, as core's do.
//!
//! Named differences from the Node module: none in behaviour. Errors are the message text core's
//! handler throws (the caller hands them to [`crate::Invoke::finish`]); a `String(x)` that throws in
//! JS (an object with an own `toString`) answers V8's TypeError text.
use crate::{Ctx, Definition, Handler, Invoke, Reply, Step, RPC_PATH};
use js_json::JValue;

/// `MAX_TEXT_BYTES`: one write's UTF-8 size limit.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// `ports.cap || 8000`.
pub const DEFAULT_CAP: usize = 8000;
/// V8's message for `String(x)` when `x` cannot be turned into a primitive.
const TO_PRIMITIVE: &str = "Cannot convert object to primitive value";

/// One stored project file, as the tools read it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectFile {
    pub name: String,
    /// `f.content` when it is a string.
    pub content: Option<String>,
    /// `f.source`: the attached folder it came from, when truthy.
    pub source: Option<String>,
    /// `!!f.document`.
    pub document: bool,
    /// `source-text.cjs`: `textLength(f)` when `isExternal(f) || typeof f.content === 'string'`, else
    /// `None` ("no readable text"). The host computes it without reading an external text.
    pub size: Option<usize>,
    /// `f.attachment.id`, for a connected upload.
    pub attachment_id: Option<String>,
}

/// The project a call acts for (`ports.getProject(ctx.projectId)`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Project {
    pub id: String,
    pub files: Vec<ProjectFile>,
}

/// `planEdit(project, name, { storageAccount })`'s answer.
#[derive(Debug, Clone, PartialEq)]
pub struct EditPlan {
    /// `plan.file`.
    pub file: ProjectFile,
    /// `plan.writeName`.
    pub write_name: String,
    /// `plan.target`: the stored path the approval card showed.
    pub target: String,
    /// `plan.account`.
    pub account: Option<String>,
    /// `plan.adopt` (#687).
    pub adopt: bool,
}

/// The fourth argument of `writeTextFile` for an edit in place (`writeInPlace`).
#[derive(Debug, Clone, PartialEq)]
pub struct Expect {
    pub expect_name: String,
    pub expect_content: String,
    pub expect_attachment: Option<String>,
    /// `adoptTo`/`adoptAccount`, only when the plan adopts.
    pub adopt: Option<(String, Option<String>)>,
}

/// One retrieval hit.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub file: String,
    pub body: String,
}

/// The in-app server's ports for the project tools. Every `Err` is the message the port threw.
pub trait Ports {
    /// `ports.cap || 8000`.
    fn cap(&self) -> usize {
        DEFAULT_CAP
    }
    /// `ports.getProject(id)`: `None` for no project (or none of this id).
    fn get_project(&self, id: &JValue) -> Option<Project>;
    /// `ports.readProjectFile(project, args)`: the built-in tool's read, paging and cap included.
    fn read_project_file(&self, project: &Project, args: &JValue) -> Result<Option<String>, String>;
    /// `ports.ragAvailable()`.
    fn rag_available(&self) -> bool;
    /// `ports.search(project.id, query, ctx.userId)`.
    fn search(&self, project_id: &str, query: &str, user_id: &JValue) -> Result<Vec<Hit>, String>;
    /// `ports.writeTextFile(project, name, text[, expect])`.
    fn write_text_file(
        &self,
        project: &Project,
        name: &str,
        text: &str,
        expect: Option<&Expect>,
    ) -> Result<(), String>;
    /// project-edit-target `uploadPathFor(project, name)`.
    fn upload_path_for(&self, project: &Project, name: &str) -> String;
    /// project-edit-target `planEdit(project, name, { storageAccount: ports.storageAccount() })`.
    fn plan_edit(&self, project: &Project, name: &JValue) -> Result<EditPlan, String>;
    /// project-edit-target `targetDigest(target, account)`.
    fn target_digest(&self, target: &str, account: Option<&str>) -> String;
}

/// The project tools' names, in core's catalogue order.
pub const NAMES: [&str; 6] = [
    "project_list_files",
    "project_read_file",
    "project_search",
    "project_create_file",
    "project_append_file",
    "project_replace_text",
];

fn string_prop(description: Option<&str>) -> JValue {
    let mut pairs = vec![("type", JValue::from("string"))];
    if let Some(text) = description {
        pairs.push(("description", JValue::from(text)));
    }
    JValue::obj(pairs)
}

fn integer_prop(description: Option<&str>) -> JValue {
    let mut pairs = vec![("type", JValue::from("integer"))];
    if let Some(text) = description {
        pairs.push(("description", JValue::from(text)));
    }
    JValue::obj(pairs)
}

fn schema(properties: Vec<(&str, JValue)>, required: &[&str]) -> JValue {
    JValue::obj([
        ("type", JValue::from("object")),
        ("properties", JValue::obj(properties)),
        (
            "required",
            JValue::Arr(required.iter().map(|r| JValue::from(*r)).collect()),
        ),
    ])
}

fn definition(name: &str, description: &str, write: bool, schema: JValue) -> Definition {
    Definition {
        name: name.to_owned(),
        description: Some(description.to_owned()),
        schema: Some(schema),
        write,
    }
}

/// The project tools' entries of `createInternalTools(ports)`, descriptions and schemas byte for byte.
pub fn definitions() -> Vec<Definition> {
    let name_text = || {
        vec![
            ("name", string_prop(None)),
            ("text", string_prop(None)),
        ]
    };
    vec![
        definition(
            "project_list_files",
            "List the files attached to this chat's project.",
            false,
            schema(vec![], &[]),
        ),
        definition(
            "project_read_file",
            "Read one project file by its listed name (or file name, if unique). Use offset to continue a long file.",
            false,
            schema(
                vec![
                    ("name", string_prop(None)),
                    ("offset", integer_prop(Some("characters to skip"))),
                    ("startPage", integer_prop(Some("first page, for documents"))),
                    ("endPage", integer_prop(None)),
                ],
                &["name"],
            ),
        ),
        definition(
            "project_search",
            "Search this project's files for a phrase and return matching excerpts.",
            false,
            schema(vec![("query", string_prop(None))], &["query"]),
        ),
        definition(
            "project_create_file",
            "Create a new text file in this project.",
            true,
            schema(name_text(), &["name", "text"]),
        ),
        definition(
            "project_append_file",
            "Add text to the end of a file in this project, uploads included. Use this, not Google Drive, for project files.",
            true,
            schema(name_text(), &["name", "text"]),
        ),
        definition(
            "project_replace_text",
            "Replace exact text in a project file or upload (not Google Drive). expectedCount: matches to require.",
            true,
            schema(
                vec![
                    ("name", string_prop(None)),
                    ("find", string_prop(None)),
                    ("replace", string_prop(None)),
                    (
                        "expectedCount",
                        integer_prop(Some("how many matches you expect; defaults to 1")),
                    ),
                ],
                &["name", "find", "replace"],
            ),
        ),
    ]
}

/// Runs project tool `name` for a verified call: `None` when `name` is not one of [`NAMES`] (the caller
/// answers it as core does for any other name), else the handler's text or the message it threw.
pub fn call(
    ports: &dyn Ports,
    name: &str,
    args: &JValue,
    ctx: &Ctx,
) -> Option<Result<Option<String>, String>> {
    let tools = Tools { ports };
    Some(match name {
        "project_list_files" => tools.list_files(ctx).map(Some),
        "project_read_file" => tools.read_file(args, ctx),
        "project_search" => tools.search(args, ctx).map(Some),
        "project_create_file" => tools.create_file(args, ctx).map(Some),
        "project_append_file" => tools.append_file(args, ctx).map(Some),
        "project_replace_text" => tools.replace_text(args, ctx).map(Some),
        _ => return None,
    })
}

/// One request to the in-app listener at [`RPC_PATH`] (core `startInternalServer`'s handler and
/// `handle`): `handler`'s rules and catalogue, the project tools run with `ports`, and every other tool
/// name the handler's catalogue admits handed to `other` (core's remaining definitions, the Diary group,
/// which stay core's). Nothing binds a port: the caller owns the socket and the clock.
#[allow(clippy::too_many_arguments)]
pub fn serve(
    handler: &Handler,
    ports: &dyn Ports,
    other: &dyn Fn(&Invoke) -> Result<Option<String>, String>,
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    authorization: Option<&str>,
    now_ms: f64,
) -> Reply {
    match handler.listen(method, url, RPC_PATH, body, authorization, now_ms) {
        Step::Reply(r) => r,
        Step::Invoke(inv) => {
            let answered = call(ports, &inv.name, &inv.args, &inv.ctx).unwrap_or_else(|| other(&inv));
            inv.finish(answered)
        }
    }
}

struct Tools<'a> {
    ports: &'a dyn Ports,
}

impl Tools<'_> {
    /// `clip(text)`: past `cap` UTF-16 units, the first `cap` and a truncation mark.
    fn clip(&self, text: &str) -> String {
        let cap = self.ports.cap();
        if js_json::js_len(text) > cap {
            format!("{}\n…[truncated]", js_json::js_slice(text, cap))
        } else {
            text.to_owned()
        }
    }

    /// `requireProject(ports, ctx)`.
    fn project(&self, ctx: &Ctx) -> Result<Project, String> {
        self.ports
            .get_project(&ctx.project_id)
            .ok_or_else(|| "this chat is not in a project, so there are no project files".to_owned())
    }

    fn list_files(&self, ctx: &Ctx) -> Result<String, String> {
        let project = self.project(ctx)?;
        if project.files.is_empty() {
            return Ok("This project has no files attached.".to_owned());
        }
        let lines: Vec<String> = project
            .files
            .iter()
            .map(|f| {
                let kind = match f.source.as_deref().filter(|s| !s.is_empty()) {
                    Some(folder) => format!("from folder {folder}"),
                    None if f.document => "document".to_owned(),
                    None => "text".to_owned(),
                };
                let size = match f.size {
                    Some(n) => format!("{n} chars"),
                    None => "no readable text".to_owned(),
                };
                format!("{} — {kind}, {size}", f.name)
            })
            .collect();
        Ok(self.clip(&format!(
            "Pass a name exactly as shown (or just its file name, when unique) to project_read_file.\n{}",
            lines.join("\n")
        )))
    }

    fn read_file(&self, args: &JValue, ctx: &Ctx) -> Result<Option<String>, String> {
        let project = self.project(ctx)?;
        self.ports.read_project_file(&project, args)
    }

    fn search(&self, args: &JValue, ctx: &Ctx) -> Result<String, String> {
        let project = self.project(ctx)?;
        let query = js_trim(&string_or(args.get("query"), "", false)?).to_owned();
        if query.is_empty() {
            return Err("a query is required".to_owned());
        }
        if !self.ports.rag_available() {
            return Ok("Retrieval is not available on this deployment, so the project could not be searched. Read a file by name instead.".to_owned());
        }
        let hits = self.ports.search(&project.id, &query, &ctx.user_id)?;
        let usable: Vec<String> = hits
            .iter()
            .filter(|h| project.files.iter().any(|f| f.name == h.file))
            .map(|h| format!("[from {}] {}", h.file, h.body))
            .collect();
        if usable.is_empty() {
            return Ok(format!("Nothing in this project matched \"{query}\"."));
        }
        Ok(self.clip(&usable.join("\n\n")))
    }

    fn create_file(&self, args: &JValue, ctx: &Ctx) -> Result<String, String> {
        let project = self.project(ctx)?;
        let name = clean_name(args.get("name"))?;
        let text = string_or(args.get("text"), "", true)?;
        if text.len() > MAX_TEXT_BYTES {
            return Err("that text is too large to write in one call".to_owned());
        }
        let upload = self.ports.upload_path_for(&project, &name);
        if project
            .files
            .iter()
            .any(|f| f.name == name || f.name == upload)
        {
            return Err(format!(
                "\"{name}\" already exists. Use project_append_file or project_replace_text."
            ));
        }
        self.ports.write_text_file(&project, &name, &text, None)?;
        Ok(format!(
            "Created \"{name}\" ({} chars).",
            js_json::js_len(&text)
        ))
    }

    /// `pinnedEdit(ports, project, args, ctx)`.
    fn pinned_edit(&self, project: &Project, args: &JValue, ctx: &Ctx) -> Result<EditPlan, String> {
        let name = args.get("name");
        let plan = self.ports.plan_edit(project, name)?;
        let pinned = ctx.edit_target.as_deref().unwrap_or("");
        if pinned.is_empty() {
            return Err(
                "this edit was not tied to a file when it was approved, so nothing was changed"
                    .to_owned(),
            );
        }
        if pinned != self.ports.target_digest(&plan.target, plan.account.as_deref()) {
            let shown = js_json::to_js_string(name).map_err(|_| TO_PRIMITIVE.to_owned())?;
            let mut quoted = String::new();
            js_json::quote(&shown, &mut quoted);
            return Err(format!(
                "{quoted} now refers to \"{}\", which is not the file that was approved. Nothing was changed; ask again.",
                plan.target
            ));
        }
        Ok(plan)
    }

    /// `writeInPlace(ports, project, plan, text)`.
    fn write_in_place(&self, project: &Project, plan: &EditPlan, text: &str) -> Result<(), String> {
        let expect = Expect {
            expect_name: plan.file.name.clone(),
            expect_content: plan.file.content.clone().unwrap_or_default(),
            expect_attachment: plan.file.attachment_id.clone(),
            adopt: plan
                .adopt
                .then(|| (plan.target.clone(), plan.account.clone())),
        };
        self.ports
            .write_text_file(project, &plan.write_name, text, Some(&expect))
    }

    fn append_file(&self, args: &JValue, ctx: &Ctx) -> Result<String, String> {
        let project = self.project(ctx)?;
        let plan = self.pinned_edit(&project, args, ctx)?;
        let content = plan.file.content.as_deref().unwrap_or("");
        let addition = string_or(args.get("text"), "", true)?;
        if addition.is_empty() {
            return Err("there is nothing to append".to_owned());
        }
        let joiner = if content.ends_with('\n') || content.is_empty() {
            ""
        } else {
            "\n"
        };
        let next = format!("{content}{joiner}{addition}");
        if next.len() > MAX_TEXT_BYTES {
            return Err("that would make the file too large to write in one call".to_owned());
        }
        self.write_in_place(&project, &plan, &next)?;
        Ok(format!(
            "Appended {} chars to \"{}\".",
            js_json::js_len(&addition),
            plan.target
        ))
    }

    fn replace_text(&self, args: &JValue, ctx: &Ctx) -> Result<String, String> {
        let project = self.project(ctx)?;
        let plan = self.pinned_edit(&project, args, ctx)?;
        let file = &plan.file;
        let find = string_or(args.get("find"), "", true)?;
        if find.is_empty() {
            return Err("find must not be empty".to_owned());
        }
        let replace = string_or(args.get("replace"), "", true)?;
        let content = file.content.as_deref().unwrap_or("");
        let found = content.matches(find.as_str()).count();
        let expected = match args.get("expectedCount") {
            JValue::Undefined => 1.0,
            other => to_number(other)?,
        };
        if !is_integer(expected) || expected < 1.0 {
            return Err("expectedCount must be a positive whole number".to_owned());
        }
        // `found !== expected`: `found` is a whole count, so a whole `expected` compares exactly.
        if (found as f64 - expected).abs() > 0.0 {
            return Err(if found == 0 {
                format!(
                    "that exact text is not in \"{}\", so nothing was changed",
                    file.name
                )
            } else {
                format!(
                    "expected {} match{} but found {found} in \"{}\", so nothing was changed",
                    js_json::number_to_string(expected),
                    if (expected - 1.0).abs() > 0.0 { "es" } else { "" },
                    file.name
                )
            });
        }
        let next = content.replace(find.as_str(), &replace);
        if next.len() > MAX_TEXT_BYTES {
            return Err("that would make the file too large to write in one call".to_owned());
        }
        self.write_in_place(&project, &plan, &next)?;
        Ok(format!(
            "Replaced {found} occurrence{} in \"{}\".",
            if found == 1 { "" } else { "s" },
            plan.target
        ))
    }
}

/// `String(v == null ? fallback : v)` when `nullish_only`, else `String(v || fallback)`.
fn string_or(v: &JValue, fallback: &str, nullish_only: bool) -> Result<String, String> {
    let empty = if nullish_only {
        v.is_nullish()
    } else {
        !v.truthy()
    };
    if empty {
        return Ok(fallback.to_owned());
    }
    js_json::to_js_string(v).map_err(|_| TO_PRIMITIVE.to_owned())
}

/// `cleanName(raw)`: the last path segment (either slash), trimmed, at most 200 units; `.`/`..`/empty
/// refused.
pub fn clean_name(raw: &JValue) -> Result<String, String> {
    let text = string_or(raw, "", false)?;
    let tail = text.rsplit('/').next().unwrap_or("");
    let tail = tail.rsplit('\\').next().unwrap_or("");
    let name = js_json::js_slice(js_trim(tail), 200);
    if name.is_empty() || name == "." || name == ".." {
        return Err("a filename is required".to_owned());
    }
    Ok(name)
}

/// JS `WhiteSpace` and `LineTerminator` (what `String.prototype.trim` removes).
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'
            | '\u{a}'
            | '\u{b}'
            | '\u{c}'
            | '\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `String.prototype.trim`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

fn is_integer(x: f64) -> bool {
    x.is_finite() && x.fract() == 0.0
}

/// `Number(v)` for a JSON value; an object with an own `toString` throws V8's TypeError.
pub fn to_number(v: &JValue) -> Result<f64, String> {
    Ok(match v {
        JValue::Num(x) => *x,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Null => 0.0,
        JValue::Undefined => f64::NAN,
        JValue::Str(text) => string_to_number(text),
        // ToPrimitive(hint number) on a plain object: valueOf gives the object, then toString.
        JValue::Arr(_) | JValue::Obj(_) => {
            string_to_number(&js_json::to_js_string(v).map_err(|_| TO_PRIMITIVE.to_owned())?)
        }
    })
}

/// `StringToNumber`: JS white space trimmed, `""` is 0, `Infinity`, the 0x/0o/0b forms, else a decimal
/// literal (no `_`, no trailing junk).
pub fn string_to_number(text: &str) -> f64 {
    let t = js_trim(text);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = |digits: &str, base: u32| {
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
            return f64::NAN;
        }
        digits
            .chars()
            .filter_map(|c| c.to_digit(base))
            .fold(0.0, |acc, d| acc * f64::from(base) + f64::from(d))
    };
    let bytes = t.as_bytes();
    if bytes.len() > 2 && bytes.first() == Some(&b'0') {
        match bytes.get(1) {
            Some(b'x' | b'X') => return radix(t.get(2..).unwrap_or(""), 16),
            Some(b'o' | b'O') => return radix(t.get(2..).unwrap_or(""), 8),
            Some(b'b' | b'B') => return radix(t.get(2..).unwrap_or(""), 2),
            _ => {}
        }
    }
    let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (unsigned.get(..at).unwrap_or(""), unsigned.get(at + 1..)),
        None => (unsigned, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((int, frac)) => {
            (digits(int) || int.is_empty())
                && (digits(frac) || frac.is_empty())
                && !(int.is_empty() && frac.is_empty())
        }
        None => digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)));
    if !mantissa_ok || !exponent_ok {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn clean_name_takes_the_last_segment_of_either_slash() {
        let n = |s: &str| clean_name(&JValue::from(s));
        assert_eq!(n("../../etc/passwd").unwrap(), "passwd");
        assert_eq!(n("a\\b\\c.md").unwrap(), "c.md");
        assert_eq!(n("dir/a\\b.md").unwrap(), "b.md");
        assert_eq!(n("  spaced.md \u{feff}").unwrap(), "spaced.md");
        for bad in ["", "dir/", "..", "x/.", "   "] {
            assert_eq!(n(bad).unwrap_err(), "a filename is required", "{bad:?}");
        }
        assert_eq!(clean_name(&JValue::Null).unwrap_err(), "a filename is required");
        // `String(12)` is a name.
        assert_eq!(clean_name(&JValue::Num(12.0)).unwrap(), "12");
        assert_eq!(js_json::js_len(&n(&"x".repeat(300)).unwrap()), 200);
        // NEL is not JS white space, BOM is.
        assert_eq!(n("\u{85}a").unwrap(), "\u{85}a");
    }

    #[test]
    fn number_follows_js() {
        assert_eq!(to_number(&JValue::from("2")).unwrap(), 2.0);
        assert_eq!(to_number(&JValue::from(" 0x10 ")).unwrap(), 16.0);
        assert_eq!(to_number(&JValue::from("")).unwrap(), 0.0);
        assert!(to_number(&JValue::from("2a")).unwrap().is_nan());
        assert_eq!(to_number(&JValue::Arr(vec![JValue::Num(3.0)])).unwrap(), 3.0);
        assert!(to_number(&JValue::Obj(vec![])).unwrap().is_nan());
        assert_eq!(
            to_number(&JValue::obj([("toString", JValue::Num(1.0))])).unwrap_err(),
            TO_PRIMITIVE
        );
        assert_eq!(to_number(&JValue::Bool(true)).unwrap(), 1.0);
    }

    #[test]
    fn definitions_are_the_project_half_with_three_writes() {
        let defs = definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, NAMES);
        let writes: Vec<&str> = defs
            .iter()
            .filter(|d| d.write)
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(
            writes,
            ["project_create_file", "project_append_file", "project_replace_text"]
        );
        for d in &defs {
            let text = d.description.as_deref().unwrap();
            assert!(js_json::js_len(text) < 120, "{}", d.name);
        }
    }
}
