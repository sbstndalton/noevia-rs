//! The streaming model client of the Rust front (sbstndalton/noevia#2218): one
//! `/v1/chat/completions` request with `stream: true`, the shape every local and machine engine
//! speaks. Deep research is the first caller; the chat turn is meant to use it unchanged.
//!
//! - [`Decoder`]: the SSE parse, as core chat.cjs reads a stream: lines ending in `\n`, split across
//!   chunks at any byte, `data:` only, `[DONE]` skipped, an unreadable line skipped. Reasoning
//!   (`reasoning_content`, `reasoning`) never joins the answer. Tool-call fragments are assembled by
//!   index. An `error` object in the stream is a provider error.
//! - [`run`]: the request. It ends with the [`Completion`] or one [`Error`], at the first of: the
//!   caller's [`Cancel`], the whole-request deadline, the idle deadline, the size cap. Whichever it is,
//!   the response is dropped, which closes the connection, so the engine stops generating.
//! - [`net`]: the on-this-network rule of core service-endpoint.cjs, for a model on another machine.
//!
//! The client never follows a redirect and never uses a proxy (`client`, `net::this_network_client`).
pub mod net;

use js_json::JValue;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// One line of a stream may be this long (a tool call's arguments arrive in fragments).
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// core research-model.cjs `readCappedJson(r, 4 MiB)`: the most a whole reply may be.
pub const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;
/// The most of an error body read for its reason.
const MAX_ERROR_BYTES: usize = 64 * 1024;
const ERROR_BODY_WAIT: Duration = Duration::from_secs(5);
const MAX_TOOL_CALLS: u64 = 32;

/// A cancel that many tasks may wait on. Cloning shares it; a cancel is permanent.
#[derive(Clone, Debug)]
pub struct Cancel(Arc<watch::Sender<bool>>);

impl Default for Cancel {
    fn default() -> Self {
        Cancel(Arc::new(watch::channel(false).0))
    }
}

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    /// Resolves when cancelled (at once when it already is).
    pub async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        // The sender is `self`'s, so this cannot end by the channel closing.
        let _ = rx.wait_for(|c| *c).await;
    }
    /// Cancels this one when `other` is cancelled, for as long as the returned task runs.
    pub fn follow(&self, other: &Cancel) -> tokio::task::JoinHandle<()> {
        let (me, other) = (self.clone(), other.clone());
        tokio::spawn(async move {
            other.cancelled().await;
            me.cancel();
        })
    }
}

/// What a stream says, as it says it.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Answer text.
    Delta(String),
    /// Reasoning text: not part of the answer.
    Reasoning(String),
    /// A fragment of tool call `index`; the fragments of one call concatenate.
    ToolCall {
        index: u64,
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
    /// The frame's cumulative `usage` object.
    Usage(Value),
    /// llama-server's `timings` fields of this frame (they may be split across frames).
    Timings(Value),
    /// `finish_reason`.
    Finish(String),
}

/// One assembled tool call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// What a whole stream came to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Completion {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    /// The last `usage`.
    pub usage: Option<Value>,
    /// The merged `timings`.
    pub timings: Option<Value>,
    /// `[DONE]` was seen.
    pub done: bool,
}

/// Why a request produced no completion.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The caller's [`Cancel`].
    Cancelled,
    /// The whole-request deadline.
    Deadline,
    /// Nothing arrived for the idle limit.
    Idle,
    /// No answer at the socket. `redirect` is a 3xx, which is never followed.
    Connect {
        detail: String,
        refused_address: bool,
    },
    /// A non-2xx answer: the status and the first of its body.
    Status { status: u16, body: String },
    /// An `error` object inside the stream: its JSON text and its numeric `code`, if any.
    Provider { error: String, code: Option<u32> },
    /// The reply passed [`MAX_REPLY_BYTES`] or a line [`MAX_LINE_BYTES`].
    TooLarge,
    /// The stream ended with no `finish_reason` and no `[DONE]`.
    Truncated,
    /// The tool-call fragments do not make whole calls.
    BadToolCalls,
}

