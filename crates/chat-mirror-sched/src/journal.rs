//! The chat change-journal consumer (core chat-change-journal.cjs `createChangeJournalReader`),
//! now reading the journal in-process instead of through the store's HTTP op.
//!
//! For each new journal entry the chat's Diary mirror is scheduled. The chat's brain (a model
//! call) is scheduled only for a history save: an entry with `op: "write"` and `origin:
//! "history"` read in order. Imports, interrupted-turn appends, removals, entries without an
//! `origin` (an older store) and every rescan schedule the mirror only (owner decision: no brain
//! builds on import or restart).
//!
//! - The cursor is kept per user, in memory. A source answers entries, the oldest and latest
//!   sequence numbers kept, whether more follow, and the start id of the writer.
//! - Rescan: this process has not read the user's journal yet (it restarted, so debounced
//!   nudges were lost), the start id changed, the cursor fell behind a rotation (`after <
//!   oldest - 1`) or is past the end (the journal was reset). A rescan schedules every chat
//!   still in the journal once, then asks for one full mirror scan. Both schedulers are
//!   idempotent, so this is safe.
//! - Which users are read: those seen within `active` (a request start here, or the front's
//!   activity record merged on each tick), on a short timer. A user who goes quiet is caught up
//!   when they come back: the journal keeps the entries, and a rotation in between is detected
//!   and rescanned. The timer reads a user only when the journal file's stamp changed since
//!   the last good read, so an idle user costs one stat per tick.
//! - Dedupe: an entry whose chat and revision were already scheduled is skipped.
//! - Move events carry a durable identity: a notice is acted on once per event id, and a brain
//!   is queued only for a live, fresh, validly-identified one from this start; replays, recovery
//!   deliveries and everything seen during a rescan schedule the mirror only.
//! - Failures are logged once per run of failures and retried on the next tick. An account the
//!   source reports gone is forgotten. Nothing throws into a caller.
use crate::timer::lock;
use crate::{BoxFut, Log};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::task::{JoinHandle, JoinSet};

pub const INTERVAL: Duration = Duration::from_millis(2000);
pub const ACTIVE: Duration = Duration::from_secs(15 * 60);
/// The most reads in one poll (1,000 entries each) before the next tick goes on.
pub const MAX_PAGES: usize = 20;
pub const RECENT_CAP: usize = 5000;

/// One read of a user's journal.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalRead {
    pub entries: Vec<Value>,
    pub oldest_seq: Option<u64>,
    pub latest_seq: Option<u64>,
    pub more: bool,
    pub start: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The account no longer exists: stop reading that user.
    Gone,
    Failed(String),
}

pub trait JournalSource: Send + Sync + 'static {
    fn read(&self, user: &str, after: u64) -> BoxFut<Result<JournalRead, ReadError>>;
    /// The journal file's stamp from one stat (`None`: unknown, so read).
    fn stamp(&self, user: &str) -> Option<String>;
}

/// What a changed chat sets off, beyond its mirror note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Build the chat's brain too.
    pub brain: bool,
    /// The chat whose brain is built, when it is not the one named (a raw metadata id).
    pub brain_chat: Option<String>,
}

pub trait ChangeSink: Send + Sync + 'static {
    /// Schedule the chat's mirror note, and its brain when `change.brain`.
    fn changed(&self, user: &str, chat: &str, change: &Change);
    /// A rescan finished: schedule one full mirror scan for the user.
    fn rescanned(&self, user: &str);
}

/// `{ user: epoch ms of their latest request }` as the front records it, or `None` while it
/// cannot be read.
pub type Activity = Arc<dyn Fn() -> Option<HashMap<String, f64>> + Send + Sync>;
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0)
    })
}

struct Mark {
    seq: u64,
    ids: HashSet<String>,
    full: bool,
}

struct Cursor {
    seq: u64,
    start: String,
}

#[derive(Default)]
struct Recent {
    by_key: HashMap<(String, String), (u64, Option<String>)>,
    order: BTreeMap<u64, (String, String)>,
    counter: u64,
}

