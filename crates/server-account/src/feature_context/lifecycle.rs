//! The shared controller serializes registration commit/activation and every later mutation.
//! Transport must retain its worker's permit and cancellation signal until the worker finishes.
use super::{source_fingerprint, unavailable, ContextSource, FeatureRuntime};
use server_store::{Reader, StoreError, WriteTx, Writer};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

const LEASE: Duration = Duration::from_secs(15);
const WAIT: Duration = Duration::from_secs(2);
const CAP: usize = 1024;

#[derive(Default)]
struct GateState {
    held: bool,
    waiting: usize,
}
#[derive(Default)]
pub(super) struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}
pub(super) struct Permit<'a>(&'a Gate);
impl Gate {
    pub(super) fn acquire(&self) -> Result<Permit<'_>, StoreError> {
        let deadline = Instant::now() + WAIT;
        let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        if state.waiting >= 32 {
            return Err(unavailable());
        }
        state.waiting += 1;
        while state.held {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                state.waiting -= 1;
                return Err(unavailable());
            };
            let (next, _) = self
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| StoreError::Poisoned)?;
            state = next;
        }
        state.waiting -= 1;
        if Instant::now() >= deadline {
            return Err(unavailable());
        }
        state.held = true;
        Ok(Permit(self))
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.held = false;
            self.0.changed.notify_one();
        }
    }
}

