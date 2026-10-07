//! A JSON scanner over UTF-16 code units that accepts exactly what ECMAScript `JSON.parse` accepts
//! (ECMA-262 25.5.1, the ECMA-404 grammar): whitespace is only tab, LF, CR and space; strings may
//! hold any code unit from U+0020 up, including a raw lone surrogate, except `"` and `\`; escapes
//! are `\" \\ \/ \b \f \n \r \t \uXXXX`; numbers are `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`.
//! No depth limit (V8's parser is iterative too): the scanner keeps its own stack, one byte per
//! open container. Values are reported to a [`Sink`] as spans of the input; nothing is decoded
//! unless a caller asks.

/// The kind of a scalar value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scalar {
    /// `null`
    Null,
    /// `true`
    True,
    /// `false`
    False,
    /// A number lexeme.
    Number,
    /// A string lexeme, quotes included.
    String,
}

/// Receives the structure of one JSON text, in document order.
pub trait Sink {
    /// A container opens at `at` (`[` when `array`, else `{`).
    fn begin(&mut self, array: bool, at: usize);
    /// An object member's key: the string lexeme `start..end`, quotes included.
    fn key(&mut self, start: usize, end: usize);
    /// A scalar value at `start..end`.
    fn scalar(&mut self, kind: Scalar, start: usize, end: usize);
    /// The innermost open container closes; `end` is just past its bracket.
    fn end(&mut self, end: usize);
}

const fn is_ws(c: u16) -> bool {
    matches!(c, 0x20 | 0x09 | 0x0a | 0x0d)
}

fn skip_ws(s: &[u16], mut i: usize) -> usize {
    while s.get(i).is_some_and(|&c| is_ws(c)) {
        i += 1;
    }
    i
}

const fn is_hex(c: u16) -> bool {
    matches!(c, 0x30..=0x39 | 0x41..=0x46 | 0x61..=0x66)
}

/// End (exclusive) of the string lexeme starting at `i` (a `"`).
fn string_end(s: &[u16], i: usize) -> Option<usize> {
    let mut j = i + 1;
    loop {
        let c = *s.get(j)?;
        match c {
            0x22 => return Some(j + 1),
            0x5c => {
                let e = *s.get(j + 1)?;
                match e {
                    0x22 | 0x5c | 0x2f | 0x62 | 0x66 | 0x6e | 0x72 | 0x74 => j += 2,
                    0x75 => {
                        if !s.get(j + 2..j + 6)?.iter().all(|&h| is_hex(h)) {
                            return None;
                        }
                        j += 6;
                    }
                    _ => return None,
                }
            }
            c if c < 0x20 => return None,
            _ => j += 1,
        }
    }
}

fn digits(s: &[u16], mut j: usize) -> usize {
    while s.get(j).is_some_and(|&c| (0x30..=0x39).contains(&c)) {
        j += 1;
    }
    j
}

fn number_end(s: &[u16], i: usize) -> Option<usize> {
    let mut j = i;
    if s.get(j) == Some(&0x2d) {
        j += 1;
    }
    match *s.get(j)? {
        0x30 => j += 1,
        0x31..=0x39 => j = digits(s, j + 1),
        _ => return None,
    }
    if s.get(j) == Some(&0x2e) {
        let k = digits(s, j + 1);
        if k == j + 1 {
            return None;
        }
        j = k;
    }
    if matches!(s.get(j), Some(&0x65) | Some(&0x45)) {
        j += 1;
        if matches!(s.get(j), Some(&0x2b) | Some(&0x2d)) {
            j += 1;
        }
        let k = digits(s, j);
        if k == j {
            return None;
        }
        j = k;
    }
    Some(j)
}

fn literal(s: &[u16], i: usize, word: &[u8]) -> Option<usize> {
    let got = s.get(i..i + word.len())?;
    got.iter()
        .zip(word)
        .all(|(&a, &b)| a == u16::from(b))
        .then_some(i + word.len())
}

/// `"key" ws : ws` at `i`; returns where the member's value starts.
fn member_key(s: &[u16], i: usize, sink: &mut impl Sink) -> Option<usize> {
    if s.get(i) != Some(&0x22) {
        return None;
    }
    let e = string_end(s, i)?;
    sink.key(i, e);
    let j = skip_ws(s, e);
    if s.get(j) != Some(&0x3a) {
        return None;
    }
    Some(skip_ws(s, j + 1))
}

