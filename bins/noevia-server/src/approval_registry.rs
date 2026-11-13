//! The chat approval registry (port contract 2026-11-04 S9, capability `rust-approval-registry`):
//! the pending write approvals, the routing questions ("Send to cloud / Keep local", #778) and the
//! per-chat "Allow for this chat" grants. They lived in Node's memory (core approvals.cjs, behind
//! the #2282 facts/resolve bridge); they now live here, in the front, beside the public decision
//! route (tool_approval_routes.rs) that answers them. Node's turn asks through the private store
//! (project_store.rs `/v1/approvals/*`).
//!
//! The rules are Node's:
//! - A question waits 5 minutes ([`TIMEOUT`]). Silence is a timeout, never an approval.
//! - A question belongs to one user; nobody else can see or answer it.
//! - A write question takes `approve`, `deny` or `approve_all`. A routing question takes `cloud`
//!   or `local` and a strictly boolean `remember`. Any other choice is refused and the question
//!   stays open.
//! - `approve_all` grants the chat for 1 hour ([`GRANT_TTL`]), only in the scope it was given in
//!   and only for the permission binding (canonical chat id, tagged holder, generation, container
//!   start) the question carried. Without a chat, a scope or a valid binding it approves this one
//!   call only.
//! - A grant is honoured only while the chat's live grant context, read from the store, is still
//!   exactly the one it was given for. A move bumps the generation, a delete tombstones the chat and
//!   a restart changes the start id, so each of them revokes the grant at the authority. Moves and
//!   deletes through the public routes also drop it at once ([`Registry::revoke`]).
//! - Node waits with a long-poll ([`Registry::wait`]). When Node drops that request (the person
//!   left, #679) the question is gone: an answer arriving later finds nothing, and nothing is
//!   recorded as declined.
//!
//! What survives a restart: nothing, as before. Everything here is in this process's memory, and
//! the web supervisor runs the front and Node as one unit under one start id (both stop when
//! either does), so a restart re-asks rather than honouring a decision made for a turn that no
//! longer exists. A grant made before a restart could not be honoured after it anyway: its binding
//! names the old start id.
use serde_json::{json, Value};
use server_projects::chat_move::{self, GrantContext, Holder};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::sync::oneshot;
use tokio::time::{Duration, Instant};

/// approvals.cjs `timeoutMs`.
pub const TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// approvals.cjs `ttlMs`.
pub const GRANT_TTL: Duration = Duration::from_secs(60 * 60);
/// Open questions held at once, across every user. Node had no bound; a turn asks one at a time.
pub const MAX_PENDING: usize = 4096;
/// Grants held at once (expired ones are swept first).
pub const MAX_GRANTS: usize = 4096;
/// How long a grant check's ticket may wait for its redeem (the write's dispatch).
pub const TICKET_TTL: Duration = Duration::from_secs(5 * 60);
/// Tickets held at once (expired ones are swept first); a full table issues none (no grant).
pub const MAX_TICKETS: usize = 4096;
/// Outstanding invalidations plus in-flight checks reserve at most this many distinct chats.
/// Admission stops rather than losing a fence when Node cannot acknowledge it.
pub const MAX_INVALIDATIONS: usize = 64;

pub enum GrantUse {
    Ticket(Option<String>),
    Redeemed(bool),
}

pub struct CoveringFence {
    pub through: u64,
    pub chats: Vec<(String, String)>,
}

/// The longest question id accepted (Node's are `ap-<uuid>` or a bare uuid).
pub const MAX_ID: usize = 128;
/// The redeem protocol (capability `registry-executor-v3`). Node names it on every grant check and
/// redeem; a check without it (a Core from before tickets, which would read a ticketed answer as
/// plain approval) is answered "no grant".
pub const PROTOCOL: &str = "registry-executor-v3";
/// Additional local fence expiry sent to Node; never used to acknowledge revocation.
pub const DISPATCH_WINDOW: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Write,
    Route,
}
impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "write" => Some(Self::Write),
            "route" => Some(Self::Route),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Route => "route",
        }
    }
}

