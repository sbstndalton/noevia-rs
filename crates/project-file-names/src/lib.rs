//! noevia-core's `server/project-file-names.cjs` in Rust, exported from `dav-parse.wasm`
//! (PROJECT_FILE_NAMES_IMPL). One resolver for every tool that reads or edits a project file by
//! the name a model gave (#642): the stored name verbatim, or a trailing part of it that ends on a
//! folder boundary, matching exactly one file of the requesting account's own project. Names that
//! look like an escape attempt (`..` or `.` segments, an absolute or drive path, backslashes,
//! control characters, percent-encoded dots/separators) are refused before any matching.
//!
//! The host sends only the stored names of the project's files (the list the JS already filtered
//! to string names, in order) and the raw argument; the reply is the index of the one file, or why
//! not: [`invalid_reason`]'s exact text, `missing`, or `ambiguous` with every candidate's index.
//!
//! # Normalization without Unicode tables
//!
//! The JS compares `normalize('NFC')` forms. The port carries no normalization data. It knows a
//! set of NFC-inert code points ([`NFC_INERT_RANGES`]): each is its own NFC form, has canonical
//! combining class 0, and neither it nor the start of its decomposition ever appears after the
//! first position of a canonical decomposition, so NFC never changes across the start of a run of
//! them (noevia-core's differential test checks every one against the runtime's own ICU). The
//! wanted name must be inert throughout. A stored name is split into a head and its longest inert
//! tail, and `NFC(name) = NFC(head) + tail` (noevia#1203):
//!
//! - exact match: a fully inert name is compared as is; otherwise it cannot equal the wanted name
//!   unless its tail is a suffix of it, and only then does the port refuse (`ambiguous`);
//! - suffix match: decided by the tail when it is at least as long as `'/' + wanted`; otherwise
//!   refused only when the tail is a suffix of `'/' + wanted`.
//!
//! So one stored name with, say, Greek in a folder name no longer blocks the whole project. Besides
//! those refusals the port is stricter than the JS only for requests over [`MAX_INPUT_BYTES`]
//! (`too_large`).
//!
//! Linear in the input (each name is compared once for equality and once as a suffix), no panics.

#![forbid(unsafe_code)]

use prompt_framing::js::trim;
use prompt_framing::json::{self, Value};

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024 + 1;
/// `MAX_NAME`: UTF-16 code units of the trimmed name.
pub const MAX_NAME: usize = 1024;

/// Why the port gives no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Input,
    TooLarge,
    /// A name outside the NFC-inert set the port reads without normalization tables.
    Ambiguous,
}

impl Refusal {
    /// The refusal reply.
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
            Refusal::Ambiguous => r#"{"error":"ambiguous"}"#,
        }
    }
}

/// The NFC-inert code points (see the crate docs), as inclusive ranges: U+0000–U+02FF, Cyrillic
/// U+0400–U+0482 and U+048A–U+04FF, punctuation U+2010–U+2027 and U+2030–U+205E, CJK symbols
/// U+3001–U+3029, Hiragana U+3041–U+3096, Katakana U+30A1–U+30FC, CJK Unified Ideographs
/// U+4E00–U+9FFF, Hangul syllables U+AC00–U+D7A3, fullwidth forms U+FF01–U+FF60 and the emoji
/// blocks U+1F300–U+1F64F, U+1F680–U+1F6FF, U+1F900–U+1F9FF, U+1FA70–U+1FAFF (whole code points).
/// noevia-core's differential test checks every one against the runtime's ICU.
pub const NFC_INERT_RANGES: [(u32, u32); 15] = [
    (0x0000, 0x02ff),
    (0x0400, 0x0482),
    (0x048a, 0x04ff),
    (0x2010, 0x2027),
    (0x2030, 0x205e),
    (0x3001, 0x3029),
    (0x3041, 0x3096),
    (0x30a1, 0x30fc),
    (0x4e00, 0x9fff),
    (0xac00, 0xd7a3),
    (0xff01, 0xff60),
    (0x1f300, 0x1f64f),
    (0x1f680, 0x1f6ff),
    (0x1f900, 0x1f9ff),
    (0x1fa70, 0x1faff),
];

/// Whether the code point `cp` is NFC-inert.
pub fn is_nfc_inert(cp: u32) -> bool {
    NFC_INERT_RANGES.iter().any(|&(a, b)| (a..=b).contains(&cp))
}

/// The length in code units of the longest suffix of `s` made of whole NFC-inert code points (a
/// lone surrogate is never inert).
pub fn inert_tail_len(s: &[u16]) -> usize {
    let mut i = s.len();
    while i > 0 {
        let lo = s.get(i - 1).copied().unwrap_or(0);
        if (0xdc00..=0xdfff).contains(&lo) {
            let hi = if i >= 2 {
                s.get(i - 2).copied().unwrap_or(0)
            } else {
                0
            };
            if !(0xd800..=0xdbff).contains(&hi) {
                break;
            }
            let cp = 0x10000 + ((u32::from(hi) - 0xd800) << 10) + (u32::from(lo) - 0xdc00);
            if !is_nfc_inert(cp) {
                break;
            }
            i -= 2;
        } else if (0xd800..=0xdbff).contains(&lo) || !is_nfc_inert(u32::from(lo)) {
            break;
        } else {
            i -= 1;
        }
    }
    s.len() - i
}

