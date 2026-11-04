//! `POST /api/diary/local-exchange`: core `routes/diary.cjs` (the add-on gate, the method, the 16 MiB
//! JSON body, the non-streamed reply) and `diary-stream.cjs` (the relay of the companion's event
//! stream and the words of its failures, #1422, #228, #230, #232), answer for answer. The sidecar stays
//! the only executor of an exchange; this module passes on the request the signed-in person made.
//!
//! What the exchange holds while it runs (core's model throttle shared with chat, the inference
//! maintenance gate and the long-document backfill's pause) is Node's memory: the caller hands
//! [`crate::Diary::local_exchange`] an admission future (the front's inference-admission lease, ask
//! `diaryExchange`), awaited after the add-on gate and the method and before the body is read, as core
//! took the throttle there; its [`ExchangeHold`] is dropped when the response ends (core's `leave` on
//! the response's finish or close), and a hold that is lost ends the relay as an interrupted stream.
use crate::body::StreamError;
use crate::client::{read_capped, CallError, Capped};
use crate::tenant::Content;
use bytes::Bytes;
use js_json::{wtf16, JValue};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{timeout_at, Instant};

const MODEL: &str = "Generating companion response…";
const EARLY: [&str; 3] = [
    "Opening diary context…",
    "Reading diary entries and memory…",
    MODEL,
];
const UNKNOWN: &str = "The diary connection was interrupted. The entry may still be saving; check the saved diary before sending again.";
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Stage {
    #[default]
    Start,
    Context,
    Model,
    Save,
}
#[derive(Default)]
pub struct StreamPolicy {
    stage: Stage,
    pub completed: bool,
}
/// `/^[a-z0-9_]{1,40}$/i` on a string.
fn reason(v: Option<&str>) -> bool {
    v.is_some_and(|s| {
        !s.is_empty() && s.len() <= 40 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    })
}

/// What the policy reads of an event: whether a property is there (JSON has no `undefined`), and
/// a property's value when it is a string.
pub trait EventView {
    fn has(&self, key: &str) -> bool;
    fn string(&self, key: &str) -> Option<String>;
}

impl EventView for JValue {
    fn has(&self, key: &str) -> bool {
        !matches!(self.get(key), JValue::Undefined)
    }
    fn string(&self, key: &str) -> Option<String> {
        self.get(key).as_str().map(str::to_owned)
    }
}

/// A top-level object read losslessly ([`crate::js_raw`]): `(key, value as JSON.stringify writes it)`.
pub(crate) struct RawEvent<'a>(pub(crate) &'a [(String, String)]);

impl EventView for RawEvent<'_> {
    fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }
    fn string(&self, key: &str) -> Option<String> {
        let (_, value) = self.0.iter().find(|(k, _)| k == key)?;
        crate::js_raw::string_of(value)
    }
}

/// What core did with one event.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Sent the frame as it came.
    Verbatim,
    /// Sent this event instead (a reworded failure).
    Replace(JValue),
    /// Sent the event without its `reason` (`withoutDisallowedReason`), everything else as
    /// `JSON.stringify` writes it.
    WithoutReason,
}

