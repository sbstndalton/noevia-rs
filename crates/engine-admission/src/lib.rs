//! Admission to the model engine from the Rust front (sbstndalton/noevia#2218, port contract row H).
//!
//! This front is the one admission authority ([`authority`]): it issues every lease, owns its renewal,
//! hard cap, expiry and release, orders Node's process starts and decides when a lease no longer covers
//! its holder. What a lease holds in Node's memory (the maintenance gate's inference count, the memory
//! budget and the gate's one-model queue, a machine's turn, the Diary exchange's throttle and pause) is
//! taken by Node's latch for a ticket this authority issued (core `inference-admission-lease.cjs`) and
//! held until the lease is given back.
//!
//! - A lease is renewed at Node every third of its TTL by a task of its own, and coverage is checked
//!   before each renewal and at every change of the authority: when Node says it ended, stops confirming
//!   it for a whole TTL, restarts (a newer incarnation registered), or the hard cap passes,
//!   [`Lease::lost`] is cancelled, so the holder's model request stops. It is never taken again silently.
//! - [`Lease::release`] gives it back. A lease dropped without that, or a ticket whose enter future is
//!   dropped at any await, gives it back from `Drop`; if even that cannot run, Node ends it after the TTL.
//! - No Node session, no answer from Node or an unreadable one is a refusal: admission fails closed.
//!
//! Partial port (row H): this authority owns lease *lifetime*. The admission *decisions* (maintenance
//! permit, budget/quarantine, machine assignment/readiness, the Diary throttle) are still made by Node's
//! latch and trusted here, until their owners (maintenance, budget, machines, chat) move them.
//!
//! - [`Admission::enter_gate`] (chat port S8): the inference gate alone, as a chat turn or the Diary local exchange
//!   meets it (core rust-chat-admission.cjs). Node waits at most [`GATE_WAIT`] for a held gate and stops when
//!   this caller goes away; the call the front then sends with [`Lease::id`] is covered by the lease and enters
//!   nothing again, so the lease is that call's one gate count.
//!
//! [`machines`]: which machine serves a chat role, read from the settings row Node owns.
pub mod authority;
pub mod machines;

pub use authority::{Authority, Ticket};
use model_stream::Cancel;
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const PATH: &str = "/internal/inference-admission/lease";
const TOKEN_HEADER: &str = "x-noevia-store-token";
/// The authority's epoch and the Node incarnation every latch call carries.
const EPOCH_HEADER: &str = "x-noevia-admission-epoch";
const INCARNATION_HEADER: &str = "x-noevia-admission-incarnation";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum buffered admission body. Larger refusals retain their status and retry metadata.
pub const MAX_ANSWER_BYTES: usize = 1024 * 1024;
pub const MAINTENANCE_MESSAGE: &str = "Model configuration is being applied. Try again shortly.";
/// A bounded, content-free refusal code, when supplied by the admission authority.
pub const REFUSAL_CODE_HEADER: &str = "x-noevia-admission-code";

#[derive(Debug)]
enum AnswerError {
    TooLarge,
    Unreadable,
}

async fn capped(mut response: reqwest::Response) -> Result<Vec<u8>, AnswerError> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_ANSWER_BYTES as u64)
    {
        return Err(AnswerError::TooLarge);
    }
    let mut out = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AnswerError::Unreadable)?
    {
        if chunk.len() > MAX_ANSWER_BYTES.saturating_sub(out.len()) {
            return Err(AnswerError::TooLarge);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}
/// Node's own queue limit is under this (`max_wait_ms`), so a queued wait ends with Node's answer.
pub const ENTER_TIMEOUT: Duration = Duration::from_secs(150);

/// Core chat-admission.cjs `CHAT_GATE_WAIT_MS`: Node waits at most this for a held gate.
pub const GATE_WAIT: Duration = Duration::from_secs(3);
/// How long a gate ask may take in all: Node's wait and a margin for the loopback call.
pub const GATE_TIMEOUT: Duration = Duration::from_secs(5);

/// Which call the inference gate is taken for (core rust-chat-admission.cjs `GATE_ROUTES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// `POST /api/chat`: a chat turn or a Cowork start.
    Chat,
    /// `POST /api/diary/local-exchange`.
    Diary,
}

impl Gate {
    pub fn name(self) -> &'static str {
        match self {
            Gate::Chat => "chat",
            Gate::Diary => "diary",
        }
    }
}

/// Why [`Admission::enter_gate`] gave no lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateRefusal {
    /// The gate's refusal, in Node's words: its status (503 while maintenance holds it), message and code.
    Refused(Refused),
    /// No answer from Node, or not one of this route's: the caller answers as for a Node that is down.
    NoAnswer,
}

/// Cancels its [`Cancel`] when dropped: the caller of [`Admission::enter_gate`] went away.
struct CancelOnDrop(Cancel);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Why a take of Node's latch gave no lease (the two asks word it differently).
enum Taken {
    Refused(Refused),
    NoAnswer,
    Cancelled,
}

