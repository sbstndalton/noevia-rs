//! The chat turn's event stream, written by the front (chat port S7, capability `rust-chat-wire`).
//!
//! The front asks Core's relayed turn for NDJSON ([`WIRE_HEADER`]: [`WIRE`]; core rust-chat-wire.cjs) and
//! writes the event stream the client reads itself, as chat.cjs wrote it (contract §1.3):
//! - the headers: `Content-Type: text/event-stream`, `Cache-Control: no-cache, no-transform`,
//!   `X-Accel-Buffering: no`, `Connection: keep-alive`, beside Core's own (`X-Noevia-API`, cookies, the
//!   security headers). They go out with the first line Core sends, as Node's went out with its first
//!   event, also for the routing question's early stream (`route_pending`);
//! - one frame per line. An object line is one event, `data: <the line>\n\n`: the line is Core's
//!   `JSON.stringify(event)`, so the frame is byte for byte the one chat.cjs writes. A string line is
//!   a whole frame Core passed on as it came (a Diary turn relays the companion's frames), written as
//!   the string says;
//! - `: keep-alive\n\n` every [`HEARTBEAT`] from the headers on (chat.cjs's `setInterval`), never inside
//!   a frame;
//! - the end when Core's body ends.
//!
//! A frame goes out only whole. The start of a line split across reads is held until its end comes,
//! within [`HOLDBACK`] for the line and the process-wide [`HOLDBACK_BUDGET`] for every stream together
//! (reserved before it is copied, never waited for), so memory stays bounded however many turns stream
//! at once. An event line that does not fit streams as it comes (`data: `, its bytes, then its end).
//!
//! When Core's stream breaks, or sends what is not a line of this wire, the unfinished line is dropped,
//! Core's socket is closed, and [`FAILED_FRAME`] (index.cjs's error for a handler that throws once the
//! headers are out) ends the stream: the client reads that error, never half a frame. Only an event
//! already streaming past the hold ends without it (its start is out; the client never parses an
//! unfinished frame and reports that the connection ended).
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Response},
};
use bytes::{Bytes, BytesMut};
use http_body::{Body as HttpBody, Frame};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::Semaphore;

/// Core rust-chat-wire.cjs `WIRE_HEADER` and `WIRE`: the relay asks for NDJSON with this.
pub const WIRE_HEADER: &str = "x-noevia-chat-wire";
pub const WIRE: &str = "ndjson/1";
/// Core rust-chat-wire.cjs `NDJSON_TYPE`.
pub const NDJSON_TYPE: &str = "application/x-ndjson";
/// chat.cjs's heartbeat period.
pub const HEARTBEAT: Duration = Duration::from_millis(5000);
pub const KEEP_ALIVE: &[u8] = b": keep-alive\n\n";
/// index.cjs handleRequestScoped's answer once the headers are out (core rust-chat-wire.cjs
/// `FAILED_EVENT`, framed).
pub const FAILED_FRAME: &str =
    "data: {\"type\":\"error\",\"text\":\"The request could not be completed. Please retry.\"}\n\n";
/// The most of one unfinished line a stream holds back: 8 MiB.
pub const HOLDBACK: usize = 8 * 1024 * 1024;
/// The most every stream in this process holds back together: 32 MiB.
pub const HOLDBACK_BUDGET: usize = 32 * 1024 * 1024;

/// The process-wide holdback budget: one permit per byte held.
static BUDGET: std::sync::LazyLock<Arc<Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(Semaphore::new(HOLDBACK_BUDGET)));

/// The headers chat.cjs wrote for its event stream.
const SSE_HEADERS: [(header::HeaderName, &str); 4] = [
    (header::CONTENT_TYPE, "text/event-stream"),
    (header::CACHE_CONTROL, "no-cache, no-transform"),
    (header::HeaderName::from_static("x-accel-buffering"), "no"),
    (header::CONNECTION, "keep-alive"),
];

/// Whether Core answered on this wire.
pub fn is_ndjson(headers: &HeaderMap) -> bool {
    media_type_is(headers, NDJSON_TYPE)
}

