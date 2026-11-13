//! The Diary's model-client protocol (port of `diary/agent/llm.py`), without I/O.
//!
//! What a chat call sends, how a streamed or plain answer is read, what is emitted while it
//! streams, when it fails and how, and which served model is chosen. The transport (HTTP, retries'
//! sleeps, the exchange deadline that closes a given-up call) belongs to the caller: [`run`] drives
//! one call over a [`Transport`] with the Python retry rules; an async caller can feed
//! [`LineDecoder`] and [`StreamState`] chunk by chunk instead.
//!
//! Known differences from the Python:
//! - Response text is always decoded as UTF-8 (lossy); httpx honours a declared other charset.
//! - JSON that serde refuses but Python's `json` accepts is a decode error here: `NaN`/`Infinity`
//!   tokens, lone surrogate escapes (`"\ud83c"`), nesting deeper than 128. An integer beyond
//!   64 bits is read as a float.
//! - Error messages that quote an httpx exception (the "after N attempts" summary) carry only the
//!   error class (and status), never the URL or the exception text, in every mode; no caller
//!   shows them (`streaming.describe_failure` logs class names only).

use serde_json::{json, Map, Value};

use crate::context::is_python_space;
use crate::pyconv;

/// #127: the default answer-length cap (tokens) of the companion answer.
pub const DEFAULT_MAX_ANSWER_TOKENS: i64 = 1024;
/// #164: the extra tokens a reasoning model may spend before its answer.
pub const DEFAULT_MAX_REASONING_TOKENS: i64 = 4096;
/// #164: the answer cap is enforced on visible characters: cap tokens times this ratio.
pub const CHARS_PER_TOKEN: i64 = 4;
/// #164: a stream is read this many visible characters past the cap before it is closed.
pub const MARKER_SLACK_CHARS: i64 = 64;
pub const SHORTENED_NOTE: &str = "(This answer was shortened: it reached the answer length limit.)";
pub const LENGTH_CAP_TEXT: &str = "The answer cap (LLM_MAX_ANSWER_TOKENS) was reached before any answer text; nothing was saved. You can resend or raise the cap (or the reasoning allowance, LLM_MAX_REASONING_TOKENS).";
pub const EMPTY_ANSWER_TEXT: &str =
    "The model returned no final answer — nothing was saved. You can resend.";
const HELD_BACK_CHARS: i64 = 128;

// ---------------------------------------------------------------- regex emulation

/// Characters Python's `re.IGNORECASE` lets each tag letter match (recorded from CPython 3.12).
pub fn letter_matches(letter: char, c: char) -> bool {
    match letter {
        'i' => matches!(c, 'I' | 'i' | '\u{130}' | '\u{131}'),
        'k' => matches!(c, 'K' | 'k' | '\u{212A}'),
        's' => matches!(c, 'S' | 's' | '\u{17F}'),
        _ => c.is_ascii() && c.to_ascii_lowercase() == letter,
    }
}

/// A backreference under `re.IGNORECASE` compares CPython's simple lowercase of both characters.
pub fn simple_lower(c: char) -> char {
    match c {
        '\u{130}' => 'i',
        '\u{212A}' => 'k',
        c if c.is_ascii() => c.to_ascii_lowercase(),
        c => c,
    }
}

fn skip_space(chars: &[char], mut at: usize) -> usize {
    while chars.get(at).is_some_and(|c| is_python_space(*c)) {
        at += 1;
    }
    at
}

/// `<(think|thinking)>` at `at`: (group chars, index after `>`). "think" is tried first.
fn opener(chars: &[char], at: usize) -> Option<(Vec<char>, usize)> {
    if chars.get(at) != Some(&'<') {
        return None;
    }
    for word in ["think", "thinking"] {
        let n = word.len();
        let fits = word.chars().enumerate().all(|(k, letter)| {
            chars
                .get(at + 1 + k)
                .is_some_and(|c| letter_matches(letter, *c))
        });
        if fits && chars.get(at + 1 + n) == Some(&'>') {
            let group = chars
                .get(at + 1..at + 1 + n)
                .map(<[char]>::to_vec)
                .unwrap_or_default();
            return Some((group, at + 2 + n));
        }
    }
    None
}

/// Python `_THINK_LEAD_BLOCK_RE.match`: the end of a leading `\s*<tag>.*?</tag>\s*` block.
fn lead_block(chars: &[char]) -> Option<usize> {
    let start = skip_space(chars, 0);
    let (group, body) = opener(chars, start)?;
    let n = group.len();
    let mut at = body;
    while at < chars.len() {
        let closes = chars.get(at) == Some(&'<')
            && chars.get(at + 1) == Some(&'/')
            && group.iter().enumerate().all(|(k, g)| {
                chars
                    .get(at + 2 + k)
                    .is_some_and(|c| simple_lower(*c) == simple_lower(*g))
            })
            && chars.get(at + 2 + n) == Some(&'>');
        if closes {
            return Some(skip_space(chars, at + 3 + n));
        }
        at += 1;
    }
    None
}

