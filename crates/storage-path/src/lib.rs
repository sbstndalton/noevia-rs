//! Storage path rules (noevia#978, strangler slice): a byte-for-byte port of noevia-core's
//! `server/storage-path.cjs` JS reference (`safeRelativePathJs`, `cleanRootJs`, `joinRootJs`,
//! `isPlainFilenameJs`).
//!
//! These guard every storage path that comes from a request (a browse/read/write path, an upload
//! filename) against traversal. The rules, as the JS reference has them:
//!
//! - [`safe_relative_path`]: trim JS whitespace, turn `\` into `/`; refuse (return `""`) a value
//!   that is empty, longer than 500 UTF-16 code units, starts with `/`, contains NUL, has no
//!   segment, or has a `.`/`..` segment; otherwise the non-empty segments joined by `/`.
//!   Percent escapes (`%2e`) are not decoded here: they stay literal name characters.
//! - [`clean_root`]: strip every leading and trailing `/`.
//! - [`join_root`]: `clean_root(root)` and `relative`, empty parts dropped, joined by `/`.
//! - [`is_plain_filename`]: non-empty, at most 200 UTF-16 code units, no `/`, `\` or C0 control,
//!   and not `.` or `..`.
//!
//! Inputs past [`MAX_INPUT_BYTES`] are refused with a typed [`Error`]; noevia-core's
//! `STORAGE_PATH_IMPL=wasm` path fails closed on any error.
#![forbid(unsafe_code)]

use dav_parse::js_trim;
use std::fmt;

/// Largest single argument accepted, in UTF-8 bytes. Real paths are capped at 500 UTF-16 units by
/// the rules themselves; this only bounds what a host can make the module copy.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
/// `safeRelativePath`'s length limit, in UTF-16 code units (JS `String.length`).
pub const MAX_RELATIVE_PATH_UNITS: usize = 500;
/// The upload filename length limit, in UTF-16 code units.
pub const MAX_FILENAME_UNITS: usize = 200;

/// Why an input was refused before any rule ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// An argument is larger than [`MAX_INPUT_BYTES`].
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
                write!(f, "path input is {len} bytes (max {max})")
            }
        }
    }
}

impl std::error::Error for Error {}

fn check(s: &str) -> Result<(), Error> {
    if s.len() > MAX_INPUT_BYTES {
        return Err(Error::InputTooLarge {
            len: s.len(),
            max: MAX_INPUT_BYTES,
        });
    }
    Ok(())
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `safeRelativePath(raw)` for a string `raw`: the normalised connection-relative path, or `""`
/// when it is refused. The output never starts with `/`, never has a `.`, `..` or empty segment and
/// never contains `\` or NUL.
pub fn safe_relative_path(raw: &str) -> Result<String, Error> {
    check(raw)?;
    let value = js_trim(raw).replace('\\', "/");
    if value.is_empty() || utf16_len(&value) > MAX_RELATIVE_PATH_UNITS {
        return Ok(String::new());
    }
    if value.starts_with('/') || value.contains('\0') {
        return Ok(String::new());
    }
    let segments: Vec<&str> = value.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() || segments.iter().any(|s| *s == "." || *s == "..") {
        return Ok(String::new());
    }
    Ok(segments.join("/"))
}

/// `cleanRoot(corpusRoot)` for a string: every leading and trailing `/` removed.
pub fn clean_root(root: &str) -> Result<String, Error> {
    check(root)?;
    Ok(root.trim_matches('/').to_owned())
}

/// `joinRoot(corpusRoot, relative)` for strings: the cleaned root and `relative`, empty parts
/// dropped, joined by `/`.
pub fn join_root(root: &str, relative: &str) -> Result<String, Error> {
    check(root)?;
    check(relative)?;
    let root = root.trim_matches('/');
    Ok(match (root.is_empty(), relative.is_empty()) {
        (true, true) => String::new(),
        (false, true) => root.to_owned(),
        (true, false) => relative.to_owned(),
        (false, false) => format!("{root}/{relative}"),
    })
}

/// The upload filename rule in `uploads.cjs validate`: whether `name` is a plain filename.
pub fn is_plain_filename(name: &str) -> Result<bool, Error> {
    check(name)?;
    Ok(!name.is_empty()
        && utf16_len(name) <= MAX_FILENAME_UNITS
        && !name.chars().any(|c| c == '/' || c == '\\' || c < '\u{20}')
        && name != "."
        && name != "..")
}

/// Which rule a WebAssembly call runs (the `storage_path(op)` argument).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    SafeRelativePath = 1,
    CleanRoot = 2,
    JoinRoot = 3,
    IsPlainFilename = 4,
}

