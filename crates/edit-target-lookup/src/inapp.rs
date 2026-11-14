//! noevia-core's in-app MCP project write tools (track R14 step 6): mcp-internal-tools.cjs
//! `project_create_file`, `project_append_file` and `project_replace_text` (argument handling,
//! the #648 pin, the new text, every message), and mcp-internal.cjs's result for a `tools/call`
//! (`{ content: [{ type: 'text', text }] }`, a thrown error as `ERROR: <message, 300 units>` with
//! `isError: true`).
//!
//! [`prepare`] runs a handler up to its write and answers either the result (a refusal) or the
//! write to make (core's `writeTextFile(project, name, text, options)`) with the text to answer
//! when it succeeds; [`finish`] shapes the write's outcome. The caller runs the write
//! (server_projects::text_write) as the account the token names. [`Prepared::Unsupported`] is a
//! shape core never stores (`files` that is not a list, a truthy non-string `projectFolder`,
//! `source` or `reservedFolder`): the caller lets core's server answer. The `content` closure
//! must read `String(file.content || '')` (a source's text); core reads `file.content` itself,
//! which differs only for a non-string inline value core never stores.
use crate::{plan_edit_file, target_digest, EditPlan};
use js_json::{wtf16, JValue};

/// mcp-internal-tools.cjs `MAX_TEXT_BYTES`.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// The tools this module answers.
pub const TOOLS: [&str; 3] = [
    "project_create_file",
    "project_append_file",
    "project_replace_text",
];

/// core's `writeTextFile` options (`undefined` = `None`).
#[derive(Clone, Debug, PartialEq)]
pub struct WriteOptions {
    pub expect_name: String,
    pub expect_content: String,
    /// `plan.file.attachment ? plan.file.attachment.id : null`.
    pub expect_attachment: JValue,
    pub adopt_to: Option<String>,
    pub adopt_account: Option<String>,
}

/// The write a handler makes.
#[derive(Clone, Debug, PartialEq)]
pub struct Write {
    pub name: String,
    pub text: String,
    /// `None` for a create (core passes no options).
    pub options: Option<WriteOptions>,
    /// The handler's answer when the write succeeds.
    pub success: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Prepared {
    /// The tool's result now (a refusal before any write).
    Answer(JValue),
    Write(Write),
    /// Let core's server answer.
    Unsupported,
}

/// mcp-internal.cjs `textResult(text, isError)`.
pub fn text_result(text: &str, is_error: bool) -> JValue {
    let content = JValue::Arr(vec![JValue::obj([
        ("type", JValue::Str("text".into())),
        ("text", JValue::Str(text.to_owned())),
    ])]);
    if is_error {
        JValue::obj([("content", content), ("isError", JValue::Bool(true))])
    } else {
        JValue::obj([("content", content)])
    }
}

/// A thrown error as the server answers it: `ERROR: ${message.slice(0, 300)}`.
pub fn error_result(message: &str) -> JValue {
    // `String((err && err.message) || err)`: an empty message reads as the error's name.
    let message = if message.is_empty() { "Error" } else { message };
    text_result(&format!("ERROR: {}", wtf16::slice(message, 300)), true)
}

/// The result of a write's outcome: the handler's answer, or the error it threw.
pub fn finish(success: &str, outcome: Result<(), String>) -> JValue {
    match outcome {
        Ok(()) => text_result(success, false),
        Err(message) => error_result(&message),
    }
}

/// V8's text for `String(v)` of an object with its own `toString`.
const NO_PRIMITIVE: &str = "Cannot convert object to primitive value";

/// `String(v)`, or the TypeError's message.
fn js_string(v: &JValue) -> Result<String, String> {
    js_json::to_js_string(v).map_err(|_| NO_PRIMITIVE.to_owned())
}

/// `String(v == null ? '' : v)`.
fn text_arg(v: &JValue) -> Result<String, String> {
    if v.is_nullish() {
        Ok(String::new())
    } else {
        js_string(v)
    }
}

/// UTF-16 code units of a wtf16 string.
fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        match wtf16::lone_unit(c) {
            Some(u) => out.push(u),
            None => out.extend_from_slice(c.encode_utf16(&mut [0; 2])),
        }
    }
    out
}