pub fn not_saved(code: &str) -> JValue {
    let text = match code {
        "storageLoginRejected" => "Diary storage refused the saved login — nothing was saved. Reconnect storage, then resend.",
        "storageThrottled" => "Diary storage is limiting sign-ins right now — nothing was saved. Wait a moment, then resend.",
        "contextFailed" => "Couldn't read your diary — nothing was saved. You can resend.",
        "modelFailed" => "Couldn't reach the local model — nothing was saved. You can resend.",
        _ => "Couldn't reach the Diary companion — nothing was saved. You can resend.",
    };
    JValue::obj([
        ("type", JValue::from("error")),
        ("code", JValue::from(code)),
        ("saved", JValue::Bool(false)),
        ("text", JValue::from(text)),
    ])
}
pub fn interrupted() -> JValue {
    JValue::obj([
        ("type", JValue::from("error")),
        ("text", JValue::from(UNKNOWN)),
    ])
}
impl StreamPolicy {
    /// One event: core's `track`, `refine` and `withoutDisallowedReason`.
    pub fn decide(&mut self, input: &dyn EventView) -> Decision {
        let kind = input.string("type");
        if kind.as_deref() == Some("status") {
            if let Some(text) = input.string("text") {
                if !EARLY.contains(&text.as_str()) {
                    self.stage = Stage::Save;
                } else if self.stage != Stage::Save {
                    if text == MODEL {
                        self.stage = Stage::Model;
                    } else if self.stage == Stage::Start {
                        self.stage = Stage::Context;
                    }
                }
            }
        }
        if kind.as_deref() == Some("done") {
            self.completed = true;
        }
        if kind.as_deref() != Some("error") {
            return Decision::Verbatim;
        }
        if !input.has("saved") && self.stage != Stage::Start && self.stage != Stage::Save {
            let model = self.stage == Stage::Model;
            let mut event = not_saved(if model {
                "modelFailed"
            } else {
                "contextFailed"
            });
            // A companion event naming another code never lends its reason.
            let code = input.string("code");
            let given = input.string("reason");
            if model
                && (!input.has("code") || code.as_deref() == Some("modelFailed"))
                && reason(given.as_deref())
            {
                if let (JValue::Obj(fields), Some(given)) = (&mut event, given) {
                    fields.push(("reason".into(), JValue::Str(given)));
                }
            }
            return Decision::Replace(event);
        }
        let allowed = input.string("code").as_deref() == Some("modelFailed")
            && reason(input.string("reason").as_deref());
        if input.has("reason") && !allowed {
            Decision::WithoutReason
        } else {
            Decision::Verbatim
        }
    }

    /// [`StreamPolicy::decide`] for a parsed event. `None`: core sent the frame as it came; `Some`:
    /// core sent `data: ${JSON.stringify(event)}` with this event.
    pub fn event(&mut self, input: &JValue) -> Option<JValue> {
        match self.decide(input) {
            Decision::Verbatim => None,
            Decision::Replace(event) => Some(event),
            Decision::WithoutReason => {
                let mut event = input.clone();
                if let JValue::Obj(fields) = &mut event {
                    fields.retain(|(k, _)| k != "reason");
                }
                Some(event)
            }
        }
    }
}