impl Error {
    /// A 3xx, which is never followed.
    pub fn is_redirect(&self) -> bool {
        matches!(self, Error::Status { status, .. } if (300..400).contains(status))
    }
    /// The upstream's reason, classified and sanitised (provider-error), for a status or stream error.
    pub fn classified(&self) -> Option<provider_error::Classified> {
        match self {
            Error::Status { status, body } => {
                Some(provider_error::classify(u32::from(*status), body))
            }
            Error::Provider { error, code } => {
                Some(provider_error::classify(code.unwrap_or(500), error))
            }
            _ => None,
        }
    }
}

/// The SSE parse: feed it bytes in any cut, take the events, then [`Decoder::finish`].
#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
    answer: Completion,
    calls: BTreeMap<u64, ToolCall>,
    reply_bytes: usize,
}

impl Decoder {
    pub fn feed(&mut self, chunk: &[u8], emit: &mut dyn FnMut(Event)) -> Result<(), Error> {
        self.reply_bytes = self.reply_bytes.saturating_add(chunk.len());
        if self.reply_bytes > MAX_REPLY_BYTES {
            return Err(Error::TooLarge);
        }
        self.pending.extend_from_slice(chunk);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.pending.drain(..=end).collect();
            self.line(&raw, emit)?;
        }
        if self.pending.len() > MAX_LINE_BYTES {
            return Err(Error::TooLarge);
        }
        Ok(())
    }

    fn line(&mut self, raw: &[u8], emit: &mut dyn FnMut(Event)) -> Result<(), Error> {
        // A line cut inside a character only ever arrives whole here: bytes are kept until the newline.
        let Ok(line) = std::str::from_utf8(raw) else {
            return Ok(());
        };
        let line = line.trim();
        let Some(payload) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            self.answer.done = true;
            return Ok(());
        }
        let Ok(frame) = serde_json::from_str::<Value>(payload) else {
            return Ok(());
        };
        if let Some(error) = frame.get("error").filter(|e| truthy(e)) {
            let code = error
                .get("code")
                .and_then(Value::as_u64)
                .and_then(|c| u32::try_from(c).ok());
            let error = match error {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            return Err(Error::Provider { error, code });
        }
        if let Some(usage) = frame.get("usage").filter(|u| truthy(u)) {
            self.answer.usage = Some(usage.clone());
            emit(Event::Usage(usage.clone()));
        }
        if let Some(timings) = frame.get("timings").filter(|t| t.is_object()) {
            let merged = self
                .answer
                .timings
                .get_or_insert_with(|| serde_json::json!({}));
            if let (Some(into), Some(from)) = (merged.as_object_mut(), timings.as_object()) {
                for (k, v) in from {
                    into.insert(k.clone(), v.clone());
                }
            }
            emit(Event::Timings(timings.clone()));
        }
        let Some(choice) = frame.get("choices").and_then(|c| c.get(0)) else {
            return Ok(());
        };
        if let Some(delta) = choice.get("delta") {
            for (field, is_answer) in [
                ("reasoning_content", false),
                ("reasoning", false),
                ("content", true),
            ] {
                let Some(text) = delta.get(field).and_then(Value::as_str) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                if is_answer {
                    self.answer.content.push_str(text);
                    emit(Event::Delta(text.to_owned()));
                } else {
                    self.answer.reasoning.push_str(text);
                    emit(Event::Reasoning(text.to_owned()));
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.tool_fragment(call, emit)?;
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.answer.finish_reason = Some(reason.to_owned());
            emit(Event::Finish(reason.to_owned()));
        }
        Ok(())
    }

    fn tool_fragment(&mut self, call: &Value, emit: &mut dyn FnMut(Event)) -> Result<(), Error> {
        // core's loop files a fragment with no index under the first call.
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        if index >= MAX_TOOL_CALLS {
            return Err(Error::BadToolCalls);
        }
        fn text(v: Option<&Value>) -> Option<&str> {
            v.and_then(Value::as_str).filter(|s| !s.is_empty())
        }
        let id = text(call.get("id"));
        let function = call.get("function");
        let name = text(function.and_then(|f| f.get("name")));
        let arguments = text(function.and_then(|f| f.get("arguments")));
        let entry = self.calls.entry(index).or_default();
        // An id or a name is repeated by some servers on every fragment: the first one stands.
        if let (Some(id), true) = (id, entry.id.is_empty()) {
            entry.id = id.to_owned();
        }
        if let (Some(name), true) = (name, entry.name.is_empty()) {
            entry.name = name.to_owned();
        }
        if let Some(arguments) = arguments {
            entry.arguments.push_str(arguments);
        }
        emit(Event::ToolCall {
            index,
            id: id.map(str::to_owned),
            name: name.map(str::to_owned),
            arguments: arguments.map(str::to_owned),
        });
        Ok(())
    }

    /// The stream ended. A last line with no newline still counts.
    pub fn finish(mut self, emit: &mut dyn FnMut(Event)) -> Result<Completion, Error> {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            self.line(&rest, emit)?;
        }
        if self.answer.finish_reason.is_none() && !self.answer.done {
            return Err(Error::Truncated);
        }
        for (index, (at, call)) in self.calls.into_iter().enumerate() {
            if at != index as u64 || call.name.is_empty() {
                return Err(Error::BadToolCalls);
            }
            self.answer.tool_calls.push(call);
        }
        Ok(self.answer)
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => false,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => true,
    }
}