/// How a question ended, as Node's turn reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Approve,
    Deny,
    Timeout,
    Cloud { remember: bool },
    Local { remember: bool },
}
impl Outcome {
    pub fn json(&self) -> Value {
        match self {
            Self::Approve => json!({"outcome":"approve"}),
            Self::Deny => json!({"outcome":"deny"}),
            Self::Timeout => json!({"outcome":"timeout"}),
            Self::Cloud { remember } => json!({"outcome":"cloud","remember":remember}),
            Self::Local { remember } => json!({"outcome":"local","remember":remember}),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    Invalid,
    Duplicate,
    Capacity,
    Unavailable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecideError {
    /// No such open question for this user (never asked, answered, expired, stopped).
    Missing,
    /// Not a choice this kind of question takes; it stays open.
    Choice,
    Unavailable,
}

/// A ticket's id: 128 random bits, hex.
fn ticket_id() -> String {
    use rand_core::RngCore;
    let mut bytes = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// chat-grant-context.cjs `safeId`.
pub fn safe_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// chat-grant-context.cjs `binding(user, raw, scope, context)`, as Node sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub user: String,
    pub canonical: String,
    pub scope: String,
    pub context: GrantContext,
}

fn holder(v: &Value) -> Option<Holder> {
    let o = v.as_object()?;
    match (o.len(), o.get("kind").and_then(Value::as_str)) {
        (1, Some("free")) => Some(Holder::Free),
        (1, Some("absent")) => Some(Holder::Absent),
        (2, Some("project")) => match o.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => Some(Holder::Project(id.to_owned())),
            _ => None,
        },
        _ => None,
    }
}

/// chat-grant-context.cjs `decimal`: a canonical decimal no larger than u64.
fn decimal(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 20
        && s.bytes().all(|b| b.is_ascii_digit())
        && (s == "0" || !s.starts_with('0'))
        && s.parse::<u64>().is_ok()
}

fn start_hex(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Binding {
    /// chat-grant-context.cjs `validate` + `binding`: the shape Node built, for `start`. A
    /// tombstoned context is never a binding.
    pub fn parse(v: &Value, start: &str) -> Option<Self> {
        let o = v.as_object()?;
        let user = o.get("user")?.as_str().filter(|s| !s.is_empty())?;
        let scope = o.get("scope")?.as_str().filter(|s| !s.is_empty())?;
        let canonical = o.get("canonicalChatId")?.as_str()?;
        let c = o.get("context")?.as_object()?;
        let context = GrantContext {
            canonical: c.get("canonicalChatId")?.as_str()?.to_owned(),
            holder: holder(c.get("holder")?)?,
            generation: c
                .get("generation")?
                .as_str()
                .filter(|g| decimal(g))?
                .to_owned(),
            start: c
                .get("start")?
                .as_str()
                .filter(|s| start_hex(s))?
                .to_owned(),
            tombstoned: c.get("tombstoned")?.as_bool()?,
        };
        if canonical.is_empty()
            || safe_id(canonical) != canonical
            || context.canonical != canonical
            || context.start != start
            || context.tombstoned
        {
            return None;
        }
        Some(Binding {
            user: user.to_owned(),
            canonical: canonical.to_owned(),
            scope: scope.to_owned(),
            context,
        })
    }
}

/// Where the live grant context is read: the store's data dir and this start's id.
#[derive(Debug, Clone)]
pub struct Live {
    pub data_dir: PathBuf,
    pub start: String,
}
impl Live {
    /// chat_move::grant_context, the authority Node's binding came from (blocking: file locks).
    fn context(&self, user: &str, chat: &str) -> Option<GrantContext> {
        chat_move::grant_context(&self.data_dir, user, chat, &self.start).ok()
    }
}

struct Pending {
    user: String,
    kind: Kind,
    chat: Option<String>,
    scope: Option<String>,
    binding: Option<Binding>,
    /// approvals.cjs `once` (#2446, #2559): approve_all on this question approves the one call and
    /// stores no grant.
    once: bool,
    expires: Instant,
    sender: Option<oneshot::Sender<Outcome>>,
    /// Taken by the one wait; `Some` until Node attaches.
    receiver: Option<oneshot::Receiver<Outcome>>,
}

struct Grant {
    /// This grant's own number: a check that read the store in between accepts only the very grant
    /// it started from (review aadda5e P1-1: a revoke, or a revoke and a new grant, meanwhile).
    serial: u64,
    until: Instant,
    scope: String,
    binding: Binding,
}

/// A grant check's answer, redeemed once just before the write it allows is dispatched.
struct Ticket {
    key: (String, String),
    scope: String,
    binding: Binding,
    serial: u64,
    /// The revocation sequence when it was issued: any later revocation of its chat refuses it.
    issued: u64,
    until: Instant,
}

#[derive(Default)]
struct State {
    pending: HashMap<String, Pending>,
    grants: HashMap<(String, String), Grant>,
    serials: u64,
    tickets: HashMap<String, Ticket>,
    /// The monotonic revocation sequence; every revocation of every chat advances it.
    revocations: u64,
    /// Each chat's revocation epoch: the sequence of its latest revocation, and when (kept for
    /// TICKET_TTL, after which every ticket issued before it has expired anyway; monotonic clock).
    revoked: HashMap<(String, String), (u64, Instant)>,
    /// Context-invalidating checks must not finish until Node confirms its executor epoch.
    /// Retained without a time limit: already-redeemed executors may still be paused, and a failed
    /// acknowledgement must remain retryable even after this check removed the grant.
    invalidated: HashMap<(String, String), u64>,
    checks: HashMap<(String, String), usize>,
    /// A counter would have overflowed: no grant is given or honoured again (fail closed).
    exhausted: bool,
}

impl State {
    /// Advances `key`'s revocation epoch and drops its grant.
    fn revoke(&mut self, key: (String, String)) -> bool {
        let now = Instant::now();
        let removed = self.grants.remove(&key).is_some();
        let Some(seq) = self.revocations.checked_add(1) else {
            // No later epoch can be told apart: refuse every grant and ticket from now on.
            self.exhausted = true;
            self.grants.clear();
            self.tickets.clear();
            return removed;
        };
        self.revocations = seq;
        self.revoked
            .retain(|_, (_, at)| now.saturating_duration_since(*at) < TICKET_TTL);
        self.revoked.insert(key, (seq, now));
        removed
    }
    fn revoked_since(&self, key: &(String, String), issued: u64) -> bool {
        self.revoked.get(key).is_some_and(|(seq, _)| *seq > issued)
    }
}

/// A reserved slot survives the blocking context read. Concurrent checks for the same chat
/// share it; converting it into an invalidation never increases the occupied-key count.
struct CheckReservation<'a> {
    registry: &'a Registry,
    key: (String, String),
}
impl Drop for CheckReservation<'_> {
    fn drop(&mut self) {
        if let Ok(mut s) = self.registry.state.lock() {
            if let Some(count) = s.checks.get_mut(&self.key) {
                *count -= 1;
                if *count == 0 {
                    s.checks.remove(&self.key);
                }
            }
        }
    }
}

/// Tests only: what a grant check runs between its live-context read and its recheck.
#[cfg(test)]
type ContextReadHook = Box<dyn FnOnce(&Registry) + Send>;

pub struct Registry {
    state: Mutex<State>,
    timeout: Duration,
    ttl: Duration,
    /// [`DISPATCH_WINDOW`], shorter in tests.
    window: Duration,
    /// [`TICKET_TTL`], shorter in tests (never longer: the revocation epochs are kept that long).
    ticket_ttl: Duration,
    live: Option<Live>,
    /// Tests only: runs between a grant check's live-context read and its recheck.
    #[cfg(test)]
    after_context_read: Mutex<Option<ContextReadHook>>,
}

/// What [`Registry::open`] takes (approvals.cjs `awaitApproval` / `awaitRouteChoice`).
#[derive(Debug, Clone)]
pub struct Question {
    pub id: String,
    pub user: String,
    pub kind: Kind,
    pub chat: Option<String>,
    pub scope: Option<String>,
    pub binding: Option<Value>,
    /// A tool never approved for more than the call on its card (the page save): approve_all then
    /// approves that call only. Absent on the wire = false.
    pub once: bool,
}

/// Removes the question when Node's wait ends without an answer (its request was dropped).
struct Waiting<'a> {
    registry: &'a Registry,
    id: String,
    armed: bool,
}
impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(mut s) = self.registry.state.lock() {
                s.pending.remove(&self.id);
            }
        }
    }
}

impl Registry {
    pub fn new(live: Option<Live>) -> Self {
        Self::with_times(live, TIMEOUT, GRANT_TTL)
    }

    pub fn with_times(live: Option<Live>, timeout: Duration, ttl: Duration) -> Self {
        Registry {
            state: Mutex::new(State::default()),
            timeout,
            ttl,
            window: DISPATCH_WINDOW,
            ticket_ttl: TICKET_TTL,
            live,
            #[cfg(test)]
            after_context_read: Mutex::new(None),
        }
    }

    /// Tests: a shorter dispatch window.
    #[must_use]
    pub fn with_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    /// The dispatch window Node is told in a redeem's answer.
    pub fn window(&self) -> Duration {
        self.window
    }

    fn sweep(s: &mut State, now: Instant) {
        s.pending.retain(|_, p| p.expires > now);
        s.grants.retain(|_, g| g.until > now);
        s.tickets.retain(|_, t| t.until > now);
    }

    /// Opens a question, before Node shows its card: an answer that comes before Node's wait
    /// attaches is kept for that wait.
    pub fn open(&self, q: Question) -> Result<(), OpenError> {
        if q.user.is_empty() || q.id.is_empty() || q.id.len() > MAX_ID {
            return Err(OpenError::Invalid);
        }
        // A binding Node could not build is no binding: approve_all then approves once.
        let binding = match (&self.live, &q.binding) {
            (Some(live), Some(v)) => Binding::parse(v, &live.start),
            _ => None,
        };
        let now = Instant::now();
        let mut s = self.state.lock().map_err(|_| OpenError::Unavailable)?;
        Self::sweep(&mut s, now);
        if s.pending.contains_key(&q.id) {
            return Err(OpenError::Duplicate);
        }
        if s.pending.len() >= MAX_PENDING {
            return Err(OpenError::Capacity);
        }
        let (sender, receiver) = oneshot::channel();
        s.pending.insert(
            q.id,
            Pending {
                user: q.user,
                kind: q.kind,
                chat: q.chat,
                scope: q.scope.filter(|x| !x.is_empty()),
                binding,
                once: q.once,
                expires: now + self.timeout,
                sender: Some(sender),
                receiver: Some(receiver),
            },
        );
        Ok(())
    }

    /// The question's kind, for its owner only (the public route's lookup before coercion).
    pub fn kind(&self, user: &str, id: &str) -> Result<Option<Kind>, DecideError> {
        let s = self.state.lock().map_err(|_| DecideError::Unavailable)?;
        let now = Instant::now();
        Ok(s.pending
            .get(id)
            .filter(|p| p.user == user && p.expires > now && p.sender.is_some())
            .map(|p| p.kind))
    }

