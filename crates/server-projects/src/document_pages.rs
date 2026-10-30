//! `GET /api/projects/{id}/documents/pages` (sbstndalton/noevia#2191 round 4, group
//! `rust-project-document-pages`): core routes/projects.cjs `docRead` with document-sources.cjs
//! `readPages`, `notice`, `problem` and `notes`, answer for answer, from the extraction results the
//! upload pipeline stored (`project-documents/<sha256 id>/<version>.json`).
//!
//! In Node's order: the id percent-decoded (a bad escape is Node's URIError, 500); getProject (its
//! self-heal saved); the file named `name` (the first `name` query value) with a `document`, else
//! 404 `no such document`; `startPage` (default 1), `endPage` (default the start) and `offset`
//! (default 0) as JS `Number(...)`; 1 to 5 whole pages and a whole non-negative offset, else 400;
//! no `availableVersion` 422; the stored result past the requested range 400; a page the
//! extraction did not keep 422; any other failure reading or using the stored result (missing,
//! unreadable, not JSON, an invalid version, a shape the JS would throw on) Node's 422 `Saved
//! document data is unavailable; upload or refresh again.` The text is the selected pages joined
//! as Node joins them, cut at the UTF-16 `offset` to 8 000 units with `nextOffset`, and the
//! document's `notice` (its quoted name, state, page outcomes and notes, versions, index state).
//!
//! Stricter than Node: the stored result is opened without following a symlink (as the round-1
//! reads do) and read only up to [`RESULT_BYTES`]; past that it is the 422 above.
use crate::js;
use crate::{Ctx, Reply};
use js_json::{wtf16, JValue};
use std::path::Path;

/// The largest stored extraction result this reads (a 20 000-page document's text is far below).
pub const RESULT_BYTES: u64 = 256 * 1024 * 1024;
const CAP: usize = 8000;
const UNAVAILABLE: &str = "Saved document data is unavailable; upload or refresh again.";
/// documents.cjs RETRY_PAGE_CAP.
const RETRY_PAGE_CAP: u32 = 20;