/// Visible text without a model's leading inline reasoning (Python `strip_reasoning`). With
/// `final_` false (while streaming) the text is held back while it could still become a leading
/// opener, so a streamed prefix never shrinks.
pub fn strip_reasoning(text: &str, final_: bool) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    while let Some(end) = lead_block(&chars) {
        chars.drain(..end);
    }
    let start = skip_space(&chars, 0);
    if opener(&chars, start).is_some() {
        return String::new();
    }
    let text: String = chars.iter().collect();
    if !final_ {
        let head = text.trim_start_matches(is_python_space).to_lowercase();
        let short = pyconv::len(&head) < pyconv::len("<thinking>");
        if head.is_empty()
            || (short
                && ["<think>", "<thinking>"]
                    .iter()
                    .any(|o| o.starts_with(head.as_str())))
        {
            return String::new();
        }
    }
    text
}

/// Python `_LOG_MARKER_RE.search`: (start char index, "ok" | "skip") of a trailing
/// `\n?\s*[LOG:\s*(ok|skip)\s*]\s*$` marker.
fn log_marker(chars: &[char]) -> Option<(usize, &'static str)> {
    // Only a marker followed by nothing but whitespace can match; find the last "[LOG:".
    let mut close = chars.len();
    while close > 0 && chars.get(close - 1).is_some_and(|c| is_python_space(*c)) {
        close -= 1;
    }
    if close == 0 || chars.get(close - 1) != Some(&']') {
        return None;
    }
    let mut at = close - 1;
    while at > 0 && chars.get(at - 1).is_some_and(|c| is_python_space(*c)) {
        at -= 1;
    }
    let word_end = at;
    let decision = ["skip", "ok"].into_iter().find(|w| {
        let n = w.len();
        word_end >= n
            && w.chars()
                .enumerate()
                .all(|(k, c)| chars.get(word_end - n + k) == Some(&c))
    })?;
    let mut at = word_end - decision.len();
    while at > 0 && chars.get(at - 1).is_some_and(|c| is_python_space(*c)) {
        at -= 1;
    }
    let tag = ['[', 'L', 'O', 'G', ':'];
    if at < tag.len()
        || !tag
            .iter()
            .enumerate()
            .all(|(k, c)| chars.get(at - tag.len() + k) == Some(c))
    {
        return None;
    }
    let mut start = at - tag.len();
    while start > 0 && chars.get(start - 1).is_some_and(|c| is_python_space(*c)) {
        start -= 1;
    }
    Some((start, decision))
}

/// Python `LLMClient.strip_log_marker`: (visible text, "ok" | "skip" | None). The marker counts
/// only as the last line.
pub fn strip_log_marker(reply: &str) -> (String, Option<&'static str>) {
    let chars: Vec<char> = reply.chars().collect();
    match log_marker(&chars) {
        None => (reply.trim_matches(is_python_space).to_owned(), None),
        Some((start, decision)) => {
            let head: String = chars.iter().take(start).collect();
            (
                head.trim_matches(is_python_space).to_owned(),
                Some(decision),
            )
        }
    }
}

/// #164: the visible-answer limit in characters for an answer cap in tokens.
pub fn visible_char_cap(max_tokens: Option<i64>) -> Option<i64> {
    max_tokens.map(|t| t.saturating_mul(CHARS_PER_TOKEN))
}

/// #164: (text, cut). The visible answer (reasoning and trailing marker removed, stripped) cut at
/// the character cap, or `content` unchanged when it fits.
pub fn cap_visible(content: &str, max_tokens: Option<i64>) -> (String, bool) {
    let Some(limit) = visible_char_cap(max_tokens) else {
        return (content.to_owned(), false);
    };
    let text = strip_reasoning(content, true);
    let chars: Vec<char> = text.chars().collect();
    let visible: String = match log_marker(&chars) {
        Some((start, _)) => chars.iter().take(start).collect(),
        None => text.clone(),
    };
    let visible = visible.trim_matches(is_python_space);
    if pyconv::len(visible) <= limit {
        return (content.to_owned(), false);
    }
    (pyconv::slice(visible, None, Some(limit)), true)
}

/// #127: an answer the cap cut off ends with the shortened note.
pub fn with_shortened_note(text: &str) -> String {
    if text.trim_matches(is_python_space).is_empty() {
        SHORTENED_NOTE.to_owned()
    } else {
        format!(
            "{}\n\n{SHORTENED_NOTE}",
            text.trim_end_matches(is_python_space)
        )
    }
}

// ---------------------------------------------------------------- errors