impl Recent {
    fn delete(&mut self, key: &(String, String)) {
        if let Some((n, _)) = self.by_key.remove(key) {
            self.order.remove(&n);
        }
    }
    fn insert(&mut self, key: (String, String), revision: Option<String>) {
        self.delete(&key);
        self.counter += 1;
        self.order.insert(self.counter, key.clone());
        self.by_key.insert(key, (self.counter, revision));
        if self.by_key.len() > RECENT_CAP {
            if let Some((_, oldest)) = self.order.pop_first() {
                self.by_key.remove(&oldest);
            }
        }
    }
    fn same(&self, key: &(String, String), revision: &str) -> bool {
        matches!(self.by_key.get(key), Some((_, Some(r))) if r == revision)
    }
}

#[derive(Default)]
struct State {
    moves: HashMap<String, Mark>,
    cursors: HashMap<String, Cursor>,
    seen: HashMap<String, f64>,
    running: HashSet<String>,
    again: HashSet<String>,
    failing: HashSet<String>,
    stamps: HashMap<String, String>,
    recent: Recent,
    merged: HashMap<String, f64>,
}

struct Inner {
    source: Arc<dyn JournalSource>,
    sink: Arc<dyn ChangeSink>,
    log: Arc<dyn Log>,
    activity: Option<Activity>,
    now: Clock,
    active: Duration,
    state: Mutex<State>,
}

pub struct Consumer {
    inner: Arc<Inner>,
    interval: Duration,
    timer: Mutex<Option<JoinHandle<()>>>,
}

type Emit = (String, Change);

fn safe_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// `grants.decimal`: a canonical decimal string that fits an unsigned 64-bit number.
fn decimal(v: &Value) -> bool {
    let Some(s) = v.as_str() else {
        return false;
    };
    let canonical = s == "0" || (!s.starts_with('0') && !s.is_empty());
    canonical && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit()) && s.parse::<u64>().is_ok()
}

fn event_id(v: &Value) -> Option<&str> {
    v.as_str()
        .filter(|s| s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

fn seq_of(e: &Value) -> u64 {
    e.get("seq").and_then(Value::as_u64).unwrap_or(0)
}

fn str_of<'a>(e: &'a Value, key: &str) -> Option<&'a str> {
    e.get(key).and_then(Value::as_str)
}

/// A history POST's save: the only entry that schedules the chat's brain.
pub fn is_history_save(e: &Value) -> bool {
    str_of(e, "op") == Some("write") && str_of(e, "origin") == Some("history")
}

fn needs_rescan(cursor: Option<&Cursor>, body: &JournalRead) -> bool {
    let Some(cursor) = cursor else {
        return true;
    };
    if body.start != cursor.start {
        return true;
    }
    if let Some(oldest) = body.oldest_seq {
        if cursor.seq < oldest.saturating_sub(1) {
            return true;
        }
    }
    match body.latest_seq {
        None => cursor.seq > 0,
        Some(latest) => cursor.seq > latest,
    }
}

impl Inner {
    /// Schedules a chat unless its revision was already scheduled. Called with the state locked.
    fn schedule(
        st: &mut State,
        user: &str,
        chat: Option<&str>,
        revision: Option<&str>,
        brain: bool,
        out: &mut Vec<Emit>,
    ) {
        let Some(chat) = chat.filter(|c| !c.is_empty()) else {
            return;
        };
        let key = (user.to_string(), chat.to_string());
        if let Some(rev) = revision {
            if st.recent.same(&key, rev) {
                return;
            }
        }
        st.recent.insert(key, revision.map(str::to_string));
        out.push((
            chat.to_string(),
            Change {
                brain,
                brain_chat: None,
            },
        ));
    }

