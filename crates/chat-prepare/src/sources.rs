//! The source notes of a project turn's system message:
//!
//! - [`is_unreadable`], [`is_waiting`], [`readable`], [`waiting_notice`]: `source-readability.cjs`.
//! - [`stored_only`]: chat.cjs's "These sources are stored only" line (its `storedOnly`).
//! - [`not_ready`]: chat.cjs's "The user just attached ..." line (`sourcesNotReady`, #1441).
//! - [`coverage_note`]: `source-coverage.cjs` `coverageNote(partial)` (#1513).
//!
//! A file is a project file record as `getProject` holds it after `instruction-skills.cjs`
//! `snapshot`: its `content` is the text Node's accessor reads (#1806), so the caller resolves a
//! `textRef` source's text into `content` before calling these. [`Thrown`] is a `String(value)`
//! that throws (a field holding an object with an own `toString`) or a property read of `null`.

use crate::names::{quote_name, quoted_names};
use crate::{js, Thrown};
use js_json::JValue;

/// `String(value)`.
pub fn string_of(v: &JValue) -> Result<String, Thrown> {
    js_json::to_js_string(v).map_err(|_| Thrown)
}

/// `typeof v === 'object' && v !== null`.
fn is_object(v: &JValue) -> bool {
    matches!(v, JValue::Obj(_) | JValue::Arr(_))
}

/// `!String(file.content || '').trim()`.
fn blank(file: &JValue) -> Result<bool, Thrown> {
    let content = file.get("content");
    if !content.truthy() {
        return Ok(true);
    }
    Ok(js::trim(&string_of(content)?).is_empty())
}

fn is(v: &JValue, s: &str) -> bool {
    v.as_str() == Some(s)
}

/// `isUnreadable(file)` (#586): a stored Text/Documents original, or a failed or queued document
/// read with no earlier text.
pub fn is_unreadable(file: &JValue) -> Result<bool, Thrown> {
    if !is_object(file) {
        return Ok(false);
    }
    let (a, d) = (file.get("attachment"), file.get("document"));
    let group = a.get("group");
    if a.truthy() && is(a.get("state"), "stored") && (is(group, "Text") || is(group, "Documents")) {
        return Ok(true);
    }
    let blank = blank(file)?;
    let state = d.get("state");
    Ok(d.truthy() && (is(state, "failed") || is(state, "queued")) && blank)
}

/// `isWaiting(file)` (#2237): a document queued for its reader with no earlier text.
pub fn is_waiting(file: &JValue) -> Result<bool, Thrown> {
    let d = if is_object(file) {
        file.get("document")
    } else {
        &JValue::Null
    };
    if !(d.truthy() && is(d.get("state"), "queued")) {
        return Ok(false);
    }
    blank(file)
}

/// `Array.isArray(files) ? files : []`.
fn list(files: &JValue) -> &[JValue] {
    match files {
        JValue::Arr(items) => items,
        _ => &[],
    }
}

/// `readable(files)`: the files that are not [`is_unreadable`], in order.
pub fn readable(files: &JValue) -> Result<Vec<&JValue>, Thrown> {
    let mut out = Vec::new();
    for f in list(files) {
        if !is_unreadable(f)? {
            out.push(f);
        }
    }
    Ok(out)
}

/// `waitingNotice(files)`: the line naming the documents waiting to be read (at most ten names,
/// the rest counted), or `None` when none are.
pub fn waiting_notice(files: &JValue) -> Result<Option<String>, Thrown> {
    let mut waiting = Vec::new();
    for f in list(files) {
        if is_waiting(f)? {
            waiting.push(f);
        }
    }
    let all = waiting
        .into_iter()
        .map(|f| string_of(f.get("name")))
        .collect::<Result<Vec<_>, _>>()?;
    if all.is_empty() {
        return Ok(None);
    }
    let names = all.get(..all.len().min(10)).unwrap_or(&[]);
    let more = all.len() - names.len();
    let one = all.len() == 1;
    let pick = |a: &'static str, b: &'static str| if one { a } else { b };
    let more = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    Ok(Some(format!(
        "{}{more} {} waiting to be read (document reading is not available yet), so {} contents are not available to you. Say so if asked about {}; do not guess at the contents or call {} empty.",
        quoted_names(names),
        pick("is", "are"),
        pick("its", "their"),
        pick("it", "them"),
        pick("it", "them"),
    )))
}

/// chat.cjs's `storedOnly` line: the [`readable`] files whose attachment is `stored` (#586), named;
/// `None` when there are none. A `null` entry among the readable files throws, as
/// `f.attachment?.state` does in Node.
pub fn stored_only(files: &JValue) -> Result<Option<String>, Thrown> {
    let mut names = Vec::new();
    for f in readable(files)? {
        if f.is_nullish() {
            return Err(Thrown);
        }
        if is(f.get("attachment").get("state"), "stored") {
            names.push(string_of(f.get("name"))?);
        }
    }
    if names.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "These sources are stored only; their contents are NOT available to the model: {}. Do not claim to know their contents.",
        quoted_names(&names)
    )))
}

/// One entry of `sourceJobs.settle(...).notReady`: an attached file whose job did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotReady {
    /// `String(f.name)`.
    pub name: String,
    /// `f.reason === 'failed'` (the other reason is `'timeout'`).
    pub failed: bool,
}

/// chat.cjs's "The user just attached ..." line, or `None` for an empty list.
pub fn not_ready(files: &[NotReady]) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    let one = files.len() == 1;
    let pick = |a: &'static str, b: &'static str| if one { a } else { b };
    let done = if files.iter().all(|f| f.failed) {
        "processed"
    } else {
        "finished processing"
    };
    Some(format!(
        "The user just attached {}, but {} not {done} in time, so {} contents are not available to you for this message. Say so if asked about {}; do not guess at the contents.",
        quoted_names(&names),
        pick("it was", "they were"),
        pick("its", "their"),
        pick("it", "them"),
    ))
}

/// One entry of `source-coverage.cjs` `partialSources(readable, injected)`: a source this reply
/// read only part of. (Computed from what retrieval placed, so the caller passes it in.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partial {
    /// `String(f.name)`.
    pub name: String,
    /// The source's text length in UTF-16 units (`source-text.cjs` `textLength`).
    pub length: u64,
    /// The units of it placed in the prompt (at most `length`).
    pub read: u64,
}

/// `Number(n).toLocaleString('en-GB')` of a whole number: groups of three digits, comma-separated.
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `coverageNote(partial)`: the note naming what each partly read source had read, or `None`
/// when every source was read whole.
pub fn coverage_note(partial: &[Partial]) -> Option<String> {
    if partial.is_empty() {
        return None;
    }
    let parts: Vec<String> = partial
        .iter()
        .map(|p| {
            let read = if p.read > 0 {
                format!(
                    "only {} characters of it (excerpts or the start)",
                    grouped(p.read)
                )
            } else {
                "none of it (it was not included)".to_string()
            };
            format!(
                "{} is {} characters long and this reply read {read}",
                quote_name(&p.name),
                grouped(p.length)
            )
        })
        .collect();
    Some(format!(
        "Source text was truncated for this reply: {}. The rest of each file was NOT read. Do not say that some text is absent from a file, or answer \"no\", from the part you read; say which part you read.{READ_FILE_HINT}",
        parts.join("; ")
    ))
}

/// `READ_FILE_HINT`, the sentence a hybrid cloud route without a grant removes (#1646, a later slice).
pub const READ_FILE_HINT: &str = " read_project_file with an offset can read more of a file.";
