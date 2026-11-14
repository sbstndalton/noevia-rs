//! The JavaScript coercions the Node code applies to values a request or the environment can hold.

pub use embedding_admin::{js_string, js_truthy};

/// JavaScript's WhiteSpace and LineTerminator code points: what `String.prototype.trim` strips and
/// what the regular expression class `\s` matches. Not Rust's `char::is_whitespace` (that one
/// counts U+0085 and not U+FEFF).
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn trim(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// The length JavaScript reports (UTF-16 code units).
pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// `Number(text)` for a string (the environment's values): blank is 0, `Infinity` with a sign,
/// `0x`/`0o`/`0b` integers, else a decimal literal; anything else is NaN.
pub fn number(text: &str) -> f64 {
    let t = trim(text);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = t.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return digits
                .chars()
                .filter_map(|c| c.to_digit(radix))
                .fold(0.0, |n, d| n * f64::from(radix) + f64::from(d));
        }
    }
    // A decimal literal: digits, one dot, an exponent, a leading sign. Rust's parser would also
    // take "inf", "nan" and "infinity", which JavaScript does not.
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    let ok = !body.is_empty()
        && body.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        && body.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '.');
    if !ok {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `Math.round`: halves go up (towards +Infinity).
pub fn round(n: f64) -> f64 {
    if !n.is_finite() {
        return n;
    }
    let f = n.floor();
    if n - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}