impl Op {
    /// The op for a wire number, if any.
    pub fn from_u32(n: u32) -> Option<Op> {
        match n {
            1 => Some(Op::SafeRelativePath),
            2 => Some(Op::CleanRoot),
            3 => Some(Op::JoinRoot),
            4 => Some(Op::IsPlainFilename),
            _ => None,
        }
    }
}

/// Run `op` on `a` (and `b`, for [`Op::JoinRoot`]); the reply is `{"value":"…"}` for the string
/// rules, `{"value":true|false}` for [`Op::IsPlainFilename`], or `{"error":"code"}`.
pub fn reply_json(op: Op, a: &str, b: &str) -> (bool, String) {
    let mut out = String::from("{");
    let result = match op {
        Op::SafeRelativePath => safe_relative_path(a).map(Some),
        Op::CleanRoot => clean_root(a).map(Some),
        Op::JoinRoot => join_root(a, b).map(Some),
        Op::IsPlainFilename => is_plain_filename(a).map(|ok| {
            out.push_str(if ok {
                "\"value\":true"
            } else {
                "\"value\":false"
            });
            None
        }),
    };
    match result {
        Ok(Some(s)) => {
            out.push_str("\"value\":");
            dav_parse::push_json_string(&mut out, &s);
        }
        Ok(None) => {}
        Err(e) => {
            out.clear();
            out.push_str("{\"error\":");
            dav_parse::push_json_string(&mut out, e.code());
        }
    }
    let ok = !out.starts_with("{\"error\"");
    out.push('}');
    (ok, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        assert_eq!(safe_relative_path("a/b.md").as_deref(), Ok("a/b.md"));
        assert_eq!(safe_relative_path(" a\\\\b//c ").as_deref(), Ok("a/b/c"));
        assert_eq!(safe_relative_path("/abs").as_deref(), Ok(""));
        assert_eq!(safe_relative_path("a/../b").as_deref(), Ok(""));
        assert_eq!(safe_relative_path("a\0b").as_deref(), Ok(""));
        assert_eq!(safe_relative_path("%2e%2e/x").as_deref(), Ok("%2e%2e/x"));
        assert_eq!(clean_root("//r/s//").as_deref(), Ok("r/s"));
        assert_eq!(join_root("/r/", "x").as_deref(), Ok("r/x"));
        assert_eq!(join_root("/", "").as_deref(), Ok(""));
        assert_eq!(is_plain_filename("a.md"), Ok(true));
        assert_eq!(is_plain_filename(".."), Ok(false));
        assert_eq!(is_plain_filename("a\u{1f}"), Ok(false));
        assert_eq!(is_plain_filename(&"é".repeat(200)), Ok(true));
        assert_eq!(is_plain_filename(&"😀".repeat(101)), Ok(false));
        let big = "a".repeat(MAX_INPUT_BYTES + 1);
        assert!(safe_relative_path(&big).is_err());
        assert_eq!(
            reply_json(Op::IsPlainFilename, &big, "").1,
            r#"{"error":"too_large"}"#
        );
        assert_eq!(
            reply_json(Op::JoinRoot, "r", "a\"b"),
            (true, r#"{"value":"r/a\"b"}"#.to_owned())
        );
        assert_eq!(
            reply_json(Op::IsPlainFilename, "x", ""),
            (true, r#"{"value":true}"#.to_owned())
        );
    }
}
