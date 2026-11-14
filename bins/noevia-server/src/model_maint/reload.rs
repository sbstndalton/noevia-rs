//! `POST /api/models/presets/reload` (core `routes/models.cjs`, `llamacpp-manager.cjs`
//! `reloadPresets` / `routerReload`, `preset-reload.cjs`): ask the llama.cpp router to re-read
//! models.ini, without unloading a model a client is using.
//!
//! The router reads models.ini at start and on `GET /models?reload=1`; a reload keeps every running
//! model whose effective preset is unchanged and unloads every running one whose preset changed or
//! was removed. So a reload with models loaded goes ahead only when all of these hold: this process
//! knows what the router last read (the baseline recorded after the last reload it made), the
//! router's own view of every model's preset is still what it reported then (so it has not re-read
//! the file since), and `preset-reload`'s check finds every loaded model's section, the `[*]`
//! section and the preamble unchanged. Anything unknown fails closed.
//!
//! The baseline lives in `native-preset-reload-<hash>.json` (`{text, view}`, or `{}` for
//! "unknown"), under a lock file (`<file>.lock`, created exclusively; one holding it longer than
//! [`LOCK_STALE`] is taken over) and replaced atomically, the format and lock Core uses too.
//!
//! [`Guard`] is its one in-memory authority (port contract 2026-11-04, RS-1): one thread performs
//! every write in the order asked (a generation each), a failed write leaves it unknown until a
//! later write succeeds (and heals the file to `{}` meanwhile), and a reload holds a ticket
//! (`begin`/`settle`) during which readers see unknown. Node reaches it on the project-store
//! listener (`POST /v1/preset-baseline/*`, [`Guard::bridge`]). Until a Node opens a protocol-1
//! session, a legacy Node still writes the file itself (shared mode); the file is re-read on every
//! use, never cached, so its writes are seen either way.

use super::router::{rows, Router};
use super::{Hold, HoldError, Maint};
use js_json::{wtf16, JValue};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// A lock file older than this belongs to a writer that died.
pub const LOCK_STALE: Duration = Duration::from_secs(30);
const LOCK_WAIT: Duration = Duration::from_secs(5);

const LOADED: &str = "A model is loaded. Unload it to apply the new settings.";
const IN_PROGRESS: &str = "Requests are in progress. Try again when chats finish.";
const NOT_RELOADED: &str = "The engine did not reload its settings. Check the Hardware tab.";
const COULD_NOT: &str = "Could not reload model profiles.";
pub(super) const NO_ARBITER: &str = "The model service could not confirm that chat is idle, so nothing was changed. Try again shortly.";
const LEASE_LOST: &str = "The model service stopped confirming that chat is held back, so the reload stopped. Try again shortly.";
const NEEDS_TOKEN: &str = "An inference gate fronts the engine and INFERENCE_GATE_ADMIN_TOKEN is not set, so a reload that unloads models is refused.";
/// `POST /models/unload` and `GET /models?reload=1`, as the router client bounds them, plus the rest.
const UNLOAD_LIMIT: Duration = Duration::from_secs(60);
const RELOAD_LIMIT: Duration = Duration::from_secs(120);
/// llamacpp-manager.cjs `PRESET_STEP_MS`: the lease a models.ini write needs left.
pub(super) const PRESET_STEP: Duration = Duration::from_millis(150_000);
const PRESETS_BUSY: &str = "Models are busy, so nothing was changed. Try again shortly.";
const OTHER_STEPS: Duration = Duration::from_secs(60);

/// The worst case of unloading `models` models one by one and then reloading.
pub fn worst_case(models: usize) -> Duration {
    UNLOAD_LIMIT.saturating_mul(u32::try_from(models).unwrap_or(u32::MAX))
        + RELOAD_LIMIT
        + OTHER_STEPS
}

/// Statuses the router counts as running (server-models.h `is_running()`).
pub fn live_ids(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .filter(|m| {
            matches!(
                m.get("status")
                    .and_then(|s| s.get("value"))
                    .and_then(Value::as_str),
                Some("loaded" | "loading" | "sleeping")
            )
        })
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// `engineView(rows)`: the router's view (model id to its reported preset text) as the canonical
/// string `JSON.stringify(ids.sort().map(id => [id, preset]))`. `None` (unknown) when any row has
/// no string id or `status.preset`.
pub fn engine_view(listing: &[Value]) -> Option<String> {
    view_of(listing.iter().map(|row| {
        let id = row.get("id")?.as_str()?;
        let preset = row.get("status")?.get("preset")?.as_str()?;
        // A character of the range wtf16 keeps lone surrogates in is not one here: fail closed
        // rather than let it read as one.
        (!holds_lone_range(id) && !holds_lone_range(preset)).then_some((id, preset))
    }))
}

/// [`engine_view`] of rows Node sent (`js_json::wtf16`, lone surrogates kept): the one view
/// implementation for both processes.
pub fn engine_view_js(rows: &[JValue]) -> Option<String> {
    view_of(rows.iter().map(|row| {
        Some((
            row.get("id").as_str()?,
            row.get("status").get("preset").as_str()?,
        ))
    }))
}

fn holds_lone_range(s: &str) -> bool {
    s.chars().any(|c| wtf16::lone_unit(c).is_some())
}

/// The JS string's UTF-16 units (a held lone surrogate is the one unit it stands for).
fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        match wtf16::lone_unit(c) {
            Some(unit) => out.push(unit),
            None => out.extend_from_slice(c.encode_utf16(&mut [0u16; 2])),
        }
    }
    out
}

fn view_of<'a>(pairs: impl Iterator<Item = Option<(&'a str, &'a str)>>) -> Option<String> {
    // JS `sort()` orders by UTF-16 code unit; the last row for an id wins.
    let mut out: BTreeMap<Vec<u16>, (&str, &str)> = BTreeMap::new();
    for pair in pairs {
        let (id, preset) = pair?;
        out.insert(units(id), (id, preset));
    }
    let pairs = out
        .into_values()
        .map(|(i, p)| JValue::Arr(vec![JValue::from(i), JValue::from(p)]))
        .collect();
    wtf16::stringify(&JValue::Arr(pairs))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Baseline {
    text: String,
    view: String,
}

/// May the router reload now without unloading any loaded model?
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub safe: bool,
    pub reason: String,
    /// The loaded models whose preset changed, as the check reports them (Core's `changed`).
    pub changed: Vec<String>,
}

impl Verdict {
    /// Core's shape: `{safe, reason}`, plus `changed` when the check itself said no.
    pub fn to_js(&self) -> JValue {
        let mut items = vec![
            ("safe".to_owned(), JValue::Bool(self.safe)),
            ("reason".to_owned(), JValue::from(self.reason.as_str())),
        ];
        let guard_reason = matches!(
            self.reason.as_str(),
            "unknown" | "engine_moved" | "unreadable" | "check_failed"
        );
        if !self.safe && !guard_reason {
            items.push((
                "changed".to_owned(),
                JValue::Arr(
                    self.changed
                        .iter()
                        .map(|c| JValue::from(c.as_str()))
                        .collect(),
                ),
            ));
        }
        JValue::Obj(items)
    }
}

fn unsafe_because(reason: &str) -> Verdict {
    Verdict {
        safe: false,
        reason: reason.into(),
        changed: Vec::new(),
    }
}

const TOMBSTONE: &str = "{}";
/// The origin of the front's own changes; `expired` and `stale` are the authority's.
pub const ORIGIN_RUST: &str = "rust";
const ORIGIN_EXPIRED: &str = "expired";
const ORIGIN_STALE: &str = "stale";
/// The bridge protocol this authority speaks.
pub const PROTOCOL: u32 = 1;
/// A reload ticket's life: the router's reload limit plus the other steps (Node dying mid-reload).
pub const TICKET_LIFE: Duration =
    Duration::from_secs(RELOAD_LIMIT.as_secs() + OTHER_STEPS.as_secs());

/// The authority's clocks: production uses [`Timing::default`].
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub ticket_life: Duration,
    /// After a failed write: retry the tombstone this often, this many times.
    pub heal_every: Duration,
    pub heal_tries: u32,
    /// How long a write waits for another writer's live lock.
    pub lock_wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            ticket_life: TICKET_LIFE,
            heal_every: Duration::from_secs(1),
            heal_tries: 60,
            lock_wait: LOCK_WAIT,
        }
    }
}

/// Who asked for a change.
#[derive(Debug, Clone)]
struct Who {
    origin: String,
    /// Asked over the bridge (Node), so a new Node session may lift its failure.
    node: bool,
}

impl Who {
    fn rust(origin: &str) -> Who {
        Who {
            origin: origin.to_owned(),
            node: false,
        }
    }
}

/// A write that failed: the baseline is unknown until a later one succeeds.
#[derive(Debug, Clone)]
struct Failure {
    gen: u64,
    origin: String,
    node: bool,
    node_session: Option<String>,
}

#[derive(Debug)]
struct Ticket {
    gen: u64,
    who: Who,
    node_session: Option<String>,
    deadline: Instant,
    overlapped: bool,
}

struct Job {
    gen: u64,
    body: String,
    who: Who,
    node_session: Option<String>,
    done: Option<SyncSender<bool>>,
}

#[derive(Default)]
struct State {
    /// +1 on every change asked for (record, forget, invalidate, begin, settle).
    gen: u64,
    /// The gen of the last write that reached the disk.
    written: u64,
    failed: Option<Failure>,
    open: BTreeMap<String, Ticket>,
    serial: u64,
    /// The last Node boot id a `session` named.
    node_session: Option<String>,
    /// A protocol-1 `session` arrived: Node writes through here. Until then, shared mode (a legacy
    /// Node writes the file itself; the file is re-read on every use either way).
    bridged: bool,
    /// The writes asked for, in gen order; one thread performs them.
    queue: VecDeque<Job>,
    closed: bool,
    no_writer: bool,
}

/// How a job's write reaches the disk ([`write_locked_within`]; tests put a failing one in).
type WriteFn = fn(&Path, &[u8], Duration) -> std::io::Result<()>;