    /// The live context an `approve_all` on this question would be granted for, read outside
    /// the registry's lock (blocking). `None`: no grant can be made.
    pub fn grant_context(&self, user: &str, id: &str) -> Option<GrantContext> {
        let live = self.live.as_ref()?;
        let chat = {
            let s = self.state.lock().ok()?;
            let p = s.pending.get(id).filter(|p| p.user == user)?;
            p.binding.as_ref()?;
            p.chat.clone()?
        };
        live.context(user, &chat)
    }

    /// Answers the question. `live` is [`Registry::grant_context`]'s answer, read before.
    pub fn decide(
        &self,
        user: &str,
        id: &str,
        decision: &str,
        remember: bool,
        live: Option<&GrantContext>,
    ) -> Result<(), DecideError> {
        let now = Instant::now();
        let mut s = self.state.lock().map_err(|_| DecideError::Unavailable)?;
        let p = s
            .pending
            .get(id)
            .filter(|p| p.user == user && p.expires > now && p.sender.is_some())
            .ok_or(DecideError::Missing)?;
        let outcome = match (p.kind, decision) {
            (Kind::Write, "approve" | "approve_all") => Outcome::Approve,
            (Kind::Write, "deny") => Outcome::Deny,
            (Kind::Route, "cloud") => Outcome::Cloud { remember },
            (Kind::Route, "local") => Outcome::Local { remember },
            _ => return Err(DecideError::Choice),
        };
        let grant = if decision == "approve_all" {
            Self::grant_for(p, user, live)
        } else {
            None
        };
        if let Some((key, scope, binding)) = grant {
            Self::sweep(&mut s, now);
            // A full table, or a serial that would overflow, approves this one call only.
            let serial = s.serials.checked_add(1).filter(|_| !s.exhausted);
            if let Some(serial) =
                serial.filter(|_| s.grants.len() < MAX_GRANTS || s.grants.contains_key(&key))
            {
                let until = now + self.ttl;
                s.serials = serial;
                s.grants.insert(
                    key,
                    Grant {
                        serial,
                        until,
                        scope,
                        binding,
                    },
                );
            }
        }
        let attached = match s.pending.get_mut(id) {
            Some(p) => {
                if let Some(tx) = p.sender.take() {
                    let _ = tx.send(outcome);
                }
                p.receiver.is_none()
            }
            None => return Err(DecideError::Missing),
        };
        if attached {
            s.pending.remove(id);
        }
        Ok(())
    }

    /// approvals.cjs's `approve_all` condition: not a `once` question, a chat, a scope, a binding for this user, scope
    /// and chat, a holder that is not absent, and (here) the live context still that binding's.
    fn grant_for(
        p: &Pending,
        user: &str,
        live: Option<&GrantContext>,
    ) -> Option<((String, String), String, Binding)> {
        if p.once {
            return None;
        }
        let chat = safe_id(p.chat.as_deref()?);
        let scope = p.scope.clone()?;
        let b = p.binding.clone()?;
        let live = live?;
        if chat.is_empty()
            || b.user != user
            || b.scope != scope
            || b.canonical != chat
            || b.context.holder == Holder::Absent
            || *live != b.context
        {
            return None;
        }
        Some(((user.to_owned(), chat), scope, b))
    }

    /// Node's wait: the question's outcome, a timeout at its expiry, or (when Node drops this
    /// future, its request gone) the question removed. `Err` when there is no such question for
    /// this user, or it is already being waited for.
    pub async fn wait(&self, user: &str, id: &str) -> Result<Outcome, DecideError> {
        let (receiver, expires) = {
            let mut s = self.state.lock().map_err(|_| DecideError::Unavailable)?;
            let p = s
                .pending
                .get_mut(id)
                .filter(|p| p.user == user)
                .ok_or(DecideError::Missing)?;
            let rx = p.receiver.take().ok_or(DecideError::Missing)?;
            (rx, p.expires)
        };
        let mut guard = Waiting {
            registry: self,
            id: id.to_owned(),
            armed: true,
        };
        let outcome = tokio::select! {
            got = receiver => got.unwrap_or(Outcome::Timeout),
            () = tokio::time::sleep_until(expires) => Outcome::Timeout,
        };
        // Answered or timed out: the entry goes either way (decide removed an attached one).
        drop(
            guard
                .registry
                .state
                .lock()
                .map(|mut s| s.pending.remove(id)),
        );
        guard.armed = false;
        Ok(outcome)
    }

    /// Withdraws a question Node opened but will not wait for (its turn stopped first).
    pub fn cancel(&self, user: &str, id: &str) -> bool {
        let Ok(mut s) = self.state.lock() else {
            return false;
        };
        if s.pending.get(id).is_some_and(|p| p.user == user) {
            s.pending.remove(id);
            return true;
        }
        false
    }

    /// approvals.cjs `chatWideApproved(user, chat, scope, binding)`, then the live context
    /// (blocking): a grant whose chat moved, was deleted or outlived its start is revoked here.
    /// approvals.cjs `chatWideApproved`, as a boolean (tests and diagnostics; issues a ticket).
    #[cfg(test)]
    fn granted(&self, user: &str, chat: &str, scope: &str, binding: Option<&Value>) -> bool {
        self.grant_ticket(user, chat, scope, binding).is_some()
    }

