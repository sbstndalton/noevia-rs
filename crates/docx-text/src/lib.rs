//! Bounded DOCX main-body text, a port of noevia-services' `ocr/docx_text.py` `extract_docx`
//! (noevia#981): the ZIP end-of-central-directory pre-check, member count / size / encryption /
//! compression-ratio limits, the DOCTYPE/ENTITY and encoding refusal, and the walk over
//! `word/document.xml`'s body. Text and truncation are identical to Python; refusals agree in
//! class (see `Refusal::class`). Nothing touches the filesystem or the network.
//!
//! Stricter than Python (each refused here, with the reason):
//! - ZIP: a non-empty archive comment (can carry a second archive); a zip64 locator before the
//!   EOCD, zip64 or Info-ZIP Unicode Path (0x7075, which renames a member in Python) extra
//!   fields, or 0xFFFFFFFF sizes/offsets; data before the first member or between members
//!   (zipfile's "concat" shifting); a central directory whose entries do not exactly fill its
//!   declared size, or whose count differs from the EOCD's; any member whose local header
//!   (signature, flags, method, name, CRC and sizes, or its data descriptor) disagrees with
//!   the central directory, or whose bytes overlap another's (Python checks only the opened
//!   member); flag bits other than encryption, DEFLATE options, data descriptor and UTF-8
//!   names; methods other than stored and DEFLATE (Python also inflates bzip2 and LZMA);
//!   member names containing NUL (Python truncates them) or empty; a DEFLATE stream that does
//!   not end exactly at the compressed size or yields more than the declared size (Python
//!   ignores trailing input and truncates excess output before the CRC check).
//! - XML: element and attribute names outside ASCII; a declared encoding other than UTF-8
//!   (expat decodes latin-1 and friends); re-binding the `xml` prefix or elements in the
//!   `xml` namespace; character references longer than 10 digits; element nesting deeper
//!   than `MAX_DEPTH` (Python's recursive walk fails near 1000 levels, but only on walked
//!   elements).

#![forbid(unsafe_code)]

mod crc;
mod xml;
mod zip;

/// Python's limits, unchanged.
pub const MAX_MEMBERS: usize = 1000;
pub const MAX_TOTAL_UNCOMPRESSED: u64 = 64 * 1024 * 1024;
pub const MAX_CENTRAL_DIR: u64 = 2 * 1024 * 1024;
pub const MAX_RATIO: u64 = 200;
pub const XML_LIMIT: usize = 8 * 1024 * 1024;
pub const TEXT_LIMIT: usize = 200_000;
/// Element nesting refused beyond this (Python: RecursionError in its walk near 1000).
pub const MAX_DEPTH: usize = 256;
/// The OCR service accepts at most 25 MiB bodies; the CLI and library refuse more.
pub const MAX_INPUT_BYTES: usize = 25 * 1024 * 1024;
pub const SCOPE: &str = "DOCX body text and tables only; page layout, images, headers, footers, comments and footnotes are not interpreted.";

/// Why a document was refused. `class()` is the label the shared fixtures use for Python's
/// exception (see tools/gen-docx-text.py).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// More than `MAX_INPUT_BYTES` (never reaches Python: the HTTP server refuses first).
    Input,
    /// 'Invalid DOCX container', and every zipfile/zlib error.
    Container,
    /// 'DOCX container exceeds its processing limits' (the EOCD pre-check).
    Limits,
    /// 'Duplicate or excessive DOCX members'.
    Members,
    /// 'Encrypted or oversized DOCX container'.
    EncryptedOrOversized,
    /// 'DOCX main document is missing'.
    Missing,
    /// 'DOCX text exceeds its decompression limit'.
    Decompression,
    /// 'DOCX text exceeds its processing limit'.
    XmlLimit,
    /// 'Unsupported DOCX XML declarations or encoding'.
    XmlDeclarations,
    /// ElementTree.ParseError.
    XmlMalformed,
    /// 'Unsupported DOCX document namespace'.
    Namespace,
    /// 'DOCX body is missing'.
    Body,
    /// RecursionError in Python's walk.
    Nesting,
}

impl Refusal {
    pub fn class(self) -> &'static str {
        match self {
            Refusal::Input => "input",
            Refusal::Container => "container",
            Refusal::Limits => "limits",
            Refusal::Members => "members",
            Refusal::EncryptedOrOversized => "encrypted_or_oversized",
            Refusal::Missing => "missing",
            Refusal::Decompression => "decompression",
            Refusal::XmlLimit => "xml_limit",
            Refusal::XmlDeclarations => "xml_declarations",
            Refusal::XmlMalformed => "xml_malformed",
            Refusal::Namespace => "namespace",
            Refusal::Body => "body",
            Refusal::Nesting => "nesting",
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.class())
    }
}

impl std::error::Error for Refusal {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub text: String,
    pub truncated: bool,
}

/// Python's `str.isspace()` characters, which `str.strip()` removes.
pub fn is_python_space(c: char) -> bool {
    matches!(c, '\u{9}'..='\u{d}' | '\u{1c}'..='\u{20}' | '\u{85}' | '\u{a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

/// Python's `re.search(br'<!\s*(?:DOCTYPE|ENTITY)', xml, re.I)`.
fn has_declaration(xml: &[u8]) -> bool {
    xml.windows(2).enumerate().any(|(i, w)| {
        w == b"<!" && {
            let rest = xml.get(i + 2..).unwrap_or_default();
            let skip = rest
                .iter()
                .take_while(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
                .count();
            let word = rest.get(skip..).unwrap_or_default();
            [b"DOCTYPE".as_slice(), b"ENTITY".as_slice()]
                .iter()
                .any(|k| {
                    word.get(..k.len())
                        .is_some_and(|w| w.eq_ignore_ascii_case(k))
                })
        }
    })
}

/// `extract_docx(data)` without the constant `scope` field.
pub fn extract_docx(data: &[u8]) -> Result<Extracted, Refusal> {
    if data.len() > MAX_INPUT_BYTES {
        return Err(Refusal::Input);
    }
    let xml = zip::document_xml(data)?;
    if xml.contains(&0) || has_declaration(&xml) {
        return Err(Refusal::XmlDeclarations);
    }
    let walked = xml::walk(&xml)?;
    if !walked.root_ok {
        return Err(Refusal::Namespace);
    }
    if !walked.body_seen {
        return Err(Refusal::Body);
    }
    Ok(Extracted {
        text: walked.text.trim_matches(is_python_space).to_owned(),
        truncated: walked.truncated,
    })
}

fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The JSON object Python's server replies with: `{"text": ..., "truncated": ..., "scope": ...}`.
pub fn to_json(e: &Extracted) -> String {
    let mut out = String::with_capacity(e.text.len() + 200);
    out.push_str("{\"text\": ");
    json_string(&mut out, &e.text);
    out.push_str(", \"truncated\": ");
    out.push_str(if e.truncated { "true" } else { "false" });
    out.push_str(", \"scope\": ");
    json_string(&mut out, SCOPE);
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_scan_matches_the_python_regex() {
        assert!(has_declaration(b"<!DOCTYPE x>"));
        assert!(has_declaration(b"a<! \x0b\x0centity"));
        assert!(!has_declaration(b"<!-- DOCTYPE -->"));
        assert!(!has_declaration(b"<!DOCTYP"));
    }

    #[test]
    fn python_strip_set() {
        assert_eq!("\u{85}\u{3000} a \u{a0}".trim_matches(is_python_space), "a");
        assert!(!is_python_space('\u{200b}'));
    }
}