fn from_units(u: &[u16]) -> String {
    char::decode_utf16(u.iter().copied())
        .map(|r| {
            r.unwrap_or_else(|e| wtf16::lone_char(e.unpaired_surrogate()).unwrap_or('\u{fffd}'))
        })
        .collect()
}

/// `Buffer.byteLength(text)`: UTF-8, a lone surrogate as the three bytes of U+FFFD.
fn byte_length(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                3
            } else {
                c.len_utf8()
            }
        })
        .sum()
}

/// mcp-internal-tools.cjs `cleanName(raw)`.
fn clean_name(raw: &JValue) -> Result<String, String> {
    let s = if raw.truthy() {
        js_string(raw)?
    } else {
        String::new()
    };
    let last = s.rsplit('/').next().unwrap_or_default();
    let last = last.rsplit('\\').next().unwrap_or_default();
    let trimmed = from_units(prompt_framing::js::trim(&units(last)));
    let name = wtf16::slice(&trimmed, 200);
    if name.is_empty() || name == "." || name == ".." {
        return Err("a filename is required".into());
    }
    Ok(name)
}

/// project-edit-target.cjs `uploadPathFor(project, base)`; `None` for a truthy non-string folder.
fn upload_path_for(project: &JValue, base: &str) -> Option<String> {
    match project.get("projectFolder") {
        JValue::Str(f) if !f.is_empty() => {
            let group = project_edit_target::classify(&units(base));
            Some(format!("{f}/{group}/{base}"))
        }
        v if !v.truthy() => Some(base.to_owned()),
        _ => None,
    }
}

