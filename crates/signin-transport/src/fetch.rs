//! `fetchFor(userId, { conversation })(url, init)` as one call: core's whole sequence from the
//! request checks to the answer the chat loop reads, over the caller's broker and backend
//! ([`Wire`]). The pieces it drives are the other modules; this one owns only the order and the
//! edges core handles inline: an answer already labelled as a provider message is passed on, a
//! failed or body-less answer is read at most 64 KiB, the first stream read failing is a 502, a
//! later one errors the stream, and the second 401 forgets the cached session.
//!
//! Named differences from core (each pinned by a test):
//! - A catalogue that is not an array is "could not be read" (core: `catalogue?.find` throws a
//!   TypeError and the fetch rejects).
//! - A body-less answer with status 204, 205 or 304 is a 502 with core's line (core builds its error
//!   answer with that status, which the Response constructor refuses: the fetch rejects). 101 and
//!   103 map the same way; a fetch never hands an informational answer on, so no case reaches it.
//! - A catalogue entry that is not an object (`null`, a number) matches no model (core reads
//!   `m.slug` on it and throws).
//! - A request body that is JSON `null` is the 400 "not JSON" answer (core reads `body.model` on it
//!   and throws).
//! - One clock reading per call ([`Call::now_ms`]); core reads the clock again after each broker
//!   await, so a cache entry stored after a slow broker answer lives up to that long more here.

use crate::events::{ChatParts, Part, SseDecoder};
use crate::reply::{completion, ChatStream, ReplyId, Start};
use crate::request::{self, headers, Protocol, Refusal};
use crate::session::{Authorise, BrokerAnswer, SessionCache, Step};
use crate::texts::Texts;
use js_json::JValue;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Mutex;

/// `readCappedText(r, 64 * 1024)` on a failed answer.
pub const ERROR_BODY_CAP: usize = 64 * 1024;
/// The accept header of every backend call.
pub const ACCEPT: &str = "text/event-stream";

/// A backend answer's body, read chunk by chunk. Dropping it cancels the rest (core cancels a
/// 401's body before asking again, and stops reading once a reply ended).
pub trait Body: Send {
    /// The next chunk; `None` at the end; `Err(message)` when the read failed.
    fn chunk(&mut self) -> impl Future<Output = Option<Result<Vec<u8>, String>>> + Send;
}

/// The backend's answer.
pub struct Upstream<B> {
    pub status: u16,
    /// It carried `x-noevia-provider-message: 1` itself: core passes such an answer on as is.
    pub labelled: bool,
    /// The answer's headers as received (names in lower case, in order). Only a pass-through
    /// answer uses them: it goes on with its own headers.
    pub headers: Vec<(String, String)>,
    /// `None`: an answer without a body (fetch's `r.body === null`).
    pub body: Option<B>,
}

impl<B> std::fmt::Debug for Upstream<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upstream")
            .field("status", &self.status)
            .field("labelled", &self.labelled)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("body", &self.body.as_ref().map(|_| "…"))
            .finish()
    }
}

/// One backend call: `POST url` with these headers and body. The caller refuses redirects (core:
/// `redirect: 'error'`, a redirect rejects the fetch) and ties the call to the turn's abort.
pub struct Outgoing<'a> {
    pub url: &'a str,
    /// Core's order. Holds the bearer token: never log it.
    pub headers: &'a [(String, String)],
    pub body: &'a str,
}

impl std::fmt::Debug for Outgoing<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("Outgoing")
            .field("url", &self.url)
            .field("headers", &names)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// The broker (in the front: the sign-in routes' own state) and the backend.
pub trait Wire: Send {
    type Body: Body;
    /// Core's `signal()`: the broker's change signal; `None` when it cannot be read (then nothing
    /// is cached).
    fn signal(&self) -> Option<String>;
    /// The account's catalogue (`models`), `None` when the broker refused or could not be reached.
    fn catalogue(&mut self, user: &str) -> impl Future<Output = Option<JValue>> + Send;
    /// A usable session; on the second ask with the token the backend refused.
    fn session(
        &mut self,
        user: &str,
        rejected_token: Option<&str>,
    ) -> impl Future<Output = BrokerAnswer> + Send;
    /// Mark this sign-in's generation as failed. Its own failure is ignored, as in core.
    fn reject(&mut self, user: &str, generation: &JValue) -> impl Future<Output = ()> + Send;
    /// Send one backend call. `Err(message)`: the fetch itself failed (network, abort, redirect).
    fn send(
        &mut self,
        request: Outgoing<'_>,
    ) -> impl Future<Output = Result<Upstream<Self::Body>, String>> + Send;
}

