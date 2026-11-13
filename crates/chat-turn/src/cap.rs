//! The local engine's thinking cap on a streamed reply: noevia-core `thinking-budget.cjs`
//! `capReasoning` (#1475, #1477, #1585), as a state machine the round driver feeds chunk by chunk.
//!
//! While the reply has written no answer and no tool call, its reasoning is counted (UTF-16 units of
//! `reasoning_content` / `reasoning`, at four per token). A chunk that takes it past the cap is not
//! passed on: the stream is cut and the answer is asked for (the driver sends the forced-answer
//! request). A reply that ends, or reaches its `[DONE]`, with reasoning and nothing else gets the same
//! one forced answer. After a cut every chunk passes as it is. Between the two streams the reader gets
//! [`CUT_MARK`], which closes a line left open and restarts the idle clock.

use js_json::{wtf16, JValue};

use crate::sse::Utf8Stream;

pub const CHARS_PER_TOKEN: u64 = 4;
/// thinking-budget.cjs CUT_MARK.
pub const CUT_MARK: &[u8] = b"\n\n: thinking cap reached; answer requested\n\n";
pub const CUT_FOR_CAP: &str =
    "Thinking reached its budget, so the answer was written without further reasoning.";
pub const CUT_FOR_NO_ANSWER: &str =
    "The model stopped after thinking without an answer, so the answer was requested again.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// The reasoning passed the cap.
    Cap,
    /// The reply ended with reasoning only.
    NoAnswer,
}

impl Cut {
    /// The warning the turn shows at the cut.
    pub fn warning(self) -> &'static str {
        match self {
            Cut::Cap => CUT_FOR_CAP,
            Cut::NoAnswer => CUT_FOR_NO_ANSWER,
        }
    }
}

/// What the reader gets for one step of the upstream stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// These bytes, as they came (or the part of a chunk before a held-back `[DONE]`).
    Pass(Vec<u8>),
    /// The stream ends here.
    End,
    /// Pass `before` (if any), then stop reading this stream: the answer is requested.
    Cut { cut: Cut, before: Option<Vec<u8>> },
}

#[derive(Debug, Clone)]
pub struct Cap {
    limit_chars: u64,
    decoder: Utf8Stream,
    buffer: String,
    reasoning_chars: u64,
    answered: bool,
    forced: bool,
    ended: bool,
}

impl Cap {
    pub fn new(cap_tokens: u64) -> Self {
        Cap {
            limit_chars: cap_tokens.saturating_mul(CHARS_PER_TOKEN),
            decoder: Utf8Stream::new(),
            buffer: String::new(),
            reasoning_chars: 0,
            answered: false,
            forced: false,
            ended: false,
        }
    }

    /// Whether the cut has happened (the forced answer's stream passes untouched).
    pub fn forced(&self) -> bool {
        self.forced
    }

    fn reasoning_only(&self) -> bool {
        !self.forced && !self.answered && self.reasoning_chars > 0
    }

    // `overCap(value)`: reads the complete lines of `chunk`; true when it takes the reasoning past the cap.
    fn over_cap(&mut self, chunk: &[u8]) -> bool {
        let text = self.decoder.decode(chunk);
        self.buffer.push_str(&text);
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
            if payload == "[DONE]" {
                self.ended = true;
                continue;
            }
            let Ok(frame) = wtf16::parse(payload) else {
                continue;
            };
            let delta = first_delta(&frame);
            if !delta.is_object() {
                continue;
            }
            let tool_calls = matches!(delta.get("tool_calls"), JValue::Arr(t) if !t.is_empty());
            if delta.get("content").truthy() || tool_calls {
                self.answered = true;
            }
            let thought = if delta.get("reasoning_content").truthy() {
                delta.get("reasoning_content")
            } else {
                delta.get("reasoning")
            };
            if let Some(t) = thought.as_str() {
                self.reasoning_chars = self
                    .reasoning_chars
                    .saturating_add(u64::try_from(wtf16::len(t)).unwrap_or(u64::MAX));
            }
        }
        self.buffer = rest.to_string();
        !self.answered && self.reasoning_chars > self.limit_chars
    }

    /// One read of the stream: `Some(chunk)`, or `None` at its end.
    pub fn step(&mut self, read: Option<&[u8]>) -> Step {
        let Some(chunk) = read else {
            if !self.reasoning_only() {
                return Step::End;
            }
            self.forced = true;
            return Step::Cut {
                cut: Cut::NoAnswer,
                before: None,
            };
        };
        if self.forced {
            return Step::Pass(chunk.to_vec());
        }
        if self.over_cap(chunk) {
            self.forced = true;
            return Step::Cut {
                cut: Cut::Cap,
                before: None,
            };
        }
        if self.ended && self.reasoning_only() {
            // The [DONE] of a reasoning-only reply is held back; what came before it is passed on.
            let text = String::from_utf8_lossy(chunk);
            let before = done_line(&text)
                .filter(|at| *at > 0)
                .and_then(|at| text.as_bytes().get(..at).map(<[u8]>::to_vec));
            self.forced = true;
            return Step::Cut {
                cut: Cut::NoAnswer,
                before,
            };
        }
        Step::Pass(chunk.to_vec())
    }
}

