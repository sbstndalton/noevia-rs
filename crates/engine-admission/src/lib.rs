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
/// Node's own queue limit is under this (`max_wait_ms`), so a queued wait ends with Node's answer.
pub const ENTER_TIMEOUT: Duration = Duration::from_secs(150);

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
}

impl Refused {
    fn unavailable() -> Refused {
        Refused {
            status: 503,
            message: "The model is busy. Try again shortly.".into(),
            code: None,
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
        if self.token.is_none() {
            return Err(Refused::unavailable());
        }
        let ticket = self.authority.issue().ok_or_else(Refused::unavailable)?;
        // From here the ticket is a guard: every early return and every drop gives it back.
        let guard = Arc::new(Held {
            admission: Arc::clone(self),
            ticket,
            released: AtomicBool::new(false),
            confirmed_until: std::sync::Mutex::new(tokio::time::Instant::now()),
        });
        let mut pending = Pending(Some(Arc::clone(&guard)));
        let Some(req) = self.request(reqwest::Method::PUT, &guard.ticket, "") else {
            return Err(Refused::unavailable());
        };
        // Node arms its latch's TTL no earlier than it receives this request, so the grant is proven to
        // hold only until `sent + ttl`: an answer that took longer is not a lease (row H review P1).
        let sent = tokio::time::Instant::now();
        let send = req
            .header("content-type", "application/json")
            .timeout(ENTER_TIMEOUT)
            .body(body.to_string())
            .send();
        let answer = tokio::select! {
            () = cancel.cancelled() => return Err(cancelled()),
            r = send => r,
        };
        let response = answer.map_err(|_| Refused::unavailable())?;
        let status = response.status().as_u16();
        let reading = response.bytes();
        let bytes = tokio::select! {
            () = cancel.cancelled() => return Err(cancelled()),
            b = reading => b.map_err(|_| Refused::unavailable())?,
        };
        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status != 201 {
            let message = parsed.get("error").and_then(Value::as_str);
            return Err(match (status, message) {
                (404 | 410, _) | (_, None) => Refused::unavailable(),
                (_, Some(m)) => Refused {
                    status,
                    message: m.chars().take(500).collect(),
                    code: parsed
                        .get("code")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            });
        }
        let ttl = parsed
            .get("ttlMs")
            .and_then(Value::as_u64)
            .filter(|t| *t > 0)
            .map(Duration::from_millis)
            .ok_or_else(Refused::unavailable)?;
        // Node echoes the ticket it latched; anything else is not this lease.
        if parsed.get("lease").and_then(Value::as_str) != Some(guard.ticket.id.as_str()) {
            return Err(Refused::unavailable());
        }
        // Coverage may have ended while Node answered (a newer Node start registered).
        if !self.authority.covers(&guard.ticket.id) {
            return Err(Refused::unavailable());
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
            return Err(Refused::unavailable());
        }
        guard.confirm(until);
        let inner = pending.0.take().ok_or_else(Refused::unavailable)?;
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
    /// What the holder's engine requests carry (the gate's lease token), as Node gave it.
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
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
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
}