struct Shared {
    file: Option<PathBuf>,
    epoch: String,
    timing: Timing,
    write: WriteFn,
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait_until<'a>(
        &self,
        st: MutexGuard<'a, State>,
        until: Option<Instant>,
    ) -> MutexGuard<'a, State> {
        match until {
            Some(t) => {
                let wait = t
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1));
                match self.wake.wait_timeout(st, wait) {
                    Ok((st, _)) => st,
                    Err(e) => e.into_inner().0,
                }
            }
            None => self.wake.wait(st).unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// Ask for a write of `body` with the next gen; `done` hears whether it reached the disk.
    fn push(
        &self,
        st: &mut State,
        body: String,
        who: Who,
        node_session: Option<String>,
        done: Option<SyncSender<bool>>,
    ) -> u64 {
        st.gen += 1;
        let gen = st.gen;
        if self.file.is_none() || st.no_writer {
            if let Some(d) = done {
                let _ = d.send(false);
            }
            return gen;
        }
        st.queue.push_back(Job {
            gen,
            body,
            who,
            node_session,
            done,
        });
        self.wake.notify_all();
        gen
    }

    /// Tickets past their deadline count as a forget (origin `expired`).
    fn sweep(&self, st: &mut State) {
        let now = Instant::now();
        let expired: Vec<String> = st
            .open
            .iter()
            .filter(|(_, t)| t.deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            st.open.remove(&k);
            eprintln!(
                "[models] a preset reload did not settle in time; the reload baseline is treated as unknown"
            );
            self.push(st, TOMBSTONE.into(), Who::rust(ORIGIN_EXPIRED), None, None);
        }
    }

    fn next_deadline(st: &State) -> Option<Instant> {
        st.open.values().map(|t| t.deadline).min()
    }

    /// How long a waiter waits for its write: every write queued up to and including its own, plus
    /// the one (or a heal try) in progress, at the lock wait each, and a margin. Past that it counts
    /// as not saved, so a stuck writer can never hold a caller (or the maintenance hold) forever.
    fn reply_within(&self, st: &State) -> Duration {
        let writes = u32::try_from(st.queue.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        self.timing
            .lock_wait
            .saturating_mul(writes)
            .saturating_add(Duration::from_secs(1))
    }
}

/// Runs when the writer ends, normally or by unwinding: nothing can be written any more, so the
/// baseline is unknown for the rest of this start and every queued waiter hears "not saved".
struct WriterExit(Arc<Shared>);

impl Drop for WriterExit {
    fn drop(&mut self) {
        let mut st = self.0.lock();
        st.no_writer = true;
        if std::thread::panicking() {
            st.failed = Some(Failure {
                gen: st.gen,
                origin: ORIGIN_RUST.into(),
                node: false,
                node_session: None,
            });
        }
        for job in st.queue.drain(..) {
            if let Some(done) = job.done {
                let _ = done.send(false);
            }
        }
        drop(st);
        if std::thread::panicking() {
            let _ = writeln!(
                std::io::stderr(),
                "[models] the preset reload baseline writer stopped; the baseline is unknown until a restart"
            );
        }
    }
}

/// Ends the writer once the last [`Guard`] handle is gone (after the queued writes).
struct Owner(Arc<Shared>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.lock().closed = true;
        self.0.wake.notify_all();
    }
}

/// The one writer: performs the queued writes in gen order, and heals after a failure.
fn writer(shared: Arc<Shared>) {
    let _exit = WriterExit(Arc::clone(&shared));
    let Some(file) = shared.file.clone() else {
        return;
    };
    loop {
        let job = {
            let mut st = shared.lock();
            loop {
                shared.sweep(&mut st);
                if let Some(job) = st.queue.pop_front() {
                    break job;
                }
                if st.closed {
                    return;
                }
                let until = Shared::next_deadline(&st);
                st = shared.wait_until(st, until);
            }
        };
        // What the file held before this write: heal leaves alone a file another writer has
        // replaced since (a legacy Node recording in shared mode).
        let seen = std::fs::read(&file).ok();
        let result = (shared.write)(&file, job.body.as_bytes(), shared.timing.lock_wait);
        {
            let mut st = shared.lock();
            match &result {
                Ok(()) => {
                    st.written = st.written.max(job.gen);
                    if st.failed.as_ref().is_some_and(|f| f.gen < job.gen) {
                        st.failed = None;
                    }
                }
                Err(e) => {
                    st.failed = Some(Failure {
                        gen: job.gen,
                        origin: job.who.origin.clone(),
                        node: job.who.node,
                        node_session: job.node_session.clone(),
                    });
                    eprintln!(
                        "[models] could not save the preset reload baseline; it is treated as unknown: {e}"
                    );
                }
            }
        }
        if let Some(done) = job.done {
            let _ = done.send(result.is_ok());
        }
        if result.is_err() {
            heal(&shared, &file, job.gen, seen.as_deref());
        }
    }
}

/// After the write of `gen` failed: write the tombstone every `heal_every`, at most `heal_tries`
/// times, while nothing newer was asked for, so the disk itself says unknown and a restart cannot
/// trust the old baseline. It never lifts the failure (only a later write does). It stops, writing
/// nothing, once the file no longer holds what it held before the failed write (`seen`): another
/// writer (a legacy Node, shared mode) replaced it under the lock since, and its baseline stands.
fn heal(shared: &Shared, file: &Path, gen: u64, seen: Option<&[u8]>) {
    for _ in 0..shared.timing.heal_tries {
        let until = Instant::now() + shared.timing.heal_every;
        {
            let mut st = shared.lock();
            loop {
                shared.sweep(&mut st);
                let still = st.gen == gen
                    && st.queue.is_empty()
                    && !st.closed
                    && st.failed.as_ref().is_some_and(|f| f.gen == gen);
                if !still {
                    return;
                }
                if Instant::now() >= until {
                    break;
                }
                st = shared.wait_until(st, Some(until));
            }
        }
        if write_locked_unless_changed(file, TOMBSTONE.as_bytes(), shared.timing.lock_wait, seen)
            .is_ok()
        {
            return;
        }
    }
}

/// A reload ticket from [`Guard::begin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Begun {
    pub ticket: String,
    pub gen: u64,
    pub epoch: String,
    /// When the ticket expires, in ms since the epoch.
    pub deadline_ms: i64,
}

/// How a reload ended, for [`Guard::settle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settle {
    /// The text read just before the reload, the router's view just after, and the text re-read
    /// after that.
    Record {
        before: Option<String>,
        view: Option<String>,
        after: Option<String>,
    },
    Forget,
}

/// A write's gen and whether it reached the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Saved {
    pub gen: u64,
    pub saved: bool,
}

/// The preset reload baseline's one in-memory authority (port contract 2026-11-04 §3). Cheap to
/// clone; every clone is the same authority.
///
/// One thread performs every write, in the order they were asked for (each takes the next gen).
/// A read is unknown while a write failed (sticky, until a later write succeeds), while any write
/// is still queued, and while any reload ticket is open; otherwise it re-reads the file, never a
/// copy, so a legacy Node that writes the file itself (shared mode) is seen.
#[derive(Clone)]
pub struct Guard {
    shared: Arc<Shared>,
    _owner: Arc<Owner>,
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guard")
            .field("file", &self.shared.file)
            .finish_non_exhaustive()
    }
}

impl Guard {
    pub fn new(file: Option<PathBuf>) -> Guard {
        Guard::with(file, "", Timing::default())
    }

    /// `epoch`: this start's NOEVIA_START_ID.
    pub fn with(file: Option<PathBuf>, epoch: &str, timing: Timing) -> Guard {
        Guard::build(file, epoch, timing, write_locked_within)
    }

    fn build(file: Option<PathBuf>, epoch: &str, timing: Timing, write: WriteFn) -> Guard {
        let shared = Arc::new(Shared {
            file,
            epoch: epoch.to_owned(),
            timing,
            write,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        });
        if shared.file.is_some() {
            let s = Arc::clone(&shared);
            let spawned = std::thread::Builder::new()
                .name("preset-baseline".into())
                .spawn(move || writer(s));
            if let Err(e) = spawned {
                eprintln!(
                    "[models] the preset reload baseline has no writer, so it is unknown: {e}"
                );
                let mut st = shared.lock();
                st.no_writer = true;
                st.failed = Some(Failure {
                    gen: 0,
                    origin: ORIGIN_RUST.into(),
                    node: false,
                    node_session: None,
                });
            }
        }
        let owner = Arc::new(Owner(Arc::clone(&shared)));
        Guard {
            shared,
            _owner: owner,
        }
    }

    pub fn file(&self) -> Option<&Path> {
        self.shared.file.as_deref()
    }

    pub fn epoch(&self) -> &str {
        &self.shared.epoch
    }

    /// A protocol-1 Node session arrived during this start (bridge mode); otherwise shared mode.
    ///
    /// `false` is not proof that Node is a legacy one: after a restart of this process alone, a
    /// bridge-mode Node has not re-opened its session yet. Node must compare the `epoch` that
    /// `begin` (or a 409) answers with the one `session` gave and send `session` again when it
    /// changed (CORE-1), and a reader choosing native over Node's facts (RS-2) must allow for it.
    pub fn bridged(&self) -> bool {
        self.shared.lock().bridged
    }

    /// The saved baseline, or `None` (unknown): a failed or pending write, an open ticket, no
    /// file, unreadable, or the `{}` tombstone.
    fn load(&self) -> Option<Baseline> {
        let file = self.shared.file.as_ref()?;
        {
            let mut st = self.shared.lock();
            self.shared.sweep(&mut st);
            if st.failed.is_some() || st.written < st.gen || !st.open.is_empty() {
                return None;
            }
        }
        read_baseline(file)
    }

    /// Queue a write and wait until it was tried.
    fn mutate(&self, body: String, who: Who, node_session: Option<String>) -> Saved {
        let (tx, rx) = sync_channel(1);
        let (gen, within) = {
            let mut st = self.shared.lock();
            self.shared.sweep(&mut st);
            let gen = self.shared.push(&mut st, body, who, node_session, Some(tx));
            (gen, self.shared.reply_within(&st))
        };
        Saved {
            gen,
            saved: rx.recv_timeout(within).unwrap_or(false),
        }
    }

    fn node_who(&self, origin: &str) -> (Who, Option<String>) {
        let session = self.shared.lock().node_session.clone();
        (
            Who {
                origin: origin.to_owned(),
                node: true,
            },
            session,
        )
    }

    /// After a reload made here without a ticket: the text read just before it and the router's
    /// rows just after. `after` is the file re-read after the rows; if it moved, the router may
    /// have read either. Returns once the write was tried; `true` when it reached the disk.
    pub fn record(&self, before: Option<&str>, listing: &[Value], after: Option<&str>) -> bool {
        let body = body_of(before, engine_view(listing), after);
        self.mutate(body, Who::rust(ORIGIN_RUST), None).saved
    }