/// A failed call, named after the Python exception it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmError {
    /// `LLMError(message)`.
    Llm(String),
    /// #135: the cap cut the answer off before any answer text.
    LengthCap,
    /// #146: a normal stop with no answer text.
    EmptyAnswer,
    /// `ModelNotServed(setting, message)`.
    ModelNotServed { setting: String, message: String },
    /// `EngineUnreachable`.
    EngineUnreachable,
    /// The provider answered with an HTTP error status (httpx `HTTPStatusError`).
    Status(u16),
    /// A malformed provider body: the Python exception class (`JSONDecodeError`, `KeyError`, ...).
    Protocol(&'static str),
    /// The transport failed (httpx `ConnectError`, `ReadError`, ...): its class name.
    Transport(&'static str),
}

impl LlmError {
    /// The Python exception class name.
    pub fn python_class(&self) -> &'static str {
        match self {
            Self::Llm(_) => "LLMError",
            Self::LengthCap => "LengthCapError",
            Self::EmptyAnswer => "EmptyAnswerError",
            Self::ModelNotServed { .. } => "ModelNotServed",
            Self::EngineUnreachable => "EngineUnreachable",
            Self::Status(_) => "HTTPStatusError",
            Self::Protocol(class) | Self::Transport(class) => class,
        }
    }
}

// ---------------------------------------------------------------- Python value semantics

fn truthy(v: &Value) -> bool {
    pyconv::truthy(v)
}

/// Python `a or b or c` over optional JSON values (a missing key is None).
fn or_chain<'a>(values: &[Option<&'a Value>]) -> Option<&'a Value> {
    let mut last = None;
    for v in values {
        last = *v;
        if v.is_some_and(truthy) {
            return *v;
        }
    }
    last.filter(|v| truthy(v))
}

/// Python `obj.get(key)` where `obj` must be a dict (else `AttributeError`).
fn get<'a>(obj: &'a Value, key: &str) -> Result<Option<&'a Value>, LlmError> {
    match obj {
        Value::Object(m) => Ok(m.get(key)),
        _ => Err(LlmError::Protocol("AttributeError")),
    }
}

/// Python `seq[0]` for a JSON value (`choices[0]`): a string gives its first character, which the
/// next `.get` or `[...]` then refuses with its own class.
fn first(v: &Value) -> Result<Value, LlmError> {
    match v {
        Value::Array(a) => a.first().cloned().ok_or(LlmError::Protocol("IndexError")),
        Value::Object(_) => Err(LlmError::Protocol("KeyError")),
        Value::String(s) => s
            .chars()
            .next()
            .map(|c| Value::String(c.to_string()))
            .ok_or(LlmError::Protocol("IndexError")),
        _ => Err(LlmError::Protocol("TypeError")),
    }
}

/// Python `obj[key]` for a JSON value.
fn index<'a>(obj: &'a Value, key: &str) -> Result<&'a Value, LlmError> {
    match obj {
        Value::Object(m) => m.get(key).ok_or(LlmError::Protocol("KeyError")),
        Value::Array(_) | Value::String(_) => Err(LlmError::Protocol("TypeError")),
        _ => Err(LlmError::Protocol("TypeError")),
    }
}

fn parse_json(raw: &str) -> Result<Value, LlmError> {
    serde_json::from_str(raw).map_err(|_| LlmError::Protocol("JSONDecodeError"))
}

/// Python `json.loads(bytes)`: UTF-8 with or without a BOM (`utf-8-sig`); invalid UTF-8 is a
/// `UnicodeDecodeError`. Known difference: BOM-less UTF-16/32 bodies, which Python detects, are a
/// decode error here.
pub fn parse_json_bytes(raw: &[u8]) -> Result<Value, LlmError> {
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    let text = std::str::from_utf8(raw).map_err(|_| LlmError::Protocol("UnicodeDecodeError"))?;
    parse_json(text)
}

// ---------------------------------------------------------------- stream reading

/// httpx `LineDecoder`: `str.splitlines` boundaries, fed incrementally.
#[derive(Debug, Default)]
pub struct LineDecoder {
    buffer: String,
    trailing_cr: bool,
}

fn line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\u{0b}'
            | '\u{0c}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// Python `str.splitlines()` (no line ends kept).
