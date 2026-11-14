//! The backend's event stream: bytes to JSON events (`sseEvents`), events to chat parts
//! (`chatParts`).

use crate::js::is_js_space;
use crate::texts::Texts;
use js_json::JValue;

/// `sseEvents(stream)`: blocks split on a blank line (`\r?\n\r?\n`), the `data:` lines of a block
/// joined, `[DONE]` and unparsable data skipped. Chunk boundaries never change the result.
#[derive(Debug, Default, Clone)]
pub struct SseDecoder {
    /// Undecoded tail bytes: an incomplete UTF-8 sequence at a chunk's end.
    pending: Vec<u8>,
    buffer: String,
}

/// The end of the first blank line in `s`: (start, end) of the `\r?\n\r?\n` match.
fn blank_line(s: &str) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    while let Some(off) = b.get(i..)?.iter().position(|&c| c == b'\n') {
        let nl = i + off;
        let start = if nl > 0 && b.get(nl - 1) == Some(&b'\r') {
            nl - 1
        } else {
            nl
        };
        let mut j = nl + 1;
        if b.get(j) == Some(&b'\r') {
            j += 1;
        }
        if b.get(j) == Some(&b'\n') {
            return Some((start, j + 1));
        }
        i = nl + 1;
    }
    None
}

fn parse_block(block: &str) -> Option<JValue> {
    let data: Vec<&str> = block
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|l| l.trim_start_matches(is_js_space))
        .collect();
    let data = data.join("\n");
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    js_json::parse(&data).ok()
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// `TextDecoder#decode(chunk, { stream: true })`: invalid bytes become U+FFFD, an incomplete
    /// sequence at the end waits for the next chunk.
    fn decode(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
        let bytes = std::mem::take(&mut self.pending);
        let mut rest: &[u8] = &bytes;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    self.buffer.push_str(s);
                    return;
                }
                Err(e) => {
                    let (good, bad) = rest.split_at(e.valid_up_to());
                    self.buffer
                        .push_str(std::str::from_utf8(good).unwrap_or(""));
                    match e.error_len() {
                        Some(n) => {
                            self.buffer.push('\u{FFFD}');
                            rest = bad.get(n..).unwrap_or(&[]);
                        }
                        None => {
                            self.pending = bad.to_vec();
                            return;
                        }
                    }
                }
            }
        }
    }

    /// The events complete after this chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<JValue> {
        self.decode(chunk);
        let mut out = Vec::new();
        while let Some((start, end)) = blank_line(&self.buffer) {
            let block: String = self.buffer.get(..start).unwrap_or("").to_string();
            self.buffer.drain(..end);
            out.extend(parse_block(&block));
        }
        out
    }

    /// The stream ended: the last block, if any. Core never flushes its decoder, so an incomplete
    /// UTF-8 sequence at the very end is dropped.
    pub fn finish(&mut self) -> Option<JValue> {
        self.pending.clear();
        let rest = std::mem::take(&mut self.buffer);
        parse_block(&rest)
    }
}

/// A `Map`/`Set` key with JS SameValueZero equality. Objects and arrays are never equal to
/// anything (identity), so they are kept as distinct unique keys.
#[derive(Debug, Clone, PartialEq)]
enum Key {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    NaN,
    Str(String),
    Unique(usize),
}

/// One chat part (`chatParts` yields).
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    /// A `/chat/completions` delta: `{ content }`, `{ reasoning_content }` or `{ tool_calls }`.
    Delta(JValue),
    /// The reply ended with an error; `status` only for the errors that carry one.
    Error {
        message: String,
        status: Option<u16>,
    },
    /// The reply finished: `stop`, `length`, `content_filter` or `tool_calls`; usage when sent.
    Finish { finish: String, usage: JValue },
}

/// `chatParts(stream)` as a state machine: feed events in order, read parts back.
#[derive(Debug, Clone)]
pub struct ChatParts {
    texts: Texts,
    now_ms: f64,
    tool_index: Vec<(Key, usize)>,
    args_sent: Vec<Key>,
    text_sent: Vec<Key>,
    saw_text: bool,
    anonymous_text: bool,
    saw_tool: bool,
    uniques: usize,
    ended: bool,
}

fn tool_open(index: usize, id: JValue, name: JValue, arguments: JValue) -> Part {
    Part::Delta(JValue::obj([(
        "tool_calls",
        JValue::Arr(vec![JValue::obj([
            ("index", JValue::Num(index as f64)),
            ("id", id),
            ("type", JValue::from("function")),
            (
                "function",
                JValue::obj([("name", name), ("arguments", arguments)]),
            ),
        ])]),
    )]))
}

fn tool_args(index: usize, arguments: &str) -> Part {
    Part::Delta(JValue::obj([(
        "tool_calls",
        JValue::Arr(vec![JValue::obj([
            ("index", JValue::Num(index as f64)),
            (
                "function",
                JValue::obj([("arguments", JValue::from(arguments))]),
            ),
        ])]),
    )]))
}