    fn reserve_check(&self, user: &str, chat: &str) -> Result<CheckReservation<'_>, DecideError> {
        let key = (user.to_owned(), safe_id(chat));
        let mut s = self.state.lock().map_err(|_| DecideError::Unavailable)?;
        let occupied = s.invalidated.len()
            + s.checks
                .keys()
                .filter(|k| !s.invalidated.contains_key(*k))
                .count();
        if s.exhausted
            || s.revocations == u64::MAX
            || s.invalidated.len() >= MAX_INVALIDATIONS
            || (!s.invalidated.contains_key(&key)
                && !s.checks.contains_key(&key)
                && occupied >= MAX_INVALIDATIONS)
        {
            return Err(DecideError::Unavailable);
        }
        let count = s.checks.entry(key.clone()).or_default();
        *count = count.checked_add(1).ok_or(DecideError::Unavailable)?;
        Ok(CheckReservation {
            registry: self,
            key,
        })
    }

    /// The only production grant/redeem entry. The slot is held across the live-context read,
    /// preventing concurrent checks from overshooting the hard invalidation limit.
    pub fn check(
        &self,
        user: &str,
        chat: &str,
        scope: &str,
        binding: Option<&Value>,
        ticket: Option<&str>,
    ) -> Result<GrantUse, DecideError> {
        let _slot = self.reserve_check(user, chat)?;
        let used = match ticket {
            Some(t) => GrantUse::Redeemed(self.redeem_raw(user, chat, scope, binding, t)),
            None => GrantUse::Ticket(self.grant_ticket_raw(user, chat, scope, binding)),
        };
        let s = self.state.lock().map_err(|_| DecideError::Unavailable)?;
        if s.exhausted || s.revocations == u64::MAX {
            return Err(DecideError::Unavailable);
        }
        Ok(used)
    }

    #[cfg(test)]
    fn grant_ticket(
        &self,
        user: &str,
        chat: &str,
        scope: &str,
        binding: Option<&Value>,
    ) -> Option<String> {
        match self.check(user, chat, scope, binding, None).ok()? {
            GrantUse::Ticket(t) => t,
            GrantUse::Redeemed(_) => None,
        }
    }
    #[cfg(test)]
    fn redeem(
        &self,
        user: &str,
        chat: &str,
        scope: &str,
        binding: Option<&Value>,
        ticket: &str,
    ) -> bool {
        matches!(
            self.check(user, chat, scope, binding, Some(ticket)),
            Ok(GrantUse::Redeemed(true))
        )
    }

    /// approvals.cjs `chatWideApproved(user, chat, scope, binding)`, then the live context
    /// (blocking): a grant whose chat moved, was deleted or outlived its start is revoked here. A
    /// grant answers with a single-use ticket, which Node redeems through [`Registry::check`] just before
    /// it dispatches the write: the answer itself never authorises anything.
    fn grant_ticket_raw(
        &self,
        user: &str,
        chat: &str,
        scope: &str,
        binding: Option<&Value>,
    ) -> Option<String> {
        self.checked(user, chat, scope, binding, |s, key, serial, b| {
            let now = Instant::now();
            s.tickets.retain(|_, t| t.until > now);
            if s.tickets.len() >= MAX_TICKETS {
                return None;
            }
            let id = ticket_id();
            let issued = s.revocations;
            s.tickets.insert(
                id.clone(),
                Ticket {
                    key: key.clone(),
                    scope: scope.to_owned(),
                    binding: b.clone(),
                    serial,
                    issued,
                    until: now + self.ticket_ttl,
                },
            );
            Some(id)
        })
    }

    /// Redeems a grant check's ticket, once: true only if the same grant is still held and
    /// unexpired, its chat has not been revoked since the ticket was issued (by Node, a move, a
    /// delete, a failed move or anything else), and the live context still matches. The final test
    /// runs under the registry's lock, which every revocation also takes: that lock is the one
    /// point where a write's use of a grant and every revocation of it are ordered.
    fn redeem_raw(
        &self,
        user: &str,
        chat: &str,
        scope: &str,
        binding: Option<&Value>,
        ticket: &str,
    ) -> bool {
        let key = (user.to_owned(), safe_id(chat));
        // Single use: taken out first, whatever the answer.
        let Some(t) = self
            .state
            .lock()
            .ok()
            .and_then(|mut s| s.tickets.remove(ticket))
        else {
            return false;
        };
        if t.key != key || t.scope != scope || Instant::now() >= t.until {
            return false;
        }
        self.checked(user, chat, scope, binding, |s, _key, serial, b| {
            let now = Instant::now();
            // Re-review 689d17a P2-3: the ticket's own expiry, again in the final section.
            if serial != t.serial
                || *b != t.binding
                || s.revoked_since(&t.key, t.issued)
                || now >= t.until
            {
                return None;
            }
            Some(())
        })
        .is_some()
    }

    /// The grant check: the grant read, the live context read outside the lock (the store's file
    /// lock), then under the lock again the very same grant still held and unexpired and the live
    /// context its binding's, and `then` in that same critical section.
    fn checked<T>(
        &self,
        user: &str,
        chat: &str,
        scope: &str,
        binding: Option<&Value>,
        then: impl FnOnce(&mut State, &(String, String), u64, &Binding) -> Option<T>,
    ) -> Option<T> {
        let canonical = safe_id(chat);
        let live = self.live.as_ref()?;
        if user.is_empty() || canonical.is_empty() || scope.is_empty() {
            return None;
        }
        let b = binding.and_then(|v| Binding::parse(v, &live.start))?;
        let key = (user.to_owned(), canonical.clone());
        let (gscope, gbinding, serial) = {
            let mut s = self.state.lock().ok()?;
            if s.exhausted {
                return None;
            }
            let now = Instant::now();
            match s.grants.get(&key) {
                None => return None,
                Some(g) if now >= g.until => {
                    s.grants.remove(&key);
                    return None;
                }
                Some(g) => (g.scope.clone(), g.binding.clone(), g.serial),
            }
        };
        if gscope != scope || b.user != user || b.canonical != canonical || gbinding != b {
            return None;
        }
        let now_live = live.context(user, chat);
        #[cfg(test)]
        if let Some(hook) = self
            .after_context_read
            .lock()
            .ok()
            .and_then(|mut h| h.take())
        {
            hook(self);
        }
        let mut s = self.state.lock().ok()?;
        // The grant read before must still be the one held now, unexpired: a revoke (or a revoke
        // and a new grant) during the read answers no.
        let same = !s.exhausted
            && s.revocations != u64::MAX
            && s.grants
                .get(&key)
                .is_some_and(|g| g.serial == serial && Instant::now() < g.until);
        if same && now_live.as_ref() == Some(&gbinding.context) {
            return then(&mut s, &key, serial, &gbinding);
        }
        // Moved, deleted or restarted: that grant is revoked (a newer one is left alone).
        if same {
            s.revoke(key.clone());
            let generation = s.revocations;
            s.invalidated.insert(key, generation);
        }
        None
    }

    /// Propagates a context invalidation (including an earlier unconfirmed one) to the async
    /// store caller. Poisoned state cannot be confirmed as safe.
    pub fn pending_invalidation(&self, user: &str, chat: &str) -> Result<Option<u64>, DecideError> {
        let s = self.state.lock().map_err(|_| DecideError::Unavailable)?;
        Ok(s.invalidated
            .get(&(user.to_owned(), safe_id(chat)))
            .copied())
    }

    /// A covering fence snapshots every pending chat at one monotonic generation. Each scope
    /// must be acknowledged; one chat's v3 ack cannot cover another chat's executor epoch.
    pub fn covering_fence(&self) -> Result<CoveringFence, DecideError> {
        let s = self.state.lock().map_err(|_| DecideError::Unavailable)?;
        let mut chats: Vec<_> = s.invalidated.keys().cloned().collect();
        chats.sort();
        Ok(CoveringFence {
            through: s.revocations,
            chats,
        })
    }

    /// Retire exactly the scope an authenticated Node acknowledgement covers. A user-wide ack
    /// covers all that user's chats; a chat ack covers that chat only. Newer generations survive.
    pub fn acknowledge_through(
        &self,
        user: &str,
        chat: Option<&str>,
        through: u64,
    ) -> Result<(), DecideError> {
        let canonical = chat.map(safe_id);
        let mut s = self.state.lock().map_err(|_| DecideError::Unavailable)?;
        s.invalidated.retain(|(u, c), generation| {
            u != user || canonical.as_ref().is_some_and(|ch| ch != c) || *generation > through
        });
        Ok(())
    }

    /// approvals.cjs `revokeChatGrant(user, chat)`; also run by the public move and delete, and
    /// around Node's store move and delete. Advances the chat's revocation epoch either way.
    pub fn revoke(&self, user: &str, chat: &str) -> bool {
        let canonical = safe_id(chat);
        if user.is_empty() || canonical.is_empty() {
            return false;
        }
        self.state
            .lock()
            .map(|mut s| s.revoke((user.to_owned(), canonical)))
            .unwrap_or(false)
    }

    /// Revokes every grant of `user` whose chat a project holds (that project's delete).
    pub fn revoke_project(&self, user: &str, project: &str) -> usize {
        let Ok(mut s) = self.state.lock() else {
            return 0;
        };
        let keys: Vec<_> = s
            .grants
            .iter()
            .filter(|(k, g)| {
                k.0 == user
                    && matches!(&g.binding.context.holder, Holder::Project(id) if id == project)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in &keys {
            s.revoke(key.clone());
        }
        keys.len()
    }

    /// Open questions (tests and diagnostics).
    pub fn pending_len(&self) -> usize {
        self.state.lock().map(|s| s.pending.len()).unwrap_or(0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const START: &str = "0123456789abcdef0123456789abcdef";
    const OWNER: &str = "11111111-2222-4333-8444-555555555555";
    const OTHER: &str = "11111111-2222-4333-8444-666666666666";

    fn binding(user: &str, chat: &str, scope: &str, holder: Value, generation: &str) -> Value {
        json!({"user":user,"canonicalChatId":chat,"scope":scope,"context":{
            "canonicalChatId":chat,"holder":holder,"generation":generation,"start":START,"tombstoned":false}})
    }
    fn ctx(chat: &str, holder: Holder, generation: &str) -> GrantContext {
        GrantContext {
            canonical: chat.into(),
            holder,
            generation: generation.into(),
            start: START.into(),
            tombstoned: false,
        }
    }
    /// A registry whose live context is a temp store: a free chat `c1` (and `c2`) for `owner`.
    fn store() -> (tempfile::TempDir, Live) {
        let dir = tempfile::tempdir().unwrap();
        for user in [OWNER, OTHER] {
            let u = server_projects::store::user_dir(dir.path(), user).unwrap();
            std::fs::create_dir_all(&u).unwrap();
            std::fs::write(
                u.join(server_projects::chats::FREE_CHATS_FILE),
                r#"[{"id":"c1","title":"Synthetic"},{"id":"c2","title":"Synthetic two"}]"#,
            )
            .unwrap();
            std::fs::write(
                u.join(server_projects::store::PROJECTS_FILE),
                r#"{"projects":[{"id":"qa-p","chats":[]}]}"#,
            )
            .unwrap();
        }
        let live = Live {
            data_dir: dir.path().to_path_buf(),
            start: START.into(),
        };
        (dir, live)
    }
    fn write_q(
        id: &str,
        user: &str,
        chat: Option<&str>,
        scope: Option<&str>,
        b: Option<Value>,
    ) -> Question {
        Question {
            id: id.into(),
            user: user.into(),
            kind: Kind::Write,
            chat: chat.map(Into::into),
            scope: scope.map(Into::into),
            binding: b,
            once: false,
        }
    }
    fn route_q(id: &str, user: &str) -> Question {
        Question {
            id: id.into(),
            user: user.into(),
            kind: Kind::Route,
            chat: Some("c1".into()),
            scope: None,
            binding: None,
            once: false,
        }
    }

    #[tokio::test]
    async fn owner_kind_and_choice_are_checked_before_an_answer_is_consumed() {
        let r = Registry::new(None);
        r.open(route_q("a", OWNER)).unwrap();
        assert_eq!(r.kind(OTHER, "a"), Ok(None), "another user cannot see it");
        assert_eq!(r.kind(OWNER, "a"), Ok(Some(Kind::Route)));
        assert_eq!(
            r.decide(OTHER, "a", "cloud", false, None),
            Err(DecideError::Missing)
        );
        assert_eq!(
            r.decide(OWNER, "a", "approve_all", false, None),
            Err(DecideError::Choice)
        );
        assert_eq!(
            r.pending_len(),
            1,
            "a refused choice leaves the question open"
        );
        r.decide(OWNER, "a", "cloud", false, None).unwrap();
        assert_eq!(
            r.wait(OWNER, "a").await,
            Ok(Outcome::Cloud { remember: false })
        );
        assert_eq!(
            r.decide(OWNER, "a", "local", false, None),
            Err(DecideError::Missing)
        );
        assert_eq!(r.pending_len(), 0);
        r.open(write_q("w", OWNER, Some("c1"), Some("space:free"), None))
            .unwrap();
        assert_eq!(
            r.decide(OWNER, "w", "cloud", false, None),
            Err(DecideError::Choice)
        );
        assert_eq!(r.open(route_q("w", OWNER)), Err(OpenError::Duplicate));
    }

    #[tokio::test]
    async fn an_answer_before_the_wait_attaches_is_kept_and_remember_is_strict() {
        let r = Registry::new(None);
        r.open(route_q("a", OWNER)).unwrap();
        r.decide(OWNER, "a", "local", true, None).unwrap();
        assert_eq!(
            r.decide(OWNER, "a", "local", true, None),
            Err(DecideError::Missing)
        );
        assert_eq!(
            r.wait(OWNER, "a").await,
            Ok(Outcome::Local { remember: true })
        );
        assert_eq!(r.wait(OWNER, "a").await, Err(DecideError::Missing));
    }

    #[tokio::test]
    async fn a_waiting_turn_receives_the_answer() {
        let r = Arc::new(Registry::new(None));
        r.open(write_q("w", OWNER, None, None, None)).unwrap();
        let waiter = {
            let r = Arc::clone(&r);
            tokio::spawn(async move { r.wait(OWNER, "w").await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            r.wait(OWNER, "w").await,
            Err(DecideError::Missing),
            "one waiter"
        );
        r.decide(OWNER, "w", "deny", false, None).unwrap();
        assert_eq!(waiter.await.unwrap(), Ok(Outcome::Deny));
        assert_eq!(r.pending_len(), 0);
    }

    #[tokio::test]
    async fn silence_times_out_and_a_late_answer_finds_nothing() {
        let r = Registry::with_times(None, Duration::from_millis(30), GRANT_TTL);
        r.open(route_q("x", OWNER)).unwrap();
        assert_eq!(r.wait(OWNER, "x").await, Ok(Outcome::Timeout));
        assert_eq!(
            r.decide(OWNER, "x", "cloud", false, None),
            Err(DecideError::Missing)
        );
        r.open(route_q("y", OWNER)).unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(r.kind(OWNER, "y"), Ok(None), "expired before anyone waited");
        assert_eq!(
            r.decide(OWNER, "y", "cloud", false, None),
            Err(DecideError::Missing)
        );
    }

    /// #679: the turn stopped while the card was open (Node dropped its wait). The question is
    /// gone; a later answer is a 404, never a recorded decline, and makes no grant.
    #[tokio::test]
    async fn a_dropped_wait_stops_the_question_rather_than_declining_it() {
        let (_d, live) = store();
        let r = Arc::new(Registry::new(Some(live)));
        let b = binding(OWNER, "c1", "space:free", json!({"kind":"free"}), "0");
        r.open(write_q(
            "w",
            OWNER,
            Some("c1"),
            Some("space:free"),
            Some(b.clone()),
        ))
        .unwrap();
        let waiter = {
            let r = Arc::clone(&r);
            tokio::spawn(async move { r.wait(OWNER, "w").await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        waiter.abort();
        let _ = waiter.await;
        assert_eq!(r.pending_len(), 0);
        let live = r.grant_context(OWNER, "w");
        assert_eq!(
            r.decide(OWNER, "w", "approve_all", false, live.as_ref()),
            Err(DecideError::Missing)
        );
        assert!(!r.granted(OWNER, "c1", "space:free", Some(&b)));
        // Withdrawn before any wait.
        r.open(write_q("v", OWNER, None, None, None)).unwrap();
        assert!(!r.cancel(OTHER, "v"));
        assert!(r.cancel(OWNER, "v"));
        assert_eq!(
            r.decide(OWNER, "v", "approve", false, None),
            Err(DecideError::Missing)
        );
    }

    #[tokio::test]
    async fn approve_all_grants_only_its_scope_binding_and_live_context() {
        let (dir, live) = store();
        let r = Registry::new(Some(live));
        let b = binding(OWNER, "c1", "space:free", json!({"kind":"free"}), "0");
        r.open(write_q(
            "w",
            OWNER,
            Some("c1"),
            Some("space:free"),
            Some(b.clone()),
        ))
        .unwrap();
        let live = r.grant_context(OWNER, "w");
        assert_eq!(live, Some(ctx("c1", Holder::Free, "0")));
        r.decide(OWNER, "w", "approve_all", false, live.as_ref())
            .unwrap();
        assert_eq!(r.wait(OWNER, "w").await, Ok(Outcome::Approve));
        assert!(r.granted(OWNER, "c1", "space:free", Some(&b)));
        assert!(
            !r.granted(OWNER, "c1", "project:p1", Some(&b)),
            "#917 scope"
        );
        assert!(
            !r.granted(OWNER, "c1", "space:free", None),
            "binding required"
        );
        assert!(!r.granted(
            OTHER,
            "c1",
            "space:free",
            Some(&binding(
                OTHER,
                "c1",
                "space:free",
                json!({"kind":"free"}),
                "0"
            ))
        ));
        assert!(!r.granted(
            OWNER,
            "c2",
            "space:free",
            Some(&binding(
                OWNER,
                "c2",
                "space:free",
                json!({"kind":"free"}),
                "0"
            ))
        ));
        let stale = binding(OWNER, "c1", "space:free", json!({"kind":"free"}), "1");
        assert!(
            !r.granted(OWNER, "c1", "space:free", Some(&stale)),
            "another generation"
        );
        // The chat is moved (its generation bumps): the authority revokes the grant.
        chat_move::move_chat(
            dir.path(),
            OWNER,
            "c1",
            Holder::Project("qa-p".into()),
            None,
            START,
        )
        .unwrap();
        assert!(!r.granted(OWNER, "c1", "space:free", Some(&b)));
        // ... and moving it back does not bring the grant back (#814: a move away and back).
        chat_move::move_chat(dir.path(), OWNER, "c1", Holder::Free, None, START).unwrap();
        assert!(!r.granted(OWNER, "c1", "space:free", Some(&b)));
        // A deleted chat: grant again on c2, then tombstone it.
        let b2 = binding(OWNER, "c2", "space:free", json!({"kind":"free"}), "0");
        r.open(write_q(
            "w2",
            OWNER,
            Some("c2"),
            Some("space:free"),
            Some(b2.clone()),
        ))
        .unwrap();
        let lc = r.grant_context(OWNER, "w2");
        r.decide(OWNER, "w2", "approve_all", false, lc.as_ref())
            .unwrap();
        assert!(r.granted(OWNER, "c2", "space:free", Some(&b2)));
        let u = server_projects::store::user_dir(dir.path(), OWNER).unwrap();
        std::fs::write(u.join(server_projects::chats::TOMBSTONES_FILE), r#"["c2"]"#).unwrap();
        assert!(!r.granted(OWNER, "c2", "space:free", Some(&b2)));
        std::fs::write(u.join(server_projects::chats::TOMBSTONES_FILE), "[]").unwrap();
        assert!(
            !r.granted(OWNER, "c2", "space:free", Some(&b2)),
            "revoked, not suspended"
        );
        // revokeChatGrant.
        r.open(write_q(
            "w3",
            OWNER,
            Some("c2"),
            Some("space:free"),
            Some(b2.clone()),
        ))
        .unwrap();
        let lc = r.grant_context(OWNER, "w3");
        r.decide(OWNER, "w3", "approve_all", false, lc.as_ref())
            .unwrap();
        assert!(r.granted(OWNER, "c2", "space:free", Some(&b2)));
        assert!(r.revoke(OWNER, "c2"));
        assert!(!r.granted(OWNER, "c2", "space:free", Some(&b2)));
    }

    /// #2559: main's #2446 rule, replayed from a fixture recorded from Node's approvals.cjs
    /// (tools/gen-approvals-once-fixture.cjs): approve_all on a `once` question approves that call
    /// and stores no grant; the same answer on an ordinary question stores one.
    #[tokio::test]
    async fn approve_all_on_a_once_question_stores_no_grant() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../contracts/http/approvals-once.v1.json"
        ))
        .unwrap();
        let cases = fixture.get("cases").and_then(Value::as_array).unwrap();
        assert_eq!(cases.len(), 6);
        for (i, case) in cases.iter().enumerate() {
            let (_d, live) = store();
            let r = Registry::new(Some(live));
            let b = binding(OWNER, "c1", "space:free", json!({"kind":"free"}), "0");
            let id = format!("q{i}");
            let mut q = write_q(&id, OWNER, Some("c1"), Some("space:free"), Some(b.clone()));
            q.once = case.get("once").and_then(Value::as_bool).unwrap();
            r.open(q).unwrap();
            let live = r.grant_context(OWNER, &id);
            r.decide(
                OWNER,
                &id,
                case.get("decision").and_then(Value::as_str).unwrap(),
                false,
                live.as_ref(),
            )
            .unwrap();
            let want = match case.get("outcome").and_then(Value::as_str).unwrap() {
                "approve" => Outcome::Approve,
                _ => Outcome::Deny,
            };
            assert_eq!(r.wait(OWNER, &id).await, Ok(want), "{case}");
            assert_eq!(
                r.granted(OWNER, "c1", "space:free", Some(&b)),
                case.get("grant").and_then(Value::as_bool).unwrap(),
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn approve_all_without_chat_scope_or_binding_approves_once() {
        let (_d, live) = store();
        let r = Registry::new(Some(live));
        let b = binding(OWNER, "c1", "space:free", json!({"kind":"free"}), "0");
        for (id, q) in [
            (
                "a",
                write_q("a", OWNER, None, Some("space:free"), Some(b.clone())),
            ),
            ("b", write_q("b", OWNER, Some("c1"), None, Some(b.clone()))),
            (
                "c",
                write_q("c", OWNER, Some("c1"), Some("space:free"), None),
            ),
            (
                "d",
                write_q("d", OWNER, Some("c1"), Some("project:p"), Some(b.clone())),
            ),
            (
                "e",
                write_q(
                    "e",
                    OWNER,
                    Some("c1"),
                    Some("space:free"),
                    Some(binding(
                        OWNER,
                        "c1",
                        "space:free",
                        json!({"kind":"absent"}),
                        "0",
                    )),
                ),
            ),
            (
                "f",
                write_q(
                    "f",
                    OWNER,
                    Some("c1"),
                    Some("space:free"),
                    Some(binding(
                        OTHER,
                        "c1",
                        "space:free",
                        json!({"kind":"free"}),
                        "0",
                    )),
                ),
            ),
        ] {
            r.open(q).unwrap();
            let live = r.grant_context(OWNER, id);
            r.decide(OWNER, id, "approve_all", false, live.as_ref())
                .unwrap();
            assert_eq!(r.wait(OWNER, id).await, Ok(Outcome::Approve));
            assert!(!r.granted(OWNER, "c1", "space:free", Some(&b)), "{id}");
        }
        // A binding whose context is no longer live at the answer makes no grant either.
        r.open(write_q(
            "g",
            OWNER,
            Some("c1"),
            Some("space:free"),
            Some(b.clone()),
        ))
        .unwrap();
        let other = ctx("c1", Holder::Free, "7");
        r.decide(OWNER, "g", "approve_all", false, Some(&other))
            .unwrap();
        assert!(!r.granted(OWNER, "c1", "space:free", Some(&b)));
    }

    #[tokio::test]
    async fn grants_expire_and_a_restart_honours_nothing() {
        let (_d, live) = store();
        let r = Registry::with_times(Some(live.clone()), TIMEOUT, Duration::from_millis(30));
        let b = binding(OWNER, "c1", "space:free", json!({"kind":"free"}), "0");
        r.open(write_q(
            "w",
            OWNER,
            Some("c1"),
            Some("space:free"),
            Some(b.clone()),
        ))
        .unwrap();
        let lc = r.grant_context(OWNER, "w");
        r.decide(OWNER, "w", "approve_all", false, lc.as_ref())
            .unwrap();
        assert!(r.granted(OWNER, "c1", "space:free", Some(&b)));
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            !r.granted(OWNER, "c1", "space:free", Some(&b)),
            "1 h grant, here 30 ms"
        );
        // Risk 8: a restart is a new registry under a new start id. Nothing pending or granted
        // survives, and a binding naming the old start is not even a binding.
        let r = Registry::new(Some(live.clone()));
        r.open(write_q(
            "w",
            OWNER,
            Some("c1"),
            Some("space:free"),
            Some(b.clone()),
        ))
        .unwrap();
        let lc = r.grant_context(OWNER, "w");
        r.decide(OWNER, "w", "approve_all", false, lc.as_ref())
            .unwrap();
        assert!(r.granted(OWNER, "c1", "space:free", Some(&b)));
        let restarted = Registry::new(Some(Live {
            data_dir: live.data_dir.clone(),
            start: "fedcba9876543210fedcba9876543210".into(),
        }));
        assert_eq!(restarted.pending_len(), 0);
        assert_eq!(restarted.kind(OWNER, "w"), Ok(None));
        assert!(!restarted.granted(OWNER, "c1", "space:free", Some(&b)));
        assert!(Binding::parse(&b, "fedcba9876543210fedcba9876543210").is_none());
    }

    /// Review aadda5e P1-1: a revoke while the check reads the live context (outside the lock) wins,
    /// and so does a revoke followed by a fresh grant for the same binding.
    #[tokio::test]
    async fn a_revoke_during_the_context_read_wins() {
        let (_d, live) = store();
        let r = Registry::new(Some(live));
        let b = binding(OWNER, "c1", "space:free", json!({"kind":"free"}), "0");
        let grant = |r: &Registry, id: &str| {
            r.open(write_q(
                id,
                OWNER,
                Some("c1"),
                Some("space:free"),
                Some(b.clone()),
            ))
            .unwrap();
            let lc = r.grant_context(OWNER, id);
            r.decide(OWNER, id, "approve_all", false, lc.as_ref())
                .unwrap();
        };
        grant(&r, "w1");
        assert!(r.granted(OWNER, "c1", "space:free", Some(&b)));
        *r.after_context_read.lock().unwrap() = Some(Box::new(|r: &Registry| {
            assert!(r.revoke(OWNER, "c1"));
        }));
        assert!(
            !r.granted(OWNER, "c1", "space:free", Some(&b)),
            "revoked mid-read"
        );
        assert!(!r.granted(OWNER, "c1", "space:free", Some(&b)));
        grant(&r, "w2");
        let b2 = b.clone();
        *r.after_context_read.lock().unwrap() = Some(Box::new(move |r: &Registry| {
            assert!(r.revoke(OWNER, "c1"));
            r.open(write_q(
                "w3",
                OWNER,
                Some("c1"),
                Some("space:free"),
                Some(b2),
            ))
            .unwrap();
            let lc = r.grant_context(OWNER, "w3");
            r.decide(OWNER, "w3", "approve_all", false, lc.as_ref())
                .unwrap();
        }));
        assert!(
            !r.granted(OWNER, "c1", "space:free", Some(&b)),
            "the check started from the revoked grant, not the new one"
        );
        assert!(
            r.granted(OWNER, "c1", "space:free", Some(&b)),
            "the new grant stands"
        );
    }

    fn granted_with_ticket(r: &Registry, id: &str, b: &Value) -> String {
        r.open(write_q(
            id,
            OWNER,
            Some("c1"),
            Some("space:free"),
            Some(b.clone()),
        ))
        .unwrap();
        let lc = r.grant_context(OWNER, id);
        r.decide(OWNER, id, "approve_all", false, lc.as_ref())
            .unwrap();
        r.grant_ticket(OWNER, "c1", "space:free", Some(b)).unwrap()
    }

    /// Re-review 1ed3a14: a revocation and a redeem are ordered at the registry's lock, in either
    /// order. Revoked first (here during the redeem's context read): refused. Redeemed first: the
    /// registry redeem precedes revocation, but Node must still pass its final executor epoch check.
    #[tokio::test]
    async fn a_revoke_racing_a_redeem_orders_at_the_lock_both_ways() {
        let (_d, live) = store();
        let r = Registry::new(Some(live));
        let b = binding(OWNER, "c1", "space:free", json!({"kind":"free"}), "0");
        let t = granted_with_ticket(&r, "w1", &b);
        *r.after_context_read.lock().unwrap() = Some(Box::new(|r: &Registry| {
            r.revoke(OWNER, "c1");
        }));
        assert!(
            !r.redeem(OWNER, "c1", "space:free", Some(&b), &t),
            "revoke first"
        );
        let t = granted_with_ticket(&r, "w2", &b);
        assert!(
            r.redeem(OWNER, "c1", "space:free", Some(&b), &t),
            "redeem first"
        );
        assert!(r.revoke(OWNER, "c1"));
        assert!(!r.granted(OWNER, "c1", "space:free", Some(&b)));
        // A revocation advances the epoch even with a new grant after it: an older ticket stays
        // refused although the new grant has the same binding.
        let old = granted_with_ticket(&r, "w3", &b);
        r.revoke(OWNER, "c1");
        let fresh = granted_with_ticket(&r, "w4", &b);
        assert!(!r.redeem(OWNER, "c1", "space:free", Some(&b), &old));
        assert!(r.redeem(OWNER, "c1", "space:free", Some(&b), &fresh));
        // A revocation with no grant held at all still refuses a ticket issued before it.
        let t = granted_with_ticket(&r, "w5", &b);
        {
            let mut s = r.state.lock().unwrap();
            s.grants.clear();
        }
        r.revoke(OWNER, "c1");
        granted_with_ticket(&r, "w6", &b);
        assert!(!r.redeem(OWNER, "c1", "space:free", Some(&b), &t));
    }

    /// Re-review 689d17a P2-3: a ticket that expires during the redeem's context read is refused
    /// in the final section.
    #[tokio::test]
    async fn a_ticket_expiring_during_the_redeem_is_refused() {
        let (_d, live) = store();
        let mut r = Registry::new(Some(live));
        r.ticket_ttl = Duration::from_millis(300);
        let b = binding(OWNER, "c1", "space:free", json!({"kind":"free"}), "0");
        let t = granted_with_ticket(&r, "w1", &b);
        *r.after_context_read.lock().unwrap() = Some(Box::new(|_: &Registry| {
            std::thread::sleep(std::time::Duration::from_millis(450));
        }));
        assert!(!r.redeem(OWNER, "c1", "space:free", Some(&b), &t));
        // Control: in time, it redeems.
        let t = granted_with_ticket(&r, "w2", &b);
        assert!(r.redeem(OWNER, "c1", "space:free", Some(&b), &t));
    }

    /// Counters that would overflow fail closed: no grant is given or honoured.
    #[tokio::test]
    async fn counter_overflow_fails_closed() {
        let (_d, live) = store();
        let r = Registry::new(Some(live));
        let b = binding(OWNER, "c1", "space:free", json!({"kind":"free"}), "0");
        r.state.lock().unwrap().serials = u64::MAX;
        r.open(write_q(
            "w1",
            OWNER,
            Some("c1"),
            Some("space:free"),
            Some(b.clone()),
        ))
        .unwrap();
        let lc = r.grant_context(OWNER, "w1");
        r.decide(OWNER, "w1", "approve_all", false, lc.as_ref())
            .unwrap();
        assert!(
            !r.granted(OWNER, "c1", "space:free", Some(&b)),
            "approved once only"
        );
        r.state.lock().unwrap().serials = 0;
        let t = granted_with_ticket(&r, "w2", &b);
        r.state.lock().unwrap().revocations = u64::MAX;
        r.revoke(OWNER, "c2");
        assert!(!r.redeem(OWNER, "c1", "space:free", Some(&b), &t));
        r.open(write_q(
            "w3",
            OWNER,
            Some("c1"),
            Some("space:free"),
            Some(b.clone()),
        ))
        .unwrap();
        let lc = r.grant_context(OWNER, "w3");
        r.decide(OWNER, "w3", "approve_all", false, lc.as_ref())
            .unwrap();
        assert!(!r.granted(OWNER, "c1", "space:free", Some(&b)), "exhausted");
    }

    #[tokio::test]
    async fn a_ticket_is_single_use_and_bound_to_its_chat_scope_and_binding() {
        let (_d, live) = store();
        let r = Registry::new(Some(live));
        let b = binding(OWNER, "c1", "space:free", json!({"kind":"free"}), "0");
        let t = granted_with_ticket(&r, "w1", &b);
        assert!(r.redeem(OWNER, "c1", "space:free", Some(&b), &t));
        assert!(
            !r.redeem(OWNER, "c1", "space:free", Some(&b), &t),
            "replay refused"
        );
        for (user, chat, scope) in [
            (OTHER, "c1", "space:free"),
            (OWNER, "c2", "space:free"),
            (OWNER, "c1", "project:qa-p"),
        ] {
            let t = r.grant_ticket(OWNER, "c1", "space:free", Some(&b)).unwrap();
            assert!(
                !r.redeem(user, chat, scope, Some(&b), &t),
                "{user} {chat} {scope}"
            );
            assert!(
                !r.redeem(OWNER, "c1", "space:free", Some(&b), &t),
                "consumed by the refusal too"
            );
        }
        assert!(!r.redeem(OWNER, "c1", "space:free", Some(&b), "no-such-ticket"));
        let t = r.grant_ticket(OWNER, "c1", "space:free", Some(&b)).unwrap();
        let stale = binding(OWNER, "c1", "space:free", json!({"kind":"free"}), "3");
        assert!(!r.redeem(OWNER, "c1", "space:free", Some(&stale), &t));
    }

    #[test]
    fn binding_shape_is_node_s() {
        let ok = binding(
            OWNER,
            "c1",
            "project:p",
            json!({"kind":"project","id":"p"}),
            "18446744073709551615",
        );
        assert!(Binding::parse(&ok, START).is_some());
        for bad in [
            binding(OWNER, "c1", "s", json!({"kind":"project"}), "0"),
            binding(OWNER, "c1", "s", json!({"kind":"free","id":"x"}), "0"),
            binding(OWNER, "c1", "s", json!({"kind":"free"}), "01"),
            binding(
                OWNER,
                "c1",
                "s",
                json!({"kind":"free"}),
                "18446744073709551616",
            ),
            binding(OWNER, "c!1", "s", json!({"kind":"free"}), "0"),
            binding("", "c1", "s", json!({"kind":"free"}), "0"),
            binding(OWNER, "c1", "", json!({"kind":"free"}), "0"),
            json!({"user":OWNER,"canonicalChatId":"c1","scope":"s","context":{"canonicalChatId":"c1",
                "holder":{"kind":"free"},"generation":"0","start":START,"tombstoned":true}}),
            json!({"user":OWNER,"canonicalChatId":"c1","scope":"s","context":{"canonicalChatId":"c2",
                "holder":{"kind":"free"},"generation":"0","start":START,"tombstoned":false}}),
        ] {
            assert!(Binding::parse(&bad, START).is_none(), "{bad}");
        }
    }

    #[test]
    fn ids_and_capacity_are_bounded() {
        let r = Registry::new(None);
        assert_eq!(r.open(route_q("", OWNER)), Err(OpenError::Invalid));
        assert_eq!(
            r.open(route_q(&"x".repeat(MAX_ID + 1), OWNER)),
            Err(OpenError::Invalid)
        );
        assert_eq!(r.open(route_q("a", "")), Err(OpenError::Invalid));
        for i in 0..MAX_PENDING {
            r.open(route_q(&format!("q{i}"), OWNER)).unwrap();
        }
        assert_eq!(r.open(route_q("one-more", OWNER)), Err(OpenError::Capacity));
    }
    #[tokio::test]
    async fn old_context_confirmation_cannot_clear_a_newer_invalidation() {
        let (dir, live) = store();
        let r = Registry::new(Some(live));
        let mut generations = Vec::new();
        for (id, generation) in [("w-first", "0"), ("w-second", "2")] {
            let b = binding(
                OWNER,
                "c1",
                "space:free",
                json!({"kind":"free"}),
                generation,
            );
            r.open(write_q(
                id,
                OWNER,
                Some("c1"),
                Some("space:free"),
                Some(b.clone()),
            ))
            .unwrap();
            let context = r.grant_context(OWNER, id);
            r.decide(OWNER, id, "approve_all", false, context.as_ref())
                .unwrap();
            chat_move::move_chat(
                dir.path(),
                OWNER,
                "c1",
                Holder::Project("qa-p".into()),
                None,
                START,
            )
            .unwrap();
            assert!(!r.granted(OWNER, "c1", "space:free", Some(&b)));
            generations.push(r.pending_invalidation(OWNER, "c1").unwrap().unwrap());
            chat_move::move_chat(dir.path(), OWNER, "c1", Holder::Free, None, START).unwrap();
        }
        let first = generations.first().copied().unwrap();
        let second = generations.last().copied().unwrap();
        assert!(second > first);
        r.acknowledge_through(OWNER, None, first).unwrap();
        assert_eq!(r.pending_invalidation(OWNER, "c1").unwrap(), Some(second));
        r.acknowledge_through(OWNER, None, second).unwrap();
        assert_eq!(r.pending_invalidation(OWNER, "c1").unwrap(), None);
    }
    #[test]
    fn concurrent_checks_reserve_bounded_marker_capacity_before_reading_context() {
        let r = Registry::new(None);
        let mut slots = Vec::new();
        for i in 0..MAX_INVALIDATIONS {
            slots.push(r.reserve_check(OWNER, &format!("chat-{i}")).unwrap());
        }
        assert!(r.reserve_check(OWNER, "overflow").is_err());
        drop(slots.pop());
        assert!(r.reserve_check(OWNER, "overflow").is_ok());
        drop(slots);
        assert!(r.state.lock().unwrap().checks.is_empty());
    }
    #[test]
    fn a_covering_ack_never_retires_another_user_or_a_newer_generation() {
        let r = Registry::new(None);
        {
            let mut s = r.state.lock().unwrap();
            s.revocations = 3;
            s.invalidated.insert((OWNER.into(), "old".into()), 1);
            s.invalidated.insert((OWNER.into(), "new".into()), 3);
            s.invalidated.insert((OTHER.into(), "other".into()), 2);
        }
        let cover = r.covering_fence().unwrap();
        assert_eq!(cover.through, 3);
        assert_eq!(cover.chats.len(), 3);
        r.acknowledge_through(OWNER, Some("new"), 2).unwrap();
        assert_eq!(r.pending_invalidation(OWNER, "old").unwrap(), Some(1));
        assert_eq!(r.pending_invalidation(OWNER, "new").unwrap(), Some(3));
        r.acknowledge_through(OWNER, None, 2).unwrap();
        assert_eq!(r.pending_invalidation(OWNER, "old").unwrap(), None);
        assert_eq!(r.pending_invalidation(OWNER, "new").unwrap(), Some(3));
        assert_eq!(r.pending_invalidation(OTHER, "other").unwrap(), Some(2));
    }
    #[test]
    fn chat_acknowledgements_canonicalise_aliases_before_retiring_markers() {
        let r = Registry::new(None);
        r.state
            .lock()
            .unwrap()
            .invalidated
            .insert((OWNER.into(), "old".into()), 1);
        r.acknowledge_through(OWNER, Some("o/l.d"), 1).unwrap();
        assert_eq!(r.pending_invalidation(OWNER, "old").unwrap(), None);
    }
    #[test]
    fn saturated_revocation_generation_refuses_new_admission() {
        let r = Registry::new(None);
        r.state.lock().unwrap().revocations = u64::MAX;
        assert!(r.reserve_check(OWNER, "new-chat").is_err());
    }
}