fn js_number(value: &JValue) -> f64 {
    match value {
        JValue::Num(n) => *n,
        JValue::Null => 0.0,
        JValue::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        JValue::Undefined | JValue::Obj(_) => f64::NAN,
        other => {
            let Ok(s) = js_json::to_js_string(other) else {
                return f64::NAN;
            };
            let s = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
            if s.is_empty() {
                return 0.0;
            }
            for (prefix, radix) in [("0x", 16), ("0b", 2), ("0o", 8)] {
                if s.to_ascii_lowercase().starts_with(prefix) {
                    return s
                        .get(2..)
                        .filter(|s| !s.is_empty())
                        .and_then(|digits| {
                            digits.chars().try_fold(0.0, |n, c| {
                                c.to_digit(radix)
                                    .map(|d| n * f64::from(radix) + f64::from(d))
                            })
                        })
                        .unwrap_or(f64::NAN);
                }
            }
            s.parse().unwrap_or(f64::NAN)
        }
    }
}
fn retry_after(value: &JValue) -> Option<u64> {
    let n = js_number(value);
    (n.is_finite() && n > 0.0).then(|| n.ceil().min(86400.0) as u64)
}
pub fn http_stream_failure(
    status: u16,
    detail: &JValue,
    retry_header: Option<&str>,
) -> Option<JValue> {
    let code = if detail.get("code").as_str() == Some("storageLoginRejected") || status == 424 {
        "storageLoginRejected"
    } else if detail.get("code").as_str() == Some("storageThrottled") {
        "storageThrottled"
    } else if detail.get("code").as_str() == Some("storageUpstream")
        || [400, 401, 403, 404, 410, 424, 428, 429, 503].contains(&status)
    {
        "diaryUnavailable"
    } else {
        return None;
    };
    let mut event = not_saved(code);
    if code == "storageThrottled" {
        let header = retry_header.map_or(JValue::Undefined, JValue::from);
        let value = if matches!(detail.get("retryAfter"), JValue::Null | JValue::Undefined) {
            &header
        } else {
            detail.get("retryAfter")
        };
        if let (Some(wait), JValue::Obj(fields)) = (retry_after(value), &mut event) {
            fields.push(("retryAfter".into(), JValue::Num(wait as f64)));
        }
    }
    Some(event)
}
/// Non-streamed failures intentionally retain only Core's two storage codes and bounded wait.
pub fn json_failure(body: &JValue) -> (JValue, Option<u64>) {
    let error = [body.get("detail"), body.get("error")]
        .into_iter()
        .find(|v| v.truthy())
        .cloned()
        .unwrap_or_else(|| JValue::from("Diary storage request failed"));
    let mut fields = vec![("error".to_owned(), error)];
    let code = body.get("code").as_str();
    let mut wait = None;
    if matches!(code, Some("storageThrottled" | "storageLoginRejected")) {
        fields.push(("code".into(), body.get("code").clone()));
        if code == Some("storageThrottled") {
            wait = retry_after(body.get("retryAfter"));
            if let Some(n) = wait {
                fields.push(("retryAfter".into(), JValue::Num(n as f64)));
            }
        }
    }
    (JValue::Obj(fields), wait)
}
fn spread(fields: &mut Vec<(String, JValue)>, value: &JValue) {
    use js_json::wtf16;
    match value {
        JValue::Obj(items) => {
            for (k, v) in items {
                wtf16::set(fields, k, v.clone());
            }
        }
        JValue::Arr(items) => {
            for (i, v) in items.iter().enumerate() {
                wtf16::set(fields, &i.to_string(), v.clone());
            }
        }
        JValue::Str(s) => {
            let mut i = 0;
            for c in s.chars() {
                let units = if let Some(unit) = wtf16::lone_unit(c) {
                    vec![unit]
                } else {
                    c.to_string().encode_utf16().collect()
                };
                for unit in units {
                    let c = char::from_u32(u32::from(unit)).or_else(|| wtf16::lone_char(unit));
                    if let Some(c) = c {
                        wtf16::set(fields, &i.to_string(), JValue::Str(c.to_string()));
                    }
                    i += 1;
                }
            }
        }
        _ => {}
    }
}
pub struct LegacyEvents {
    pub events: Vec<JValue>,
    pub completed: bool,
}
/// Compatibility with the older companion that answers one JSON object instead of an event stream.
pub fn legacy_events(full: &JValue) -> LegacyEvents {
    let mut events = Vec::new();
    let choice = match full.get("choices") {
        JValue::Arr(a) => a.first().map(|v| v.get("message")),
        _ => None,
    };
    if let Some(choice) = choice.filter(|v| v.truthy()) {
        if choice.get("reasoning_content").truthy() {
            events.push(JValue::obj([
                ("type", JValue::from("reasoning")),
                ("text", choice.get("reasoning_content").clone()),
            ]));
        }
        if !choice.get("content").truthy() {
            return LegacyEvents {
                events,
                completed: false,
            };
        }
        events.push(JValue::obj([
            ("type", JValue::from("answer")),
            ("text", choice.get("content").clone()),
        ]));
        let mut diary = vec![("type".into(), JValue::from("diary"))];
        spread(&mut diary, full.get("diary"));
        events.push(JValue::Obj(diary));
    } else {
        if full.get("reply").as_str().is_none() {
            return LegacyEvents {
                events,
                completed: false,
            };
        }
        if full.get("reasoning").truthy() {
            events.push(JValue::obj([
                ("type", JValue::from("reasoning")),
                ("text", full.get("reasoning").clone()),
            ]));
        }
        events.push(JValue::obj([
            ("type", JValue::from("answer")),
            ("text", full.get("reply").clone()),
        ]));
        let mut diary = vec![("type".into(), JValue::from("diary"))];
        spread(&mut diary, full);
        events.push(JValue::Obj(diary));
    }
    events.push(JValue::obj([("type", JValue::from("done"))]));
    LegacyEvents {
        events,
        completed: true,
    }
}

