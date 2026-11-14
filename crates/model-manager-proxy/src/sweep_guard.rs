//! noevia-core `server/benchmark-sweep-guard.cjs` (#873, #895, #2187, row J): the throughput sweep's
//! hold on the inference maintenance gate, as a state machine without I/O.
//!
//! A sweep runs llama-bench in its own container, so nothing in the web process sees its memory.
//! The guard keeps the maintenance hold from the moment the sweep is accepted until the model
//! manager reports the job is no longer active (`watch`), and after a web restart takes the hold
//! back for a sweep that is still running (`adopt`). Today core runs this guard and the front
//! relays `POST /api/model-manager/benchmark/sweep` to core (bins/noevia-server
//! model_manager_routes.rs); this module is what lets the front keep the sweep itself later.
//! Nothing calls it yet.
//!
//! The machine never waits, sleeps or calls out. Each method returns the [`Effect`]s Node's code
//! would perform at that point, in Node's order; the host performs them and feeds the answers back
//! ([`Guard::progress_answered`], [`Guard::hold_answered`], [`Guard::timer_fired`]). Calls are
//! answered in the order they were asked, as Node's `await`s are.
//!
//! Read and kept from Node, on purpose:
//! - Only a terminal acknowledgement releases the hold: a progress outage, a thrown call, a job
//!   that is `null` while watching, a local deadline: none of them ends the watch.
//! - `cancel_adopt` (shutdown) does not lift a restart barrier that `adopt` raised, and the
//!   cancelled flag is never cleared: a later `adopt` settles `false` at once, barrier raised.
//! - A thrown `Number(job.total)` (a `total` object with an own `toString`) rejects `adopt` with
//!   the barrier still up.
//!
//! Node options read by nothing that decides: `perModelMs`, `slackMs`, `unreachableMs`,
//! `adoptAttempts` and `now` feed a deadline, a bound and a last-seen time that no branch reads
//! (the "retain until terminal" fixes left them behind), and `finish(why)` is only ever called
//! with `''`, so its "chat resumed" line is never logged. They have no counterpart here; the
//! recorded scenarios vary them to show they change nothing.
//!
//! Named difference: Node lets two `adopt()` calls run side by side (each with its own barrier);
//! core calls it once at start. Here a second [`Guard::adopt`] while one runs settles `false`
//! at once and raises nothing.

use js_json::JValue;

use crate::js::{strict_eq, to_number};
use crate::{SWEEP_BUSY_ERROR, SWEEP_UNAVAILABLE_ERROR};

/// `pollMs`: how often a watched sweep's progress is asked for.
pub const POLL_MS: u64 = 5000;
/// `adoptRetryMs`: the first wait between `adopt`'s attempts.
pub const ADOPT_RETRY_MS: u64 = 2000;
/// `adoptMaxDelayMs`: the doubling wait's cap.
pub const ADOPT_MAX_DELAY_MS: u64 = 60 * 1000;

/// The lines Node logs.
pub const LOG_LEASE_LOST: &str = "[models] throughput sweep: the maintenance lease was lost; chat stays paused until the sweep is over and the model server is checked.";
pub const LOG_AUTHORITY_DOWN: &str = "[models] throughput sweep still running after a restart and the lease authority is not answering: chat paused while the hold is retried.";
pub const LOG_FINISHED_WHILE_PAUSED: &str =
    "[models] throughput sweep: finished while chat was paused for it.";
pub const LOG_ADOPTED: &str =
    "[models] throughput sweep still running after a restart: chat paused until it finishes.";

/// `createSweepGuard`'s timing options and whether a `barrier` was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub poll_ms: u64,
    pub adopt_retry_ms: u64,
    pub adopt_max_delay_ms: u64,
    /// `typeof barrier === 'function'`: `adopt` closes chat while the restart's state is unknown.
    pub barrier: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            poll_ms: POLL_MS,
            adopt_retry_ms: ADOPT_RETRY_MS,
            adopt_max_delay_ms: ADOPT_MAX_DELAY_MS,
            barrier: true,
        }
    }
}

