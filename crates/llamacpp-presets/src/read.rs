//! The models.ini parse, revision and facts of core's `llamacpp-presets.cjs` (`read`, `parse`,
//! `get`), the one definition both the Rust front (GET `/api/models/preset`, the conditional
//! writes' revision check) and Node (through `dav-parse.wasm`, ops 5 and 6) use. A second parser
//! on either side could give a stale write the current revision, so there is none.
//!
//! - [`parse`]: the 1 MiB limit (UTF-8 bytes of the decoded text), the sha256 [`Parsed::revision`]
//!   of that text, and the sections: header `/^\s*\[([^\]\r\n]+)\]\s*(?:[;#].*)?$/`, option
//!   `/^\s*([^=\s]+)\s*=\s*(.*?)\s*$/` kept under its canonical name (a later line keeps the
//!   earlier line's place), a duplicate section name refused.
//! - [`Parsed::facts`]: `get(model)`: the model's own options, the `*` section's as the defaults.
//!
//! Text is UTF-16 code units, as the JS holds it; `\s` and the `.` that stops at a line terminator
//! are the ECMAScript sets ([`is_js_space`](crate::is_js_space), CR, LF, U+2028, U+2029).
//! File I/O, and telling a missing file from an unreadable one, stay with the caller.

use crate::{canonical, is_js_space, FIELDS};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

/// The editor limit: the decoded text's UTF-8 length.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// Why a text is not a preset file, with the status core answers it with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Over [`MAX_TEXT_BYTES`]: 413 "Preset file exceeds the editor limit".
    Oversized,
    /// A section name twice: 409 "Duplicate preset sections require operator repair".
    Duplicate,
}

impl Fault {
    /// The word the wasm op answers with.
    pub fn word(self) -> &'static str {
        match self {
            Fault::Oversized => "oversized",
            Fault::Duplicate => "duplicate",
        }
    }
}

/// One `[section]`: its lines `start..end` (the header line to the next header or the end), its
/// options under their canonical names in insertion order, and the lines that assign a field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: Vec<u16>,
    pub start: usize,
    pub end: usize,
    pub options: Vec<(&'static str, Vec<u16>)>,
    /// `(line, field)` for every line `/^\s*([^=\s]+)\s*=/` reads as `key =` whose key names a
    /// field, whether or not its value is one a later line could replace (`prepare` drops these
    /// lines when it writes the field; an option line whose value spans a line terminator is not in
    /// `options`, but it is here).
    pub assignments: Vec<(usize, &'static str)>,
}

/// A parsed models.ini.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    /// Lowercase hex sha256 of the decoded text's UTF-8 bytes.
    pub revision: String,
    /// `text.split('\n').length`.
    pub line_count: usize,
    pub sections: Vec<Section>,
}

/// What `get(model)` answers, besides the model name: one snapshot, one revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts<'a> {
    pub revision: &'a str,
    pub exists: bool,
    pub options: &'a [(&'static str, Vec<u16>)],
    pub defaults: &'a [(&'static str, Vec<u16>)],
}

const LF: u16 = 0x0a;
const CR: u16 = 0x0d;

const fn line_terminator(c: u16) -> bool {
    matches!(c, 0x0a | 0x0d | 0x2028 | 0x2029)
}

fn skip_space(s: &[u16]) -> &[u16] {
    let n = s.iter().position(|&c| !is_js_space(c)).unwrap_or(s.len());
    s.get(n..).unwrap_or(&[])
}

/// `\s*(.*?)\s*$` over `rest`: the value, or `None` where a line terminator sits inside it.
fn trimmed_value(rest: &[u16]) -> Option<&[u16]> {
    let v = skip_space(rest);
    let end = v
        .iter()
        .rposition(|&c| !is_js_space(c))
        .map_or(0, |i| i + 1);
    let v = v.get(..end)?;
    (!v.iter().copied().any(line_terminator)).then_some(v)
}

/// `/^\s*\[([^\]\r\n]+)\]\s*(?:[;#].*)?$/`: the section name.
pub fn header_of(line: &[u16]) -> Option<&[u16]> {
    let rest = skip_space(line).strip_prefix(&[u16::from(b'[')])?;
    let end = rest
        .iter()
        .position(|&c| c == u16::from(b']') || c == CR || c == LF)?;
    if end == 0 || rest.get(end) != Some(&u16::from(b']')) {
        return None;
    }
    let name = rest.get(..end)?;
    let after = skip_space(rest.get(end + 1..)?);
    let Some((&first, comment)) = after.split_first() else {
        return Some(name);
    };
    if first != u16::from(b';') && first != u16::from(b'#') {
        return None;
    }
    (!comment.iter().copied().any(line_terminator)).then_some(name)
}

