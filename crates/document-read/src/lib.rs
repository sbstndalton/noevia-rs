//! What a stored project document says about itself and its pages: noevia-core
//! `server/document-sources.cjs` `notes`, `noteText`, `problem`, `problemNotes`, `notice` and
//! `readPages`, answer for answer, recorded from core (`tests/record_document_read.cjs`).
//!
//! Node still runs these for the chat's `read_project_file` tool (toolboxes.cjs), the retrieval
//! context's per-document notice (rag.cjs `filesContext`) and the in-app MCP read tool; the front's
//! `GET documents/pages` has its own copy (crates/server-projects document_pages.rs). This crate is
//! the one native copy those callers can share.
//!
//! Inputs are the JSON a source record holds (`file.document`, written by the upload pipeline),
//! read as JavaScript reads it: template literals and `+` coerce with `ToString`, comparisons with
//! `ToNumber`, `===` is strict (two objects are never equal). Where the JS throws a TypeError (a
//! `null` page entry, a `pageStatus` that is not an array, an object with an own `toString`) the
//! port returns [`Thrown`].
//!
//! Differences, named (none reachable with records the pipeline writes):
//! - [`read_pages`]: a stored result that cannot be read or parsed is [`ReadError::Unreadable`]
//!   with a fixed message; Node's message there is the fs or `JSON.parse` error text (with the
//!   server's file path), which is not copied. The result is opened without following a symlink
//!   and read only up to [`RESULT_BYTES`] (as the front's route does); JSON the port's parser
//!   refuses but `JSON.parse` takes (a number past the `f64` range, nesting past 128) is
//!   `Unreadable` too.
//! - Two page texts whose ends are halves of one surrogate pair are joined as two lone halves
//!   (`\ud83d\ude00` in JSON) where JS would join them into the pair.
use js_json::{to_js_string, wtf16, JValue};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// documents.cjs `RETRY_PAGE_CAP` (the OCR retry limit the notes name).
pub const RETRY_PAGE_CAP: u32 = 20;
/// `readPages`' default `cap` (toolboxes.cjs `TOOL_RESULT_CAP` and the route's).
pub const READ_CAP: usize = 8000;
/// The largest stored extraction result [`stored_result`] reads.
pub const RESULT_BYTES: u64 = 256 * 1024 * 1024;

/// A stored result that could not be read or parsed ([`stored_result`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unreadable;

/// Where the JS throws a TypeError (or another error without a status) on the data it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thrown;

/// `readPages`' failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// An error Node throws with this status and message.
    Status(u16, &'static str),
    /// versionPath's `invalid document version` (no status).
    InvalidVersion,
    /// The stored result could not be read or parsed (no status).
    Unreadable,
    /// A TypeError on the record or the stored result (no status).
    Thrown,
}

/// The front route's words for every failure without a status (routes/projects.cjs `docRead`).
pub const UNAVAILABLE: &str = "Saved document data is unavailable; upload or refresh again.";

impl ReadError {
    /// The status Node's error carries, if any.
    pub fn status(&self) -> Option<u16> {
        match self {
            ReadError::Status(s, _) => Some(*s),
            _ => None,
        }
    }

    /// Node's `err.message` (see the crate notes for [`ReadError::Unreadable`] and
    /// [`ReadError::Thrown`], where Node's text is not copied).
    pub fn message(&self) -> &'static str {
        match self {
            ReadError::Status(_, m) => m,
            ReadError::InvalidVersion => "invalid document version",
            ReadError::Unreadable | ReadError::Thrown => UNAVAILABLE,
        }
    }
}

impl From<Thrown> for ReadError {
    fn from(_: Thrown) -> Self {
        ReadError::Thrown
    }
}

/// The selected pages: `readPages`' `{ text, nextOffset, notice, startPage, endPage }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pages {
    pub text: String,
    pub next_offset: Option<f64>,
    pub notice: String,
    pub start_page: f64,
    pub end_page: f64,
}