/// One call of the chat loop's fetch.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    pub texts: &'a Texts,
    pub protocol: &'a Protocol,
    /// The backend's base address (core's configured backend base, trailing slashes dropped).
    pub base_url: &'a str,
    pub user: &'a str,
    /// The chat's conversation id; the backend sees only [`request::uuid_from`] of user and it.
    pub conversation: Option<&'a str>,
    pub url: &'a str,
    pub body: &'a str,
    /// `Date.now()` for this call (cache times, reset times, the reply's `created`).
    pub now_ms: f64,
    /// `crypto.randomUUID()` of the reply id (`chatcmpl-<uuid>`).
    pub reply_uuid: &'a str,
}

/// What the chat loop receives.
#[derive(Debug)]
pub enum Answer<B> {
    /// Core's own error answer: `application/json`, `x-noevia-provider-message: 1`,
    /// [`crate::reply::error_body`].
    Error(Refusal),
    /// The backend's answer, labelled by the backend itself: passed on unchanged.
    Passthrough(Upstream<B>),
    /// 200 `application/json`: one completion.
    Completion(String),
    /// 200 `text/event-stream`.
    Stream(Box<Streaming<B>>),
    /// Core's fetch rejected with this message: the chat loop treats it as a failed fetch.
    Thrown(String),
}

/// The chat parts read from a backend body, one at a time (core's `chatParts(r.body)` generator).
struct PartReader<B> {
    body: Option<B>,
    decoder: SseDecoder,
    parts: ChatParts,
    queue: VecDeque<Part>,
}

impl<B: Body> PartReader<B> {
    fn new(texts: &Texts, now_ms: f64, body: B) -> Self {
        PartReader {
            body: Some(body),
            decoder: SseDecoder::new(),
            parts: ChatParts::new(texts.clone(), now_ms),
            queue: VecDeque::new(),
        }
    }

    /// The next part; `Ok(None)` when the generator is done; `Err` when the body read failed.
    async fn next(&mut self) -> Result<Option<Part>, String> {
        loop {
            if let Some(p) = self.queue.pop_front() {
                return Ok(Some(p));
            }
            if self.parts.ended() {
                // The reply ended: the rest of the body is never read (dropping it cancels it).
                self.body = None;
                return Ok(None);
            }
            let Some(body) = self.body.as_mut() else {
                return Ok(None);
            };
            match body.chunk().await {
                Some(Ok(chunk)) => {
                    for evt in self.decoder.push(&chunk) {
                        self.queue.extend(self.parts.feed(&evt));
                        if self.parts.ended() {
                            break;
                        }
                    }
                }
                Some(Err(e)) => {
                    self.body = None;
                    return Err(e);
                }
                None => {
                    self.body = None;
                    if let Some(evt) = self.decoder.finish() {
                        self.queue.extend(self.parts.feed(&evt));
                    }
                    self.queue.extend(self.parts.end());
                }
            }
        }
    }
}

/// A streamed reply after its first part (core's `chatStream`): the head frames first, then
/// [`Streaming::next`] until `None`.
pub struct Streaming<B> {
    head: Vec<String>,
    stream: ChatStream,
    reader: PartReader<B>,
    /// A read failed: the stream is errored, nothing follows.
    failed: bool,
}

impl<B> std::fmt::Debug for Streaming<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Streaming")
            .field("head_frames", &self.head.len())
            .finish()
    }
}

impl<B: Body> Streaming<B> {
    /// The frames ready at once (the role chunk and the first part's frames).
    pub fn take_head(&mut self) -> Vec<String> {
        std::mem::take(&mut self.head)
    }

    /// The frames of the next part. `None`: the stream is closed. `Err(message)`: the backend
    /// read failed; core errors the stream (the chat loop's read rejects), nothing more follows.
    pub async fn next(&mut self) -> Option<Result<Vec<String>, String>> {
        if !self.head.is_empty() {
            return Some(Ok(self.take_head()));
        }
        if self.failed {
            return None;
        }
        let part = match self.reader.next().await {
            Ok(p) => p,
            Err(e) => {
                self.failed = true;
                return Some(Err(e));
            }
        };
        let frames = self.stream.frames(part.as_ref());
        (!frames.is_empty()).then_some(Ok(frames))
    }
}

/// `readCappedText(r, cap)`: at most `cap` bytes, UTF-8 with replacement; a character cut at the
/// cap is dropped. `Err` when a read failed (core then uses an empty text).
pub async fn read_capped<B: Body>(body: &mut B, cap: usize) -> Result<String, String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut capped = false;
    while buf.len() < cap {
        match body.chunk().await {
            None => break,
            Some(Err(e)) => return Err(e),
            Some(Ok(v)) => {
                let room = cap - buf.len();
                if v.len() > room {
                    buf.extend_from_slice(v.get(..room).unwrap_or(&[]));
                    capped = true;
                    break;
                }
                buf.extend_from_slice(&v);
            }
        }
    }
    if buf.len() >= cap && !capped && matches!(body.chunk().await, Some(Ok(_))) {
        capped = true;
    }
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if capped && text.ends_with('\u{FFFD}') {
        text.pop();
    }
    Ok(text)
}

