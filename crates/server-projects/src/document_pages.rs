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
//! The reading and the notice are crates/document-read's (one copy with the chat's read tool, track
//! R13); it opens the stored result without following a symlink (as the round-1 reads do) and reads
//! only up to [`document_read::RESULT_BYTES`]; past that it is the 422 above.
use crate::js;
use crate::{Ctx, Reply};
use js_json::{wtf16, JValue};

/// JS `StringToNumber` (what `Number(string)` does): document-read's, also for source_limits.rs and
/// folder_import.rs.
pub use document_read::string_to_number;

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
    let read = document_read::read_pages(
        file,
        &JValue::Num(start),
        &JValue::Num(end),
        &JValue::Num(offset),
        document_read::READ_CAP,
        |version| document_read::stored_result(&dir, &id, version),
    );
    match read.map(|pages| wtf16::stringify(&pages.to_json())) {
        Ok(Some(text)) => Reply::json(200, text),
        Err(document_read::ReadError::Status(status, message)) => Reply::error(status, message),
        Ok(None) | Err(_) => Reply::error(422, document_read::UNAVAILABLE),
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
}
