//! Core's side of a handed-off turn, as the front reads it: the NDJSON core writes for the turn's
//! preparation on the front's wire (chat port S7, core rust-chat-wire.cjs; rust-chat-native.cjs),
//! passed on to the front's event-stream writer until the one `prepared` line, which is never passed
//! on but handed to the native executor.
//!
//! Core writes one JSON value per line (`JSON.stringify(value) + "\n"`; `JSON.stringify` never writes
//! a raw line break). The `prepared` line is the one that starts with `{"type":"prepared"`
//! (rust-chat-native.cjs `preparedTurn` puts `type` first); no other line can (every other object
//! line is an event of another type, a string line starts with `"`).
//!
//! Bounded: only the start of a line is held, until it is known not to be the `prepared` line (then it
//! and the rest of that line pass on as they come), or the `prepared` line itself, up to
//! [`PREPARED_CAP`] bytes.

use js_json::{wtf16, JValue};

const PREPARED: &[u8] = b"{\"type\":\"prepared\"";
/// The most bytes the `prepared` line may take: the wire messages of a chat body at its 32 MiB cap,
/// with room for the tools and the rest.
pub const PREPARED_CAP: usize = 40 * 1024 * 1024;

/// The header the front names its executor's capabilities in when it asks for a hand-off (core
/// rust-chat-native.cjs `CAPS_HEADER`; read only with the hand-off secret).
pub const HANDOFF_CAPS_HEADER: &str = "x-noevia-chat-handoff-caps";
/// What this executor runs beyond the first hand-off step, for [`HANDOFF_CAPS_HEADER`]: `skills`, the
/// instruction skills checks ([`crate::skills`]); `once`, the coworker page save's per-call card (#2446,
/// [`crate::gate`]). Without them core keeps a turn whose project has skills, or that offers the page save.
pub const HANDOFF_CAPS: &str = "skills,once";

/// What a chunk of core's stream gives.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    /// Bytes to pass on, as core wrote them (not always whole lines: the writer reassembles them).
    Forward(Vec<u8>),
    /// The `prepared` line, parsed; nothing after it is core's to send.
    Prepared(JValue),
    /// A `prepared` line that does not parse or is too long: the turn cannot go on.
    Unreadable,
}

/// Splits core's stream at the `prepared` line.
#[derive(Debug, Default, Clone)]
pub struct Frames {
    /// The start of the current line, while it may be the `prepared` line.
    held: Vec<u8>,
    /// Inside a line known not to be the `prepared` line: its bytes pass on until its end.
    passing: bool,
    done: bool,
}

impl Frames {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the `prepared` line has been seen (later bytes are dropped).
    pub fn handed_off(&self) -> bool {
        self.done
    }

