//! The few JavaScript string semantics the preparation depends on, over `js_json::wtf16` text: a
//! lone surrogate is one private-use character, lengths and cuts count UTF-16 code units as
//! `String.prototype.length` and `slice` do. (A copy of what `chat-turn`'s `js` module holds; the
//! two crates do not depend on each other yet.)

use js_json::{wtf16, JValue};

/// `s.length`.
pub fn len(s: &str) -> usize {
    wtf16::len(s)
}

/// `s.slice(0, n)`.
pub fn slice(s: &str, n: usize) -> String {
    wtf16::slice(s, n)
}

/// The characters `String.prototype.trim` and `/\s/` match (WhiteSpace and LineTerminator).
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{a}' | '\u{b}' | '\u{c}' | '\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// `s.trim()`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// `s.length > max ? `${s.slice(0, max)}…` : s`.
pub fn clip(s: &str, max: usize) -> String {
    if len(s) > max {
        format!("{}…", slice(s, max))
    } else {
        s.to_string()
    }
}

/// The UTF-16 code units of a JS string held as `wtf16` text.
pub fn to_units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u16; 2];
    for c in s.chars() {
        match wtf16::lone_unit(c) {
            Some(u) => out.push(u),
            None => out.extend_from_slice(c.encode_utf16(&mut buf)),
        }
    }
    out
}

/// A JS string from its code units, lone surrogates as `wtf16` holds them.
pub fn from_units(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|r| match r {
            Ok(c) => c,
            Err(e) => wtf16::lone_char(e.unpaired_surrogate()).unwrap_or('\u{FFFD}'),
        })
        .collect()
}

/// `prompt-framing.cjs` `frameUntrusted(kind, label, text)`: the Rust port core runs (in its
/// `dav-parse.wasm`), over code units.
pub fn frame_untrusted(kind: &str, label: &str, text: &str) -> String {
    from_units(&prompt_framing::framing::frame_untrusted(
        &to_units(kind),
        &to_units(label),
        &to_units(text),
    ))
}

/// `{ role, content }`, the shape every message of the wire has here.
pub fn message(role: &str, content: String) -> JValue {
    JValue::obj([("role", JValue::from(role)), ("content", JValue::Str(content))])
}

/// `JSON.parse(fs.readFileSync(file, 'utf8'))` of a file's bytes: invalid UTF-8 decodes to U+FFFD as
/// Node's decoder writes it, and the text is parsed keeping lone surrogates. `None` where Node
/// throws, and where `wtf16::parse` refuses text Node would accept (see the crate's differences).
pub fn parse_file(bytes: &[u8]) -> Option<JValue> {
    wtf16::parse(&String::from_utf8_lossy(bytes)).ok()
}