fn inert(s: &[u16]) -> bool {
    inert_tail_len(s) == s.len()
}

const fn u(c: char) -> u16 {
    c as u16
}

fn eq_ascii_ci(c: u16, b: u8) -> bool {
    let fold = |x: u16| {
        if (0x41..=0x5a).contains(&x) {
            x + 0x20
        } else {
            x
        }
    };
    fold(c) == fold(u16::from(b))
}

/// `/%(?:2e|2f|5c|00)/i`.
fn has_encoded_separator(s: &[u16]) -> bool {
    s.windows(3).any(|w| {
        let [p, a, b] = w else { return false };
        *p == u('%')
            && ["2e", "2f", "5c", "00"]
                .iter()
                .any(|pair| pair.bytes().zip([*a, *b]).all(|(x, c)| eq_ascii_ci(c, x)))
    })
}

/// `invalidReason(raw)` for a string `raw`: the JS's text, or `None` when the name is usable.
pub fn invalid_reason(raw: &[u16]) -> Option<&'static str> {
    let name = trim(raw);
    if name.is_empty() {
        return Some("a file name is required");
    }
    if name.len() > MAX_NAME {
        return Some("that file name is too long");
    }
    if name.iter().any(|&c| c < 0x20 || c == 0x7f) {
        return Some("a file name cannot contain control characters");
    }
    if name.contains(&u('\\')) {
        return Some("a file name cannot contain backslashes");
    }
    if has_encoded_separator(name) {
        return Some("a file name cannot contain encoded dots or separators");
    }
    // `/^[A-Za-z]:\//`
    let drive = name.get(1) == Some(&u(':'))
        && name.get(2) == Some(&u('/'))
        && name
            .first()
            .is_some_and(|d| (u('A')..=u('Z')).contains(d) || (u('a')..=u('z')).contains(d));
    if name.first() == Some(&u('/')) || drive {
        return Some("a file name cannot be an absolute path");
    }
    if name
        .split(|&c| c == u('/'))
        .any(|seg| seg.is_empty() || seg == [u('.')] || seg == [u('.'), u('.')])
    {
        return Some("a file name cannot contain empty, \".\" or \"..\" parts");
    }
    None
}

/// `resolveProjectFile(project, raw)`'s answer over the stored names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The index of the one file.
    File(usize),
    /// `code: 'invalid'` with the reason text.
    Invalid(&'static str),
    /// `code: 'missing'`.
    Missing,
    /// `code: 'ambiguous'` with every candidate's index, in order.
    Ambiguous(Vec<usize>),
}

/// `resolveProjectFile` for a non-string argument (`invalidReason` says a name is required).
pub const REQUIRED: Resolved = Resolved::Invalid("a file name is required");

/// project-file-names.cjs `resolveProjectFile(project, raw)` for a string `raw` over `names` (the
/// project's string file names, in order).
pub fn resolve(names: &[Vec<u16>], raw: &[u16]) -> Result<Resolved, Refusal> {
    if let Some(reason) = invalid_reason(raw) {
        return Ok(Resolved::Invalid(reason));
    }
    let wanted = trim(raw);
    if !inert(wanted) {
        return Err(Refusal::Ambiguous);
    }
    // A stored name is `head + tail`, `tail` its longest inert suffix. NFC changes nothing across
    // that boundary (the tail starts with a class-0 code point that never composes with what comes
    // before), so NFC(name) = NFC(head) + tail: only the tail is known without tables.
    // `files.find((f) => norm(f.name) === wanted)`, in order until one equals.
    for (i, name) in names.iter().enumerate() {
        let tail_at = name.len() - inert_tail_len(name);
        if tail_at == 0 {
            if name.as_slice() == wanted {
                return Ok(Resolved::File(i));
            }
        } else if wanted.ends_with(name.get(tail_at..).unwrap_or(&[])) {
            // Equal exactly when NFC(head) is the rest of `wanted`: unknown without tables.
            return Err(Refusal::Ambiguous);
        }
    }
    let mut suffix = Vec::with_capacity(wanted.len() + 1);
    suffix.push(u('/'));
    suffix.extend_from_slice(wanted);
    let mut matches: Vec<usize> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let tail = name.get(name.len() - inert_tail_len(name)..).unwrap_or(&[]);
        if tail.len() == name.len() || tail.len() >= suffix.len() {
            if name.ends_with(&suffix) {
                matches.push(i);
            }
        } else if suffix.ends_with(tail) {
            // The tail is too short to decide `NFC(name).endsWith('/' + wanted)`.
            return Err(Refusal::Ambiguous);
        }
    }
    Ok(match matches.as_slice() {
        [] => Resolved::Missing,
        [one] => Resolved::File(*one),
        _ => Resolved::Ambiguous(matches),
    })
}