/// One watch (`watch(release, models)`'s entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WatchId(pub u64);

/// A timer the guard keeps; at most one of each is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Timer {
    /// A watch's next progress poll.
    Watch(WatchId),
    /// `adopt`'s wait before its next attempt.
    Adopt,
}

/// Why a `progress()` call was made (the host answers it with the same value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Call {
    /// A watch's poll.
    Watch(WatchId),
    /// `adopt`'s first loop: is a sweep running at all?
    AdoptProbe,
    /// `adopt`'s second loop, after a refused hold: is the sweep still running?
    AdoptCheck,
}

/// What `adopt()`'s promise did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    Resolved(bool),
    /// It threw (`Number(job.total)` on an object with an own `toString`).
    Rejected,
}

/// One thing Node's guard does, for the host to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// `await progress()` (the model manager's benchmark progress); answer with
    /// [`Guard::progress_answered`].
    Progress(Call),
    /// `adopt`'s `acquire({ recovery: true })`: `hold(SWEEP_PAUSE_REASON, { recovery: true })`;
    /// answer with [`Guard::hold_answered`].
    Hold,
    /// `barrier(SWEEP_PAUSE_REASON)`: chat closed while the restart's sweep state is unknown.
    Barrier,
    /// The barrier's lift.
    Lift,
    /// `setTimer(fn, ms)` (the host's timers need not keep the process alive: Node unrefs them).
    SetTimer(Timer, u64),
    /// `clearTimer(handle)` of the pending timer.
    ClearTimer(Timer),
    /// The watch's `release()`: the sweep is over (or the watch was stopped).
    Release(WatchId),
    /// `release()` of the hold `adopt` took and did not hand to a watch.
    ReleaseAdopted,
    /// The hold `adopt` took now belongs to this watch (no Node counterpart: bookkeeping for the
    /// host, which then gets `Release(id)` for it).
    Adopted(WatchId),
    Log(&'static str),
    /// `adopt()` settled.
    AdoptSettled(Settled),
}

/// A `progress()` call's outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// It resolved to this value (`null` included).
    Value(JValue),
    /// It threw or rejected.
    Threw,
}

/// A hold `adopt` asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldAnswer {
    /// Granted; `lost` when its lease signal is already aborted.
    Granted { lost: bool },
    /// Refused or unreachable (every refusal is waited out the same way).
    Refused,
}

/// The error `hold()` threw or rejected with, as `acquire` reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub status: JValue,
    pub code: JValue,
    /// `e.fromGate` (the inference gate refused).
    pub from_gate: JValue,
    /// `e.chatActive` (this process's chat is running).
    pub chat_active: JValue,
}

impl Default for Refusal {
    /// Every field `undefined`.
    fn default() -> Self {
        Refusal {
            status: JValue::Undefined,
            code: JValue::Undefined,
            from_gate: JValue::Undefined,
            chat_active: JValue::Undefined,
        }
    }
}

/// The error `acquire` throws for a refused hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    pub status: u16,
    /// `e.code` (only the unavailable authority has one).
    pub code: Option<&'static str>,
    /// `e.publicMessage` (also `e.message`).
    pub public_message: &'static str,
    /// `e.waitable`: a refusal that ends on its own.
    pub waitable: bool,
    /// `e.unavailable === true`.
    pub unavailable: bool,
}

/// `acquire`'s `failure(e)`: the lease authority not answering keeps its 503; anything else is
/// the busy 409, waitable when the inference gate refused, this process's chat runs, or the gate
/// did not answer (502).
pub fn acquire_failure(e: &Refusal) -> Failure {
    let unavailable = strict_eq(&e.status, &JValue::Num(503.0))
        && strict_eq(&e.code, &JValue::from("maintenanceAuthorityUnavailable"));
    if unavailable {
        return Failure {
            status: 503,
            code: Some("maintenanceAuthorityUnavailable"),
            public_message: SWEEP_UNAVAILABLE_ERROR,
            waitable: true,
            unavailable: true,
        };
    }
    let waitable = strict_eq(&e.from_gate, &JValue::Bool(true))
        || strict_eq(&e.chat_active, &JValue::Bool(true))
        || strict_eq(&e.status, &JValue::Num(502.0));
    Failure {
        status: 409,
        code: None,
        public_message: SWEEP_BUSY_ERROR,
        waitable,
        unavailable: false,
    }
}

