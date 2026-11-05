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

/// `Number(v)` (ECMAScript ToNumber over a JSON value; the same as crates/server-diary local_exchange.rs).
pub(crate) fn js_number(value: &JValue) -> f64 {
    match value {
        JValue::Num(n) => *n,
        JValue::Null => 0.0,
        JValue::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        JValue::Undefined | JValue::Obj(_) => f64::NAN,
        other => match js_json::to_js_string(other) {
            Ok(s) => string_to_number(&s),
            Err(_) => f64::NAN,
        },
    }
}

/// `Number(s)` for a string (ECMAScript StringToNumber): JavaScript's white space and line
/// terminators trimmed ([`js_space`]), then the StringNumericLiteral grammar: `0x`/`0o`/`0b` digits
/// (no sign), or a decimal literal with an optional sign, or `Infinity` with one; anything else is
/// `NaN`, and nothing at all is 0.
pub(crate) fn string_to_number(s: &str) -> f64 {
    let s = trim(s);
    if s.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        let lower = s.get(..2).map(str::to_ascii_lowercase);
        if lower.as_deref() == Some(prefix) {
            let digits = s.get(2..).unwrap_or_default();
            if digits.is_empty() {
                return f64::NAN;
            }
            return digits
                .chars()
                .try_fold(0.0, |n, c| {
                    c.to_digit(radix)
                        .map(|d| n * f64::from(radix) + f64::from(d))
                })
                .unwrap_or(f64::NAN);
        }
    }
    let (sign, unsigned) = match s.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, s.strip_prefix('+').unwrap_or(s)),
    };
    if unsigned == "Infinity" {
        return sign * f64::INFINITY;
    }
    // digits [. digits] | . digits, at least one digit, then [e|E [+|-] digits].
    let b = unsigned.as_bytes();
    let digits_from = |mut i: usize| {
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        i
    };
    let whole = digits_from(0);
    let mut i = whole;
    let mut fraction = 0;
    if b.get(i) == Some(&b'.') {
        let end = digits_from(i + 1);
        fraction = end - i - 1;
        i = end;
    }
    if whole + fraction == 0 {
        return f64::NAN;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let end = digits_from(i);
        if end == i {
            return f64::NAN;
        }
        i = end;
    }
    if i != b.len() {
        return f64::NAN;
    }
    unsigned.parse::<f64>().map_or(f64::NAN, |n| sign * n)
}

/// A regex source whose literal `k`/`s` (either case) fold ASCII only, as JavaScript's non-unicode
/// `/i` does: Rust's Unicode folding would also match the Kelvin sign (U+212A) and the long s
/// (U+017F). Each such literal outside a class becomes `(?-u:x)`, which keeps the enclosing `i`.
/// No input character is rewritten (review S5–S9 P2: in-band sentinels collided with U+FFFF/U+FFFE).
pub(crate) fn js_fold(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 16);
    let mut it = src.chars().peekable();
    // Copies through `end` (inclusive), escapes kept whole.
    fn copy_to(out: &mut String, it: &mut std::iter::Peekable<std::str::Chars<'_>>, end: char) {
        while let Some(ch) = it.next() {
            out.push(ch);
            if ch == '\\' {
                if let Some(e) = it.next() {
                    out.push(e);
                }
            } else if ch == end {
                break;
            }
        }
    }
    while let Some(ch) = it.next() {
        match ch {
            '\\' => {
                out.push(ch);
                if let Some(e) = it.next() {
                    out.push(e);
                    if matches!(e, 'x' | 'u' | 'p' | 'P') && it.peek() == Some(&'{') {
                        copy_to(&mut out, &mut it, '}');
                    }
                }
            }
            '[' => {
                out.push(ch);
                if let Some(c) = it.next_if_eq(&'^') {
                    out.push(c);
                }
                if let Some(c) = it.next_if_eq(&']') {
                    out.push(c);
                }
                copy_to(&mut out, &mut it, ']');
            }
            // A group's flags (`(?i)`, `(?is)`, `(?-u:`) are not literals.
            '(' if it.peek() == Some(&'?') => {
                out.push(ch);
                for f in it.by_ref() {
                    out.push(f);
                    if f == ':' || f == ')' {
                        break;
                    }
                }
            }
            '{' => {
                out.push(ch);
                copy_to(&mut out, &mut it, '}');
            }
            'k' | 'K' | 's' | 'S' => {
                out.push_str("(?-u:");
                out.push(ch);
                out.push(')');
            }
            _ => out.push(ch),
        }
    }
    out
}