fn splitlines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if line_break(c) {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl LineDecoder {
    pub fn decode(&mut self, text: &str) -> Vec<String> {
        let mut text = if self.trailing_cr {
            self.trailing_cr = false;
            format!("\r{text}")
        } else {
            text.to_owned()
        };
        if text.ends_with('\r') {
            self.trailing_cr = true;
            text.pop();
        }
        if text.is_empty() {
            return Vec::new();
        }
        let trailing_newline = text.chars().last().is_some_and(line_break);
        let mut lines = splitlines(&text);
        if lines.len() == 1 && !trailing_newline {
            self.buffer
                .push_str(lines.first().map_or("", String::as_str));
            return Vec::new();
        }
        if !self.buffer.is_empty() {
            let head = std::mem::take(&mut self.buffer);
            match lines.first_mut() {
                Some(first) => first.insert_str(0, &head),
                None => lines.push(head),
            }
        }
        if !trailing_newline {
            self.buffer = lines.pop().unwrap_or_default();
        }
        lines
    }

    pub fn flush(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.buffer.is_empty() || self.trailing_cr {
            out.push(std::mem::take(&mut self.buffer));
            self.trailing_cr = false;
        }
        out
    }
}

/// Incremental UTF-8 decoding with replacement, across chunk boundaries.
#[derive(Debug, Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        // Keep an incomplete trailing sequence (at most 3 bytes) for the next chunk.
        let mut keep = 0;
        for back in 1..=3.min(self.pending.len()) {
            let b = self
                .pending
                .get(self.pending.len() - back)
                .copied()
                .unwrap_or(0);
            if b & 0xC0 == 0x80 {
                continue;
            }
            let need = if b >= 0xF0 {
                4
            } else if b >= 0xE0 {
                3
            } else if b >= 0xC0 {
                2
            } else {
                1
            };
            if need > back {
                keep = back;
            }
            break;
        }
        let cut = self.pending.len() - keep;
        let tail = self.pending.split_off(cut);
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending = tail;
        text
    }

    pub fn flush(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

/// What a stream or a call emitted (the Python `emit` events of a model call).
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Delta(String),
    /// Speculative-decoding telemetry: the model id and the raw `draft_n` / `draft_n_accepted`.
    Mtp {
        model: Value,
        draft_n: Value,
        draft_n_accepted: Value,
    },
    Answer(String),
}

impl Event {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Delta(text) => json!({"type": "delta", "text": text}),
            Self::Mtp {
                model,
                draft_n,
                draft_n_accepted,
            } => {
                json!({"type": "mtp", "model": model, "timings": {"draft_n": draft_n, "draft_n_accepted": draft_n_accepted}})
            }
            Self::Answer(text) => json!({"type": "answer", "text": text}),
        }
    }
}

/// The read of one completion (Python `_stream_completion`), fed line by line or as one body.
#[derive(Debug)]
pub struct StreamState {
    chat_model: String,
    emit: bool,
    limit: Option<i64>,
    max_tokens: Option<i64>,
    content: String,
    reasoning: String,
    sent: i64,
    finish: Value,
    completed: bool,
}

/// What the caller does after a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Stop reading and close the stream (`[DONE]`, or the cap was passed).
    Stop,
}

/// The finished read: (content, reasoning, finish reason) as `_stream_completion` returns them.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub content: String,
    pub reasoning: String,
    pub finish: Value,
}

impl StreamState {
    /// `emit` says whether the caller wants delta and telemetry events (Python `emit is not None`).
    pub fn new(chat_model: &str, max_tokens: Option<i64>, emit: bool) -> Self {
        Self {
            chat_model: chat_model.to_owned(),
            emit,
            limit: visible_char_cap(max_tokens),
            max_tokens,
            content: String::new(),
            reasoning: String::new(),
            sent: 0,
            finish: Value::Null,
            completed: false,
        }
    }

    fn telemetry(&self, data: &Value, events: &mut Vec<Event>) -> Result<(), LlmError> {
        if !self.emit {
            return Ok(());
        }
        if let Some(Value::Object(t)) = get(data, "timings")? {
            if let (Some(n), Some(a)) = (t.get("draft_n"), t.get("draft_n_accepted")) {
                let model = or_chain(&[get(data, "model")?])
                    .cloned()
                    .unwrap_or_else(|| json!(self.chat_model));
                events.push(Event::Mtp {
                    model,
                    draft_n: n.clone(),
                    draft_n_accepted: a.clone(),
                });
            }
        }
        Ok(())
    }

    /// One line of an event stream (the caller reports provider progress before calling).
    pub fn line(&mut self, line: &str, events: &mut Vec<Event>) -> Result<Flow, LlmError> {
        let Some(rest) = line.strip_prefix("data:") else {
            return Ok(Flow::Continue);
        };
        let raw = rest.trim_matches(is_python_space);
        if raw == "[DONE]" {
            self.completed = true;
            return Ok(Flow::Stop);
        }
        let data = parse_json(raw)?;
        self.telemetry(&data, events)?;
        if get(&data, "error")?.is_some_and(truthy) {
            return Err(LlmError::Llm("provider stream failed".into()));
        }
        let Some(choices) = or_chain(&[get(&data, "choices")?]) else {
            return Ok(Flow::Continue);
        };
        let choice = &first(choices)?;
        if let Some(reason) = get(choice, "finish_reason")?.filter(|v| truthy(v)) {
            self.completed = true;
            self.finish = reason.clone();
        }
        let empty = Value::Object(Map::new());
        let delta = or_chain(&[get(choice, "delta")?]).unwrap_or(&empty);
        let thought = or_chain(&[get(delta, "reasoning_content")?, get(delta, "reasoning")?]);
        if let Some(Value::String(t)) = thought {
            self.reasoning.push_str(t);
        }
        if let Some(Value::String(text)) = or_chain(&[get(delta, "content")?]) {
            self.content.push_str(text);
            if !self.emit && self.limit.is_none() {
                return Ok(Flow::Continue);
            }
            let shown = strip_reasoning(&self.content, false);
            if let Some(limit) = self.limit {
                if pyconv::len(shown.trim_matches(is_python_space))
                    > limit.saturating_add(MARKER_SLACK_CHARS)
                {
                    self.completed = true;
                    return Ok(Flow::Stop);
                }
            }
            if !self.emit {
                return Ok(Flow::Continue);
            }
            let shown_len = pyconv::len(&shown);
            let end = self
                .sent
                .max((shown_len - HELD_BACK_CHARS).min(self.limit.unwrap_or(shown_len)));
            if end > self.sent {
                events.push(Event::Delta(pyconv::slice(
                    &shown,
                    Some(self.sent),
                    Some(end),
                )));
                self.sent = end;
            }
        }
        Ok(Flow::Continue)
    }