/// What to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask<'a> {
    /// Embeddings need the maintenance guard, never a text-model load or router queue.
    /// PAIRED ONLY: callers MUST verify the boot-bound embedding-background-v1
    /// ownership capability. Older Core treats this request as a text-engine load.
    Embedding { model: &'a str },
    /// This machine's engine, for this model.
    Engine { model: &'a str },
    /// A chat role on another machine: nothing is admitted here, the machine counts a turn (#1880), and
    /// its health and served model are checked first (#1851).
    Machine {
        role: &'a str,
        machine: &'a str,
        model: &'a str,
    },
    /// The Diary local exchange of this account: what core's own exchange took (the maintenance
    /// gate, waited for briefly as chat waits; then the model throttle chat shares, `429` when over;
    /// and the long-document backfill's pause), held until the lease ends. No model is named: the
    /// companion picks its own, and the gate is what a tune or a preset apply waits for.
    DiaryExchange { user: &'a str },
}

/// Why no lease was given. `message` is the words to show; `code` is core's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub status: u16,
    pub message: String,
    pub code: Option<String>,
    /// Retry-After supplied by Node, including HTTP-date values.
    pub retry_after: Option<String>,
}

impl Refused {
    fn oversized(status: u16, headers: &reqwest::header::HeaderMap) -> Option<Self> {
        (status == 503 || status == 409).then(|| Self {
            status,
            message: MAINTENANCE_MESSAGE.into(),
            code: headers
                .get(REFUSAL_CODE_HEADER)
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty() && s.len() <= 128)
                .map(str::to_owned),
            retry_after: headers
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        })
    }

    fn unavailable() -> Refused {
        Refused {
            status: 503,
            message: "The model is busy. Try again shortly.".into(),
            code: None,
            retry_after: None,
        }
    }
}

/// The client. Cheap to share.
pub struct Admission {
    client: reqwest::Client,
    base: String,
    token: Option<HeaderValue>,
    authority: Authority,
}

