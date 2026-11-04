//! `POST /api/chat` (chat port S5, capability `rust-chat-turn`; its edge since S6, `rust-chat-edge`):
//! the front owns the route, answers everything routes/chat.cjs answered before the loop
//! (chat_edge.rs), and relays a turn that passes to Core's loopback `POST /internal/chat/turn` (core
//! rust-chat-turn.cjs), which runs the rest of the unchanged pipeline. Core refuses a POST /api/chat
//! that did not come through here.
//!
//! In order:
//! 1. Node's request gate, as identity.rs answers it: 401 signed out, 403 `invalid CSRF token`, 403
//!    `browser_session_required`.
//!    Node's inference gate answered before these, so a refusal is first offered to Core's
//!    `POST /internal/chat/gate`: when the gate is held its 503 (`Retry-After: 3` and the retryable
//!    body) is the answer, as Node gave it; otherwise the refusal.
//! 2. Node's inference gate, taken and held for this call (chat_edge.rs `hold`): its 503, or the
//!    gate held while the rest is decided.
//! 3. The edge (chat_edge.rs), the gate held: the shared `llm:` budget (429), counted only now, the
//!    body cap (413), JSON and the object (400), the request shape (400), Cowork (403/409) and the
//!    Diary add-on (404).
//! 4. The request, its body as read, to `/internal/chat/turn` (a Cowork start to
//!    `/internal/chat/cowork`) with the per-start secret, the signed-in user and the gate's lease,
//!    which Core's turn adopts instead of entering the gate again (legacy_proxy.rs `relay`): the
//!    client's headers and credentials ride along, Core authenticates them again and must find the
//!    same user.
//! 5. Core's answer byte for byte and frame by frame: JSON, the 503 with `Retry-After`, or the event
//!    stream with its `: keep-alive` comments. The client leaving drops the response, which closes
//!    Core's socket (the turn stops and keeps its interrupted reply, #679).
//!
//! When Core fails before its headers the answer is the proxy's 502 JSON. When an event stream breaks
//! after them, the client gets the frame index.cjs writes for a handler that throws mid-stream, and
//! the end: `data: {"type":"error","text":"The request could not be completed. Please retry."}`.
use crate::{chat_edge, identity, reply, serve::Conn, App};
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::Semaphore;

pub const PATH: &str = "/api/chat";
/// Core rust-chat-turn.cjs `TURN_PATH` and `GATE_PATH`.
pub const TURN_PATH: &str = "/internal/chat/turn";
pub const GATE_PATH: &str = "/internal/chat/gate";
/// Core rust-chat-turn.cjs `USER_HEADER`: the user the front authenticated.
pub const USER_HEADER: &str = "x-noevia-chat-user";
/// core index.cjs `STORED_HISTORY_BYTES`: routes/chat.cjs refuses a longer body with its 413.
pub const STORED_HISTORY_BYTES: u64 = 32 * 1024 * 1024;
/// Core reads one byte past its cap before it answers that 413.
pub const RELAY_LIMIT: u64 = STORED_HISTORY_BYTES + 1;
/// index.cjs handleRequestScoped's answer once the headers are out.
pub const FAILED_FRAME: &str =
    "data: {\"type\":\"error\",\"text\":\"The request could not be completed. Please retry.\"}\n\n";
/// The proxy's 502 (legacy_proxy.rs) for a Core that does not answer.
const UNAVAILABLE: &str = "The server is not answering. Please retry.";

fn unavailable() -> Response<Body> {
    reply::error(StatusCode::BAD_GATEWAY, UNAVAILABLE, true)
}

fn sensitive(value: &str) -> Option<HeaderValue> {
    let mut v = HeaderValue::from_str(value).ok()?;
    v.set_sensitive(true);
    Some(v)
}

