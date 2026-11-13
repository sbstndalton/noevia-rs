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
//! A frame goes out only whole. A whole line in one read goes out without a copy. The start of a line
//! split across reads is held in one growable buffer until its end comes, within [`HOLDBACK`] for the
//! line and the process-wide [`HOLDBACK_BUDGET`] for every stream together. The budget is charged by
//! the buffer's allocated capacity, reserved before every growth (doubling when the budget has room,
//! else exactly what is needed), never waited for. The held bytes leave as one `Bytes` that owns the
//! buffer and its reservation, so the permits come back only when the response body drops them (they
//! were written, or the client left): a slow client never keeps copies the budget no longer counts. A
//! relayed frame's decoded copy is reserved the same way. So memory stays bounded however many turns
//! stream at once. An event line that does not fit streams as it comes (`data: `, its bytes, then its
//! end).
//!
//! Every event line is checked before any of it goes out: it must be one JSON object in UTF-8 that the
//! web client's `JSON.parse` reads, with no raw control character in any string, key or value. The check
//! reads the bytes where they are (the read's own, or the held buffer and the line's end) a byte at a
//! time with fixed state: it allocates nothing, so it needs no part of the budget. A relayed frame's
//! string is decoded into its reserved copy, which checks it. The turn is complete once a `done` or
//! `error` event has gone out: an object line, or the first `data: ` line of a relayed frame (the web
//! client's completion rule, api.ts), whose own `type` member (wherever it stands; the last one counts)
//! is `"done"` or `"error"`. What follows (`title` after `done`, contract §1.3) still goes out.
//!
//! When Core's stream breaks, sends what is not a line of this wire (a malformed event included), or ends
//! before the turn is complete (an empty body too), the unfinished line is dropped, Core's socket is
//! closed, and [`FAILED_FRAME`] (index.cjs's error for a handler that throws once the headers are out)
//! ends the stream: the client reads that error, never half a frame or a silent end. Only an event
//! already streaming past the hold ends without it, when Core breaks inside it or its next bytes are not
//! its JSON: its start is out, so its frame is never ended (what is not its JSON never goes out), the
//! client never parses it and reports that the connection ended. Such an event is checked as it
//! streams, with the same fixed state, so whether it completes the turn is known when it ends,
//! whatever its key order.
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Response},
};
use bytes::Bytes;
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
/// The most every stream in this process holds back together, held bytes still queued on their way out
/// included: 32 MiB, charged by allocated capacity.
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
    let (res, _) = stream_with_completion(res).await;
    res
}

/// [`stream`], with the flag that is set once the turn is complete (for the lease's body, #2494).
pub async fn stream_with_completion(
    res: Response<Body>,
) -> (Response<Body>, Arc<std::sync::atomic::AtomicBool>) {
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
    if wire.complete {
        wire.completed
            .store(true, std::sync::atomic::Ordering::Release);
    }
    let completed = wire.completed();
    (Response::from_parts(parts, Body::new(wire)), completed)
}

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

/// Bytes on their way out with the reservation that covers their buffer's capacity, dropped together
/// when the response body is done with them (written, or the client left).
struct Reserved {
    buf: Vec<u8>,
    _reservation: Reservation,
}

impl AsRef<[u8]> for Reserved {
    fn as_ref(&self) -> &[u8] {
        &self.buf
    }
}

fn reserved(buf: Vec<u8>, reservation: Reservation) -> Bytes {
    Bytes::from_owner(Reserved {
        buf,
        _reservation: reservation,
    })
}

/// The start of one unfinished line, held in one buffer whose allocated capacity is reserved on the byte
/// budget before it is allocated. An event line is held as its frame's start (`data: ` and the line).
struct Held {
    buf: Vec<u8>,
    reservation: Reservation,
    /// The line's first byte: `{` an event, `"` a relayed frame.
    kind: Option<u8>,
}