/// JS `StringToNumber` over a trimmed string: `NaN` for anything that is not a numeric literal.
fn string_to_number(t: &str) -> f64 {
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = |prefix: [&str; 2], base: u32| {
        prefix.iter().find_map(|p| t.strip_prefix(p)).map(|digits| {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
                f64::NAN
            } else {
                digits.chars().fold(0.0, |n, c| {
                    n * f64::from(base) + f64::from(c.to_digit(base).unwrap_or(0))
                })
            }
        })
    };
    if let Some(n) = radix(["0x", "0X"], 16)
        .or_else(|| radix(["0o", "0O"], 8))
        .or_else(|| radix(["0b", "0B"], 2))
    {
        return n;
    }
    // StrDecimalLiteral: [+-] (digits [. digits?] | . digits) [(e|E) [+-] digits]
    let b = t.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let int = b
        .get(i..)
        .map_or(0, |r| r.iter().take_while(|c| c.is_ascii_digit()).count());
    i += int;
    let mut frac = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        frac = b
            .get(i..)
            .map_or(0, |r| r.iter().take_while(|c| c.is_ascii_digit()).count());
        i += frac;
    }
    if int + frac == 0 {
        return f64::NAN;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp = b
            .get(i..)
            .map_or(0, |r| r.iter().take_while(|c| c.is_ascii_digit()).count());
        if exp == 0 {
            return f64::NAN;
        }
        i += exp;
    }
    if i != b.len() {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `Number(v)`, or the TypeError's message (an object with its own `toString`).
fn js_number(v: &JValue) -> Result<f64, String> {
    Ok(match v {
        JValue::Num(n) => *n,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Null => 0.0,
        JValue::Undefined => f64::NAN,
        JValue::Str(s) => string_to_number(&from_units(prompt_framing::js::trim(&units(s)))),
        JValue::Arr(_) | JValue::Obj(_) => {
            let s = js_string(v)?;
            string_to_number(&from_units(prompt_framing::js::trim(&units(&s))))
        }
    })
}

/// `str.split(find).length - 1` and `parts.join(replace)` over code units (non-overlapping, from
/// the left, as JS splits a string by a string), linear in the text (Knuth-Morris-Pratt).
fn split_replace(content: &str, find: &str, replace: &str) -> (usize, String) {
    let (hay, needle, with) = (units(content), units(find), units(replace));
    if needle.is_empty() {
        return (0, content.to_owned());
    }
    // The failure table: the longest proper prefix of needle[..=k] that is also its suffix.
    let mut fail = vec![0usize; needle.len()];
    let mut k = 0;
    for q in 1..needle.len() {
        while k > 0 && needle.get(q) != needle.get(k) {
            k = fail.get(k - 1).copied().unwrap_or(0);
        }
        if needle.get(q) == needle.get(k) {
            k += 1;
        }
        if let Some(slot) = fail.get_mut(q) {
            *slot = k;
        }
    }
    let mut out = Vec::with_capacity(hay.len());
    let (mut matched, mut found, mut last) = (0usize, 0usize, 0usize);
    for (i, u) in hay.iter().enumerate() {
        while matched > 0 && needle.get(matched) != Some(u) {
            matched = fail.get(matched - 1).copied().unwrap_or(0);
        }
        if needle.get(matched) == Some(u) {
            matched += 1;
        }
        if matched == needle.len() {
            let start = i + 1 - needle.len();
            out.extend_from_slice(hay.get(last..start).unwrap_or_default());
            out.extend_from_slice(&with);
            last = i + 1;
            found += 1;
            // Non-overlapping: the next match starts after this one.
            matched = 0;
        }
    }
    out.extend_from_slice(hay.get(last..).unwrap_or_default());
    (found, from_units(&out))
}

fn number_text(n: f64) -> String {
    js_json::number_to_string(n)
}

/// One `tools/call` of an in-app write tool, up to its write. `project` is the handler's
/// `ports.getProject(ctx.projectId)`; `account` its `ports.storageAccount()`; `edit_target` the
/// token's pin (`claims.t`); `content` reads a source's text as `file.content` does.
pub fn prepare(
    tool: &str,
    args: &JValue,
    project: Option<&JValue>,
    account: Option<&str>,
    edit_target: Option<&str>,
    content: &dyn Fn(&JValue) -> String,
) -> Prepared {
    // mcp-internal.cjs: arguments that are not an object are `{}`.
    let empty = JValue::Obj(Vec::new());
    let args = if matches!(args, JValue::Obj(_)) {
        args
    } else {
        &empty
    };
    match handle(tool, args, project, account, edit_target, content) {
        Ok(Some(p)) => p,
        Ok(None) => Prepared::Unsupported,
        Err(message) => Prepared::Answer(error_result(&message)),
    }
}

fn handle(
    tool: &str,
    args: &JValue,
    project: Option<&JValue>,
    account: Option<&str>,
    edit_target: Option<&str>,
    content: &dyn Fn(&JValue) -> String,
) -> Result<Option<Prepared>, String> {
    // requireProject
    let Some(project) = project.filter(|p| p.truthy()) else {
        return Err("this chat is not in a project, so there are no project files".into());
    };
    match tool {
        "project_create_file" => {
            let name = clean_name(args.get("name"))?;
            let text = text_arg(args.get("text"))?;
            if byte_length(&text) > MAX_TEXT_BYTES {
                return Err("that text is too large to write in one call".into());
            }
            let files: &[JValue] = match project.get("files") {
                JValue::Arr(items) => items,
                v if !v.truthy() => &[],
                _ => return Ok(None),
            };
            // `files.some((f) => f.name === name || f.name === uploadPathFor(project, name))`: the
            // upload path only for an entry whose name did not match.
            for f in files {
                if matches!(f, JValue::Null) {
                    return Err("Cannot read properties of null (reading 'name')".into());
                }
                let n = f.get("name").as_str();
                if n == Some(name.as_str()) {
                    return Err(format!("\"{name}\" already exists. Use project_append_file or project_replace_text."));
                }
                let Some(upload) = upload_path_for(project, &name) else {
                    return Ok(None);
                };
                if n == Some(upload.as_str()) {
                    return Err(format!("\"{name}\" already exists. Use project_append_file or project_replace_text."));
                }
            }
            let success = format!("Created \"{name}\" ({} chars).", wtf16::len(&text));
            Ok(Some(Prepared::Write(Write {
                name,
                text,
                options: None,
                success,
            })))
        }
        "project_append_file" | "project_replace_text" => {
            let account = account.filter(|a| !a.is_empty());
            let plan = match plan_edit_file(project, args.get("name"), account, content) {
                Ok(Ok(plan)) => plan,
                Ok(Err(message)) => return Err(message),
                Err(crate::Unsupported) => return Ok(None),
            };
            let pinned = edit_target.unwrap_or_default();
            if pinned.is_empty() {
                return Err(
                    "this edit was not tied to a file when it was approved, so nothing was changed"
                        .into(),
                );
            }
            if pinned != target_digest(&plan.target, plan.account.as_deref()) {
                let shown = wtf16::stringify(&JValue::Str(js_string(args.get("name"))?))
                    .unwrap_or_default();
                return Err(format!("{shown} now refers to \"{}\", which is not the file that was approved. Nothing was changed; ask again.", plan.target));
            }
            let JValue::Arr(files) = project.get("files") else {
                return Ok(None);
            };
            let Some(file) = files.get(plan.index) else {
                return Ok(None);
            };
            let current = content(file);
            let (next, success) = if tool == "project_append_file" {
                let addition = text_arg(args.get("text"))?;
                if addition.is_empty() {
                    return Err("there is nothing to append".into());
                }
                let sep = if current.is_empty() || current.ends_with('\n') {
                    ""
                } else {
                    "\n"
                };
                let success = format!(
                    "Appended {} chars to \"{}\".",
                    wtf16::len(&addition),
                    plan.target
                );
                (format!("{current}{sep}{addition}"), success)
            } else {
                let find = text_arg(args.get("find"))?;
                if find.is_empty() {
                    return Err("find must not be empty".into());
                }
                let replace = text_arg(args.get("replace"))?;
                let (found, next) = split_replace(&current, &find, &replace);
                let expected = match args.get("expectedCount") {
                    JValue::Undefined => 1.0,
                    v => js_number(v)?,
                };
                if expected.fract() != 0.0 || !expected.is_finite() || expected < 1.0 {
                    return Err("expectedCount must be a positive whole number".into());
                }
                let fname = file.get("name").as_str().unwrap_or_default();
                if found as f64 != expected {
                    return Err(if found == 0 {
                        format!("that exact text is not in \"{fname}\", so nothing was changed")
                    } else {
                        format!(
                            "expected {} match{} but found {found} in \"{fname}\", so nothing was changed",
                            number_text(expected),
                            if expected == 1.0 { "" } else { "es" }
                        )
                    });
                }
                let success = format!(
                    "Replaced {found} occurrence{} in \"{}\".",
                    if found == 1 { "" } else { "s" },
                    plan.target
                );
                (next, success)
            };
            if byte_length(&next) > MAX_TEXT_BYTES {
                return Err("that would make the file too large to write in one call".into());
            }
            let EditPlan {
                write_name,
                target,
                adopt,
                account,
                ..
            } = plan;
            let attachment = file.get("attachment");
            let options = WriteOptions {
                expect_name: file.get("name").as_str().unwrap_or_default().to_owned(),
                expect_content: current,
                expect_attachment: if attachment.truthy() {
                    attachment.get("id").clone()
                } else {
                    JValue::Null
                },
                adopt_to: adopt.then(|| target.clone()),
                adopt_account: if adopt { account } else { None },
            };
            Ok(Some(Prepared::Write(Write {
                name: write_name,
                text: next,
                options: Some(options),
                success,
            })))
        }
        _ => Ok(None),
    }
}