/// One POST to exactly [`PATH`] (WHATWG pathname `path`).
pub async fn serve(app: &Arc<App>, conn: Conn, req: Request<Body>, path: String) -> Response<Body> {
    // An assembled native backend has no Node to relay to: its own executor answers, as before.
    if app.proxy.is_in_process() {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (mut parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gated = tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &path, now)).await;
    let authn = match gated {
        Ok(Ok(id)) => id,
        Ok(Err(rejection @ identity::Rejection::Refused(_))) => {
            return refused(app, conn, &parts.headers, rejection.into_response()).await;
        }
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let token = app.config.research.front_token().and_then(sensitive);
    let (Some(token), Some(user)) = (token, sensitive(&authn.user.id)) else {
        return unavailable();
    };
    // Node's inference gate first: a call is counted only once the gate let it in.
    let held = match chat_edge::hold(app, conn, &parts.headers, chat_edge::CHAT_HOLD_PATH).await {
        chat_edge::Gate::Held(h) => h,
        chat_edge::Gate::Answer(answer) => return answer,
    };
    // The edge (chat_edge.rs), in routes/chat.cjs's order, the gate held: the budget before the body.
    let Some(limited) = chat_edge::charge(app, &authn.user.id) else {
        return unavailable();
    };
    if limited {
        return reply::error(
            StatusCode::TOO_MANY_REQUESTS,
            chat_edge::CHAT_TOO_MANY,
            true,
        );
    }
    let bytes = match crate::account_routes::read_body(body, STORED_HISTORY_BYTES as usize).await {
        Ok(b) if !b.over => b.bytes,
        failed => match chat_edge::unread(failed.is_ok()) {
            chat_edge::Verdict::Refuse(status, text) => {
                return reply::json_text(status, text, true)
            }
            _ => return unavailable(),
        },
    };
    let account = chat_edge::Account {
        admin: authn.user.role == server_auth::Role::Admin,
        diary: authn.user.diary_enabled,
    };
    // The harness flag is read only for a Cowork request that got that far.
    let verdict = match chat_edge::check(&bytes, account, || None) {
        chat_edge::Verdict::Cowork => {
            let a = Arc::clone(app);
            let harness = tokio::task::spawn_blocking(move || chat_edge::harness_blocking(&a))
                .await
                .ok()
                .flatten();
            chat_edge::check(&bytes, account, || harness)
        }
        other => other,
    };
    let target = match verdict {
        chat_edge::Verdict::Turn => TURN_PATH,
        chat_edge::Verdict::Cowork => chat_edge::COWORK_PATH,
        chat_edge::Verdict::Refuse(status, text) => return reply::json_text(status, text, true),
    };
    parts.headers.remove(header::TRANSFER_ENCODING);
    parts
        .headers
        .insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    let req = Request::from_parts(parts, Body::from(bytes));
    let extra = [
        (crate::project_store::TOKEN_HEADER, token),
        (USER_HEADER, user),
        (chat_edge::LEASE_HEADER, held.lease.clone()),
    ];
    let res = app
        .proxy
        .relay(conn.peer, req, target, &extra, RELAY_LIMIT)
        .await;
    // Core's turn adopted the hold when it read the request; the probe can close now.
    drop(held);
    let Some(res) = res else {
        return unavailable();
    };
    if is_event_stream(&res) {
        res.map(|b| Body::new(StreamTail::new(b)))
    } else {
        res
    }
}

/// A refusal of the session gate's, after Core's inference gate: the gate's 503 when it is held,
/// else `answer`.
async fn refused(
    app: &Arc<App>,
    conn: Conn,
    headers: &HeaderMap,
    answer: Response<Body>,
) -> Response<Body> {
    gate_probe(app, conn, headers, GATE_PATH)
        .await
        .unwrap_or(answer)
}

/// Core's inference gate for a request the front refuses, at `target` (the chat gate, or the Diary
/// local exchange's): its 503 when the gate is held, else `None` (a 204, or no answer: the front's
/// refusal stands).
pub(crate) async fn gate_probe(
    app: &Arc<App>,
    conn: Conn,
    headers: &HeaderMap,
    target: &str,
) -> Option<Response<Body>> {
    let token = app.config.research.front_token().and_then(sensitive)?;
    let mut probe = Request::post(GATE_PATH).body(Body::empty()).ok()?;
    if let Some(host) = headers.get(header::HOST) {
        probe.headers_mut().insert(header::HOST, host.clone());
    }
    let extra = [(crate::project_store::TOKEN_HEADER, token)];
    let res = app.proxy.relay(conn.peer, probe, target, &extra, 0).await?;
    (res.status() == StatusCode::SERVICE_UNAVAILABLE).then_some(res)
}

fn is_event_stream(res: &Response<Body>) -> bool {
    res.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("text/event-stream"))
        })
}

/// The most of one unfinished frame a [`StreamTail`] holds back: 8 MiB.
pub const HOLDBACK: usize = 8 * 1024 * 1024;
/// The most every [`StreamTail`] in this process holds back together, held frames still queued on their
/// way out included: 32 MiB ([`BUDGET`]), charged by allocated capacity.
pub const HOLDBACK_BUDGET: usize = 32 * 1024 * 1024;

