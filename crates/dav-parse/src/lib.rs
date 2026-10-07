//! Bounded WebDAV PROPFIND (`Depth: 1`) listing parser (noevia#967, strangler slice 4).
//!
//! A multistatus body comes from a storage server the user (or an administrator) configured, so
//! every byte is untrusted. This crate is a byte-for-byte port of the JS reference in noevia-core
//! (`server/dav-listing.cjs`, `listingEntriesJs`): it finds each `<[p:]response>` block, takes the
//! first `<[p:]href>` in it, decodes the five predefined XML entities and numeric character
//! references (single pass, never recursive), resolves the href against the request URL with the
//! WHATWG URL rules, percent-decodes the path like `decodeURIComponent`, and keeps only direct
//! children of the requested directory. Hrefs outside it (other folders, `..` traversal that
//! resolves elsewhere, encoded slashes that would add a level) are dropped, exactly like the JS.
//!
//! It is not an XML parser and never becomes one: there is no DTD, no entity declaration, no
//! external resource and no recursion, so XXE and entity expansion ("billion laughs") have nothing
//! to act on. Work is linear in the input. Inputs past the caps below are refused with a typed
//! [`Error`]; the caller (noevia-core's `DAV_PARSE_IMPL=wasm` path) fails closed on any error.
#![forbid(unsafe_code)]

use std::fmt;

/// Largest body accepted, in UTF-8 bytes. noevia-core reads at most 4 MiB of a listing; a body of
/// invalid UTF-8 can grow up to 3x when the decoder substitutes U+FFFD, so 16 MiB covers every
/// body the server can hand over.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Largest request URL accepted, in bytes.
pub const MAX_TARGET_BYTES: usize = 8 * 1024;
/// Most `<response>` blocks scanned in one listing.
pub const MAX_RESPONSES: usize = 100_000;

/// Why a listing was refused. Inside the caps the parser never fails on a hostile body (it drops
/// what it cannot use, as the JS does); only these input-contract violations are errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The body is larger than [`MAX_BODY_BYTES`].
    BodyTooLarge { len: usize, max: usize },
    /// The request URL is larger than [`MAX_TARGET_BYTES`].
    TargetTooLong { len: usize, max: usize },
    /// The request URL does not parse, or its path does not percent-decode (the JS throws here).
    InvalidTarget,
    /// More than [`MAX_RESPONSES`] `<response>` blocks.
    TooManyResponses { max: usize },
}

impl Error {
    /// A stable machine-readable code, used across the WebAssembly boundary.
    pub fn code(&self) -> &'static str {
        match self {
            Error::BodyTooLarge { .. } => "body_too_large",
            Error::TargetTooLong { .. } => "target_too_long",
            Error::InvalidTarget => "invalid_target",
            Error::TooManyResponses { .. } => "too_many_responses",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BodyTooLarge { len, max } => {
                write!(f, "listing body is {len} bytes (max {max})")
            }
            Error::TargetTooLong { len, max } => {
                write!(f, "request URL is {len} bytes (max {max})")
            }
            Error::InvalidTarget => write!(f, "request URL is not a valid URL"),
            Error::TooManyResponses { max } => write!(f, "listing has more than {max} responses"),
        }
    }
}

impl std::error::Error for Error {}

/// One direct child of the listed directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The decoded child name (never empty, never contains `/`).
    pub name: String,
    /// A `<[p:]collection>` element appears in the response block.
    pub is_dir: bool,
    /// The ASCII digits of the first `<[p:]getcontentlength>` that holds only digits, unparsed:
    /// the JS turns them into a Number, and so does the WebAssembly caller.
    pub size: Option<String>,
}

