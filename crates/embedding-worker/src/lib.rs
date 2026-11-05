//! Native embedding-service refresh, migration and durable source outbox execution.
//! Server lifecycle is gated by paired ownership proof. Adapters enforce the
//! storage fences documented by [`Host`].
// Reuse the integration fixtures in the private executor's unit tests.
#[cfg(test)]
extern crate self as embedding_worker;

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
pub mod fill;
pub mod http;
pub mod outbox;
pub mod runtime;
pub mod service;
pub mod sqlite;
pub use sqlite_vec_loader;
pub type Task<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Already validated/normalized by the embedding configuration owner. Endpoint is
/// part of request identity, but never part of stored vector identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Target {
    pub profile: String,
    pub model: String,
    pub base_url: Option<String>,
}
impl Target {
    pub fn same_identity(&self, other: &Self) -> bool {
        self.profile == other.profile && self.model == other.model
    }
}
#[derive(Clone, Debug)]
pub struct Config {
    pub version: u64,
    pub active: Target,
    pub next: Option<Target>,
    pub previous: Option<Target>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Index {
    pub user: Option<String>,
    pub project: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    EndpointUnverified,
    ModelNotServed,
}
#[derive(Default, Debug)]
pub struct Batch {
    pub embedded: usize,
    pub attempted: usize,
    pub refused: Vec<i64>,
    pub remaining: usize,
}
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Pass {
    pub at_ms: u64,
    pub embedded: usize,
    pub failed: usize,
    pub refused: usize,
    pub error: Option<String>,
    pub aborted: bool,
}

/// An adapter must enumerate only existing indexes of active tenants; never create
/// an index during backfill. `fill` owns batch-to-single fallback and must recheck
/// cancellation, config version, tenant removal/project tombstones and chunk hashes
/// inside publication. A failed/dropped future must never acknowledge an outbox job.
/// `drop_slot` must also refuse removed tenants/projects and preserve other identities.
/// All network and sleep operations must be bounded and cancellation-aware.
pub trait Host: Sync {
    fn config(&self) -> Config;
    fn now_ms(&self) -> u64;
    fn indexes(&self) -> Vec<Index>;
    fn served<'a>(&'a self, target: &'a Target) -> Task<'a, Result<Vec<String>, String>>;
    fn fill<'a>(
        &'a self,
        index: &'a Index,
        target: &'a Target,
        limit: usize,
        exclude: &'a BTreeSet<i64>,
        version: u64,
        control: &'a Control,
    ) -> Task<'a, Result<Option<Batch>, String>>;
    fn drop_slot(&self, index: &Index, target: &Target) -> Result<(), String>;
    fn sleep<'a>(&'a self, ms: u64, control: &'a Control) -> Task<'a, ()>;
}
/// Kicks coalesce while a pass is running. A single task must own the Worker;
/// callers can clone this handle to request a pass or permanently stop it.
type PublicationProbe = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;
#[derive(Clone, Default)]
pub struct Control {
    cancellation: Arc<tokio::sync::Notify>,
    publication_probe: Arc<std::sync::Mutex<Option<PublicationProbe>>>,
    stopped: Arc<AtomicBool>,
    pending: Arc<AtomicBool>,
    suspended: Arc<AtomicBool>,
    unconfirmed_since: Arc<AtomicU64>,
    busy_ms: Arc<AtomicU64>,
    busy_samples: Arc<AtomicU64>,
    proof_required: Arc<AtomicBool>,
    proof_at: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
}
impl Control {
    pub fn kick(&self) {
        self.pending.store(true, Ordering::Release);
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.cancellation.notify_waiters();
    }
    pub fn suspend(&self) {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(1)
            .max(1);
        let _ = self
            .unconfirmed_since
            .compare_exchange(0, at, Ordering::AcqRel, Ordering::Acquire);
        self.suspended.store(true, Ordering::Release);
        self.cancellation.notify_waiters();
    }
    pub fn confirm_ownership(&self) {
        self.proof_required.store(true, Ordering::Release);
        if let Ok(mut at) = self.proof_at.lock() {
            *at = Some(std::time::Instant::now());
        }
    }
    pub fn set_publication_probe(
        &self,
        probe: impl Fn() -> Result<(), String> + Send + Sync + 'static,
    ) {
        if let Ok(mut slot) = self.publication_probe.lock() {
            *slot = Some(Arc::new(probe));
        }
    }
    /// Called after every publication fence is granted. The bounded proof and
    /// transaction share a new 250ms window; time queued for locks is excluded.
    pub fn renew_publication_proof(&self) -> Result<(), String> {
        let probe = self
            .publication_probe
            .lock()
            .map_err(|_| "publication proof unavailable")?
            .clone();
        if let Some(probe) = probe {
            let begun = std::time::Instant::now();
            if let Err(error) = probe() {
                self.suspend();
                return Err(error);
            }
            *self
                .proof_at
                .lock()
                .map_err(|_| "publication proof unavailable")? = Some(begun);
        }
        if !self.ownership_fresh() {
            self.suspend();
            return Err("publication proof expired".into());
        }
        Ok(())
    }
    pub fn ownership_fresh(&self) -> bool {
        !self.proof_required.load(Ordering::Acquire)
            || self.proof_at.lock().is_ok_and(|at| {
                at.is_some_and(|at| at.elapsed() <= std::time::Duration::from_millis(250))
            })
    }
    pub fn resume(&self) {
        if self.suspended.swap(false, Ordering::AcqRel) {
            self.unconfirmed_since.store(0, Ordering::Release);
            self.kick();
        }
    }
    pub fn suspended(&self) -> bool {
        self.suspended.load(Ordering::Acquire)
    }
    pub fn unconfirmed_since(&self) -> Option<u64> {
        let at = self.unconfirmed_since.load(Ordering::Acquire);
        (at != 0).then_some(at)
    }
    /// Record only time spent embedding after admission is granted.
    pub fn record_embedding_busy(&self, ms: u64) {
        self.busy_ms.fetch_add(ms, Ordering::AcqRel);
        self.busy_samples.fetch_add(1, Ordering::AcqRel);
    }
    pub fn embedding_busy(&self) -> (u64, u64) {
        (
            self.busy_samples.load(Ordering::Acquire),
            self.busy_ms.load(Ordering::Acquire),
        )
    }
    pub async fn cancelled(&self) {
        // Register before checking the state so a concurrent stop/suspend
        // cannot be lost between the check and the await.
        let notified = self.cancellation.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.stopped() || self.suspended() {
            return;
        }
        notified.await;
    }

    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}