/// The process-wide holdback budget: one permit per byte held.
static BUDGET: std::sync::LazyLock<Arc<Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(Semaphore::new(HOLDBACK_BUDGET)));

/// Permits of the holdback budget, one per byte; given back when dropped.
struct Reservation {
    budget: Arc<Semaphore>,
    bytes: usize,
}

impl Reservation {
    /// Covers `bytes` in all, reserving the difference now; `false` (nothing taken) when the budget
    /// lacks it. Never waits.
    fn cover(&mut self, bytes: usize) -> bool {
        let Some(more) = bytes.checked_sub(self.bytes).filter(|m| *m > 0) else {
            return true;
        };
        let Ok(n) = u32::try_from(more) else {
            return false;
        };
        match self.budget.try_acquire_many(n) {
            Ok(permit) => permit.forget(),
            Err(_) => return false,
        }
        self.bytes = bytes;
        true
    }

    fn none(&self) -> Reservation {
        Reservation {
            budget: Arc::clone(&self.budget),
            bytes: 0,
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.bytes > 0 {
            self.budget.add_permits(self.bytes);
        }
    }
}

/// A held frame on its way out: its buffer and the reservation that covers the buffer's capacity, dropped
/// together when the response body is done with the bytes (written, or the client left).
struct Reserved {
    buf: Vec<u8>,
    _reservation: Reservation,
}

impl AsRef<[u8]> for Reserved {
    fn as_ref(&self) -> &[u8] {
        &self.buf
    }
}

/// The start of one unfinished frame, held back in one buffer whose allocated capacity is reserved on
/// the byte budget before it is allocated.
struct Held {
    buf: Vec<u8>,
    reservation: Reservation,
}

impl Held {
    fn new(budget: Arc<Semaphore>) -> Self {
        Held {
            buf: Vec::new(),
            reservation: Reservation { budget, bytes: 0 },
        }
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn ends_with_newline(&self) -> bool {
        self.buf.last() == Some(&b'\n')
    }

    /// Appends `tail` when the frame stays within [`HOLDBACK`] and the budget covers the buffer's
    /// capacity; never waits. The buffer grows by doubling (so tiny reads cost few allocations), or
    /// to exactly what is needed when the budget has no room for the doubling.
    fn try_hold(&mut self, tail: &[u8]) -> bool {
        let need = self.buf.len().saturating_add(tail.len());
        if need > HOLDBACK {
            return false;
        }
        if need > self.buf.capacity() {
            let doubled = need
                .max(self.buf.capacity().saturating_mul(2))
                .min(HOLDBACK);
            let Some(capacity) = [doubled, need]
                .into_iter()
                .find(|c| self.reservation.cover(*c))
            else {
                return false;
            };
            self.buf.reserve_exact(capacity - self.buf.len());
            // Vec may round up; what it reports is what is charged.
            if !self.reservation.cover(self.buf.capacity()) {
                return false;
            }
        }
        self.buf.extend_from_slice(tail);
        true
    }

    /// What is held, as bytes that keep their reservation until they are dropped.
    fn take(&mut self) -> Option<Bytes> {
        let empty = self.reservation.none();
        let reservation = std::mem::replace(&mut self.reservation, empty);
        let buf = std::mem::take(&mut self.buf);
        (!buf.is_empty()).then(|| {
            Bytes::from_owner(Reserved {
                buf,
                _reservation: reservation,
            })
        })
    }
}

/// Core's event stream, sent a whole frame at a time (a frame ends with a blank line, `\n\n`). When it
/// breaks before its end, the unfinished frame Core left is dropped and [`FAILED_FRAME`] ends the stream,
/// so the client reads that error instead of failing on half a frame (the web client parses every
/// complete frame's `data:` line as JSON) or reading a reset as "the model connection ended".
///
/// Core writes each frame whole, so a read normally ends on a frame end and goes out at once, unchanged,
/// holding nothing. Only the start of a frame split across reads is held, until its end comes, and only
/// while it fits both [`HOLDBACK`] and the process-wide [`HOLDBACK_BUDGET`] (reserved before it is
/// copied, never waited for). A frame that does not fit streams as it comes, as the relay did before
/// holding anything; if such a frame breaks mid-way its start is already out, and the stream ends
/// without the failure frame: the client never parses an unfinished frame and reports the connection
/// ended.
struct StreamTail {
    inner: Body,
    held: Held,
    /// What goes out next, in order.
    pending: std::collections::VecDeque<Bytes>,
    /// The current frame streams as it comes (it did not fit); its start is out.
    overflow: bool,
    /// The last byte sent was `\n` (an end can straddle two reads while overflowing).
    sent_nl: bool,
    done: bool,
}

impl StreamTail {
    fn new(inner: Body) -> Self {
        Self::with_budget(inner, Arc::clone(&BUDGET))
    }

