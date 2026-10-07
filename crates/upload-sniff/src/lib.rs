//! Upload checks (noevia#977): the Rust port of noevia-core's `server/upload-sniff.cjs`
//! (`validateJs`, `classifyJs`, `decodeTextJs`, moved there unchanged from `uploads.cjs`).
//!
//! - [`validate`]: the plain-filename rule (reused from `storage-path`, not duplicated), the 25 MB
//!   cap ([`CAP`]) and the archive refusal (archive file names, and archive magic numbers unless the
//!   name is a packaged Office/OpenDocument document). It needs only the name, the total length and
//!   the first [`SNIFF_BYTES`] bytes.
//! - [`classify`]: the upload group (`Documents`, `Images`, `Text`, `Other`) from Node's
//!   `path.extname(name).toLowerCase()`.
//! - [`decode_text`]: BOM (UTF-8, UTF-16LE, UTF-16BE), then NUL means binary, then strict UTF-8,
//!   then windows-1252, each exactly as Node's WHATWG `TextDecoder(…, { fatal: true })` decodes
//!   (including its own removal of one further leading BOM in the UTF-8/UTF-16 decoders).
#![forbid(unsafe_code)]

use std::fmt;

/// The upload size cap (`uploads.cjs` CAP): 25 MiB.
pub const CAP: u64 = 25 * 1024 * 1024;
/// How many leading bytes [`validate`] looks at: `ustar` ends at offset 262.
pub const SNIFF_BYTES: usize = 262;
/// Largest input [`decode_text`] accepts. Callers only decode validated uploads (at most [`CAP`]).
pub const MAX_DECODE_BYTES: usize = CAP as usize;

/// Why an input was refused before any rule ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The input is larger than the call accepts.
    InputTooLarge { len: usize, max: usize },
}

impl Error {
    /// A stable machine-readable code, used across the WebAssembly boundary.
    pub fn code(&self) -> &'static str {
        match self {
            Error::InputTooLarge { .. } => "too_large",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InputTooLarge { len, max } => {
                write!(f, "upload input is {len} bytes (max {max})")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Why [`validate`] refused an upload. The JS side owns the messages; `status` is the HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not a plain filename of at most 200 characters (400).
    Filename,
    /// Empty (400).
    Empty,
    /// Larger than [`CAP`] (413).
    TooBig,
    /// An archive bundle (400).
    Archive,
}

impl Refusal {
    /// The wire code.
    pub fn code(self) -> &'static str {
        match self {
            Refusal::Filename => "filename",
            Refusal::Empty => "empty",
            Refusal::TooBig => "too_big",
            Refusal::Archive => "archive",
        }
    }
    /// The HTTP status `uploads.cjs` attaches.
    pub fn status(self) -> u16 {
        match self {
            Refusal::TooBig => 413,
            _ => 400,
        }
    }
}

/// Node's POSIX `path.extname`, on UTF-8 (only the ASCII `/` and `.` matter, so byte offsets give
/// the same slice as UTF-16 offsets).
pub fn extname(path: &str) -> &str {
    let b = path.as_bytes();
    let mut start_dot: Option<usize> = None;
    let mut start_part = 0usize;
    let mut end: Option<usize> = None;
    let mut matched_slash = true;
    let mut pre_dot_state = 0i8;
    for (i, &c) in b.iter().enumerate().rev() {
        if c == b'/' {
            if !matched_slash {
                start_part = i + 1;
                break;
            }
            continue;
        }
        if end.is_none() {
            matched_slash = false;
            end = Some(i + 1);
        }
        if c == b'.' {
            if start_dot.is_none() {
                start_dot = Some(i);
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot.is_some() {
            pre_dot_state = -1;
        }
    }
    match (start_dot, end) {
        (Some(dot), Some(end))
            if pre_dot_state != 0
                && !(pre_dot_state == 1 && dot + 1 == end && dot == start_part + 1) =>
        {
            path.get(dot..end).unwrap_or("")
        }
        _ => "",
    }
}

fn lower_ext(name: &str) -> String {
    // JS toLowerCase: full Unicode lowercase (e.g. KELVIN SIGN → k), as Rust's to_lowercase.
    extname(name).to_lowercase()
}

const DOCUMENT_EXTS: &[&str] = &[
    ".pdf", ".doc", ".docx", ".odt", ".rtf", ".ppt", ".pptx", ".xls", ".xlsx", ".ods", ".odp",
    ".epub",
];
const IMAGE_EXTS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".webp", ".gif", ".heic", ".heif", ".tif", ".tiff", ".bmp", ".svg",
    ".avif",
];
/// `storage-client.cjs` TEXT_EXTENSIONS.
pub const TEXT_EXTENSIONS: &[&str] = &[
    ".txt",
    ".md",
    ".markdown",
    ".json",
    ".csv",
    ".yml",
    ".yaml",
    ".ts",
    ".tsx",
    ".js",
    ".jsx",
    ".py",
    ".sh",
    ".html",
    ".css",
];
const PACKAGED_EXTS: &[&str] = &[".docx", ".xlsx", ".pptx", ".odt", ".ods", ".odp", ".epub"];
const ARCHIVE_SUFFIXES: &[&str] = &[
    "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "cab", "iso",
];

/// `classify(name)`: the upload group.
pub fn classify(name: &str) -> &'static str {
    let ext = lower_ext(name);
    if DOCUMENT_EXTS.contains(&ext.as_str()) {
        "Documents"
    } else if IMAGE_EXTS.contains(&ext.as_str()) {
        "Images"
    } else if TEXT_EXTENSIONS.contains(&ext.as_str()) {
        "Text"
    } else {
        "Other"
    }
}

/// `/\.(zip|…)$/i` on the whole name: the JS `i` flag without `u` folds ASCII only.
fn archive_name(name: &str) -> bool {
    let b = name.as_bytes();
    ARCHIVE_SUFFIXES.iter().any(|s| {
        let n = s.len() + 1;
        b.len() >= n
            && b.get(b.len() - n) == Some(&b'.')
            && b.get(b.len() - s.len()..)
                .is_some_and(|tail| tail.eq_ignore_ascii_case(s.as_bytes()))
    })
}

/// The archive magic numbers, on the first bytes of the upload.
pub fn archive_magic(head: &[u8]) -> bool {
    head.starts_with(&[0x50, 0x4b, 3, 4])
        || head.starts_with(&[0x1f, 0x8b])
        || head.starts_with(b"Rar!")
        || head.starts_with(&[b'7', b'z', 0xbc, 0xaf])
        || head.starts_with(b"BZh")
        || head.get(257..262) == Some(b"ustar".as_slice())
}

/// `validate(name, bytes)` given the name, the upload's total length and (at least) its first
/// [`SNIFF_BYTES`] bytes (fewer only when the upload is shorter). `None` means accepted.
pub fn validate(name: &str, len: u64, head: &[u8]) -> Option<Refusal> {
    // A name past storage-path's input cap is far past 200 characters: not plain.
    if !storage_path::is_plain_filename(name).unwrap_or(false) {
        return Some(Refusal::Filename);
    }
    if len == 0 {
        return Some(Refusal::Empty);
    }
    if len > CAP {
        return Some(Refusal::TooBig);
    }
    let packaged = PACKAGED_EXTS.contains(&lower_ext(name).as_str());
    if archive_name(name) || (archive_magic(head) && !packaged) {
        return Some(Refusal::Archive);
    }
    None
}

/// The encoding `decodeText` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

impl Encoding {
    /// The name `decodeText` returns.
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf16Le => "utf-16le",
            Encoding::Utf16Be => "utf-16be",
            Encoding::Windows1252 => "windows-1252",
        }
    }
    /// The wire tag (1-4; 0 means "not text").
    pub fn tag(self) -> u8 {
        match self {
            Encoding::Utf8 => 1,
            Encoding::Utf16Le => 2,
            Encoding::Utf16Be => 3,
            Encoding::Windows1252 => 4,
        }
    }
}