#[derive(Clone, Debug)]
pub struct Options {
    pub duty: f64,
    pub batch: usize,
    pub min_pause_ms: u64,
    pub retry_ms: u64,
    pub recheck_ms: u64,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            duty: 0.5,
            batch: 4,
            min_pause_ms: 50,
            retry_ms: 60_000,
            recheck_ms: 300_000,
        }
    }
}
impl Options {
    /// Equivalent to the finite/clamped numeric values in embedding-migration.cjs.
    /// Environment/JS string coercion belongs to the configuration adapter.
    pub fn new(duty: f64, batch: f64, pause: f64, retry: f64, recheck: f64) -> Self {
        fn clamp(v: f64, fallback: f64, lo: f64, hi: f64) -> f64 {
            if v.is_finite() {
                v.clamp(lo, hi)
            } else {
                fallback
            }
        }
        Self {
            duty: clamp(duty, 0.5, 0.05, 1.0),
            batch: clamp(batch, 4.0, 1.0, 16.0).round() as usize,
            min_pause_ms: clamp(pause, 50.0, 0.0, 60_000.0).round() as u64,
            retry_ms: clamp(retry, 60_000.0, 1000.0, 3_600_000.0).round() as u64,
            recheck_ms: clamp(recheck, 300_000.0, 1000.0, 3_600_000.0).round() as u64,
        }
    }
}
pub struct Worker {
    options: Options,
    control: Control,
    verified: BTreeMap<Target, u64>,
    written: BTreeSet<Target>,
    pub held: Option<(Target, Held)>,
    pub last_pass: Option<Pass>,
    pub retry_at_ms: Option<u64>,
}
impl Worker {
    pub fn new(options: Options) -> Self {
        // Re-clamp public options before any division or batching.
        let options = Options::new(
            options.duty,
            options.batch as f64,
            options.min_pause_ms as f64,
            options.retry_ms as f64,
            options.recheck_ms as f64,
        );
        Self {
            options,
            control: Control::default(),
            verified: BTreeMap::new(),
            written: BTreeSet::new(),
            held: None,
            last_pass: None,
            retry_at_ms: None,
        }
    }
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    pub fn verified(&self, target: &Target) -> bool {
        self.verified.contains_key(target)
    }
    fn fresh(&self, host: &impl Host, target: &Target) -> bool {
        self.verified
            .get(target)
            .is_some_and(|at| host.now_ms().saturating_sub(*at) < self.options.recheck_ms)
    }
    async fn gate(&mut self, host: &impl Host) -> bool {
        let Some(next) = host.config().next else {
            return false;
        };
        if self.fresh(host, &next) {
            return true;
        }
        let was_verified = self.verified.contains_key(&next);
        let result = match host.served(&next).await {
            Ok(ids) if serves_model(&ids, &next.model) => Ok(()),
            Ok(_) => Err(Held::ModelNotServed),
            Err(_) => Err(Held::EndpointUnverified),
        };
        // Unlike Node's successful-check branch, never retain an obsolete answer
        // after a concurrent config change or shutdown.
        if self.control.stopped() || host.config().next.as_ref() != Some(&next) {
            return false;
        }
        if result.is_ok() {
            self.verified.insert(next.clone(), host.now_ms());
            self.written.remove(&next);
            self.held = None;
            return true;
        }
        self.verified.remove(&next);
        let reason = result.err().unwrap_or(Held::EndpointUnverified);
        self.held = Some((next.clone(), reason));
        if (was_verified || self.written.contains(&next)) && reason == Held::ModelNotServed {
            self.written.remove(&next);
            let c = host.config();
            if !next.same_identity(&c.active)
                && !c.previous.as_ref().is_some_and(|p| next.same_identity(p))
            {
                for index in host.indexes() {
                    let _ = host.drop_slot(&index, &next);
                }
            }
        }
        false
    }
    async fn fill_target(
        &mut self,
        host: &impl Host,
        index: &Index,
        target: &Target,
        version: u64,
    ) -> Result<Pass, String> {
        let mut out = Pass::default();
        let mut refused = BTreeSet::new();
        loop {
            if self.control.stopped()
                || self.control.suspended()
                || host.config().version != version
            {
                out.aborted = true;
                return Ok(out);
            }
            let c = host.config();
            let is_next = c.next.as_ref() == Some(target) && target != &c.active;
            if is_next && !self.fresh(host, target) && !self.gate(host).await {
                return Ok(out);
            }
            let started = host.now_ms();
            let busy_before = self.control.embedding_busy();
            let Some(r) = host
                .fill(
                    index,
                    target,
                    self.options.batch,
                    &refused,
                    version,
                    &self.control,
                )
                .await?
            else {
                out.failed = 1;
                return Ok(out);
            };
            out.embedded += r.embedded;
            if r.embedded > 0 && is_next {
                self.written.insert(target.clone());
            }
            refused.extend(r.refused.iter().copied());
            if r.attempted == 0 || r.remaining == 0 {
                out.refused = refused.len();
                return Ok(out);
            }
            if r.embedded == 0 && r.refused.is_empty() {
                out.failed = 1;
                return Ok(out);
            }
            let busy_after = self.control.embedding_busy();
            let busy = if busy_after.0 != busy_before.0 {
                busy_after.1.saturating_sub(busy_before.1)
            } else {
                host.now_ms().saturating_sub(started)
            };
            let pause =
                ((busy as f64) * (1.0 - self.options.duty) / self.options.duty).round() as u64;
            host.sleep(self.options.min_pause_ms.max(pause), &self.control)
                .await;
        }
    }
    async fn pass(&mut self, host: &impl Host) -> Pass {
        let version = host.config().version;
        let mut out = Pass::default();
        for index in host.indexes() {
            // Node takes targets anew per index so a held next is skipped thereafter.
            let c = host.config();
            let mut targets = vec![c.active];
            if let Some(next) = c.next.filter(|n| self.verified.contains_key(n)) {
                targets.push(next);
            }
            for target in targets {
                match self.fill_target(host, &index, &target, version).await {
                    Ok(r) => {
                        out.embedded += r.embedded;
                        out.failed += r.failed;
                        out.refused += r.refused;
                        if r.aborted {
                            out.aborted = true;
                            return out;
                        }
                    }
                    Err(error) => {
                        out.failed += 1;
                        out.error = Some(error.chars().take(200).collect());
                    }
                }
            }
        }
        out
    }
    /// Run explicitly requested work or a due retry. No task, timer, network or DB
    /// is started at construction. Owner drives this method from its event loop.
    /// Dropping the future leaves no spawned work; kick again to resume.
    pub async fn tick(&mut self, host: &impl Host) {
        if self.control.suspended() {
            return;
        }
        if self.control.stopped() {
            self.retry_at_ms = None;
            return;
        }
        let due = self.retry_at_ms.is_some_and(|at| host.now_ms() >= at);
        if due {
            self.retry_at_ms = None;
        }
        if !due && !self.control.pending.swap(false, Ordering::AcqRel) {
            return;
        }
        loop {
            self.control.pending.store(false, Ordering::Release);
            self.gate(host).await;
            let mut pass = self.pass(host).await;
            pass.at_ms = host.now_ms();
            if (pass.failed > 0
                || self
                    .held
                    .as_ref()
                    .is_some_and(|(t, _)| host.config().next.as_ref() == Some(t)))
                && host.config().next.is_some()
                && !self.control.stopped()
                && self.retry_at_ms.is_none()
            {
                self.retry_at_ms = Some(host.now_ms().saturating_add(self.options.retry_ms));
            }
            self.last_pass = Some(pass);
            if self.control.stopped() || !self.control.pending.swap(false, Ordering::AcqRel) {
                break;
            }
        }
        if self.control.stopped() {
            self.retry_at_ms = None;
        }
    }
}
pub fn serves_model(ids: &[String], model: &str) -> bool {
    fn canonical(s: &str) -> String {
        let lower = s.trim().to_lowercase();
        lower.strip_suffix(":latest").unwrap_or(&lower).to_owned()
    }
    let want = canonical(model);
    !want.is_empty() && ids.iter().any(|id| canonical(id) == want)
}
