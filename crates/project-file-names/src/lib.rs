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
//! The JS compares `normalize('NFC')` forms. The port carries no normalization data: it reads only
//! strings whose every code unit is in [`is_nfc_inert`] — code points that are their own NFC form,
//! have canonical combining class 0 and never appear after the first position of any canonical
//! decomposition, so a string of them is unchanged by NFC whatever its neighbours (noevia-core's
//! differential test checks this exhaustively against the runtime's own ICU). When the wanted name
//! or a stored name it is compared with holds anything else (combining marks, Greek, Arabic,
//! Hebrew, Indic scripts, lone surrogates, …), the port refuses (`ambiguous`), and the host then
//! resolves nothing. That is the one way it is stricter than the JS, besides requests over
//! [`MAX_INPUT_BYTES`] (`too_large`).
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

/// A code unit that NFC leaves alone in any context (see the crate docs): U+0000–U+02FF, Cyrillic
/// U+0400–U+0482 and U+048A–U+04FF, Hiragana U+3041–U+3096, Katakana U+30A1–U+30FA, CJK Unified
/// Ideographs U+4E00–U+9FFF and Hangul syllables U+AC00–U+D7A3.
pub fn is_nfc_inert(c: u16) -> bool {
    matches!(c,
        0x0000..=0x02ff
        | 0x0400..=0x0482
        | 0x048a..=0x04ff
        | 0x3041..=0x3096
        | 0x30a1..=0x30fa
        | 0x4e00..=0x9fff
        | 0xac00..=0xd7a3)
}

/// The NFC-inert ranges, for the host's exhaustive check.
pub const NFC_INERT_RANGES: [(u32, u32); 7] = [
    (0x0000, 0x02ff),
    (0x0400, 0x0482),
    (0x048a, 0x04ff),
    (0x3041, 0x3096),
    (0x30a1, 0x30fa),
    (0x4e00, 0x9fff),
    (0xac00, 0xd7a3),
];

fn inert(s: &[u16]) -> bool {
    s.iter().all(|&c| is_nfc_inert(c))
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
    // `files.find((f) => norm(f.name) === wanted)`: names are compared in order until one equals.
    for (i, name) in names.iter().enumerate() {
        if !inert(name) {
            return Err(Refusal::Ambiguous);
        }
        if name.as_slice() == wanted {
            return Ok(Resolved::File(i));
        }
    }
    let mut suffix = Vec::with_capacity(wanted.len() + 1);
    suffix.push(u('/'));
    suffix.extend_from_slice(wanted);
    let matches: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| n.ends_with(&suffix))
        .map(|(i, _)| i)
        .collect();
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