/// JS `WhiteSpace` + `LineTerminator`: the set `String.prototype.trim` strips and `\s` matches.
pub fn is_js_whitespace(c: char) -> bool {
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

/// `String.prototype.trim`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

fn is_tag_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// At `i` (a `<`), the index just past `<[prefix:]name>` (or `</[prefix:]name>` when `closing`).
fn tag_end_at(b: &[u8], i: usize, name: &[u8], closing: bool) -> Option<usize> {
    let mut j = i + 1;
    if closing {
        if b.get(j) != Some(&b'/') {
            return None;
        }
        j += 1;
    }
    let mut k = j;
    while b.get(k).is_some_and(|&c| is_tag_name_byte(c)) {
        k += 1;
    }
    if k > j && b.get(k) == Some(&b':') {
        j = k + 1;
    }
    let end = j + name.len();
    if b.get(j..end) == Some(name) && b.get(end) == Some(&b'>') {
        Some(end + 1)
    } else {
        None
    }
}

fn find_from(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let hay = b.get(from..)?;
    hay.windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// The text inside each `<[p:]name>…</[p:]name>`, in order, at most `limit` of them: the nearest
/// closing tag wins and any prefix is accepted on either side. Linear in `body`.
pub fn element_texts<'a>(body: &'a str, name: &str, limit: usize) -> Vec<&'a str> {
    let b = body.as_bytes();
    let n = name.as_bytes();
    let mut out = Vec::new();
    let mut pos = 0;
    while out.len() < limit {
        let mut start = None;
        let mut i = find_from(b, pos, b"<");
        while let Some(at) = i {
            if let Some(end) = tag_end_at(b, at, n, false) {
                start = Some(end);
                break;
            }
            i = find_from(b, at + 1, b"<");
        }
        let Some(start) = start else { break };
        let mut found = None;
        let mut i = find_from(b, start, b"</");
        while let Some(at) = i {
            if let Some(end) = tag_end_at(b, at, n, true) {
                found = Some((at, end));
                break;
            }
            i = find_from(b, at + 2, b"</");
        }
        let Some((close, after)) = found else { break };
        // Both ends sit on ASCII bytes, so the slice is on char boundaries.
        if let Some(text) = body.get(start..close) {
            out.push(text);
        }
        pos = after;
    }
    out
}

fn run_len(b: &[u8], from: usize, pred: impl Fn(u8) -> bool) -> usize {
    b.get(from..)
        .map_or(0, |rest| rest.iter().take_while(|&&c| pred(c)).count())
}

/// Code point of a run of digits in `radix`, if it is a valid, non-NUL, non-surrogate scalar.
fn scalar_from_digits(digits: &str, radix: u32) -> Option<char> {
    let trimmed = digits.trim_start_matches('0');
    if trimmed.len() > 8 {
        return None;
    }
    let code = if trimmed.is_empty() {
        0
    } else {
        u32::from_str_radix(trimmed, radix).ok()?
    };
    if code == 0 {
        return None;
    }
    char::from_u32(code)
}