    pub fn forget(&self) -> bool {
        self.mutate(TOMBSTONE.into(), Who::rust(ORIGIN_RUST), None)
            .saved
    }

    /// The router is about to re-read without a ticket (a raw reload): unknown from now on.
    pub fn invalidate(&self, origin: &str) -> Saved {
        self.mutate(TOMBSTONE.into(), Who::rust(origin), None)
    }

    /// A reload is about to start: readers see unknown until it settles or expires.
    pub fn begin(&self, origin: &str) -> Begun {
        self.begin_as(Who::rust(origin), None)
    }

    fn begin_as(&self, who: Who, node_session: Option<String>) -> Begun {
        let life = self.shared.timing.ticket_life;
        let mut st = self.shared.lock();
        self.shared.sweep(&mut st);
        st.gen += 1;
        st.serial += 1;
        let gen = st.gen;
        let ticket = format!(
            "{}-{}",
            if self.shared.epoch.is_empty() {
                "0"
            } else {
                self.shared.epoch.as_str()
            },
            st.serial
        );
        let overlapped = !st.open.is_empty();
        for t in st.open.values_mut() {
            t.overlapped = true;
        }
        st.open.insert(
            ticket.clone(),
            Ticket {
                gen,
                who,
                node_session,
                deadline: Instant::now() + life,
                overlapped,
            },
        );
        // The writer waits for the earliest deadline.
        self.shared.wake.notify_all();
        Begun {
            ticket,
            gen,
            epoch: self.shared.epoch.clone(),
            deadline_ms: crate::identity::now_ms()
                .saturating_add(i64::try_from(life.as_millis()).unwrap_or(i64::MAX)),
        }
    }

    /// The reload of `ticket` ended. A ticket that overlapped another, or that anything else was
    /// asked for after, forgets: the router may have read something else meanwhile. An unknown or
    /// expired ticket forgets too and is `Err` (the bridge's 409 `stale_ticket`).
    pub fn settle(&self, ticket: &str, outcome: Settle) -> Result<Saved, Saved> {
        self.settle_as(ticket, outcome, Who::rust(ORIGIN_STALE))
    }

    fn settle_as(&self, ticket: &str, outcome: Settle, stale: Who) -> Result<Saved, Saved> {
        let (tx, rx) = sync_channel(1);
        let (gen, known, within) = {
            let mut st = self.shared.lock();
            self.shared.sweep(&mut st);
            match st.open.remove(ticket) {
                None => {
                    let session = stale.node.then(|| st.node_session.clone()).flatten();
                    let gen = self
                        .shared
                        .push(&mut st, TOMBSTONE.into(), stale, session, Some(tx));
                    (gen, false, self.shared.reply_within(&st))
                }
                Some(t) => {
                    let body = match outcome {
                        Settle::Record {
                            before,
                            view,
                            after,
                        } if !t.overlapped && t.gen == st.gen => {
                            body_of(before.as_deref(), view, after.as_deref())
                        }
                        _ => TOMBSTONE.into(),
                    };
                    let gen = self
                        .shared
                        .push(&mut st, body, t.who, t.node_session, Some(tx));
                    (gen, true, self.shared.reply_within(&st))
                }
            }
        };
        let saved = Saved {
            gen,
            saved: rx.recv_timeout(within).unwrap_or(false),
        };
        if known {
            Ok(saved)
        } else {
            Err(saved)
        }
    }

    /// Node (re)started with boot id `node_boot`. A failure Node caused in an earlier session is
    /// lifted (as a Node restart did when Node kept the baseline), and that session's open tickets
    /// end as a forget. A failure the front caused stays until a later write succeeds.
    pub fn session(&self, node_boot: &str) -> String {
        let mut st = self.shared.lock();
        self.shared.sweep(&mut st);
        st.bridged = true;
        if st.node_session.as_deref() != Some(node_boot) {
            st.node_session = Some(node_boot.to_owned());
            let lifted = st
                .failed
                .as_ref()
                .filter(|f| f.node && f.node_session.as_deref() != Some(node_boot))
                .map(|f| (f.gen, f.origin.clone()));
            if let Some((gen, origin)) = lifted {
                eprintln!(
                    "[models] Node restarted: its failed preset reload baseline write ({origin}) no longer counts"
                );
                st.failed = None;
                st.written = st.written.max(gen);
            }
            let old: Vec<String> = st
                .open
                .iter()
                .filter(|(_, t)| t.who.node && t.node_session.as_deref() != Some(node_boot))
                .map(|(k, _)| k.clone())
                .collect();
            for k in old {
                st.open.remove(&k);
                self.shared.push(
                    &mut st,
                    TOMBSTONE.into(),
                    Who::rust(ORIGIN_EXPIRED),
                    None,
                    None,
                );
            }
        }
        self.shared.epoch.clone()
    }

    pub fn known(&self) -> bool {
        self.load().is_some()
    }

    /// The models.ini text the router last read, while that is still knowable.
    pub fn router_text(&self, listing: &[Value]) -> Option<String> {
        self.router_text_of(engine_view(listing))
    }

    fn router_text_of(&self, view: Option<String>) -> Option<String> {
        let b = self.load()?;
        (view.as_deref() == Some(b.view.as_str())).then_some(b.text)
    }

    /// `verdict(currentText, rows, loadedIds)`.
    pub fn verdict(
        &self,
        current: Option<&str>,
        listing: Option<&[Value]>,
        loaded: &[String],
    ) -> Verdict {
        self.verdict_of_view(current, listing.map(engine_view), loaded)
    }

    /// `view`: `None` without a listing, `Some(None)` for a listing with no view.
    fn verdict_of_view(
        &self,
        current: Option<&str>,
        view: Option<Option<String>>,
        loaded: &[String],
    ) -> Verdict {
        let Some(base) = self.load() else {
            return unsafe_because("unknown");
        };
        let Some(view) = view.flatten() else {
            return unsafe_because("unknown");
        };
        if view != base.view {
            return unsafe_because("engine_moved");
        }
        let Some(current) = current else {
            return unsafe_because("unreadable");
        };
        let request =
            json!({ "baseline": base.text, "current": current, "loaded": loaded }).to_string();
        let (status, reply) = preset_reload::check_json(&request);
        if status != 0 {
            eprintln!("[models] preset reload check failed closed: {reply}");
            return unsafe_because("check_failed");
        }
        let v: Value = serde_json::from_str(&reply).unwrap_or(Value::Null);
        if v.get("safe") == Some(&Value::Bool(true)) {
            return Verdict {
                safe: true,
                reason: "unchanged".into(),
                changed: Vec::new(),
            };
        }
        let reason = v
            .get("reason")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
            .unwrap_or("changed");
        let changed = v
            .get("changed")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Verdict {
            safe: false,
            reason: reason.into(),
            changed,
        }
    }

    /// `POST /v1/preset-baseline/{op}` on the project-store listener (project_store.rs; the token
    /// is checked there): the status and the answer. See the port contract §4.
    pub fn bridge(&self, op: &str, body: &JValue) -> (u16, JValue) {
        match op {
            "session" => {
                let boot = match body.get("nodeBoot") {
                    JValue::Str(s) if !s.is_empty() && s.len() <= 128 => s.clone(),
                    _ => return bad("nodeBoot must be a non-empty string", "bad_request"),
                };
                if body.get("protocol").as_num() != Some(f64::from(PROTOCOL)) {
                    return bad("protocol must be 1", "unsupported_protocol");
                }
                let epoch = self.session(&boot);
                (
                    200,
                    JValue::obj([
                        ("epoch", JValue::Str(epoch)),
                        ("protocol", JValue::Num(f64::from(PROTOCOL))),
                    ]),
                )
            }
            "begin" => {
                let origin = match node_origin(body) {
                    Ok(o) => o,
                    Err(e) => return e,
                };
                let (who, session) = self.node_who(&origin);
                let b = self.begin_as(who, session);
                (
                    200,
                    JValue::obj([
                        ("ticket", JValue::Str(b.ticket)),
                        ("gen", num(b.gen)),
                        ("epoch", JValue::Str(b.epoch)),
                        ("deadlineMs", JValue::Num(b.deadline_ms as f64)),
                    ]),
                )
            }
            "settle" => {
                let Some(ticket) = body.get("ticket").as_str() else {
                    return bad("ticket must be a string", "bad_request");
                };
                let text = |k: &str| match body.get(k) {
                    JValue::Str(s) => Ok(Some(s.clone())),
                    JValue::Undefined | JValue::Null => Ok(None),
                    _ => Err(()),
                };
                let outcome = match body.get("outcome").as_str() {
                    Some("forget") => Settle::Forget,
                    Some("record") => {
                        let (Ok(before), Ok(after)) = (text("before"), text("after")) else {
                            return bad("before and after must be strings or null", "bad_request");
                        };
                        let view = match body.get("rows") {
                            JValue::Arr(rows) => engine_view_js(rows),
                            _ => None,
                        };
                        Settle::Record {
                            before,
                            view,
                            after,
                        }
                    }
                    _ => return bad("outcome must be record or forget", "bad_request"),
                };
                let (who, _) = self.node_who(ORIGIN_STALE);
                match self.settle_as(ticket, outcome, who) {
                    Ok(s) => (200, saved_js(s)),
                    Err(s) => {
                        let JValue::Obj(mut items) = saved_js(s) else {
                            return (409, JValue::Null);
                        };
                        items.insert(
                            0,
                            (
                                "error".into(),
                                JValue::from("The reload ticket is unknown or expired."),
                            ),
                        );
                        items.insert(1, ("code".into(), JValue::from("stale_ticket")));
                        (409, JValue::Obj(items))
                    }
                }
            }
            "invalidate" => {
                let origin = match node_origin(body) {
                    Ok(o) => o,
                    Err(e) => return e,
                };
                let (who, session) = self.node_who(&origin);
                (200, saved_js(self.mutate(TOMBSTONE.into(), who, session)))
            }
            "verdict" => {
                let current = match body.get("current") {
                    JValue::Str(s) => Some(s.as_str()),
                    JValue::Undefined | JValue::Null => None,
                    _ => return bad("current must be a string or null", "bad_request"),
                };
                let loaded: Option<Vec<String>> = match body.get("loaded") {
                    JValue::Arr(ids) => ids.iter().map(|i| i.as_str().map(str::to_owned)).collect(),
                    _ => None,
                };
                let Some(loaded) = loaded else {
                    return bad("loaded must be a list of ids", "bad_request");
                };
                let view = match body.get("rows") {
                    JValue::Arr(rows) => Some(engine_view_js(rows)),
                    JValue::Undefined | JValue::Null => None,
                    _ => Some(None),
                };
                (200, self.verdict_of_view(current, view, &loaded).to_js())
            }
            "router-text" => {
                let view = match body.get("rows") {
                    JValue::Arr(rows) => engine_view_js(rows),
                    _ => None,
                };
                let text = self
                    .router_text_of(view)
                    .map(JValue::Str)
                    .unwrap_or(JValue::Null);
                (200, JValue::obj([("text", text)]))
            }
            _ => (404, JValue::obj([("error", JValue::from("not found"))])),
        }
    }
}