/// `/^\s*([^=\s]+)\s*=/`: the key and what follows the `=`.
fn assignment_of(line: &[u16]) -> Option<(&[u16], &[u16])> {
    let rest = skip_space(line);
    let end = rest
        .iter()
        .position(|&c| c == u16::from(b'=') || is_js_space(c))?;
    if end == 0 {
        return None;
    }
    let key = rest.get(..end)?;
    let after = skip_space(rest.get(end..)?).strip_prefix(&[u16::from(b'=')])?;
    Some((key, after))
}

/// `/^\s*([^=\s]+)\s*=\s*(.*?)\s*$/`: the key and the value.
pub fn option_of(line: &[u16]) -> Option<(&[u16], &[u16])> {
    let (key, after) = assignment_of(line)?;
    Some((key, trimmed_value(after)?))
}

/// The lowercase hex sha256 of the text as core hashes it (`crypto.createHash('sha256')
/// .update(text)`: the string's UTF-8, a lone surrogate as U+FFFD).
pub fn revision_of(text: &str) -> String {
    use std::fmt::Write as _;
    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn put(options: &mut Vec<(&'static str, Vec<u16>)>, key: &'static str, value: &[u16]) {
    match options.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = value.to_vec(),
        None => options.push((key, value.to_vec())),
    }
}

/// `parse(text)` and `revision(text)`.
pub fn parse(text: &[u16]) -> Result<Parsed, Fault> {
    let decoded = String::from_utf16_lossy(text);
    if decoded.len() > MAX_TEXT_BYTES {
        return Err(Fault::Oversized);
    }
    let revision = revision_of(&decoded);
    let mut sections: Vec<Section> = Vec::new();
    let mut seen: HashSet<&[u16]> = HashSet::new();
    let mut line_count = 0;
    for (index, line) in text.split(|&c| c == LF).enumerate() {
        line_count = index + 1;
        if let Some(name) = header_of(line) {
            if !seen.insert(name) {
                return Err(Fault::Duplicate);
            }
            if let Some(previous) = sections.last_mut() {
                previous.end = index;
            }
            sections.push(Section {
                name: name.to_vec(),
                start: index,
                end: usize::MAX,
                options: Vec::new(),
                assignments: Vec::new(),
            });
        } else if let Some(current) = sections.last_mut() {
            if let Some((key, after)) = assignment_of(line) {
                if let Some(field) = canonical(key) {
                    current.assignments.push((index, field.name));
                    if let Some(value) = trimmed_value(after) {
                        put(&mut current.options, field.name, value);
                    }
                }
            }
        }
    }
    for s in sections.iter_mut().filter(|s| s.end == usize::MAX) {
        s.end = line_count;
    }
    Ok(Parsed {
        revision,
        line_count,
        sections,
    })
}

impl Parsed {
    /// `sections.get(name)`.
    pub fn section(&self, name: &[u16]) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// `get(model)` without the model name: the facts of one snapshot.
    pub fn facts(&self, model: &[u16]) -> Facts<'_> {
        let own = self.section(model);
        Facts {
            revision: &self.revision,
            exists: own.is_some(),
            options: own.map_or(&[], |s| s.options.as_slice()),
            defaults: self
                .section(&[u16::from(b'*')])
                .map_or(&[], |s| s.options.as_slice()),
        }
    }
}