/// WHATWG windows-1252 for 0x80..=0x9F; the five bytes it leaves undefined (0x81, 0x8D, 0x8F,
/// 0x90, 0x9D) decode to the C1 control of the same value, as the index does.
const CP1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

/// One windows-1252 byte (a total mapping).
pub fn cp1252(b: u8) -> char {
    if (0x80..0xa0).contains(&b) {
        CP1252_HIGH
            .get(usize::from(b - 0x80))
            .copied()
            .unwrap_or('\u{FFFD}')
    } else {
        char::from(b)
    }
}

const BOM8: &[u8] = &[0xef, 0xbb, 0xbf];

/// Strict UTF-8 into `out`; `TextDecoder` (not ignoreBOM) drops one further leading BOM.
fn utf8_into(bytes: &[u8], out: &mut String) -> bool {
    let Ok(s) = std::str::from_utf8(bytes) else {
        return false;
    };
    out.reserve_exact(s.len());
    out.push_str(s.strip_prefix('\u{FEFF}').unwrap_or(s));
    true
}

/// Strict UTF-16 into `out`: an odd length (a truncated unit) or an unpaired surrogate fails, and
/// one leading U+FEFF is dropped. `out` is unchanged on failure.
fn utf16_into(bytes: &[u8], le: bool, out: &mut String) -> bool {
    if !bytes.len().is_multiple_of(2) {
        return false;
    }
    let units = || {
        bytes.as_chunks::<2>().0.iter().map(move |&p| {
            if le {
                u16::from_le_bytes(p)
            } else {
                u16::from_be_bytes(p)
            }
        })
    };
    let mut size = 0usize;
    for c in char::decode_utf16(units()) {
        match c {
            Ok(c) => size += c.len_utf8(),
            Err(_) => return false,
        }
    }
    out.reserve_exact(size);
    let mut first = true;
    for c in char::decode_utf16(units()).flatten() {
        if !(first && c == '\u{FEFF}') {
            out.push(c);
        }
        first = false;
    }
    true
}