/// The ops [`Guard::bridge`] answers.
pub const BRIDGE_OPS: [&str; 6] = [
    "session",
    "begin",
    "settle",
    "invalidate",
    "verdict",
    "router-text",
];

fn num(n: u64) -> JValue {
    JValue::Num(n as f64)
}

fn saved_js(s: Saved) -> JValue {
    JValue::obj([("gen", num(s.gen)), ("saved", JValue::Bool(s.saved))])
}

fn bad(message: &str, code: &str) -> (u16, JValue) {
    (
        400,
        JValue::obj([
            ("error", JValue::from(message)),
            ("code", JValue::from(code)),
        ]),
    )
}

/// A bridge request's `origin`: a short `[a-z0-9_-]` name; `rust`, `expired` and `stale` are the
/// authority's own.
fn node_origin(body: &JValue) -> Result<String, (u16, JValue)> {
    match body.get("origin") {
        JValue::Str(s)
            if !s.is_empty()
                && s.len() <= 64
                && s.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
                })
                && ![ORIGIN_RUST, ORIGIN_EXPIRED, ORIGIN_STALE].contains(&s.as_str()) =>
        {
            Ok(s.clone())
        }
        _ => Err(bad("origin must be a short lowercase name", "bad_origin")),
    }
}

/// The file's text for a reload's outcome: `{text, view}` when the text did not move and the view
/// is known, else the tombstone.
fn body_of(before: Option<&str>, view: Option<String>, after: Option<&str>) -> String {
    match (before, after, view) {
        (Some(b), Some(a), Some(view)) if b == a => wtf16::stringify(&JValue::obj([
            ("text", JValue::from(b)),
            ("view", JValue::Str(view)),
        ]))
        .unwrap_or_else(|| TOMBSTONE.into()),
        _ => TOMBSTONE.into(),
    }
}

/// `{text, view}` from the file, or `None` (no file, unreadable, the tombstone, anything else).
fn read_baseline(file: &Path) -> Option<Baseline> {
    let raw = std::fs::read_to_string(file).ok()?;
    let v = wtf16::parse(&raw).ok()?;
    Some(Baseline {
        text: v.get("text").as_str()?.to_owned(),
        view: v.get("view").as_str()?.to_owned(),
    })
}

/// The lock file beside a state file.
fn lock_path(file: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", file.display()))
}

/// The lock file and what this writer wrote in it. Removed only while it still says that: a writer
/// that overran [`LOCK_STALE`] may have lost it to another.
struct Lock(PathBuf, String);

impl Drop for Lock {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.0).is_ok_and(|held| held == self.1) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

static LOCK_SERIAL: AtomicU64 = AtomicU64::new(0);

