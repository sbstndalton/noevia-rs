//! `lines()` from noevia-core `code-sandbox/pi-acp-bridge.cjs`: strict LF framing of the
//! sandboxed agent's (and the client's) JSONL stream.
//!
//! JS semantics reproduced exactly:
//! - each chunk is appended to the buffer first; if the buffer's length in UTF-16 code units then
//!   exceeds the limit, the whole buffer (complete lines in it included) is dropped and the
//!   caller is told the size, and framing carries on from an empty buffer;
//! - otherwise every `\n`-terminated line is taken, one trailing `\r` stripped;
//! - a line that is empty after JS `String.prototype.trim()` is skipped;
//! - a line `JSON.parse` would refuse is skipped;
//! - the rest stays buffered.
//!
//! Lines are returned as text; the host runs `JSON.parse` on each, so a message's value (duplicate
//! keys, numbers, `__proto__`) is the JS one by construction, and [`crate::json::validate`]
//! decides only which lines are skipped.

use crate::json::validate;

/// pi-acp-bridge.cjs's default line-buffer limit, in UTF-16 code units.
pub const DEFAULT_LIMIT: usize = 16 * 1024 * 1024;

/// What one chunk produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pushed {
    /// The complete JSON lines, in order.
    Lines(Vec<String>),
    /// The buffer reached this many UTF-16 code units and was dropped.
    Overflow(usize),
}

/// One framing state (one `lines()` closure).
#[derive(Debug, Clone)]
pub struct Framer {
    limit: usize,
    buffer: String,
    /// `buffer`'s length in UTF-16 code units.
    units: usize,
}

/// JS `String.prototype.trim()` whitespace: WhiteSpace and LineTerminator.
pub const fn js_space(c: char) -> bool {
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

fn units(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

impl Framer {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            buffer: String::new(),
            units: 0,
        }
    }

    /// Drop the buffer; its length in UTF-16 code units. (A host that sees a chunk longer than
    /// the limit knows it overflows: `size = reset() + chunk.length`, as JS would report.)
    pub fn reset(&mut self) -> usize {
        let size = self.units;
        self.buffer = String::new();
        self.units = 0;
        size
    }

    /// Feed one chunk.
    pub fn push(&mut self, chunk: &str) -> Pushed {
        self.buffer.push_str(chunk);
        self.units += units(chunk);
        if self.units > self.limit {
            let size = self.units;
            self.buffer = String::new();
            self.units = 0;
            return Pushed::Overflow(size);
        }
        let mut out = Vec::new();
        let mut consumed = 0usize;
        while let Some(rel) = self.buffer.get(consumed..).and_then(|s| s.find('\n')) {
            let end = consumed + rel;
            let raw = self.buffer.get(consumed..end).unwrap_or("");
            self.units -= units(raw) + 1;
            consumed = end + 1;
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if line.trim_matches(js_space).is_empty() || !validate(line) {
                continue;
            }
            out.push(line.to_owned());
        }
        if consumed > 0 {
            self.buffer.drain(..consumed);
            if self.buffer.capacity() > 1024 * 1024 && self.buffer.len() < 64 * 1024 {
                self.buffer.shrink_to_fit();
            }
        }
        Pushed::Lines(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_crlf_empty_invalid() {
        let mut f = Framer::new(DEFAULT_LIMIT);
        assert_eq!(f.push("{\"a\":"), Pushed::Lines(vec![]));
        assert_eq!(
            f.push("1}\r\n\r\n \u{feff}\nnot json\n[2]\r\r\n3"),
            Pushed::Lines(vec!["{\"a\":1}".to_owned(), "[2]\r".to_owned()])
        );
        assert_eq!(f.push("\n"), Pushed::Lines(vec!["3".to_owned()]));
    }

    #[test]
    fn overflow_counts_utf16_and_resets() {
        let mut f = Framer::new(4);
        assert_eq!(f.push("1\n😀😀"), Pushed::Overflow(6));
        assert_eq!(f.push("2\n"), Pushed::Lines(vec!["2".to_owned()]));
    }
}