impl std::fmt::Debug for Admission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admission")
            .field("base", &self.base)
            .field("token", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

impl Admission {
    /// A client of this process's one authority ([`authority::global`]).
    pub fn new(base: &str, token: Option<&str>) -> Result<Admission, String> {
        Self::with_authority(base, token, authority::global().clone())
    }

    pub fn with_authority(
        base: &str,
        token: Option<&str>,
        authority: Authority,
    ) -> Result<Admission, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| format!("inference admission client: {e}"))?;
        let token = token
            .filter(|t| !t.is_empty())
            .and_then(|t| HeaderValue::from_str(t).ok())
            .map(|mut t| {
                t.set_sensitive(true);
                t
            });
        Ok(Admission {
            client,
            base: base.trim_end_matches('/').to_owned(),
            token,
            authority,
        })
    }

    pub fn authority(&self) -> &Authority {
        &self.authority
    }

    fn request(
        &self,
        method: reqwest::Method,
        ticket: &Ticket,
        suffix: &str,
    ) -> Option<reqwest::RequestBuilder> {
        let token = self.token.clone()?;
        Some(
            self.client
                .request(method, format!("{}{PATH}/{}{suffix}", self.base, ticket.id))
                .header(HeaderName::from_static(TOKEN_HEADER), token)
                .header(HeaderName::from_static(EPOCH_HEADER), ticket.epoch.as_str())
                .header(
                    HeaderName::from_static(INCARNATION_HEADER),
                    ticket.incarnation.to_string(),
                ),
        )
    }

    /// Takes a lease: this authority issues it, then Node's latch takes what it holds there. Fails
    /// closed: no Node session, no answer or an unreadable one is the `503` busy refusal. Cancel-safe at
    /// every await: the ticket is given back (here and to Node) when this future is dropped.
    pub async fn enter(self: &Arc<Self>, ask: Ask<'_>, cancel: &Cancel) -> Result<Lease, Refused> {
        let body = match ask {
            Ask::Embedding { model } => json!({ "model": model, "embedding": true }),
            Ask::Engine { model } => json!({ "model": model }),
            Ask::Machine {
                role,
                machine,
                model,
            } => json!({ "role": role, "machine": machine, "model": model }),
            Ask::DiaryExchange { user } => json!({ "diaryExchange": { "user": user } }),
        };
        self.take(&body, ENTER_TIMEOUT, cancel)
            .await
            .map_err(|t| match t {
                Taken::Refused(r) => r,
                Taken::NoAnswer => Refused::unavailable(),
                Taken::Cancelled => cancelled(),
            })
    }

    /// Chat port S8: takes the inference gate for one `gate` call (core rust-chat-admission.cjs). Node waits at
    /// most [`GATE_WAIT`]; the whole ask at most [`GATE_TIMEOUT`]. Dropping this future (the client left) ends
    /// the ask, which ends Node's wait; a lease that was already on its way is given back, never kept.
    pub async fn enter_gate(self: &Arc<Self>, gate: Gate) -> Result<Lease, GateRefusal> {
        let gone = Cancel::new();
        let _guard = CancelOnDrop(gone.clone());
        let me = Arc::clone(self);
        // Its own task, so that a grant that arrives as the caller leaves is still read and given back.
        let asking = tokio::spawn(async move {
            me.take(&json!({ "gate": gate.name() }), GATE_TIMEOUT, &gone)
                .await
        });
        match asking.await {
            Ok(Ok(lease)) => Ok(lease),
            // The gate refuses only with its 503; anything else is not this route's answer (a Core that is
            // not this front's pair answers an ask it does not know with 400 or 404).
            Ok(Err(Taken::Refused(r))) if r.status == 503 => Err(GateRefusal::Refused(r)),
            _ => Err(GateRefusal::NoAnswer),
        }
    }

    /// One ticketed ask of Node's latch (`body` says what to hold). Fails closed; cancel-safe at every await.
    async fn take(
        self: &Arc<Self>,
        body: &Value,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<Lease, Taken> {
        if self.token.is_none() {
            return Err(Taken::NoAnswer);
        }
        let ticket = self.authority.issue().ok_or(Taken::NoAnswer)?;
        // From here the ticket is a guard: every early return and every drop gives it back.
        let guard = Arc::new(Held {
            admission: Arc::clone(self),
            ticket,
            released: AtomicBool::new(false),
            confirmed_until: std::sync::Mutex::new(tokio::time::Instant::now()),
        });
        let mut pending = Pending(Some(Arc::clone(&guard)));
        let Some(req) = self.request(reqwest::Method::PUT, &guard.ticket, "") else {
            return Err(Taken::NoAnswer);
        };
        // Node arms its latch's TTL no earlier than it receives this request, so the grant is proven to
        // hold only until `sent + ttl`: an answer that took longer is not a lease (row H review P1).
        let sent = tokio::time::Instant::now();
        let send = req
            .header("content-type", "application/json")
            .timeout(timeout)
            .body(body.to_string())
            .send();
        let answer = tokio::select! {
            () = cancel.cancelled() => return Err(Taken::Cancelled),
            r = send => r,
        };
        let response = answer.map_err(|_| Taken::NoAnswer)?;
        let status = response.status().as_u16();
        let metadata = Refused::oversized(status, response.headers());
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let reading = capped(response);
        let bytes = tokio::select! {
            () = cancel.cancelled() => return Err(Taken::Cancelled),
            b = reading => match b {
                Ok(bytes) => bytes,
                Err(AnswerError::TooLarge) => return Err(metadata.map_or(Taken::NoAnswer, Taken::Refused)),
                Err(AnswerError::Unreadable) => return Err(Taken::NoAnswer),
            },
        };
        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status != 201 {
            let message = parsed.get("error").and_then(Value::as_str);
            return Err(match (status, message) {
                (404 | 410, _) | (_, None) => Taken::NoAnswer,
                (_, Some(m)) => Taken::Refused(Refused {
                    status,
                    message: m.to_owned(),
                    retry_after,
                    code: parsed
                        .get("code")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }),
            });
        }
        let ttl = parsed
            .get("ttlMs")
            .and_then(Value::as_u64)
            .filter(|t| *t > 0)
            .map(Duration::from_millis)
            .ok_or(Taken::NoAnswer)?;
        // Node echoes the ticket it latched; anything else is not this lease.
        if parsed.get("lease").and_then(Value::as_str) != Some(guard.ticket.id.as_str()) {
            return Err(Taken::NoAnswer);
        }
        // Coverage may have ended while Node answered (a newer Node start registered).
        if !self.authority.covers(&guard.ticket.id) {
            return Err(Taken::NoAnswer);
        }
        let headers = parsed
            .get("headers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(k, v)| {
                k.as_str() == "x-noevia-lease"
                    && v.as_str().is_some_and(|t| {
                        !t.is_empty()
                            && t.len() <= 128
                            && t.bytes().all(|b| b.is_ascii_alphanumeric())
                    })
            })
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
            .collect();
        let until = sent + ttl;
        if tokio::time::Instant::now() >= until {
            return Err(Taken::NoAnswer);
        }
        guard.confirm(until);
        let inner = pending.0.take().ok_or(Taken::NoAnswer)?;
        let lost = Cancel::new();
        let renewing = tokio::spawn(renew(Arc::clone(&inner), ttl, lost.clone(), sent));
        Ok(Lease {
            inner,
            headers,
            lost,
            renewing,
        })
    }

    async fn renew_once(&self, ticket: &Ticket) -> Option<bool> {
        let res = self
            .request(reqwest::Method::POST, ticket, "/renew")?
            .timeout(CALL_TIMEOUT)
            .send()
            .await
            .ok()?;
        match res.status().as_u16() {
            200 => Some(true),
            404 | 410 => Some(false),
            _ => None,
        }
    }

    async fn release_once(&self, ticket: &Ticket) {
        if let Some(req) = self.request(reqwest::Method::DELETE, ticket, "") {
            let _ = req.timeout(CALL_TIMEOUT).send().await;
        }
    }
}

fn cancelled() -> Refused {
    Refused {
        status: 499,
        message: "Cancelled.".into(),
        code: None,
        retry_after: None,
    }
}