fn take_lock(file: &Path, wait: Duration) -> std::io::Result<Lock> {
    let path = lock_path(file);
    let started = Instant::now();
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                let owner = format!(
                    "{}:{}:{}",
                    std::process::id(),
                    LOCK_SERIAL.fetch_add(1, Ordering::Relaxed),
                    crate::identity::now_ms()
                );
                let _ = write!(f, "{owner}");
                return Ok(Lock(path, owner));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > LOCK_STALE);
                if stale {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if started.elapsed() > wait {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Atomic replace of `file` with `bytes` (0600) while holding its lock file.
#[cfg(test)]
fn write_locked(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_locked_within(file, bytes, LOCK_WAIT)
}

/// [`write_locked`], waiting at most `wait` for another writer's live lock.
fn write_locked_within(file: &Path, bytes: &[u8], wait: Duration) -> std::io::Result<()> {
    let _lock = take_lock(file, wait)?;
    replace(file, bytes)
}

/// [`write_locked_within`] only while the file still holds `seen` (`None`: no file), checked under
/// the lock; `Ok(false)` when another writer changed it, and nothing is written.
fn write_locked_unless_changed(
    file: &Path,
    bytes: &[u8],
    wait: Duration,
    seen: Option<&[u8]>,
) -> std::io::Result<bool> {
    let _lock = take_lock(file, wait)?;
    if std::fs::read(file).ok().as_deref() != seen {
        return Ok(false);
    }
    replace(file, bytes).map(|()| true)
}

/// The atomic replace itself (0600, fsync, rename); the caller holds the lock.
fn replace(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let temp = PathBuf::from(format!("{}.{}.tmp", file.display(), std::process::id()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&temp, file)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// The answer: status and the JSON text, in Node's key order.
pub struct Outcome {
    pub status: u16,
    pub body: String,
}

/// Whether an outcome is the hold's lease ending under a step (core's `coverageLost`, which Node
/// throws rather than answers).
pub(crate) fn lease_lost(o: &Outcome) -> bool {
    o.status == 503
        && serde_json::from_str::<Value>(&o.body)
            .is_ok_and(|b| b.get("error").and_then(Value::as_str) == Some(LEASE_LOST))
}

fn out(status: u16, body: &Value) -> Outcome {
    Outcome {
        status,
        body: body.to_string(),
    }
}

fn message(status: u16, text: &str) -> Outcome {
    out(status, &json!({ "error": text }))
}

fn refuse(ids: &[String], reason: Option<&str>) -> Outcome {
    let mut body = format!("{{\"error\":{},\"loaded\":{}", json!(LOADED), json!(ids));
    if let Some(r) = reason {
        body.push_str(&format!(",\"reason\":{}", json!(r)));
    }
    body.push('}');
    Outcome { status: 409, body }
}

/// A router answer that is not ok, passed on as the route does (`json(res, status, body)`).
fn passed_on(status: u16, body: &Option<Value>) -> Outcome {
    Outcome {
        status,
        body: body.clone().unwrap_or(Value::Null).to_string(),
    }
}

impl Maint {
    /// The models.ini text now, or `None` when it cannot be read (no preset path, a symlink, a
    /// duplicate section, over the limit).
    pub(super) async fn ini_text(&self) -> Option<String> {
        let store = self.ini.clone()?;
        tokio::task::spawn_blocking(move || store.snapshot().ok().map(|s| s.text))
            .await
            .ok()
            .flatten()
    }

    pub(super) async fn guard_op<T: Send + 'static>(
        &self,
        f: impl FnOnce(Guard) -> T + Send + 'static,
    ) -> Option<T> {
        let g = self.guard.clone();
        tokio::task::spawn_blocking(move || f(g)).await.ok()
    }

    /// `routerReload(120000, { expectText })`: `Ok(Ok(reply))` or `Ok(Err(refused))`; `Err` when the
    /// request threw (the baseline is forgotten first). The reload holds a ticket of the baseline
    /// authority (origin `rust`) from just before the router call until it settles, so readers see
    /// unknown meanwhile and an overlapping reload forgets.
    pub(super) async fn router_reload(
        &self,
        router: &Router,
        holding: &Hold,
        expect: Option<&str>,
    ) -> Result<Result<super::router::Reply, &'static str>, ()> {
        let before = self.ini_text().await;
        if let Some(expect) = expect {
            if before.as_deref() != Some(expect) {
                return Ok(Err("file_changed"));
            }
        }
        // Checked again here, right at the call: reading the file above takes what it takes.
        if !holding.may_start(RELOAD_LIMIT) {
            return Ok(Err("lease_lost"));
        }
        let ticket = self.guard.begin(ORIGIN_RUST).ticket;
        let token = holding.token();
        let result = router
            .get("/models?reload=1", token.as_deref(), RELOAD_LIMIT)
            .await;
        let reply = match result {
            Ok(r) => r,
            Err(_) => {
                self.settle(ticket, Settle::Forget).await;
                return Err(());
            }
        };
        if !reply.ok {
            self.settle(ticket, Settle::Forget).await;
            return Ok(Ok(reply));
        }
        let outcome = match router.models(token.as_deref()).await {
            Ok(after) if after.ok => {
                let after_text = self.ini_text().await;
                match (rows(&after.body), before) {
                    (Some(listing), Some(before)) => Settle::Record {
                        before: Some(before),
                        view: engine_view(listing),
                        after: after_text,
                    },
                    _ => Settle::Forget,
                }
            }
            _ => Settle::Forget,
        };
        self.settle(ticket, outcome).await;
        Ok(Ok(reply))
    }

    /// Settle a reload ticket of this process (the write waits on the blocking pool). An expired
    /// ticket has already counted as a forget.
    async fn settle(&self, ticket: String, outcome: Settle) {
        let _ = self
            .guard_op(move |g| {
                let _ = g.settle(&ticket, outcome);
            })
            .await;
    }

    /// `reloadPresets({ unload })`, under the maintenance hold. Runs to its end whatever happens to
    /// the client that asked: callers spawn it.
    pub async fn reload_presets(&self, unload: bool) -> Outcome {
        let Some(router) = self.router.as_ref() else {
            return message(404, "This engine does not use a preset file");
        };
        let hold = match self.acquire("", Duration::from_secs(300), unload).await {
            Ok(h) => h,
            Err(HoldError::Busy) => return message(409, IN_PROGRESS),
            Err(HoldError::Gate { status, message: m }) => return message(status, &m),
            Err(HoldError::NeedsGate) => return message(503, NEEDS_TOKEN),
            Err(HoldError::NodeUnavailable) => return message(503, NO_ARBITER),
        };
        let outcome = self.reload_inner(router, &hold, unload).await;
        hold.release().await;
        outcome
    }

    /// `writePresets(write, reloadIf)` (#2219): a models.ini write and its reload under one hold, so no other
    /// reload runs between them. `Err` with nothing written when the hold is refused (409 `maintenance_held`,
    /// retryable) or cannot be taken or covered; otherwise the written value and, when `reload_if` asked for
    /// one, whether the reload applied (the write stands whatever the reload does).
    pub async fn write_presets_held<T, F>(
        &self,
        write: impl FnOnce() -> F,
        reload_if: impl FnOnce(&T) -> bool,
    ) -> Result<(T, bool), Outcome>
    where
        F: std::future::Future<Output = T>,
    {
        let hold = self.preset_hold().await?;
        let written = write().await;
        let reloaded = if reload_if(&written) {
            self.reload_held(&hold).await.status == 200
        } else {
            false
        };
        hold.release().await;
        Ok((written, reloaded))
    }

    /// `writePresets`'s hold on its own, for a writer that decides between the write and the reload
    /// itself (track R11, model_writes_routes.rs): taken and covered for one preset write, or `Err`
    /// with the refusal (409 `maintenance_held` when another hold is in place). The caller releases it.
    pub(crate) async fn preset_hold(&self) -> Result<Hold, Outcome> {
        if self.router.is_none() {
            return Err(message(404, "This engine does not use a preset file"));
        }
        // Node's key order: `error`, `code`, `retryable`.
        let busy = || Outcome {
            status: 409,
            body: format!(
                r#"{{"error":{},"code":"maintenance_held","retryable":true}}"#,
                json!(PRESETS_BUSY)
            ),
        };
        let hold = match self.acquire("", Duration::from_secs(300), false).await {
            Ok(h) => h,
            Err(HoldError::Busy) => return Err(busy()),
            Err(HoldError::Gate { status: 409, .. }) => return Err(busy()),
            Err(HoldError::Gate { status, message: m }) => return Err(message(status, &m)),
            Err(HoldError::NeedsGate) => return Err(message(503, NEEDS_TOKEN)),
            Err(HoldError::NodeUnavailable) => return Err(message(503, NO_ARBITER)),
        };
        if !hold.may_start(PRESET_STEP) {
            hold.release().await;
            return Err(message(503, LEASE_LOST));
        }
        Ok(hold)
    }

    /// llamacpp-manager.cjs `presetsInRouter()`: the presets text the router runs on, from this
    /// process's reload baseline when the router's view still matches it; `None` when unknown.
    pub(crate) async fn router_text(&self) -> Option<String> {
        let listing = self.router.as_ref()?.models(None).await.ok()?;
        if !listing.ok {
            return None;
        }
        let rows = rows(&listing.body)?.clone();
        self.guard_op(move |g| g.router_text(&rows)).await.flatten()
    }

    /// `reloadHeld({ unload: false })` under a hold [`Maint::preset_hold`] took.
    pub(crate) async fn reload_held(&self, hold: &Hold) -> Outcome {
        match self.router.as_ref() {
            Some(router) => self.reload_inner(router, hold, false).await,
            None => message(404, "This engine does not use a preset file"),
        }
    }

    async fn reload_inner(&self, router: &Router, holding: &Hold, unload: bool) -> Outcome {
        let token = holding.token();
        let hold = token.as_deref();
        let listing = match router.models(hold).await {
            Ok(l) => l,
            Err(_) => return message(500, COULD_NOT),
        };
        if !listing.ok {
            return passed_on(listing.status, &listing.body);
        }
        let first: Vec<Value> = rows(&listing.body).cloned().unwrap_or_default();
        let loaded = live_ids(&first);
        let mut kept: Vec<String> = Vec::new();
        let mut expect: Option<String> = None;
        if !loaded.is_empty() && !unload {
            let judged = self.ini_text().await;
            let v = self
                .verdict_of(judged.clone(), Some(first.clone()), loaded.clone())
                .await;
            if !v.safe {
                return refuse(&loaded, Some(&v.reason));
            }
            // Right before the call: list again; a model that started loading meanwhile, or a
            // moved router, refuses (#1040). The file itself is re-read inside the reload.
            let again = match router.models(hold).await {
                Ok(a) if a.ok => a,
                _ => return refuse(&loaded, Some("unknown")),
            };
            let Some(again_rows) = rows(&again.body).cloned() else {
                return refuse(&loaded, Some("unknown"));
            };
            let now = live_ids(&again_rows);
            if now.iter().any(|id| !loaded.contains(id)) {
                return refuse(&now, Some("loaded_changed"));
            }
            let recheck = self
                .verdict_of(judged.clone(), Some(again_rows), now.clone())
                .await;
            if !recheck.safe {
                return refuse(&now, Some(&recheck.reason));
            }
            kept = now;
            expect = judged;
        }
        if kept.is_empty() {
            holding.cover(worst_case(loaded.len()));
            for id in &loaded {
                if !holding.may_start(UNLOAD_LIMIT) {
                    return message(503, LEASE_LOST);
                }
                let _ = router.unload(id, hold).await;
            }
        }
        if !holding.may_start(RELOAD_LIMIT) {
            return message(503, LEASE_LOST);
        }
        let result = match self.router_reload(router, holding, expect.as_deref()).await {
            Ok(r) => r,
            Err(()) => return message(500, COULD_NOT),
        };
        match result {
            Err("lease_lost") => message(503, LEASE_LOST),
            Err(why) => refuse(&kept, Some(why)),
            Ok(reply) if !reply.ok => message(502, NOT_RELOADED),
            Ok(_) => {
                let unloaded: Vec<String> = if kept.is_empty() { loaded } else { Vec::new() };
                let mut body = format!("{{\"reloaded\":true,\"unloaded\":{}", json!(unloaded));
                if !kept.is_empty() {
                    body.push_str(&format!(",\"kept\":{}", json!(kept)));
                }
                body.push('}');
                Outcome { status: 200, body }
            }
        }
    }

    async fn verdict_of(
        &self,
        current: Option<String>,
        listing: Option<Vec<Value>>,
        loaded: Vec<String>,
    ) -> Verdict {
        self.guard_op(move |g| g.verdict(current.as_deref(), listing.as_deref(), &loaded))
            .await
            .unwrap_or_else(|| unsafe_because("unknown"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_writer_does_not_remove_a_lock_another_writer_took_over() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("state.json");
        let lock = take_lock(&file, LOCK_WAIT).unwrap();
        // Its lock went stale while it worked; another writer took it over.
        std::fs::write(lock_path(&file), "another-writer").unwrap();
        drop(lock);
        assert_eq!(
            std::fs::read_to_string(lock_path(&file)).unwrap(),
            "another-writer"
        );
        std::fs::remove_file(lock_path(&file)).unwrap();
        // Its own it does remove.
        drop(take_lock(&file, LOCK_WAIT).unwrap());
        assert!(!lock_path(&file).exists());
    }

    /// Fast clocks for the tests: no heal unless asked, a short lock wait.
    fn timing(heal_tries: u32, ticket_life: Duration) -> Timing {
        Timing {
            ticket_life,
            heal_every: Duration::from_millis(30),
            heal_tries,
            lock_wait: Duration::from_millis(150),
        }
    }

    fn fast(dir: &Path, heal_tries: u32) -> (Guard, PathBuf) {
        let file = dir.join("native-preset-reload-x.json");
        let g = Guard::with(
            Some(file.clone()),
            "0123456789abcdef0123456789abcdef",
            timing(heal_tries, Duration::from_secs(30)),
        );
        (g, file)
    }

    fn wait_for(what: &str, cond: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn disk(file: &Path) -> String {
        std::fs::read_to_string(file).unwrap()
    }

    fn gen_of(g: &Guard) -> u64 {
        g.shared.lock().gen
    }

    fn js(text: &str) -> JValue {
        wtf16::parse(text).unwrap()
    }

    #[test]
    fn a_write_that_cannot_take_the_lock_makes_the_baseline_unknown_until_one_succeeds() {
        // Core :330.
        let d = tempfile::tempdir().unwrap();
        let (guard, file) = fast(d.path(), 0);
        let listing = [row("a", "unloaded", "p")];
        assert!(guard.record(Some("text"), &listing, Some("text")));
        assert!(guard.known());
        // A live writer holds the lock past the wait.
        std::fs::write(lock_path(&file), "other").unwrap();
        assert!(!guard.forget());
        assert!(
            disk(&file).contains("text"),
            "the old baseline is still on disk"
        );
        assert!(!guard.known(), "and is not trusted");
        assert_eq!(
            guard.verdict(Some("text"), Some(&listing), &[]).reason,
            "unknown"
        );
        assert_eq!(disk(&lock_path(&file)), "other", "a lock not mine stays");
        std::fs::remove_file(lock_path(&file)).unwrap();
        assert!(
            !guard.known(),
            "the failure is sticky once the lock is free"
        );
        assert!(guard.record(Some("text"), &listing, Some("text")));
        assert!(guard.known(), "a write that succeeds restores it");
    }

    #[test]
    fn heal_writes_the_tombstone_once_the_lock_is_free_and_keeps_it_unknown() {
        let d = tempfile::tempdir().unwrap();
        let (g, file) = fast(d.path(), 200);
        let rows = vec![row("A", "loaded", "p")];
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        std::fs::write(lock_path(&file), "other").unwrap();
        assert!(!g.record(Some(BASE), &rows, Some(BASE)));
        std::fs::remove_file(lock_path(&file)).unwrap();
        wait_for("the heal tombstone", || disk(&file) == "{}");
        // A restart now finds `{}`: the old baseline cannot come back.
        assert!(!guard(d.path()).known());
        // The heal is the failed write's own gen, so it does not lift the failure.
        assert!(!g.known());
        assert!(g.shared.lock().failed.is_some());
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        assert!(g.known());
    }

    type DyingGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
    static DYING_GATE: std::sync::Mutex<Option<DyingGate>> = std::sync::Mutex::new(None);

    fn dying_write(_: &Path, _: &[u8], _: Duration) -> std::io::Result<()> {
        let (entered, release) = DYING_GATE.lock().unwrap().take().unwrap();
        entered.send(()).unwrap();
        release.recv_timeout(Duration::from_secs(120)).unwrap();
        panic!("synthetic writer fault")
    }

    fn slow_write(file: &Path, bytes: &[u8], wait: Duration) -> std::io::Result<()> {
        std::thread::sleep(Duration::from_secs(3));
        write_locked_within(file, bytes, wait)
    }

    #[test]
    fn a_writer_that_dies_answers_not_saved_and_leaves_the_baseline_unknown() {
        // Review P3-1: no waiter may hang on a dead writer (it would wedge the maintenance hold).
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("native-preset-reload-x.json");
        let rows = vec![row("A", "loaded", "p")];
        std::fs::write(
            &file,
            serde_json::to_string(&json!({"text": BASE, "view": engine_view(&rows).unwrap()}))
                .unwrap(),
        )
        .unwrap();
        let (entered, started) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        *DYING_GATE.lock().unwrap() = Some((entered, resume));
        // The normal reply timeout is > 600 s. A separate 120 s watchdog can only
        // observe replies from writer exit, never a successful timeout fallback.
        let mut t = timing(0, Duration::from_secs(30));
        t.lock_wait = Duration::from_secs(600);
        let g = Guard::build(Some(file.clone()), "", t, dying_write);
        assert!(g.known());
        let (done, replies) = std::sync::mpsc::channel();
        let first = {
            let (g, done) = (g.clone(), done.clone());
            std::thread::spawn(move || done.send(g.forget()).unwrap())
        };
        started.recv_timeout(Duration::from_secs(120)).unwrap();
        let second = {
            let (g, done) = (g.clone(), done.clone());
            std::thread::spawn(move || done.send(g.record(Some(BASE), &rows, Some(BASE))).unwrap())
        };
        wait_for("the second write queued behind the blocked writer", || {
            gen_of(&g) == 2
        });
        release.send(()).unwrap();
        for _ in 0..2 {
            assert!(!replies.recv_timeout(Duration::from_secs(120)).unwrap());
        }
        first.join().unwrap();
        second.join().unwrap();
        assert!(g.shared.lock().no_writer);
        assert!(!g.known(), "unknown for the rest of this start");
        // Later writes must also reply without waiting for the normal timeout.
        let later = {
            let g = g.clone();
            std::thread::spawn(move || {
                let forgotten = g.forget();
                let t = g.begin("rust");
                done.send(forgotten || g.settle(&t.ticket, Settle::Forget).unwrap().saved)
                    .unwrap();
            })
        };
        assert!(!replies.recv_timeout(Duration::from_secs(120)).unwrap());
        later.join().unwrap();
        assert!(!g.known());
    }

    #[test]
    fn a_write_that_outlasts_its_wait_counts_as_not_saved() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("native-preset-reload-x.json");
        let g = Guard::build(
            Some(file.clone()),
            "",
            timing(0, Duration::from_secs(30)),
            slow_write,
        );
        let started = Instant::now();
        assert!(!g.forget(), "the waiter gives up");
        assert!(started.elapsed() < Duration::from_millis(2900));
        assert!(!g.known(), "still unknown while the write is in flight");
        wait_for("the slow write to land", || {
            std::fs::read_to_string(&file).is_ok_and(|t| t == "{}")
        });
    }

    #[test]
    fn heal_leaves_alone_a_baseline_another_writer_recorded_since() {
        // Review P3-2: shared mode, a legacy Node records after this process's write failed.
        let d = tempfile::tempdir().unwrap();
        let (g, file) = fast(d.path(), 200);
        let rows = vec![row("A", "loaded", "p")];
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        std::fs::write(lock_path(&file), "node").unwrap();
        assert!(!g.forget());
        // Node, holding the lock, replaces the file with its own new baseline, then lets go.
        let node =
            serde_json::to_string(&json!({"text": "[*]\n", "view": engine_view(&rows).unwrap()}))
                .unwrap();
        std::fs::write(&file, &node).unwrap();
        std::fs::remove_file(lock_path(&file)).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(disk(&file), node, "heal did not overwrite Node's baseline");
        assert!(guard(d.path()).known(), "a fresh start trusts it");
        assert!(!g.known(), "this start's own failure still stands");
    }

    /// Written by Node (Core b91b3f03 `JSON.stringify({text, view: engineView(rows)})`) over these
    /// rows, with lone surrogates in the text and, escaped, in the view.
    const NODE_FILE: &str =
        r#"{"text":"[*]\nx = \ud800\n","view":"[[\"\\ud83d\",\"p\\udc00\"],[\"～\",\"y\"]]"}"#;
    const NODE_ROWS: &str = r#"[{"id":"\ud83d","status":{"value":"loaded","preset":"p\udc00"}},{"id":"～","status":{"value":"unloaded","preset":"y"}}]"#;

    #[test]
    fn a_file_node_wrote_with_lone_surrogates_round_trips_byte_for_byte() {
        // Review P3-4.
        assert_eq!(wtf16::stringify(&js(NODE_FILE)).unwrap(), NODE_FILE);
        let rows = match js(NODE_ROWS) {
            JValue::Arr(rows) => rows,
            _ => Vec::new(),
        };
        let file_view = js(NODE_FILE).get("view").as_str().unwrap().to_owned();
        assert_eq!(
            engine_view_js(&rows).unwrap(),
            file_view,
            "the same view as Node's"
        );
        let d = tempfile::tempdir().unwrap();
        let (g, file) = fast(d.path(), 0);
        std::fs::write(&file, NODE_FILE).unwrap();
        let (_, a) = g.bridge("router-text", &js(&format!(r#"{{"rows":{NODE_ROWS}}}"#)));
        assert_eq!(
            wtf16::stringify(&a).unwrap(),
            r#"{"text":"[*]\nx = \ud800\n"}"#
        );
        // Recording the same through the bridge writes Node's bytes.
        assert!(g.forget());
        let (_, begun) = g.bridge("begin", &js(r#"{"origin":"node"}"#));
        let ticket = begun.get("ticket").as_str().unwrap();
        let text = r#""[*]\nx = \ud800\n""#;
        let settle = format!(
            r#"{{"ticket":"{ticket}","outcome":"record","before":{text},"rows":{NODE_ROWS},"after":{text}}}"#
        );
        assert_eq!(g.bridge("settle", &js(&settle)).0, 200);
        assert_eq!(disk(&file), NODE_FILE);
    }

    #[test]
    fn heal_stops_when_a_newer_generation_arrives() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("native-preset-reload-x.json");
        let g = Guard::with(
            Some(file.clone()),
            "",
            Timing {
                heal_every: Duration::from_millis(400),
                ..timing(200, Duration::from_secs(30))
            },
        );
        let rows = vec![row("A", "loaded", "p")];
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        let recorded = disk(&file);
        std::fs::write(lock_path(&file), "other").unwrap();
        assert!(!g.forget());
        // A reload begins (a newer gen) before the first heal try.
        let t = g.begin("rust");
        std::fs::remove_file(lock_path(&file)).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(disk(&file), recorded, "no heal write after the newer gen");
        assert!(!g.known());
        assert!(
            g.settle(
                &t.ticket,
                Settle::Record {
                    before: Some(BASE.into()),
                    view: engine_view(&rows),
                    after: Some(BASE.into()),
                }
            )
            .unwrap()
            .saved
        );
        assert!(g.known(), "the newer write lifts the failure");
    }

    #[test]
    fn a_pending_write_is_unknown_and_overlapping_writes_keep_call_order() {
        // Core :358 and :373 (#2216): the generation port.
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("native-preset-reload-x.json");
        let g = Guard::with(
            Some(file.clone()),
            "",
            Timing {
                lock_wait: Duration::from_secs(5),
                ..timing(0, Duration::from_secs(30))
            },
        );
        let rows = vec![row("A", "loaded", "p")];
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        let recorded = disk(&file);
        std::fs::write(lock_path(&file), "another-writer").unwrap();
        let a = {
            let (g, rows) = (g.clone(), rows.clone());
            std::thread::spawn(move || g.record(Some(BASE), &rows, Some(BASE)))
        };
        wait_for("the record to be queued", || gen_of(&g) == 2);
        let b = {
            let g = g.clone();
            std::thread::spawn(move || g.forget())
        };
        wait_for("the forget to be queued", || gen_of(&g) == 3);
        assert_eq!(disk(&file), recorded, "the old baseline is still on disk");
        assert!(!g.known(), "a pending write never trusts the old baseline");
        std::fs::remove_file(lock_path(&file)).unwrap();
        assert!(a.join().unwrap());
        assert!(b.join().unwrap());
        assert_eq!(disk(&file), "{}", "the later call wins");
        assert!(!g.known());
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        assert!(g.known());
    }

    fn record_of(rows: &[Value]) -> Settle {
        Settle::Record {
            before: Some(BASE.into()),
            view: engine_view(rows),
            after: Some(BASE.into()),
        }
    }

    #[test]
    fn a_reload_ticket_is_unknown_while_open_and_records_when_settled_alone() {
        let d = tempfile::tempdir().unwrap();
        let (g, file) = fast(d.path(), 0);
        let rows = vec![row("A", "loaded", "p")];
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        let t = g.begin("rust");
        assert_eq!(t.epoch, "0123456789abcdef0123456789abcdef");
        assert!(t.ticket.starts_with("0123456789abcdef0123456789abcdef-"));
        assert!(t.deadline_ms > crate::identity::now_ms());
        assert!(!g.known(), "a reload in flight is unknown");
        assert_eq!(g.router_text(&rows), None);
        let s = g.settle(&t.ticket, record_of(&rows)).unwrap();
        assert!(s.saved);
        assert!(s.gen > t.gen);
        assert!(g.known());
        assert!(disk(&file).contains("\"view\""));
        // A ticket settles once.
        assert!(g.settle(&t.ticket, record_of(&rows)).is_err());
        assert_eq!(disk(&file), "{}");
    }

    #[test]
    fn overlapping_tickets_are_forced_to_forget() {
        let d = tempfile::tempdir().unwrap();
        let (g, file) = fast(d.path(), 0);
        let rows = vec![row("A", "loaded", "p")];
        let t1 = g.begin("rust");
        let t2 = g.begin("rust");
        assert!(g.settle(&t1.ticket, record_of(&rows)).unwrap().saved);
        assert_eq!(disk(&file), "{}");
        assert!(!g.known());
        assert!(g.settle(&t2.ticket, record_of(&rows)).unwrap().saved);
        assert_eq!(disk(&file), "{}");
        assert!(!g.known());
        // Alone again, a reload records.
        let t3 = g.begin("rust");
        assert!(g.settle(&t3.ticket, record_of(&rows)).unwrap().saved);
        assert!(g.known());
    }

    #[test]
    fn a_change_asked_for_during_a_reload_downgrades_its_record_to_forget() {
        let d = tempfile::tempdir().unwrap();
        let (g, file) = fast(d.path(), 0);
        let rows = vec![row("A", "loaded", "p")];
        let t = g.begin("rust");
        assert!(g.invalidate("calibration").saved);
        assert!(g.settle(&t.ticket, record_of(&rows)).unwrap().saved);
        assert_eq!(disk(&file), "{}");
        assert!(!g.known());
    }

    #[test]
    fn an_expired_ticket_is_unknown_and_writes_a_tombstone() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("native-preset-reload-x.json");
        let g = Guard::with(
            Some(file.clone()),
            "",
            timing(0, Duration::from_millis(150)),
        );
        let rows = vec![row("A", "loaded", "p")];
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        let t = g.begin("node");
        // Nobody reads: the writer itself ends the ticket at its deadline (Node died mid-reload).
        wait_for("the expiry tombstone", || disk(&file) == "{}");
        assert!(!g.known());
        assert!(g.shared.lock().open.is_empty());
        let late = g.settle(&t.ticket, record_of(&rows));
        assert!(late.is_err(), "a late settle is stale");
        assert_eq!(disk(&file), "{}");
        assert!(!g.known());
    }

    #[test]
    fn a_stale_settle_forgets() {
        let d = tempfile::tempdir().unwrap();
        let (g, file) = fast(d.path(), 0);
        let rows = vec![row("A", "loaded", "p")];
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        let s = g.settle("no-such-ticket", record_of(&rows)).unwrap_err();
        assert!(s.saved);
        assert_eq!(disk(&file), "{}");
        assert!(!g.known());
    }

    #[test]
    fn a_new_node_session_lifts_only_a_failure_node_caused() {
        let d = tempfile::tempdir().unwrap();
        let (g, file) = fast(d.path(), 0);
        let rows = vec![row("A", "loaded", "p")];
        assert_eq!(
            g.bridge("session", &js(r#"{"nodeBoot":"boot-1","protocol":1}"#))
                .0,
            200
        );
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        std::fs::write(lock_path(&file), "other").unwrap();
        let (status, answer) = g.bridge("invalidate", &js(r#"{"origin":"calibration"}"#));
        assert_eq!(status, 200);
        assert_eq!(answer.get("saved"), &JValue::Bool(false));
        std::fs::remove_file(lock_path(&file)).unwrap();
        assert!(!g.known());
        g.session("boot-1");
        assert!(!g.known(), "the same session keeps its failure");
        g.session("boot-2");
        assert!(g.known(), "a Node restart lifts the failure Node caused");
        // A failure of the front's own is lifted only by a later write (or a front restart).
        std::fs::write(lock_path(&file), "other").unwrap();
        assert!(!g.forget());
        std::fs::remove_file(lock_path(&file)).unwrap();
        g.session("boot-3");
        assert!(!g.known());
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        assert!(g.known());
    }

    #[test]
    fn a_new_node_session_ends_the_old_sessions_tickets() {
        let d = tempfile::tempdir().unwrap();
        let (g, file) = fast(d.path(), 0);
        let rows = vec![row("A", "loaded", "p")];
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        g.session("boot-1");
        let (_, begun) = g.bridge("begin", &js(r#"{"origin":"node"}"#));
        let ticket = begun.get("ticket").as_str().unwrap().to_owned();
        g.session("boot-2");
        wait_for("the old ticket's tombstone", || disk(&file) == "{}");
        assert!(g.shared.lock().open.is_empty());
        assert!(g.settle(&ticket, record_of(&rows)).is_err());
    }

    #[test]
    fn shared_mode_sees_what_a_legacy_node_writes_itself() {
        let d = tempfile::tempdir().unwrap();
        let (g, file) = fast(d.path(), 0);
        assert!(!g.bridged(), "shared mode until a protocol-1 session");
        let rows = vec![row("A", "loaded", "p")];
        // A legacy Node writes the file itself (JSON.stringify({text, view})).
        std::fs::write(
            &file,
            serde_json::to_string(&json!({"text": BASE, "view": engine_view(&rows).unwrap()}))
                .unwrap(),
        )
        .unwrap();
        assert!(g.known());
        assert!(g.verdict(Some(BASE), Some(&rows), &["A".into()]).safe);
        assert_eq!(g.router_text(&rows).as_deref(), Some(BASE));
        std::fs::write(&file, "{}").unwrap();
        assert!(!g.known(), "and its tombstone");
        g.session("boot-1");
        assert!(g.bridged());
    }

    #[test]
    fn the_bridge_answers_in_cores_shapes() {
        let d = tempfile::tempdir().unwrap();
        let (g, file) = fast(d.path(), 0);
        let rows = r#"[{"id":"A","status":{"value":"loaded","preset":"p"}}]"#;
        let (s, a) = g.bridge("session", &js(r#"{"nodeBoot":"b","protocol":1}"#));
        assert_eq!(s, 200);
        assert_eq!(
            wtf16::stringify(&a).unwrap(),
            r#"{"epoch":"0123456789abcdef0123456789abcdef","protocol":1}"#
        );
        assert_eq!(
            g.bridge("session", &js(r#"{"nodeBoot":"b","protocol":2}"#))
                .0,
            400
        );
        assert_eq!(g.bridge("session", &js(r#"{"protocol":1}"#)).0, 400);
        for bad in [
            r#"{}"#,
            r#"{"origin":"rust"}"#,
            r#"{"origin":"Expired"}"#,
            r#"{"origin":1}"#,
        ] {
            let (s, a) = g.bridge("begin", &js(bad));
            assert_eq!(
                (s, a.get("code").as_str()),
                (400, Some("bad_origin")),
                "{bad}"
            );
        }
        let (s, begun) = g.bridge("begin", &js(r#"{"origin":"node"}"#));
        assert_eq!(s, 200);
        let keys: Vec<&str> = match &begun {
            JValue::Obj(items) => items.iter().map(|(k, _)| k.as_str()).collect(),
            _ => Vec::new(),
        };
        assert_eq!(keys, ["ticket", "gen", "epoch", "deadlineMs"]);
        let ticket = begun.get("ticket").as_str().unwrap();
        let base = wtf16::stringify(&JValue::from(BASE)).unwrap();
        let settle = format!(
            r#"{{"ticket":"{ticket}","outcome":"record","before":{base},"rows":{rows},"after":{base}}}"#
        );
        let (s, a) = g.bridge("settle", &js(&settle));
        assert_eq!(s, 200);
        assert_eq!(a.get("saved"), &JValue::Bool(true));
        assert!(g.known());
        assert_eq!(
            disk(&file),
            serde_json::to_string(&json!({"text": BASE, "view": r#"[["A","p"]]"#})).unwrap()
        );
        // A second settle of the same ticket: 409 stale_ticket, and forgotten.
        let (s, a) = g.bridge("settle", &js(&settle));
        assert_eq!(s, 409);
        assert_eq!(a.get("code").as_str(), Some("stale_ticket"));
        assert_eq!(a.get("saved"), &JValue::Bool(true));
        assert_eq!(disk(&file), "{}");
        assert_eq!(
            g.bridge("settle", &js(r#"{"ticket":"x","outcome":"maybe"}"#))
                .0,
            400
        );
        // Record again, then the reads.
        let (_, begun) = g.bridge("begin", &js(r#"{"origin":"node"}"#));
        let ticket = begun.get("ticket").as_str().unwrap();
        let settle = format!(
            r#"{{"ticket":"{ticket}","outcome":"record","before":{base},"rows":{rows},"after":{base}}}"#
        );
        assert_eq!(g.bridge("settle", &js(&settle)).0, 200);
        let (s, a) = g.bridge("router-text", &js(&format!(r#"{{"rows":{rows}}}"#)));
        assert_eq!(s, 200);
        assert_eq!(a.get("text").as_str(), Some(BASE));
        let (_, a) = g.bridge("router-text", &js(r#"{"rows":"not rows"}"#));
        assert_eq!(a.get("text"), &JValue::Null);
        let verdict = |current: &str| {
            let body = format!(r#"{{"current":{current},"rows":{rows},"loaded":["A"]}}"#);
            wtf16::stringify(&g.bridge("verdict", &js(&body)).1).unwrap()
        };
        assert_eq!(verdict(&base), r#"{"safe":true,"reason":"unchanged"}"#);
        let changed =
            wtf16::stringify(&JValue::from(BASE.replace("a.gguf", "b.gguf").as_str())).unwrap();
        assert_eq!(
            verdict(&changed),
            r#"{"safe":false,"reason":"changed","changed":["A"]}"#
        );
        assert_eq!(verdict("null"), r#"{"safe":false,"reason":"unreadable"}"#);
        let (_, a) = g.bridge(
            "verdict",
            &js(r#"{"current":"x","rows":[{"id":"A","status":{"value":"loaded","preset":"moved"}}],"loaded":["A"]}"#),
        );
        assert_eq!(
            wtf16::stringify(&a).unwrap(),
            r#"{"safe":false,"reason":"engine_moved"}"#
        );
        let (_, a) = g.bridge("verdict", &js(r#"{"current":"x","loaded":["A"]}"#));
        assert_eq!(a.get("reason").as_str(), Some("unknown"));
        assert_eq!(
            g.bridge("verdict", &js(r#"{"current":"x","rows":[],"loaded":[1]}"#))
                .0,
            400
        );
        let (s, a) = g.bridge("invalidate", &js(r#"{"origin":"autotune"}"#));
        assert_eq!(s, 200);
        assert_eq!(a.get("saved"), &JValue::Bool(true));
        assert!(!g.known());
        assert_eq!(g.bridge("nope", &js("{}")).0, 404);
    }

    #[test]
    fn the_file_is_byte_identical_to_cores_and_leaves_nothing_behind() {
        // Core :293 (#2210).
        let d = tempfile::tempdir().unwrap();
        let (g, file) = fast(d.path(), 0);
        let rows = vec![row("A", "loaded", "p\"q\n\u{1}")];
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        let view = engine_view(&rows).unwrap();
        assert_eq!(
            disk(&file),
            serde_json::to_string(&json!({"text": BASE, "view": view})).unwrap()
        );
        assert_eq!(
            disk(&file),
            format!("{{\"text\":{},\"view\":{}}}", json!(BASE), json!(view))
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let names: Vec<String> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["native-preset-reload-x.json"]);
    }

    #[test]
    fn what_one_process_records_or_forgets_the_other_sees() {
        // Core :279 (#2210): two handles on the file are two processes.
        let d = tempfile::tempdir().unwrap();
        let (mine, _) = fast(d.path(), 0);
        let (theirs, _) = fast(d.path(), 0);
        let rows = vec![row("A", "loaded", "p")];
        assert!(!mine.known());
        assert!(theirs.record(Some(BASE), &rows, Some(BASE)));
        assert!(mine.known());
        assert!(mine.verdict(Some(BASE), Some(&rows), &["A".into()]).safe);
        assert_eq!(mine.router_text(&rows).as_deref(), Some(BASE));
        assert!(mine.forget());
        assert!(!theirs.known());
        assert_eq!(
            theirs
                .verdict(Some(BASE), Some(&rows), &["A".into()])
                .reason,
            "unknown"
        );
    }

    #[test]
    fn router_text_ignores_load_state_and_is_null_after_a_re_read_or_forget() {
        // Core :266 (#1863).
        let d = tempfile::tempdir().unwrap();
        let (g, _) = fast(d.path(), 0);
        let text = "[*]\nsleep-idle-seconds = 1800\n";
        let idle = vec![row("A", "unloaded", "[A]"), row("B", "unloaded", "[B]")];
        assert_eq!(g.router_text(&idle), None, "unknown before any reload");
        assert!(g.record(Some(text), &idle, Some(text)));
        let busy = vec![row("A", "loading", "[A]"), row("B", "sleeping", "[B]")];
        assert_eq!(g.router_text(&busy).as_deref(), Some(text));
        let moved = vec![
            row("A", "unloaded", "[A]\nctx-size = 1\n"),
            row("B", "unloaded", "[B]"),
        ];
        assert_eq!(g.router_text(&moved), None);
        assert!(g.forget());
        assert_eq!(g.router_text(&idle), None);
    }

    #[test]
    fn the_view_sees_order_adds_removes_and_preset_changes() {
        // Core :73.
        let r = vec![row("A", "loaded", "[A]"), row("B", "unloaded", "[B]")];
        let reversed: Vec<Value> = r.iter().rev().cloned().collect();
        assert_eq!(engine_view(&r), engine_view(&reversed));
        assert_ne!(engine_view(&r), engine_view(&r[1..]));
        let mut added = r.clone();
        added.push(row("C", "unloaded", "[C]"));
        assert_ne!(engine_view(&r), engine_view(&added));
        let changed = vec![
            row("A", "loaded", "[A]"),
            row("B", "unloaded", "[B]\nx = 1"),
        ];
        assert_ne!(engine_view(&r), engine_view(&changed));
    }

    #[test]
    fn the_view_of_nodes_rows_is_the_same_and_keeps_lone_surrogates() {
        let r = vec![row("b", "loaded", "pb"), row("a", "loaded", "p\"a\n")];
        let js_rows: Vec<JValue> = r
            .iter()
            .map(|v| wtf16::parse(&v.to_string()).unwrap())
            .collect();
        assert_eq!(engine_view_js(&js_rows), engine_view(&r));
        // A lone high surrogate sorts as its one unit (0xD83D, before U+FF5E) and is written as JS
        // writes it.
        let rows = wtf16::parse(
            r#"[{"id":"\ud83d","status":{"preset":"x"}},{"id":"\uff5e","status":{"preset":"y"}}]"#,
        )
        .unwrap();
        let rows = match rows {
            JValue::Arr(rows) => rows,
            _ => Vec::new(),
        };
        assert_eq!(
            engine_view_js(&rows).unwrap(),
            r#"[["\ud83d","x"],["～","y"]]"#
        );
        assert!(engine_view_js(&[js(r#"{"id":"a","status":{}}"#)]).is_none());
        // Such a character in a serde row is not a lone surrogate: unknown.
        assert!(engine_view(&[row("\u{10F800}", "loaded", "x")]).is_none());
    }

    #[test]
    fn the_verdict_names_the_changed_models() {
        // Core :28: `changed` ids on a changed verdict, none otherwise.
        let d = tempfile::tempdir().unwrap();
        let (g, _) = fast(d.path(), 0);
        let rows = vec![row("A", "loaded", "p")];
        let loaded = vec!["A".to_owned()];
        let v = g.verdict(Some(BASE), Some(&rows), &loaded);
        assert_eq!((v.reason.as_str(), v.changed.len()), ("unknown", 0));
        assert_eq!(
            wtf16::stringify(&v.to_js()).unwrap(),
            r#"{"safe":false,"reason":"unknown"}"#
        );
        assert!(g.record(Some(BASE), &rows, Some(BASE)));
        let v = g.verdict(
            Some(&BASE.replace("a.gguf", "b.gguf")),
            Some(&rows),
            &loaded,
        );
        assert_eq!(v.changed, ["A"]);
        let ok = g.verdict(Some(BASE), Some(&rows), &loaded);
        assert!(ok.safe && ok.changed.is_empty());
        assert_eq!(
            wtf16::stringify(&ok.to_js()).unwrap(),
            r#"{"safe":true,"reason":"unchanged"}"#
        );
    }

    fn row(id: &str, status: &str, preset: &str) -> Value {
        json!({"id": id, "status": {"value": status, "preset": preset}})
    }

    #[test]
    fn live_models_are_loaded_loading_and_sleeping() {
        let r = vec![
            row("a", "loaded", "x"),
            row("b", "unloaded", "x"),
            row("c", "sleeping", "x"),
            row("d", "loading", "x"),
            json!({"id": "e"}),
            Value::Null,
        ];
        assert_eq!(live_ids(&r), ["a", "c", "d"]);
    }

    #[test]
    fn the_view_is_js_json_of_the_sorted_pairs_and_unknown_on_a_gap() {
        let r = vec![row("b", "loaded", "pb"), row("a", "loaded", "p\"a\n")];
        assert_eq!(engine_view(&r).unwrap(), r#"[["a","p\"a\n"],["b","pb"]]"#);
        assert_eq!(engine_view(&[]).unwrap(), "[]");
        // A later row for the same id wins, as in the JS object.
        let dup = vec![row("a", "loaded", "1"), row("a", "loaded", "2")];
        assert_eq!(engine_view(&dup).unwrap(), r#"[["a","2"]]"#);
        // UTF-16 order, not UTF-8: U+FF5E sorts after U+1F600 in UTF-8, before it in UTF-16.
        let order = vec![
            row("\u{1F600}", "loaded", "x"),
            row("\u{FF5E}", "loaded", "y"),
        ];
        assert!(engine_view(&order).unwrap().starts_with("[[\"\u{1F600}\""));
        assert!(engine_view(&[json!({"id": "a", "status": {}})]).is_none());
        assert!(engine_view(&[json!({"id": 1, "status": {"preset": "x"}})]).is_none());
    }

    const BASE: &str = "[*]\ncache-ram = 512\n\n[A]\nmodel = /models/a.gguf\n";

    fn guard(dir: &Path) -> Guard {
        Guard::new(Some(dir.join("native-preset-reload-x.json")))
    }

    #[test]
    fn a_recorded_baseline_is_read_back_and_judges_an_unchanged_file_safe() {
        let d = tempfile::tempdir().unwrap();
        let g = guard(d.path());
        let rows = vec![row("A", "loaded", "p")];
        assert!(!g.known());
        g.record(Some(BASE), &rows, Some(BASE));
        assert!(g.known());
        // Another handle on the same file (the other process) sees it: nothing is cached.
        assert!(guard(d.path()).known());
        let loaded = vec!["A".to_owned()];
        assert!(g.verdict(Some(BASE), Some(&rows), &loaded).safe);
        let changed = BASE.replace("a.gguf", "b.gguf");
        let v = g.verdict(Some(&changed), Some(&rows), &loaded);
        assert_eq!((v.safe, v.reason.as_str()), (false, "changed"));
        let moved = vec![row("A", "loaded", "other")];
        assert_eq!(
            g.verdict(Some(BASE), Some(&moved), &loaded).reason,
            "engine_moved"
        );
        assert_eq!(g.verdict(None, Some(&rows), &loaded).reason, "unreadable");
        assert_eq!(g.verdict(Some(BASE), None, &loaded).reason, "unknown");
        assert_eq!(g.router_text(&rows).as_deref(), Some(BASE));
        assert_eq!(g.router_text(&moved), None);
    }

    #[test]
    fn a_file_that_moved_during_the_reload_forgets_the_baseline() {
        let d = tempfile::tempdir().unwrap();
        let g = guard(d.path());
        let rows = vec![row("A", "loaded", "p")];
        g.record(Some(BASE), &rows, Some(BASE));
        g.record(Some(BASE), &rows, Some("[A]\n"));
        assert!(!g.known());
        // The tombstone is `{}`, as Core writes it, and mode 0600.
        let file = d.path().join("native-preset-reload-x.json");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{}");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        g.record(None, &rows, None);
        assert!(!g.known());
    }

    #[test]
    fn a_file_core_wrote_is_the_same_format() {
        let d = tempfile::tempdir().unwrap();
        let g = guard(d.path());
        let rows = vec![row("A", "loaded", "p")];
        let view = engine_view(&rows).unwrap();
        std::fs::write(
            d.path().join("native-preset-reload-x.json"),
            serde_json::to_string(&json!({"text": BASE, "view": view})).unwrap(),
        )
        .unwrap();
        assert!(g.verdict(Some(BASE), Some(&rows), &["A".into()]).safe);
        // Anything else is unknown.
        for junk in ["", "not json", "[]", r#"{"text":1,"view":"x"}"#] {
            std::fs::write(d.path().join("native-preset-reload-x.json"), junk).unwrap();
            assert!(!g.known(), "{junk:?}");
        }
    }

    #[test]
    fn a_stale_lock_is_taken_over_and_a_live_one_is_waited_for() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("s.json");
        let lock = lock_path(&file);
        std::fs::write(&lock, "999999").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(120))
            .unwrap();
        write_locked(&file, b"{}").unwrap();
        assert!(!lock.exists(), "the lock is released");
        // A live lock released by its holder lets the write through.
        std::fs::write(&lock, "1").unwrap();
        let l2 = lock.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            std::fs::remove_file(l2).unwrap();
        });
        write_locked(&file, b"{\"a\":1}").unwrap();
        t.join().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"a\":1}");
    }
}
