//! The interrupted-turn record: what a reply had streamed when it stopped, kept so a reload can
//! show it with Retry and Continue (noevia-core's chat-partial.cjs, #1507).
//!
//! The record is built from the same events the client saw ([`Partial::observe`]), so it can
//! never hold text the client did not get. Each text field keeps its newest [`FIELD_CAP_CHARS`]
//! characters, as chat-partial.cjs keeps its newest UTF-16 units there; `truncated` says a field
//! was cut. A completed or failed reply clears the record; a reply whose client left keeps it in
//! the transcript ([`Outcome::disposition`]).

use crate::event::Event;
use crate::tools::cap_result;
use serde_json::{json, Value};

/// Most characters kept per text field (chat-partial.cjs `PARTIAL_CAP_CHARS`).
pub const FIELD_CAP_CHARS: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStep {
    pub index: usize,
    pub name: String,
    pub result: Option<String>,
}

/// The partial reply of one turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Partial {
    pub content: String,
    pub reasoning: String,
    pub tools: Vec<ToolStep>,
    pub truncated: bool,
}

/// How the turn ended, as far as the record is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Failed,
    /// The client disconnected before the reply ended.
    Left,
}

/// What happens to the record when the turn ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Removed; the reply is the transcript now (or it failed and the client shows the error).
    Clear,
    /// Kept as an interrupted turn in the transcript.
    KeepInTranscript,
}

impl Outcome {
    pub fn disposition(self) -> Disposition {
        match self {
            Outcome::Completed | Outcome::Failed => Disposition::Clear,
            Outcome::Left => Disposition::KeepInTranscript,
        }
    }
}

impl Partial {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one event the client was sent into the record. Events that carry no reply text are
    /// ignored.
    pub fn observe(&mut self, event: &Event) {
        match event {
            Event::Delta { text } => self.push_content(text),
            Event::Reasoning { text } => self.push_reasoning(text),
            Event::Tool { index, name, .. } => self.tools.push(ToolStep {
                index: *index,
                name: name.clone(),
                result: None,
            }),
            Event::ToolResult { index, text, .. } => {
                if let Some(step) = self.tools.iter_mut().rev().find(|s| s.index == *index) {
                    step.result = Some(cap_result(text));
                }
            }
            _ => {}
        }
    }

    fn push_content(&mut self, text: &str) {
        self.content.push_str(text);
        self.truncated |= keep_newest(&mut self.content, FIELD_CAP_CHARS);
    }

    fn push_reasoning(&mut self, text: &str) {
        self.reasoning.push_str(text);
        self.truncated |= keep_newest(&mut self.reasoning, FIELD_CAP_CHARS);
    }

    /// The record as the store would hold it. Empty `reasoning` and `tools` are left out.
    pub fn to_json(&self) -> Value {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|t| json!({"index": t.index, "name": t.name, "result": t.result}))
            .collect();
        let mut obj = serde_json::Map::new();
        obj.insert("content".into(), json!(self.content));
        if !self.reasoning.is_empty() {
            obj.insert("reasoning".into(), json!(self.reasoning));
        }
        if !tools.is_empty() {
            obj.insert("tools".into(), Value::Array(tools));
        }
        if self.truncated {
            obj.insert("truncated".into(), json!(true));
        }
        obj.insert("interrupted".into(), json!(true));
        Value::Object(obj)
    }
}

/// Drops the oldest characters so that at most `cap` remain. Returns whether anything was dropped.
/// A string of `cap` bytes or fewer holds at most `cap` characters, so the count runs only when
/// the byte length is over the cap.
fn keep_newest(text: &mut String, cap: usize) -> bool {
    if text.len() <= cap {
        return false;
    }
    let count = text.chars().count();
    if count <= cap {
        return false;
    }
    let skip = count - cap;
    if let Some((byte, _)) = text.char_indices().nth(skip) {
        text.drain(..byte);
    }
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_newest_characters_and_marks_truncation() {
        let mut p = Partial::new();
        let big = "x".repeat(FIELD_CAP_CHARS) + "END";
        p.observe(&Event::Delta { text: big });
        assert_eq!(p.content.chars().count(), FIELD_CAP_CHARS);
        assert!(p.content.ends_with("END"));
        assert!(p.truncated);
    }

    #[test]
    fn cut_never_splits_a_multibyte_character() {
        let mut text = "é".repeat(FIELD_CAP_CHARS + 5);
        assert!(keep_newest(&mut text, FIELD_CAP_CHARS));
        assert_eq!(text.chars().count(), FIELD_CAP_CHARS);
        assert!(text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn tool_results_are_capped_and_matched_by_index() {
        let mut p = Partial::new();
        p.observe(&Event::Tool {
            index: 0,
            name: "a".into(),
            args: Value::Null,
        });
        p.observe(&Event::Tool {
            index: 1,
            name: "b".into(),
            args: Value::Null,
        });
        p.observe(&Event::ToolResult {
            index: 1,
            name: "b".into(),
            text: "z".repeat(1000),
            not_run: false,
        });
        assert_eq!(p.tools[0].result, None);
        assert_eq!(
            p.tools[1].result.as_deref().map(str::len),
            Some(crate::tools::RESULT_CAP_CHARS)
        );
    }

    #[test]
    fn json_keeps_interrupted_marker_and_skips_empty_keys() {
        let mut p = Partial::new();
        p.observe(&Event::Delta { text: "Hi".into() });
        let v = p.to_json();
        assert_eq!(v["content"], "Hi");
        assert_eq!(v["interrupted"], true);
        assert!(v.get("reasoning").is_none());
        assert!(v.get("tools").is_none());
        assert!(v.get("truncated").is_none());
    }

    #[test]
    fn only_a_left_client_keeps_the_record() {
        assert_eq!(Outcome::Completed.disposition(), Disposition::Clear);
        assert_eq!(Outcome::Failed.disposition(), Disposition::Clear);
        assert_eq!(Outcome::Left.disposition(), Disposition::KeepInTranscript);
    }
}