/// Whether an answer is an event stream (a Core that is not this front's pair writes its own).
pub fn is_event_stream(headers: &HeaderMap) -> bool {
    media_type_is(headers, "text/event-stream")
}

fn media_type_is(headers: &HeaderMap, want: &str) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case(want))
        })
}

/// Core's NDJSON answer as the client's event stream: its status and headers with the event stream's
/// own in place of Core's type and caching, sent when the first frame is ready.
pub async fn stream(res: Response<Body>) -> Response<Body> {
    let (mut parts, body) = res.into_parts();
    for name in [
        header::CONTENT_TYPE,
        header::CACHE_CONTROL,
        header::CONTENT_LENGTH,
    ] {
        parts.headers.remove(name);
    }
    for (name, value) in SSE_HEADERS {
        parts.headers.insert(name, HeaderValue::from_static(value));
    }
    let mut wire = EventWire::new(body);
    std::future::poll_fn(|cx| wire.fill(cx)).await;
    Response::from_parts(parts, Body::new(wire))
}

/// The bytes of one unfinished line held back, each reserved on a byte budget before it is copied in;
/// the reservation goes back when the bytes leave (sent or dropped) and when this is dropped.
struct Held {
    budget: Arc<Semaphore>,
    chunks: Vec<Bytes>,
    len: usize,
}