/// `r?.ok && r.body && typeof r.body === 'object'`: a definitive answer, and then
/// `r.body.job || null` (`JValue::Null` for no job).
fn definitive(answer: &Answer) -> Option<JValue> {
    let Answer::Value(r) = answer else {
        return None;
    };
    let body = r.get("body");
    if !(r.get("ok").truthy() && body.truthy() && body.type_of() == "object") {
        return None;
    }
    let job = body.get("job");
    Some(if job.truthy() {
        job.clone()
    } else {
        JValue::Null
    })
}

/// `job.active !== true` (for a truthy job).
fn inactive(job: &JValue) -> bool {
    !strict_eq(job.get("active"), &JValue::Bool(true))
}

#[derive(Debug)]
struct Watch {
    id: WatchId,
    /// A poll timer is pending (Node's `timer`).
    timer: bool,
    done: bool,
    /// The lease-lost line was logged (Node's listener is `{ once: true }` and outlives `finish`).
    lost_logged: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// First loop: a progress call is out.
    Probe { delay: u64 },
    /// First loop: waiting `delay` before the next probe.
    ProbePause { next: u64 },
    /// Second loop: the hold was asked for.
    Hold { delay: u64 },
    /// Second loop: after a refusal, a progress call is out.
    Check { delay: u64 },
    /// Second loop: waiting before the next hold.
    CheckPause { next: u64 },
}

#[derive(Debug)]
struct Adopt {
    phase: Phase,
    /// `unbar` is set (a barrier is up).
    unbar: bool,
}

/// The guard (`createSweepGuard`'s closure state).
#[derive(Debug)]
pub struct Guard {
    options: Options,
    watches: Vec<Watch>,
    watching: Option<WatchId>,
    next_id: u64,
    adopt: Option<Adopt>,
    adopt_cancelled: bool,
}

impl Guard {
    pub fn new(options: Options) -> Self {
        Guard {
            options,
            watches: Vec::new(),
            watching: None,
            next_id: 1,
            adopt: None,
            adopt_cancelled: false,
        }
    }

    /// `active()`: a sweep is watched.
    pub fn active(&self) -> bool {
        self.watching.is_some()
    }

    /// The watch that `active()` reports.
    pub fn watching(&self) -> Option<WatchId> {
        self.watching
    }

    /// `adopt()` is still running.
    pub fn adopting(&self) -> bool {
        self.adopt.is_some()
    }

    /// `watch(release, models)`: keep the hold the host took until the model manager says the
    /// sweep is over. `lost` when the hold's lease signal is already aborted. (`models` bounds
    /// nothing in Node any more, so it is not taken.)
    pub fn watch(&mut self, lost: bool) -> (WatchId, Vec<Effect>) {
        let mut out = Vec::new();
        let id = self.start_watch(lost, &mut out);
        (id, out)
    }

    fn start_watch(&mut self, lost: bool, out: &mut Vec<Effect>) -> WatchId {
        let id = WatchId(self.next_id);
        self.next_id += 1;
        if lost {
            out.push(Effect::Log(LOG_LEASE_LOST));
        }
        self.watches.push(Watch {
            id,
            timer: true,
            done: false,
            lost_logged: lost,
        });
        self.watching = Some(id);
        out.push(Effect::SetTimer(Timer::Watch(id), self.options.poll_ms));
        id
    }

    /// The watch's lease signal aborted: log once (also after the watch finished, as Node's
    /// listener stays). The hold is kept.
    pub fn lease_lost(&mut self, id: WatchId) -> Vec<Effect> {
        match self.watches.iter_mut().find(|w| w.id == id) {
            Some(w) if !w.lost_logged => {
                w.lost_logged = true;
                vec![Effect::Log(LOG_LEASE_LOST)]
            }
            _ => Vec::new(),
        }
    }