/// What an admitted exchange holds: `keep` is dropped when the response ends (that gives it back);
/// `lost` completes if the hold ended without that, and then the relay stops.
pub struct ExchangeHold {
    pub keep: Box<dyn Send>,
    pub lost: Pin<Box<dyn Future<Output = ()> + Send>>,
}

/// An exchange that was not admitted: core's answer, `{ error: message }` with this status (the
/// gate's 503 in its own words, the throttle's 429).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeRefusal {
    pub status: u16,
    pub message: String,
}

/// A hold lost before the sidecar was asked: the words of an admission Node could not confirm
/// (crates/engine-admission's fail-closed refusal).
pub const LEASE_LOST: &str = "The model is busy. Try again shortly.";
/// The throttle's words (core `routes/diary.cjs`).
pub const THROTTLED: &str = "Please wait before sending another message";
/// The first event of every stream.
pub const CONNECTING: &str = "Connecting to diary companion…";
/// The sidecar route.
pub(crate) const SIDECAR_PATH: &str = "/api/local-exchange";
/// `readCappedJson(upstream, 64 * 1024)` of a refusal, and `(upstream, 4 MiB)` of an older companion's reply.
const REFUSAL_CAP: usize = 64 * 1024;
const LEGACY_CAP: usize = 4 * 1024 * 1024;

/// Core's deadlines and caps for the exchange. [`ExchangeLimits::default`] is core's; tests lower them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExchangeLimits {
    /// `readJson(req, 16 * 1024 * 1024)`.
    pub body: usize,
    /// How long the body may take to arrive (core had only the server's 20-minute request timeout).
    pub body_deadline: Duration,
    /// `diaryFetchJson(..., 600000)` of the non-streamed exchange.
    pub reply_timeout: Duration,
    /// The stream's one deadline (`setTimeout(() => controller.abort(), 15 * 60 * 1000)`).
    pub stream_deadline: Duration,
    /// `heartbeatMs = 5000`.
    pub heartbeat: Duration,
}

impl Default for ExchangeLimits {
    fn default() -> Self {
        Self {
            body: 16 * 1024 * 1024,
            body_deadline: Duration::from_secs(120),
            reply_timeout: Duration::from_secs(600),
            stream_deadline: Duration::from_secs(15 * 60),
            heartbeat: Duration::from_secs(5),
        }
    }
}

/// `TextDecoder('utf-8').decode(chunk, { stream: true })`: a leading byte order mark is dropped once,
/// a sequence split across chunks waits for its end, an invalid one is U+FFFD per maximal subpart (as
/// the WHATWG decoder and `String::from_utf8_lossy` both do). Core never flushed the decoder, so an
/// incomplete sequence at the very end is dropped.
#[derive(Default)]
pub(crate) struct Utf8Stream {
    pending: Vec<u8>,
    started: bool,
}