/// Scan `s` as one JSON text. `true` when `JSON.parse` would accept it; the sink has then seen the
/// whole structure (on `false` it may have seen a prefix).
pub fn parse(s: &[u16], sink: &mut impl Sink) -> bool {
    parse_inner(s, sink).is_some()
}

fn parse_inner(s: &[u16], sink: &mut impl Sink) -> Option<()> {
    // true = array, false = object
    let mut stack: Vec<bool> = Vec::new();
    let mut i = skip_ws(s, 0);
    'value: loop {
        match *s.get(i)? {
            0x7b => {
                sink.begin(false, i);
                i = skip_ws(s, i + 1);
                if s.get(i) == Some(&0x7d) {
                    i += 1;
                    sink.end(i);
                } else {
                    i = member_key(s, i, sink)?;
                    stack.push(false);
                    continue 'value;
                }
            }
            0x5b => {
                sink.begin(true, i);
                i = skip_ws(s, i + 1);
                if s.get(i) == Some(&0x5d) {
                    i += 1;
                    sink.end(i);
                } else {
                    stack.push(true);
                    continue 'value;
                }
            }
            0x22 => {
                let e = string_end(s, i)?;
                sink.scalar(Scalar::String, i, e);
                i = e;
            }
            0x74 => {
                let e = literal(s, i, b"true")?;
                sink.scalar(Scalar::True, i, e);
                i = e;
            }
            0x66 => {
                let e = literal(s, i, b"false")?;
                sink.scalar(Scalar::False, i, e);
                i = e;
            }
            0x6e => {
                let e = literal(s, i, b"null")?;
                sink.scalar(Scalar::Null, i, e);
                i = e;
            }
            0x2d | 0x30..=0x39 => {
                let e = number_end(s, i)?;
                sink.scalar(Scalar::Number, i, e);
                i = e;
            }
            _ => return None,
        }
        // After a value: close containers or move to the next member/element.
        loop {
            i = skip_ws(s, i);
            let Some(&array) = stack.last() else {
                return (i == s.len()).then_some(());
            };
            match *s.get(i)? {
                0x2c => {
                    i = skip_ws(s, i + 1);
                    if !array {
                        i = member_key(s, i, sink)?;
                    }
                    continue 'value;
                }
                0x7d if !array => {
                    stack.pop();
                    i += 1;
                    sink.end(i);
                }
                0x5d if array => {
                    stack.pop();
                    i += 1;
                    sink.end(i);
                }
                _ => return None,
            }
        }
    }
}

fn hex_value(c: u16) -> u16 {
    match c {
        0x30..=0x39 => c - 0x30,
        0x41..=0x46 => c - 0x41 + 10,
        0x61..=0x66 => c - 0x61 + 10,
        _ => 0,
    }
}

/// Decode the string lexeme `start..end` (quotes included, already validated) to code units.
pub fn decode_string(s: &[u16], start: usize, end: usize) -> Vec<u16> {
    let body = s.get(start + 1..end.saturating_sub(1)).unwrap_or(&[]);
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while let Some(&c) = body.get(i) {
        if c != 0x5c {
            out.push(c);
            i += 1;
            continue;
        }
        let e = body.get(i + 1).copied().unwrap_or(0);
        let unit = match e {
            0x62 => 0x08,
            0x66 => 0x0c,
            0x6e => 0x0a,
            0x72 => 0x0d,
            0x74 => 0x09,
            0x75 => {
                let v = body
                    .get(i + 2..i + 6)
                    .unwrap_or(&[])
                    .iter()
                    .fold(0u16, |a, &h| (a << 4) | hex_value(h));
                i += 4;
                v
            }
            other => other,
        };
        out.push(unit);
        i += 2;
    }
    out
}

/// The number of code units `decode_string` would return, without allocating.
pub fn string_units(s: &[u16], start: usize, end: usize) -> usize {
    let body = s.get(start + 1..end.saturating_sub(1)).unwrap_or(&[]);
    let mut n = 0;
    let mut i = 0;
    while let Some(&c) = body.get(i) {
        i += if c != 0x5c {
            1
        } else if body.get(i + 1) == Some(&0x75) {
            6
        } else {
            2
        };
        n += 1;
    }
    n
}

/// Whether the string lexeme `start..end` decodes to the ASCII `lit`. Long lexemes are rejected
/// without decoding (an escape is at most six units per character).
pub fn string_is(s: &[u16], start: usize, end: usize, lit: &str) -> bool {
    if end - start > 2 + 6 * lit.len() {
        return false;
    }
    decode_string(s, start, end)
        .iter()
        .copied()
        .eq(lit.bytes().map(u16::from))
}