/// `item.call_id || item.id`.
fn call_id(item: &JValue) -> JValue {
    if item.get("call_id").truthy() {
        item.get("call_id").clone()
    } else {
        item.get("id").clone()
    }
}

/// `messageText(item)`.
fn message_text(item: &JValue) -> String {
    match item.get("content") {
        JValue::Arr(parts) => parts
            .iter()
            .filter(|c| {
                c.is_object() && matches!(c.get("type").as_str(), Some("output_text" | "text"))
            })
            .filter_map(|c| c.get("text").as_str())
            .collect(),
        _ => String::new(),
    }
}

/// `usageOf(u)`.
pub fn usage_of(u: &JValue) -> JValue {
    if !u.is_object() {
        return JValue::Null;
    }
    let or_null = |v: &JValue| {
        if v.is_nullish() {
            JValue::Null
        } else {
            v.clone()
        }
    };
    let total = if u.get("total_tokens").is_nullish() {
        match (
            u.get("input_tokens").as_num(),
            u.get("output_tokens").as_num(),
        ) {
            (Some(a), Some(b)) if a.is_finite() && b.is_finite() => JValue::Num(a + b),
            _ => JValue::Null,
        }
    } else {
        u.get("total_tokens").clone()
    };
    JValue::obj([
        ("prompt_tokens", or_null(u.get("input_tokens"))),
        ("completion_tokens", or_null(u.get("output_tokens"))),
        ("total_tokens", total),
    ])
}

impl ChatParts {
    /// `now_ms` stands for core's `Date.now()` in the usage-limit line.
    pub fn new(texts: Texts, now_ms: f64) -> Self {
        ChatParts {
            texts,
            now_ms,
            tool_index: Vec::new(),
            args_sent: Vec::new(),
            text_sent: Vec::new(),
            saw_text: false,
            anonymous_text: false,
            saw_tool: false,
            uniques: 0,
            ended: false,
        }
    }

    /// True after a finish or an error: core stops reading the stream there.
    pub fn ended(&self) -> bool {
        self.ended
    }

    fn key(&mut self, v: &JValue) -> Key {
        match v {
            JValue::Undefined => Key::Undefined,
            JValue::Null => Key::Null,
            JValue::Bool(b) => Key::Bool(*b),
            JValue::Num(n) if n.is_nan() => Key::NaN,
            JValue::Num(n) => Key::Num(if *n == 0.0 { 0.0 } else { *n }),
            JValue::Str(s) => Key::Str(s.clone()),
            JValue::Arr(_) | JValue::Obj(_) => {
                self.uniques += 1;
                Key::Unique(self.uniques)
            }
        }
    }

    fn index_of(&mut self, id: &JValue) -> Option<usize> {
        let k = self.key(id);
        self.tool_index
            .iter()
            .find(|(key, _)| *key == k)
            .map(|(_, i)| *i)
    }

    /// `index = toolIndex.size; toolIndex.set(id, index)`: a key already present (two calls
    /// without an id, say) is overwritten and the size does not grow, as core's Map does.
    fn open_index(&mut self, id: &JValue) -> usize {
        let index = self.tool_index.len();
        let k = self.key(id);
        match self.tool_index.iter_mut().find(|(key, _)| *key == k) {
            Some(slot) => slot.1 = index,
            None => self.tool_index.push((k, index)),
        }
        self.saw_tool = true;
        index
    }

    fn unsent_message(&self, item: &JValue) -> bool {
        if item.get("id").truthy() {
            let sent = match item.get("id") {
                JValue::Str(s) => self.text_sent.contains(&Key::Str(s.clone())),
                JValue::Num(n) if !n.is_nan() => self.text_sent.contains(&Key::Num(*n)),
                JValue::Bool(b) => self.text_sent.contains(&Key::Bool(*b)),
                _ => false,
            };
            !sent && !self.anonymous_text
        } else {
            !self.saw_text
        }
    }

    fn mark_text_sent(&mut self, id: &JValue) {
        let k = self.key(id);
        if !self.text_sent.contains(&k) {
            self.text_sent.push(k);
        }
    }

    fn message_done(&mut self, item: &JValue, out: &mut Vec<Part>) {
        if self.unsent_message(item) {
            let text = message_text(item);
            if item.get("id").truthy() {
                self.mark_text_sent(item.get("id"));
            }
            if !text.is_empty() {
                self.saw_text = true;
                out.push(Part::Delta(JValue::obj([("content", JValue::from(text))])));
            }
        }
    }