struct Held {
    admission: Arc<Admission>,
    ticket: Ticket,
    released: AtomicBool,
    /// Until when Node's latch is proven held: the send time of the last confirmed call plus the TTL.
    confirmed_until: std::sync::Mutex<tokio::time::Instant>,
}

impl Held {
    fn confirm(&self, until: tokio::time::Instant) {
        let mut c = match self.confirmed_until.lock() {
            Ok(c) => c,
            Err(p) => p.into_inner(),
        };
        if until > *c {
            *c = until;
        }
    }

    fn confirmed_until(&self) -> tokio::time::Instant {
        match self.confirmed_until.lock() {
            Ok(c) => *c,
            Err(p) => *p.into_inner(),
        }
    }

    /// Covered now: the authority's entry holds and Node's latch is proven held.
    fn covered(&self) -> bool {
        !self.released.load(Ordering::SeqCst)
            && self.admission.authority.covers(&self.ticket.id)
            && tokio::time::Instant::now() < self.confirmed_until()
    }

    /// Ends the lease here and asks Node to give its latch back (once). The authority's entry ends at
    /// once, so nothing is covered from then on whether or not Node answers.
    fn give_back(self: &Arc<Self>) -> Option<impl std::future::Future<Output = ()>> {
        if self.released.swap(true, Ordering::SeqCst) {
            return None;
        }
        self.admission.authority.end(&self.ticket.id);
        let me = Arc::clone(self);
        Some(async move { me.admission.release_once(&me.ticket).await })
    }

    fn give_back_detached(self: &Arc<Self>) {
        if let Some(fut) = self.give_back() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(fut);
            }
        }
    }
}

/// The ticket between its issue and the lease: dropped (any early return, any cancelled await), it
/// gives the ticket back, so a latch Node took but whose answer never arrived is released too.
struct Pending(Option<Arc<Held>>);

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(held) = self.0.take() {
            held.give_back_detached();
        }
    }
}

/// Renews the latch on an absolute schedule (`sent + every`, then each confirmed call's send time plus
/// `every`) that no notification resets, and ends the lease at the first of: a newer Node start or an
/// ended entry (checked at every authority change, also while a renewal is in flight), the hard cap, or
/// the absolute deadline until which Node's latch is proven held (`confirmed_until`), whatever the HTTP
/// calls in flight are doing.
async fn renew(held: Arc<Held>, ttl: Duration, lost: Cancel, sent: tokio::time::Instant) {
    use tokio::time::{sleep_until, Instant};
    let authority = held.admission.authority.clone();
    let every = (ttl / 3).max(Duration::from_millis(50));
    let hard_at = authority.hard_at(&held.ticket.id);
    let mut next = sent + every;
    let end = |held: &Arc<Held>| {
        lost.cancel();
        held.give_back_detached();
    };
    loop {
        if held.released.load(Ordering::SeqCst) {
            return;
        }
        let changed = authority.changed();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if !held.covered() {
            return end(&held);
        }
        let deadline = hard_at.map_or(held.confirmed_until(), |h| h.min(held.confirmed_until()));
        tokio::select! {
            () = sleep_until(next) => {}
            () = &mut changed => continue,
            () = sleep_until(deadline) => continue,
        }
        // A renewal: raced against every way coverage can end while it is in flight.
        let started = Instant::now();
        let call = held.admission.renew_once(&held.ticket);
        tokio::pin!(call);
        let answer = loop {
            let changed = authority.changed();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !held.covered() {
                return end(&held);
            }
            let deadline =
                hard_at.map_or(held.confirmed_until(), |h| h.min(held.confirmed_until()));
            tokio::select! {
                a = &mut call => break a,
                () = &mut changed => {}
                () = sleep_until(deadline) => {}
            }
        };
        match answer {
            Some(true) => {
                held.confirm(started + ttl);
                next = started + every;
            }
            Some(false) => return end(&held),
            // No answer: try again on the schedule; the proven deadline is not extended.
            None => next = Instant::now() + every,
        }
    }
}

/// A held lease. Watch [`Lease::lost`]: it is cancelled once the lease no longer holds (Node ended it,
/// stopped confirming it, restarted, or the hard cap passed); the holder's model request must stop then.
pub struct Lease {
    inner: Arc<Held>,
    headers: Vec<(String, String)>,
    lost: Cancel,
    renewing: tokio::task::JoinHandle<()>,
}

impl Lease {
    /// Headers to send with the model request (the inference gate's lease, when there is a gate).
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn lost(&self) -> &Cancel {
        &self.lost
    }

    /// The authority's id of this lease.
    pub fn id(&self) -> &str {
        &self.inner.ticket.id
    }

    /// Whether the lease still holds, checked synchronously before a step.
    pub fn covered(&self) -> bool {
        !self.lost.is_cancelled() && self.inner.covered()
    }