fn cp1252_into(bytes: &[u8], out: &mut String) {
    out.reserve_exact(bytes.iter().map(|&b| cp1252(b).len_utf8()).sum());
    out.extend(bytes.iter().map(|&b| cp1252(b)));
}

/// The decoding step of [`decode_text`], appending to `out`.
fn decode_into(bytes: &[u8], out: &mut String) -> Result<Option<Encoding>, Error> {
    if bytes.len() > MAX_DECODE_BYTES {
        return Err(Error::InputTooLarge {
            len: bytes.len(),
            max: MAX_DECODE_BYTES,
        });
    }
    if let Some(rest) = bytes.strip_prefix(BOM8) {
        if utf8_into(rest, out) {
            return Ok(Some(Encoding::Utf8));
        }
    }
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        if utf16_into(rest, true, out) {
            return Ok(Some(Encoding::Utf16Le));
        }
    }
    if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        if utf16_into(rest, false, out) {
            return Ok(Some(Encoding::Utf16Be));
        }
    }
    if bytes.contains(&0) {
        return Ok(None);
    }
    if utf8_into(bytes, out) {
        return Ok(Some(Encoding::Utf8));
    }
    cp1252_into(bytes, out);
    Ok(Some(Encoding::Windows1252))
}

/// `decodeText(bytes)`: `None` when the bytes are not text.
pub fn decode_text(bytes: &[u8]) -> Result<Option<(String, Encoding)>, Error> {
    let mut out = String::new();
    Ok(decode_into(bytes, &mut out)?.map(|e| (out, e)))
}

/// The `validate` reply: `{"value":null}` or `{"value":{"refusal":"…","status":N}}`.
pub fn validate_json(name: &str, len: u64, head: &[u8]) -> String {
    match validate(name, len, head) {
        None => "{\"value\":null}".to_owned(),
        Some(r) => format!(
            "{{\"value\":{{\"refusal\":\"{}\",\"status\":{}}}}}",
            r.code(),
            r.status()
        ),
    }
}

/// The `classify` reply: `{"value":"Group"}`.
pub fn classify_json(name: &str) -> String {
    format!("{{\"value\":\"{}\"}}", classify(name))
}

/// The `decodeText` reply on the wire: one tag byte (0 = not text, else [`Encoding::tag`]) then
/// the decoded text as UTF-8. Raw rather than JSON: a JSON string of control characters would be up
/// to six times the text. Decoded in place after the tag, so the module holds the input and one
/// exactly-sized copy of the text (at most 3 bytes per input byte), never two.
pub fn decode_reply(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = String::from("\0");
    match decode_into(bytes, &mut out)? {
        None => Ok(vec![0]),
        Some(enc) => {
            let mut v = out.into_bytes();
            if let Some(t) = v.first_mut() {
                *t = enc.tag();
            }
            Ok(v)
        }
    }
}

/// `{"error":"code"}` for an [`Error`].
pub fn error_json(e: &Error) -> String {
    let mut out = String::from("{\"error\":");
    dav_parse::push_json_string(&mut out, e.code());
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn extname_matches_node() {
        for (p, e) in [
            ("a.txt", ".txt"),
            (".bashrc", ""),
            ("..", ""),
            ("...", "."),
            ("a.", "."),
            ("a..", "."),
            (".a.b", ".b"),
            ("dir.x/file", ""),
            ("dir/file.y/", ".y"),
            ("", ""),
            ("..a", ".a"),
        ] {
            assert_eq!(extname(p), e, "{p}");
        }
    }

    #[test]
    fn cp1252_undefined_bytes_are_c1() {
        for b in [0x81u8, 0x8d, 0x8f, 0x90, 0x9d] {
            assert_eq!(cp1252(b) as u32, u32::from(b));
        }
        assert_eq!(cp1252(0x80), '€');
        assert_eq!(cp1252(0xff), 'ÿ');
    }

    #[test]
    fn decode_edges() {
        assert_eq!(
            decode_text(b"").unwrap(),
            Some((String::new(), Encoding::Utf8))
        );
        assert_eq!(decode_text(b"a\0").unwrap(), None);
        assert_eq!(
            decode_text(&[0xef, 0xbb, 0xbf, 0xef, 0xbb, 0xbf, b'a']).unwrap(),
            Some(("a".to_owned(), Encoding::Utf8))
        );
        assert_eq!(
            decode_text(&[0xff, 0xfe, b'a', 0]).unwrap(),
            Some(("a".to_owned(), Encoding::Utf16Le))
        );
        assert_eq!(
            decode_text(&[0xff, 0xfe, b'a']).unwrap(),
            Some(("ÿþa".to_owned(), Encoding::Windows1252))
        );
        assert_eq!(decode_text(&[0xff, 0xfe, 0]).unwrap(), None);
        assert_eq!(
            decode_text(&[0xc0, 0x80]).unwrap(),
            Some(("À€".to_owned(), Encoding::Windows1252))
        );
    }
}