/// Decode `&amp; &lt; &gt; &quot; &apos;` and `&#N;` / `&#xH;` in one left-to-right pass. Unknown
/// names and invalid code points (NUL, surrogates, past U+10FFFF) are left as written. The output
/// is never rescanned, so nothing can expand recursively.
pub fn decode_xml_entities(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while let Some(amp) = find_from(b, i, b"&") {
        let p = amp + 1;
        let (body_len, replacement) = if b.get(p..p + 2) == Some(b"#x") {
            let h = run_len(b, p + 2, |c| c.is_ascii_hexdigit());
            if h > 0 && b.get(p + 2 + h) == Some(&b';') {
                let digits = s.get(p + 2..p + 2 + h).unwrap_or("");
                (Some(2 + h), scalar_from_digits(digits, 16))
            } else {
                (None, None)
            }
        } else if b.get(p) == Some(&b'#') {
            let d = run_len(b, p + 1, |c| c.is_ascii_digit());
            if d > 0 && b.get(p + 1 + d) == Some(&b';') {
                let digits = s.get(p + 1..p + 1 + d).unwrap_or("");
                (Some(1 + d), scalar_from_digits(digits, 10))
            } else {
                (None, None)
            }
        } else {
            let l = run_len(b, p, |c| c.is_ascii_alphabetic());
            if l > 0 && b.get(p + l) == Some(&b';') {
                let r = match s.get(p..p + l).unwrap_or("") {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => None,
                };
                (Some(l), r)
            } else {
                (None, None)
            }
        };
        match body_len {
            Some(len) => {
                let end = p + len + 1; // past the ';'
                if let Some(c) = replacement {
                    out.push_str(s.get(copied..amp).unwrap_or(""));
                    out.push(c);
                    copied = end;
                }
                i = end;
            }
            None => i = p,
        }
    }
    out.push_str(s.get(copied..).unwrap_or(""));
    out
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `decodeURIComponent`: `None` where it would throw (a `%` without two hex digits, or bytes that
/// are not well-formed UTF-8, overlong forms and surrogates included).
pub fn decode_uri_component(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        if c == b'%' {
            let hi = hex_val(*b.get(i + 1)?)?;
            let lo = hex_val(*b.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(c);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `decodeURIComponent(url.pathname).replace(/\/+$/, '')`.
fn decoded_dir(url: &url::Url) -> Option<String> {
    let decoded = decode_uri_component(url.path())?;
    Some(decoded.trim_end_matches('/').to_owned())
}

/// At the first `<` where `<[alnum+:]name` starts and `rest` accepts what follows, `rest`'s value.
fn first_tag<'a, T>(block: &'a str, name: &str, rest: impl Fn(&'a str) -> Option<T>) -> Option<T> {
    let b = block.as_bytes();
    let mut i = find_from(b, 0, b"<");
    while let Some(at) = i {
        let j = at + 1;
        let k = j + run_len(b, j, is_tag_name_byte);
        // `(?:[a-zA-Z0-9]+:)?` tries the prefix first, then no prefix.
        let mut starts = Vec::with_capacity(2);
        if k > j && b.get(k) == Some(&b':') {
            starts.push(k + 1);
        }
        starts.push(j);
        for s in starts {
            let end = s + name.len();
            if b.get(s..end) == Some(name.as_bytes()) {
                if let Some(r) = block.get(end..).and_then(&rest) {
                    return Some(r);
                }
            }
        }
        i = find_from(b, at + 1, b"<");
    }
    None
}

/// `/<(?:[a-zA-Z0-9]+:)?collection\s*\/?>/`
fn is_collection(block: &str) -> bool {
    first_tag(block, "collection", |after: &str| {
        let after = after.trim_start_matches(is_js_whitespace);
        (after.starts_with('>') || after.starts_with("/>")).then_some(())
    })
    .is_some()
}

/// `/<(?:[a-zA-Z0-9]+:)?getcontentlength>(\d+)</`
fn content_length(block: &str) -> Option<String> {
    first_tag(block, "getcontentlength", |after: &str| {
        let rest = after.strip_prefix('>')?;
        let d = run_len(rest.as_bytes(), 0, |c| c.is_ascii_digit());
        if d > 0 && rest.as_bytes().get(d) == Some(&b'<') {
            rest.get(..d).map(str::to_owned)
        } else {
            None
        }
    })
}

/// Parse a PROPFIND `Depth: 1` reply for the directory at `target` (the request URL) into its
/// direct children, in body order. Matches noevia-core's `listingEntriesJs(body, target)`.
pub fn list_entries(body: &str, target: &str) -> Result<Vec<Entry>, Error> {
    if body.len() > MAX_BODY_BYTES {
        return Err(Error::BodyTooLarge {
            len: body.len(),
            max: MAX_BODY_BYTES,
        });
    }
    if target.len() > MAX_TARGET_BYTES {
        return Err(Error::TargetTooLong {
            len: target.len(),
            max: MAX_TARGET_BYTES,
        });
    }
    let base = url::Url::parse(target).map_err(|_| Error::InvalidTarget)?;
    let request_dir = decoded_dir(&base).ok_or(Error::InvalidTarget)?;
    let blocks = element_texts(body, "response", MAX_RESPONSES + 1);
    if blocks.len() > MAX_RESPONSES {
        return Err(Error::TooManyResponses { max: MAX_RESPONSES });
    }
    let mut entries = Vec::new();
    for block in blocks {
        let Some(href_text) = element_texts(block, "href", 1).into_iter().next() else {
            continue;
        };
        let decoded = decode_xml_entities(href_text);
        let Ok(resolved) = base.join(js_trim(&decoded)) else {
            continue;
        };
        let Some(href) = decoded_dir(&resolved) else {
            continue;
        };
        // A foreign href (not under the browsed directory) is dropped.
        let Some(rest) = href.strip_prefix(request_dir.as_str()) else {
            continue;
        };
        let Some(relative) = rest.strip_prefix('/') else {
            continue;
        };
        if relative.is_empty() || relative.contains('/') {
            continue; // the directory itself, or not a direct child
        }
        entries.push(Entry {
            name: relative.to_owned(),
            is_dir: is_collection(block),
            size: content_length(block),
        });
    }
    Ok(entries)
}

fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The WebAssembly reply: `{"entries":[{"name":…,"isDir":…,"size":"digits"|null},…]}` or
/// `{"error":"code"}`.
pub fn reply_json(result: &Result<Vec<Entry>, Error>) -> String {
    let mut out = String::new();
    match result {
        Err(e) => {
            out.push_str("{\"error\":");
            push_json_string(&mut out, e.code());
            out.push('}');
        }
        Ok(entries) => {
            out.push_str("{\"entries\":[");
            for (n, e) in entries.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                out.push_str("{\"name\":");
                push_json_string(&mut out, &e.name);
                out.push_str(if e.is_dir {
                    ",\"isDir\":true,\"size\":"
                } else {
                    ",\"isDir\":false,\"size\":"
                });
                match &e.size {
                    Some(d) => push_json_string(&mut out, d),
                    None => out.push_str("null"),
                }
                out.push('}');
            }
            out.push_str("]}");
        }
    }
    out
}