    fn move_notice(
        st: &mut State,
        user: &str,
        entry: &Value,
        start: &str,
        live: bool,
        out: &mut Vec<Emit>,
    ) {
        let mark = st.moves.entry(user.to_string()).or_insert_with(|| Mark {
            seq: 0,
            ids: HashSet::new(),
            full: false,
        });
        let chat = str_of(entry, "chatId");
        let raw = str_of(entry, "rawChatId");
        let id = entry.get("eventId").and_then(event_id);
        let valid = id.is_some()
            && entry.get("generation").is_some_and(decimal)
            && chat.is_some_and(|c| !c.is_empty() && safe_id(c) == c)
            && raw.is_some_and(|r| Some(safe_id(r).as_str()) == chat);
        let known = valid && id.is_some_and(|i| mark.ids.contains(i));
        let eseq = entry.get("seq").and_then(Value::as_u64);
        let fresh = eseq.is_some_and(|s| s > mark.seq);
        let brain = valid
            && !known
            && fresh
            && !mark.full
            && live
            && str_of(entry, "delivery") == Some("live")
            && str_of(entry, "start") == Some(start);
        // Mark BEFORE any callback: partially delivered scheduling never starts a second brain.
        mark.seq = mark.seq.max(eseq.unwrap_or(0));
        if let (true, false, Some(i)) = (valid, known, id) {
            if mark.ids.len() >= RECENT_CAP {
                mark.full = true; // no evicted replay may become live
            } else {
                mark.ids.insert(i.to_string());
            }
        }
        if !live {
            return;
        }
        if let Some(chat) = chat {
            out.push((
                chat.to_string(),
                Change {
                    brain,
                    brain_chat: if valid { raw.map(str::to_string) } else { None },
                },
            ));
        }
    }

    fn emit(&self, user: &str, out: Vec<Emit>) {
        for (chat, change) in out {
            self.sink.changed(user, &chat, &change);
        }
    }

    /// Every chat still in the journal, once each; the cursor moves to the end.
    async fn rescan(&self, user: &str, first: JournalRead) -> Result<(), ReadError> {
        let start = first.start.clone();
        let mut body = first;
        let mut after = 0u64;
        let mut pages = 0usize;
        let mut chats: Vec<(String, Option<String>)> = Vec::new();
        let mut moves_seen: Vec<Value> = Vec::new();
        loop {
            for e in &body.entries {
                if str_of(e, "op") == Some("move") {
                    moves_seen.push(e.clone());
                }
                if let Some(chat) = str_of(e, "chatId") {
                    chats.retain(|(c, _)| c != chat);
                    chats.push((chat.to_string(), str_of(e, "revision").map(str::to_string)));
                }
                after = after.max(seq_of(e));
            }
            pages += 1;
            if !body.more || pages >= MAX_PAGES {
                break;
            }
            body = self.source.read(user, after).await?;
        }
        let mut out = Vec::new();
        {
            let mut st = lock(&self.state);
            for e in &moves_seen {
                Self::move_notice(&mut st, user, e, &start, false, &mut out);
            }
            // A rescan schedules even a revision already scheduled: what was pending may have been lost.
            for (chat, revision) in &chats {
                st.recent.delete(&(user.to_string(), chat.clone()));
                Self::schedule(
                    &mut st,
                    user,
                    Some(chat.as_str()),
                    revision.as_deref(),
                    false,
                    &mut out,
                );
            }
            if let Some(mark) = st.moves.get_mut(user) {
                mark.seq = after;
            }
            st.cursors
                .insert(user.to_string(), Cursor { seq: after, start });
        }
        self.emit(user, out);
        // One full mirror scan also covers events already rotated away.
        self.sink.rescanned(user);
        Ok(())
    }

