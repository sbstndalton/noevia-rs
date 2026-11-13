//! `server/quoted-name.cjs`: the one way a name (a file's, a project's) is quoted into the system
//! text. JSON-quoted, then every code point `JSON.stringify` leaves raw but a reader or a model can
//! take for a line break or a display reorder is written as `\uXXXX` (U+0085, U+200E, U+200F,
//! U+2028, U+2029, U+202A-U+202E, U+2066-U+2069), so a name can neither start a line of its own nor
//! be drawn as something else.
//!
//! `project-retrieval::names::quote_name` and `server-projects`'s copy quote the same way but write
//! a `wtf16` lone surrogate raw (they quote lossy Rust text); this crate holds JS strings as `wtf16`
//! text, where `JSON.stringify` writes a lone surrogate as `\udxxx`, so it quotes through
//! `wtf16::stringify` instead of depending on either.

use js_json::{wtf16, JValue};

/// The code points `quoted-name.cjs` `RAW` escapes after JSON quoting.
fn raw(c: char) -> bool {
    matches!(
        c,
        '\u{85}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// `quoteName(name)` of a name already through `String(name)`.
pub fn quote_name(name: &str) -> String {
    let quoted = wtf16::stringify(&JValue::Str(name.to_string())).unwrap_or_default();
    let mut out = String::with_capacity(quoted.len());
    for c in quoted.chars() {
        if raw(c) {
            out.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

/// `quotedNames(names)`: each name quoted, joined with `", "`.
pub fn quoted_names<S: AsRef<str>>(names: &[S]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| quote_name(n.as_ref())).collect();
    quoted.join(", ")
}