    /// A body that is not an event stream: one JSON completion. `alive` is called once it parsed.
    pub fn body(
        &mut self,
        raw: &[u8],
        events: &mut Vec<Event>,
        alive: &mut dyn FnMut(),
    ) -> Result<(), LlmError> {
        let data = parse_json_bytes(raw)?;
        alive();
        self.telemetry(&data, events)?;
        self.completed = true;
        let choice = &first(index(&data, "choices")?)?;
        self.finish = get(choice, "finish_reason")?
            .cloned()
            .unwrap_or(Value::Null);
        let message = index(choice, "message")?;
        let content = or_chain(&[get(message, "content")?])
            .cloned()
            .unwrap_or(json!(""));
        let reasoning = or_chain(&[
            get(message, "reasoning_content")?,
            get(message, "reasoning")?,
        ])
        .cloned()
        .unwrap_or(json!(""));
        let (Value::String(content), Value::String(reasoning)) = (content, reasoning) else {
            return Err(LlmError::Llm("invalid provider answer".into()));
        };
        self.content = strip_reasoning(&content, true);
        self.reasoning = reasoning;
        Ok(())
    }

    /// The end of the read: the cap applied exactly, whatever the chunking.
    pub fn finish(self) -> Result<Completion, LlmError> {
        if !self.completed {
            return Err(LlmError::Llm(
                "provider stream ended before completion".into(),
            ));
        }
        let (content, cut) = cap_visible(&self.content, self.max_tokens);
        Ok(Completion {
            content,
            reasoning: self.reasoning,
            finish: if cut { json!("length") } else { self.finish },
        })
    }
}

/// An answer with its request-local reasoning (Python `ChatReply`).
#[derive(Debug, Clone, PartialEq)]
pub struct ChatReply {
    pub content: String,
    pub reasoning: String,
    /// The provider stopped at the length cap (#127).
    pub truncated: bool,
}

/// Python `_chat_stream` after the read: the answer event, or why there is none.
pub fn finish_chat_stream(c: Completion, events: &mut Vec<Event>) -> Result<ChatReply, LlmError> {
    let content = strip_reasoning(&c.content, true);
    let (visible, _) = strip_log_marker(&content);
    let truncated = c.finish == json!("length");
    if visible.is_empty() && truncated {
        return Err(LlmError::LengthCap);
    }
    if visible.is_empty() {
        return Err(LlmError::EmptyAnswer);
    }
    events.push(Event::Answer(if truncated {
        with_shortened_note(&visible)
    } else {
        visible
    }));
    Ok(ChatReply {
        content,
        reasoning: c.reasoning,
        truncated,
    })
}

/// Python `chat()`'s `parse_chat` for a plain (not streamed) answer body.
pub fn parse_chat(data: &Value, max_tokens: Option<i64>) -> Result<ChatReply, LlmError> {
    let choice = &first(index(data, "choices")?)?;
    let message = index(choice, "message")?;
    let Value::String(content) = index(message, "content")? else {
        return Err(LlmError::Llm("unexpected chat response shape".into()));
    };
    let reasoning = or_chain(&[
        get(message, "reasoning_content")?,
        get(message, "reasoning")?,
    ]);
    let reasoning = match reasoning {
        Some(Value::String(r)) => r.clone(),
        _ => String::new(),
    };
    let (content, cut) = cap_visible(content, max_tokens);
    let finish = get(choice, "finish_reason")?;
    Ok(ChatReply {
        content: strip_reasoning(&content, true),
        reasoning,
        truncated: cut || finish == Some(&json!("length")),
    })
}

// ---------------------------------------------------------------- requests