/// The double `JSON.parse` gives the number lexeme `start..end` (correctly rounded, like V8;
/// overflow is infinite).
pub fn number_value(s: &[u16], start: usize, end: usize) -> f64 {
    let text: String = s
        .get(start..end)
        .unwrap_or(&[])
        .iter()
        .map(|&c| char::from(u8::try_from(c).unwrap_or(b'x')))
        .collect();
    text.parse::<f64>().unwrap_or(f64::NAN)
}

/// Append `units` as UTF-8, writing each lone surrogate as a `\uXXXX` escape (lowercase hex).
/// Used only where a lone surrogate can only be inside a string literal, so the result is the same
/// JSON value.
pub fn push_utf8_escaped(out: &mut Vec<u8>, units: &[u16]) {
    for r in char::decode_utf16(units.iter().copied()) {
        match r {
            Ok(c) => {
                let mut b = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
            Err(e) => push_u_escape(out, e.unpaired_surrogate()),
        }
    }
}

fn push_u_escape(out: &mut Vec<u8>, u: u16) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.extend_from_slice(b"\\u");
    for shift in [12u16, 8, 4, 0] {
        out.push(
            HEX.get(usize::from((u >> shift) & 0xf))
                .copied()
                .unwrap_or(b'0'),
        );
    }
}

/// Append `units` as a JSON string literal exactly as ECMAScript `JSON.stringify` writes it.
pub fn push_json_string(out: &mut Vec<u8>, units: &[u16]) {
    out.push(b'"');
    let mut plain: Vec<u16> = Vec::new();
    let flush = |out: &mut Vec<u8>, plain: &mut Vec<u16>| {
        push_utf8_escaped(out, plain);
        plain.clear();
    };
    let mut i = 0;
    while let Some(&c) = units.get(i) {
        let esc: Option<&[u8]> = match c {
            0x22 => Some(b"\\\""),
            0x5c => Some(b"\\\\"),
            0x08 => Some(b"\\b"),
            0x0c => Some(b"\\f"),
            0x0a => Some(b"\\n"),
            0x0d => Some(b"\\r"),
            0x09 => Some(b"\\t"),
            _ => None,
        };
        if let Some(e) = esc {
            flush(out, &mut plain);
            out.extend_from_slice(e);
        } else if c < 0x20 {
            flush(out, &mut plain);
            push_u_escape(out, c);
        } else {
            // Lone surrogates come out as \udxxx via push_utf8_escaped, as JSON.stringify does.
            plain.push(c);
        }
        i += 1;
    }
    flush(out, &mut plain);
    out.push(b'"');
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    struct Nop;
    impl Sink for Nop {
        fn begin(&mut self, _: bool, _: usize) {}
        fn key(&mut self, _: usize, _: usize) {}
        fn scalar(&mut self, _: Scalar, _: usize, _: usize) {}
        fn end(&mut self, _: usize) {}
    }

    fn ok(t: &str) -> bool {
        let u: Vec<u16> = t.encode_utf16().collect();
        parse(&u, &mut Nop)
    }

    #[test]
    fn grammar() {
        for t in [
            "0",
            "-0",
            "1e5",
            "1E+5",
            "1.5e-3",
            " [ ] ",
            "{}",
            "{\"a\":[1,{}]}",
            "\"\\u00e9\"",
            "\"\u{2028}\"",
            "null",
            "\t\r\n true",
            "[[[[]]]]",
        ] {
            assert!(ok(t), "{t}");
        }
        for t in [
            "",
            " ",
            "01",
            "1.",
            ".1",
            "+1",
            "1e",
            "-",
            "[1,]",
            "{\"a\":1,}",
            "{a:1}",
            "'a'",
            "\"\\x\"",
            "\"\\u12\"",
            "\"\t\"",
            "nul",
            "[1] x",
            "\u{feff}1",
            "NaN",
            "Infinity",
            "{\"a\"}",
            "[",
            "\u{a0}1",
        ] {
            assert!(!ok(t), "{t:?}");
        }
        let deep = "[".repeat(100_000) + &"]".repeat(100_000);
        assert!(ok(&deep));
    }

    #[test]
    fn stringify_escaping() {
        let mut out = Vec::new();
        push_json_string(
            &mut out,
            &[
                0x22, 0x5c, 0x08, 0x1f, 0x7f, 0xd800, 0x41, 0xd83d, 0xde00, 0x2028,
            ],
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\"\\\"\\\\\\b\\u001f\u{7f}\\ud800A\u{1f600}\u{2028}\""
        );
    }
}