impl Held {
    fn new(budget: Arc<Semaphore>) -> Self {
        Held {
            buf: Vec::new(),
            reservation: Reservation { budget, bytes: 0 },
            kind: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Appends `tail` (the start of a line when nothing is held) while the line stays within
    /// [`HOLDBACK`] and the budget covers the buffer's capacity; never waits. The buffer grows by
    /// doubling (so tiny reads cost few allocations), or to exactly what is needed when the budget has
    /// no room for the doubling.
    fn try_hold(&mut self, tail: &[u8]) -> bool {
        let prefix: &[u8] = if self.buf.is_empty() && tail.first() == Some(&b'{') {
            b"data: "
        } else {
            b""
        };
        if !self.grow(prefix.len() + tail.len()) {
            return false;
        }
        if self.buf.is_empty() {
            self.kind = tail.first().copied();
        }
        self.buf.extend_from_slice(prefix);
        self.buf.extend_from_slice(tail);
        true
    }

    /// Ends the held event with `part` and the frame's blank line, in the same buffer, when the budget
    /// covers it (exactly, no doubling): the frame then leaves as one piece.
    fn try_finish(&mut self, part: &[u8]) -> bool {
        let need = self.buf.len() + part.len() + 2;
        if need > self.buf.capacity() {
            if !self.reservation.cover(need) {
                return false;
            }
            self.buf.reserve_exact(need - self.buf.len());
            if !self.reservation.cover(self.buf.capacity()) {
                return false;
            }
        }
        self.buf.extend_from_slice(part);
        self.buf.extend_from_slice(b"\n\n");
        true
    }

    /// Room for `add` more bytes within [`HOLDBACK`], its capacity reserved first.
    fn grow(&mut self, add: usize) -> bool {
        let need = self.buf.len() + add;
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
        true
    }

    /// What is held and its reservation; nothing is held after.
    fn take_parts(&mut self) -> (Vec<u8>, Reservation) {
        let empty = self.reservation.none();
        self.kind = None;
        (
            std::mem::take(&mut self.buf),
            std::mem::replace(&mut self.reservation, empty),
        )
    }

    /// What is held, as bytes that keep their reservation until they are dropped.
    fn take(&mut self) -> Option<Bytes> {
        let (buf, reservation) = self.take_parts();
        (!buf.is_empty()).then(|| reserved(buf, reservation))
    }
}

/// A relayed frame: the JSON string line `pieces` (`len` bytes in all), decoded into one buffer that
/// `reservation` covers before it is allocated; `None` when it is not a whole frame or the budget
/// cannot cover it. The decoded frame is never longer than its line (an escape is at least as long as
/// what it stands for), so the buffer is allocated once, at the line's length, and never grows: the
/// decoding has no other copy and no scratch.
fn relayed_frame(pieces: &[&[u8]], len: usize, reservation: Reservation) -> Option<Bytes> {
    let (buf, reservation) = decode_relayed(pieces, len, reservation)?;
    Some(reserved(buf, reservation))
}

/// [`relayed_frame`]'s decoded buffer and the reservation that covers exactly its capacity.
fn decode_relayed(
    pieces: &[&[u8]],
    len: usize,
    mut reservation: Reservation,
) -> Option<(Vec<u8>, Reservation)> {
    if !reservation.cover(len) {
        return None;
    }
    let mut buf = Vec::with_capacity(len);
    if !reservation.cover(buf.capacity()) {
        return None;
    }
    let mut decode = Decode::default();
    if !pieces.iter().all(|p| decode.feed(p, &mut buf)) || !decode.finish() {
        return None;
    }
    // A whole frame: it ends with its only blank line.
    if !buf
        .windows(2)
        .position(|w| w == b"\n\n")
        .is_some_and(|i| i + 2 == buf.len())
    {
        return None;
    }
    Some((buf, reservation))
}

/// The UTF-8 lead byte `b`: how many continuation bytes follow, and the range the first must fall in.
fn utf8_lead(b: u8) -> Option<(u8, u8, u8)> {
    Some(match b {
        0xC2..=0xDF => (1, 0x80, 0xBF),
        0xE0 => (2, 0xA0, 0xBF),
        0xE1..=0xEC | 0xEE..=0xEF => (2, 0x80, 0xBF),
        0xED => (2, 0x80, 0x9F),
        0xF0 => (3, 0x90, 0xBF),
        0xF1..=0xF3 => (3, 0x80, 0xBF),
        0xF4 => (3, 0x80, 0x8F),
        _ => return None,
    })
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum DecSt {
    #[default]
    Before,
    In,
    Esc,
    Hex(u8),
    After,
    Bad,
}

/// One JSON string (a relayed frame's line, Core's `JSON.stringify` of it) decoded a byte at a time
/// into the bytes Node would have written for it: UTF-8, a lone surrogate escape as U+FFFD (what
/// Node's UTF-8 write makes of one). Raw control characters and invalid UTF-8 are refused, as
/// `JSON.parse` and [`Json`] refuse them. Writes only within the buffer's capacity, so it never
/// allocates.
#[derive(Default)]
struct Decode {
    st: DecSt,
    hex: u32,
    /// A high surrogate escape waiting for its low half.
    high: Option<u32>,
    need: u8,
    lo: u8,
    hi: u8,
}

impl Decode {
    fn put(out: &mut Vec<u8>, bytes: &[u8]) -> bool {
        if out.capacity() - out.len() < bytes.len() {
            return false;
        }
        out.extend_from_slice(bytes);
        true
    }

    fn put_char(out: &mut Vec<u8>, c: char) -> bool {
        let mut b = [0u8; 4];
        Self::put(out, c.encode_utf8(&mut b).as_bytes())
    }

    /// A waiting high surrogate that no low half followed.
    fn flush(&mut self, out: &mut Vec<u8>) -> bool {
        self.high.take().is_none() || Self::put_char(out, char::REPLACEMENT_CHARACTER)
    }

    fn unit(&mut self, u: u32, out: &mut Vec<u8>) -> bool {
        if let Some(h) = self.high {
            if (0xDC00..=0xDFFF).contains(&u) {
                self.high = None;
                let c = 0x10000 + ((h - 0xD800) << 10) + (u - 0xDC00);
                return Self::put_char(
                    out,
                    char::from_u32(c).unwrap_or(char::REPLACEMENT_CHARACTER),
                );
            }
            if !self.flush(out) {
                return false;
            }
        }
        if (0xD800..=0xDBFF).contains(&u) {
            self.high = Some(u);
            return true;
        }
        Self::put_char(
            out,
            char::from_u32(u).unwrap_or(char::REPLACEMENT_CHARACTER),
        )
    }

    fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> bool {
        for &b in bytes {
            let ok = match self.st {
                DecSt::Bad => false,
                DecSt::Before | DecSt::After if matches!(b, b' ' | b'\t' | b'\r' | b'\n') => true,
                DecSt::Before if b == b'"' => {
                    self.st = DecSt::In;
                    true
                }
                DecSt::Before | DecSt::After => false,
                DecSt::In if self.need > 0 => {
                    if (self.lo..=self.hi).contains(&b) {
                        self.need -= 1;
                        self.lo = 0x80;
                        self.hi = 0xBF;
                        Self::put(out, &[b])
                    } else {
                        false
                    }
                }
                DecSt::In => match b {
                    b'"' => {
                        self.st = DecSt::After;
                        self.flush(out)
                    }
                    b'\\' => {
                        self.st = DecSt::Esc;
                        true
                    }
                    0x00..=0x1F => false,
                    0x20..=0x7F => self.flush(out) && Self::put(out, &[b]),
                    _ => match utf8_lead(b) {
                        Some((need, lo, hi)) => {
                            (self.need, self.lo, self.hi) = (need, lo, hi);
                            self.flush(out) && Self::put(out, &[b])
                        }
                        None => false,
                    },
                },
                DecSt::Esc => {
                    self.st = DecSt::In;
                    match b {
                        b'"' | b'\\' | b'/' => self.unit(u32::from(b), out),
                        b'b' => self.unit(0x08, out),
                        b'f' => self.unit(0x0C, out),
                        b'n' => self.unit(0x0A, out),
                        b'r' => self.unit(0x0D, out),
                        b't' => self.unit(0x09, out),
                        b'u' => {
                            self.hex = 0;
                            self.st = DecSt::Hex(0);
                            true
                        }
                        _ => false,
                    }
                }
                DecSt::Hex(n) => match char::from(b).to_digit(16) {
                    Some(d) => {
                        self.hex = self.hex * 16 + d;
                        if n == 3 {
                            self.st = DecSt::In;
                            self.unit(self.hex, out)
                        } else {
                            self.st = DecSt::Hex(n + 1);
                            true
                        }
                    }
                    None => false,
                },
            };
            if !ok {
                self.st = DecSt::Bad;
                return false;
            }
        }
        true
    }

    /// The whole line has been read: whether it was one string.
    fn finish(&self) -> bool {
        self.st == DecSt::After
    }
}

/// Nesting the checker keeps without allocating: one bit per level in [`Json::inline`].
const INLINE_DEPTH: usize = 4 * 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum St {
    /// Before the line's object.
    Start,
    /// A value is next.
    Value,
    /// After `[`: a value or `]`.
    ArrFirst,
    /// After `{`: a key or `}`.
    ObjFirst,
    /// After `,` in an object: a key.
    Key,
    /// After a key: `:`.
    Colon,
    /// After a value: `,` or the container's end.
    After,
    Str,
    Esc,
    /// `\u` and the hex digits read so far.
    Hex(u8),
    Minus,
    Zero,
    Int,
    Dot,
    Frac,
    Exp,
    ExpSign,
    ExpInt,
    /// `true`, `false` or `null`: the rest of the word, and how much of it has come.
    Lit(&'static [u8], u8),
    /// The line's object has ended: only whitespace may follow.
    End,
    Bad,
}

/// One event line's JSON, checked a byte at a time with bounded state (the held buffer and the read's
/// own bytes are the only copies of the line): the grammar `JSON.parse`
/// accepts, a single object, strings (keys and values alike) in UTF-8 without raw control characters,
/// escapes as JSON writes them (a lone surrogate escape too, as `JSON.parse` reads it). It also reads
/// the object's own `type` member wherever it stands (the last one counts, as with `JSON.parse`): the
/// line completes the turn when that is `"done"` or `"error"`. An event that streams through past the
/// hold is checked the same way as it goes (module docs), so it needs no assumption about key order.
///
/// Nesting has no fixed limit. Which kind each open container is takes one bit per level: the first
/// [`INLINE_DEPTH`] levels inline, deeper ones in [`Json::spill`], whose capacity is reserved on the
/// shared budget before it grows (doubling, else exactly what is needed). Each level takes at least
/// one byte of the line, so that is at most an eighth of the line. When the budget cannot cover it the
/// line is over the budget: refused like any line this wire cannot carry (the failure frame; for an
/// event already streaming, the end without its frame's end), never taken as complete.
struct Json {
    st: St,
    /// One bit per open container: set for an object.
    inline: [u64; INLINE_DEPTH / 64],
    spill: Vec<u64>,
    /// Covers [`Self::spill`]'s capacity.
    reservation: Reservation,
    /// The spill could not grow within the budget: the line is over the budget, not malformed.
    over_budget: bool,
    depth: usize,
    /// The string being read is a key.
    key: bool,
    /// How much of `type` a top-level key has matched so far (`None`: not `type`).
    key_match: Option<u8>,
    /// The key just read is the top-level `type`.
    type_key: bool,
    /// The value about to start is the top-level `type`'s.
    type_next: bool,
    /// The string being read is the top-level `type`'s value; how much of `done` / `error` it matches.
    type_value: bool,
    done_match: Option<u8>,
    error_match: Option<u8>,
    /// The last top-level `type` is `done` or `error`.
    completes: bool,
    /// A `\u` escape's value so far.
    hex: u32,
    /// UTF-8: continuation bytes still due, and the range the next one must fall in.
    need: u8,
    lo: u8,
    hi: u8,
}

/// The next step of matching `word` against a string, one decoded code unit at a time.
fn matched(m: Option<u8>, word: &[u8], unit: u32) -> Option<u8> {
    let i = m?;
    (word.get(usize::from(i)).map(|&c| u32::from(c)) == Some(unit)).then_some(i + 1)
}

impl Json {
    fn new(reservation: Reservation) -> Self {
        Json {
            st: St::Start,
            inline: [0; INLINE_DEPTH / 64],
            spill: Vec::new(),
            reservation,
            over_budget: false,
            depth: 0,
            key: false,
            key_match: None,
            type_key: false,
            type_next: false,
            type_value: false,
            done_match: None,
            error_match: None,
            completes: false,
            hex: 0,
            need: 0,
            lo: 0,
            hi: 0,
        }
    }

    /// Reads `bytes`, the next of the line; false once it is not JSON this wire carries.
    fn feed(&mut self, bytes: &[u8]) -> bool {
        for &b in bytes {
            self.byte(b);
            if self.st == St::Bad {
                return false;
            }
        }
        true
    }

    /// The whole line has been read: `None` unless it was one object, else whether it completes the
    /// turn.
    fn finish(&self) -> Option<bool> {
        (self.st == St::End).then_some(self.completes)
    }

    fn word(&self, i: usize) -> Option<&u64> {
        match i.checked_sub(self.inline.len()) {
            None => self.inline.get(i),
            Some(j) => self.spill.get(j),
        }
    }

    fn in_object(&self) -> bool {
        let Some(i) = self.depth.checked_sub(1) else {
            return false;
        };
        self.word(i / 64)
            .is_some_and(|w| w & (1u64 << (i % 64)) != 0)
    }

    /// The word for level `i`, the spill grown to hold it within the budget first.
    fn word_mut(&mut self, i: usize) -> Option<&mut u64> {
        let Some(j) = i.checked_sub(self.inline.len()) else {
            return self.inline.get_mut(i);
        };
        if j >= self.spill.len() {
            let need = j + 1;
            if need > self.spill.capacity() {
                let doubled = need.max(self.spill.capacity().saturating_mul(2));
                let bytes = |words: usize| words.saturating_mul(std::mem::size_of::<u64>());
                let words = [doubled, need]
                    .into_iter()
                    .find(|w| self.reservation.cover(bytes(*w)))?;
                self.spill.reserve_exact(words - self.spill.len());
                // Vec may round up; what it reports is what is charged.
                if !self.reservation.cover(bytes(self.spill.capacity())) {
                    return None;
                }
            }
            self.spill.resize(need, 0);
        }
        self.spill.get_mut(j)
    }

    fn push(&mut self, object: bool) -> St {
        let i = self.depth;
        let Some(w) = self.word_mut(i / 64) else {
            self.over_budget = true;
            return St::Bad;
        };
        if object {
            *w |= 1u64 << (i % 64);
        } else {
            *w &= !(1u64 << (i % 64));
        }
        self.depth += 1;
        if object {
            St::ObjFirst
        } else {
            St::ArrFirst
        }
    }

    /// A container closed: what follows it.
    fn pop(&mut self) -> St {
        self.depth = self.depth.saturating_sub(1);
        self.after_value()
    }

    fn after_value(&self) -> St {
        if self.depth == 0 {
            St::End
        } else {
            St::After
        }
    }

    fn start_key(&mut self) -> St {
        self.key = true;
        self.key_match = (self.depth == 1).then_some(0);
        St::Str
    }

    /// One decoded code unit of the string being read.
    fn unit(&mut self, u: u32) {
        if self.key {
            self.key_match = matched(self.key_match, b"type", u);
        } else if self.type_value {
            self.done_match = matched(self.done_match, b"done", u);
            self.error_match = matched(self.error_match, b"error", u);
        }
    }

    fn end_string(&mut self) -> St {
        if self.need > 0 {
            return St::Bad;
        }
        if self.key {
            self.key = false;
            self.type_key = self.key_match == Some(4);
            return St::Colon;
        }
        if self.type_value {
            self.type_value = false;
            self.completes = self.done_match == Some(4) || self.error_match == Some(5);
        }
        self.after_value()
    }

    fn begin_value(&mut self, b: u8) -> St {
        if std::mem::take(&mut self.type_next) {
            self.type_value = b == b'"';
            self.done_match = Some(0);
            self.error_match = Some(0);
            // A `type` that is not a string completes nothing.
            self.completes = false;
        }
        match b {
            b'{' => self.push(true),
            b'[' => self.push(false),
            b'"' => {
                self.key = false;
                St::Str
            }
            b'-' => St::Minus,
            b'0' => St::Zero,
            b'1'..=b'9' => St::Int,
            b't' => St::Lit(b"rue", 0),
            b'f' => St::Lit(b"alse", 0),
            b'n' => St::Lit(b"ull", 0),
            _ => St::Bad,
        }
    }

    fn byte(&mut self, b: u8) {
        let ws = matches!(b, b' ' | b'\t' | b'\n' | b'\r');
        self.st = match self.st {
            St::Bad => St::Bad,
            St::Start | St::End if ws => self.st,
            St::Start if b == b'{' => self.push(true),
            St::Start | St::End => St::Bad,
            St::Value | St::ArrFirst | St::ObjFirst | St::Key | St::Colon | St::After if ws => {
                self.st
            }
            St::Value => self.begin_value(b),
            St::ArrFirst if b == b']' => self.pop(),
            St::ArrFirst => self.begin_value(b),
            St::ObjFirst if b == b'}' => self.pop(),
            St::ObjFirst | St::Key if b == b'"' => self.start_key(),
            St::ObjFirst | St::Key => St::Bad,
            St::Colon if b == b':' => {
                self.type_next = std::mem::take(&mut self.type_key);
                St::Value
            }
            St::Colon => St::Bad,
            St::After => match b {
                b',' if self.in_object() => St::Key,
                b',' => St::Value,
                b'}' if self.in_object() => self.pop(),
                b']' if !self.in_object() => self.pop(),
                _ => St::Bad,
            },
            St::Str if self.need > 0 => {
                if (self.lo..=self.hi).contains(&b) {
                    self.need -= 1;
                    self.lo = 0x80;
                    self.hi = 0xBF;
                    St::Str
                } else {
                    St::Bad
                }
            }
            St::Str => match b {
                b'"' => self.end_string(),
                b'\\' => St::Esc,
                0x00..=0x1F => St::Bad,
                0x20..=0x7F => {
                    self.unit(u32::from(b));
                    St::Str
                }
                _ => {
                    let Some((need, lo, hi)) = utf8_lead(b) else {
                        return self.st = St::Bad;
                    };
                    (self.need, self.lo, self.hi) = (need, lo, hi);
                    // Not ASCII: matches neither `type` nor `done` nor `error`.
                    self.unit(0x80);
                    St::Str
                }
            },
            St::Esc => {
                let unit = match b {
                    b'"' | b'\\' | b'/' => u32::from(b),
                    b'b' => 0x08,
                    b'f' => 0x0C,
                    b'n' => 0x0A,
                    b'r' => 0x0D,
                    b't' => 0x09,
                    b'u' => {
                        self.hex = 0;
                        return self.st = St::Hex(0);
                    }
                    _ => return self.st = St::Bad,
                };
                self.unit(unit);
                St::Str
            }
            St::Hex(n) => {
                let Some(d) = char::from(b).to_digit(16) else {
                    return self.st = St::Bad;
                };
                self.hex = self.hex * 16 + d;
                if n == 3 {
                    self.unit(self.hex);
                    St::Str
                } else {
                    St::Hex(n + 1)
                }
            }
            St::Minus => match b {
                b'0' => St::Zero,
                b'1'..=b'9' => St::Int,
                _ => St::Bad,
            },
            St::Dot => match b {
                b'0'..=b'9' => St::Frac,
                _ => St::Bad,
            },
            St::Exp => match b {
                b'+' | b'-' => St::ExpSign,
                b'0'..=b'9' => St::ExpInt,
                _ => St::Bad,
            },
            St::ExpSign => match b {
                b'0'..=b'9' => St::ExpInt,
                _ => St::Bad,
            },
            St::Zero | St::Int | St::Frac | St::ExpInt => match (self.st, b) {
                (St::Int, b'0'..=b'9') => St::Int,
                (St::Frac, b'0'..=b'9') => St::Frac,
                (St::ExpInt, b'0'..=b'9') => St::ExpInt,
                (St::Zero | St::Int, b'.') => St::Dot,
                (St::Zero | St::Int | St::Frac, b'e' | b'E') => St::Exp,
                _ => {
                    // The number has ended: this byte is what follows it.
                    self.st = St::After;
                    return self.byte(b);
                }
            },
            St::Lit(word, i) => match word.get(usize::from(i)) {
                Some(&c) if c == b => {
                    if usize::from(i) + 1 == word.len() {
                        self.after_value()
                    } else {
                        St::Lit(word, i + 1)
                    }
                }
                _ => St::Bad,
            },
        };
    }
}

/// What checking one whole line found.
#[derive(Debug, PartialEq, Eq)]
enum Checked {
    /// One JSON object; whether it completes the turn.
    Event(bool),
    /// Not one JSON object in UTF-8.
    Malformed,
    /// Its nesting needs more of the budget than is left (`reservation` draws on it): it cannot be
    /// told whether it is an event, so it is never taken as one that is not.
    OverBudget,
}

fn check_line(pieces: &[&[u8]], reservation: Reservation) -> Checked {
    let mut json = Json::new(reservation);
    let read = pieces.iter().all(|p| json.feed(p));
    match (read, json.finish()) {
        _ if json.over_budget => Checked::OverBudget,
        (true, Some(completes)) => Checked::Event(completes),
        _ => Checked::Malformed,
    }
}

/// One whole event line (its pieces in order, nothing copied): `None` when it is not one JSON object
/// in UTF-8 or its nesting is over the budget, else whether the event completes the turn.
fn event_line(pieces: &[&[u8]], reservation: Reservation) -> Option<bool> {
    match check_line(pieces, reservation) {
        Checked::Event(completes) => Some(completes),
        Checked::Malformed | Checked::OverBudget => None,
    }
}

/// Whether a relayed frame completes the turn: its first `data: ` line is a `done` or `error` event
/// (api.ts reads that line alone; a data line that is not JSON completes nothing). `None` when that
/// line's nesting is over the budget: then whether it completes the turn is unknown, and the frame
/// must not go out as one that does not (the caller fails the stream before it is sent).
fn frame_completes(frame: &[u8], reservation: Reservation) -> Option<bool> {
    let Some(data) = frame
        .split(|&b| b == b'\n')
        .find_map(|l| l.strip_prefix(b"data: "))
    else {
        return Some(false);
    };
    match check_line(&[data], reservation) {
        Checked::Event(completes) => Some(completes),
        Checked::Malformed => Some(false),
        Checked::OverBudget => None,
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
    /// A `done` or `error` event has gone out: the turn is complete (module docs).
    complete: bool,
    /// `complete`, shared with whoever holds the stream (the admission lease's body, #2494).
    completed: Arc<std::sync::atomic::AtomicBool>,
    /// The event streaming through, checked as it goes.
    json: Json,
    done: bool,
}

impl EventWire {
    pub fn new(inner: Body) -> Self {
        Self::with(inner, Arc::clone(&BUDGET), HEARTBEAT)
    }

    fn with(inner: Body, budget: Arc<Semaphore>, period: Duration) -> Self {
        EventWire {
            inner,
            held: Held::new(Arc::clone(&budget)),
            pending: VecDeque::new(),
            overflow: false,
            period,
            beat: None,
            beat_due: false,
            complete: false,
            completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            json: Json::new(Reservation {
                budget: Arc::clone(&budget),
                bytes: 0,
            }),
            done: false,
        }
    }

    fn send(&mut self, bytes: Bytes) {
        if !bytes.is_empty() {
            self.pending.push_back(bytes);
        }
    }

    /// Set once the turn is complete; read by the lease's body when the lease is lost (#2494).
    pub fn completed(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.completed)
    }

    /// The stream ends here with [`FAILED_FRAME`] (unless an event's start is already out, or the
    /// turn is already complete: no failure event follows `done`, #2494); Core's socket closes.
    fn fail(&mut self) {
        drop(self.held.take());
        if !self.overflow && !self.complete {
            self.send(Bytes::from_static(FAILED_FRAME.as_bytes()));
        }
        self.end();
    }

    fn end(&mut self) {
        self.done = true;
        self.inner = Body::empty();
    }

    /// One whole line that came in one read; false when it is not a line of this wire. An event goes
    /// out without a copy.
    fn line(&mut self, line: Bytes) -> bool {
        if line.contains(&b'\r') {
            return false;
        }
        match line.first() {
            None => true,
            Some(b'{') => {
                let Some(completes) = event_line(&[&line], self.held.reservation.none()) else {
                    return false;
                };
                self.complete |= completes;
                // One reserved copy of the frame; when the budget cannot cover it, the line itself
                // between its frame's two static ends (no copy at all).
                let len = line.len() + 8;
                let mut reservation = self.held.reservation.none();
                if reservation.cover(len) {
                    let mut buf = Vec::with_capacity(len);
                    buf.extend_from_slice(b"data: ");
                    buf.extend_from_slice(&line);
                    buf.extend_from_slice(b"\n\n");
                    if reservation.cover(buf.capacity()) {
                        self.send(reserved(buf, reservation));
                        return true;
                    }
                }
                self.send(Bytes::from_static(b"data: "));
                self.send(line);
                self.send(Bytes::from_static(b"\n\n"));
                true
            }
            Some(b'"') => {
                let reservation = self.held.reservation.none();
                match relayed_frame(&[&line], line.len(), reservation) {
                    Some(frame) => match frame_completes(&frame, self.held.reservation.none()) {
                        Some(completes) => {
                            self.complete |= completes;
                            self.send(frame);
                            true
                        }
                        None => false,
                    },
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// The end of the held line; false when it is not a line of this wire.
    fn held_line(&mut self, part: Bytes) -> bool {
        if part.contains(&b'\r') {
            return false;
        }
        match self.held.kind {
            Some(b'{') => {
                if self.held.try_finish(&part) {
                    // One buffer: `data: `, the line, the blank line.
                    let Some(frame) = self.held.take() else {
                        return false;
                    };
                    let Some(completes) = frame
                        .get(6..frame.len().saturating_sub(2))
                        .and_then(|l| event_line(&[l], self.held.reservation.none()))
                    else {
                        return false;
                    };
                    self.complete |= completes;
                    self.send(frame);
                    return true;
                }
                // The held start and the line's end, checked where they are.
                let Some(completes) =
                    self.held.buf.get(6..).and_then(|start| {
                        event_line(&[start, &part], self.held.reservation.none())
                    })
                else {
                    return false;
                };
                self.complete |= completes;
                if let Some(start) = self.held.take() {
                    self.send(start);
                }
                self.send(part);
                self.send(Bytes::from_static(b"\n\n"));
                true
            }
            Some(b'"') => {
                let (buf, reservation) = self.held.take_parts();
                let len = buf.len() + part.len();
                // The held buffer stays reserved until the decoded copy has its own reservation.
                let copy = reservation.none();
                let decoded = relayed_frame(&[&buf, &part], len, copy);
                drop((buf, reservation));
                match decoded {
                    Some(frame) => match frame_completes(&frame, self.held.reservation.none()) {
                        Some(completes) => {
                            self.complete |= completes;
                            self.send(frame);
                            true
                        }
                        None => false,
                    },
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// One read of Core's NDJSON: what of it goes out now, in [`Self::pending`].
    fn take(&mut self, mut data: Bytes) {
        if self.overflow {
            let Some(end) = data.iter().position(|&b| b == b'\n') else {
                // Checked before it goes out: what is not this event's JSON never does, and the
                // event's frame is never ended (the client never parses it).
                if data.contains(&b'\r') || !self.json.feed(&data) {
                    return self.fail();
                }
                return self.send(data);
            };
            let head = data.split_to(end);
            data = data.slice(1..);
            if head.contains(&b'\r') || !self.json.feed(&head) {
                return self.fail();
            }
            let Some(completes) = self.json.finish() else {
                return self.fail();
            };
            self.send(head);
            self.send(Bytes::from_static(b"\n\n"));
            self.overflow = false;
            self.complete |= completes;
            if std::mem::take(&mut self.beat_due) {
                self.send(Bytes::from_static(KEEP_ALIVE));
            }
        }
        while let Some(end) = data.iter().position(|&b| b == b'\n') {
            let part = data.split_to(end);
            data = data.slice(1..);
            let ok = if self.held.is_empty() {
                self.line(part)
            } else {
                self.held_line(part)
            };
            if !ok {
                return self.fail();
            }
        }
        if data.is_empty() {
            return;
        }
        // What is not the start of a line of this wire fails at once.
        let kind = self.held.kind.or_else(|| data.first().copied());
        if !matches!(kind, Some(b'{' | b'"')) || data.contains(&b'\r') {
            return self.fail();
        }
        if self.held.try_hold(&data) {
            return;
        }
        // Over the line's cap, or the shared budget is spent: an event streams as it comes (its held
        // start keeps its reservation until it is written). A relayed frame cannot be written before its
        // whole string is read, so it fails instead.
        if kind != Some(b'{') {
            return self.fail();
        }
        // Checked from its start before any of it goes out, then as it streams.
        self.json = Json::new(self.held.reservation.none());
        let start = self.held.buf.get(6..).unwrap_or_default();
        if !self.json.feed(start) || !self.json.feed(&data) {
            return self.fail();
        }
        match self.held.take() {
            Some(start) => self.send(start),
            None => self.send(Bytes::from_static(b"data: ")),
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
                    // An unfinished line at Core's end is not an event: it never reaches the client;
                    // nor does an end before the turn is complete.
                    if self.overflow {
                        self.end();
                    } else if self.held.is_empty() && self.complete {
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
        let polled = self.fill(cx);
        if self.complete {
            self.completed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        match polled {
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
        // Held as its frame's start, charged by the buffer's capacity.
        assert_eq!(w.held.buf, b"data: {\"type\":\"de");
        assert!(w.held.reservation.bytes >= w.held.buf.capacity());
        assert_eq!(in_use(&budget, HOLDBACK_BUDGET), w.held.reservation.bytes);
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
        // Each holds its frame's start (`data: ` and the line), charged by capacity.
        assert_eq!(
            holding,
            TOTAL / (PART + 6),
            "as many hold as the budget allows"
        );
        let charged: usize = streams.iter().map(|(w, _)| w.held.reservation.bytes).sum();
        assert_eq!(in_use(&budget, TOTAL), charged);
        assert!(charged >= holding * (PART + 6));
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
        // A plain copy: the reserved frames themselves are dropped here.
        let out = Body::new(EventWire::with(Body::new(body), budget, HEARTBEAT))
            .collect()
            .await
            .unwrap()
            .to_bytes();
        Bytes::from(out.to_vec())
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
        // Core's end after the turn is complete is the end; an unfinished line at its end never goes
        // out, nor does an end before the turn is complete (both below and in the premature-end test).
        let done = b"{\"type\":\"done\"}\n".to_vec();
        let out = through(vec![meta.clone(), done], false, Arc::clone(&budget)).await;
        assert_eq!(
            out,
            Bytes::from(format!("{framed}data: {{\"type\":\"done\"}}\n\n"))
        );
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

    /// #2494: after the turn is complete (`done` or `error` out), Core's socket breaking, or its end
    /// with an unfinished line (as while it waits for the title), ends the stream cleanly: no failure
    /// event follows `done`, so the client keeps its finished reply (and Diary its saved entry).
    #[tokio::test]
    async fn after_the_turn_is_complete_a_break_ends_cleanly_without_the_failure_frame() {
        let budget = Arc::new(Semaphore::new(HOLDBACK_BUDGET));
        let done = b"{\"type\":\"done\"}\n".to_vec();
        let framed = "data: {\"type\":\"done\"}\n\n";
        for (reads, broken) in [
            (vec![done.clone()], true),
            (vec![done.clone(), b"{\"type\":\"ti".to_vec()], true),
            (vec![done.clone(), b"{\"type\":\"ti".to_vec()], false),
            (
                vec![b"{\"type\":\"error\",\"text\":\"x\"}\n".to_vec()],
                true,
            ),
        ] {
            let first = reads[0].clone();
            let out = through(reads, broken, Arc::clone(&budget)).await;
            assert!(!out.ends_with(FAILED_FRAME.as_bytes()), "{out:?}");
            if first == done {
                assert_eq!(out, Bytes::from(framed));
            }
        }
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
        // The held start (`data: ` and 4,102 bytes) and the frame the client still has.
        assert_eq!(budget.available_permits(), TOTAL - 4108 - first.len());
        drop(stream);
        assert_eq!(budget.available_permits(), TOTAL - first.len());
        drop(first);
        assert_eq!(budget.available_permits(), TOTAL);
    }

    /// Re-review P2-1 (S5 b74a57a), on this wire: a held line that is complete but still queued (the
    /// client reads slowly) keeps its reservation until the response body drops it, so what many stalled
    /// streams retain together stays within the budget.
    #[test]
    fn stalled_consumers_keep_queued_held_lines_within_the_budget() {
        const TOTAL: usize = 512 * 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        let mut streams: Vec<EventWire> = (0..64).map(|_| wire(&budget)).collect();
        let piece = vec![b'q'; 2048];
        for w in &mut streams {
            w.take(Bytes::from_static(b"{\"n\":1}\n"));
            w.take(Bytes::from_static(b"{\"t\":\""));
            for _ in 0..8 {
                w.take(Bytes::copy_from_slice(&piece));
                assert!(in_use(&budget, TOTAL) <= TOTAL);
            }
        }
        let before = in_use(&budget, TOTAL);
        assert!(before > 0 && before <= TOTAL);
        // Every line completes; nobody reads (the consumer is stalled): nothing is given back yet, and
        // the next lines cannot take more than the budget either.
        for w in &mut streams {
            w.take(Bytes::from_static(b"\"}\n{\"t\":\"more"));
            assert!(in_use(&budget, TOTAL) <= TOTAL);
        }
        assert!(
            in_use(&budget, TOTAL) >= before,
            "queued held lines keep their reservation"
        );
        assert!(streams.iter().all(|w| !w.pending.is_empty()));
        // One client reads its queue: exactly its frames' reservation goes back.
        let held_before = streams[0].held.reservation.bytes;
        let used = in_use(&budget, TOTAL);
        let out: Vec<u8> = streams[0]
            .pending
            .drain(..)
            .flat_map(|b| b.to_vec())
            .collect();
        let whole = format!(
            "data: {{\"n\":1}}\n\ndata: {{\"t\":\"{}\"}}\n\n",
            "q".repeat(8 * 2048)
        );
        // With the budget spent, its next line may already stream (its start is queued too).
        assert!(out.starts_with(whole.as_bytes()));
        let rest = &out[whole.len()..];
        assert!(
            rest.is_empty() || rest == b"data: {\"t\":\"more",
            "{rest:?}"
        );
        assert!(in_use(&budget, TOTAL) < used);
        assert_eq!(streams[0].held.reservation.bytes, held_before);
        // Every stream dropped with its queue and its held start: all of it comes back.
        drop(streams);
        assert_eq!(in_use(&budget, TOTAL), 0);
    }

    /// Re-review P2-2 (S5 b74a57a), on this wire: a held line is one buffer, and its allocated capacity
    /// (not its length) is what is reserved; tiny reads cost few allocations.
    #[test]
    fn the_held_buffer_is_one_allocation_charged_by_its_capacity() {
        const TOTAL: usize = 1024 * 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        let mut w = wire(&budget);
        w.take(Bytes::from_static(b"{\"t\":\""));
        let mut capacities = std::collections::BTreeSet::new();
        for _ in 0..20_000 {
            w.take(Bytes::from_static(b"r"));
            let held = &w.held;
            assert!(held.reservation.bytes >= held.buf.capacity());
            assert_eq!(in_use(&budget, TOTAL), held.reservation.bytes);
            capacities.insert(held.buf.capacity());
        }
        assert!(w.pending.is_empty(), "all of it is held");
        assert!(capacities.len() <= 16, "{} allocations", capacities.len());
        assert_eq!(w.held.buf.len(), 6 + 6 + 20_000);
        // Near the budget the buffer grows by exactly what is needed, still charged by capacity.
        let tight = Arc::new(Semaphore::new(10_000));
        let mut w = wire(&tight);
        w.take(Bytes::from([&b"{\"t\":\""[..], &[b'a'; 5994]].concat()));
        w.take(Bytes::copy_from_slice(&[b'b'; 3000]));
        assert_eq!(w.held.buf.capacity(), 9006);
        assert_eq!(tight.available_permits(), 994);
        w.take(Bytes::copy_from_slice(&[b'c'; 2000]));
        assert!(w.overflow && w.held.is_empty());
        // Streamed out, the old buffer still holds its reservation while queued; then it is all back.
        assert_eq!(tight.available_permits(), 994);
        w.pending.clear();
        assert_eq!(tight.available_permits(), 10_000);
        // A whole line the budget cannot cover still goes out, without a copy.
        let none = Arc::new(Semaphore::new(0));
        let mut w = wire(&none);
        assert_eq!(read(&mut w, b"{\"n\":1}\n"), b"data: {\"n\":1}\n\n");
    }

    #[tokio::test]
    async fn nothing_leaks_after_drops_breaks_and_ends() {
        const TOTAL: usize = 256 * 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        // Held mid-line, completed but queued, overflowed and queued, then all dropped.
        let mut a = wire(&budget);
        a.take(Bytes::from_static(b"{\"t"));
        let mut b = wire(&budget);
        b.take(Bytes::from_static(b"{\"t"));
        b.take(Bytes::from_static(b"\":1}\n{"));
        let mut c = wire(&budget);
        c.take(Bytes::from_static(b"{\"t\":\""));
        c.take(Bytes::from(vec![b'o'; HOLDBACK]));
        assert!(c.overflow);
        let mut d = wire(&budget);
        d.take(Bytes::from_static(b"\"data: x"));
        d.take(Bytes::from_static(b"\\n\\n\"\n\"da"));
        assert_eq!(d.pending.len(), 1, "the relayed frame is queued");
        assert!(budget.available_permits() < TOTAL);
        drop((a, b, c, d));
        assert_eq!(budget.available_permits(), TOTAL);
        // Through a response body: Core breaks, and Core ends inside a line.
        for broken in [true, false] {
            let (mut tx, body) = Chan::new(4);
            tx.send_data(Bytes::from_static(b"{\"n\":1}\n{\"ty"))
                .await
                .unwrap();
            if broken {
                tx.abort(std::io::Error::other("Core went away"));
            } else {
                drop(tx);
            }
            let out = Body::new(EventWire::with(
                Body::new(body),
                Arc::clone(&budget),
                HEARTBEAT,
            ))
            .collect()
            .await
            .unwrap()
            .to_bytes();
            assert_eq!(
                out,
                Bytes::from(format!("data: {{\"n\":1}}\n\n{FAILED_FRAME}"))
            );
            assert_eq!(budget.available_permits(), TOTAL);
        }
        // The client keeps a held line's frame a while (as a socket write would), then leaves.
        let (mut tx, body) = Chan::new(4);
        let mut stream = Body::new(EventWire::with(
            Body::new(body),
            Arc::clone(&budget),
            HEARTBEAT,
        ));
        tx.send_data(Bytes::from_static(b"{\"a")).await.unwrap();
        tx.send_data(Bytes::from_static(b"\":1}\n{\"b"))
            .await
            .unwrap();
        let first = stream.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(first, Bytes::from_static(b"data: {\"a\":1}\n\n"));
        assert!(budget.available_permits() < TOTAL);
        drop(stream);
        assert!(
            budget.available_permits() < TOTAL,
            "the copy still out keeps its reservation"
        );
        drop(first);
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
                let done = b"{\"type\":\"done\"}\n".to_vec();
                let out = through(vec![a.to_vec(), b.to_vec(), done], false, budget).await;
                assert_eq!(
                    out,
                    Bytes::from(format!("data: {}\ndata: {{\"type\":\"done\"}}\n\n", line))
                );
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
        tx.send_data(Bytes::from_static(b"{\"type\":\"done\"}\n"))
            .await
            .unwrap();
        drop(tx);
        let done = stream.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(done, Bytes::from_static(b"data: {\"type\":\"done\"}\n\n"));
        assert!(stream.frame().await.is_none());
    }

    /// S7 review P2-1 (b47c7d13): a clean end before the turn is complete (no `done` or `error`, as an
    /// event or as a relayed frame's event) ends with the failure frame; `title` after `done` still goes
    /// out and the end after it is clean.
    #[tokio::test]
    async fn an_end_before_the_turn_is_complete_ends_with_the_failure_frame() {
        let budget = Arc::new(Semaphore::new(HOLDBACK_BUDGET));
        let ends = |reads: Vec<&[u8]>| {
            let budget = Arc::clone(&budget);
            let reads: Vec<Vec<u8>> = reads.into_iter().map(<[u8]>::to_vec).collect();
            async move { through(reads, false, budget).await }
        };
        // An empty body.
        assert_eq!(ends(vec![]).await, Bytes::from(FAILED_FRAME));
        // meta and a delta, then Core ends.
        assert_eq!(
            ends(vec![b"{\"type\":\"meta\"}\n{\"type\":\"delta\",\"text\":\"Ste\"}\n"]).await,
            Bytes::from(format!(
                "data: {{\"type\":\"meta\"}}\n\ndata: {{\"type\":\"delta\",\"text\":\"Ste\"}}\n\n{FAILED_FRAME}"
            ))
        );
        // A Diary turn: the companion's frames, none of them a done or error event.
        let diary: &[u8] = b"{\"type\":\"status\",\"text\":\"Connecting to diary companion\"}\n\"data: {\\\"type\\\":\\\"answer\\\",\\\"text\\\":\\\"Dear diary\\\"}\\n\\n\"\n\"event: ping\\ndata: [DONE]\\n\\n\"\n";
        let framed = "data: {\"type\":\"status\",\"text\":\"Connecting to diary companion\"}\n\ndata: {\"type\":\"answer\",\"text\":\"Dear diary\"}\n\nevent: ping\ndata: [DONE]\n\n";
        assert_eq!(
            ends(vec![diary]).await,
            Bytes::from(format!("{framed}{FAILED_FRAME}"))
        );
        // A relayed frame whose event is done completes it (split across reads too); so does an error.
        let saved: &[u8] =
            b"\"id: 3\\ndata: {\\\"type\\\":\\\"done\\\",\\\"saved\\\":true}\\n\\n\"\n";
        let (a, b) = saved.split_at(9);
        assert_eq!(
            ends(vec![diary, a, b]).await,
            Bytes::from(format!(
                "{framed}id: 3\ndata: {{\"type\":\"done\",\"saved\":true}}\n\n"
            ))
        );
        assert_eq!(
            ends(vec![
                b"\"data: {\\\"type\\\":\\\"error\\\",\\\"text\\\":\\\"x\\\"}\\n\\n\"\n"
            ])
            .await,
            Bytes::from("data: {\"type\":\"error\",\"text\":\"x\"}\n\n")
        );
        // Contract §1.3: done, then the title, then a clean end.
        assert_eq!(
            ends(vec![
                b"{\"type\":\"meta\"}\n{\"type\":\"done\",\"model\":\"m\"}\n{\"type\":\"title\",",
                b"\"chatId\":\"c1\",\"title\":\"Tea poem\"}\n"
            ])
            .await,
            Bytes::from(
                "data: {\"type\":\"meta\"}\n\ndata: {\"type\":\"done\",\"model\":\"m\"}\n\ndata: {\"type\":\"title\",\"chatId\":\"c1\",\"title\":\"Tea poem\"}\n\n"
            )
        );
        // An error event completes it; a `type` naming done elsewhere does not.
        assert_eq!(
            ends(vec![b"{\"type\":\"error\",\"text\":\"x\"}\n"]).await,
            Bytes::from("data: {\"type\":\"error\",\"text\":\"x\"}\n\n")
        );
        assert_eq!(
            ends(vec![b"{\"type\":\"delta\",\"text\":\"done\",\"x\":{\"type\":\"done\"}}\n"]).await,
            Bytes::from(format!(
                "data: {{\"type\":\"delta\",\"text\":\"done\",\"x\":{{\"type\":\"done\"}}}}\n\n{FAILED_FRAME}"
            ))
        );
        // An event that streamed through (past the hold) is read from its start: a long done completes
        // the turn, a long delta does not.
        for (kind, complete) in [("done", true), ("delta", false)] {
            let start = format!("{{\"type\":\"{kind}\",\"t\":\"");
            let mut big = start.clone().into_bytes();
            big.resize(HOLDBACK + 16, b'x');
            let out = ends(vec![&big, b"\"}\n"]).await;
            assert_eq!(out.ends_with(FAILED_FRAME.as_bytes()), !complete, "{kind}");
            assert!(out.starts_with(format!("data: {start}").as_bytes()));
        }
        assert_eq!(budget.available_permits(), HOLDBACK_BUDGET);
    }

    /// S7 review P2-2 (b47c7d13): a whole event line goes out only when it is one JSON object in UTF-8,
    /// whole in one read or split across reads (held, or past the budget); a malformed one is dropped,
    /// Core's socket closes, and the failure frame ends the stream. Nothing is copied to check it, and
    /// the budget comes back.
    #[test]
    fn a_malformed_event_line_never_goes_out() {
        let budget = Arc::new(Semaphore::new(HOLDBACK_BUDGET));
        let meta: &[u8] = b"{\"type\":\"meta\"}\n";
        let framed = [
            &b"data: {\"type\":\"meta\"}\n\n"[..],
            FAILED_FRAME.as_bytes(),
        ]
        .concat();
        let bad_utf8: &[u8] = b"{\"type\":\"delta\",\"text\":\"caf\xc3\x28 \xff\"}\n";
        let bad: [&[u8]; 8] = [
            b"{bad}\n",
            b"{\"type\":\"delta\"\n",
            b"{\"type\":\"delta\"} {}\n",
            b"{\"type\":\"delta\",}\n",
            b"{\"text\":\"tab\there\"}\n",
            b"{\"type\":\"done\"}x\n",
            bad_utf8,
            b"{\"type\":\"delta\",\"text\":\"\xe2\x82\"}\n",
        ];
        for line in bad {
            let name = String::from_utf8_lossy(line).to_string();
            // Whole in one read.
            let mut w = wire(&budget);
            assert_eq!(read(&mut w, &[meta, line].concat()), framed, "{name}");
            assert!(w.done, "{name}");
            // Split at every point (invalid UTF-8 split inside a quoted value among them): held, then
            // finished in its buffer.
            for at in 1..line.len() - 1 {
                let mut w = wire(&budget);
                let mut out = read(&mut w, meta);
                out.extend(read(&mut w, &line[..at]));
                out.extend(read(&mut w, &line[at..]));
                assert_eq!(out, framed, "{name} split at {at}");
                assert!(w.done && w.pending.is_empty());
            }
        }
        // Past the budget the held start and the line's end are checked where they are.
        let tight = Arc::new(Semaphore::new(64));
        for (line, ok) in [
            (
                &b"{\"type\":\"delta\",\"text\":\"\xc3\xa9t\xc3\xa9 ok\"}\n"[..],
                true,
            ),
            (b"{\"type\":\"delta\",\"text\":\"\xc3\x28 bad\"}\n", false),
        ] {
            // Split inside the two-byte character: \xc3 held, \xa9 (or \x28) in the end.
            let at = line.iter().position(|&b| b == 0xc3).unwrap() + 1;
            let mut w = wire(&tight);
            assert!(read(&mut w, &line[..at]).is_empty());
            assert!(!w.held.is_empty());
            // Spend the rest of the budget, so the line's end cannot join the held buffer.
            let rest = tight.available_permits();
            let spent = tight.try_acquire_many(rest as u32).unwrap();
            let out = read(&mut w, &line[at..]);
            drop(spent);
            if ok {
                let want = [&b"data: "[..], &line[..line.len() - 1], b"\n\n"].concat();
                assert_eq!(out, want);
                assert!(!w.done);
            } else {
                assert_eq!(out, FAILED_FRAME.as_bytes());
                assert!(w.done);
            }
            drop(w);
            assert_eq!(tight.available_permits(), 64);
        }
        // What JSON.parse reads stays readable: escapes, lone surrogates, deep values, a repeated type.
        for line in [
            &b"{\"type\":\"delta\",\"text\":\"\\ud800 lone, \\udfff, \\u0000, \xf0\x9f\x98\x80\"}\n"[..],
            b"{\"type\":\"tool\",\"args\":{\"deep\":[[[{\"a\":-0,\"b\":1e21,\"c\":null}]]]}}\n",
            b"{\"type\":\"delta\",\"type\":\"done\"}\n",
            b"{ \"type\" : \"delta\" }\n",
        ] {
            let mut w = wire(&budget);
            let want = [&b"data: "[..], &line[..line.len() - 1], b"\n\n"].concat();
            assert_eq!(read(&mut w, line), want, "{}", String::from_utf8_lossy(line));
            assert!(!w.done);
        }
        let mut w = wire(&budget);
        read(&mut w, b"{\"type\":\"delta\",\"type\":\"done\"}\n");
        assert!(w.complete, "the last type counts");
        assert_eq!(budget.available_permits(), HOLDBACK_BUDGET);
    }

    /// S7 re-review P2-1 (6ed3633b): a key is a string like any other: a raw control character in one
    /// (at any depth) is not JSON the client can parse, whole, split at every point, or past the budget.
    #[test]
    fn a_raw_control_character_in_a_key_never_goes_out() {
        let budget = Arc::new(Semaphore::new(HOLDBACK_BUDGET));
        let meta: &[u8] = b"{\"type\":\"meta\"}\n";
        let framed = [
            &b"data: {\"type\":\"meta\"}\n\n"[..],
            FAILED_FRAME.as_bytes(),
        ]
        .concat();
        let bad: [&[u8]; 4] = [
            b"{\"type\":\"delta\",\"bad\tkey\":1}\n",
            b"{\"type\":\"delta\",\"nul\x00\":1}\n",
            b"{\"type\":\"tool\",\"args\":{\"inner\x01\":2}}\n",
            b"{\"\x1f\":1,\"type\":\"done\"}\n",
        ];
        for line in bad {
            let name = String::from_utf8_lossy(line).to_string();
            let mut w = wire(&budget);
            assert_eq!(read(&mut w, &[meta, line].concat()), framed, "{name}");
            for at in 1..line.len() - 1 {
                let mut w = wire(&budget);
                let mut out = read(&mut w, meta);
                out.extend(read(&mut w, &line[..at]));
                out.extend(read(&mut w, &line[at..]));
                assert_eq!(out, framed, "{name} split at {at}");
            }
            // Held, then the rest of the budget spent: the start and the end are checked apart.
            let tight = Arc::new(Semaphore::new(256));
            let mut w = wire(&tight);
            assert!(read(&mut w, &line[..4]).is_empty());
            let rest = tight.available_permits();
            let spent = tight
                .try_acquire_many(u32::try_from(rest).unwrap())
                .unwrap();
            assert_eq!(read(&mut w, &line[4..]), FAILED_FRAME.as_bytes(), "{name}");
            drop((spent, w));
            assert_eq!(tight.available_permits(), 256);
        }
        // Escaped, the same characters are fine in keys, as in values.
        let mut w = wire(&budget);
        let ok = b"{\"type\":\"delta\",\"tab\\tkey\\u0000\":\"v\\u0001\"}\n";
        assert_eq!(
            read(&mut w, ok),
            [&b"data: "[..], &ok[..ok.len() - 1], b"\n\n"].concat()
        );
        assert_eq!(budget.available_permits(), HOLDBACK_BUDGET);
    }

    /// S7 re-review P2-3 (6ed3633b): an event that streams through (the budget spent, or past the hold)
    /// completes the turn by its own `type` member wherever it stands and the last one counting (Core's
    /// Diary relay keeps the companion's key order), and a malformed one never has its frame ended.
    #[tokio::test]
    async fn an_event_streaming_through_is_read_by_its_own_type_member() {
        let none = Arc::new(Semaphore::new(0));
        let ends = |reads: &[&[u8]]| {
            let reads: Vec<Vec<u8>> = reads.iter().map(|r| r.to_vec()).collect();
            through(reads, false, Arc::clone(&none))
        };
        // diary-stream.cjs's reworded error in the companion's order: complete, no failure frame.
        assert_eq!(
            ends(&[b"{\"text\":\"failure\",", b"\"type\":\"error\"}\n"]).await,
            Bytes::from("data: {\"text\":\"failure\",\"type\":\"error\"}\n\n")
        );
        // The last type counts: a done that a later type replaces does not complete it.
        assert_eq!(
            ends(&[b"{\"type\":\"done\",\"x\":1,", b"\"type\":\"delta\"}\n"]).await,
            Bytes::from(format!(
                "data: {{\"type\":\"done\",\"x\":1,\"type\":\"delta\"}}\n\n{FAILED_FRAME}"
            ))
        );
        // `type` only as the object's own member, not inside another value.
        assert!(
            ends(&[b"{\"x\":{\"type\":", b"\"done\"},\"type\":\"delta\"}\n"])
                .await
                .ends_with(FAILED_FRAME.as_bytes())
        );
        // A malformed event streaming through: nothing from its bad bytes on goes out and its frame is
        // never ended, so the client never parses it.
        assert_eq!(
            ends(&[b"{\"type\":\"delta\",", b"\"bad\tkey\":1}\n"]).await,
            Bytes::from_static(b"data: {\"type\":\"delta\",")
        );
        assert_eq!(
            ends(&[b"{\"type\":\"delta\",", b"\"t\":1}x\n"]).await,
            Bytes::from_static(b"data: {\"type\":\"delta\",")
        );
        // Past the hold with the budget there: a long error whose type comes last completes it.
        let budget = Arc::new(Semaphore::new(HOLDBACK_BUDGET));
        let mut big = b"{\"text\":\"".to_vec();
        big.resize(HOLDBACK + 16, b'x');
        let out = through(
            vec![big, b"\",\"type\":\"error\"}\n".to_vec()],
            false,
            Arc::clone(&budget),
        )
        .await;
        assert!(out.ends_with(b"x\",\"type\":\"error\"}\n\n"));
        assert_eq!(budget.available_permits(), HOLDBACK_BUDGET);
    }

    /// S7 re-review P2-1 (990797fc): nesting has no fixed limit. The reviewer's relayed frame (an
    /// object holding 16,384 nested arrays) completes the turn, as does a far deeper event, whole or
    /// split; the stack that tracks the levels draws on the shared budget, and when the budget cannot
    /// cover it the line takes the over-budget path (the failure frame, or for an event already
    /// streaming the end without its frame's end), never a false end.
    #[tokio::test]
    async fn nesting_has_no_fixed_limit_and_its_stack_is_charged_to_the_budget() {
        let nested = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let budget = Arc::new(Semaphore::new(HOLDBACK_BUDGET));
        // The reviewer's case: a relayed frame whose event is done and 16,384 arrays deep inside.
        let frame = format!(
            "data: {{\"type\":\"done\",\"x\":{}}}\n\n",
            nested(16 * 1024)
        );
        let line = format!("{}\n", serde_json::to_string(&frame).unwrap());
        let out = through(vec![line.into_bytes()], false, Arc::clone(&budget)).await;
        assert_eq!(out, Bytes::from(frame));
        // A million levels: whole in one read, and split (held, then finished).
        let event = format!("{{\"type\":\"done\",\"x\":{}}}", nested(1_000_000));
        let framed = Bytes::from(format!("data: {event}\n\n"));
        let line = format!("{event}\n").into_bytes();
        let out = through(vec![line.clone()], false, Arc::clone(&budget)).await;
        assert_eq!(out, framed);
        let (a, b) = line.split_at(line.len() / 2);
        let out = through(vec![a.to_vec(), b.to_vec()], false, Arc::clone(&budget)).await;
        assert_eq!(out, framed);
        assert_eq!(budget.available_permits(), HOLDBACK_BUDGET);
        // The budget cannot cover the stack (a million levels need 125,000 bytes of it): the line is
        // over the budget. Whole, or with nothing of it out yet, it is refused with the failure frame
        // alone; already streaming (its first 1,000 bytes could not be held but could be checked, so
        // they went out) it ends without its frame's end.
        let tight = Arc::new(Semaphore::new(512));
        let out = through(vec![line.clone()], false, Arc::clone(&tight)).await;
        assert_eq!(out, Bytes::from(FAILED_FRAME));
        let out = through(vec![a.to_vec(), b.to_vec()], false, Arc::clone(&tight)).await;
        assert_eq!(out, Bytes::from(FAILED_FRAME));
        let (a, b) = line.split_at(1000);
        let out = through(vec![a.to_vec(), b.to_vec()], false, Arc::clone(&tight)).await;
        assert_eq!(out, Bytes::from([&b"data: "[..], a].concat()));
        assert_eq!(tight.available_permits(), 512);
    }

    /// S7 re-review P2-2 (990797fc): a relayed frame is decoded by this module into its reserved
    /// buffer: escapes, surrogate pairs and raw UTF-8 as Node writes them (a lone surrogate escape as
    /// U+FFFD, what Node's UTF-8 write makes of it), whole or split anywhere; what is not one JSON string
    /// is refused.
    #[test]
    fn a_relayed_frame_is_decoded_into_its_reservation() {
        let budget = Arc::new(Semaphore::new(HOLDBACK_BUDGET));
        let line: &[u8] = b"\"data: {\\\"t\\\":\\\"\\u00e9 \xc3\xa9 \\ud83d\\ude00 \\ud800 x \\udfff \\\\ \\/ \\t\\\"}\\n\\n\"\n";
        let want = "data: {\"t\":\"\u{e9} \u{e9} \u{1F600} \u{FFFD} x \u{FFFD} \\ / \t\"}\n\n";
        for at in 0..line.len() {
            let mut w = wire(&budget);
            let mut out = read(&mut w, &line[..at]);
            out.extend(read(&mut w, &line[at..]));
            assert_eq!(out, want.as_bytes(), "split at {at}");
            assert!(!w.done);
        }
        for bad in [
            &b"\"data: x\\n\\n\" \"\"\n"[..],
            b"\"data: \x01\\n\\n\"\n",
            b"\"data: \xc3\x28\\n\\n\"\n",
            b"\"data: \\x\\n\\n\"\n",
            b"\"data: \\u12g4\\n\\n\"\n",
        ] {
            let mut w = wire(&budget);
            assert_eq!(
                read(&mut w, bad),
                FAILED_FRAME.as_bytes(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(budget.available_permits(), HOLDBACK_BUDGET);
    }

    /// S7 re-review P2 (b0689c27): a relayed frame whose decoded copy takes what is left of the budget
    /// leaves none for the nesting of its data line. Whether it completes the turn is then unknown, so
    /// it must not go out as a frame that does not: the stream fails before it is sent (whole, and held
    /// then finished), and with eight permits more the same frame completes the turn.
    #[tokio::test]
    async fn a_decoded_frame_that_spends_the_budget_never_goes_out_unfinished() {
        let frame = format!(
            "data: {{\"type\":\"done\",\"x\":{}{}}}\n\n",
            "[".repeat(256),
            "]".repeat(256)
        );
        let line = serde_json::to_string(&frame).unwrap();
        let whole = format!("{line}\n").into_bytes();
        // Exactly the decoded copy's reservation (the line's length) and nothing more.
        let budget = Arc::new(Semaphore::new(line.len()));
        let out = through(vec![whole.clone()], false, Arc::clone(&budget)).await;
        assert_eq!(out, Bytes::from(FAILED_FRAME));
        assert_eq!(budget.available_permits(), line.len());
        // Four bytes held first (their four permits come back after decoding: still too few).
        let budget = Arc::new(Semaphore::new(4 + line.len()));
        let (a, b) = whole.split_at(4);
        let out = through(vec![a.to_vec(), b.to_vec()], false, Arc::clone(&budget)).await;
        assert_eq!(out, Bytes::from(FAILED_FRAME));
        assert_eq!(budget.available_permits(), 4 + line.len());
        // One spill word more: the stack fits, the frame goes out and completes the turn.
        let budget = Arc::new(Semaphore::new(line.len() + 8));
        let out = through(vec![whole], false, Arc::clone(&budget)).await;
        assert_eq!(out, Bytes::from(frame));
        assert_eq!(budget.available_permits(), line.len() + 8);
    }
}

/// S7 re-review P3 (b0689c27): the memory this module allocates for a line is accounted on the
/// budget itself, deterministically (the workspace forbids the unsafe code a counting allocator
/// needs): a relayed frame's buffer is exactly its reservation and never moves or grows; the nesting
/// stack is charged by its capacity at every growth; refusals take nothing; every permit comes back.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod accounting_tests {
    use super::*;

    fn reservation(budget: &Arc<Semaphore>) -> Reservation {
        Reservation {
            budget: Arc::clone(budget),
            bytes: 0,
        }
    }

    fn in_use(budget: &Semaphore, total: usize) -> usize {
        total - budget.available_permits()
    }

    #[test]
    fn a_relayed_frames_buffer_is_exactly_its_reservation() {
        const TOTAL: usize = 4096;
        let budget = Arc::new(Semaphore::new(TOTAL));
        let line: &[u8] = br#""data: {\"t\":\"\u00e9 \ud83d\ude00 \ud800 x \\ \/\"}\n\n""#;
        let len = line.len();
        // Decoding writes into the buffer as allocated: the same pointer and capacity throughout,
        // split anywhere.
        for at in 0..=len {
            let mut buf = Vec::with_capacity(len);
            let (ptr, cap) = (buf.as_ptr(), buf.capacity());
            let mut d = Decode::default();
            assert!(d.feed(&line[..at], &mut buf) && d.feed(&line[at..], &mut buf) && d.finish());
            assert_eq!((buf.as_ptr(), buf.capacity()), (ptr, cap), "split at {at}");
            assert!(buf.len() <= cap);
        }
        // Its capacity is its reservation, which is the line's length, all of it charged.
        let (buf, r) = decode_relayed(&[line], len, reservation(&budget)).unwrap();
        assert_eq!(buf.capacity(), r.bytes);
        assert_eq!(r.bytes, len);
        assert_eq!(in_use(&budget, TOTAL), len);
        // The permits come back only when the last owner of the frame drops it.
        let frame = reserved(buf, r);
        let copy = frame.clone();
        drop(frame);
        assert_eq!(in_use(&budget, TOTAL), len);
        drop(copy);
        assert_eq!(in_use(&budget, TOTAL), 0);
        // Too few permits: refused before anything is allocated, nothing taken.
        let short = Arc::new(Semaphore::new(len - 1));
        assert!(decode_relayed(&[line], len, reservation(&short)).is_none());
        assert_eq!(short.available_permits(), len - 1);
        // Malformed, or not one whole frame: refused, and every permit back.
        for bad in [
            &br#""data: x""#[..],
            br#""data: \x\n\n""#,
            b"\"data: \x01\\n\\n\"",
            br#""a\n\nb\n\n""#,
            br#""data: x\n\n" ""#,
        ] {
            assert!(decode_relayed(&[bad], bad.len(), reservation(&budget)).is_none());
            assert_eq!(
                in_use(&budget, TOTAL),
                0,
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        // The decoder never writes past the capacity it was given (it would rather refuse).
        let mut tiny = Vec::with_capacity(2);
        let cap = tiny.capacity();
        assert!(!Decode::default().feed(b"\"abcdefgh\"", &mut tiny));
        assert_eq!(tiny.capacity(), cap);
    }

    #[test]
    fn the_nesting_stack_is_charged_by_its_capacity() {
        const TOTAL: usize = 1024;
        let budget = Arc::new(Semaphore::new(TOTAL));
        let mut json = Json::new(reservation(&budget));
        assert!(json.feed(b"{\"type\":\"done\",\"x\":"));
        // The first INLINE_DEPTH levels cost nothing.
        assert!(json.feed(&[b'['; INLINE_DEPTH - 1]));
        assert_eq!(json.depth, INLINE_DEPTH);
        assert_eq!((json.spill.capacity(), json.reservation.bytes), (0, 0));
        assert_eq!(in_use(&budget, TOTAL), 0);
        // Past them the spill grows by doubling, each capacity charged before it is allocated.
        let mut capacities = Vec::new();
        for _ in 0..1024 {
            assert!(json.feed(b"["));
            assert_eq!(json.reservation.bytes, json.spill.capacity() * 8);
            assert_eq!(in_use(&budget, TOTAL), json.reservation.bytes);
            if capacities.last() != Some(&json.spill.capacity()) {
                capacities.push(json.spill.capacity());
            }
        }
        assert_eq!(capacities, [1, 2, 4, 8, 16]);
        assert!(json.feed(&[b']'; 1024 + INLINE_DEPTH - 1]) && json.feed(b"}"));
        assert_eq!(json.finish(), Some(true));
        drop(json);
        assert_eq!(in_use(&budget, TOTAL), 0, "back when the checker drops");
        // A growth the budget cannot cover (32 bytes doubled, 24 exact, 20 left): refused as over the
        // budget, the charged capacity unchanged, and all of it back on drop.
        let tight = Arc::new(Semaphore::new(20));
        let mut json = Json::new(reservation(&tight));
        assert!(json.feed(b"{\"x\":"));
        // Levels 1 to 383 fit (inline, then spill words 1 and 2); level 384 needs a third word.
        assert!(!json.feed(&[b'['; INLINE_DEPTH + 2 * 64]));
        assert!(json.over_budget);
        assert_eq!(json.depth, INLINE_DEPTH + 2 * 64);
        assert_eq!((json.spill.capacity(), json.reservation.bytes), (2, 16));
        assert_eq!(in_use(&tight, 20), 16);
        drop(json);
        assert_eq!(in_use(&tight, 20), 0);
        // The same, through a line: over the budget is its own outcome, not "not an event".
        let deep = [
            &b"{\"type\":\"done\",\"x\":"[..],
            &[b'['; 400],
            &[b']'; 400],
            b"}",
        ]
        .concat();
        assert_eq!(
            check_line(&[&deep], reservation(&tight)),
            Checked::OverBudget
        );
        assert_eq!(
            check_line(&[&deep], reservation(&budget)),
            Checked::Event(true)
        );
        assert_eq!(
            check_line(&[b"{x}"], reservation(&budget)),
            Checked::Malformed
        );
        assert_eq!((in_use(&tight, 20), in_use(&budget, TOTAL)), (0, 0));
    }
}
