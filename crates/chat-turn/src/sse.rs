//! Reading a model's event stream as chat.cjs reads it (lines 2072-2153): the bytes decoded as a
//! streaming `TextDecoder` decodes them, split into lines on `\n`, each line trimmed, and only
//! `data:` lines with a payload other than `[DONE]` kept. A line split across chunks keeps its
//! leading fragment until the rest arrives.

/// A streaming UTF-8 decoder with the WHATWG replacement rules (`new TextDecoder()` with
/// `{ stream: true }`): an incomplete sequence at a chunk's end waits for the next chunk, every
/// maximal invalid subsequence becomes one U+FFFD, and a leading byte-order mark is dropped.
#[derive(Debug, Default, Clone)]
pub struct Utf8Stream {
    pending: Vec<u8>,
    started: bool,
}

impl Utf8Stream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes `chunk`, keeping an unfinished trailing sequence for the next call.
    pub fn decode(&mut self, chunk: &[u8]) -> String {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(chunk);
        let mut out = String::with_capacity(bytes.len());
        let mut rest: &[u8] = &bytes;
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    break;
                }
                Err(e) => {
                    let (good, after) = rest.split_at(e.valid_up_to());
                    // valid_up_to marks a char boundary of valid UTF-8.
                    out.push_str(std::str::from_utf8(good).unwrap_or_default());
                    match e.error_len() {
                        Some(bad) => {
                            out.push('\u{FFFD}');
                            rest = after.get(bad..).unwrap_or_default();
                        }
                        None => {
                            // The input ends inside a sequence that may still complete.
                            self.pending = after.to_vec();
                            break;
                        }
                    }
                }
            }
        }
        if !self.started && !out.is_empty() {
            self.started = true;
            if let Some(stripped) = out.strip_prefix('\u{FEFF}') {
                return stripped.to_string();
            }
        }
        out
    }
}

/// The SSE line reader of one model round. The decoder is the exchange's (chat.cjs makes one
/// `TextDecoder` per turn): bytes a round left unfinished, and whether a byte-order mark was seen,
/// carry over into the next round, as there.
#[derive(Debug, Default, Clone)]
pub struct Lines {
    buffer: String,
}

impl Lines {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `data:` payloads (trimmed, `[DONE]` left out) completed by `chunk`, in order.
    pub fn feed(&mut self, decoder: &mut Utf8Stream, chunk: &[u8]) -> Vec<String> {
        let text = decoder.decode(chunk);
        self.buffer.push_str(&text);
        let mut out = Vec::new();
        // One pass over the lines; what is left after the last newline waits for the next chunk.
        let buffer = std::mem::take(&mut self.buffer);
        let mut rest = buffer.as_str();
        while let Some(at) = rest.find('\n') {
            let (line, after) = rest.split_at(at);
            rest = after.get(1..).unwrap_or("");
            let line = crate::js::trim(line);
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = crate::js::trim(payload);
            if payload != "[DONE]" {
                out.push(payload.to_string());
            }
        }
        self.buffer = rest.to_string();
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn a_character_split_between_chunks_is_carried_over() {
        let bytes = "a\u{1F600}b".as_bytes();
        let mut d = Utf8Stream::new();
        assert_eq!(d.decode(&bytes[..3]), "a");
        assert_eq!(d.decode(&bytes[3..]), "\u{1F600}b");
    }

    #[test]
    fn invalid_bytes_become_one_replacement_each_as_whatwg_says() {
        let mut d = Utf8Stream::new();
        assert_eq!(d.decode(b"a\xffb\xe2\x82c"), "a\u{FFFD}b\u{FFFD}c");
        let mut bom = Utf8Stream::new();
        assert_eq!(bom.decode(b"\xef\xbb\xbfx"), "x");
        assert_eq!(bom.decode("\u{FEFF}y".as_bytes()), "\u{FEFF}y");
    }

    #[test]
    fn many_lines_in_one_chunk_are_read_in_one_pass() {
        let (mut l, mut d) = (Lines::new(), Utf8Stream::new());
        let chunk = "data: x\n".repeat(200_000);
        let started = std::time::Instant::now();
        assert_eq!(l.feed(&mut d, chunk.as_bytes()).len(), 200_000);
        assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
    }

    #[test]
    fn lines_keep_their_fragment_and_skip_what_is_not_data() {
        let (mut l, mut d) = (Lines::new(), Utf8Stream::new());
        assert!(l.feed(&mut d, b": ping\n\ndata: {\"a\"").is_empty());
        assert_eq!(
            l.feed(&mut d, b":1}\n\ndata: [DONE]\n\n  data:x \r\nevent: y\n"),
            vec!["{\"a\":1}", "x"]
        );
    }
}