    fn with_budget(inner: Body, budget: Arc<Semaphore>) -> Self {
        StreamTail {
            inner,
            held: Held::new(budget),
            pending: std::collections::VecDeque::new(),
            overflow: false,
            sent_nl: false,
            done: false,
        }
    }

    fn send(&mut self, bytes: Bytes) {
        if !bytes.is_empty() {
            self.pending.push_back(bytes);
        }
    }

    /// One read of Core's stream: what of it goes out now, in [`Self::pending`].
    fn take(&mut self, mut data: Bytes) {
        // The usual case: whole frames and nothing held. Sent as they came, at once.
        if !self.overflow && self.held.is_empty() && data.ends_with(b"\n\n") {
            self.sent_nl = true;
            self.send(data);
            return;
        }
        if self.overflow {
            let end = if self.sent_nl && data.first() == Some(&b'\n') {
                Some(1)
            } else {
                data.windows(2).position(|w| w == b"\n\n").map(|i| i + 2)
            };
            let Some(end) = end else {
                if let Some(&last) = data.last() {
                    self.sent_nl = last == b'\n';
                }
                self.send(data);
                return;
            };
            let head = data.split_to(end);
            self.send(head);
            self.sent_nl = true;
            self.overflow = false;
        }
        if data.is_empty() {
            return;
        }
        // The last frame end in what is held and this read.
        let end = data
            .windows(2)
            .rposition(|w| w == b"\n\n")
            .map(|i| i + 2)
            .or_else(|| {
                (self.held.ends_with_newline() && data.first() == Some(&b'\n')).then_some(1)
            });
        if let Some(end) = end {
            if let Some(held) = self.held.take() {
                self.send(held);
            }
            let whole = data.split_to(end);
            self.send(whole);
            self.sent_nl = true;
        }
        if data.is_empty() || self.held.try_hold(&data) {
            return;
        }
        // Over the frame's cap, or the shared budget is spent: this frame streams as it comes.
        if let Some(held) = self.held.take() {
            self.send(held);
        }
        if let Some(&last) = data.last() {
            self.sent_nl = last == b'\n';
        }
        self.send(data);
        self.overflow = true;
    }
}

impl HttpBody for StreamTail {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        loop {
            if let Some(next) = self.pending.pop_front() {
                return Poll::Ready(Some(Ok(Frame::data(next))));
            }
            if self.done {
                return Poll::Ready(None);
            }
            match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) => self.take(data),
                    Err(other) => return Poll::Ready(Some(Ok(other))),
                },
                Poll::Ready(Some(Err(_))) => {
                    self.done = true;
                    if !self.overflow {
                        drop(self.held.take());
                        self.send(Bytes::from_static(FAILED_FRAME.as_bytes()));
                    }
                }
                Poll::Ready(None) => {
                    self.done = true;
                    // Core ended without a final blank line: what it wrote goes out as it wrote it.
                    if let Some(held) = self.held.take() {
                        self.send(held);
                    }
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done && self.pending.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::time::Duration;

    type Chan = http_body_util::channel::Channel<Bytes, std::io::Error>;

    #[test]
    fn the_failure_frame_is_index_cjs_s() {
        // JSON.stringify's key order: type, then text.
        let inner = FAILED_FRAME
            .strip_prefix("data: ")
            .and_then(|f| f.strip_suffix("\n\n"))
            .unwrap();
        assert!(inner.starts_with("{\"type\":\"error\",\"text\":"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(inner).unwrap(),
            serde_json::json!({"type":"error","text":"The request could not be completed. Please retry."})
        );
        assert_eq!(RELAY_LIMIT, 32 * 1024 * 1024 + 1);
    }

    /// Each read Core's stream gives, and what goes out for it.
    /// What goes out for one read.
    fn read(tail: &mut StreamTail, r: &[u8]) -> Vec<u8> {
        tail.take(Bytes::copy_from_slice(r));
        tail.pending.drain(..).flat_map(|b| b.to_vec()).collect()
    }

    fn feed(reads: &[&[u8]]) -> (StreamTail, Vec<Vec<u8>>) {
        let mut tail = StreamTail::new(Body::empty());
        let out = reads.iter().map(|r| read(&mut tail, r)).collect();
        (tail, out)
    }

    fn in_use(budget: &Semaphore, total: usize) -> usize {
        total - budget.available_permits()
    }

    /// Review P2 (rereview b29b16a): the held bytes of every stream together stay within the shared
    /// budget; the streams that find it spent stream their split frame as it comes, and every stream's
    /// bytes come out exactly as Core wrote them.
    #[test]
    fn many_streams_holding_split_frames_never_exceed_the_shared_budget() {
        const TOTAL: usize = 1024 * 1024;
        const PART: usize = 100 * 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        let start = format!("data: \"{}", "x".repeat(PART - 7));
        let mut streams: Vec<(StreamTail, Vec<u8>)> = (0..64)
            .map(|_| {
                (
                    StreamTail::with_budget(Body::empty(), Arc::clone(&budget)),
                    Vec::new(),
                )
            })
            .collect();
        for (tail, out) in &mut streams {
            out.extend(read(tail, b"data: 1\n\n"));
            out.extend(read(tail, start.as_bytes()));
            assert!(in_use(&budget, TOTAL) <= TOTAL);
        }
        let holding = streams.iter().filter(|(t, _)| !t.held.is_empty()).count();
        assert_eq!(holding, TOTAL / PART, "as many hold as the budget allows");
        assert_eq!(in_use(&budget, TOTAL), holding * PART);
        // The others stream their frame: its start is already out, nothing of it is held.
        for (tail, out) in &streams {
            assert_eq!(tail.overflow, tail.held.is_empty());
            if tail.overflow {
                assert_eq!(out.len(), 9 + PART);
            }
        }
        for (tail, out) in &mut streams {
            out.extend(read(tail, b"\"\n\ndata: 2\n\n"));
            assert!(tail.held.is_empty() && !tail.overflow);
        }
        let whole = format!("data: 1\n\n{start}\"\n\ndata: 2\n\n");
        for (_, out) in &streams {
            assert_eq!(out, whole.as_bytes());
        }
        assert_eq!(
            in_use(&budget, TOTAL),
            0,
            "every reservation went back on completion"
        );
    }

    #[tokio::test]
    async fn concurrent_streams_with_split_frames_stay_correct_and_give_the_budget_back() {
        const TOTAL: usize = 64 * 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        let mut tasks = Vec::new();
        for n in 0..48 {
            let budget = Arc::clone(&budget);
            tasks.push(tokio::spawn(async move {
                let (mut tx, body) = Chan::new(2);
                let frame = format!("data: {{\"n\":{n},\"t\":\"{}\"}}\n\n", "y".repeat(8 * 1024));
                let (a, b) = frame.as_bytes().split_at(frame.len() / 2);
                let (a, b) = (Bytes::copy_from_slice(a), Bytes::copy_from_slice(b));
                tokio::spawn(async move {
                    tx.send_data(a).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    tx.send_data(b).await.unwrap();
                });
                let out = Body::new(StreamTail::with_budget(Body::new(body), budget))
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes();
                assert_eq!(out, Bytes::from(frame));
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(budget.available_permits(), TOTAL);
    }

    #[test]
    fn a_spent_budget_streams_the_split_frame_and_stays_correct() {
        let budget = Arc::new(Semaphore::new(0));
        let mut tail = StreamTail::with_budget(Body::empty(), Arc::clone(&budget));
        assert_eq!(
            read(&mut tail, b"data: {\"type\":\"meta\"}\n\n"),
            b"data: {\"type\":\"meta\"}\n\n"
        );
        // Nothing can be held: the split frame's start goes out at once, as the relay did before.
        assert_eq!(read(&mut tail, b"data: {\"ty"), b"data: {\"ty");
        assert!(tail.overflow);
        assert_eq!(read(&mut tail, b"pe\":\"done\"}\n"), b"pe\":\"done\"}\n");
        assert_eq!(read(&mut tail, b"\ndata: 3\n\n"), b"\ndata: 3\n\n");
        assert!(!tail.overflow && tail.held.is_empty());
        assert_eq!(budget.available_permits(), 0);
    }

    #[tokio::test]
    async fn a_spent_budget_then_a_break_ends_without_a_frame_the_client_would_parse() {
        let budget = Arc::new(Semaphore::new(0));
        let (mut tx, body) = Chan::new(4);
        tx.send_data(Bytes::from_static(b"data: 1\n\n"))
            .await
            .unwrap();
        tx.send_data(Bytes::from_static(b"data: {\"ty"))
            .await
            .unwrap();
        tx.abort(std::io::Error::other("Core went away"));
        let out = Body::new(StreamTail::with_budget(Body::new(body), budget))
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(out, Bytes::from_static(b"data: 1\n\ndata: {\"ty"));
    }

    #[tokio::test]
    async fn reservations_go_back_when_the_client_leaves_and_when_core_breaks() {
        const TOTAL: usize = 1024 * 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        // The client leaving drops the response body, and with it the stream holding a split frame.
        let (mut tx, body) = Chan::new(4);
        let mut stream = Body::new(StreamTail::with_budget(
            Body::new(body),
            Arc::clone(&budget),
        ));
        tx.send_data(Bytes::from_static(b"data: 1\n\n"))
            .await
            .unwrap();
        tx.send_data(Bytes::from(format!("data: \"{}", "z".repeat(4096))))
            .await
            .unwrap();
        let first = stream.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(first, Bytes::from_static(b"data: 1\n\n"));
        let pending = tokio::time::timeout(Duration::from_millis(100), stream.frame()).await;
        assert!(pending.is_err(), "the split frame is held");
        assert_eq!(budget.available_permits(), TOTAL - 4103);
        drop(stream);
        assert_eq!(budget.available_permits(), TOTAL);
        // Core breaking inside the held frame gives it back too.
        let (mut tx, body) = Chan::new(4);
        tx.send_data(Bytes::from_static(b"data: {\"ty"))
            .await
            .unwrap();
        tx.abort(std::io::Error::other("Core went away"));
        let out = Body::new(StreamTail::with_budget(
            Body::new(body),
            Arc::clone(&budget),
        ))
        .collect()
        .await
        .unwrap()
        .to_bytes();
        assert_eq!(out, Bytes::from_static(FAILED_FRAME.as_bytes()));
        assert_eq!(budget.available_permits(), TOTAL);
    }

    /// Re-review P2-1 (b74a57a): a held frame that is complete but still queued (the client reads
    /// slowly) keeps its reservation until the response body drops it, so what many stalled streams
    /// retain together stays within the budget.
    #[test]
    fn stalled_consumers_keep_queued_held_frames_within_the_budget() {
        const TOTAL: usize = 512 * 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        let mut streams: Vec<StreamTail> = (0..64)
            .map(|_| StreamTail::with_budget(Body::empty(), Arc::clone(&budget)))
            .collect();
        let piece = vec![b'q'; 2048];
        for tail in &mut streams {
            tail.take(Bytes::from_static(b"data: 1\n\n"));
            tail.take(Bytes::from_static(b"data: \""));
            for _ in 0..8 {
                tail.take(Bytes::copy_from_slice(&piece));
                assert!(in_use(&budget, TOTAL) <= TOTAL);
            }
        }
        let before = in_use(&budget, TOTAL);
        assert!(before > 0 && before <= TOTAL);
        // Every frame completes; nobody reads (the consumer is stalled): nothing is given back yet,
        // and the next frames cannot take more than the budget either.
        for tail in &mut streams {
            tail.take(Bytes::from_static(b"\"\n\ndata: \"more"));
            assert!(in_use(&budget, TOTAL) <= TOTAL);
        }
        assert!(
            in_use(&budget, TOTAL) >= before,
            "queued held frames keep their reservation"
        );
        assert!(streams.iter().all(|t| !t.pending.is_empty()));
        // One client reads its queue: exactly its frames' reservation goes back.
        let held_before = streams[0].held.reservation.bytes;
        let used = in_use(&budget, TOTAL);
        let out: Vec<u8> = streams[0]
            .pending
            .drain(..)
            .flat_map(|b| b.to_vec())
            .collect();
        let whole = format!("data: 1\n\ndata: \"{}\"\n\n", "q".repeat(8 * 2048));
        assert_eq!(out, whole.as_bytes());
        assert!(in_use(&budget, TOTAL) < used);
        assert_eq!(streams[0].held.reservation.bytes, held_before);
        // Every stream dropped with its queue and its held start: all of it comes back.
        drop(streams);
        assert_eq!(in_use(&budget, TOTAL), 0);
    }

    /// Re-review P2-2 (b74a57a): a held frame is one buffer, and its allocated capacity (not its length)
    /// is what is reserved; tiny reads cost few allocations.
    #[test]
    fn the_held_buffer_is_one_allocation_charged_by_its_capacity() {
        const TOTAL: usize = 1024 * 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        let mut tail = StreamTail::with_budget(Body::empty(), Arc::clone(&budget));
        tail.take(Bytes::from_static(b"data: \""));
        let mut capacities = std::collections::BTreeSet::new();
        for _ in 0..20_000 {
            tail.take(Bytes::from_static(b"r"));
            let held = &tail.held;
            assert!(held.reservation.bytes >= held.buf.capacity());
            assert_eq!(in_use(&budget, TOTAL), held.reservation.bytes);
            capacities.insert(held.buf.capacity());
        }
        assert!(tail.pending.is_empty(), "all of it is held");
        assert!(capacities.len() <= 16, "{} allocations", capacities.len());
        assert_eq!(tail.held.buf.len(), 20_007);
        // Near the budget the buffer grows by exactly what is needed, still charged by capacity.
        let tight = Arc::new(Semaphore::new(10_000));
        let mut tail = StreamTail::with_budget(Body::empty(), Arc::clone(&tight));
        tail.take(Bytes::copy_from_slice(&[b'a'; 6000]));
        tail.take(Bytes::copy_from_slice(&[b'b'; 3000]));
        assert_eq!(tail.held.buf.capacity(), 9000);
        assert_eq!(tight.available_permits(), 1000);
        tail.take(Bytes::copy_from_slice(&[b'c'; 2000]));
        assert!(tail.overflow && tail.held.is_empty());
        // Streamed out, the old buffer still holds its reservation while queued; then it is all back.
        assert_eq!(tight.available_permits(), 1000);
        tail.pending.clear();
        assert_eq!(tight.available_permits(), 10_000);
    }

    #[tokio::test]
    async fn nothing_leaks_after_drops_breaks_and_ends() {
        const TOTAL: usize = 256 * 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        // Held mid-frame, completed but queued, overflowed and queued, then all dropped.
        let mut a = StreamTail::with_budget(Body::empty(), Arc::clone(&budget));
        a.take(Bytes::from_static(b"data: {\"t"));
        let mut b = StreamTail::with_budget(Body::empty(), Arc::clone(&budget));
        b.take(Bytes::from_static(b"data: {\"t"));
        b.take(Bytes::from_static(b"\"}\n\ndata: {"));
        let mut c = StreamTail::with_budget(Body::empty(), Arc::clone(&budget));
        c.take(Bytes::from_static(b"data: \""));
        c.take(Bytes::from(vec![b'o'; HOLDBACK]));
        assert!(c.overflow);
        assert!(budget.available_permits() < TOTAL);
        drop((a, b, c));
        assert_eq!(budget.available_permits(), TOTAL);
        // Through a response body: Core breaks, Core ends without a blank line, the client leaves.
        for (reads, broken) in [
            (vec![&b"data: 1\n\ndata: {\"ty"[..]], true),
            (vec![&b"data: 1\n\ndata: x"[..]], false),
        ] {
            let (mut tx, body) = Chan::new(4);
            for r in reads {
                tx.send_data(Bytes::copy_from_slice(r)).await.unwrap();
            }
            if broken {
                tx.abort(std::io::Error::other("Core went away"));
            } else {
                drop(tx);
            }
            let _ = Body::new(StreamTail::with_budget(
                Body::new(body),
                Arc::clone(&budget),
            ))
            .collect()
            .await
            .unwrap();
            assert_eq!(budget.available_permits(), TOTAL);
        }
        let (mut tx, body) = Chan::new(4);
        let mut stream = Body::new(StreamTail::with_budget(
            Body::new(body),
            Arc::clone(&budget),
        ));
        tx.send_data(Bytes::from_static(b"data: {\"a"))
            .await
            .unwrap();
        tx.send_data(Bytes::from_static(b"\"}\n\ndata: {\"b"))
            .await
            .unwrap();
        let first = stream.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(first, Bytes::from_static(b"data: {\"a"));
        // The client keeps that held copy a while (as a socket write would), then leaves.
        assert!(budget.available_permits() < TOTAL);
        drop(stream);
        assert!(
            budget.available_permits() < TOTAL,
            "the copy still out keeps its reservation"
        );
        drop(first);
        assert_eq!(budget.available_permits(), TOTAL);
    }

    #[test]
    fn the_shared_budget_is_four_frames_of_the_cap() {
        assert_eq!(HOLDBACK_BUDGET, 4 * HOLDBACK);
        assert!(BUDGET.available_permits() <= HOLDBACK_BUDGET);
    }

    #[test]
    fn whole_frames_go_at_once_and_an_unfinished_one_waits_for_its_end() {
        let (_, out) = feed(&[
            b"data: {\"type\":\"meta\"}\n\n",
            b": keep-alive\n\n",
            b"data: {\"type\":\"delta\"}\n\ndata: {\"ty",
            b"pe\":\"done\"}\n",
            b"\n",
        ]);
        let want: [&[u8]; 5] = [
            b"data: {\"type\":\"meta\"}\n\n",
            b": keep-alive\n\n",
            b"data: {\"type\":\"delta\"}\n\n",
            b"",
            b"data: {\"type\":\"done\"}\n\n",
        ];
        assert_eq!(out, want.map(<[u8]>::to_vec));
    }

    #[test]
    fn a_frame_longer_than_the_holdback_still_streams() {
        let big = vec![b'x'; HOLDBACK];
        let (tail, out) = feed(&[b"data: \"", &big, b"more", b"\"\n", b"\ndata: 1\n\n"]);
        assert!(out[0].is_empty());
        // Past the bound the held start goes out, then the rest as it comes.
        assert_eq!(out[1].len(), 7 + HOLDBACK);
        assert_eq!(out[2], b"more");
        assert_eq!(out[3], b"\"\n");
        // Its end straddles two reads; the next frame is whole and goes at once.
        assert_eq!(out[4], b"\ndata: 1\n\n");
        assert!(!tail.overflow && tail.held.is_empty());
    }

    async fn through(reads: Vec<&'static [u8]>, broken: bool) -> Bytes {
        let (mut tx, body) = Chan::new(8);
        tokio::spawn(async move {
            for r in reads {
                tx.send_data(Bytes::from_static(r)).await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if broken {
                tx.abort(std::io::Error::other("Core went away"));
            }
        });
        Body::new(StreamTail::new(Body::new(body)))
            .collect()
            .await
            .unwrap()
            .to_bytes()
    }

    #[tokio::test]
    async fn a_break_inside_a_frame_drops_it_and_ends_with_the_failure_frame() {
        let out = through(
            vec![
                b"data: {\"type\":\"meta\"}\n",
                b"\n: keep-alive\n\ndata: {\"ty",
            ],
            true,
        )
        .await;
        assert_eq!(
            out,
            Bytes::from(format!(
                "data: {{\"type\":\"meta\"}}\n\n: keep-alive\n\n{FAILED_FRAME}"
            ))
        );
        // Between frames: the failure frame follows at once.
        let out = through(vec![b"data: {\"type\":\"meta\"}\n\n"], true).await;
        assert_eq!(
            out,
            Bytes::from(format!("data: {{\"type\":\"meta\"}}\n\n{FAILED_FRAME}"))
        );
        // A whole stream passes unchanged, and so does an end Core wrote without a blank line.
        let out = through(vec![b"data: {\"type\":\"done\"}\n\n"], false).await;
        assert_eq!(out, Bytes::from_static(b"data: {\"type\":\"done\"}\n\n"));
        let out = through(vec![b"data: {\"type\":\"done\"}\n\ndata: x"], false).await;
        assert_eq!(
            out,
            Bytes::from_static(b"data: {\"type\":\"done\"}\n\ndata: x")
        );
    }

    #[tokio::test]
    async fn an_oversized_frame_that_breaks_ends_without_a_frame_the_client_would_parse() {
        let big: &'static [u8] = Box::leak(vec![b'x'; HOLDBACK + 1].into_boxed_slice());
        let out = through(vec![b"data: 1\n\n", b"data: \"", big], true).await;
        assert_eq!(out.len(), 9 + 7 + HOLDBACK + 1);
        assert!(!out.ends_with(FAILED_FRAME.as_bytes()));
    }
}