fn reply(r: &Resolved) -> Vec<u8> {
    let mut out = Vec::new();
    match r {
        Resolved::File(i) => out.extend_from_slice(format!("{{\"file\":{i}}}").as_bytes()),
        Resolved::Invalid(reason) => {
            out.extend_from_slice(b"{\"code\":\"invalid\",\"reason\":");
            json::push_ascii(&mut out, reason);
            out.push(b'}');
        }
        Resolved::Missing => out.extend_from_slice(b"{\"code\":\"missing\"}"),
        Resolved::Ambiguous(c) => {
            out.extend_from_slice(b"{\"code\":\"ambiguous\",\"candidates\":[");
            for (k, i) in c.iter().enumerate() {
                if k > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(i.to_string().as_bytes());
            }
            out.extend_from_slice(b"]}");
        }
    }
    out
}

/// The `project_file_names` wasm call: input `u8(1)` and UTF-8 JSON `[names, raw]` (names an
/// array of strings, raw any JSON value; at most [`MAX_INPUT_BYTES`]). Replies `{"file":i}`,
/// `{"code":"invalid","reason":"…"}`, `{"code":"missing"}` or
/// `{"code":"ambiguous","candidates":[i,…]}`; status 1 with `{"error":"input"|"too_large"|
/// "ambiguous"}` when refused.
pub fn call(input: &[u8]) -> (u32, Vec<u8>) {
    let refuse = |r: Refusal| (1, r.json().as_bytes().to_vec());
    if input.len() > MAX_INPUT_BYTES {
        return refuse(Refusal::TooLarge);
    }
    let Some((&1, body)) = input.split_first() else {
        return refuse(Refusal::Input);
    };
    let Some(Value::Arr(args)) = json::parse_utf8(body, 2) else {
        return refuse(Refusal::Input);
    };
    let [Value::Arr(list), raw] = args.as_slice() else {
        return refuse(Refusal::Input);
    };
    let mut names = Vec::with_capacity(list.len());
    for n in list {
        match n {
            Value::Str(s) => names.push(s.clone()),
            _ => return refuse(Refusal::Input),
        }
    }
    let resolved = match raw {
        Value::Str(s) => resolve(&names, s),
        _ => Ok(REQUIRED),
    };
    match resolved {
        Ok(r) => (0, reply(&r)),
        Err(e) => refuse(e),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use prompt_framing::js::units;

    fn names(list: &[&str]) -> Vec<Vec<u16>> {
        list.iter().map(|s| units(s)).collect()
    }

    #[test]
    fn resolves() {
        let n = names(&[
            "noevia projects/p/Text/notes.md",
            "a.md",
            "x/b.md",
            "y/b.md",
        ]);
        assert_eq!(resolve(&n, &units(" notes.md ")), Ok(Resolved::File(0)));
        assert_eq!(resolve(&n, &units("a.md")), Ok(Resolved::File(1)));
        assert_eq!(
            resolve(&n, &units("b.md")),
            Ok(Resolved::Ambiguous(vec![2, 3]))
        );
        assert_eq!(resolve(&n, &units("c.md")), Ok(Resolved::Missing));
        assert_eq!(
            resolve(&n, &units("../a.md")),
            Ok(Resolved::Invalid(
                "a file name cannot contain empty, \".\" or \"..\" parts"
            ))
        );
        assert_eq!(
            resolve(&n, &units("C:/a.md")),
            Ok(Resolved::Invalid("a file name cannot be an absolute path"))
        );
        assert_eq!(
            resolve(&n, &units("%2E%2e/a")),
            Ok(Resolved::Invalid(
                "a file name cannot contain encoded dots or separators"
            ))
        );
        assert_eq!(
            resolve(&names(&["e\u{301}.md"]), &units("é.md")),
            Err(Refusal::Ambiguous)
        );
    }

    #[test]
    fn wire() {
        let (s, out) = call(b"\x01[[\"a\",\"x/a\"],\"a\"]");
        assert_eq!((s, out.as_slice()), (0, br#"{"file":0}"#.as_slice()));
        let (s, out) = call(b"\x01[[\"x/a\",\"y/a\"],\"a\"]");
        assert_eq!(
            (s, out.as_slice()),
            (0, br#"{"code":"ambiguous","candidates":[0,1]}"#.as_slice())
        );
        let (s, out) = call(b"\x01[[],5]");
        assert_eq!(
            (s, out.as_slice()),
            (
                0,
                br#"{"code":"invalid","reason":"a file name is required"}"#.as_slice()
            )
        );
        assert_eq!(call(b"\x02[[],\"a\"]").0, 1);
    }
}