/// The chat request body. `max_tokens` is the visible-answer cap; the server is sent the cap plus
/// the reasoning allowance (#164).
pub fn chat_payload(
    model: &str,
    messages: &Value,
    temperature: f64,
    stream: bool,
    max_tokens: Option<i64>,
    reasoning_tokens: i64,
    stop: Option<&[String]>,
) -> Value {
    let mut payload = Map::new();
    payload.insert("model".into(), json!(model));
    payload.insert("messages".into(), messages.clone());
    payload.insert("temperature".into(), json!(temperature));
    payload.insert("stream".into(), json!(stream));
    if let Some(cap) = max_tokens {
        payload.insert(
            "max_tokens".into(),
            json!(cap.saturating_add(reasoning_tokens.max(0))),
        );
    }
    if let Some(stop) = stop.filter(|s| !s.is_empty()) {
        payload.insert("stop".into(), json!(stop));
    }
    Value::Object(payload)
}

// ---------------------------------------------------------------- driving one call

/// One HTTP answer as the protocol needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// POST a JSON body to a path of the model engine (`/chat/completions`). A transport failure is
/// the httpx exception class (`ConnectError`, `ReadTimeout`, ...).
pub trait Transport {
    fn post(&mut self, path: &str, body: &Value) -> Result<HttpResponse, &'static str>;
    /// Wait `seconds` before a retry (Python `time.sleep`).
    fn sleep(&mut self, seconds: f64);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `chat_stream(emit=...)`: streamed, events emitted, answer checked (#135/#146).
    Stream,
    /// `chat(alive=...)`: streamed internally, retried before the answer began (#127).
    Cancellable,
    /// `chat()`: one plain completion with `_post_with_retries`.
    Plain,
}

pub struct Call<'a> {
    pub mode: Mode,
    pub model: &'a str,
    pub messages: &'a Value,
    pub temperature: f64,
    pub max_tokens: Option<i64>,
    pub reasoning_tokens: i64,
    pub stop: Option<&'a [String]>,
    pub max_retries: i64,
    pub retry_backoff_s: f64,
    /// Cancellable mode: whether the exchange deadline gave the call up (checked before each try,
    /// so a given-up call never re-sends its prompt).
    pub abandoned: &'a dyn Fn() -> bool,
}

/// httpx `raise_for_status`: anything outside 2xx (a 3xx the client did not follow included).
fn http_error(status: u16) -> bool {
    !(200..300).contains(&status)
}

/// Reads one streamed answer from a full response (Python `_stream_completion` after the POST).
/// `alive` is called on every line the provider sends.
pub fn read_stream(
    resp: &HttpResponse,
    model: &str,
    max_tokens: Option<i64>,
    emit: bool,
    events: &mut Vec<Event>,
    alive: &mut dyn FnMut(),
) -> Result<Completion, LlmError> {
    if http_error(resp.status) {
        return Err(LlmError::Status(resp.status));
    }
    let mut state = StreamState::new(model, max_tokens, emit);
    if !resp.content_type.contains("text/event-stream") {
        state.body(&resp.body, events, alive)?;
        return state.finish();
    }
    let mut utf8 = Utf8Decoder::default();
    let mut lines = LineDecoder::default();
    let mut all = lines.decode(&utf8.decode(&resp.body));
    let tail = utf8.flush();
    if !tail.is_empty() {
        all.extend(lines.decode(&tail));
    }
    all.extend(lines.flush());
    for line in all {
        alive();
        if state.line(&line, events)? == Flow::Stop {
            break;
        }
    }
    state.finish()
}