    /// The pieces this chunk gives, in order.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Piece> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        let mut forward = Vec::new();
        let mut rest = chunk;
        while !rest.is_empty() {
            if self.passing {
                match rest.iter().position(|b| *b == b'\n') {
                    Some(at) => {
                        let (line, after) = rest.split_at(at + 1);
                        forward.extend_from_slice(line);
                        self.passing = false;
                        rest = after;
                    }
                    None => {
                        forward.extend_from_slice(rest);
                        break;
                    }
                }
                continue;
            }
            // At the start of a line, or holding one that may be the `prepared` line.
            let end = rest.iter().position(|b| *b == b'\n');
            let take = end.map_or(rest.len(), |at| at + 1);
            let (part, after) = rest.split_at(take);
            self.held.extend_from_slice(part);
            rest = after;
            let probe = self.held.len().min(PREPARED.len());
            let maybe = self.held.get(..probe) == PREPARED.get(..probe);
            if !maybe {
                forward.append(&mut self.held);
                self.passing = end.is_none();
                continue;
            }
            if end.is_some() {
                if self.held.starts_with(PREPARED) {
                    if !forward.is_empty() {
                        out.push(Piece::Forward(std::mem::take(&mut forward)));
                    }
                    let line = self.held.strip_suffix(b"\n").unwrap_or(&self.held);
                    let parsed = std::str::from_utf8(line)
                        .ok()
                        .and_then(|t| wtf16::parse(t).ok());
                    out.push(parsed.map_or(Piece::Unreadable, Piece::Prepared));
                    self.done = true;
                    self.held = Vec::new();
                    return out;
                }
                // A short line that is a prefix of the marker (not an event): passes on.
                forward.append(&mut self.held);
                continue;
            }
            if self.held.len() > PREPARED_CAP {
                if !forward.is_empty() {
                    out.push(Piece::Forward(std::mem::take(&mut forward)));
                }
                out.push(Piece::Unreadable);
                self.done = true;
                self.held = Vec::new();
                return out;
            }
        }
        if !forward.is_empty() {
            out.push(Piece::Forward(forward));
        }
        out
    }

    /// Bytes held when core's stream ended (passed on as they are: the writer decides what they are).
    pub fn rest(self) -> Vec<u8> {
        if self.done {
            Vec::new()
        } else {
            self.held
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn forwarded(pieces: &[Piece]) -> Vec<u8> {
        pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Forward(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn lines_pass_on_until_the_prepared_one_whatever_the_chunking() {
        let stream = b"{\"type\":\"meta\",\"model\":\"m\"}\n\"data: {}\\n\\n\"\n{\"type\":\"prepared\",\"v\":1}\n{\"type\":\"late\"}\n";
        for cut in 0..=stream.len() {
            for cut2 in cut..=stream.len() {
                let mut f = Frames::new();
                let mut pieces = f.feed(&stream[..cut]);
                pieces.extend(f.feed(&stream[cut..cut2]));
                pieces.extend(f.feed(&stream[cut2..]));
                assert_eq!(
                    forwarded(&pieces),
                    b"{\"type\":\"meta\",\"model\":\"m\"}\n\"data: {}\\n\\n\"\n".to_vec(),
                    "cuts {cut} {cut2}"
                );
                let prepared: Vec<&Piece> = pieces
                    .iter()
                    .filter(|p| matches!(p, Piece::Prepared(_)))
                    .collect();
                assert_eq!(prepared.len(), 1, "cuts {cut} {cut2}");
                assert!(f.handed_off());
                assert!(f.rest().is_empty());
            }
        }
    }

    #[test]
    fn a_turn_core_ran_itself_passes_whole_and_an_unreadable_line_is_said() {
        let mut f = Frames::new();
        let pieces = f.feed(b"{\"type\":\"done\"}\n{\"type\":\"ti");
        assert_eq!(
            pieces,
            vec![Piece::Forward(
                b"{\"type\":\"done\"}\n{\"type\":\"ti".to_vec()
            )]
        );
        assert!(!f.handed_off());
        // A held start that could still be the marker comes back at the end.
        let mut g = Frames::new();
        assert!(g.feed(b"{\"type\":\"pre").is_empty());
        assert_eq!(g.rest(), b"{\"type\":\"pre".to_vec());
        let mut bad = Frames::new();
        assert_eq!(
            bad.feed(b"{\"type\":\"prepared\",x}\n"),
            vec![Piece::Unreadable]
        );
    }

    #[test]
    fn a_prepared_line_of_exactly_the_cap_is_read() {
        let mut line = PREPARED.to_vec();
        line.extend_from_slice(b",\"pad\":\"");
        let body = PREPARED_CAP - line.len() - 2;
        line.extend(std::iter::repeat_n(b'x', body));
        line.extend_from_slice(b"\"}");
        assert_eq!(line.len(), PREPARED_CAP);
        line.push(b'\n');
        let mut f = Frames::new();
        let mut pieces = Vec::new();
        for chunk in line.chunks(64 * 1024) {
            pieces.extend(f.feed(chunk));
        }
        assert!(
            matches!(pieces.as_slice(), [Piece::Prepared(_)]),
            "{:?}",
            pieces.len()
        );
    }

    #[test]
    fn a_long_event_line_is_not_held_and_a_too_long_prepared_line_is_refused() {
        let mut f = Frames::new();
        let mut line = b"{\"type\":\"delta\",\"text\":\"".to_vec();
        line.extend(std::iter::repeat_n(b'x', 1 << 20));
        let pieces = f.feed(&line);
        assert_eq!(forwarded(&pieces), line);
        assert!(f.held.is_empty());
        let mut g = Frames::new();
        let mut big = PREPARED.to_vec();
        big.extend(std::iter::repeat_n(b'x', PREPARED_CAP));
        assert_eq!(g.feed(&big), vec![Piece::Unreadable]);
        assert!(g.handed_off());
    }
}
