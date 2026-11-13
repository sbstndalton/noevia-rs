//! The untrusted-text helpers of code-verify.cjs: `tailUtf8`, `cleanTail`, and the incremental
//! UTF-8 decoding Node's `socket.setEncoding('utf8')` does.

use js_json::wtf16;

/// code-verify.cjs `tailUtf8`: the last `cap` bytes of `text`, cut forward to a character boundary.
///
/// A lone surrogate (held as a mapped character, see [`js_json::wtf16`]) is written as U+FFFD, as
/// `Buffer.from(text, 'utf8')` does.
pub fn tail_utf8(text: &str, cap: usize) -> String {
    let s: String = text
        .chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect();
    if s.len() <= cap {
        return s;
    }
    let mut start = s.len().saturating_sub(cap);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s.get(start..).unwrap_or_default().to_string()
}

/// code-verify.cjs `cleanTail`: CRLF and CR become LF, the control characters
/// U+0000..U+0008, U+000B..U+001F and U+007F..U+009F are removed (tab and LF stay), then the
/// last `cap` bytes are kept ([`tail_utf8`]).
pub fn clean_tail(text: &str, cap: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => {}
            c => out.push(c),
        }
    }
    tail_utf8(&out, cap)
}

/// Node's `StringDecoder('utf8')` as a stream's `setEncoding` uses it: an incomplete sequence at
/// the end of a chunk waits for the next one, an invalid one becomes U+FFFD (one per maximal
/// invalid subpart), and at the end of the stream an incomplete one becomes one U+FFFD.
#[derive(Default)]
pub(crate) struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub(crate) fn push(&mut self, chunk: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(chunk);
        let mut out = String::with_capacity(buf.len());
        let mut rest: &[u8] = &buf;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    break;
                }
                Err(e) => {
                    let good = rest.get(..e.valid_up_to()).unwrap_or_default();
                    out.push_str(&String::from_utf8_lossy(good));
                    let after = rest.get(e.valid_up_to()..).unwrap_or_default();
                    match e.error_len() {
                        None => {
                            self.pending = after.to_vec();
                            break;
                        }
                        Some(n) => {
                            out.push('\u{FFFD}');
                            rest = after.get(n..).unwrap_or_default();
                        }
                    }
                }
            }
        }
        out
    }

    pub(crate) fn finish(&mut self) -> String {
        if std::mem::take(&mut self.pending).is_empty() {
            String::new()
        } else {
            "\u{FFFD}".to_string()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn clean_tail_normalises_line_ends_and_strips_controls() {
        let raw = "a\r\nb\rc\u{0}d\u{1b}[31me\u{7f}f\u{85}g\th\n\u{2028}i\u{b}\u{c}j\r\r\nk";
        assert_eq!(clean_tail(raw, 1000), "a\nb\ncd[31mefg\th\n\u{2028}ij\n\nk");
    }

    #[test]
    fn tail_utf8_cuts_on_a_character_boundary() {
        let t = "€".repeat(400);
        let cut = tail_utf8(&t, 1024);
        assert_eq!(cut.len(), 1023);
        assert!(cut.chars().all(|c| c == '€'));
        assert_eq!(tail_utf8("abc", 3), "abc");
        assert_eq!(tail_utf8("abc", 2), "bc");
        assert_eq!(tail_utf8("abc", 0), "");
        assert_eq!(tail_utf8("é", 1), "");
    }

    #[test]
    fn lone_surrogates_become_replacement_characters() {
        let lone = wtf16::lone_char(0xD83D).unwrap();
        assert_eq!(clean_tail(&format!("a{lone}b"), 100), "a\u{FFFD}b");
    }

    #[test]
    fn decoder_holds_back_an_incomplete_sequence() {
        let mut d = Utf8Stream::default();
        assert_eq!(d.push(&[b'a', 0xE2]), "a");
        assert_eq!(d.push(&[0x82]), "");
        assert_eq!(d.push(&[0xAC, b'b']), "€b");
        assert_eq!(d.finish(), "");
        assert_eq!(d.push(&[0xE2]), "");
        assert_eq!(d.finish(), "\u{FFFD}");
    }

    #[test]
    fn decoder_replaces_invalid_bytes() {
        let mut d = Utf8Stream::default();
        assert_eq!(
            d.push(&[0xFF, b'a', 0xE2, 0x82, b'b']),
            "\u{FFFD}a\u{FFFD}b"
        );
    }
}