    /// The entry's `stop()`: finish the watch now.
    pub fn stop(&mut self, id: WatchId) -> Vec<Effect> {
        let mut out = Vec::new();
        self.finish(id, &mut out);
        out
    }

    fn finish(&mut self, id: WatchId, out: &mut Vec<Effect>) {
        let Some(w) = self.watches.iter_mut().find(|w| w.id == id) else {
            return;
        };
        if w.done {
            return;
        }
        w.done = true;
        if w.timer {
            w.timer = false;
            out.push(Effect::ClearTimer(Timer::Watch(id)));
        }
        if self.watching == Some(id) {
            self.watching = None;
        }
        out.push(Effect::Release(id));
    }

    /// `adopt()`: after a restart, ask the model manager until it answers and take the hold back
    /// for a sweep still running.
    pub fn adopt(&mut self) -> Vec<Effect> {
        if self.adopt.is_some() {
            return vec![Effect::AdoptSettled(Settled::Resolved(false))];
        }
        let mut out = Vec::new();
        let unbar = self.options.barrier;
        if unbar {
            out.push(Effect::Barrier);
        }
        self.adopt = Some(Adopt {
            phase: Phase::Probe { delay: 0 },
            unbar,
        });
        self.probe(self.options.adopt_retry_ms, &mut out);
        out
    }

    /// `cancelAdopt()`: a pending wait ends at once and `adopt` settles `false`; a call that is out
    /// is let finish. The barrier stays.
    pub fn cancel_adopt(&mut self) -> Vec<Effect> {
        self.adopt_cancelled = true;
        let mut out = Vec::new();
        let pausing = matches!(
            self.adopt.as_ref().map(|a| a.phase),
            Some(Phase::ProbePause { .. } | Phase::CheckPause { .. })
        );
        if pausing {
            out.push(Effect::ClearTimer(Timer::Adopt));
            // The woken loop sees the flag at its top.
            self.settle(Settled::Resolved(false), &mut out);
        }
        out
    }

    fn settle(&mut self, how: Settled, out: &mut Vec<Effect>) {
        self.adopt = None;
        out.push(Effect::AdoptSettled(how));
    }

    fn lift(&mut self, out: &mut Vec<Effect>) {
        if let Some(a) = self.adopt.as_mut() {
            if a.unbar {
                a.unbar = false;
                out.push(Effect::Lift);
            }
        }
    }

    /// The first loop's top.
    fn probe(&mut self, delay: u64, out: &mut Vec<Effect>) {
        if self.adopt_cancelled {
            return self.settle(Settled::Resolved(false), out);
        }
        if let Some(a) = self.adopt.as_mut() {
            a.phase = Phase::Probe { delay };
        }
        out.push(Effect::Progress(Call::AdoptProbe));
    }

    /// The second loop's top.
    fn try_hold(&mut self, delay: u64, out: &mut Vec<Effect>) {
        if self.adopt_cancelled {
            return self.settle(Settled::Resolved(false), out);
        }
        if self.watching.is_some() {
            self.lift(out);
            return self.settle(Settled::Resolved(false), out);
        }
        if let Some(a) = self.adopt.as_mut() {
            a.phase = Phase::Hold { delay };
        }
        out.push(Effect::Hold);
    }

    fn next_delay(&self, delay: u64) -> u64 {
        delay.saturating_mul(2).min(self.options.adopt_max_delay_ms)
    }

    /// A pending timer fired.
    pub fn timer_fired(&mut self, timer: Timer) -> Vec<Effect> {
        let mut out = Vec::new();
        match timer {
            Timer::Watch(id) => {
                let Some(w) = self.watches.iter_mut().find(|w| w.id == id) else {
                    return out;
                };
                if !w.timer {
                    return out;
                }
                w.timer = false;
                if !w.done {
                    out.push(Effect::Progress(Call::Watch(id)));
                }
            }
            Timer::Adopt => match self.adopt.as_ref().map(|a| a.phase) {
                Some(Phase::ProbePause { next }) => self.probe(next, &mut out),
                Some(Phase::CheckPause { next }) => self.try_hold(next, &mut out),
                _ => {}
            },
        }
        out
    }