fn lock(cache: &Mutex<SessionCache>) -> std::sync::MutexGuard<'_, SessionCache> {
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Statuses whose answer cannot carry a body (the Response constructor refuses one).
fn null_body_status(status: u16) -> bool {
    matches!(status, 101 | 103 | 204 | 205 | 304)
}

/// The whole of core's fetch of the sign-in provider (`fetchFor(user)(url, init)`). `cache` is the process's session cache (held
/// only between awaits, never across one).
pub async fn fetch<W: Wire>(
    wire: &mut W,
    cache: &Mutex<SessionCache>,
    call: &Call<'_>,
) -> Answer<W::Body> {
    let t = call.texts;
    let parsed = match request::parse(t, call.url, call.body) {
        Ok(p) => p,
        Err(r) => return Answer::Error(r),
    };
    // Named difference: a catalogue that is not an array counts as unreadable (core throws).
    let catalogue = wire
        .catalogue(call.user)
        .await
        .filter(|c| matches!(c, JValue::Arr(_)));
    let conversation = call
        .conversation
        .map(|c| request::uuid_from(&format!("{}:{c}", call.user)));
    let planned = match request::plan(t, &parsed, catalogue.as_ref(), conversation.as_deref()) {
        Ok(p) => p,
        Err(r) => return Answer::Error(r),
    };
    let url = request::responses_url(call.base_url);

    // authorised(userId, send): the refresh-once-on-401 rule.
    let mut auth = Authorise::new();
    let mut step = auth.start();
    let upstream = loop {
        match step {
            Step::AskBroker { rejected_token } => {
                let mark = wire.signal();
                let hit = lock(cache).lookup(
                    call.user,
                    mark.as_deref(),
                    rejected_token.is_some(),
                    call.now_ms,
                );
                if let Some(hit) = hit {
                    step = auth.on_session(t, BrokerAnswer::Ok(hit));
                    continue;
                }
                let answer = wire.session(call.user, rejected_token.as_deref()).await;
                if let BrokerAnswer::Ok(sess) = &answer {
                    lock(cache).store(call.user, sess, mark.as_deref(), call.now_ms);
                }
                step = auth.on_session(t, answer);
            }
            Step::Send(sess) => {
                let h = headers(call.protocol, &sess, ACCEPT, conversation.as_deref());
                let sent = wire
                    .send(Outgoing {
                        url: &url,
                        headers: &h,
                        body: &planned.body,
                    })
                    .await;
                let r = match sent {
                    Ok(r) => r,
                    Err(e) => return Answer::Thrown(e),
                };
                step = auth.on_status(t, r.status);
                if step == Step::Pass {
                    break r;
                }
                // A 401: `r` and its body are dropped (cancelled) here, before the next step.
            }
            Step::Reject { generation, then } => {
                // Only the sign-in that failed: forget its cached session, tell the broker.
                lock(cache).forget(call.user);
                wire.reject(call.user, &generation).await;
                return Answer::Error(then);
            }
            Step::Refuse(r) => return Answer::Error(r),
            Step::Pass => {
                return Answer::Error(Refusal::new(502, t.could_not_finish()));
            }
        }
    };

    if upstream.labelled {
        return Answer::Passthrough(upstream);
    }
    let status = upstream.status;
    let body = match upstream.body {
        Some(b) if (200..300).contains(&status) => b,
        rest => {
            let text = match rest {
                Some(mut b) => read_capped(&mut b, ERROR_BODY_CAP)
                    .await
                    .unwrap_or_default(),
                None => String::new(),
            };
            let line = t.upstream_message(&text, status, call.now_ms);
            // Named difference: core's error answer with a null-body status throws.
            let out = if status == 0 || null_body_status(status) {
                502
            } else {
                status
            };
            return Answer::Error(Refusal::new(out, line));
        }
    };

    let reply = ReplyId {
        id: format!("chatcmpl-{}", call.reply_uuid),
        created: (call.now_ms / 1000.0).floor(),
        model: planned.model.clone(),
    };
    let mut reader = PartReader::new(t, call.now_ms, body);
    if planned.stream {
        let first = match reader.next().await {
            Ok(p) => p,
            Err(e) => return Answer::Error(Refusal::new(502, t.stream_failed(&e))),
        };
        return match ChatStream::start(t, reply, first.as_ref()) {
            (Start::Refuse(r), _) => Answer::Error(r),
            (Start::Stream(head), Some(stream)) => Answer::Stream(Box::new(Streaming {
                head,
                stream,
                reader,
                failed: false,
            })),
            (Start::Stream(_), None) => Answer::Error(Refusal::new(502, t.could_not_finish())),
        };
    }
    let mut parts = Vec::new();
    loop {
        match reader.next().await {
            Ok(Some(p)) => {
                let error = matches!(p, Part::Error { .. });
                parts.push(p);
                if error {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => return Answer::Thrown(e),
        }
    }
    match completion(&reply, &parts) {
        Ok(body) => Answer::Completion(body),
        Err(r) => Answer::Error(r),
    }
}
