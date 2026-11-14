//! The chat parts as the chat loop reads them: a `/chat/completions` SSE stream (`chatStream`) or
//! one completion (`chatCompletion`), and the transport's own error answers (`errorResponse`).

use crate::events::Part;
use crate::request::Refusal;
use crate::texts::Texts;
use js_json::JValue;

/// The response header that tells the chat loop the error message is safe to show as is.
pub const PROVIDER_MESSAGE_HEADER: &str = "x-noevia-provider-message";

/// `errorResponse(status, message, code)`: the JSON body (content-type application/json, plus
/// [`PROVIDER_MESSAGE_HEADER`]: 1).
pub fn error_body(refusal: &Refusal) -> String {
    let mut error = vec![("message".to_string(), JValue::from(refusal.message.as_str()))];
    if let Some(code) = refusal.code.as_deref().filter(|c| !c.is_empty()) {
        error.push(("code".to_string(), JValue::from(code)));
    }
    js_json::stringify(&JValue::obj([("error", JValue::Obj(error))])).unwrap_or_default()
}

/// The fixed fields of every frame of one reply (core: `chatcmpl-<random uuid>`, the second it began).
#[derive(Debug, Clone, PartialEq)]
pub struct ReplyId {
    pub id: String,
    pub created: f64,
    pub model: String,
}

impl ReplyId {
    fn chunk(&self, delta: JValue, finish: JValue) -> String {
        frame(&JValue::obj([
            ("id", JValue::from(self.id.as_str())),
            ("object", JValue::from("chat.completion.chunk")),
            ("created", JValue::Num(self.created)),
            ("model", JValue::from(self.model.as_str())),
            (
                "choices",
                JValue::Arr(vec![JValue::obj([("index", JValue::Num(0.0)), ("delta", delta), ("finish_reason", finish)])]),
            ),
        ]))
    }
}

fn frame(v: &JValue) -> String {
    format!("data: {}\n\n", js_json::stringify(v).unwrap_or_default())
}

pub const DONE: &str = "data: [DONE]\n\n";

/// How a reply starts, given its first part (core reads one part before answering, so an error
/// that comes before any output is a real HTTP error, not 200 and a cut stream).
#[derive(Debug, Clone, PartialEq)]
pub enum Start {
    /// Answer with this error (status, `errorResponse`).
    Refuse(Refusal),
    /// Answer 200 `text/event-stream` and send these frames, then [`ChatStream::frames`] per part.
    Stream(Vec<String>),
}

/// `chatStream(parts, first, model)` as a frame writer.
#[derive(Debug, Clone)]
pub struct ChatStream {
    reply: ReplyId,
    source: String,
    closed: bool,
}

impl ChatStream {
    /// The start of a streamed reply.
    pub fn start(texts: &Texts, reply: ReplyId, first: Option<&Part>) -> (Start, Option<ChatStream>) {
        if let Some(Part::Error { message, status }) = first {
            return (Start::Refuse(Refusal::new(status.unwrap_or(502), message.clone())), None);
        }
        let mut s = ChatStream { reply, source: texts.source.clone(), closed: false };
        let mut frames = vec![s.reply.chunk(JValue::obj([("role", JValue::from("assistant"))]), JValue::Null)];
        match first {
            Some(p) => frames.extend(s.frames(Some(p))),
            None => frames.extend(s.frames(None)),
        }
        (Start::Stream(frames), Some(s))
    }