    /// Gives the lease back and waits for Node's answer (bounded).
    pub async fn release(self) {
        self.renewing.abort();
        if let Some(fut) = self.inner.give_back() {
            fut.await;
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.renewing.abort();
        self.inner.give_back_detached();
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct Node {
        /// `METHOD path token epoch incarnation` of each request.
        seen: Mutex<Vec<String>>,
        renew_status: Mutex<u16>,
        asks: Mutex<Vec<String>>,
        /// `None`: echo the ticket id; `Some`: this lease id instead.
        echo: Mutex<Option<String>>,
        /// Never answer a PUT (a wait that does not end).
        stall: bool,
    }

    fn header<'a>(text: &'a str, name: &str) -> &'a str {
        text.lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case(name).then(|| v.trim())
            })
            .unwrap_or("-")
    }

    /// A synthetic Node latch: `PUT` answers `grant` (echoing the ticket); renewals `renew_status`.
    async fn node_with(grant: (u16, String), stall: bool) -> (String, Arc<Node>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Node {
            seen: Mutex::default(),
            renew_status: Mutex::new(200),
            asks: Mutex::default(),
            echo: Mutex::new(None),
            stall,
        });
        let st = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let st = Arc::clone(&st);
                let grant = grant.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let first = text.lines().next().unwrap_or("").to_owned();
                    let path = first.split(' ').nth(1).unwrap_or("").to_owned();
                    let line = format!(
                        "{} {path} {} {} {}",
                        first.split(' ').next().unwrap_or(""),
                        if header(&text, "x-noevia-store-token") == "secret" {
                            "T"
                        } else {
                            "-"
                        },
                        header(&text, "x-noevia-admission-epoch"),
                        header(&text, "x-noevia-admission-incarnation"),
                    );
                    st.seen.lock().unwrap().push(line);
                    if let Some((_, body)) = text.split_once("\r\n\r\n") {
                        st.asks.lock().unwrap().push(body.to_owned());
                    }
                    let id = path
                        .rsplit('/')
                        .find(|p| p.len() == 32)
                        .unwrap_or("")
                        .to_owned();
                    let (status, body) = if first.starts_with("POST") && first.contains("/renew") {
                        (*st.renew_status.lock().unwrap(), "{}".to_owned())
                    } else if first.starts_with("PUT") {
                        if st.stall {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                        let echo = st.echo.lock().unwrap().clone().unwrap_or(id);
                        (grant.0, grant.1.replace("{ID}", &echo))
                    } else {
                        (204, String::new())
                    };
                    let reply = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    let _ = s.write_all(reply.as_bytes()).await;
                });
            }
        });
        (base, state)
    }

    async fn node(grant: (u16, String)) -> (String, Arc<Node>) {
        node_with(grant, false).await
    }

    const GRANT: &str =
        r#"{"lease":"{ID}","ttlMs":300,"headers":{"x-noevia-lease":"abc123","x-evil":"no"}}"#;

    fn admission_with(base: &str, authority: &Authority) -> Arc<Admission> {
        Arc::new(Admission::with_authority(base, Some("secret"), authority.clone()).unwrap())
    }

    fn registered() -> Authority {
        let a = Authority::new();
        a.node_session("boot-1", 1).unwrap();
        a
    }

    fn admission(base: &str) -> Arc<Admission> {
        admission_with(base, &registered())
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    fn deletes(state: &Node) -> usize {
        state
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("DELETE"))
            .count()
    }

    #[tokio::test]
    async fn a_lease_is_issued_here_latched_by_ticket_and_given_back() {
        let (base, state) = node((201, GRANT.into())).await;
        let authority = registered();
        let lease = admission_with(&base, &authority)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .unwrap();
        assert_eq!(
            lease.headers(),
            &[("x-noevia-lease".to_owned(), "abc123".to_owned())]
        );
        assert!(lease.covered());
        assert_eq!(authority.held(), 1);
        let id = lease.id().to_owned();
        lease.release().await;
        assert_eq!(authority.held(), 0);
        let seen = state.seen.lock().unwrap().clone();
        let put = format!(
            "PUT /internal/inference-admission/lease/{id} T {} 1",
            authority.epoch()
        );
        assert_eq!(seen[0], put, "{seen:?}");
        assert!(
            seen.iter().any(|l| l.starts_with(&format!(
                "DELETE /internal/inference-admission/lease/{id} T"
            ))),
            "{seen:?}"
        );
        assert_eq!(state.asks.lock().unwrap()[0], r#"{"model":"m"}"#);
    }

    #[tokio::test]
    async fn without_a_node_session_nothing_is_asked_and_it_fails_closed() {
        let (base, state) = node((201, GRANT.into())).await;
        let refused = admission_with(&base, &Authority::new())
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .err()
            .unwrap();
        assert_eq!(refused, Refused::unavailable());
        assert!(state.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_diary_exchange_asks_for_its_account_and_a_throttle_refusal_keeps_its_429() {
        let (base, state) = node((
            429,
            r#"{"error":"Please wait before sending another message"}"#.into(),
        ))
        .await;
        let authority = registered();
        let refused = admission_with(&base, &authority)
            .enter(
                Ask::DiaryExchange {
                    user: "u-synthetic",
                },
                &Cancel::new(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(
            (refused.status, refused.message.as_str(), refused.code),
            (429, "Please wait before sending another message", None)
        );
        assert_eq!(
            state.asks.lock().unwrap().first().map(String::as_str),
            Some(r#"{"diaryExchange":{"user":"u-synthetic"}}"#)
        );
        assert_eq!(authority.held(), 0, "a refused ticket is given back");
    }

    #[tokio::test]
    async fn a_refusal_keeps_nodes_words_and_no_answer_fails_closed() {
        let (base, _s) = node((409, r#"{"error":"over budget","code":"overBudget"}"#.into())).await;
        let refused = admission(&base)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .err()
            .unwrap();
        assert_eq!(
            (
                refused.status,
                refused.message.as_str(),
                refused.code.as_deref()
            ),
            (409, "over budget", Some("overBudget"))
        );
        for grant in [
            (404, "{}".to_owned()),
            (410, r#"{"error":"retired"}"#.to_owned()),
            (201, r#"{"lease":"{ID}","ttlMs":0}"#.to_owned()),
            (500, "x".to_owned()),
        ] {
            let (base, state) = node(grant).await;
            let authority = registered();
            let refused = admission_with(&base, &authority)
                .enter(Ask::Engine { model: "m" }, &Cancel::new())
                .await
                .err()
                .unwrap();
            assert_eq!(refused, Refused::unavailable());
            settle().await;
            assert_eq!(authority.held(), 0);
            assert_eq!(deletes(&state), 1, "the ticket is given back to Node too");
        }
        let refused = admission("http://127.0.0.1:9")
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .err()
            .unwrap();
        assert_eq!(refused, Refused::unavailable());
        let no_token =
            Arc::new(Admission::with_authority("http://127.0.0.1:9", None, registered()).unwrap());
        assert!(no_token
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_grant_of_another_lease_is_not_this_lease() {
        let (base, state) = node((201, GRANT.into())).await;
        *state.echo.lock().unwrap() = Some("0123456789abcdef0123456789abcdef".into());
        let authority = registered();
        let refused = admission_with(&base, &authority)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .err()
            .unwrap();
        assert_eq!(refused, Refused::unavailable());
        settle().await;
        assert_eq!((authority.held(), deletes(&state)), (0, 1));
    }

    #[tokio::test]
    async fn dropping_a_lease_gives_it_back() {
        let (base, state) = node((201, GRANT.into())).await;
        let authority = registered();
        let lease = admission_with(&base, &authority)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .unwrap();
        drop(lease);
        assert_eq!(authority.held(), 0);
        settle().await;
        assert_eq!(deletes(&state), 1);
    }

    #[tokio::test]
    async fn renewal_runs_and_a_lease_node_ended_is_lost_and_not_taken_again() {
        let (base, state) = node((201, GRANT.into())).await;
        let lease = admission(&base)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(state
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("/renew")));
        assert!(!lease.lost().is_cancelled());
        *state.renew_status.lock().unwrap() = 404;
        tokio::time::timeout(Duration::from_secs(5), lease.lost().cancelled())
            .await
            .unwrap();
        assert!(!lease.covered());
        let puts = state
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("PUT"))
            .count();
        assert_eq!(puts, 1);
    }

    #[tokio::test]
    async fn a_node_that_stops_confirming_loses_the_lease_within_its_ttl() {
        let (base, state) = node((201, GRANT.into())).await;
        let lease = admission(&base)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .unwrap();
        *state.renew_status.lock().unwrap() = 500;
        tokio::time::timeout(Duration::from_millis(900), lease.lost().cancelled())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_node_restart_fences_the_lease_at_once() {
        let (base, state) = node((201, GRANT.into())).await;
        let authority = registered();
        let lease = admission_with(&base, &authority)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .unwrap();
        authority.node_session("boot-2", 2).unwrap();
        assert!(
            !lease.covered(),
            "checked synchronously before the next step"
        );
        tokio::time::timeout(Duration::from_millis(80), lease.lost().cancelled())
            .await
            .unwrap();
        settle().await;
        assert_eq!((authority.held(), deletes(&state)), (0, 1));
        // The new start's leases carry its incarnation.
        let next = admission_with(&base, &authority)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .unwrap();
        assert!(state.seen.lock().unwrap().last().unwrap().ends_with(" 2"));
        next.release().await;
    }

    #[tokio::test]
    async fn the_hard_cap_ends_the_lease() {
        let (base, _state) = node((201, r#"{"lease":"{ID}","ttlMs":30000}"#.into())).await;
        let authority = Authority::with_hard(Duration::from_millis(120));
        authority.node_session("b", 1).unwrap();
        let lease = admission_with(&base, &authority)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), lease.lost().cancelled())
            .await
            .unwrap();
        assert_eq!(authority.held(), 0);
    }

    #[tokio::test]
    async fn the_machine_ask_names_its_machine() {
        let (base, state) = node((201, GRANT.into())).await;
        let lease = admission(&base)
            .enter(
                Ask::Machine {
                    role: "smart",
                    machine: "m1",
                    model: "x",
                },
                &Cancel::new(),
            )
            .await
            .unwrap();
        lease.release().await;
        let ask: Value = serde_json::from_str(&state.asks.lock().unwrap()[0]).unwrap();
        assert_eq!(ask, json!({"role": "smart", "machine": "m1", "model": "x"}));
    }

    #[tokio::test]
    async fn a_cancel_while_waiting_ends_the_wait_and_gives_the_ticket_back() {
        let (base, state) = node_with((201, GRANT.into()), true).await;
        let authority = registered();
        let cancel = Cancel::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            settle().await;
            trigger.cancel();
        });
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            admission_with(&base, &authority).enter(Ask::Engine { model: "m" }, &cancel),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert_eq!(refused.status, 499);
        assert_eq!(authority.held(), 0);
        settle().await;
        assert_eq!(
            deletes(&state),
            1,
            "a latch whose answer was lost is released by ticket"
        );
    }

    #[tokio::test]
    async fn aborting_the_enter_future_gives_the_ticket_back() {
        let (base, state) = node_with((201, GRANT.into()), true).await;
        let authority = registered();
        let adm = admission_with(&base, &authority);
        let task = tokio::spawn(async move {
            let _ = adm.enter(Ask::Engine { model: "m" }, &Cancel::new()).await;
        });
        settle().await;
        assert_eq!(authority.held(), 1);
        task.abort();
        let _ = task.await;
        assert_eq!(authority.held(), 0);
        settle().await;
        assert_eq!(deletes(&state), 1);
    }

    // Chat port S8: the gate alone, for a chat call.

    #[tokio::test]
    async fn a_gate_lease_asks_for_its_route_and_carries_its_id() {
        let (base, state) = node((201, r#"{"lease":"{ID}","ttlMs":30000}"#.into())).await;
        for (gate, name) in [(Gate::Chat, "chat"), (Gate::Diary, "diary")] {
            let lease = admission(&base).enter_gate(gate).await.unwrap();
            let id = lease.id().to_owned();
            assert_eq!(id.len(), 32);
            assert!(lease.headers().is_empty());
            lease.release().await;
            let ask = state
                .asks
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|a| a.contains("gate"))
                .cloned()
                .unwrap();
            assert_eq!(ask, format!(r#"{{"gate":"{name}"}}"#));
            let seen = state.seen.lock().unwrap().clone();
            let put = seen
                .iter()
                .find(|l| l.starts_with(&format!("PUT {PATH}/{id} T ")))
                .expect("a fenced PUT");
            assert!(!put.ends_with(" - -"), "{put}");
            assert!(
                seen.iter()
                    .any(|l| l.starts_with(&format!("DELETE {PATH}/{id} T "))),
                "{seen:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_gate_refusal_keeps_nodes_words_and_anything_else_is_no_answer() {
        let (base, _s) = node((503, r#"{"error":"Chat is paused while noevia calibrates m.","code":"inferenceMaintenance"}"#.into())).await;
        let refused = admission(&base).enter_gate(Gate::Chat).await.err().unwrap();
        assert_eq!(
            refused,
            GateRefusal::Refused(Refused {
                status: 503,
                message: "Chat is paused while noevia calibrates m.".into(),
                code: Some("inferenceMaintenance".into()),
                retry_after: None,
            })
        );
        // Node's words whole, however long (index.cjs keeps err.message).
        let long = "x".repeat(700);
        let (base, _s) = node((
            503,
            format!(r#"{{"error":"{long}","code":"inferenceMaintenance"}}"#),
        ))
        .await;
        match admission(&base).enter_gate(Gate::Chat).await.err().unwrap() {
            GateRefusal::Refused(r) => assert_eq!(r.message, long),
            other => panic!("{other:?}"),
        }
        for grant in [
            (404, r#"{"error":"Not found"}"#.to_owned()),
            (400, r#"{"error":"A model is required."}"#.to_owned()),
            (503, "{}".to_owned()),
            (201, r#"{"lease":"short","ttlMs":1}"#.to_owned()),
            (500, "x".to_owned()),
        ] {
            let (base, _s) = node(grant).await;
            assert_eq!(
                admission(&base).enter_gate(Gate::Chat).await.err(),
                Some(GateRefusal::NoAnswer)
            );
        }
        assert_eq!(
            admission("http://127.0.0.1:9")
                .enter_gate(Gate::Chat)
                .await
                .err(),
            Some(GateRefusal::NoAnswer)
        );
        let no_token = Arc::new(Admission::new("http://127.0.0.1:9", None).unwrap());
        assert_eq!(
            no_token.enter_gate(Gate::Diary).await.err(),
            Some(GateRefusal::NoAnswer)
        );
    }

    #[tokio::test]
    async fn a_caller_that_leaves_during_the_wait_ends_the_ask_at_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = s.read(&mut buf).await;
            // Node waits for its gate: it answers nothing until the asker goes away.
            let n = s.read(&mut buf).await.unwrap_or(0);
            assert_eq!(n, 0, "the asker closed its socket");
            let _ = closed_tx.send(());
        });
        let a = admission(&base);
        let asking = tokio::spawn(async move { a.enter_gate(Gate::Chat).await.is_ok() });
        settle().await;
        asking.abort();
        tokio::time::timeout(Duration::from_secs(1), closed_rx)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_grant_already_on_its_way_when_the_caller_leaves_is_given_back() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let deleted = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = Arc::clone(&deleted);
        let (headers_tx, headers_rx) = tokio::sync::oneshot::channel::<()>();
        let headers_tx = Arc::new(Mutex::new(Some(headers_tx)));
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let log = Arc::clone(&log);
                let headers_tx = Arc::clone(&headers_tx);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let first = String::from_utf8_lossy(&buf[..n])
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_owned();
                    if first.starts_with("DELETE") {
                        log.lock().unwrap().push(first);
                        let _ = s
                            .write_all(
                                b"HTTP/1.1 204 X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                            )
                            .await;
                        return;
                    }
                    let body = r#"{"lease":"0123456789abcdef0123456789abcdef","ttlMs":30000}"#;
                    let head = format!(
                        "HTTP/1.1 201 X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.flush().await;
                    if let Some(tx) = headers_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let _ = s.write_all(body.as_bytes()).await;
                });
            }
        });
        let a = admission(&base);
        let asking = tokio::spawn(async move { a.enter_gate(Gate::Chat).await.is_ok() });
        headers_rx.await.unwrap();
        settle().await;
        asking.abort();
        tokio::time::sleep(Duration::from_millis(500)).await;
        // The ticket this side issued is what is given back (Node's reply does not name another).
        let deleted = deleted.lock().unwrap().clone();
        assert_eq!(deleted.len(), 1, "{deleted:?}");
        assert!(
            deleted[0].starts_with("DELETE /internal/inference-admission/lease/")
                && deleted[0].ends_with(" HTTP/1.1"),
            "{deleted:?}"
        );
    }

    // S8 re-review P2-4: an admission answer is read to at most MAX_ANSWER_BYTES.

    /// A Node that answers every ask with `status` and `body`, chunked when `chunked`.
    async fn raw_node(status: u16, body: String, chunked: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let head = if chunked {
                        format!("HTTP/1.1 {status} X\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n")
                    } else {
                        format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len())
                    };
                    let _ = s.write_all(head.as_bytes()).await;
                    if chunked {
                        for piece in body.as_bytes().chunks(8192) {
                            let _ = s
                                .write_all(format!("{:x}\r\n", piece.len()).as_bytes())
                                .await;
                            let _ = s.write_all(piece).await;
                            let _ = s.write_all(b"\r\n").await;
                        }
                        let _ = s.write_all(b"0\r\n\r\n").await;
                    } else {
                        let _ = s.write_all(body.as_bytes()).await;
                    }
                });
            }
        });
        base
    }

    #[tokio::test]
    async fn bounded_answers_fail_closed_for_grants_but_preserve_refusals() {
        let pad = "p".repeat(MAX_ANSWER_BYTES);
        let big = format!(
            r#"{{"lease":"0123456789abcdef0123456789abcdef","ttlMs":30000,"pad":"{pad}"}}"#
        );
        for chunked in [false, true] {
            let base = raw_node(201, big.clone(), chunked).await;
            assert_eq!(
                admission(&base).enter_gate(Gate::Chat).await.err(),
                Some(GateRefusal::NoAnswer),
                "chunked {chunked}"
            );
            let refused = admission(&base)
                .enter(Ask::Engine { model: "m" }, &Cancel::new())
                .await
                .err()
                .unwrap();
            assert_eq!(refused, Refused::unavailable(), "chunked {chunked}");
        }
        // Exactly at the cap, the words (including a final multibyte character) remain whole.
        let overhead = r#"{"error":"","code":"inferenceMaintenance"}"#.len();
        let long = format!("{}🧪", "w".repeat(MAX_ANSWER_BYTES - overhead - "🧪".len()));
        let body = format!(r#"{{"error":"{long}","code":"inferenceMaintenance"}}"#);
        assert_eq!(body.len(), MAX_ANSWER_BYTES);
        for chunked in [false, true] {
            let base = raw_node(503, body.clone(), chunked).await;
            match admission(&base).enter_gate(Gate::Chat).await.err().unwrap() {
                GateRefusal::Refused(r) => assert_eq!(r.message, long),
                other => panic!("{other:?}"),
            }
            let refused = admission(&base)
                .enter(Ask::Engine { model: "m" }, &Cancel::new())
                .await
                .err()
                .unwrap();
            assert_eq!(refused.message, long);
            let over = raw_node(503, format!("{body} "), chunked).await;
            assert_eq!(
                admission(&over).enter_gate(Gate::Chat).await.err(),
                Some(GateRefusal::Refused(Refused {
                    status: 503,
                    message: MAINTENANCE_MESSAGE.into(),
                    code: None,
                    retry_after: None,
                })),
                "one byte over, chunked {chunked}"
            );
        }
    }
}