    /// The answer to an [`Effect::Progress`] call.
    pub fn progress_answered(&mut self, call: Call, answer: &Answer) -> Vec<Effect> {
        let mut out = Vec::new();
        match call {
            Call::Watch(id) => self.watch_answer(id, answer, &mut out),
            Call::AdoptProbe => self.probe_answer(answer, &mut out),
            Call::AdoptCheck => self.check_answer(answer, &mut out),
        }
        out
    }

    fn watch_answer(&mut self, id: WatchId, answer: &Answer, out: &mut Vec<Effect>) {
        let poll = self.options.poll_ms;
        let Some(w) = self.watches.iter_mut().find(|w| w.id == id) else {
            return;
        };
        if w.done {
            return;
        }
        let job = definitive(answer).unwrap_or(JValue::Null);
        if job.truthy() && inactive(&job) {
            return self.finish(id, out);
        }
        // A progress outage or a local timer is no acknowledgement that the container stopped.
        w.timer = true;
        out.push(Effect::SetTimer(Timer::Watch(id), poll));
    }

    fn probe_answer(&mut self, answer: &Answer, out: &mut Vec<Effect>) {
        let Some(Phase::Probe { delay }) = self.adopt.as_ref().map(|a| a.phase) else {
            return;
        };
        if self.adopt_cancelled {
            return self.settle(Settled::Resolved(false), out);
        }
        let Some(job) = definitive(answer) else {
            // Unknown progress keeps the restart barrier; a retry count cannot prove termination.
            let next = self.next_delay(delay);
            if let Some(a) = self.adopt.as_mut() {
                a.phase = Phase::ProbePause { next };
            }
            out.push(Effect::SetTimer(Timer::Adopt, delay));
            return;
        };
        // Only a throughput sweep (counted in models) runs outside the engine.
        if !job.truthy()
            || inactive(&job)
            || !strict_eq(job.get("unit"), &JValue::from("models"))
            || self.watching.is_some()
        {
            self.lift(out);
            return self.settle(Settled::Resolved(false), out);
        }
        // `Math.max(1, Number(job.total) || 1)`: only its throw matters now.
        if to_number(job.get("total")).is_err() {
            return self.settle(Settled::Rejected, out);
        }
        self.try_hold(self.options.adopt_retry_ms, out);
    }

    /// The answer to [`Effect::Hold`].
    pub fn hold_answered(&mut self, answer: HoldAnswer) -> Vec<Effect> {
        let mut out = Vec::new();
        let Some(Phase::Hold { delay }) = self.adopt.as_ref().map(|a| a.phase) else {
            // Not asked for: give back what came.
            if let HoldAnswer::Granted { .. } = answer {
                out.push(Effect::ReleaseAdopted);
            }
            return out;
        };
        match answer {
            HoldAnswer::Granted { lost } => {
                self.lift(&mut out);
                if self.adopt_cancelled || self.watching.is_some() {
                    out.push(Effect::ReleaseAdopted);
                    self.settle(Settled::Resolved(false), &mut out);
                    return out;
                }
                out.push(Effect::Log(LOG_ADOPTED));
                let id = WatchId(self.next_id);
                out.push(Effect::Adopted(id));
                let started = self.start_watch(lost, &mut out);
                debug_assert_eq!(started, id);
                self.settle(Settled::Resolved(true), &mut out);
            }
            HoldAnswer::Refused => {
                let barrier = self.options.barrier;
                if let Some(a) = self.adopt.as_mut() {
                    if !a.unbar && barrier {
                        a.unbar = true;
                        out.push(Effect::Barrier);
                        out.push(Effect::Log(LOG_AUTHORITY_DOWN));
                    }
                    a.phase = Phase::Check { delay };
                }
                out.push(Effect::Progress(Call::AdoptCheck));
            }
        }
        out
    }