/// Drives one call with the Python retry rules of its mode.
pub fn run(
    transport: &mut dyn Transport,
    call: &Call<'_>,
    events: &mut Vec<Event>,
    alive: &mut dyn FnMut(),
) -> Result<ChatReply, LlmError> {
    let tries = call.max_retries;
    match call.mode {
        Mode::Stream => {
            let payload = chat_payload(
                call.model,
                call.messages,
                call.temperature,
                true,
                call.max_tokens,
                call.reasoning_tokens,
                None,
            );
            let resp = transport
                .post("/chat/completions", &payload)
                .map_err(LlmError::Transport)?;
            let c = read_stream(&resp, call.model, call.max_tokens, true, events, alive)?;
            finish_chat_stream(c, events)
        }
        Mode::Cancellable => {
            let payload = chat_payload(
                call.model,
                call.messages,
                call.temperature,
                true,
                call.max_tokens,
                call.reasoning_tokens,
                call.stop,
            );
            let mut last = String::from("None");
            for attempt in 0..tries.max(0) {
                if (call.abandoned)() {
                    break;
                }
                let outcome = transport
                    .post("/chat/completions", &payload)
                    .map_err(LlmError::Transport)
                    .and_then(|resp| {
                        read_stream(
                            &resp,
                            call.model,
                            call.max_tokens,
                            false,
                            &mut Vec::new(),
                            alive,
                        )
                    });
                match outcome {
                    Ok(c) => {
                        return Ok(ChatReply {
                            content: strip_reasoning(&c.content, true),
                            reasoning: c.reasoning,
                            truncated: c.finish == json!("length"),
                        })
                    }
                    Err(LlmError::Status(status)) if status < 500 => {
                        return Err(LlmError::Llm(format!("chat failed: HTTP {status}")))
                    }
                    Err(LlmError::Status(status)) => last = format!("HTTPStatusError {status}"),
                    Err(LlmError::Transport(class @ ("ConnectError" | "ConnectTimeout"))) => {
                        last = class.to_owned()
                    }
                    Err(e @ LlmError::Llm(_)) => return Err(e),
                    Err(e) => {
                        return Err(LlmError::Llm(format!("chat failed: {}", e.python_class())))
                    }
                }
                if attempt == tries - 1 {
                    break;
                }
                transport.sleep(
                    call.retry_backoff_s * 2f64.powi(i32::try_from(attempt).unwrap_or(i32::MAX)),
                );
            }
            Err(LlmError::Llm(format!(
                "chat failed after {tries} attempts: {last}"
            )))
        }
        Mode::Plain => {
            let payload = chat_payload(
                call.model,
                call.messages,
                call.temperature,
                false,
                call.max_tokens,
                call.reasoning_tokens,
                call.stop,
            );
            let mut last = String::from("None");
            for attempt in 0..tries.max(0) {
                let outcome = transport
                    .post("/chat/completions", &payload)
                    .map_err(LlmError::Transport)
                    .and_then(|resp| {
                        if http_error(resp.status) {
                            return Err(LlmError::Status(resp.status));
                        }
                        parse_chat(&parse_json_bytes(&resp.body)?, call.max_tokens)
                    });
                match outcome {
                    Ok(reply) => return Ok(reply),
                    Err(e) => last = e.python_class().to_owned(),
                }
                if attempt == tries - 1 {
                    break;
                }
                transport.sleep(
                    call.retry_backoff_s * 2f64.powi(i32::try_from(attempt).unwrap_or(i32::MAX)),
                );
            }
            Err(LlmError::Llm(format!(
                "chat failed after {tries} attempts: {last}"
            )))
        }
    }
}

// ---------------------------------------------------------------- served models

/// One entry of the engine's model list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    pub id: String,
    pub loaded: bool,
    pub chat: bool,
}

const NON_CHAT_FLAGS: [&str; 4] = ["--embedding", "--embeddings", "--reranking", "--rerank"];

/// The router's `/v1/models` body as entries (Python `_served_entries`).
pub fn served_entries(data: &Value) -> Result<Vec<Served>, LlmError> {
    let Some(Value::Array(items)) = data.as_object().and_then(|o| o.get("data")) else {
        return Err(LlmError::Protocol("ValueError"));
    };
    let mut out = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let Some(Value::String(id)) = obj.get("id") else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let status = obj.get("status").and_then(Value::as_object);
        let args = status.and_then(|s| s.get("args")).and_then(Value::as_array);
        let non_chat = args.is_some_and(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .any(|f| NON_CHAT_FLAGS.contains(&f))
        });
        out.push(Served {
            id: id.clone(),
            loaded: status.is_none_or(|s| s.get("value") == Some(&json!("loaded"))),
            chat: !non_chat,
        });
    }
    Ok(out)
}

/// Which served id to send (Python `_choose`): (model, source, why). `partner` is the id the chat
/// client resolved to, for the aux role on the same engine.
pub fn choose(
    entries: &[Served],
    chat_model: &str,
    fallback_model: &str,
    setting_key: &str,
    fallback_setting_key: &str,
    partner: Option<&str>,
) -> Result<(String, &'static str, String), LlmError> {
    let chat_ids: Vec<&str> = entries
        .iter()
        .filter(|e| e.chat)
        .map(|e| e.id.as_str())
        .collect();
    if chat_ids.is_empty() {
        return Err(LlmError::Llm(
            "the model engine serves no chat model".into(),
        ));
    }
    if chat_ids.contains(&chat_model) {
        return Ok((
            chat_model.to_owned(),
            "configured",
            "the configured model is served".into(),
        ));
    }
    if !fallback_model.is_empty() && chat_ids.contains(&fallback_model) {
        return Ok((
            fallback_model.to_owned(),
            "fallback",
            format!("the explicit fallback set in {fallback_setting_key}"),
        ));
    }
    if let Some(p) = partner.filter(|p| !p.is_empty() && chat_ids.contains(p)) {
        return Ok((p.to_owned(), "fallback", "the chat model's id".into()));
    }
    let tail = if fallback_model.is_empty() {
        format!(" or set {fallback_setting_key} to a served id")
    } else {
        format!(" (the fallback set in {fallback_setting_key} is not served either)")
    };
    Err(LlmError::ModelNotServed {
        setting: setting_key.to_owned(),
        message: format!(
            "the model id set in {setting_key} is not served by the model engine; set {setting_key} to a served id{tail}. No request was sent."
        ),
    })
}

// ---------------------------------------------------------------- embeddings errors

/// #1759: the embeddings server refused an input as too long: (message, tokens, limit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooLarge {
    pub message: String,
    pub tokens: Option<i64>,
    pub limit: Option<i64>,
}

