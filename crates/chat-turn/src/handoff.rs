//! Core's side of a handed-off turn, as the front reads it: the event stream core writes for the
//! turn's preparation (rust-chat-native.cjs), forwarded frame by frame to the client until the one
//! `prepared` frame, which is never forwarded but handed to the native executor.
//!
//! Core writes every frame as `data: <JSON.stringify(event)>\n\n` or `: keep-alive\n\n` (chat.cjs
//! `send`, the heartbeat). `JSON.stringify` never writes a raw line break, so a frame ends exactly at
//! the first blank line, whatever the chunking. The `prepared` frame is the one whose JSON starts with
//! `{"type":"prepared"` (rust-chat-native.cjs `preparedTurn` puts `type` first).

use js_json::{wtf16, JValue};

const PREPARED: &[u8] = b"data: {\"type\":\"prepared\"";

/// What a chunk of core's stream gives.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    /// Whole frames to forward to the client, as core wrote them.
    Forward(Vec<u8>),
    /// The `prepared` frame, parsed; nothing after it is core's to send.
    Prepared(JValue),
    /// A `prepared` frame that does not parse: the turn cannot go on (core sent it, so it is a bug).
    Unreadable,
}

/// Splits core's stream into frames.
#[derive(Debug, Default, Clone)]
pub struct Frames {
    buf: Vec<u8>,
    done: bool,
}

impl Frames {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the `prepared` frame has been seen (later bytes are dropped).
    pub fn handed_off(&self) -> bool {
        self.done
    }

    /// The pieces this chunk completes, in order.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Piece> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        self.buf.extend_from_slice(chunk);
        let mut forward = Vec::new();
        let mut start = 0;
        while let Some(at) = find(&self.buf, start, b"\n\n") {
            let end = at + 2;
            let frame = self.buf.get(start..end).unwrap_or_default();
            if frame.starts_with(PREPARED) {
                if !forward.is_empty() {
                    out.push(Piece::Forward(std::mem::take(&mut forward)));
                }
                // `data: ` is six bytes; `at` is where the blank line starts, in the buffer.
                let json = self.buf.get(start + 6..at).unwrap_or_default();
                let parsed = std::str::from_utf8(json)
                    .ok()
                    .and_then(|t| wtf16::parse(t).ok());
                out.push(parsed.map_or(Piece::Unreadable, Piece::Prepared));
                self.done = true;
                self.buf.clear();
                return out;
            }
            forward.extend_from_slice(frame);
            start = end;
        }
        self.buf.drain(..start);
        if !forward.is_empty() {
            out.push(Piece::Forward(forward));
        }
        out
    }

    /// Bytes left without their blank line when core's stream ended (forwarded as they are).
    pub fn rest(self) -> Vec<u8> {
        if self.done {
            Vec::new()
        } else {
            self.buf
        }
    }
}

fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_forwarded_until_the_prepared_one_whatever_the_chunking() {
        let stream = b"data: {\"type\":\"meta\",\"model\":\"m\"}\n\n: keep-alive\n\ndata: {\"type\":\"prepared\",\"v\":1}\n\ndata: {\"type\":\"late\"}\n\n";
        for cut in 1..stream.len() {
            let mut f = Frames::new();
            let mut pieces = f.feed(&stream[..cut]);
            pieces.extend(f.feed(&stream[cut..]));
            let forwarded: Vec<u8> = pieces
                .iter()
                .filter_map(|p| match p {
                    Piece::Forward(b) => Some(b.clone()),
                    _ => None,
                })
                .flatten()
                .collect();
            assert_eq!(
                forwarded,
                b"data: {\"type\":\"meta\",\"model\":\"m\"}\n\n: keep-alive\n\n".to_vec(),
                "cut {cut}"
            );
            let prepared: Vec<&Piece> = pieces
                .iter()
                .filter(|p| matches!(p, Piece::Prepared(_)))
                .collect();
            assert_eq!(prepared.len(), 1, "cut {cut}");
            assert!(f.handed_off());
            assert!(f.rest().is_empty());
        }
    }

    #[test]
    fn a_turn_core_ran_itself_passes_whole_and_an_unreadable_frame_is_said() {
        let mut f = Frames::new();
        let pieces = f.feed(b"data: {\"type\":\"done\"}\n\ndata: {\"type\":\"ti");
        assert_eq!(
            pieces,
            vec![Piece::Forward(b"data: {\"type\":\"done\"}\n\n".to_vec())]
        );
        assert!(!f.handed_off());
        assert_eq!(f.rest(), b"data: {\"type\":\"ti".to_vec());
        let mut bad = Frames::new();
        assert_eq!(
            bad.feed(b"data: {\"type\":\"prepared\",x}\n\n"),
            vec![Piece::Unreadable]
        );
    }
}