    async fn poll_once(&self, user: &str) -> Result<(), ReadError> {
        let (cursor_seq, cursor_start) = {
            let st = lock(&self.state);
            match st.cursors.get(user) {
                Some(c) => (Some(c.seq), Some(c.start.clone())),
                None => (None, None),
            }
        };
        let mut body = self.source.read(user, cursor_seq.unwrap_or(0)).await?;
        let rescan = {
            let st = lock(&self.state);
            needs_rescan(st.cursors.get(user), &body)
        };
        if rescan {
            // Read from the start of what is kept (the first read may have begun past it).
            if cursor_seq.is_some() {
                body = self.source.read(user, 0).await?;
            }
            return self.rescan(user, body).await;
        }
        let (Some(mut seq), Some(start)) = (cursor_seq, cursor_start) else {
            return Ok(());
        };
        let mut pages = 0usize;
        loop {
            let mut out = Vec::new();
            {
                let mut st = lock(&self.state);
                for e in &body.entries {
                    let s = seq_of(e);
                    if s <= seq {
                        continue;
                    }
                    seq = s;
                    if str_of(e, "op") == Some("move") {
                        Self::move_notice(&mut st, user, e, &start, true, &mut out);
                    } else {
                        Self::schedule(
                            &mut st,
                            user,
                            str_of(e, "chatId"),
                            str_of(e, "revision"),
                            is_history_save(e),
                            &mut out,
                        );
                    }
                }
                st.cursors.insert(
                    user.to_string(),
                    Cursor {
                        seq,
                        start: start.clone(),
                    },
                );
            }
            self.emit(user, out);
            pages += 1;
            if !body.more || pages >= MAX_PAGES {
                break;
            }
            body = self.source.read(user, seq).await?;
        }
        Ok(())
    }

    /// The account is gone: nothing of theirs is kept; a later request start begins afresh.
    fn forget(&self, user: &str) {
        {
            let mut st = lock(&self.state);
            st.moves.remove(user);
            st.cursors.remove(user);
            st.stamps.remove(user);
            st.seen.remove(user);
            st.failing.remove(user);
            st.again.remove(user);
            let keys: Vec<_> = st
                .recent
                .by_key
                .keys()
                .filter(|(u, _)| u == user)
                .cloned()
                .collect();
            for k in keys {
                st.recent.delete(&k);
            }
        }
        self.log
            .line("chat-change-journal: account removed, no longer reading");
    }

    /// Reads `user`'s new entries now: one poll at a time per user; asked again meanwhile, it
    /// reads once more after (a save landing mid-read is not missed). Never fails.
    async fn poll(self: &Arc<Self>, user: &str) {
        if user.is_empty() {
            return;
        }
        {
            let mut st = lock(&self.state);
            if st.running.contains(user) {
                st.again.insert(user.to_string());
                return;
            }
            st.running.insert(user.to_string());
        }
        let mut guard = Running {
            inner: Arc::clone(self),
            user: user.to_string(),
            done: false,
        };
        loop {
            match self.poll_once(user).await {
                Ok(()) => {
                    let again = lock(&self.state).failing.remove(user);
                    if again {
                        self.log.line("chat-change-journal: reading again");
                    }
                }
                Err(ReadError::Gone) => self.forget(user),
                Err(ReadError::Failed(why)) => {
                    let first = {
                        let mut st = lock(&self.state);
                        st.stamps.remove(user); // the next tick reads again whatever the stamp says
                        st.failing.insert(user.to_string())
                    };
                    if first {
                        self.log.line(&format!(
                            "chat-change-journal: read failed, retrying: {why}"
                        ));
                    }
                }
            }
            let mut st = lock(&self.state);
            if st.again.remove(user) {
                continue;
            }
            st.running.remove(user);
            guard.done = true;
            return;
        }
    }

    /// Whether a timer read of `user` can be skipped: nothing changed since the last good read.
    fn unchanged(&self, st: &mut State, user: &str) -> bool {
        let stamp = self.source.stamp(user);
        let skip = stamp.as_deref().is_some_and(|s| {
            st.cursors.contains_key(user)
                && st.stamps.get(user).map(String::as_str) == Some(s)
                && !st.failing.contains(user)
        });
        if skip {
            return true;
        }
        match stamp {
            Some(s) => {
                st.stamps.insert(user.to_string(), s);
            }
            None => {
                st.stamps.remove(user);
            }
        }
        false
    }

