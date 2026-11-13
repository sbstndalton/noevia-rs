//! JavaScript string and object semantics the turn depends on, over `js_json`'s values.
//!
//! Text that came from a model or a client is held as `js_json::wtf16` holds it: a lone surrogate
//! is one private-use character, so a reply that splits an emoji across two deltas relays exactly
//! as Node relays it (each delta written with its `\udxxx` escape), and lengths and cuts count
//! UTF-16 code units as `String.prototype.length` and `slice` do.

use js_json::{wtf16, JValue};

/// `a + b` for JS strings held as `wtf16` text: a lone high surrogate at the end of `a` and a lone
/// low one at the start of `b` become the one character they are, as JS concatenation makes them.
pub fn concat(a: &mut String, b: &str) {
    let mut rest = b.chars();
    let first = rest.clone().next();
    if let (Some(hi), Some(lo)) = (
        a.chars().next_back().and_then(wtf16::lone_unit),
        first.and_then(wtf16::lone_unit),
    ) {
        if (0xd800..0xdc00).contains(&hi) && (0xdc00..0xe000).contains(&lo) {
            let joined = 0x10000 + ((u32::from(hi) - 0xd800) << 10) + (u32::from(lo) - 0xdc00);
            if let Some(c) = char::from_u32(joined) {
                a.pop();
                a.push(c);
                rest.next();
                a.push_str(rest.as_str());
                return;
            }
        }
    }
    a.push_str(b);
}

/// `s.length`.
pub fn len(s: &str) -> usize {
    wtf16::len(s)
}

/// `s.slice(0, n)`.
pub fn slice(s: &str, n: usize) -> String {
    wtf16::slice(s, n)
}

/// The characters `String.prototype.trim` removes (WhiteSpace and LineTerminator).
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

/// `s.trim() !== ''`, the turn's test for "wrote something".
pub fn has_text(s: &str) -> bool {
    !trim(s).is_empty()
}

/// An object from its pairs, in order.
pub fn obj<K: Into<String>>(pairs: impl IntoIterator<Item = (K, JValue)>) -> JValue {
    JValue::obj(pairs)
}

pub fn s(text: impl Into<String>) -> JValue {
    JValue::Str(text.into())
}

pub fn n(value: f64) -> JValue {
    JValue::Num(value)
}

/// `{ ...o, [key]: value }`: an existing key keeps its place, a new one goes last.
pub fn set(o: &mut JValue, key: &str, value: JValue) {
    if let JValue::Obj(items) = o {
        wtf16::set(items, key, value);
    }
}

/// `delete o[key]`.
pub fn remove(o: &mut JValue, key: &str) {
    if let JValue::Obj(items) = o {
        items.retain(|(k, _)| k != key);
    }
}

/// `JSON.stringify(v)` (lone surrogates written as JS writes them). `undefined` gives "".
pub fn stringify(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_default()
}

/// A JS number for a count or a size.
pub fn count(v: usize) -> JValue {
    // usize to f64 is exact far beyond any length the turn handles (2^53).
    #[allow(clippy::cast_precision_loss)]
    JValue::Num(v as f64)
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

/// `Number(v)` for the values a provider's JSON can hold.
pub fn to_number(v: &JValue) -> f64 {
    match v {
        JValue::Num(x) => *x,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Null => 0.0,
        JValue::Str(text) => string_to_number(text),
        // `Number([])` is 0 and `Number([x])` is `Number(String(x))`; objects are NaN.
        JValue::Arr(items) => match items.as_slice() {
            [] => 0.0,
            [one @ (JValue::Num(_) | JValue::Str(_))] => to_number(one),
            _ => f64::NAN,
        },
        JValue::Undefined | JValue::Obj(_) => f64::NAN,
    }
}

/// `StringToNumber`: JS white space trimmed, "" is 0, `Infinity` and the 0x/0o/0b forms, else a decimal literal.
pub fn string_to_number(text: &str) -> f64 {
    let t = trim(text);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = |digits: &str, base: u32| {
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
            return f64::NAN;
        }
        digits
            .chars()
            .filter_map(|c| c.to_digit(base))
            .fold(0.0, |acc, d| acc * f64::from(base) + f64::from(d))
    };
    let bytes = t.as_bytes();
    if bytes.len() > 2 && bytes.first() == Some(&b'0') {
        match bytes.get(1) {
            Some(b'x' | b'X') => return radix(t.get(2..).unwrap_or(""), 16),
            Some(b'o' | b'O') => return radix(t.get(2..).unwrap_or(""), 8),
            Some(b'b' | b'B') => return radix(t.get(2..).unwrap_or(""), 2),
            _ => {}
        }
    }
    let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (unsigned.get(..at).unwrap_or(""), unsigned.get(at + 1..)),
        None => (unsigned, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((int, frac)) => {
            (digits(int) || int.is_empty())
                && (digits(frac) || frac.is_empty())
                && !(int.is_empty() && frac.is_empty())
        }
        None => digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)));
    if !mantissa_ok || !exponent_ok {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn number_is_javascript_number() {
        for (text, want) in [
            ("", 0.0),
            (" 12 ", 12.0),
            ("0x1f", 31.0),
            ("1e3", 1000.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("-2", -2.0),
            ("Infinity", f64::INFINITY),
        ] {
            assert_eq!(string_to_number(text), want, "{text:?}");
        }
        for text in [
            "inf", "nan", "1_0", "0x", "+0x1", "1e", "--1", ".", "infinity",
        ] {
            assert!(string_to_number(text).is_nan(), "{text:?}");
        }
        assert_eq!(to_number(&JValue::Bool(true)), 1.0);
        assert!(to_number(&JValue::Undefined).is_nan());
    }

    #[test]
    fn a_split_surrogate_pair_joins_and_counts_as_js_does() {
        let parsed = wtf16::parse(r#"["\ud83d","\ude00"]"#).unwrap();
        let JValue::Arr(parts) = parsed else { panic!() };
        let mut text = parts[0].as_str().unwrap().to_string();
        assert_eq!(len(&text), 1);
        concat(&mut text, parts[1].as_str().unwrap());
        assert_eq!(text, "\u{1F600}");
        assert_eq!(len(&text), 2);
        let mut plain = String::from("a");
        concat(&mut plain, "b");
        assert_eq!(plain, "ab");
    }

    #[test]
    fn trim_is_javascript_trim() {
        assert_eq!(trim("\u{feff}\u{a0} x \u{2028}\n"), "x");
        assert!(!has_text("\u{3000}\t"));
        assert_eq!(
            trim("\u{200b}x"),
            "\u{200b}x",
            "a zero-width space is not white space"
        );
    }

    #[test]
    fn set_keeps_an_existing_key_in_place() {
        let mut o = obj([("a", n(1.0)), ("b", n(2.0))]);
        set(&mut o, "a", n(3.0));
        set(&mut o, "c", n(4.0));
        assert_eq!(stringify(&o), r#"{"a":3,"b":2,"c":4}"#);
        remove(&mut o, "b");
        assert_eq!(stringify(&o), r#"{"a":3,"c":4}"#);
    }
}