    /// The frames for one more part (`None`: the parts ended).
    pub fn frames(&mut self, part: Option<&Part>) -> Vec<String> {
        if self.closed {
            return Vec::new();
        }
        match part {
            None => {
                self.closed = true;
                vec![DONE.to_string()]
            }
            Some(Part::Error { message, .. }) => {
                self.closed = true;
                vec![
                    frame(&JValue::obj([(
                        "error",
                        JValue::obj([("message", JValue::from(message.as_str())), ("source", JValue::from(self.source.as_str()))]),
                    )])),
                    DONE.to_string(),
                ]
            }
            Some(Part::Delta(delta)) => vec![self.reply.chunk(delta.clone(), JValue::Null)],
            Some(Part::Finish { finish, usage }) => {
                let mut out = vec![self.reply.chunk(JValue::obj::<&str>([]), JValue::from(finish.as_str()))];
                if usage.truthy() {
                    out.push(frame(&JValue::obj([
                        ("id", JValue::from(self.reply.id.as_str())),
                        ("object", JValue::from("chat.completion.chunk")),
                        ("created", JValue::Num(self.reply.created)),
                        ("model", JValue::from(self.reply.model.as_str())),
                        ("choices", JValue::Arr(Vec::new())),
                        ("usage", usage.clone()),
                    ])));
                }
                out
            }
        }
    }
}

/// `chatCompletion(body, model)`: the parts folded into one `chat.completion` (or the error).
pub fn completion(reply: &ReplyId, parts: &[Part]) -> Result<String, Refusal> {
    let mut content = String::new();
    let mut reasoning = String::new();
    // (id, name, arguments) per tool_calls index.
    let mut calls: Vec<Option<(JValue, String, String)>> = Vec::new();
    let mut finish = "stop".to_string();
    let mut usage = JValue::Null;
    for part in parts {
        match part {
            Part::Error { message, status } => return Err(Refusal::new(status.unwrap_or(502), message.clone())),
            Part::Finish { finish: f, usage: u } => {
                finish.clone_from(f);
                usage = u.clone();
            }
            Part::Delta(delta) => {
                if let Some(c) = delta.get("content").as_str() {
                    content.push_str(c);
                }
                if let Some(r) = delta.get("reasoning_content").as_str() {
                    reasoning.push_str(r);
                }
                if let JValue::Arr(tcs) = delta.get("tool_calls") {
                    for tc in tcs {
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let index = tc.get("index").as_num().unwrap_or(0.0) as usize;
                        if calls.len() <= index {
                            calls.resize(index + 1, None);
                        }
                        let Some(slot) = calls.get_mut(index) else { continue };
                        let entry = slot.get_or_insert_with(|| (tc.get("id").clone(), String::new(), String::new()));
                        if tc.get("id").truthy() {
                            entry.0 = tc.get("id").clone();
                        }
                        if let Some(n) = tc.get("function").get("name").as_str() {
                            entry.1.push_str(n);
                        }
                        if let Some(a) = tc.get("function").get("arguments").as_str() {
                            entry.2.push_str(a);
                        }
                    }
                }
            }
        }
    }
    let mut message = vec![
        ("role".to_string(), JValue::from("assistant")),
        ("content".to_string(), if content.is_empty() { JValue::Null } else { JValue::from(content) }),
    ];
    if !reasoning.is_empty() {
        message.push(("reasoning_content".to_string(), JValue::from(reasoning)));
    }
    let calls: Vec<JValue> = calls
        .into_iter()
        .flatten()
        .map(|(id, name, arguments)| {
            JValue::obj([
                ("id", id),
                ("type", JValue::from("function")),
                ("function", JValue::obj([("name", JValue::from(name)), ("arguments", JValue::from(arguments))])),
            ])
        })
        .collect();
    if !calls.is_empty() {
        message.push(("tool_calls".to_string(), JValue::Arr(calls)));
    }
    let mut out = vec![
        ("id".to_string(), JValue::from(reply.id.as_str())),
        ("object".to_string(), JValue::from("chat.completion")),
        ("created".to_string(), JValue::Num(reply.created)),
        ("model".to_string(), JValue::from(reply.model.as_str())),
        (
            "choices".to_string(),
            JValue::Arr(vec![JValue::obj([
                ("index", JValue::Num(0.0)),
                ("message", JValue::Obj(message)),
                ("finish_reason", JValue::from(finish)),
            ])]),
        ),
    ];
    if usage.truthy() {
        out.push(("usage".to_string(), usage));
    }
    Ok(js_json::stringify(&JValue::Obj(out)).unwrap_or_default())
}