    fn check_answer(&mut self, answer: &Answer, out: &mut Vec<Effect>) {
        let Some(Phase::Check { delay }) = self.adopt.as_ref().map(|a| a.phase) else {
            return;
        };
        // `still`: `null` = no job, a job, or `undefined` (no definitive answer).
        if let Some(still) = definitive(answer) {
            if !still.truthy() || inactive(&still) {
                self.lift(out);
                out.push(Effect::Log(LOG_FINISHED_WHILE_PAUSED));
                return self.settle(Settled::Resolved(false), out);
            }
        }
        // No local deadline can prove external termination.
        let next = self.next_delay(delay);
        if let Some(a) = self.adopt.as_mut() {
            a.phase = Phase::CheckPause { next };
        }
        out.push(Effect::SetTimer(Timer::Adopt, delay));
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn ok_job(job: JValue) -> Answer {
        Answer::Value(JValue::obj([
            ("ok", JValue::Bool(true)),
            ("body", JValue::obj([("job", job)])),
        ]))
    }

    fn running(total: f64) -> JValue {
        JValue::obj([
            ("active", JValue::Bool(true)),
            ("unit", JValue::from("models")),
            ("total", JValue::Num(total)),
        ])
    }

    #[test]
    fn definitive_reads_like_node() {
        assert_eq!(definitive(&Answer::Threw), None);
        assert_eq!(definitive(&Answer::Value(JValue::Null)), None);
        let not_ok = JValue::obj([("ok", JValue::Bool(false)), ("status", JValue::Num(503.0))]);
        assert_eq!(definitive(&Answer::Value(not_ok)), None);
        // An array body is an object: its `job` is undefined, so no job.
        let arr = JValue::obj([("ok", JValue::Num(1.0)), ("body", JValue::Arr(vec![]))]);
        assert_eq!(definitive(&Answer::Value(arr)), Some(JValue::Null));
        let string_body = JValue::obj([("ok", JValue::Bool(true)), ("body", JValue::from("x"))]);
        assert_eq!(definitive(&Answer::Value(string_body)), None);
        assert_eq!(definitive(&ok_job(JValue::Num(0.0))), Some(JValue::Null));
        assert_eq!(definitive(&ok_job(running(2.0))), Some(running(2.0)));
    }

    #[test]
    fn failure_mapping() {
        let outage = Refusal {
            status: JValue::Num(503.0),
            code: JValue::from("maintenanceAuthorityUnavailable"),
            ..Refusal::default()
        };
        let f = acquire_failure(&outage);
        assert_eq!(
            (f.status, f.code, f.waitable, f.unavailable),
            (503, Some("maintenanceAuthorityUnavailable"), true, true)
        );
        assert_eq!(f.public_message, SWEEP_UNAVAILABLE_ERROR);
        let text_status = Refusal {
            status: JValue::from("503"),
            code: JValue::from("maintenanceAuthorityUnavailable"),
            ..Refusal::default()
        };
        assert_eq!(acquire_failure(&text_status).status, 409);
        let busy = acquire_failure(&Refusal {
            status: JValue::Num(409.0),
            ..Refusal::default()
        });
        assert_eq!(
            (busy.status, busy.waitable, busy.public_message),
            (409, false, SWEEP_BUSY_ERROR)
        );
        assert!(
            acquire_failure(&Refusal {
                status: JValue::Num(502.0),
                ..Refusal::default()
            })
            .waitable
        );
        assert!(
            acquire_failure(&Refusal {
                from_gate: JValue::Bool(true),
                ..Refusal::default()
            })
            .waitable
        );
        assert!(
            acquire_failure(&Refusal {
                chat_active: JValue::Bool(true),
                ..Refusal::default()
            })
            .waitable
        );
        assert!(
            !acquire_failure(&Refusal {
                chat_active: JValue::Num(1.0),
                ..Refusal::default()
            })
            .waitable
        );
    }

    #[test]
    fn watch_keeps_the_hold_until_a_terminal_answer() {
        let mut g = Guard::new(Options {
            poll_ms: 1000,
            ..Options::default()
        });
        let (id, out) = g.watch(false);
        assert_eq!(out, vec![Effect::SetTimer(Timer::Watch(id), 1000)]);
        for answer in [
            Answer::Threw,
            Answer::Value(JValue::Null),
            ok_job(JValue::Null),
            ok_job(running(1.0)),
        ] {
            assert_eq!(
                g.timer_fired(Timer::Watch(id)),
                vec![Effect::Progress(Call::Watch(id))]
            );
            assert_eq!(
                g.progress_answered(Call::Watch(id), &answer),
                vec![Effect::SetTimer(Timer::Watch(id), 1000)]
            );
            assert!(g.active());
        }
        g.timer_fired(Timer::Watch(id));
        let done = JValue::obj([("active", JValue::Bool(false))]);
        assert_eq!(
            g.progress_answered(Call::Watch(id), &ok_job(done)),
            vec![Effect::Release(id)]
        );
        assert!(!g.active());
        // Lost after the finish: Node's listener still logs, once.
        assert_eq!(g.lease_lost(id), vec![Effect::Log(LOG_LEASE_LOST)]);
        assert_eq!(g.lease_lost(id), vec![]);
    }

    #[test]
    fn stop_clears_the_pending_poll_and_releases_once() {
        let mut g = Guard::new(Options::default());
        let (id, _) = g.watch(true);
        assert_eq!(
            g.stop(id),
            vec![Effect::ClearTimer(Timer::Watch(id)), Effect::Release(id)]
        );
        assert_eq!(g.stop(id), vec![]);
        assert_eq!(g.lease_lost(id), vec![], "logged at watch time already");
    }

    #[test]
    fn adopt_backs_off_then_takes_the_hold() {
        let mut g = Guard::new(Options {
            adopt_retry_ms: 100,
            adopt_max_delay_ms: 150,
            ..Options::default()
        });
        assert_eq!(
            g.adopt(),
            vec![Effect::Barrier, Effect::Progress(Call::AdoptProbe)]
        );
        assert_eq!(
            g.progress_answered(Call::AdoptProbe, &Answer::Threw),
            vec![Effect::SetTimer(Timer::Adopt, 100)]
        );
        assert_eq!(
            g.timer_fired(Timer::Adopt),
            vec![Effect::Progress(Call::AdoptProbe)]
        );
        let not_ok = Answer::Value(JValue::obj([("ok", JValue::Bool(false))]));
        assert_eq!(
            g.progress_answered(Call::AdoptProbe, &not_ok),
            vec![Effect::SetTimer(Timer::Adopt, 150)]
        );
        g.timer_fired(Timer::Adopt);
        assert_eq!(
            g.progress_answered(Call::AdoptProbe, &ok_job(running(2.0))),
            vec![Effect::Hold]
        );
        let out = g.hold_answered(HoldAnswer::Granted { lost: false });
        let id = g.watching().expect("watched");
        assert_eq!(
            out,
            vec![
                Effect::Lift,
                Effect::Log(LOG_ADOPTED),
                Effect::Adopted(id),
                Effect::SetTimer(Timer::Watch(id), POLL_MS),
                Effect::AdoptSettled(Settled::Resolved(true)),
            ]
        );
        assert!(!g.adopting());
    }

    #[test]
    fn cancel_during_a_wait_settles_false_and_keeps_the_barrier() {
        let mut g = Guard::new(Options {
            adopt_retry_ms: 10,
            ..Options::default()
        });
        g.adopt();
        g.progress_answered(Call::AdoptProbe, &Answer::Value(JValue::Null));
        assert_eq!(
            g.cancel_adopt(),
            vec![
                Effect::ClearTimer(Timer::Adopt),
                Effect::AdoptSettled(Settled::Resolved(false))
            ]
        );
        // Never reset: a later adopt raises its barrier and gives up at once.
        assert_eq!(
            g.adopt(),
            vec![
                Effect::Barrier,
                Effect::AdoptSettled(Settled::Resolved(false))
            ]
        );
    }

    #[test]
    fn nothing_running_answers_at_once() {
        let mut g = Guard::new(Options::default());
        g.adopt();
        assert_eq!(
            g.progress_answered(Call::AdoptProbe, &ok_job(JValue::Null)),
            vec![Effect::Lift, Effect::AdoptSettled(Settled::Resolved(false))]
        );
        // A prompt suite (not counted in models) runs inside the engine: nothing to adopt.
        g.adopt();
        let suite = JValue::obj([
            ("active", JValue::Bool(true)),
            ("unit", JValue::from("prompts")),
        ]);
        assert_eq!(
            g.progress_answered(Call::AdoptProbe, &ok_job(suite)),
            vec![Effect::Lift, Effect::AdoptSettled(Settled::Resolved(false))]
        );
    }

    #[test]
    fn refused_holds_are_waited_out_while_the_sweep_runs() {
        let mut g = Guard::new(Options {
            adopt_retry_ms: 1,
            adopt_max_delay_ms: 2,
            ..Options::default()
        });
        g.adopt();
        g.progress_answered(Call::AdoptProbe, &ok_job(running(1.0)));
        for delay in [1, 2, 2, 2] {
            assert_eq!(
                g.hold_answered(HoldAnswer::Refused),
                vec![Effect::Progress(Call::AdoptCheck)]
            );
            assert_eq!(
                g.progress_answered(Call::AdoptCheck, &ok_job(running(1.0))),
                vec![Effect::SetTimer(Timer::Adopt, delay)]
            );
            assert_eq!(g.timer_fired(Timer::Adopt), vec![Effect::Hold]);
        }
        g.hold_answered(HoldAnswer::Refused);
        let done = JValue::obj([
            ("active", JValue::Bool(false)),
            ("unit", JValue::from("models")),
        ]);
        assert_eq!(
            g.progress_answered(Call::AdoptCheck, &ok_job(done)),
            vec![
                Effect::Lift,
                Effect::Log(LOG_FINISHED_WHILE_PAUSED),
                Effect::AdoptSettled(Settled::Resolved(false)),
            ]
        );
    }

    #[test]
    fn without_a_barrier_a_refusal_raises_one_and_logs() {
        let mut g = Guard::new(Options {
            barrier: false,
            ..Options::default()
        });
        assert_eq!(g.adopt(), vec![Effect::Progress(Call::AdoptProbe)]);
        g.progress_answered(Call::AdoptProbe, &ok_job(running(3.0)));
        assert_eq!(
            g.hold_answered(HoldAnswer::Refused),
            vec![Effect::Progress(Call::AdoptCheck)],
            "typeof barrier !== 'function': no barrier, no line"
        );
    }

    #[test]
    fn a_total_that_cannot_be_a_number_rejects_with_the_barrier_up() {
        let mut g = Guard::new(Options::default());
        g.adopt();
        let job = JValue::obj([
            ("active", JValue::Bool(true)),
            ("unit", JValue::from("models")),
            ("total", JValue::obj([("toString", JValue::from("x"))])),
        ]);
        assert_eq!(
            g.progress_answered(Call::AdoptProbe, &ok_job(job)),
            vec![Effect::AdoptSettled(Settled::Rejected)]
        );
    }

    #[test]
    fn a_watch_started_while_adopting_wins() {
        let mut g = Guard::new(Options::default());
        g.adopt();
        g.progress_answered(Call::AdoptProbe, &ok_job(running(1.0)));
        let (id, _) = g.watch(false);
        assert_eq!(
            g.hold_answered(HoldAnswer::Granted { lost: false }),
            vec![
                Effect::Lift,
                Effect::ReleaseAdopted,
                Effect::AdoptSettled(Settled::Resolved(false))
            ]
        );
        assert_eq!(g.watching(), Some(id));
    }
}