/// `Object.keys(fields)`: the canonical option names, in order.
pub fn field_names() -> impl Iterator<Item = &'static str> {
    FIELDS.iter().map(|f| f.name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use prompt_framing::js::units;

    fn p(text: &str) -> Parsed {
        parse(&units(text)).unwrap()
    }

    fn opts(o: &[(&'static str, Vec<u16>)]) -> Vec<(&'static str, String)> {
        o.iter()
            .map(|(k, v)| (*k, String::from_utf16(v).unwrap()))
            .collect()
    }

    #[test]
    fn valid_empty_is_not_a_fault() {
        let parsed = p("");
        assert_eq!(
            parsed.revision,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!((parsed.line_count, parsed.sections.len()), (1, 0));
        let facts = parsed.facts(&units("m"));
        assert!(!facts.exists && facts.options.is_empty() && facts.defaults.is_empty());
        assert!(p("; only a comment\n\n").sections.is_empty());
    }

    #[test]
    fn sections_options_and_defaults() {
        let parsed = p("[*]\nparallel=2\n[m]\nc=4096\nmodel=/private/x.gguf\nnp = 3\nc = 8192\n");
        let f = parsed.facts(&units("m"));
        assert!(f.exists);
        assert_eq!(
            opts(f.options),
            [("ctx-size", "8192".into()), ("parallel", "3".into())]
        );
        assert_eq!(opts(f.defaults), [("parallel", "2".into())]);
        let s = parsed.section(&units("m")).unwrap();
        assert_eq!((s.start, s.end), (2, 8));
        assert_eq!(
            s.assignments,
            [(3, "ctx-size"), (5, "parallel"), (6, "ctx-size")],
            "model= is not a field"
        );
        assert_eq!(parsed.section(&units("*")).unwrap().end, 2);
        let absent = parsed.facts(&units("other"));
        assert!(!absent.exists && absent.options.is_empty());
        assert_eq!(absent.defaults.len(), 1);
        assert!(p("[a]\nc=1\n").facts(&units("*")).defaults.is_empty());
    }

    #[test]
    fn faults() {
        assert_eq!(parse(&units("[m]\n[m]\n")), Err(Fault::Duplicate));
        assert_eq!(
            parse(&units("[m]\n[ m]\n")).map(|s| s.sections.len()),
            Ok(2)
        );
        let big = "x".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(parse(&units(&big)), Err(Fault::Oversized));
        assert!(parse(&units(&big[1..])).is_ok());
        // The limit is on UTF-8 bytes, not units, and comes before the duplicate check.
        let wide = format!("[m]\n[m]\n{}", "\u{20ac}".repeat(MAX_TEXT_BYTES / 3));
        assert_eq!(parse(&units(&wide)), Err(Fault::Oversized));
    }

    #[test]
    fn regex_edges() {
        let h = |s: &str| header_of(&units(s)).map(|n| String::from_utf16(n).unwrap());
        assert_eq!(h("[a]"), Some("a".into()));
        assert_eq!(h("  [a b] ; note"), Some("a b".into()));
        assert_eq!(h("[a]\r"), Some("a".into()));
        assert_eq!(h("[a] ;c\r"), None);
        assert_eq!(h("[]"), None);
        assert_eq!(h("[a] x"), None);
        assert_eq!(h("\u{feff}[a]"), Some("a".into()));
        assert_eq!(h("[a\r]"), None);
        let o = |s: &str| {
            option_of(&units(s)).map(|(k, v)| {
                (
                    String::from_utf16(k).unwrap(),
                    String::from_utf16(v).unwrap(),
                )
            })
        };
        assert_eq!(
            o(" model   =  /m/x  "),
            Some(("model".into(), "/m/x".into()))
        );
        assert_eq!(o("c=\u{2028}8192"), Some(("c".into(), "8192".into())));
        assert_eq!(o("c = 81\u{2028}92"), None);
        assert_eq!(o("k ="), Some(("k".into(), String::new())));
        assert_eq!(o("= v"), None);
        assert_eq!(o("a b = c"), None);
        assert_eq!(o("a ="), Some(("a".into(), String::new())));
    }

    #[test]
    fn an_assignment_whose_value_spans_a_terminator_is_listed_but_has_no_option() {
        let parsed = p("[m]\nc = 81\u{2028}92\nnp=2\n");
        let s = parsed.section(&units("m")).unwrap();
        assert_eq!(s.assignments, [(1, "ctx-size"), (2, "parallel")]);
        assert_eq!(opts(&s.options), [("parallel", "2".into())]);
    }

    #[test]
    fn crlf_lines_keep_their_options() {
        let parsed = p("[m]\r\nc=4096\r\nnp=2\r\n");
        assert_eq!(
            opts(&parsed.section(&units("m")).unwrap().options),
            [("ctx-size", "4096".into()), ("parallel", "2".into())]
        );
    }

    #[test]
    fn revision_hashes_the_decoded_text() {
        // A lone surrogate is U+FFFD to the hash, as `update(string)` encodes it.
        let lone = [0xd800u16, u16::from(b'a')];
        assert_eq!(parse(&lone).unwrap().revision, revision_of("\u{fffd}a"));
        assert_eq!(field_names().next(), Some("ctx-size"));
        assert_eq!(field_names().count(), FIELDS.len());
    }
}
