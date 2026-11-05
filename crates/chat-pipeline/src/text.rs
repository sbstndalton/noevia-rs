//! JavaScript string helpers over [`js_json::wtf16`]-mapped strings.
use crate::Thrown;
use js_json::{wtf16, JValue};

/// The UTF-16 code units of a mapped string.
pub(crate) fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        if let Some(u) = wtf16::lone_unit(c) {
            out.push(u);
        } else {
            let mut buf = [0u16; 2];
            out.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    out
}

/// A mapped string from UTF-16 code units (a lone surrogate kept as its mapped character).
pub(crate) fn from_units(u: &[u16]) -> String {
    let mut out = String::with_capacity(u.len());
    for r in char::decode_utf16(u.iter().copied()) {
        match r {
            Ok(c) => out.push(c),
            Err(e) => {
                if let Some(c) = wtf16::lone_char(e.unpaired_surrogate()) {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// `String(v)`.
pub(crate) fn string(v: &JValue) -> Result<String, Thrown> {
    js_json::to_js_string(v).map_err(|_| Thrown)
}

/// `typeof v === 'string' ? v : None`.
pub(crate) fn as_str(v: &JValue) -> Option<&str> {
    match v {
        JValue::Str(s) => Some(s),
        _ => None,
    }
}

/// `s.length > max ? `${s.slice(0, max)}…` : s`.
pub(crate) fn clip(s: &str, max: usize) -> String {
    if wtf16::len(s) > max {
        let mut out = wtf16::slice(s, max);
        out.push('\u{2026}');
        out
    } else {
        s.to_owned()
    }
}

/// core prompt-framing.cjs `frameUntrusted(kind, label, text)` (the same Rust, as Core runs it in wasm).
pub(crate) fn frame(kind: &str, label: &str, text: &str) -> String {
    from_units(&prompt_framing::framing::frame_untrusted(
        &units(kind),
        &units(label),
        &units(text),
    ))
}

/// JS `WhiteSpace` and `LineTerminator`, as `String.prototype.trim` strips them.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'
            | '\u{a}'
            | '\u{b}'
            | '\u{c}'
            | '\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `s.trim()`.
pub(crate) fn trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

/// The value of an object's own property, as `o[key]` reads it (`undefined` when absent or not an object).
pub(crate) fn get<'a>(o: &'a JValue, key: &str) -> &'a JValue {
    o.get(key)
}

/// `o[key] = v` on an object built here.
pub(crate) fn set(o: &mut JValue, key: &str, v: JValue) {
    if let JValue::Obj(items) = o {
        wtf16::set(items, key, v);
    }
}

/// `{ ...v }`: own enumerable properties of an object, the indices of an array or a string.
pub(crate) fn spread(v: &JValue) -> JValue {
    match v {
        JValue::Obj(items) => JValue::Obj(items.clone()),
        JValue::Arr(items) => JValue::Obj(
            items
                .iter()
                .enumerate()
                .map(|(i, x)| (i.to_string(), x.clone()))
                .collect(),
        ),
        JValue::Str(s) => JValue::Obj(
            units(s)
                .iter()
                .enumerate()
                .map(|(i, u)| (i.to_string(), JValue::Str(from_units(&[*u]))))
                .collect(),
        ),
        _ => JValue::Obj(Vec::new()),
    }
}