/// `evt?.choices?.[0]?.delta`: an array's first item, or an object's `"0"` key, as JS reads `[0]`.
pub fn first_delta(frame: &JValue) -> &JValue {
    match frame.get("choices") {
        JValue::Arr(c) => c.first().map_or(&JValue::Undefined, |c| c.get("delta")),
        o @ JValue::Obj(_) => o.get("0").get("delta"),
        _ => &JValue::Undefined,
    }
}

/// `text.search(/^data:\s*\[DONE\]/m)`: the byte offset of the first line that starts with it. (JS `\s*`
/// may also cross a line break between `data:` and `[DONE]`; no engine writes that, and this does
/// not match it: a named difference with no recorded case.)
fn done_line(text: &str) -> Option<usize> {
    let mut at = 0;
    for line in text.split_inclusive(['\n', '\r', '\u{2028}', '\u{2029}']) {
        if let Some(rest) = line.strip_prefix("data:") {
            if rest
                .trim_start_matches(crate::js::is_js_space)
                .starts_with("[DONE]")
            {
                return Some(at);
            }
        }
        at += line.len();
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn frame(delta: &str) -> Vec<u8> {
        format!("data: {{\"choices\":[{{\"delta\":{delta}}}]}}\n\n").into_bytes()
    }

    #[test]
    fn reasoning_past_the_cap_is_cut_before_the_chunk_that_crosses_it() {
        let mut cap = Cap::new(2);
        assert_eq!(
            cap.step(Some(&frame(r#"{"reasoning_content":"1234"}"#))),
            Step::Pass(frame(r#"{"reasoning_content":"1234"}"#))
        );
        assert_eq!(
            cap.step(Some(&frame(r#"{"reasoning":"56789"}"#))),
            Step::Cut {
                cut: Cut::Cap,
                before: None
            }
        );
        assert!(cap.forced());
        assert_eq!(cap.step(Some(b"x")), Step::Pass(b"x".to_vec()));
        assert_eq!(cap.step(None), Step::End);
    }

    #[test]
    fn an_answer_lifts_the_cap_and_reasoning_only_asks_once() {
        let mut cap = Cap::new(1);
        cap.step(Some(&frame(r#"{"content":"a"}"#)));
        assert!(matches!(
            cap.step(Some(&frame(r#"{"reasoning":"long reasoning"}"#))),
            Step::Pass(_)
        ));
        assert_eq!(cap.step(None), Step::End);
        let mut only = Cap::new(100);
        only.step(Some(&frame(r#"{"reasoning":"x"}"#)));
        assert_eq!(
            only.step(None),
            Step::Cut {
                cut: Cut::NoAnswer,
                before: None
            }
        );
    }

    #[test]
    fn choices_as_an_object_are_read_as_js_reads_index_zero() {
        let frame = js_json::parse(r#"{"choices":{"0":{"delta":{"content":"a"}}}}"#).unwrap();
        assert_eq!(first_delta(&frame).get("content").as_str(), Some("a"));
        let none = js_json::parse(r#"{"choices":"x"}"#).unwrap();
        assert!(matches!(first_delta(&none), JValue::Undefined));
    }

    #[test]
    fn a_reasoning_only_done_is_held_back() {
        let mut cap = Cap::new(100);
        cap.step(Some(&frame(r#"{"reasoning":"x"}"#)));
        let tail = b"data: {\"choices\":[]}\n\ndata: [DONE]\n\n";
        assert_eq!(
            cap.step(Some(tail)),
            Step::Cut {
                cut: Cut::NoAnswer,
                before: Some(b"data: {\"choices\":[]}\n\n".to_vec())
            }
        );
        let mut first = Cap::new(100);
        first.step(Some(&frame(r#"{"reasoning":"x"}"#)));
        assert_eq!(
            first.step(Some(b"data: [DONE]\n\n")),
            Step::Cut {
                cut: Cut::NoAnswer,
                before: None
            }
        );
    }
}