    /// The front's record of authenticated requests: a time newer than the one last merged for
    /// that user is a request start of theirs. Never fails.
    fn absorb(&self) {
        let Some(doc) = self.activity.as_ref().and_then(|a| a()) else {
            return;
        };
        let mut st = lock(&self.state);
        st.merged.retain(|u, _| doc.contains_key(u));
        for (user, at) in doc {
            if user.is_empty() || !at.is_finite() {
                continue;
            }
            if st.merged.get(&user).is_some_and(|m| at <= *m) {
                continue;
            }
            st.merged.insert(user.clone(), at);
            if st.seen.get(&user).is_none_or(|s| *s < at) {
                st.seen.insert(user, at);
            }
        }
    }

    /// The timer pass (see [`Consumer::tick`]).
    async fn tick(self: &Arc<Self>) {
        self.absorb();
        let cutoff = (self.now)() - self.active.as_secs_f64() * 1000.0;
        let mut users = Vec::new();
        {
            let mut st = lock(&self.state);
            let seen: Vec<(String, f64)> = st.seen.iter().map(|(u, a)| (u.clone(), *a)).collect();
            for (user, at) in seen {
                if at < cutoff {
                    st.seen.remove(&user);
                    continue;
                }
                if self.unchanged(&mut st, &user) {
                    continue;
                }
                users.push(user);
            }
        }
        let mut polls = JoinSet::new();
        for user in users {
            let inner = Arc::clone(self);
            polls.spawn(async move { inner.poll(&user).await });
        }
        while polls.join_next().await.is_some() {}
    }
}

/// Removes the user from `running` however the poll ends (a panic included).
struct Running {
    inner: Arc<Inner>,
    user: String,
    done: bool,
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut st = lock(&self.inner.state);
        st.running.remove(&self.user);
        st.again.remove(&self.user);
    }
}

impl Consumer {
    pub fn new(
        source: Arc<dyn JournalSource>,
        sink: Arc<dyn ChangeSink>,
        log: Arc<dyn Log>,
        activity: Option<Activity>,
        now: Clock,
        interval: Duration,
        active: Duration,
    ) -> Self {
        Consumer {
            inner: Arc::new(Inner {
                source,
                sink,
                log,
                activity,
                now,
                active,
                state: Mutex::new(State::default()),
            }),
            interval,
            timer: Mutex::new(None),
        }
    }

    /// A request from `user` started: they are active, and their journal is read now unless its
    /// stamp shows nothing changed since the last good read (a stat, not a read).
    pub fn touch(&self, user: &str) {
        if user.is_empty() {
            return;
        }
        {
            let mut st = lock(&self.inner.state);
            st.seen.insert(user.to_string(), (self.inner.now)());
            if self.inner.unchanged(&mut st, user) {
                return;
            }
        }
        let inner = Arc::clone(&self.inner);
        let who = user.to_string();
        tokio::spawn(async move { inner.poll(&who).await });
    }

    /// Reads `user` now (tests and `touch`). Never fails.
    pub async fn poll(&self, user: &str) {
        self.inner.poll(user).await;
    }

    /// One timer pass: merges the front's activity, then reads every active user whose journal
    /// changed; waits for those reads.
    pub async fn tick(&self) {
        self.inner.tick().await;
    }

    /// Starts the timer. A second call does nothing.
    pub fn start(&self) {
        let mut timer = lock(&self.timer);
        if timer.is_some() {
            return;
        }
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let interval = self.interval;
        *timer = Some(tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                // Not awaited: a slow read never delays the next tick; polls serialise per user.
                tokio::spawn(async move { inner.tick().await });
            }
        }));
    }

    pub fn stop(&self) {
        if let Some(handle) = lock(&self.timer).take() {
            handle.abort();
        }
    }

    pub fn cursor(&self, user: &str) -> Option<u64> {
        lock(&self.inner.state).cursors.get(user).map(|c| c.seq)
    }

    pub fn tracked(&self, user: &str) -> bool {
        let st = lock(&self.inner.state);
        st.seen.contains_key(user)
            || st.cursors.contains_key(user)
            || st.stamps.contains_key(user)
            || st.failing.contains(user)
    }
}

impl Drop for Consumer {
    fn drop(&mut self) {
        self.stop();
    }
}