/// One request's limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Everything: connecting, the wait for a first byte, the whole reply.
    pub deadline: Duration,
    /// The most time between two chunks, after the answer started. `None`: only `deadline`.
    pub idle: Option<Duration>,
}

/// One request. `headers` are sent as given; nothing is added but the content type.
pub struct Request<'a> {
    pub url: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a str,
    pub limits: Limits,
}

/// `{ model, stream: true, max_tokens, messages }`, plus any `extra` keys (sampling) after them.
pub fn chat_body(
    model: &str,
    max_tokens: u64,
    messages: &JValue,
    extra: &[(String, JValue)],
) -> String {
    let mut items = vec![
        ("model".to_owned(), JValue::Str(model.to_owned())),
        ("stream".to_owned(), JValue::Bool(true)),
        ("max_tokens".to_owned(), JValue::Num(max_tokens as f64)),
        ("messages".to_owned(), messages.clone()),
    ];
    items.extend(extra.iter().cloned());
    js_json::wtf16::stringify(&JValue::Obj(items)).unwrap_or_default()
}

/// The client for this machine's engine: no redirects, no proxy.
pub fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|e| format!("model client: {e}"))
}

fn connect_error(e: &reqwest::Error) -> Error {
    let refused_address = net::refused_in_chain(e);
    Error::Connect {
        detail: e.to_string(),
        refused_address,
    }
}