/// Handler owns this guard; dropping it cancels activation even if blocking SQL later commits.
#[derive(Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0)
    }
}
impl Drop for Cancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
pub struct Control {
    pub request: Uuid,
    pub deadline: Instant,
    pub cancelled: Arc<AtomicBool>,
}
impl Control {
    pub(super) fn check(&self) -> Result<(), StoreError> {
        if self.request.is_nil()
            || self.cancelled.load(Ordering::SeqCst)
            || Instant::now() >= self.deadline
        {
            Err(unavailable())
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BrowserFact {
    Available,
    MissingPlaywright,
    MissingChromium,
}
#[derive(Clone, PartialEq, Eq)]
pub struct BootFacts {
    /// Canonical Feature::ALL order; parsed env choices, never raw environment text.
    pub env: [Option<bool>; 23],
    /// Actual Node construction values: diaryMcpWrite, kiwix.
    pub restart: [bool; 2],
    pub browser: BrowserFact,
    pub code: bool,
    pub trust_proxy: bool,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChoiceReason {
    Unconfigured,
    Unsupported,
}
#[derive(Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub decision: String,
    pub machines: String,
    pub choice: Option<ChoiceReason>,
    pub system_one_unconfigured: bool,
}
impl Snapshot {
    pub(super) fn validate(&self) -> Result<(), StoreError> {
        if [&self.decision, &self.machines].iter().all(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            Ok(())
        } else {
            Err(unavailable())
        }
    }
    fn check_sources(&self, reader: &Reader<'_>) -> Result<(), StoreError> {
        if self.decision != source_fingerprint(reader, ContextSource::Decision)?
            || self.machines != source_fingerprint(reader, ContextSource::InferenceMachines)?
        {
            Err(unavailable())
        } else {
            Ok(())
        }
    }
}
/// An admission is evidence for one lifecycle generation, not reusable after an OFF transition.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    incarnation: Uuid,
    session: Uuid,
    generation: u64,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Ready(Admission),
    Released,
    Unavailable,
}
pub struct Registration {
    pub boot: Uuid,
    pub session: Uuid,
    pub facts: BootFacts,
    pub snapshot: Snapshot,
}
struct Session {
    boot: Uuid,
    register: Uuid,
    initial: Snapshot,
    sequence: u64,
    request: Uuid,
    snapshot: Snapshot,
    expires: Instant,
    reply: Reply,
    register_reply: Reply,
    renew_reply: Reply,
    release: Option<Uuid>,
}
#[derive(Default)]
struct State {
    boots: BTreeMap<Uuid, BootFacts>,
    sessions: BTreeMap<Uuid, Session>,
    current: Option<Uuid>,
    generation: u64,
}
impl State {
    fn expire(&mut self, now: Instant) {
        if self.current.is_some_and(|id| {
            self.sessions
                .get(&id)
                .is_none_or(|s| s.expires <= now || !matches!(s.reply, Reply::Ready(_)))
        }) {
            self.current = None;
        }
    }
}
pub struct Controller {
    runtime: Arc<FeatureRuntime>,
    env: [Option<bool>; 23],
    state: Mutex<State>,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    #[cfg(test)]
    control_state_probe: Mutex<Option<std::sync::mpsc::SyncSender<bool>>>,
}
impl Controller {
    pub(super) fn runtime(&self) -> &FeatureRuntime {
        &self.runtime
    }
    pub fn new(runtime: Arc<FeatureRuntime>, env: [Option<bool>; 23]) -> Self {
        Self {
            runtime,
            env,
            state: Mutex::new(State::default()),
            clock: Arc::new(Instant::now),
            #[cfg(test)]
            control_state_probe: Mutex::new(None),
        }
    }
    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, StoreError> {
        self.state.lock().map_err(|_| StoreError::Poisoned)
    }
    fn now(&self) -> Instant {
        (self.clock)()
    }
    #[cfg(test)]
    fn before_control_state_wait(&self) -> Result<(), StoreError> {
        let probe = self
            .control_state_probe
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .take();
        if let Some(probe) = probe {
            // Observe the ACTUAL held mutex after all DB/pre-state checks. The test cancels
            // only after receiving this observation, while retaining the state guard.
            let contended = matches!(
                self.state.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            );
            probe.send(contended).map_err(|_| unavailable())?;
        }
        Ok(())
    }

    /// Cleanup/migration is supplied by the Account owner inside the SAME existing Writer.
    /// No Ready state is installed inside its callback; commit failures and late commits retire it.
    pub fn register(
        &self,
        writer: &Writer,
        control: &Control,
        registration: Registration,
        initialize: impl FnOnce(&WriteTx<'_>) -> Result<(), StoreError>,
    ) -> Result<Reply, StoreError> {
        let Registration {
            boot,
            session,
            facts,
            snapshot,
        } = registration;
        control.check()?;
        snapshot.validate()?;
        if boot.is_nil() || session.is_nil() || facts.env != self.env {
            return Err(unavailable());
        }
        let _permit = self.runtime.operation_permit()?;
        control.check()?;
        self.runtime.initialize_under_permit(writer)?;
        let generation = {
            let mut state = self.state()?;
            state.expire(self.now());
            if let Some(old) = state.sessions.get(&session) {
                if old.boot != boot
                    || old.register != control.request
                    || old.initial != snapshot
                    || state.boots.get(&boot) != Some(&facts)
                {
                    return Err(unavailable());
                }
                return Ok(if state.current == Some(session) {
                    old.register_reply
                } else {
                    Reply::Unavailable
                });
            }
            if state.current.is_some()
                || state.sessions.len() >= CAP
                || (state.boots.len() >= CAP && !state.boots.contains_key(&boot))
            {
                return Err(unavailable());
            }
            if state.boots.get(&boot).is_some_and(|old| old != &facts) {
                return Err(unavailable());
            }
            state.generation = state.generation.checked_add(1).ok_or_else(unavailable)?;
            let generation = state.generation;
            state.boots.entry(boot).or_insert(facts);
            state.sessions.insert(
                session,
                Session {
                    boot,
                    register: control.request,
                    initial: snapshot.clone(),
                    sequence: 0,
                    request: control.request,
                    snapshot: snapshot.clone(),
                    expires: self.now(),
                    reply: Reply::Unavailable,
                    register_reply: Reply::Unavailable,
                    renew_reply: Reply::Unavailable,
                    release: None,
                },
            );
            // Pending never grants authority to read/device/public mutation paths.
            state.current = None;
            generation
        };
        let committed = writer.write(|tx| {
            self.runtime.check_write_incarnation(tx)?;
            snapshot.check_sources(&tx.reader())?;
            control.check()?;
            initialize(tx)
        });
        if committed.is_err() {
            return Ok(Reply::Unavailable);
        }
        // This worker retains the outer permit through commit, cancellation and epoch resolution.
        // A cancelled transaction may have committed; exact retries must not repeat cleanup.
        let activation = writer.read(|reader| {
            self.runtime.check_incarnation(reader)?;
            snapshot.check_sources(reader)?;
            control.check()
        });
        if activation.is_err() || control.check().is_err() {
            return Ok(Reply::Unavailable);
        }
        // Publish only after BOTH the writer COMMIT and confirming read transaction finish.
        let mut state = self.state()?;
        if state.generation != generation || state.current.is_some() || control.check().is_err() {
            return Ok(Reply::Unavailable);
        }
        let admission = self.admission_for(session, generation)?;
        let record = state.sessions.get_mut(&session).ok_or_else(unavailable)?;
        record.expires = self.now() + LEASE;
        record.reply = Reply::Ready(admission);
        record.register_reply = record.reply;
        state.current = Some(session);
        Ok(Reply::Ready(admission))
    }
    fn admission_for(&self, session: Uuid, generation: u64) -> Result<Admission, StoreError> {
        Ok(Admission {
            incarnation: Uuid::parse_str(self.runtime.incarnation()).map_err(|_| unavailable())?,
            session,
            generation,
        })
    }
    pub fn admission(&self, reader: &Reader<'_>) -> Result<Admission, StoreError> {
        self.runtime.check_incarnation(reader)?;
        let mut state = self.state()?;
        state.expire(self.now());
        let session = state.current.ok_or_else(unavailable)?;
        let record = state.sessions.get(&session).ok_or_else(unavailable)?;
        record.snapshot.check_sources(reader)?;
        match record.reply {
            Reply::Ready(admission) => Ok(admission),
            _ => Err(unavailable()),
        }
    }
    /// Exact same sequence/request/body retry never extends the monotonic expiration.
    pub fn renew(
        &self,
        writer: &Writer,
        control: &Control,
        boot: Uuid,
        session: Uuid,
        sequence: u64,
        snapshot: Snapshot,
    ) -> Result<Reply, StoreError> {
        if sequence == 0 {
            return Err(unavailable());
        }
        snapshot.validate()?;
        control.check()?;
        let _permit = self.runtime.operation_permit()?;
        writer.read(|reader| {
            self.runtime.check_incarnation(reader)?;
            snapshot.check_sources(reader)?;
            control.check()
        })?;
        control.check()?;
        {
            #[cfg(test)]
            self.before_control_state_wait()?;
            let mut state = self.state()?;
            control.check()?; // The request can retire while waiting for THIS mutex.

            state.expire(self.now());
            if state.current != Some(session) {
                return Err(unavailable());
            }
            let record = state.sessions.get_mut(&session).ok_or_else(unavailable)?;
            if record.boot != boot {
                return Err(unavailable());
            }
            if sequence == record.sequence {
                if control.request != record.request || snapshot != record.snapshot {
                    return Err(unavailable());
                }
                return Ok(record.renew_reply);
            }
            if sequence != record.sequence.checked_add(1).ok_or_else(unavailable)?
                || control.request == record.request
                || control.request == record.register
            {
                return Err(unavailable());
            }
            record.sequence = sequence;
            record.request = control.request;
            record.snapshot = snapshot;
            record.expires = self.now() + LEASE;
            record.renew_reply = record.reply;
            Ok(record.renew_reply)
        }
    }
    pub fn release(
        &self,
        writer: &Writer,
        control: &Control,
        boot: Uuid,
        session: Uuid,
    ) -> Result<Reply, StoreError> {
        control.check()?;
        let _permit = self.runtime.operation_permit()?;
        writer.read(|reader| {
            self.runtime.check_incarnation(reader)?;
            control.check()
        })?;
        control.check()?;
        {
            #[cfg(test)]
            self.before_control_state_wait()?;
            let mut state = self.state()?;
            control.check()?; // The request can retire while waiting for THIS mutex.

            let record = state.sessions.get_mut(&session).ok_or_else(unavailable)?;
            if record.boot != boot
                || control.request == record.register
                || control.request == record.request
                || record.release.is_some_and(|id| id != control.request)
            {
                return Err(unavailable());
            }
            record.release = Some(control.request);
            record.reply = Reply::Released;
            if state.current == Some(session) {
                state.current = None;
            }
            Ok(Reply::Released)
        }
    }
    /// Final device/feature mutation fence. Caller supplies its earlier coherent admission;
    /// lifecycle change retires ALL such admissions only after a successful commit.
    pub fn mutate<T>(
        &self,
        writer: &Writer,
        admission: Admission,
        lifecycle_change: bool,
        action: impl FnOnce(&WriteTx<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let _permit = self.runtime.operation_permit()?;
        let next_generation = if lifecycle_change {
            Some(
                self.state()?
                    .generation
                    .checked_add(1)
                    .ok_or_else(unavailable)?,
            )
        } else {
            None
        };
        let result = writer.write(|tx| {
            self.runtime.check_write_incarnation(tx)?;
            if self.admission(&tx.reader())? != admission {
                return Err(unavailable());
            }
            let result = action(tx)?;
            if self.admission(&tx.reader())? != admission {
                return Err(unavailable());
            }
            Ok(result)
        })?;
        if let Some(generation) = next_generation {
            let mut state = self.state()?;
            state.generation = generation;
            if let Some(session) = state.current {
                if let Some(record) = state.sessions.get_mut(&session) {
                    record.reply = Reply::Ready(self.admission_for(session, generation)?);
                }
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use server_store::{db_path, rusqlite::Connection, RustAuth, RustFeatureSettings};
    use std::sync::atomic::AtomicU64;
    type Error = Box<dyn std::error::Error>;
    struct Fixture {
        _dir: tempfile::TempDir,
        node: Connection,
        writer: Writer,
        controller: Controller,
        clock: Arc<AtomicU64>,
    }
    fn fixture() -> Result<Fixture, Error> {
        let dir = tempfile::tempdir()?;
        let node = Connection::open(db_path(dir.path()))?;
        let schema = include_str!("../../../server-store/tests/fixtures/node-schema.sql").replace(
            "REFERENCES users(id) ON DELETE CASCADE",
            "REFERENCES users(id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED",
        );
        node.execute_batch(&schema)?;
        let writer = Writer::open(dir.path(), RustAuth::owned())?;
        let runtime = Arc::new(FeatureRuntime::new()?);
        let mut controller = Controller::new(runtime, [None; 23]);
        let base = Instant::now();
        let clock = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&clock);
        controller.clock =
            Arc::new(move || base + Duration::from_millis(observed.load(Ordering::SeqCst)));
        Ok(Fixture {
            _dir: dir,
            node,
            writer,
            controller,
            clock,
        })
    }
    fn control(id: u128) -> (Cancellation, Control) {
        let cancel = Cancellation::default();
        let request = Control {
            request: Uuid::from_u128(id),
            deadline: Instant::now() + Duration::from_secs(30),
            cancelled: cancel.signal(),
        };
        (cancel, request)
    }
    fn registration(f: &Fixture, session: u128) -> Result<Registration, StoreError> {
        Ok(Registration {
            boot: Uuid::from_u128(1),
            session: Uuid::from_u128(session),
            facts: BootFacts {
                env: [None; 23],
                restart: [false, true],
                browser: BrowserFact::MissingPlaywright,
                code: false,
                trust_proxy: true,
            },
            snapshot: f.writer.read(|r| {
                Ok(Snapshot {
                    decision: source_fingerprint(r, ContextSource::Decision)?,
                    machines: source_fingerprint(r, ContextSource::InferenceMachines)?,
                    choice: Some(ChoiceReason::Unconfigured),
                    system_one_unconfigured: true,
                })
            })?,
        })
    }
    fn ready(f: &Fixture) -> Result<Admission, Error> {
        let (_cancel, c) = control(10);
        let reply = f
            .controller
            .register(&f.writer, &c, registration(f, 2)?, |_| Ok(()))?;
        match reply {
            Reply::Ready(a) => Ok(a),
            _ => Err("not ready".into()),
        }
    }
    #[test]
    fn pending_commit_then_exact_retry_never_repeats_cleanup() -> Result<(), Error> {
        let f = fixture()?;
        let (_cancel, c) = control(10);
        let count = AtomicU64::new(0);
        let first = f
            .controller
            .register(&f.writer, &c, registration(&f, 2)?, |tx| {
                assert!(f.controller.admission(&tx.reader()).is_err());
                count.fetch_add(1, Ordering::SeqCst);
                tx.set_feature_enabled(
                    RustFeatureSettings::owned(),
                    server_features::Feature::PlannerReview,
                    true,
                )
                .map(|_| ())
            })?;
        assert!(matches!(first, Reply::Ready(_)));
        let replay = f
            .controller
            .register(&f.writer, &c, registration(&f, 2)?, |_| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })?;
        assert!(first == replay);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(f.writer.read(|r| f.controller.admission(r)).is_ok());
        Ok(())
    }
    #[test]
    fn cancelled_worker_can_commit_but_never_activate_or_repeat_cleanup() -> Result<(), Error> {
        let f = fixture()?;
        let (cancel, c) = control(10);
        let count = AtomicU64::new(0);
        let result = f
            .controller
            .register(&f.writer, &c, registration(&f, 2)?, |tx| {
                count.fetch_add(1, Ordering::SeqCst);
                tx.set_feature_enabled(
                    RustFeatureSettings::owned(),
                    server_features::Feature::PlannerReview,
                    true,
                )?;
                cancel.0.store(true, Ordering::SeqCst);
                Ok(())
            })?;
        assert!(result == Reply::Unavailable);
        let value: String = f.node.query_row(
            "SELECT value FROM settings WHERE key='feature:plannerReview'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(value, "true"); // cancellation did NOT roll back the actual committed SQL.
        assert!(f.writer.read(|r| f.controller.admission(r)).is_err());
        let (_retry_cancel, retry) = control(10);
        assert!(
            f.controller
                .register(&f.writer, &retry, registration(&f, 2)?, |_| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })?
                == Reply::Unavailable
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let (_recover_cancel, recover) = control(11);
        assert!(matches!(
            f.controller
                .register(&f.writer, &recover, registration(&f, 3)?, |_| Ok(()))?,
            Reply::Ready(_)
        ));
        Ok(())
    }
    #[test]
    fn actual_deferred_sqlite_commit_failure_does_not_activate() -> Result<(), Error> {
        let f = fixture()?;
        let (_cancel, c) = control(10);
        let callback_completed = AtomicBool::new(false);
        assert!(f.controller.register(&f.writer,&c,registration(&f,2)?,|tx| {
            tx.set_feature_enabled(RustFeatureSettings::owned(),server_features::Feature::PlannerReview,true)?;
            tx.execute("INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES('synthetic','absent','synthetic',1,1,99,'','')",[])?;
            callback_completed.store(true,Ordering::SeqCst);Ok(())
        })?==Reply::Unavailable);
        assert!(callback_completed.load(Ordering::SeqCst)); // failure really occurred at COMMIT.
        assert!(f.writer.read(|r| f.controller.admission(r)).is_err());
        assert_eq!(
            f.node.query_row(
                "SELECT count(*) FROM settings WHERE key='feature:plannerReview'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        Ok(())
    }
    #[test]
    fn renewal_is_ordered_idempotent_and_expired_sessions_never_revive() -> Result<(), Error> {
        let f = fixture()?;
        ready(&f)?;
        let (_cancel, c) = control(11);
        let first = registration(&f, 2)?;
        f.clock.store(5000, Ordering::SeqCst);
        assert!(matches!(
            f.controller.renew(
                &f.writer,
                &c,
                first.boot,
                first.session,
                1,
                first.snapshot.clone()
            )?,
            Reply::Ready(_)
        ));
        f.clock.store(19000, Ordering::SeqCst);
        f.controller.renew(
            &f.writer,
            &c,
            first.boot,
            first.session,
            1,
            first.snapshot.clone(),
        )?;
        let (_changed_cancel, changed) = control(12);
        assert!(f
            .controller
            .renew(
                &f.writer,
                &changed,
                first.boot,
                first.session,
                1,
                first.snapshot.clone()
            )
            .is_err());
        f.clock.store(20000, Ordering::SeqCst);
        assert!(f
            .controller
            .renew(
                &f.writer,
                &changed,
                first.boot,
                first.session,
                2,
                first.snapshot
            )
            .is_err());
        assert!(f.writer.read(|r| f.controller.admission(r)).is_err());
        let (_old_cancel, old) = control(10);
        assert!(
            f.controller
                .register(&f.writer, &old, registration(&f, 2)?, |_| Ok(()))?
                == Reply::Unavailable
        );
        Ok(())
    }
    #[test]
    fn release_recovery_preserves_boot_facts_and_stale_cleanup_cannot_retire_successor(
    ) -> Result<(), Error> {
        let f = fixture()?;
        ready(&f)?;
        let (_cancel, c) = control(11);
        assert!(
            f.controller
                .release(&f.writer, &c, Uuid::from_u128(1), Uuid::from_u128(2))?
                == Reply::Released
        );
        let (_next_cancel, next) = control(12);
        let mut changed = registration(&f, 3)?;
        changed.facts.restart = [true, false];
        assert!(f
            .controller
            .register(&f.writer, &next, changed, |_| Ok(()))
            .is_err());
        f.controller
            .register(&f.writer, &next, registration(&f, 3)?, |_| Ok(()))?;
        assert!(
            f.controller
                .release(&f.writer, &c, Uuid::from_u128(1), Uuid::from_u128(2))?
                == Reply::Released
        );
        assert!(f.writer.read(|r| f.controller.admission(r)).is_ok());
        let (_competing_cancel, competing) = control(13);
        assert!(f
            .controller
            .register(&f.writer, &competing, registration(&f, 4)?, |_| Ok(()))
            .is_err());
        Ok(())
    }
    #[test]
    fn old_admission_stays_retired_after_off_then_on_and_failed_mutation_does_not_advance(
    ) -> Result<(), Error> {
        let f = fixture()?;
        let before = ready(&f)?;
        assert!(f
            .controller
            .mutate(&f.writer, before, true, |_| Err::<(), _>(unavailable()))
            .is_err());
        f.controller.mutate(&f.writer, before, true, |tx| {
            tx.set_feature_enabled(
                RustFeatureSettings::owned(),
                server_features::Feature::NativeClientAuth,
                false,
            )
        })?;
        let after_off = f.writer.read(|r| f.controller.admission(r))?;
        f.controller.mutate(&f.writer, after_off, true, |tx| {
            tx.set_feature_enabled(
                RustFeatureSettings::owned(),
                server_features::Feature::NativeClientAuth,
                true,
            )
        })?;
        let executed = AtomicBool::new(false);
        assert!(f
            .controller
            .mutate(&f.writer, before, false, |_| {
                executed.store(true, Ordering::SeqCst);
                Ok(())
            })
            .is_err());
        assert!(!executed.load(Ordering::SeqCst));
        let (_retry_cancel, retry) = control(10);
        assert!(
            f.controller
                .register(&f.writer, &retry, registration(&f, 2)?, |_| Ok(()))?
                == Reply::Ready(before)
        );
        let fresh = f.writer.read(|r| f.controller.admission(r))?;
        f.controller.mutate(&f.writer, fresh, false, |_| Ok(()))?;
        Ok(())
    }
    #[test]
    fn changed_sources_new_epoch_and_expiry_inside_mutation_refuse_without_writes(
    ) -> Result<(), Error> {
        let f = fixture()?;
        let admission = ready(&f)?;
        assert!(f
            .controller
            .mutate(&f.writer, admission, false, |tx| {
                tx.set_feature_enabled(
                    RustFeatureSettings::owned(),
                    server_features::Feature::PlannerReview,
                    true,
                )?;
                f.clock.store(15000, Ordering::SeqCst);
                Ok(())
            })
            .is_err());
        assert_eq!(
            f.node.query_row(
                "SELECT count(*) FROM settings WHERE key='feature:plannerReview'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        let g = fixture()?;
        ready(&g)?;
        g.node.execute(
            "INSERT INTO settings VALUES('decision:configuration','{}')",
            [],
        )?;
        assert!(g.writer.read(|r| g.controller.admission(r)).is_err());
        let new = FeatureRuntime::new()?;
        new.initialize(&g.writer)?;
        assert!(g.writer.read(|r| g.controller.admission(r)).is_err());
        Ok(())
    }
    #[test]
    fn actual_worker_drop_cancellation_keeps_permit_until_commit_and_no_activation(
    ) -> Result<(), Error> {
        let f = fixture()?;
        let registration = registration(&f, 2)?;
        let Fixture {
            writer,
            controller,
            node,
            _dir,
            clock: _,
        } = f;
        let writer = Arc::new(writer);
        let controller = Arc::new(controller);
        let (cancel, c) = control(10);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
        let worker_controller = Arc::clone(&controller);
        let worker_writer = Arc::clone(&writer);
        let worker = std::thread::spawn(move || {
            worker_controller.register(&worker_writer, &c, registration, |tx| {
                tx.set_feature_enabled(
                    RustFeatureSettings::owned(),
                    server_features::Feature::PlannerReview,
                    true,
                )?;
                entered_tx.send(()).map_err(|_| unavailable())?;
                resume_rx.recv().map_err(|_| unavailable())?;
                Ok(())
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(5))?;
        drop(cancel); // actual handler guard disappears while its SQL worker is still blocked.
        assert!(
            controller
                .runtime
                .operations
                .state
                .lock()
                .map_err(|_| "gate poisoned")?
                .held
        );
        assert!(controller.state()?.current.is_none());
        resume_tx.send(())?;
        let reply = worker.join().map_err(|_| "worker panic")??;
        assert!(reply == Reply::Unavailable);
        assert!(
            !controller
                .runtime
                .operations
                .state
                .lock()
                .map_err(|_| "gate poisoned")?
                .held
        );
        assert_eq!(
            node.query_row(
                "SELECT value FROM settings WHERE key='feature:plannerReview'",
                [],
                |r| r.get::<_, String>(0)
            )?,
            "true"
        );
        assert!(writer.read(|r| controller.admission(r)).is_err());
        drop(_dir);
        Ok(())
    }
    #[test]
    fn real_record_caps_never_evict_boot_or_session_fences() -> Result<(), Error> {
        let f = fixture()?;
        for index in 0..CAP {
            f.clock
                .store(u64::try_from(index)? * 15000, Ordering::SeqCst);
            let (_cancel, c) = control(30000 + index as u128);
            let mut next = registration(&f, 10000 + index as u128)?;
            next.boot = Uuid::from_u128(20000 + index as u128);
            assert!(matches!(
                f.controller.register(&f.writer, &c, next, |_| Ok(()))?,
                Reply::Ready(_)
            ));
        }
        assert_eq!(f.controller.state()?.boots.len(), CAP);
        assert_eq!(f.controller.state()?.sessions.len(), CAP);
        let (_cancel, c) = control(99999);
        f.clock.fetch_add(15000, Ordering::SeqCst);
        assert!(f
            .controller
            .register(&f.writer, &c, registration(&f, 99999)?, |_| Ok(()))
            .is_err());
        assert_eq!(f.controller.state()?.boots.len(), CAP);
        let (_old_cancel, old) = control(30000);
        let mut original = registration(&f, 10000)?;
        original.boot = Uuid::from_u128(20000);
        assert!(
            f.controller
                .register(&f.writer, &old, original, |_| Ok(()))?
                == Reply::Unavailable
        );
        Ok(())
    }
    #[test]
    fn native_restart_incarnation_fences_even_reused_boot_session_and_generation(
    ) -> Result<(), Error> {
        let f = fixture()?;
        let old = ready(&f)?;
        let replacement = Controller::new(Arc::new(FeatureRuntime::new()?), [None; 23]);
        let (_cancel, c) = control(10);
        assert!(matches!(
            replacement.register(&f.writer, &c, registration(&f, 2)?, |_| Ok(()))?,
            Reply::Ready(_)
        ));
        let executed = AtomicBool::new(false);
        assert!(replacement
            .mutate(&f.writer, old, false, |_| {
                executed.store(true, Ordering::SeqCst);
                Ok(())
            })
            .is_err());
        assert!(!executed.load(Ordering::SeqCst));
        Ok(())
    }
    #[test]
    fn serialization_wait_is_finite_and_pending_queue_is_bounded() -> Result<(), Error> {
        let gate = Gate::default();
        let held = gate.acquire()?;
        let start = Instant::now();
        assert!(gate.acquire().is_err());
        assert!(start.elapsed() >= WAIT);
        drop(held);
        gate.state.lock().map_err(|_| "gate poisoned")?.waiting = 32;
        assert!(gate.acquire().is_err());
        gate.state.lock().map_err(|_| "gate poisoned")?.waiting = 0;
        assert!(gate.acquire().is_ok());
        Ok(())
    }
    #[derive(PartialEq, Eq)]
    struct SessionObservation {
        sequence: u64,
        request: Uuid,
        snapshot: Snapshot,
        expires: Instant,
        reply: Reply,
        release: Option<Uuid>,
        current: Option<Uuid>,
    }
    impl SessionObservation {
        fn capture(state: &State) -> Result<Self, Error> {
            let record = state
                .sessions
                .get(&Uuid::from_u128(2))
                .ok_or("missing session")?;
            Ok(Self {
                sequence: record.sequence,
                request: record.request,
                snapshot: record.snapshot.clone(),
                expires: record.expires,
                reply: record.reply,
                release: record.release,
                current: state.current,
            })
        }
    }
    fn final_state_race(release: bool, retry: bool, expire_deadline: bool) -> Result<(), Error> {
        let f = fixture()?;
        ready(&f)?;
        let registration = registration(&f, 2)?;
        // Prime an exact retry, or advance the existing session so a fresh renewal really
        // would extend it. None of these are the blocked operation being tested.
        f.clock.store(5000, Ordering::SeqCst);
        let (_prime_cancel, prime) = control(11);
        f.controller.renew(
            &f.writer,
            &prime,
            registration.boot,
            registration.session,
            1,
            registration.snapshot.clone(),
        )?;
        if release && retry {
            let (_release_cancel, prior_release) = control(12);
            f.controller.release(
                &f.writer,
                &prior_release,
                registration.boot,
                registration.session,
            )?;
        }
        let Fixture {
            controller,
            writer,
            node: _node,
            _dir,
            clock: _,
        } = f;
        let controller = Arc::new(controller);
        let writer = Arc::new(writer);
        let request = if release {
            12
        } else if retry {
            11
        } else {
            13
        };
        let (cancel, mut c) = control(request);
        if expire_deadline {
            c.deadline = Instant::now() + Duration::from_secs(1);
        }
        let deadline = c.deadline;
        let (tx, rx) = std::sync::mpsc::sync_channel(0);
        *controller
            .control_state_probe
            .lock()
            .map_err(|_| "probe poisoned")? = Some(tx);
        let held = controller.state()?;
        let before = SessionObservation::capture(&held)?;
        let worker_controller = Arc::clone(&controller);
        let worker_writer = Arc::clone(&writer);
        let worker = std::thread::spawn(move || {
            if release {
                worker_controller.release(
                    &worker_writer,
                    &c,
                    registration.boot,
                    registration.session,
                )
            } else {
                worker_controller.renew(
                    &worker_writer,
                    &c,
                    registration.boot,
                    registration.session,
                    if retry { 1 } else { 2 },
                    registration.snapshot,
                )
            }
        });
        assert!(rx.recv_timeout(Duration::from_secs(5))?); // actual final mutex contention after DB checks
        if expire_deadline {
            while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                std::thread::park_timeout(remaining);
            }
        } else {
            cancel.0.store(true, Ordering::SeqCst);
        }
        drop(held);
        let outcome = worker.join().map_err(|_| "worker panic")?;
        assert!(outcome.is_err());
        let observed = controller.state()?;
        assert!(before == SessionObservation::capture(&observed)?);
        drop(cancel);
        Ok(())
    }
    #[test]
    fn final_renew_state_wait_rechecks_cancel_and_deadline_including_exact_retry(
    ) -> Result<(), Error> {
        for retry in [false, true] {
            for expired in [false, true] {
                final_state_race(false, retry, expired)?;
            }
        }
        Ok(())
    }
    #[test]
    fn final_release_state_wait_rechecks_cancel_and_deadline_including_exact_retry(
    ) -> Result<(), Error> {
        for retry in [false, true] {
            for expired in [false, true] {
                final_state_race(true, retry, expired)?;
            }
        }
        Ok(())
    }
}