/// A failure inside `readPages`: one with a status keeps its message, the rest are Node's 422.
enum Fail {
    Status(u16, &'static str),
    Unavailable,
}

/// JS `StringToNumber` (what `Number(string)` does).
pub fn string_to_number(raw: &str) -> f64 {
    let s = js::trim(raw);
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
    s.parse::<f64>().unwrap_or(f64::NAN)
}

/// JS `ToNumber` of a JSON value (an array or object is NaN here; a stored result never holds one
/// where a number is compared).
fn to_number(v: &JValue) -> f64 {
    match v {
        JValue::Num(n) => *n,
        JValue::Str(s) => string_to_number(s),
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Null => 0.0,
        _ => f64::NAN,
    }
}

fn is_integer(n: f64) -> bool {
    n.is_finite() && n.trunc() == n
}

/// JS `String(v)` / a template literal's `${v}`.
fn text_of(v: &JValue) -> String {
    match v {
        JValue::Undefined => "undefined".into(),
        other => crate::files::js_string(other),
    }
}

/// quoted-name.cjs `quoteName(name)`.
pub fn quote_name(name: &str) -> String {
    let mut out = String::new();
    js_json::quote(name, &mut out);
    let mut escaped = String::with_capacity(out.len());
    for c in out.chars() {
        let u = u32::from(c);
        if u == 0x85
            || u == 0x200e
            || u == 0x200f
            || u == 0x2028
            || u == 0x2029
            || (0x202a..=0x202e).contains(&u)
            || (0x2066..=0x2069).contains(&u)
        {
            escaped.push_str(&format!("\\u{u:04x}"));
        } else {
            escaped.push(c);
        }
    }
    escaped
}

/// document-sources.cjs `pageList(pages)`.
fn page_list(pages: &[&JValue]) -> String {
    let mut out = pages
        .iter()
        .take(20)
        .map(|p| {
            let n = p.get("number");
            if n.is_nullish() {
                String::new()
            } else {
                text_of(n)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    if pages.len() > 20 {
        out.push('…');
    }
    out
}

/// document-sources.cjs `notes(file)` as `noteText` words them. `Err` where the JS would throw.
fn note_texts(d: &JValue) -> Result<Vec<String>, ()> {
    let mut out = Vec::new();
    let stopped = d.get("stoppedAt");
    if stopped.truthy() {
        let limit = stopped.get("limit");
        let words = match limit.as_str() {
            Some("pages") => "page limit".to_string(),
            Some("text") => "text limit".to_string(),
            Some("error") => "read error".to_string(),
            _ => text_of(limit),
        };
        let error = stopped.get("error");
        let reason = if error.truthy() {
            format!(": {}", text_of(error))
        } else {
            String::new()
        };
        out.push(format!(
            "stopped at page {} ({words}{reason})",
            text_of(stopped.get("page"))
        ));
    }
    let searchable = d.get("searchable");
    if searchable.truthy()
        && searchable.get("version") == d.get("availableVersion")
        && searchable.get("complete") == &JValue::Bool(false)
        && matches!(searchable.get("page"), JValue::Num(n) if is_integer(*n))
    {
        out.push(format!(
            "searchable up to page {} so far; later pages are still being indexed",
            text_of(searchable.get("page"))
        ));
    }
    let pages: Vec<&JValue> = match d.get("pageStatus") {
        JValue::Arr(a) => a.iter().collect(),
        _ => Vec::new(),
    };
    if pages.iter().any(|p| p.is_nullish()) {
        return Err(());
    }
    let by = |outcome: &str| -> Vec<&JValue> {
        pages
            .iter()
            .copied()
            .filter(|p| p.get("retry").get("outcome").as_str() == Some(outcome))
            .collect()
    };
    let mut failed: Vec<(String, Vec<&JValue>)> = Vec::new();
    for p in by("failed") {
        let e = p.get("retry").get("error");
        let reason = if e.truthy() {
            text_of(e)
        } else {
            "unknown".into()
        };
        match failed.iter_mut().find(|(r, _)| *r == reason) {
            Some((_, list)) => list.push(p),
            None => failed.push((reason, vec![p])),
        }
    }
    for (reason, list) in failed {
        out.push(format!(
            "pages {}: OCR retry failed: {reason}",
            page_list(&list)
        ));
    }
    for (outcome, words) in [
        ("empty", "OCR retry found no text".to_string()),
        (
            "unavailable",
            "OCR retry unavailable (no OCR service is configured)".to_string(),
        ),
        (
            "skipped",
            format!("not retried (OCR retry limit of {RETRY_PAGE_CAP} pages)"),
        ),
        (
            "recovered",
            "recovered by an OCR retry; verify numbers against the original".to_string(),
        ),
    ] {
        let list = by(outcome);
        if !list.is_empty() {
            out.push(format!("pages {}: {words}", page_list(&list)));
        }
    }
    Ok(out)
}

/// document-sources.cjs `notes(file)` as the ids and parameters the browser words in its language
/// (`[{ id, params }]`), for a skipped source's `notes`.
pub(crate) fn notes_json(d: &JValue) -> Result<Vec<JValue>, ()> {
    let mut out = Vec::new();
    let stopped = d.get("stoppedAt");
    if stopped.truthy() {
        let mut params = vec![
            ("page".to_owned(), stopped.get("page").clone()),
            ("limit".to_owned(), stopped.get("limit").clone()),
        ];
        if stopped.get("error").truthy() {
            params.push(("reason".to_owned(), stopped.get("error").clone()));
        }
        out.push(JValue::obj([
            ("id", JValue::Str("stoppedAtPage".into())),
            ("params", JValue::Obj(params)),
        ]));
    }
    let searchable = d.get("searchable");
    if searchable.truthy()
        && searchable.get("version") == d.get("availableVersion")
        && searchable.get("complete") == &JValue::Bool(false)
        && matches!(searchable.get("page"), JValue::Num(n) if is_integer(*n))
    {
        out.push(JValue::obj([
            ("id", JValue::Str("searchableToPage".into())),
            (
                "params",
                JValue::obj([("page", searchable.get("page").clone())]),
            ),
        ]));
    }
    let pages: Vec<&JValue> = match d.get("pageStatus") {
        JValue::Arr(a) => a.iter().collect(),
        _ => Vec::new(),
    };
    if pages.iter().any(|p| p.is_nullish()) {
        return Err(());
    }
    let by = |outcome: &str| -> Vec<&JValue> {
        pages
            .iter()
            .copied()
            .filter(|p| p.get("retry").get("outcome").as_str() == Some(outcome))
            .collect()
    };
    let mut failed: Vec<(String, Vec<&JValue>)> = Vec::new();
    for p in by("failed") {
        let e = p.get("retry").get("error");
        let reason = if e.truthy() {
            text_of(e)
        } else {
            "unknown".into()
        };
        match failed.iter_mut().find(|(r, _)| *r == reason) {
            Some((_, list)) => list.push(p),
            None => failed.push((reason, vec![p])),
        }
    }
    for (reason, list) in failed {
        out.push(JValue::obj([
            ("id", JValue::Str("ocrRetryFailed".into())),
            (
                "params",
                JValue::obj([
                    ("pages", JValue::Str(page_list(&list))),
                    ("reason", JValue::Str(reason)),
                ]),
            ),
        ]));
    }
    for (outcome, id) in [
        ("empty", "ocrRetryEmpty"),
        ("unavailable", "ocrRetryUnavailable"),
        ("skipped", "ocrRetryLimit"),
        ("recovered", "ocrRetryRecovered"),
    ] {
        let list = by(outcome);
        if !list.is_empty() {
            out.push(JValue::obj([
                ("id", JValue::Str(id.into())),
                (
                    "params",
                    JValue::obj([("pages", JValue::Str(page_list(&list)))]),
                ),
            ]));
        }
    }
    Ok(out)
}

/// document-sources.cjs `problem(file)`.
pub(crate) fn problem(d: &JValue) -> Result<String, ()> {
    problem_with(d, true)
}

/// `problem(file, { notes })`.
pub(crate) fn problem_with(d: &JValue, with_notes: bool) -> Result<String, ()> {
    let pages: Vec<&JValue> = match d.get("pageStatus") {
        JValue::Arr(a) => a.iter().collect(),
        v if !v.truthy() => Vec::new(),
        _ => return Err(()),
    };
    if pages.iter().any(|p| p.is_nullish()) {
        return Err(());
    }
    let issues: Vec<&JValue> = pages
        .into_iter()
        .filter(|p| !matches!(p.get("status").as_str(), Some("native" | "blank" | "ocr")))
        .collect();
    let extra = if with_notes {
        note_texts(d)?
    } else {
        Vec::new()
    };
    let mut s = text_of(d.get("state"));
    if d.get("stale").truthy() {
        s.push_str("; using STALE text from the last readable version");
    }
    if d.get("pages").truthy() {
        s.push_str(&format!("; {} pages", text_of(d.get("pages"))));
    }
    if d.get("truncated").truthy() {
        s.push_str("; summary/page extraction limits reached");
    }
    if !issues.is_empty() {
        let listed = issues
            .iter()
            .take(20)
            .map(|p| {
                let reason = p.get("reason");
                format!(
                    "{} ({}{})",
                    text_of(p.get("number")),
                    text_of(p.get("status")),
                    if reason.truthy() {
                        format!(": {}", text_of(reason))
                    } else {
                        String::new()
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        s.push_str(&format!(
            "; incomplete pages {listed}{}",
            if issues.len() > 20 { "…" } else { "" }
        ));
    }
    let extra: Vec<String> = extra.into_iter().filter(|e| !e.is_empty()).collect();
    if !extra.is_empty() {
        let joined = extra.join("; ");
        s.push_str("; ");
        s.push_str(joined.strip_suffix('.').unwrap_or(&joined));
    }
    // `${d.error ? '; ' + d.error : ''}`.replace(/\.$/, '') + '.'
    let error = d.get("error");
    if error.truthy() {
        let tail = format!("; {}", text_of(error));
        s.push_str(tail.strip_suffix('.').unwrap_or(&tail));
    }
    s.push('.');
    Ok(s)
}

/// document-sources.cjs `notice(file)`.
fn notice(file: &JValue) -> Result<String, ()> {
    let d = file.get("document");
    let mut s = format!(
        "{}: {}",
        quote_name(&text_of(file.get("name"))),
        problem(d)?
    );
    if d.get("version").truthy() {
        s.push_str(&format!(" Source version {}", text_of(d.get("version"))));
    }
    if d.get("availableVersion").truthy() {
        s.push_str(&format!(
            "; text version {}",
            text_of(d.get("availableVersion"))
        ));
    }
    let indexing = d.get("indexing");
    s.push_str(&format!(
        "; index: {}.",
        if indexing.truthy() {
            text_of(indexing)
        } else {
            "unavailable".into()
        }
    ));
    Ok(s)
}

fn read_result(dir: &Path, project_id: &str, version: &str) -> Result<JValue, ()> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    if version.len() != 64
        || !version
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(());
    }
    let path = crate::lifecycle::documents_dir(dir, project_id).join(format!("{version}.json"));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| ())?;
    let md = file.metadata().map_err(|_| ())?;
    if !md.is_file() || md.len() > RESULT_BYTES {
        return Err(());
    }
    let mut bytes = Vec::new();
    file.take(RESULT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    // readFileSync(.., 'utf8') keeps a leading BOM and JSON.parse refuses it: so does this.
    wtf16::parse(&String::from_utf8_lossy(&bytes)).map_err(|_| ())
}

/// document-sources.cjs `readPages(workspace, projectId, file, start, end, offset)`.
fn read_pages(
    dir: &Path,
    id: &str,
    file: &JValue,
    start: f64,
    end: f64,
    offset: f64,
) -> Result<String, Fail> {
    if !is_integer(start)
        || !is_integer(end)
        || start < 1.0
        || end < start
        || end - start >= 5.0
        || !is_integer(offset)
        || offset < 0.0
    {
        return Err(Fail::Status(
            400,
            "Choose 1–5 pages (startPage/endPage) and a non-negative character offset.",
        ));
    }
    let version = file.get("document").get("availableVersion");
    if !version.truthy() {
        return Err(Fail::Status(
            422,
            "No saved readable pages; re-upload or refresh this PDF.",
        ));
    }
    let Some(version) = version.as_str() else {
        return Err(Fail::Unavailable);
    };
    let out = read_result(dir, id, version).map_err(|_| Fail::Unavailable)?;
    if out.is_nullish() {
        return Err(Fail::Unavailable);
    }
    // `end > out.pages`: a missing count compares false, as in JS.
    let pages = out.get("pages");
    if !matches!(pages, JValue::Undefined) && end > to_number(pages) {
        return Err(Fail::Status(400, "Page range exceeds the document."));
    }
    let JValue::Arr(texts) = out.get("pageTexts") else {
        return Err(Fail::Unavailable);
    };
    let mut selected = Vec::new();
    for p in texts {
        if p.is_nullish() {
            return Err(Fail::Unavailable);
        }
        let n = to_number(p.get("number"));
        if n >= start && n <= end {
            selected.push(p);
        }
    }
    if selected.len() as f64 != end - start + 1.0 {
        return Err(Fail::Status(
            422,
            "Requested pages were not extracted because of a processing limit.",
        ));
    }
    let text = selected
        .iter()
        .map(|p| {
            let reason = p.get("reason");
            let body = p.get("text");
            format!(
                "[Page {}; {}{}]\n{}",
                text_of(p.get("number")),
                text_of(p.get("status")),
                if reason.truthy() {
                    format!(": {}", text_of(reason))
                } else {
                    String::new()
                },
                if body.truthy() {
                    text_of(body)
                } else {
                    "(no native text)".into()
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let notice = notice(file).map_err(|_| Fail::Unavailable)?;
    let units = wtf16::len(&text);
    // Whole numbers below 2^53 here (a UTF-16 length, a checked offset).
    let off = offset as usize;
    let from = off.min(units);
    let to = off.saturating_add(CAP).min(units);
    let piece = slice_range(&text, from, to);
    let next = if (offset + CAP as f64) < units as f64 {
        JValue::Num(offset + CAP as f64)
    } else {
        JValue::Null
    };
    wtf16::stringify(&JValue::obj([
        ("text", JValue::Str(piece)),
        ("nextOffset", next),
        ("notice", JValue::Str(notice)),
        ("startPage", JValue::Num(start)),
        ("endPage", JValue::Num(end)),
    ]))
    .ok_or(Fail::Unavailable)
}

/// `s.slice(from, to)` in UTF-16 units: a surrogate pair cut at either end keeps its half, as in JS.
pub fn slice_range(s: &str, from: usize, to: usize) -> String {
    let mut out = String::new();
    let mut at = 0usize;
    for c in s.chars() {
        if at >= to {
            break;
        }
        let lone = wtf16::lone_unit(c).is_some();
        let mut buf = [0u16; 2];
        let units: &[u16] = if lone { &[] } else { c.encode_utf16(&mut buf) };
        if lone || units.len() == 1 {
            if at >= from {
                out.push(c);
            }
            at += 1;
            continue;
        }
        for &u in units {
            if at >= from && at < to {
                if let Some(m) = wtf16::lone_char(u) {
                    out.push(m);
                }
            }
            at += 1;
        }
        // A whole pair inside the range is the character itself, not two lone halves.
        if at - 2 >= from && at <= to {
            for _ in 0..2 {
                out.pop();
            }
            out.push(c);
        }
    }
    out
}

/// `url.searchParams.get(key)`: the first value, form-decoded.
fn param(query: Option<&str>, key: &str) -> Option<String> {
    url::form_urlencoded::parse(query.unwrap_or("").as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// GET /api/projects/{id}/documents/pages; `raw` is the id segment as the URL spelled it.
pub fn get(ctx: Ctx<'_>, raw: &str, query: Option<&str>) -> Reply {
    let Some(id) = js::decode_uri_component(raw) else {
        return Reply::internal();
    };
    let (dir, project) = match crate::originals::project(ctx, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let name = param(query, "name");
    let file = project.as_ref().and_then(|p| match p.get("files") {
        JValue::Arr(files) => files.iter().find(|f| match (&name, f.get("name")) {
            (Some(n), JValue::Str(s)) => s == n,
            (None, JValue::Null) => true,
            _ => false,
        }),
        _ => None,
    });
    let Some(file) = file.filter(|f| f.get("document").truthy()) else {
        return Reply::error(404, "no such document");
    };
    let number = |key: &str, default: f64| match param(query, key).filter(|s| !s.is_empty()) {
        Some(s) => string_to_number(&s),
        None => default,
    };
    let start = number("startPage", 1.0);
    let end = number("endPage", start);
    let offset = number("offset", 0.0);
    match read_pages(&dir, &id, file, start, end, offset) {
        Ok(text) => Reply::json(200, text),
        Err(Fail::Status(status, message)) => Reply::error(status, message),
        Err(Fail::Unavailable) => Reply::error(422, UNAVAILABLE),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn number_is_js_number() {
        for (s, n) in [
            ("", 0.0),
            (" 2 ", 2.0),
            ("1e0", 1.0),
            ("0x10", 16.0),
            ("+3", 3.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("-0", -0.0),
        ] {
            assert_eq!(string_to_number(s), n, "{s:?}");
        }
        for s in [
            "abc", "1px", "0x", "e5", "1e", "inf", "NaN", "--1", "0x-1", "1_0",
        ] {
            assert!(string_to_number(s).is_nan(), "{s:?}");
        }
        assert_eq!(string_to_number("Infinity"), f64::INFINITY);
    }

    #[test]
    fn slices_by_utf16_units_as_js() {
        let s = "a\u{1F600}b";
        assert_eq!(slice_range(s, 0, 4), s);
        assert_eq!(slice_range(s, 1, 3), "\u{1F600}");
        assert_eq!(wtf16::len(&slice_range(s, 0, 2)), 2);
        assert_eq!(wtf16::len(&slice_range(s, 2, 4)), 2);
        assert_eq!(slice_range(s, 3, 9), "b");
        assert_eq!(slice_range(s, 9, 12), "");
    }

    #[test]
    fn names_are_quoted_as_quoted_name_cjs_does() {
        assert_eq!(quote_name("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_name("x\u{2028}y\u{202e}z"), "\"x\\u2028y\\u202ez\"");
    }

    #[test]
    fn problem_and_notes_read_as_node_words_them() {
        let d = wtf16::parse(r#"{"state":"partial","stale":true,"pages":3,"truncated":true,
            "pageStatus":[{"number":1,"status":"native"},{"number":2,"status":"failed","reason":"bad.","retry":{"outcome":"failed","error":"busy"}},
                          {"number":3,"status":"empty","retry":{"outcome":"recovered"}}],
            "stoppedAt":{"page":3,"limit":"pages"},"error":"x."}"#).unwrap();
        assert_eq!(
            problem(&d).unwrap(),
            "partial; using STALE text from the last readable version; 3 pages; summary/page extraction limits reached; incomplete pages 2 (failed: bad.), 3 (empty); stopped at page 3 (page limit); pages 2: OCR retry failed: busy; pages 3: recovered by an OCR retry; verify numbers against the original; x."
        );
        let bare = wtf16::parse(r#"{"state":"ready"}"#).unwrap();
        assert_eq!(problem(&bare).unwrap(), "ready.");
        assert!(problem(&wtf16::parse(r#"{"state":"x","pageStatus":{"a":1}}"#).unwrap()).is_err());
    }
}