    /// One event; the parts it yields, in order.
    pub fn feed(&mut self, evt: &JValue) -> Vec<Part> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        let item = if evt.get("item").is_object() {
            Some(evt.get("item"))
        } else {
            None
        };
        match evt.get("type").as_str().unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = evt.get("delta").as_str().filter(|d| !d.is_empty()) {
                    match evt.get("item_id").as_str() {
                        Some(id) => {
                            let k = Key::Str(id.to_string());
                            if !self.text_sent.contains(&k) {
                                self.text_sent.push(k);
                            }
                        }
                        None => self.anonymous_text = true,
                    }
                    self.saw_text = true;
                    out.push(Part::Delta(JValue::obj([("content", JValue::from(delta))])));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = evt.get("delta").as_str().filter(|d| !d.is_empty()) {
                    out.push(Part::Delta(JValue::obj([(
                        "reasoning_content",
                        JValue::from(delta),
                    )])));
                }
            }
            "response.output_item.added" => {
                if let Some(item) = item {
                    if item.get("type").as_str() == Some("function_call") {
                        if let Some(name) = item.get("name").as_str() {
                            let index = self.open_index(item.get("id"));
                            out.push(tool_open(
                                index,
                                call_id(item),
                                JValue::from(name),
                                JValue::from(""),
                            ));
                        }
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let (Some(index), Some(delta)) =
                    (self.index_of(evt.get("item_id")), evt.get("delta").as_str())
                {
                    let k = self.key(evt.get("item_id"));
                    self.args_sent.push(k);
                    out.push(tool_args(index, delta));
                }
            }
            "response.output_item.done" => {
                if let Some(item) = item {
                    if item.get("type").as_str() == Some("message") {
                        self.message_done(item, &mut out);
                    }
                    if item.get("type").as_str() == Some("function_call") {
                        let index = match self.index_of(item.get("id")) {
                            Some(i) => i,
                            None => {
                                let i = self.open_index(item.get("id"));
                                let name = crate::js::or_str(item.get("name"), "");
                                out.push(tool_open(
                                    i,
                                    call_id(item),
                                    JValue::from(name),
                                    JValue::from(""),
                                ));
                                i
                            }
                        };
                        let k = self.key(item.get("id"));
                        let sent = self.args_sent.contains(&k);
                        if let Some(args) = item
                            .get("arguments")
                            .as_str()
                            .filter(|a| !sent && !a.is_empty())
                        {
                            self.args_sent.push(k);
                            out.push(tool_args(index, args));
                        }
                    }
                }
            }
            kind @ ("response.completed" | "response.incomplete") => {
                let response = evt.get("response");
                if let JValue::Arr(outputs) = response.get("output") {
                    for o in outputs.iter().filter(|o| o.is_object()) {
                        if o.get("type").as_str() == Some("message") {
                            self.message_done(o, &mut out);
                        } else if o.get("type").as_str() == Some("function_call")
                            && self.index_of(o.get("id")).is_none()
                        {
                            if let Some(name) = o.get("name").as_str() {
                                let index = self.open_index(o.get("id"));
                                let args = o.get("arguments").as_str().unwrap_or("");
                                out.push(tool_open(
                                    index,
                                    call_id(o),
                                    JValue::from(name),
                                    JValue::from(args),
                                ));
                            }
                        }
                    }
                }
                let usage = usage_of(response.get("usage"));
                let reason = if kind == "response.incomplete" {
                    response.get("incomplete_details").get("reason").as_str()
                } else {
                    None
                };
                let finish = match reason {
                    Some("max_output_tokens") => "length",
                    Some("content_filter") => "content_filter",
                    _ if self.saw_tool => "tool_calls",
                    _ => "stop",
                };
                out.push(Part::Finish {
                    finish: finish.to_string(),
                    usage,
                });
                self.ended = true;
            }
            "response.failed" => {
                let response = evt.get("response");
                let target = if response.get("error").truthy() {
                    response.get("error")
                } else {
                    response
                };
                let fallback = self.texts.could_not_finish();
                let message = self.texts.error_text(target, &fallback, self.now_ms);
                out.push(Part::Error {
                    message,
                    status: Some(502),
                });
                self.ended = true;
            }
            "error" => {
                let fallback = self.texts.returned_error();
                let message = self.texts.error_text(evt, &fallback, self.now_ms);
                let known = crate::texts::error_object(evt)
                    .and_then(|e| self.texts.known_error(&e, self.now_ms))
                    .is_some();
                out.push(Part::Error {
                    message,
                    status: Some(if known { 429 } else { 502 }),
                });
                self.ended = true;
            }
            _ => {}
        }
        out
    }

    /// The stream ended. Without a finish, core's "ended early" error (no status).
    pub fn end(&mut self) -> Vec<Part> {
        if self.ended {
            return Vec::new();
        }
        self.ended = true;
        vec![Part::Error {
            message: self.texts.ended_early(),
            status: None,
        }]
    }
}

/// All parts of a whole byte stream, cut into `chunks` (a test and replay helper).
pub fn parts_of(texts: &Texts, now_ms: f64, chunks: &[&[u8]]) -> Vec<Part> {
    let mut decoder = SseDecoder::new();
    let mut parts = ChatParts::new(texts.clone(), now_ms);
    let mut out = Vec::new();
    for chunk in chunks {
        for evt in decoder.push(chunk) {
            out.extend(parts.feed(&evt));
            if parts.ended() {
                return out;
            }
        }
    }
    if let Some(evt) = decoder.finish() {
        out.extend(parts.feed(&evt));
    }
    out.extend(parts.end());
    out
}
