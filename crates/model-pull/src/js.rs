//! The JavaScript built-ins the two handlers lean on, with their exact semantics: `encodeURIComponent`,
//! `Date.prototype.toISOString`, `URLSearchParams.get` over a request target's query, the relational
//! `>` against a number, and the ASCII-only case folding of a non-Unicode `/i` regular expression.

use js_json::{wtf16, JValue};
use std::fmt::Write as _;

/// `encodeURIComponent(s)`; `None` where JS throws `URIError: URI malformed` (a lone surrogate).
pub fn encode_uri_component(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if wtf16::lone_unit(c).is_some() {
            return None;
        }
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    Some(out)
}

/// The message of the `URIError` `encodeURIComponent` throws.
pub const URI_MALFORMED: &str = "URI malformed";

/// `new Date(ms).toISOString()` for a time value inside the JS range (±8.64e15 ms); outside it JS
/// throws, which a host clock never reaches.
pub fn iso_string(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let in_day = ms.rem_euclid(86_400_000);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", -year)
    } else {
        format!("+{year:06}")
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000
    )
}

/// `new URL(target, 'http://localhost').searchParams.get(name)` for the request target's query
/// (`query`: what follows the first `?` of the target, as sent). The URL parser first drops C0
/// controls and spaces at the end of the target and every tab and newline in it, and a `#` starts
/// the fragment; the form decoding then reads `+` as a space and `%XX` as a byte, the bytes as
/// UTF-8 (U+FFFD where they are not). The first pair with the name wins.
pub fn search_param(query: &str, name: &str) -> Option<String> {
    let trimmed = query.trim_end_matches(|c: char| c <= ' ');
    let cleaned: String = trimmed
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let query = cleaned.split('#').next().unwrap_or("");
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if form_decode(key) == name {
            return Some(form_decode(value));
        }
    }
    None
}

fn form_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let hex = |at: usize| bytes.get(at).and_then(|h| (*h as char).to_digit(16));
        match b {
            b'+' => out.push(b' '),
            b'%' => match (hex(i + 1), hex(i + 2)) {
                (Some(hi), Some(lo)) => {
                    out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            _ => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `v > n` for the values a GGUF inspection keeps (a number, a string, `null`).
pub fn greater_than(v: &JValue, n: f64) -> bool {
    let x = match v {
        JValue::Num(x) => *x,
        JValue::Str(s) => chat_turn::js::string_to_number(s),
        JValue::Null => 0.0,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        _ => f64::NAN,
    };
    x > n
}

/// Whether `text` starts with the ASCII `prefix`, ignoring ASCII case (a non-Unicode `/i` regular
/// expression folds nothing outside ASCII onto ASCII letters).
pub fn starts_with_ci(text: &[u8], prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text
            .iter()
            .zip(prefix.bytes())
            .all(|(a, b)| a.eq_ignore_ascii_case(&b))
}

/// `[\w]`: an ASCII letter, digit or underscore.
pub fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `/^[\w.-]+\/[\w.-]+$/` (the route's and llamacpp-variants.cjs's repository test).
pub fn is_repo(s: &str) -> bool {
    let part = |p: &str| !p.is_empty() && p.bytes().all(|b| is_word(b) || b == b'.' || b == b'-');
    match s.split_once('/') {
        Some((owner, name)) => part(owner) && part(name),
        None => false,
    }
}

/// `String(v)` (`None`: JS throws `TypeError: Cannot convert object to primitive value`).
pub fn to_string(v: &JValue) -> Option<String> {
    js_json::to_js_string(v).ok()
}

/// The message of that `TypeError`.
pub const NO_PRIMITIVE: &str = "Cannot convert object to primitive value";

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn iso_strings_are_javascript_s() {
        assert_eq!(iso_string(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_string(1_794_744_000_123), "2026-11-15T12:00:00.123Z");
        assert_eq!(iso_string(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(iso_string(253_402_300_800_000), "+010000-01-01T00:00:00.000Z");
        assert_eq!(iso_string(-62_198_755_200_000), "-000001-01-01T00:00:00.000Z");
        assert_eq!(iso_string(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn search_params_are_url_search_params() {
        assert_eq!(search_param("repo=a%2Fb&repo=c", "repo").unwrap(), "a/b");
        assert_eq!(search_param("a+b=c+d", "a b").unwrap(), "c d");
        assert_eq!(search_param("x=%zz%4", "x").unwrap(), "%zz%4");
        assert_eq!(search_param("x=%FF", "x").unwrap(), "\u{FFFD}");
        assert_eq!(search_param("x=1#y=2&z", "z"), None);
        assert_eq!(search_param("x=a\tb ", "x").unwrap(), "ab");
        assert_eq!(search_param("x", "x").unwrap(), "");
    }

    #[test]
    fn encode_uri_component_matches_javascript() {
        assert_eq!(
            encode_uri_component("a b/é😀!~*'()").unwrap(),
            "a%20b%2F%C3%A9%F0%9F%98%80!~*'()"
        );
        assert_eq!(encode_uri_component("\u{10F800}"), None);
    }
}
