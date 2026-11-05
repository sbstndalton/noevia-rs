//! Python-compatible text helpers: the exact behaviour of the str/regex/urllib calls the Python
//! Diary routes use, so a reply built here is byte-identical to Python's.

/// Python `str.isspace` (Unicode 15, as Python 3.12): includes U+001C..U+001F, which Rust's
/// `char::is_whitespace` does not.
pub fn is_space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{001c}'..='\u{0020}' | '\u{0085}' | '\u{00a0}'
        | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

/// Python `str.strip()` with no argument.
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// Python `re` `\d` for a str pattern: a Unicode decimal digit and its value (Python 3.12,
/// Unicode 15; same table as diary-corpus-parse).
pub fn decimal(c: char) -> Option<u32> {
    let code = c as u32;
    [
        0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66,
        0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90,
        0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0,
        0xff10, 0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0,
        0x11650, 0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16a60,
        0x16ac0, 0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0,
        0x1e950, 0x1fbf0,
    ]
    .into_iter()
    .find(|start| code >= *start && code < *start + 10)
    .map(|start| code - start)
}

/// `int()` of a run of `\d` characters: Some(value) when every char is a decimal digit and the
/// count is within `min..=max`.
pub fn digits(s: &str, min: usize, max: usize) -> Option<u32> {
    let mut count = 0usize;
    let mut value: u32 = 0;
    for c in s.chars() {
        let d = decimal(c)?;
        count += 1;
        if count > max {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(d)?;
    }
    (count >= min).then_some(value)
}

/// Starlette decodes header values (and the raw query string) as latin-1.
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|b| char::from(*b)).collect()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `urllib.parse.unquote(s)` (utf-8, errors="replace") applied to a str that came from latin-1
/// decoding: literal characters are kept, each run of %XX escapes is decoded as UTF-8.
fn unquote_latin1(raw: &[u8]) -> String {
    let mut out = String::new();
    let mut run: Vec<u8> = Vec::new();
    let mut i = 0usize;
    while i < raw.len() {
        let b = raw.get(i).copied().unwrap_or(0);
        if b == b'%' {
            if let (Some(h), Some(l)) = (
                raw.get(i + 1).copied().and_then(hex_val),
                raw.get(i + 2).copied().and_then(hex_val),
            ) {
                run.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        if !run.is_empty() {
            out.push_str(&String::from_utf8_lossy(&run));
            run.clear();
        }
        out.push(char::from(b));
        i += 1;
    }
    if !run.is_empty() {
        out.push_str(&String::from_utf8_lossy(&run));
    }
    out
}

/// Starlette's query parameters: `parse_qsl(raw.decode("latin-1"), keep_blank_values=True)`.
pub fn parse_qsl(raw: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in raw.split(|b| *b == b'&') {
        if part.is_empty() {
            continue;
        }
        let plus: Vec<u8> = part
            .iter()
            .map(|b| if *b == b'+' { b' ' } else { *b })
            .collect();
        let (name, value) = match plus.iter().position(|b| *b == b'=') {
            Some(at) => (
                plus.get(..at).unwrap_or(&[]),
                plus.get(at + 1..).unwrap_or(&[]),
            ),
            None => (plus.as_slice(), &[][..]),
        };
        out.push((unquote_latin1(name), unquote_latin1(value)));
    }
    out
}

/// Starlette `QueryParams.get(name)`: the last value given for the name.
pub fn query_get(raw: &[u8], name: &str) -> Option<String> {
    parse_qsl(raw)
        .into_iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

/// The last `n` characters (code points) of `s`, as Python `s[-n:]`.
pub fn tail_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    let skip = count - n;
    match s.char_indices().nth(skip) {
        Some((at, _)) => s.get(at..).unwrap_or(s),
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_matches_python_whitespace() {
        assert_eq!(strip("\u{1c}\u{1f} a \u{3000}"), "a");
        assert_eq!(strip("\u{200b}a"), "\u{200b}a");
    }

    #[test]
    fn query_matches_starlette() {
        assert_eq!(
            query_get(b"month=2030-01&month=2030-02", "month").as_deref(),
            Some("2030-02")
        );
        assert_eq!(query_get(b"month", "month").as_deref(), Some(""));
        assert_eq!(
            query_get(b"m%6Fnth=a+b%2", "month").as_deref(),
            Some("a b%2")
        );
        assert_eq!(
            query_get(b"month=%D9%A2", "month").as_deref(),
            Some("\u{662}")
        );
        // A raw (unescaped) high byte is a latin-1 character, not UTF-8.
        assert_eq!(
            query_get(b"month=\xd9\xa2", "month").as_deref(),
            Some("\u{d9}\u{a2}")
        );
        assert_eq!(
            query_get(b"month=%ff", "month").as_deref(),
            Some("\u{fffd}")
        );
    }

    #[test]
    fn digits_accept_unicode_decimals() {
        assert_eq!(digits("\u{662}\u{660}\u{663}\u{660}", 4, 4), Some(2030));
        assert_eq!(digits("203", 4, 4), None);
        assert_eq!(digits("²", 1, 1), None);
    }

    #[test]
    fn tail_counts_code_points() {
        assert_eq!(tail_chars("aé€b", 2), "€b");
        assert_eq!(tail_chars("ab", 5), "ab");
    }
}