impl Pages {
    /// The object Node returns, keys in its order.
    pub fn to_json(&self) -> JValue {
        JValue::obj([
            ("text", JValue::Str(self.text.clone())),
            ("nextOffset", self.next_offset.map_or(JValue::Null, JValue::Num)),
            ("notice", JValue::Str(self.notice.clone())),
            ("startPage", JValue::Num(self.start_page)),
            ("endPage", JValue::Num(self.end_page)),
        ])
    }
}

fn js_str(v: &JValue) -> Result<String, Thrown> {
    to_js_string(v).map_err(|_| Thrown)
}

/// `a === b`.
fn strict_eq(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Undefined, JValue::Undefined) | (JValue::Null, JValue::Null) => true,
        (JValue::Bool(x), JValue::Bool(y)) => x == y,
        (JValue::Num(x), JValue::Num(y)) => x == y,
        (JValue::Str(x), JValue::Str(y)) => x == y,
        _ => false,
    }
}

/// `Number.isInteger(v)`.
fn integer(v: &JValue) -> Option<f64> {
    v.as_num().filter(|n| n.is_finite() && n.trunc() == *n)
}

/// JS `\s`: what `String.prototype.trim` strips.
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// JS `StringToNumber` (what `Number(string)` and a numeric comparison do to a string).
fn string_to_number(raw: &str) -> f64 {
    let s = raw.trim_matches(is_space);
    if s.is_empty() {
        return 0.0;
    }
    match s {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = s.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return digits.chars().fold(0.0, |acc, c| {
                acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            });
        }
    }
    // StrDecimalLiteral: sign? (digits [. digits?] | . digits) ([eE] sign? digits)?
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let count = |from: usize| {
        b.get(from..)
            .map_or(0, |r| r.iter().take_while(|c| c.is_ascii_digit()).count())
    };
    let int = count(i);
    i += int;
    let mut frac = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        frac = count(i);
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
        let exp = count(i);
        if exp == 0 {
            return f64::NAN;
        }
        i += exp;
    }
    if i != b.len() {
        return f64::NAN;
    }
    s.parse::<f64>().unwrap_or(f64::NAN)
}

/// JS `ToNumber(v)` for a JSON value (an array through its string, an object NaN, or a TypeError
/// when it has an own `toString`).
fn to_number(v: &JValue) -> Result<f64, Thrown> {
    Ok(match v {
        JValue::Undefined => f64::NAN,
        JValue::Null => 0.0,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Num(n) => *n,
        JValue::Str(s) => string_to_number(s),
        JValue::Arr(_) => string_to_number(&js_str(v)?),
        JValue::Obj(items) => {
            if items.iter().any(|(k, _)| k == "toString") {
                return Err(Thrown);
            }
            f64::NAN
        }
    })
}