async fn read_error_body(mut response: reqwest::Response, status: u16) -> Error {
    let mut bytes = Vec::new();
    let reading = async {
        while let Ok(Some(chunk)) = response.chunk().await {
            bytes.extend_from_slice(&chunk);
            if bytes.len() >= MAX_ERROR_BYTES {
                bytes.truncate(MAX_ERROR_BYTES);
                break;
            }
        }
    };
    let _ = tokio::time::timeout(ERROR_BODY_WAIT, reading).await;
    Error::Status {
        status,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// Sends the request and reads the stream. `emit` gets each event as it arrives. The future may also
/// simply be dropped: the connection closes with it.
pub async fn run(
    client: &reqwest::Client,
    request: Request<'_>,
    cancel: &Cancel,
    emit: &mut (dyn FnMut(Event) + Send),
) -> Result<Completion, Error> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let deadline = Instant::now() + request.limits.deadline;
    let mut builder = client
        .post(request.url)
        .header("content-type", "application/json")
        .body(request.body.to_owned());
    for (k, v) in request.headers {
        builder = builder.header(k, v);
    }
    let sent = tokio::select! {
        () = cancel.cancelled() => return Err(Error::Cancelled),
        () = tokio::time::sleep_until(deadline) => return Err(Error::Deadline),
        r = builder.send() => r,
    };
    let mut response = sent.map_err(|e| connect_error(&e))?;
    let status = response.status();
    if !status.is_success() {
        let reading = read_error_body(response, status.as_u16());
        return tokio::select! {
            () = cancel.cancelled() => Err(Error::Cancelled),
            e = reading => Err(e),
        };
    }
    let mut decoder = Decoder::default();
    loop {
        // The idle limit starts over with each chunk; it is the wait for this chunk.
        let idle_at = request
            .limits
            .idle
            .map_or(deadline, |idle| deadline.min(Instant::now() + idle));
        let next = tokio::select! {
            () = cancel.cancelled() => return Err(Error::Cancelled),
            () = tokio::time::sleep_until(idle_at) => {
                return Err(if idle_at < deadline { Error::Idle } else { Error::Deadline });
            }
            c = response.chunk() => c,
        };
        match next {
            Ok(Some(chunk)) => decoder.feed(&chunk, &mut *emit)?,
            Ok(None) => return decoder.finish(&mut *emit),
            // A socket that dies mid-reply is a stream that ended early.
            Err(_) => return Err(Error::Truncated),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn events(wire: &[u8], cut: usize) -> (Result<Completion, Error>, Vec<Event>) {
        let mut d = Decoder::default();
        let mut out = Vec::new();
        for piece in wire.chunks(cut) {
            if let Err(e) = d.feed(piece, &mut |e| out.push(e)) {
                return (Err(e), out);
            }
        }
        (d.finish(&mut |e| out.push(e)), out)
    }

    const FRAMES: &str = concat!(
        ": keep-alive\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning\":\"more\"}}]}\r\n\r\n",
        "data:{\"choices\":[{\"delta\":{\"content\":\"h\u{e9}\"}}]}\n\n",
        "data: not json\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"t\",\"arguments\":\"{\\\"a\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"arguments\":\"\\\":1}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"tool_calls\"}],\"timings\":{\"a\":1}}\n\n",
        "data: {\"choices\":[],\"usage\":{\"total_tokens\":9},\"timings\":{\"b\":2}}\n\n",
        "data: [DONE]\n\n",
    );

    #[test]
    fn reasoning_and_answer_stay_apart_and_any_cut_gives_one_answer() {
        for cut in [1, 2, 3, 7, 4096] {
            let (done, seen) = events(FRAMES.as_bytes(), cut);
            let done = done.unwrap();
            assert_eq!(done.content, "h\u{e9}llo", "cut {cut}");
            assert_eq!(done.reasoning, "think more");
            assert_eq!(done.finish_reason.as_deref(), Some("tool_calls"));
            assert_eq!(
                done.tool_calls,
                vec![ToolCall {
                    id: "c1".into(),
                    name: "t".into(),
                    arguments: "{\"a\":1}".into()
                }]
            );
            assert_eq!(done.usage, Some(serde_json::json!({"total_tokens":9})));
            assert_eq!(done.timings, Some(serde_json::json!({"a":1,"b":2})));
            assert!(done.done);
            assert_eq!(seen[0], Event::Reasoning("think ".into()));
            assert!(seen.contains(&Event::Delta("h\u{e9}".into())));
        }
    }

    #[test]
    fn a_stream_error_is_a_provider_error_with_its_code() {
        let wire = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\ndata: {\"error\":{\"code\":429,\"message\":\"slow\"}}\n\n";
        let (done, seen) = events(wire, 5);
        assert_eq!(seen, vec![Event::Delta("x".into())]);
        match done.unwrap_err() {
            Error::Provider { code, error } => {
                assert_eq!(code, Some(429));
                assert!(error.contains("slow"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_early_end_is_truncated_and_the_caps_hold() {
        let (done, _) = events(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            9,
        );
        assert_eq!(done.unwrap_err(), Error::Truncated);
        // A last line with no newline still counts.
        let (done, _) = events(b"data: {\"choices\":[{\"finish_reason\":\"stop\"}]}", 9);
        assert_eq!(done.unwrap().finish_reason.as_deref(), Some("stop"));
        let long = vec![b'x'; MAX_LINE_BYTES + 1];
        assert_eq!(events(&long, 4096).0.unwrap_err(), Error::TooLarge);
        let mut big = Vec::new();
        while big.len() <= MAX_REPLY_BYTES {
            big.extend_from_slice(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n");
        }
        assert_eq!(events(&big, 4096).0.unwrap_err(), Error::TooLarge);
    }

    #[test]
    fn tool_fragments_must_make_whole_calls() {
        let gap = b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"name\":\"t\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
        assert_eq!(events(gap, 8).0.unwrap_err(), Error::BadToolCalls);
        let too_many = b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":99}]}}]}\n\n";
        assert_eq!(events(too_many, 8).0.unwrap_err(), Error::BadToolCalls);
    }

    /// A synthetic engine: answers with `head` then `chunks`, keeps the socket open after the last
    /// chunk when `hold`, and reports when the client's side of the connection closes.
    async fn engine(
        head: &'static str,
        chunks: Vec<&'static str>,
        hold: bool,
    ) -> (String, tokio::sync::oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut seen = Vec::new();
            loop {
                let n = s.read(&mut buf).await.unwrap();
                seen.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&seen);
                if let Some((h, b)) = text.split_once("\r\n\r\n") {
                    let lower = h.to_ascii_lowercase();
                    let want = lower
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if b.len() >= want {
                        break;
                    }
                }
            }
            s.write_all(head.as_bytes()).await.unwrap();
            for c in chunks {
                s.write_all(c.as_bytes()).await.unwrap();
                s.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if hold {
                // Nothing more is sent: only the client's close ends this read.
                let _ = s.read(&mut buf).await;
                let _ = closed_tx.send(());
            }
        });
        (url, closed_rx)
    }

    const SSE_HEAD: &str =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";

    async fn ask(url: &str, limits: Limits, cancel: &Cancel) -> Result<Completion, Error> {
        let c = client().unwrap();
        run(
            &c,
            Request {
                url,
                headers: &[],
                body: "{}",
                limits,
            },
            cancel,
            &mut |_| {},
        )
        .await
    }

    const LONG: Limits = Limits {
        deadline: Duration::from_secs(20),
        idle: None,
    };

    #[tokio::test]
    async fn a_whole_stream_over_a_socket() {
        let (url, _closed) = engine(
            SSE_HEAD,
            vec![
                "data: {\"choices\":[{\"delta\":{\"content\":\"o\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"k\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ],
            false,
        )
        .await;
        let done = ask(&url, LONG, &Cancel::new()).await.unwrap();
        assert_eq!(done.content, "ok");
    }

    #[tokio::test]
    async fn cancel_ends_the_request_and_closes_the_connection() {
        let (url, closed) = engine(
            SSE_HEAD,
            vec!["data: {\"choices\":[{\"delta\":{\"content\":\"o\"}}]}\n\n"],
            true,
        )
        .await;
        let cancel = Cancel::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            trigger.cancel();
        });
        assert_eq!(
            ask(&url, LONG, &cancel).await.unwrap_err(),
            Error::Cancelled
        );
        tokio::time::timeout(Duration::from_secs(5), closed)
            .await
            .expect("the engine saw the close")
            .unwrap();
    }

    #[tokio::test]
    async fn the_deadlines_end_a_silent_engine_and_close_the_connection() {
        let (url, closed) = engine(SSE_HEAD, vec![], true).await;
        let idle = Limits {
            deadline: Duration::from_secs(20),
            idle: Some(Duration::from_millis(100)),
        };
        assert_eq!(
            ask(&url, idle, &Cancel::new()).await.unwrap_err(),
            Error::Idle
        );
        tokio::time::timeout(Duration::from_secs(5), closed)
            .await
            .expect("the engine saw the close")
            .unwrap();
        let (url, _closed) = engine(SSE_HEAD, vec![], true).await;
        let whole = Limits {
            deadline: Duration::from_millis(100),
            idle: None,
        };
        assert_eq!(
            ask(&url, whole, &Cancel::new()).await.unwrap_err(),
            Error::Deadline
        );
    }

    #[tokio::test]
    async fn a_refusal_keeps_its_status_and_a_redirect_is_not_followed() {
        let (url, _c) = engine(
            "HTTP/1.1 503 Busy\r\ncontent-length: 2\r\nconnection: close\r\n\r\nno",
            vec![],
            false,
        )
        .await;
        match ask(&url, LONG, &Cancel::new()).await.unwrap_err() {
            Error::Status { status, body } => assert_eq!((status, body.as_str()), (503, "no")),
            other => panic!("{other:?}"),
        }
        let (url, _c) = engine(
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:9/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            vec![],
            false,
        )
        .await;
        assert!(ask(&url, LONG, &Cancel::new())
            .await
            .unwrap_err()
            .is_redirect());
    }

    #[test]
    fn the_body_has_core_order_and_extra_keys_last() {
        let messages = JValue::Arr(vec![JValue::obj([("role", JValue::Str("user".into()))])]);
        let extra = [("temperature".to_owned(), JValue::Num(0.5))];
        assert_eq!(
            chat_body("m", 600, &messages, &extra),
            r#"{"model":"m","stream":true,"max_tokens":600,"messages":[{"role":"user"}],"temperature":0.5}"#
        );
    }
}