impl Utf8Stream {
    pub(crate) fn decode(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        let mut out = String::new();
        let mut rest: &[u8] = &self.pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let valid = rest.get(..e.valid_up_to()).unwrap_or_default();
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    let after = rest.get(e.valid_up_to()..).unwrap_or_default();
                    match e.error_len() {
                        Some(n) => {
                            out.push('\u{FFFD}');
                            rest = after.get(n..).unwrap_or_default();
                        }
                        None => {
                            rest = after;
                            break;
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        if !self.started && !out.is_empty() {
            self.started = true;
            if let Some(stripped) = out.strip_prefix('\u{FEFF}') {
                out = stripped.to_owned();
            }
        }
        out
    }
}

/// `String.prototype.trim`'s white space and line terminators.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// The event a frame carries, as core read it: the first line that starts with `data: `, through
/// `JSON.parse`. Only an object can be anything but sent as it came (an array or a scalar has no
/// `type`), so only an object is read, losslessly and with no nesting limit ([`crate::js_raw`]).
pub(crate) fn frame_event(frame: &str) -> Option<Vec<(String, String)>> {
    let line = frame.split('\n').find(|l| l.starts_with("data: "))?;
    let json = line.strip_prefix("data: ")?;
    if json
        .trim_start_matches([' ', '\t', '\r'])
        .as_bytes()
        .first()
        != Some(&b'{')
    {
        return None;
    }
    match crate::js_raw::read(json)? {
        crate::js_raw::Top::Object(entries) => Some(entries),
        crate::js_raw::Top::Other(_) => None,
    }
}

/// The frame core sent for one data frame.
pub(crate) fn relayed(policy: &mut StreamPolicy, frame: String) -> String {
    let Some(entries) = frame_event(&frame) else {
        return frame;
    };
    match policy.decide(&RawEvent(&entries)) {
        Decision::Verbatim => frame,
        Decision::Replace(event) => frame_of(&event),
        Decision::WithoutReason => {
            match crate::js_raw::join(entries.iter().filter(|(k, _)| k != "reason")) {
                Some(json) => format!("data: {json}\n\n"),
                None => frame,
            }
        }
    }
}

/// A JSON reply as core's code read it, through `JSON.parse` (no nesting limit, `1e400` is
/// `Infinity`): `None` when `JSON.parse` would throw. A value nested past what [`JValue`] holds
/// (128 levels) is kept as an empty object, which none of these readers looks into.
pub(crate) fn parsed_reply(text: &str) -> Option<JValue> {
    match crate::js_raw::read(text)? {
        crate::js_raw::Top::Other(out) => wtf16::parse(&out).ok(),
        crate::js_raw::Top::Object(entries) => Some(JValue::Obj(
            entries
                .into_iter()
                .map(|(k, v)| {
                    let value = wtf16::parse(&v).unwrap_or(JValue::Obj(Vec::new()));
                    (k, value)
                })
                .collect(),
        )),
    }
}

/// `data: ${JSON.stringify(event)}\n\n`.
pub(crate) fn frame_of(event: &JValue) -> String {
    format!(
        "data: {}\n\n",
        wtf16::stringify(event).unwrap_or_else(|| "null".to_owned())
    )
}

/// Why the relay stopped before `done`: core's catch, which sends one event.
enum Failure {
    /// The connection was cut or the reply was not what the companion sends: nothing is known.
    Interrupted,
    /// A failure after which core knows nothing was saved.
    Known(JValue),
}

impl Failure {
    fn event(self) -> JValue {
        match self {
            Failure::Interrupted => interrupted(),
            Failure::Known(event) => event,
        }
    }
}

/// An older companion's whole reply: `JSON.parse` then the reply's own fields. Its values are spread
/// into the `diary` event, so they are read as Node wrote them back (`1e400` is `null`); one nested
/// past 128 levels is the interrupted stream.
fn legacy_reply(text: &str) -> Option<JValue> {
    wtf16::parse(&crate::js_raw::canonical(text)?).ok()
}

/// The browser's side of the stream: `false` once it has gone away.
struct Out {
    tx: mpsc::Sender<Result<Bytes, StreamError>>,
}

impl Out {
    /// Only ever awaited inside the relay's select, where the deadline, the browser leaving and a
    /// lost hold all win over a browser that stopped reading.
    async fn raw(&self, text: String) -> bool {
        self.tx.send(Ok(Bytes::from(text))).await.is_ok()
    }
    async fn event(&self, event: &JValue) -> bool {
        self.raw(frame_of(event)).await
    }
}

/// The stream task: the first status, the companion's events through the policy, a keep-alive
/// comment every `heartbeat`, and the one failure event; then the response ends and `hold` is
/// given back. A browser that goes away stops it (the sidecar connection is dropped), as core's
/// `res.on('close')` aborted its fetch; so does a lost hold and the deadline. No send waits outside
/// that select: a keep-alive is skipped while the browser is not reading (core's would queue), and
/// once the exchange has stopped and the hold is given back, the failure event gets at most one
/// `heartbeat` to find room.
pub(crate) async fn relay(
    diary: crate::Diary,
    ctx: crate::Ctx,
    body: Bytes,
    hold: ExchangeHold,
    tx: mpsc::Sender<Result<Bytes, StreamError>>,
    limits: ExchangeLimits,
) {
    let ExchangeHold { keep, mut lost } = hold;
    let out = Out { tx };
    let deadline = Instant::now() + limits.stream_deadline;
    let beat = limits.heartbeat.max(Duration::from_millis(1));
    let first = frame_of(&JValue::obj([
        ("type", JValue::from("status")),
        ("text", JValue::from(CONNECTING)),
    ]));
    // The channel is empty here: the first status never waits.
    let failure = if out.tx.try_send(Ok(Bytes::from(first))).is_ok() {
        let work = exchange(&diary, &ctx, body, &out, deadline);
        tokio::pin!(work);
        let mut ticks = tokio::time::interval_at(Instant::now() + beat, beat);
        loop {
            tokio::select! {
                done = &mut work => break done.err(),
                () = out.tx.closed() => break None,
                () = &mut lost => break Some(Failure::Interrupted),
                () = tokio::time::sleep_until(deadline) => break Some(Failure::Interrupted),
                _ = ticks.tick() => {
                    let beat = Ok(Bytes::from_static(b": keep-alive\n\n"));
                    if let Err(mpsc::error::TrySendError::Closed(_)) = out.tx.try_send(beat) {
                        break None;
                    }
                }
            }
        }
        // `work` (and with it the companion connection) ends with this block.
    } else {
        None
    };
    drop(keep);
    if let Some(failure) = failure {
        let last = Ok(Bytes::from(frame_of(&failure.event())));
        if let Err(mpsc::error::TrySendError::Full(last)) = out.tx.try_send(last) {
            let _ = tokio::time::timeout(beat, out.tx.send(last)).await;
        }
    }
}

/// What core did between the first status and its catch.
async fn exchange(
    diary: &crate::Diary,
    ctx: &crate::Ctx,
    body: Bytes,
    out: &Out,
    deadline: Instant,
) -> Result<(), Failure> {
    let tenant = diary
        .tenant_for(ctx)
        .await
        .map_err(|_| Failure::Interrupted)?;
    let sidecar = diary.sidecar();
    let url = sidecar
        .url(SIDECAR_PATH)
        .map_err(|_| Failure::Interrupted)?;
    let (mut upstream, _) = sidecar
        .send(
            &tenant,
            reqwest::Method::POST,
            &url,
            Content::Json(Some(&body)),
            Some(&body),
            || deadline,
        )
        .await
        .map_err(|e| match e {
            // No connection at all (refused, unresolvable): the request was never delivered.
            CallError::Refused => Failure::Known(not_saved("diaryUnavailable")),
            _ => Failure::Interrupted,
        })?;
    if !upstream.status().is_success() {
        let status = upstream.status().as_u16();
        let header = upstream
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        // `await readCappedJson(upstream, 64 KiB) || {}`, `{}` when it throws.
        let detail = match read_capped(&mut upstream, REFUSAL_CAP, deadline).await {
            Ok(Capped::Bytes(bytes)) => {
                parsed_reply(&String::from_utf8_lossy(&bytes)).filter(JValue::truthy)
            }
            _ => None,
        }
        .unwrap_or(JValue::Obj(Vec::new()));
        return Err(
            match http_stream_failure(status, &detail, header.as_deref()) {
                Some(event) => Failure::Known(event),
                None => Failure::Interrupted,
            },
        );
    }
    let sse = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    if !sse {
        // Compatibility with a previous sidecar during a rolling upgrade.
        let full = match read_capped(&mut upstream, LEGACY_CAP, deadline).await {
            Ok(Capped::Bytes(bytes)) => {
                legacy_reply(&String::from_utf8_lossy(&bytes)).ok_or(Failure::Interrupted)?
            }
            _ => return Err(Failure::Interrupted),
        };
        let legacy = legacy_events(&full);
        for event in &legacy.events {
            out.event(event).await;
        }
        return if legacy.completed {
            Ok(())
        } else {
            Err(Failure::Interrupted)
        };
    }
    let mut decoder = Utf8Stream::default();
    let mut buffer = String::new();
    let mut policy = StreamPolicy::default();
    loop {
        let next = timeout_at(deadline, upstream.chunk())
            .await
            .map_err(|_| Failure::Interrupted)?
            .map_err(|_| Failure::Interrupted)?;
        let Some(chunk) = next else { break };
        buffer.push_str(&decoder.decode(&chunk));
        // A heartbeat is never put inside a fragmented event: frames go out whole.
        while let Some(at) = buffer.find("\n\n") {
            let rest = buffer.split_off(at + 2);
            let frame = std::mem::replace(&mut buffer, rest);
            out.raw(relayed(&mut policy, frame)).await;
        }
    }
    if buffer.chars().any(|c| !js_space(c)) {
        return Err(Failure::Interrupted);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use js_json::wtf16;
    fn fixtures() -> JValue {
        wtf16::parse(include_str!("../tests/fixtures/local-exchange-node.json")).unwrap()
    }
    fn array(v: &JValue) -> &[JValue] {
        let JValue::Arr(a) = v else {
            panic!("array");
        };
        a
    }
    #[test]
    fn all_stream_stages_http_refusals_and_legacy_replies_match_real_node() {
        for case in array(fixtures().get("streams")) {
            let mut out = vec![JValue::obj([
                ("type", JValue::from("status")),
                ("text", JValue::from("Connecting to diary companion…")),
            ])];
            if matches!(case.get("events"), JValue::Arr(_)) {
                let mut policy = StreamPolicy::default();
                for event in array(case.get("events")) {
                    out.push(policy.event(event).unwrap_or_else(|| event.clone()));
                }
            } else if !matches!(case.get("json"), JValue::Undefined) {
                let legacy = legacy_events(case.get("json"));
                out.extend(legacy.events);
                if !legacy.completed {
                    out.push(interrupted());
                }
            } else {
                let status = case.get("http").as_num().unwrap() as u16;
                out.push(
                    http_stream_failure(
                        status,
                        case.get("detail"),
                        case.get("retryHeader").as_str(),
                    )
                    .unwrap_or_else(interrupted),
                );
            }
            assert_eq!(
                wtf16::stringify(&JValue::Arr(out)),
                wtf16::stringify(case.get("expected"))
            );
        }
    }
    #[test]
    fn nonstream_error_codes_details_and_number_coercion_match_real_node() {
        for case in array(fixtures().get("failures")) {
            let (body, wait) = json_failure(case.get("body"));
            assert_eq!(
                wtf16::stringify(&body),
                wtf16::stringify(case.get("expected").get("body"))
            );
            let expected_wait = case
                .get("expected")
                .get("headers")
                .get("Retry-After")
                .as_str()
                .map(|s| s.parse::<u64>().unwrap());
            assert_eq!(wait, expected_wait);
        }
    }
}