impl Held {
    fn new(budget: Arc<Semaphore>) -> Self {
        Held {
            budget,
            chunks: Vec::new(),
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn first_byte(&self) -> Option<u8> {
        self.chunks.first().and_then(|c| c.first()).copied()
    }

    /// Holds a copy of `tail` when it fits the line's [`HOLDBACK`] and the shared budget has room;
    /// never waits.
    fn try_hold(&mut self, tail: &[u8]) -> bool {
        if self.len.saturating_add(tail.len()) > HOLDBACK {
            return false;
        }
        let Ok(n) = u32::try_from(tail.len()) else {
            return false;
        };
        match self.budget.try_acquire_many(n) {
            Ok(permit) => permit.forget(),
            Err(_) => return false,
        }
        self.chunks.push(Bytes::copy_from_slice(tail));
        self.len += tail.len();
        true
    }

    /// Everything held, in order; its reservation goes back.
    fn take(&mut self) -> Vec<Bytes> {
        self.release();
        std::mem::take(&mut self.chunks)
    }

    fn release(&mut self) {
        if self.len > 0 {
            self.budget.add_permits(self.len);
            self.len = 0;
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.release();
    }
}

/// Core's NDJSON turned into the event stream the client reads (module docs).
pub struct EventWire {
    inner: Body,
    held: Held,
    /// What goes out next, in order.
    pending: VecDeque<Bytes>,
    /// An event line streams as it comes (it did not fit): its `data: ` and start are out.
    overflow: bool,
    period: Duration,
    /// The heartbeat, from the first frame on.
    beat: Option<tokio::time::Interval>,
    /// A beat came while an event was streaming: it goes out after that event's end.
    beat_due: bool,
    done: bool,
}

impl EventWire {
    pub fn new(inner: Body) -> Self {
        Self::with(inner, Arc::clone(&BUDGET), HEARTBEAT)
    }

    fn with(inner: Body, budget: Arc<Semaphore>, period: Duration) -> Self {
        EventWire {
            inner,
            held: Held::new(budget),
            pending: VecDeque::new(),
            overflow: false,
            period,
            beat: None,
            beat_due: false,
            done: false,
        }
    }

    fn send(&mut self, bytes: Bytes) {
        if !bytes.is_empty() {
            self.pending.push_back(bytes);
        }
    }

    /// The stream ends here with [`FAILED_FRAME`] (unless an event's start is already out); Core's
    /// socket closes.
    fn fail(&mut self) {
        drop(self.held.take());
        if !self.overflow {
            self.send(Bytes::from_static(FAILED_FRAME.as_bytes()));
        }
        self.end();
    }

    fn end(&mut self) {
        self.done = true;
        self.inner = Body::empty();
    }

    /// One whole line; false when it is not a line of this wire.
    fn line(&mut self, line: Bytes) -> bool {
        match line.first() {
            None => true,
            Some(b'{') if !line.contains(&b'\r') => {
                let mut out = BytesMut::with_capacity(line.len() + 8);
                out.extend_from_slice(b"data: ");
                out.extend_from_slice(&line);
                out.extend_from_slice(b"\n\n");
                self.send(out.freeze());
                true
            }
            Some(b'"') => match serde_json::from_slice::<String>(&line) {
                // A whole frame: it ends with its only blank line.
                Ok(raw) if raw.find("\n\n").is_some_and(|i| i + 2 == raw.len()) => {
                    self.send(Bytes::from(raw));
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// One read of Core's NDJSON: what of it goes out now, in [`Self::pending`].
    fn take(&mut self, mut data: Bytes) {
        if self.overflow {
            let Some(end) = data.iter().position(|&b| b == b'\n') else {
                if data.contains(&b'\r') {
                    return self.fail();
                }
                return self.send(data);
            };
            let head = data.split_to(end);
            data = data.slice(1..);
            if head.contains(&b'\r') {
                return self.fail();
            }
            self.send(head);
            self.send(Bytes::from_static(b"\n\n"));
            self.overflow = false;
            if std::mem::take(&mut self.beat_due) {
                self.send(Bytes::from_static(KEEP_ALIVE));
            }
        }
        while let Some(end) = data.iter().position(|&b| b == b'\n') {
            let part = data.split_to(end);
            data = data.slice(1..);
            let line = if self.held.is_empty() {
                part
            } else {
                let held = self.held.take();
                let mut whole = BytesMut::with_capacity(
                    held.iter().map(Bytes::len).sum::<usize>() + part.len(),
                );
                for chunk in held {
                    whole.extend_from_slice(&chunk);
                }
                whole.extend_from_slice(&part);
                whole.freeze()
            };
            if !self.line(line) {
                return self.fail();
            }
        }
        if data.is_empty() || self.held.try_hold(&data) {
            return;
        }
        // Over the line's cap, or the shared budget is spent: an event streams as it comes. A relayed
        // frame cannot be written before its whole string is read, so it fails instead.
        let first = self.held.first_byte().or_else(|| data.first().copied());
        if first != Some(b'{') || data.contains(&b'\r') {
            return self.fail();
        }
        self.send(Bytes::from_static(b"data: "));
        for chunk in self.held.take() {
            self.send(chunk);
        }
        self.send(data);
        self.overflow = true;
    }

    /// Ready when a frame is waiting in [`Self::pending`] or the stream has ended.
    fn fill(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            if !self.pending.is_empty() {
                if self.beat.is_none() {
                    // The headers go out with this first frame: chat.cjs's heartbeat starts with them.
                    let mut beat = tokio::time::interval_at(
                        tokio::time::Instant::now() + self.period,
                        self.period,
                    );
                    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    self.beat = Some(beat);
                }
                return Poll::Ready(());
            }
            if self.done {
                return Poll::Ready(());
            }
            if let Some(beat) = self.beat.as_mut() {
                if beat.poll_tick(cx).is_ready() {
                    if self.overflow {
                        self.beat_due = true;
                    } else {
                        self.send(Bytes::from_static(KEEP_ALIVE));
                        continue;
                    }
                }
            }
            match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    // Trailers are not part of the wire.
                    if let Ok(data) = frame.into_data() {
                        self.take(data);
                    }
                }
                Poll::Ready(Some(Err(_))) => self.fail(),
                Poll::Ready(None) => {
                    // An unfinished line at Core's end is not an event: it never reaches the client.
                    if self.overflow {
                        self.end();
                    } else if self.held.is_empty() {
                        self.done = true;
                    } else {
                        self.fail();
                    }
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl HttpBody for EventWire {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        match self.fill(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => Poll::Ready(self.pending.pop_front().map(|b| Ok(Frame::data(b)))),
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

    type Chan = http_body_util::channel::Channel<Bytes, std::io::Error>;

    fn wire(budget: &Arc<Semaphore>) -> EventWire {
        EventWire::with(Body::empty(), Arc::clone(budget), HEARTBEAT)
    }

    /// What goes out for one read.
    fn read(w: &mut EventWire, r: &[u8]) -> Vec<u8> {
        w.take(Bytes::copy_from_slice(r));
        w.pending.drain(..).flat_map(|b| b.to_vec()).collect()
    }

    fn in_use(budget: &Semaphore, total: usize) -> usize {
        total - budget.available_permits()
    }

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
        assert_eq!(HOLDBACK_BUDGET, 4 * HOLDBACK);
        assert!(BUDGET.available_permits() <= HOLDBACK_BUDGET);
    }

    #[test]
    fn each_line_is_one_frame_and_a_split_line_waits_for_its_end() {
        let budget = Arc::new(Semaphore::new(HOLDBACK_BUDGET));
        let mut w = wire(&budget);
        assert_eq!(
            read(
                &mut w,
                b"{\"type\":\"meta\"}\n{\"type\":\"delta\",\"text\":\"\\u2028\"}\n"
            ),
            b"data: {\"type\":\"meta\"}\n\ndata: {\"type\":\"delta\",\"text\":\"\\u2028\"}\n\n"
        );
        assert_eq!(read(&mut w, b"{\"type\":\"de"), b"");
        assert_eq!(in_use(&budget, HOLDBACK_BUDGET), 11);
        assert_eq!(read(&mut w, b"lta\"}"), b"");
        assert_eq!(
            read(&mut w, b"\n{\"type\":\"done\"}\n"),
            b"data: {\"type\":\"delta\"}\n\ndata: {\"type\":\"done\"}\n\n"
        );
        assert_eq!(in_use(&budget, HOLDBACK_BUDGET), 0);
        // A relayed frame (a JSON string line) is written as it came; empty lines are nothing.
        assert_eq!(
            read(&mut w, b"\n\"event: ping\\ndata: [DONE]\\n\\n\"\n"),
            b"event: ping\ndata: [DONE]\n\n"
        );
        assert!(!w.done);
    }

    #[test]
    fn what_is_not_a_line_of_this_wire_ends_the_stream_with_the_failure_frame() {
        let budget = Arc::new(Semaphore::new(HOLDBACK_BUDGET));
        for bad in [
            &b"data: {\"type\":\"meta\"}\n"[..],
            b"[1]\n",
            b"{\"type\":\"delta\",\"text\":\"a\"}\r\n",
            b"\"no blank line\"\n",
            b"\"two\\n\\nframes\\n\\n\"\n",
            b"\"unterminated\n",
        ] {
            let mut w = wire(&budget);
            let out = read(&mut w, &[&b"{\"type\":\"meta\"}\n"[..], bad].concat());
            assert_eq!(
                out,
                [
                    &b"data: {\"type\":\"meta\"}\n\n"[..],
                    FAILED_FRAME.as_bytes()
                ]
                .concat(),
                "{}",
                String::from_utf8_lossy(bad)
            );
            assert!(w.done);
        }
    }

    #[test]
    fn an_event_longer_than_the_holdback_streams_and_a_beat_waits_for_its_end() {
        let budget = Arc::new(Semaphore::new(HOLDBACK_BUDGET));
        let mut w = wire(&budget);
        let big = vec![b'x'; HOLDBACK];
        assert_eq!(read(&mut w, b"{\"t\":\""), b"");
        let out = read(&mut w, &big);
        assert_eq!(out.len(), 6 + 6 + HOLDBACK);
        assert!(out.starts_with(b"data: {\"t\":\"xx"));
        assert!(w.overflow && w.held.is_empty());
        w.beat_due = true;
        assert_eq!(read(&mut w, b"more"), b"more");
        assert_eq!(
            read(&mut w, b"\"}\n{\"type\":\"done\"}\n"),
            [
                &b"\"}\n\n"[..],
                KEEP_ALIVE,
                b"data: {\"type\":\"done\"}\n\n"
            ]
            .concat()
        );
        assert!(!w.overflow && !w.beat_due);
        // A relayed frame cannot stream: past the hold it fails.
        let mut w = wire(&budget);
        assert_eq!(read(&mut w, b"\""), b"");
        assert_eq!(read(&mut w, &big), FAILED_FRAME.as_bytes());
        assert!(w.done);
        assert_eq!(in_use(&budget, HOLDBACK_BUDGET), 0);
    }

    /// Memory stays bounded under concurrent streams: what every stream holds together stays within the
    /// shared budget; the streams that find it spent stream their event as it comes, and every stream's
    /// frames come out exactly as Core's events.
    #[test]
    fn many_streams_holding_split_lines_never_exceed_the_shared_budget() {
        const TOTAL: usize = 1024 * 1024;
        const PART: usize = 100 * 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        let start = format!("{{\"t\":\"{}", "x".repeat(PART - 6));
        let mut streams: Vec<(EventWire, Vec<u8>)> =
            (0..64).map(|_| (wire(&budget), Vec::new())).collect();
        for (w, out) in &mut streams {
            out.extend(read(w, b"{\"n\":1}\n"));
            out.extend(read(w, start.as_bytes()));
            assert!(in_use(&budget, TOTAL) <= TOTAL);
        }
        let holding = streams.iter().filter(|(w, _)| !w.held.is_empty()).count();
        assert_eq!(holding, TOTAL / PART, "as many hold as the budget allows");
        assert_eq!(in_use(&budget, TOTAL), holding * PART);
        for (w, out) in &streams {
            assert_eq!(w.overflow, w.held.is_empty());
            if w.overflow {
                assert_eq!(out.len(), 15 + 6 + PART);
            }
        }
        for (w, out) in &mut streams {
            out.extend(read(w, b"\"}\n{\"n\":2}\n"));
            assert!(w.held.is_empty() && !w.overflow);
        }
        let whole = format!("data: {{\"n\":1}}\n\ndata: {start}\"}}\n\ndata: {{\"n\":2}}\n\n");
        for (_, out) in &streams {
            assert_eq!(out, whole.as_bytes());
        }
        assert_eq!(in_use(&budget, TOTAL), 0, "every reservation went back");
    }

    async fn through(reads: Vec<Vec<u8>>, broken: bool, budget: Arc<Semaphore>) -> Bytes {
        let (mut tx, body) = Chan::new(8);
        tokio::spawn(async move {
            for r in reads {
                tx.send_data(Bytes::from(r)).await.unwrap();
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            if broken {
                tx.abort(std::io::Error::other("Core went away"));
            }
        });
        Body::new(EventWire::with(Body::new(body), budget, HEARTBEAT))
            .collect()
            .await
            .unwrap()
            .to_bytes()
    }

    #[tokio::test]
    async fn a_break_drops_the_unfinished_line_and_ends_with_the_failure_frame_alone() {
        let budget = Arc::new(Semaphore::new(HOLDBACK_BUDGET));
        let meta = b"{\"type\":\"meta\"}\n".to_vec();
        let framed = "data: {\"type\":\"meta\"}\n\n";
        let out = through(
            vec![meta.clone(), b"{\"type\":\"del".to_vec()],
            true,
            Arc::clone(&budget),
        )
        .await;
        assert_eq!(out, Bytes::from(format!("{framed}{FAILED_FRAME}")));
        let out = through(vec![meta.clone()], true, Arc::clone(&budget)).await;
        assert_eq!(out, Bytes::from(format!("{framed}{FAILED_FRAME}")));
        // Core's end after a whole line is the end; an unfinished line at its end never goes out.
        let out = through(vec![meta.clone()], false, Arc::clone(&budget)).await;
        assert_eq!(out, Bytes::from(framed));
        let out = through(
            vec![meta.clone(), b"{\"x\"".to_vec()],
            false,
            Arc::clone(&budget),
        )
        .await;
        assert_eq!(out, Bytes::from(format!("{framed}{FAILED_FRAME}")));
        // An event already streaming past the hold ends without a frame the client would parse.
        let mut big = b"{\"t\":\"".to_vec();
        big.resize(HOLDBACK + 7, b'x');
        let out = through(vec![meta, big], true, Arc::clone(&budget)).await;
        assert_eq!(out.len(), framed.len() + 6 + HOLDBACK + 7);
        assert!(!out.ends_with(FAILED_FRAME.as_bytes()));
        assert_eq!(budget.available_permits(), HOLDBACK_BUDGET);
    }

    #[tokio::test]
    async fn reservations_go_back_when_the_client_leaves() {
        const TOTAL: usize = 1024 * 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        let (mut tx, body) = Chan::new(4);
        let mut stream = Body::new(EventWire::with(
            Body::new(body),
            Arc::clone(&budget),
            HEARTBEAT,
        ));
        tx.send_data(Bytes::from_static(b"{\"n\":1}\n"))
            .await
            .unwrap();
        tx.send_data(Bytes::from(format!("{{\"t\":\"{}", "z".repeat(4096))))
            .await
            .unwrap();
        let first = stream.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(first, Bytes::from_static(b"data: {\"n\":1}\n\n"));
        let pending = tokio::time::timeout(Duration::from_millis(100), stream.frame()).await;
        assert!(pending.is_err(), "the split line is held");
        assert_eq!(budget.available_permits(), TOTAL - 4102);
        drop(stream);
        assert_eq!(budget.available_permits(), TOTAL);
    }

    #[tokio::test]
    async fn concurrent_streams_with_split_lines_stay_correct_and_give_the_budget_back() {
        const TOTAL: usize = 64 * 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        let mut tasks = Vec::new();
        for n in 0..48 {
            let budget = Arc::clone(&budget);
            tasks.push(tokio::spawn(async move {
                let line = format!("{{\"n\":{n},\"t\":\"{}\"}}\n", "y".repeat(8 * 1024));
                let (a, b) = line.as_bytes().split_at(line.len() / 2);
                let out = through(vec![a.to_vec(), b.to_vec()], false, budget).await;
                assert_eq!(out, Bytes::from(format!("data: {}\n", line)));
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(budget.available_permits(), TOTAL);
    }

    /// chat.cjs's heartbeat: every period from the first frame on (none before it), not moved by events.
    #[tokio::test]
    async fn the_keep_alive_runs_from_the_first_frame_every_period() {
        const P: Duration = Duration::from_millis(300);
        let budget = Arc::new(Semaphore::new(HOLDBACK_BUDGET));
        let (mut tx, body) = Chan::new(4);
        let mut w = EventWire::with(Body::new(body), budget, P);
        // Nothing (no beat either) before the first line.
        let wait = tokio::time::timeout(P * 3, std::future::poll_fn(|cx| w.fill(cx)));
        assert!(wait.await.is_err(), "no frame before the first line");
        tx.send_data(Bytes::from_static(b"{\"type\":\"meta\"}\n"))
            .await
            .unwrap();
        let mut stream = Body::new(w);
        let start = std::time::Instant::now();
        let first = stream.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(first, Bytes::from_static(b"data: {\"type\":\"meta\"}\n\n"));
        let within = |n: u32| {
            let t = start.elapsed();
            assert!(
                t >= P * n - Duration::from_millis(20) && t < P * n + P / 2,
                "beat {n} at {t:?}"
            );
        };
        for n in 1..=2u32 {
            let beat = stream.frame().await.unwrap().unwrap().into_data().unwrap();
            assert_eq!(beat, Bytes::from_static(KEEP_ALIVE));
            within(n);
        }
        // An event between beats does not move them (setInterval is not reset by writes).
        tokio::time::sleep(P / 3).await;
        tx.send_data(Bytes::from_static(b"{\"type\":\"delta\"}\n"))
            .await
            .unwrap();
        let delta = stream.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(delta, Bytes::from_static(b"data: {\"type\":\"delta\"}\n\n"));
        let beat = stream.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(beat, Bytes::from_static(KEEP_ALIVE));
        within(3);
        drop(tx);
        assert!(stream.frame().await.is_none());
    }
}