/// quoted-name.cjs `quoteName(name)`: `JSON.stringify(String(name))`, then the line breaks and
/// direction marks JSON.stringify leaves raw written as `\uXXXX`.
pub fn quote_name(name: &JValue) -> Result<String, Thrown> {
    let quoted = wtf16::stringify(&JValue::Str(js_str(name)?)).ok_or(Thrown)?;
    let mut out = String::with_capacity(quoted.len());
    for c in quoted.chars() {
        let u = u32::from(c);
        if u == 0x85
            || u == 0x200e
            || u == 0x200f
            || u == 0x2028
            || u == 0x2029
            || (0x202a..=0x202e).contains(&u)
            || (0x2066..=0x2069).contains(&u)
        {
            out.push_str(&format!("\\u{u:04x}"));
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// `pageList(pages)`: the first 20 page numbers joined with ", " (`Array.prototype.join`: null
/// and undefined are empty), and "…" past 20.
fn page_list(pages: &[&JValue]) -> Result<String, Thrown> {
    let mut parts = Vec::new();
    for p in pages.iter().take(20) {
        let n = p.get("number");
        parts.push(if n.is_nullish() {
            String::new()
        } else {
            js_str(n)?
        });
    }
    let mut out = parts.join(", ");
    if pages.len() > 20 {
        out.push('…');
    }
    Ok(out)
}

fn note(id: &str, params: Vec<(&str, JValue)>) -> JValue {
    JValue::obj([("id", JValue::Str(id.into())), ("params", JValue::obj(params))])
}

/// `notes(file)`: the per-page outcomes as `[{ id, params }]` (where reading stopped, how far the
/// text is searchable, each OCR retry outcome, failed pages grouped by reason); `[]` without a
/// document.
pub fn notes(file: &JValue) -> Result<Vec<JValue>, Thrown> {
    let d = file.get("document");
    if !d.truthy() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let stopped = d.get("stoppedAt");
    if stopped.truthy() {
        let mut params = vec![
            ("page", stopped.get("page").clone()),
            ("limit", stopped.get("limit").clone()),
        ];
        if stopped.get("error").truthy() {
            params.push(("reason", stopped.get("error").clone()));
        }
        out.push(note("stoppedAtPage", params));
    }
    let searchable = d.get("searchable");
    if searchable.truthy()
        && strict_eq(searchable.get("version"), d.get("availableVersion"))
        && strict_eq(searchable.get("complete"), &JValue::Bool(false))
        && integer(searchable.get("page")).is_some()
    {
        out.push(note(
            "searchableToPage",
            vec![("page", searchable.get("page").clone())],
        ));
    }
    let pages: Vec<&JValue> = match d.get("pageStatus") {
        JValue::Arr(a) => a.iter().collect(),
        _ => Vec::new(),
    };
    // `p.retry?.outcome`: reading `retry` of a null entry throws.
    if pages.iter().any(|p| p.is_nullish()) {
        return Err(Thrown);
    }
    let by = |outcome: &str| -> Vec<&JValue> {
        pages
            .iter()
            .copied()
            .filter(|p| strict_eq(p.get("retry").get("outcome"), &JValue::Str(outcome.into())))
            .collect()
    };
    // A Map keyed by `p.retry.error || 'unknown'` (SameValueZero: two objects never share a key).
    let mut failed: Vec<(JValue, Vec<&JValue>)> = Vec::new();
    for p in by("failed") {
        let e = p.get("retry").get("error");
        let reason = if e.truthy() {
            e.clone()
        } else {
            JValue::Str("unknown".into())
        };
        match failed.iter_mut().find(|(r, _)| strict_eq(r, &reason)) {
            Some((_, list)) => list.push(p),
            None => failed.push((reason, vec![p])),
        }
    }
    for (reason, list) in failed {
        out.push(note(
            "ocrRetryFailed",
            vec![("pages", JValue::Str(page_list(&list)?)), ("reason", reason)],
        ));
    }
    for (outcome, id) in [
        ("empty", "ocrRetryEmpty"),
        ("unavailable", "ocrRetryUnavailable"),
        ("skipped", "ocrRetryLimit"),
        ("recovered", "ocrRetryRecovered"),
    ] {
        let list = by(outcome);
        if !list.is_empty() {
            out.push(note(id, vec![("pages", JValue::Str(page_list(&list)?))]));
        }
    }
    Ok(out)
}

/// The properties a plain object inherits from `Object.prototype`, as `${STOP_WORDS[key]}` writes
/// them (a native function's source text; `__proto__` is `Object.prototype` itself).
fn inherited(key: &str) -> Option<String> {
    match key {
        "constructor" => Some("function Object() { [native code] }".into()),
        "__defineGetter__" | "__defineSetter__" | "hasOwnProperty" | "__lookupGetter__"
        | "__lookupSetter__" | "isPrototypeOf" | "propertyIsEnumerable" | "toString"
        | "valueOf" | "toLocaleString" => Some(format!("function {key}() {{ [native code] }}")),
        "__proto__" => Some("[object Object]".into()),
        _ => None,
    }
}

/// `noteText(n)`: the English for one note ('' for an id it does not know).
pub fn note_text(n: &JValue) -> Result<String, Thrown> {
    if n.is_nullish() {
        return Err(Thrown);
    }
    let params = n.get("params");
    let empty = JValue::Obj(Vec::new());
    let p = if params.truthy() { params } else { &empty };
    Ok(match n.get("id").as_str() {
        Some("stoppedAtPage") => {
            let page = js_str(p.get("page"))?;
            let limit = p.get("limit");
            let key = js_str(limit)?;
            let words = match key.as_str() {
                "pages" => "page limit".to_string(),
                "text" => "text limit".to_string(),
                "error" => "read error".to_string(),
                other => match inherited(other) {
                    Some(w) => w,
                    None => js_str(limit)?,
                },
            };
            let reason = p.get("reason");
            let reason = if reason.truthy() {
                format!(": {}", js_str(reason)?)
            } else {
                String::new()
            };
            format!("stopped at page {page} ({words}{reason})")
        }
        Some("ocrRetryFailed") => format!(
            "pages {}: OCR retry failed: {}",
            js_str(p.get("pages"))?,
            js_str(p.get("reason"))?
        ),
        Some("ocrRetryEmpty") => format!("pages {}: OCR retry found no text", js_str(p.get("pages"))?),
        Some("ocrRetryUnavailable") => format!(
            "pages {}: OCR retry unavailable (no OCR service is configured)",
            js_str(p.get("pages"))?
        ),
        Some("ocrRetryLimit") => format!(
            "pages {}: not retried (OCR retry limit of {RETRY_PAGE_CAP} pages)",
            js_str(p.get("pages"))?
        ),
        Some("searchableToPage") => format!(
            "searchable up to page {} so far; later pages are still being indexed",
            js_str(p.get("page"))?
        ),
        Some("ocrRetryRecovered") => format!(
            "pages {}: recovered by an OCR retry; verify numbers against the original",
            js_str(p.get("pages"))?
        ),
        _ => String::new(),
    })
}

/// `s.replace(/\.$/, '')`: one trailing full stop off the end of the text.
fn strip_stop(s: &str) -> &str {
    s.strip_suffix('.').unwrap_or(s)
}

/// `problem(file, { notes })`: the document's state, pages, limits, incomplete pages, notes (with
/// `with_notes`) and error, ending in "."; '' without a document.
pub fn problem(file: &JValue, with_notes: bool) -> Result<String, Thrown> {
    if file.is_nullish() {
        return Err(Thrown);
    }
    let d = file.get("document");
    if !d.truthy() {
        return Ok(String::new());
    }
    let stale = if d.get("stale").truthy() {
        "; using STALE text from the last readable version"
    } else {
        ""
    };
    if strict_eq(d.get("state"), &JValue::Str("queued".into())) {
        return Ok(format!("waiting to be read; document reading is not available yet and the file will be read automatically{stale}."));
    }
    // `(d.pageStatus || []).filter(...)`: a truthy value that is not an array has no `filter`.
    let pages: Vec<&JValue> = match d.get("pageStatus") {
        JValue::Arr(a) => a.iter().collect(),
        v if !v.truthy() => Vec::new(),
        _ => return Err(Thrown),
    };
    if pages.iter().any(|p| p.is_nullish()) {
        return Err(Thrown);
    }
    let issues: Vec<&JValue> = pages
        .into_iter()
        .filter(|p| !matches!(p.get("status").as_str(), Some("native" | "blank" | "ocr")))
        .collect();
    let mut extra = Vec::new();
    if with_notes {
        for n in notes(file)? {
            let t = note_text(&n)?;
            if !t.is_empty() {
                extra.push(t);
            }
        }
    }
    let mut s = js_str(d.get("state"))?;
    s.push_str(stale);
    if d.get("pages").truthy() {
        s.push_str(&format!("; {} pages", js_str(d.get("pages"))?));
    }
    if d.get("truncated").truthy() {
        s.push_str("; summary/page extraction limits reached");
    }
    if !issues.is_empty() {
        let mut listed = Vec::new();
        for p in issues.iter().take(20) {
            let reason = p.get("reason");
            listed.push(format!(
                "{} ({}{})",
                js_str(p.get("number"))?,
                js_str(p.get("status"))?,
                if reason.truthy() {
                    format!(": {}", js_str(reason)?)
                } else {
                    String::new()
                }
            ));
        }
        s.push_str("; incomplete pages ");
        s.push_str(&listed.join(", "));
        if issues.len() > 20 {
            s.push('…');
        }
    }
    if !extra.is_empty() {
        s.push_str("; ");
        s.push_str(strip_stop(&extra.join("; ")));
    }
    // `${d.error ? '; ' + d.error : ''}`.replace(/\.$/, '') + '.'
    let error = d.get("error");
    if error.truthy() {
        s.push_str(strip_stop(&format!("; {}", js_str(error)?)));
    }
    s.push('.');
    Ok(s)
}

/// `problemNotes(file)`: `{ reasonBase, notes }` (the problem without the notes, and the notes) for
/// a document with notes, else `{}`.
pub fn problem_notes(file: &JValue) -> Result<JValue, Thrown> {
    let list = notes(file)?;
    if list.is_empty() {
        return Ok(JValue::Obj(Vec::new()));
    }
    Ok(JValue::obj([
        ("reasonBase", JValue::Str(problem(file, false)?)),
        ("notes", JValue::Arr(list)),
    ]))
}

/// `notice(file)`: the quoted name, the problem, the source and text versions and the index state;
/// '' without a document.
pub fn notice(file: &JValue) -> Result<String, Thrown> {
    if file.is_nullish() {
        return Err(Thrown);
    }
    let d = file.get("document");
    if !d.truthy() {
        return Ok(String::new());
    }
    let mut s = format!("{}: {}", quote_name(file.get("name"))?, problem(file, true)?);
    if d.get("version").truthy() {
        s.push_str(&format!(" Source version {}", js_str(d.get("version"))?));
    }
    if d.get("availableVersion").truthy() {
        s.push_str(&format!("; text version {}", js_str(d.get("availableVersion"))?));
    }
    let indexing = d.get("indexing");
    s.push_str(&format!(
        "; index: {}.",
        if indexing.truthy() {
            js_str(indexing)?
        } else {
            "unavailable".into()
        }
    ));
    Ok(s)
}

/// `directory(workspace, projectId)`: `<data dir>/project-documents/<sha256 of the project id>`.
pub fn documents_dir(data_dir: &Path, project_id: &str) -> PathBuf {
    let hex: String = Sha256::digest(project_id.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    data_dir.join("project-documents").join(hex)
}

/// `/^[a-f0-9]{64}$/`.
fn version_like(v: &str) -> bool {
    v.len() == 64 && v.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `JSON.parse(fs.readFileSync(versionPath(workspace, projectId, version, '.json'), 'utf8'))`: the
/// stored extraction result, opened without following a symlink and read up to [`RESULT_BYTES`].
/// `Err` for a version that is not 64 lowercase hex digits or anything that cannot be read or
/// parsed (a leading BOM included: `JSON.parse` refuses it too).
pub fn stored_result(
    data_dir: &Path,
    project_id: &str,
    version: &str,
) -> Result<JValue, Unreadable> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    if !version_like(version) {
        return Err(Unreadable);
    }
    let path = documents_dir(data_dir, project_id).join(format!("{version}.json"));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| Unreadable)?;
    let md = file.metadata().map_err(|_| Unreadable)?;
    if !md.is_file() || md.len() > RESULT_BYTES {
        return Err(Unreadable);
    }
    let mut bytes = Vec::new();
    file.take(RESULT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Unreadable)?;
    if bytes.len() as u64 > RESULT_BYTES {
        return Err(Unreadable);
    }
    wtf16::parse(&String::from_utf8_lossy(&bytes)).map_err(|_| Unreadable)
}

/// The UTF-16 units of a [`wtf16`] string.
fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u16; 2];
    for c in s.chars() {
        match wtf16::lone_unit(c) {
            Some(u) => out.push(u),
            None => out.extend_from_slice(c.encode_utf16(&mut buf)),
        }
    }
    out
}

/// A [`wtf16`] string from UTF-16 units (a lone surrogate kept as its mapped character).
fn from_units(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .filter_map(|r| match r {
            Ok(c) => Some(c),
            Err(e) => wtf16::lone_char(e.unpaired_surrogate()),
        })
        .collect()
}

/// `readPages(workspace, projectId, file, start = 1, end = start, offset = 0, cap)`: pages `start`
/// to `end` (1 to 5 whole pages) of the stored result for `file.document.availableVersion`, as
/// `[Page n; status: reason]` blocks joined by a blank line, cut to `cap` UTF-16 units from
/// `offset`, with the document's [`notice`]. `start`, `end` and `offset` are the values the caller
/// passes (`undefined` takes the default). `load(version)` reads the stored result (see
/// [`stored_result`]); it is called only for a version that passed versionPath's check.
pub fn read_pages(
    file: &JValue,
    start: &JValue,
    end: &JValue,
    offset: &JValue,
    cap: usize,
    load: impl FnOnce(&str) -> Result<JValue, Unreadable>,
) -> Result<Pages, ReadError> {
    let start = match start {
        JValue::Undefined => &JValue::Num(1.0),
        v => v,
    };
    let end = match end {
        JValue::Undefined => start,
        v => v,
    };
    let offset = match offset {
        JValue::Undefined => &JValue::Num(0.0),
        v => v,
    };
    let range = (integer(start), integer(end), integer(offset));
    let (Some(start), Some(end), Some(offset)) = range else {
        return Err(bad_range());
    };
    if start < 1.0 || end < start || end - start >= 5.0 || offset < 0.0 {
        return Err(bad_range());
    }
    if file.is_nullish() {
        return Err(ReadError::Thrown);
    }
    let version = file.get("document").get("availableVersion");
    if !version.truthy() {
        return Err(ReadError::Status(
            422,
            "No saved readable pages; re-upload or refresh this PDF.",
        ));
    }
    let version = js_str(version)?;
    if !version_like(&version) {
        return Err(ReadError::InvalidVersion);
    }
    let out = load(&version).map_err(|_| ReadError::Unreadable)?;
    if out.is_nullish() {
        return Err(ReadError::Thrown);
    }
    if end > to_number(out.get("pages"))? {
        return Err(ReadError::Status(400, "Page range exceeds the document."));
    }
    let JValue::Arr(texts) = out.get("pageTexts") else {
        return Err(ReadError::Thrown);
    };
    let mut selected = Vec::new();
    for p in texts {
        if p.is_nullish() {
            return Err(ReadError::Thrown);
        }
        let n = p.get("number");
        if to_number(n)? >= start && to_number(n)? <= end {
            selected.push(p);
        }
    }
    if selected.len() as f64 != end - start + 1.0 {
        return Err(ReadError::Status(
            422,
            "Requested pages were not extracted because of a processing limit.",
        ));
    }
    let mut blocks = Vec::with_capacity(selected.len());
    for p in selected {
        let reason = p.get("reason");
        let body = p.get("text");
        blocks.push(format!(
            "[Page {}; {}{}]\n{}",
            js_str(p.get("number"))?,
            js_str(p.get("status"))?,
            if reason.truthy() {
                format!(": {}", js_str(reason)?)
            } else {
                String::new()
            },
            if body.truthy() {
                js_str(body)?
            } else {
                "(no native text)".into()
            }
        ));
    }
    let text = units(&blocks.join("\n\n"));
    let len = text.len() as f64;
    // `text.slice(offset, offset + cap)`: both ends clamped to the length.
    let from = offset.min(len) as usize;
    let to = (offset + cap as f64).min(len) as usize;
    let piece = text.get(from..to.max(from)).map(from_units).unwrap_or_default();
    let next_offset = (offset + (cap as f64) < len).then_some(offset + cap as f64);
    Ok(Pages {
        text: piece,
        next_offset,
        notice: notice(file)?,
        start_page: start,
        end_page: end,
    })
}

fn bad_range() -> ReadError {
    ReadError::Status(
        400,
        "Choose 1–5 pages (startPage/endPage) and a non-negative character offset.",
    )
}