/// `re.search(needle, hay, re.I)` for a literal ASCII needle: each letter also matches the
/// non-ASCII characters CPython 3.12's re.I lets it (U+0130/U+0131 for i, U+212A for k, U+017F
/// for s).
fn find_ci(hay: &str, needle: &str) -> bool {
    let hay: Vec<char> = hay.chars().collect();
    let needle: Vec<char> = needle.chars().collect();
    if needle.len() > hay.len() {
        return false;
    }
    (0..=hay.len() - needle.len()).any(|at| {
        needle.iter().enumerate().all(|(k, n)| {
            hay.get(at + k).is_some_and(|h| {
                if n.is_ascii_alphabetic() {
                    letter_matches(*n, *h)
                } else {
                    h == n
                }
            })
        })
    })
}

#[derive(Clone, Copy)]
enum Part {
    Lit(&'static str),
    Space,
    Digits,
}

/// A match of `parts` at char index `at` (literal text, `\s*`, one `(\d+)` group): the number.
fn match_at(chars: &[char], at: usize, parts: &[Part]) -> Option<Option<i64>> {
    let mut i = at;
    let mut number = None;
    for part in parts {
        match part {
            Part::Lit(lit) => {
                for c in lit.chars() {
                    if chars.get(i) != Some(&c) {
                        return None;
                    }
                    i += 1;
                }
            }
            Part::Space => {
                while chars.get(i).is_some_and(|c| is_python_space(*c)) {
                    i += 1;
                }
            }
            Part::Digits => {
                let start = i;
                let mut value: Option<i64> = Some(0);
                while let Some(d) = chars
                    .get(i)
                    .and_then(|c| diary_entity_focus::python_decimal_value(*c))
                {
                    value = value
                        .and_then(|v| v.checked_mul(10))
                        .and_then(|v| v.checked_add(i64::from(d)));
                    i += 1;
                }
                if i == start {
                    return None;
                }
                number = value;
            }
        }
    }
    Some(number)
}

/// `_first_int(rx, text)`: the number of the leftmost match of any alternative of `rx`.
fn first_int(text: &str, alternatives: &[&[Part]]) -> Option<i64> {
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len()).find_map(|at| {
        alternatives
            .iter()
            .find_map(|alt| match_at(&chars, at, alt))
    })?
}

/// Python `too_large_from_response` for an error answer's status and text.
pub fn too_large(status: u16, text: &str) -> Option<TooLarge> {
    if status < 400 {
        return None;
    }
    let body = pyconv::slice(text, None, Some(4000));
    let hit = [
        "too large to process",
        "exceeds the available context size",
        "larger than the max context size",
        "exceed_context_size",
    ]
    .iter()
    .any(|n| find_ci(&body, n));
    if !hit {
        return None;
    }
    Some(TooLarge {
        message: format!(
            "embeddings refused as too long (HTTP {status}): {}",
            pyconv::slice(&body, None, Some(300))
        ),
        tokens: first_int(
            &body,
            &[
                &[Part::Lit("input ("), Part::Digits, Part::Lit(" tokens)")],
                &[
                    Part::Lit("\"n_prompt_tokens\""),
                    Part::Space,
                    Part::Lit(":"),
                    Part::Space,
                    Part::Digits,
                ],
            ],
        ),
        limit: first_int(
            &body,
            &[
                &[Part::Lit("current batch size:"), Part::Space, Part::Digits],
                &[
                    Part::Lit("\"n_ctx\""),
                    Part::Space,
                    Part::Lit(":"),
                    Part::Space,
                    Part::Digits,
                ],
            ],
        ),
    })
}

/// Python `embed()`'s `parse_embed`: one vector per input, in the order of each item's `index`
/// (a stable sort; a missing index counts as 0). Known difference: an `embedding` that is not a
/// list of numbers is a protocol error here; Python returns it as is and fails later.
pub fn parse_embed(data: &Value, sent: usize) -> Result<Vec<Vec<f64>>, LlmError> {
    let items = match get(data, "data")? {
        None => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(_) => return Err(LlmError::Protocol("TypeError")),
    };
    if items.len() != sent {
        return Err(LlmError::Llm(format!("embedding count mismatch: sent {sent}, got {}", items.len())));
    }
    let mut keyed = Vec::with_capacity(items.len());
    for item in items {
        let order = match get(item, "index")? {
            None => 0.0,
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
            Some(Value::Bool(b)) => f64::from(u8::from(*b)),
            Some(_) => return Err(LlmError::Protocol("TypeError")),
        };
        let Value::Array(values) = index(item, "embedding")? else {
            return Err(LlmError::Protocol("TypeError"));
        };
        let vector = values.iter().map(Value::as_f64).collect::<Option<Vec<f64>>>().ok_or(LlmError::Protocol("TypeError"))?;
        keyed.push((order, vector));
    }
    keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(keyed.into_iter().map(|(_, v)| v).collect())
}
