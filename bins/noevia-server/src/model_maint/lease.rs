//! The model-maintenance lease authority (port contract 2026-11-04 row J). It replaces core
//! `model-maintenance-lease.cjs` as the single arbiter of the exclusive maintenance hold that the
//! preset reload and calibration of this front share with Node's chat, tune, calibration, sweep,
//! idle-unload, delete and prune.
//!
//! Exactly one lease exists at a time. Its holder is this front (a [`super::Hold`] taken by
//! [`super::Maint::acquire`]) or Node (its maintenance gate, through the private store listener:
//! `POST /v1/maintenance-lease/{acquire,renew,release,held}` with the per-start secret; core
//! `model-maintenance-lease.cjs` `createNativeLeaseClient`). The timing is Core's, unchanged:
//!
//! - a lease not renewed within [`TTL`] (180 s) ends by itself, so a holder that dies frees the others;
//! - it is renewed every [`RENEW`] (10 s);
//! - however often it is renewed it ends at its hard cap: [`HARD`] (60 min) unless the holder asks
//!   for longer (`maxMs`, calibration), clamped to [`HARD`, [`MAX`] (24 h)].
//!
//! Fencing: each grant carries a fresh random id and a generation that only grows. A holder acts only
//! while [`Authority::covers`] its exact (id, generation): once its lease has expired, been released
//! or been replaced, nothing it holds is honoured again (renew answers ended; release is a no-op).
//! A restart starts an empty table with a new `epoch`: no lease of an earlier process is granted again.
//! A Node that restarts announces a new `boot`; leases a previous Node boot held are dropped then, so a
//! crashed Node does not keep everyone waiting for its TTL.
//!
//! Node's chat admission is synchronous and stays in Node, so a lease this front holds is backed by a
//! hold of Node's local admission (the latch, core `model-maintenance-lease.cjs` routes, reached
//! through [`super::node_lease`]); the latch has no authority of its own and ends with this lease.
//!
//! Every table operation is synchronous under a std mutex that is never held across an await.

use rand_core::RngCore;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::time::Instant;

/// The release marker Noevia's release assembly pairs with core's client (`noevia-maintenance-lease-client/1`).
pub const AUTHORITY_MARKER: &str = "noevia-maintenance-lease-authority/1";

/// model-maintenance-lease.cjs `TTL_MS`.
pub const TTL: Duration = Duration::from_millis(180_000);
/// `RENEW_MS`.
pub const RENEW: Duration = Duration::from_millis(10_000);
/// `HARD_MS`.
pub const HARD: Duration = Duration::from_millis(60 * 60 * 1000);
/// `MAX_MS`.
pub const MAX: Duration = Duration::from_millis(24 * 60 * 60 * 1000);
/// The longest a Node acquire may wait for the current holder (inference-maintenance.cjs asks ≤30 s).
pub const MAX_WAIT: Duration = Duration::from_secs(30);

/// Who holds a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// This front's own work.
    Front,
    /// Node's maintenance gate, for the Node process start `boot`.
    Node { boot: String },
}

/// A lease granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub id: String,
    pub generation: u64,
    pub epoch: String,
    pub ttl: Duration,
    pub renew_every: Duration,
    pub hard: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Another holder has it: 409.
    Busy,
}

struct Entry {
    id: String,
    generation: u64,
    origin: Origin,
    renewed_until: Instant,
    hard_at: Instant,
    reason: String,
}

struct State {
    holder: Option<Entry>,
    generation: u64,
    node_boot: Option<String>,
}

struct Inner {
    state: Mutex<State>,
    epoch: String,
    changed: tokio::sync::Notify,
    ttl: Duration,
    hard: Duration,
    max: Duration,
}

/// The authority. Cheap to clone; every clone is the same table.
#[derive(Clone)]
pub struct Authority {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority")
            .field("held", &self.held())
            .finish_non_exhaustive()
    }
}

impl Default for Authority {
    fn default() -> Self {
        Self::new()
    }
}

fn random_hex() -> String {
    let mut b = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `capFor`: the hard cap a lease gets for `requested` (`None`: the default).
pub fn cap_for(requested: Option<Duration>, hard: Duration, max: Duration) -> Duration {
    match requested {
        None => hard,
        Some(r) => r.max(hard).min(hard.max(max)),
    }
}

impl Authority {
    pub fn new() -> Self {
        Self::with_timing(TTL, HARD, MAX)
    }

    pub fn with_timing(ttl: Duration, hard: Duration, max: Duration) -> Self {
        Authority {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    holder: None,
                    generation: 0,
                    node_boot: None,
                }),
                epoch: random_hex(),
                changed: tokio::sync::Notify::new(),
                ttl,
                hard,
                max,
            }),
        }
    }

    /// This process start's identity: grants of another epoch are never honoured.
    pub fn epoch(&self) -> &str {
        &self.inner.epoch
    }

    pub fn ttl(&self) -> Duration {
        self.inner.ttl
    }

    /// `renewMs`: `min(RENEW, max(1 ms, ttl / 3))`.
    pub fn renew_every(&self) -> Duration {
        RENEW.min((self.inner.ttl / 3).max(Duration::from_millis(1)))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.inner.state.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        }
    }

    /// Ends the current lease if it has expired. True when it did.
    fn expire(state: &mut State, now: Instant) -> bool {
        let gone = state
            .holder
            .as_ref()
            .is_some_and(|e| now >= e.renewed_until || now >= e.hard_at);
        if gone {
            if let Some(e) = state.holder.take() {
                let why = if now >= e.hard_at {
                    "passed its limit"
                } else {
                    "was not renewed"
                };
                eprintln!(
                    "[models] a maintenance lease ({}) {why}; chat and tuning may run again",
                    e.reason
                );
            }
        }
        gone
    }

    /// A Node process start: a different boot than the last one drops the leases earlier boots held.
    fn note_boot(state: &mut State, origin: &Origin) -> bool {
        let Origin::Node { boot } = origin else {
            return false;
        };
        if state.node_boot.as_deref() == Some(boot.as_str()) {
            return false;
        }
        state.node_boot = Some(boot.clone());
        let stale = state
            .holder
            .as_ref()
            .is_some_and(|e| matches!(&e.origin, Origin::Node { boot: b } if b != boot));
        if stale {
            state.holder = None;
            eprintln!(
                "[models] Node restarted; the maintenance lease its previous start held has ended"
            );
        }
        stale
    }

    /// Announces Node's process start (see [`Self::note_boot`]).
    pub fn node_session(&self, boot: &str) {
        let freed = Self::note_boot(
            &mut self.lock(),
            &Origin::Node {
                boot: boot.to_owned(),
            },
        );
        if freed {
            self.inner.changed.notify_waiters();
        }
    }

    /// Takes the lease now, or refuses with [`Refusal::Busy`]. Check and set in one step.
    pub fn try_acquire(
        &self,
        origin: Origin,
        requested: Option<Duration>,
        reason: &str,
    ) -> Result<Grant, Refusal> {
        let now = Instant::now();
        let cap = cap_for(requested, self.inner.hard, self.inner.max);
        let mut state = self.lock();
        Self::note_boot(&mut state, &origin);
        Self::expire(&mut state, now);
        if state.holder.is_some() {
            return Err(Refusal::Busy);
        }
        state.generation += 1;
        let entry = Entry {
            id: random_hex(),
            generation: state.generation,
            origin,
            renewed_until: now + self.inner.ttl,
            hard_at: now + cap,
            reason: reason.chars().take(300).collect(),
        };
        let grant = Grant {
            id: entry.id.clone(),
            generation: entry.generation,
            epoch: self.inner.epoch.clone(),
            ttl: self.inner.ttl,
            renew_every: self.renew_every(),
            hard: cap,
        };
        state.holder = Some(entry);
        Ok(grant)
    }

    /// Like [`Self::try_acquire`], waiting up to `wait` for the current lease to end. Cancel-safe:
    /// a dropped future has taken nothing (the grant is made in the same step that returns it).
    pub async fn acquire(
        &self,
        origin: Origin,
        requested: Option<Duration>,
        reason: &str,
        wait: Duration,
    ) -> Result<Grant, Refusal> {
        let deadline = Instant::now() + wait.min(MAX_WAIT);
        loop {
            let notified = self.inner.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.try_acquire(origin.clone(), requested, reason) {
                Ok(g) => return Ok(g),
                Err(r) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(r);
                    }
                    // Wake at the release, the current lease's own end, or the deadline.
                    let until = self.ends_at().map_or(deadline, |e| e.min(deadline));
                    tokio::select! {
                        () = notified => {}
                        () = tokio::time::sleep_until(until) => {}
                    }
                }
            }
        }
    }

    /// When the current lease ends unless renewed (`None`: none is held).
    fn ends_at(&self) -> Option<Instant> {
        self.lock()
            .holder
            .as_ref()
            .map(|e| e.renewed_until.min(e.hard_at))
    }

    fn matches(state: &State, id: &str, generation: Option<u64>) -> bool {
        state
            .holder
            .as_ref()
            .is_some_and(|e| e.id == id && generation.is_none_or(|g| g == e.generation))
    }

    /// Keeps the lease `id` from expiring. False when it has ended (or passed its hard cap).
    pub fn renew(&self, id: &str, generation: Option<u64>) -> bool {
        let now = Instant::now();
        let mut state = self.lock();
        let ended = Self::expire(&mut state, now);
        let ok = Self::matches(&state, id, generation);
        if ok {
            if let Some(e) = state.holder.as_mut() {
                e.renewed_until = now + self.inner.ttl;
            }
        }
        drop(state);
        if ended {
            self.inner.changed.notify_waiters();
        }
        ok
    }

    /// Updates the reason the lease shows (`setReason`); false when it has ended.
    pub fn set_reason(&self, id: &str, reason: &str) -> bool {
        let mut state = self.lock();
        if !Self::matches(&state, id, None) {
            return false;
        }
        if let Some(e) = state.holder.as_mut() {
            e.reason = reason.chars().take(300).collect();
        }
        true
    }

    /// Ends the lease `id`; idempotent. True when it was the current lease.
    pub fn release(&self, id: &str, generation: Option<u64>) -> bool {
        let mut state = self.lock();
        let ours = Self::matches(&state, id, generation);
        if ours {
            state.holder = None;
        }
        drop(state);
        if ours {
            self.inner.changed.notify_waiters();
        }
        ours
    }

    /// The fence: whether (`id`, `generation`) is the current, unexpired lease and stays so for `step`.
    pub fn covers(&self, id: &str, generation: u64, step: Duration) -> bool {
        let now = Instant::now();
        let state = self.lock();
        state.holder.as_ref().is_some_and(|e| {
            e.id == id
                && e.generation == generation
                && now + step < e.renewed_until
                && now + step < e.hard_at
        })
    }

    /// Whether any lease is held now.
    pub fn held(&self) -> bool {
        let now = Instant::now();
        let mut state = self.lock();
        let ended = Self::expire(&mut state, now);
        let held = state.holder.is_some();
        drop(state);
        if ended {
            self.inner.changed.notify_waiters();
        }
        held
    }

    /// Whether this front (not Node) holds it now.
    pub fn held_by_front(&self) -> bool {
        self.held()
            && self
                .lock()
                .holder
                .as_ref()
                .is_some_and(|e| e.origin == Origin::Front)
    }
}

// ---- The private store-listener routes Node's maintenance gate uses (core createNativeLeaseClient). ----

/// The bodies are tiny (`BODY_LIMIT` in Core).
pub const BODY_LIMIT: usize = 4096;
pub const ROUTE_PREFIX: &str = "/v1/maintenance-lease";
pub const OPS: &[&str] = &["acquire", "renew", "release", "held", "session"];

fn answer(status: u16, body: &serde_json::Value) -> axum::http::Response<axum::body::Body> {
    let mut res = axum::http::Response::new(axum::body::Body::from(body.to_string()));
    *res.status_mut() = axum::http::StatusCode::from_u16(status)
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    res.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("application/json"),
    );
    res.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    res
}

fn bad(message: &str) -> axum::http::Response<axum::body::Body> {
    answer(400, &serde_json::json!({ "error": message }))
}

fn is_hex32(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A whole number of milliseconds (`Number.isSafeInteger`), absent as `None`; `Err` otherwise.
fn millis(body: &serde_json::Value, key: &str) -> Result<Option<i64>, ()> {
    const SAFE: i64 = 9_007_199_254_740_991;
    match body.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .filter(|n| (-SAFE..=SAFE).contains(n))
            .or_else(|| {
                v.as_f64()
                    .filter(|f| f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_991.0)
                    .map(|f| f as i64)
            })
            .map(Some)
            .ok_or(()),
    }
}

/// Answers one Node operation. `body` is the already-authorised request's JSON.
pub async fn answer_node(
    authority: &Authority,
    op: &str,
    body: &serde_json::Value,
) -> axum::http::Response<axum::body::Body> {
    use serde_json::json;
    let Some(boot) = body
        .get("boot")
        .and_then(serde_json::Value::as_str)
        .filter(|b| !b.is_empty() && b.len() <= 64)
    else {
        return bad("boot required");
    };
    let origin = Origin::Node {
        boot: boot.to_owned(),
    };
    let reason = body
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let id = || {
        body.get("lease")
            .and_then(serde_json::Value::as_str)
            .filter(|l| is_hex32(l))
    };
    match op {
        "session" => {
            authority.node_session(boot);
            answer(200, &json!({ "epoch": authority.epoch() }))
        }
        "acquire" => {
            let (Ok(max), Ok(wait)) = (millis(body, "maxMs"), millis(body, "waitMs")) else {
                return bad("maxMs must be a whole number of milliseconds.");
            };
            let max = max.map(|m| Duration::from_millis(u64::try_from(m).unwrap_or(0)));
            let wait = Duration::from_millis(wait.and_then(|w| u64::try_from(w).ok()).unwrap_or(0));
            match authority.acquire(origin, max, reason, wait).await {
                Ok(g) => answer(
                    201,
                    &json!({
                        "lease": g.id,
                        "generation": g.generation,
                        "epoch": g.epoch,
                        "ttlMs": u64::try_from(g.ttl.as_millis()).unwrap_or(u64::MAX),
                        "renewMs": u64::try_from(g.renew_every.as_millis()).unwrap_or(u64::MAX),
                        "hardMs": u64::try_from(g.hard.as_millis()).unwrap_or(u64::MAX),
                    }),
                ),
                Err(Refusal::Busy) => answer(
                    409,
                    &json!({
                        "error": "Inference is busy. Wait for requests to finish before changing native presets.",
                        "code": "maintenance_held",
                        "chatActive": false,
                    }),
                ),
            }
        }
        "renew" => {
            let Some(id) = id() else {
                return answer(404, &json!({ "error": "Not found" }));
            };
            let generation = body.get("generation").and_then(serde_json::Value::as_u64);
            if !authority.renew(id, generation) {
                return answer(404, &json!({ "error": "Not found" }));
            }
            if !reason.is_empty() {
                authority.set_reason(id, reason);
            }
            answer(200, &json!({}))
        }
        "release" => {
            if let Some(id) = id() {
                let generation = body.get("generation").and_then(serde_json::Value::as_u64);
                authority.release(id, generation);
            }
            let mut res = axum::http::Response::new(axum::body::Body::empty());
            *res.status_mut() = axum::http::StatusCode::NO_CONTENT;
            res
        }
        "held" => answer(
            200,
            &json!({ "held": authority.held(), "front": authority.held_by_front() }),
        ),
        _ => answer(404, &json!({ "error": "Not found" })),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn node(boot: &str) -> Origin {
        Origin::Node { boot: boot.into() }
    }

    #[tokio::test(start_paused = true)]
    async fn exclusive_hold_and_busy() {
        let a = Authority::new();
        let g = a.try_acquire(Origin::Front, None, "reload").unwrap();
        assert_eq!(g.id.len(), 32);
        assert_eq!(g.ttl, TTL);
        assert_eq!(g.renew_every, RENEW);
        assert_eq!(g.hard, HARD);
        assert_eq!(a.try_acquire(node("n1"), None, "tune"), Err(Refusal::Busy));
        assert_eq!(a.try_acquire(Origin::Front, None, "x"), Err(Refusal::Busy));
        assert!(a.release(&g.id, Some(g.generation)));
        let g2 = a.try_acquire(node("n1"), None, "tune").unwrap();
        assert!(g2.generation > g.generation);
        assert_ne!(g2.id, g.id);
    }

    #[tokio::test(start_paused = true)]
    async fn caps_are_clamped_like_core() {
        assert_eq!(cap_for(None, HARD, MAX), HARD);
        assert_eq!(cap_for(Some(Duration::from_secs(1)), HARD, MAX), HARD);
        assert_eq!(
            cap_for(Some(Duration::from_secs(8 * 3600)), HARD, MAX),
            Duration::from_secs(8 * 3600)
        );
        assert_eq!(
            cap_for(Some(Duration::from_secs(48 * 3600)), HARD, MAX),
            MAX
        );
        let a = Authority::new();
        let g = a
            .try_acquire(
                Origin::Front,
                Some(Duration::from_secs(9 * 3600)),
                "calibration",
            )
            .unwrap();
        assert_eq!(g.hard, Duration::from_secs(9 * 3600));
    }

    #[tokio::test(start_paused = true)]
    async fn renewal_keeps_it_and_missing_renewal_ends_it() {
        let a = Authority::new();
        let g = a.try_acquire(Origin::Front, None, "r").unwrap();
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(170)).await;
            assert!(a.renew(&g.id, Some(g.generation)));
        }
        tokio::time::advance(TTL).await;
        assert!(!a.held());
        assert!(
            !a.renew(&g.id, Some(g.generation)),
            "an expired lease is not renewed"
        );
        assert!(a.try_acquire(node("n"), None, "t").is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn hard_expiry_ends_it_however_often_renewed() {
        let a = Authority::new();
        let g = a.try_acquire(Origin::Front, None, "r").unwrap();
        let mut renewed = 0;
        while a.renew(&g.id, Some(g.generation)) {
            renewed += 1;
            tokio::time::advance(RENEW).await;
            assert!(renewed < 1000);
        }
        assert_eq!(renewed, 360, "60 min of 10 s renewals");
        assert!(!a.held());
    }

    #[tokio::test(start_paused = true)]
    async fn lost_coverage_is_fenced() {
        let a = Authority::new();
        let g = a.try_acquire(Origin::Front, None, "r").unwrap();
        assert!(a.covers(&g.id, g.generation, Duration::from_secs(120)));
        // A step longer than what the last renewal covers may not start.
        tokio::time::advance(Duration::from_secs(70)).await;
        assert!(!a.covers(&g.id, g.generation, Duration::from_secs(120)));
        tokio::time::advance(TTL).await;
        assert!(!a.covers(&g.id, g.generation, Duration::ZERO));
        // A newer lease with the same holder kind does not revive the old one's fence.
        let g2 = a.try_acquire(Origin::Front, None, "r").unwrap();
        assert!(!a.covers(&g.id, g.generation, Duration::ZERO));
        assert!(!a.renew(&g.id, Some(g.generation)));
        assert!(
            !a.release(&g.id, Some(g.generation)),
            "a stale release frees nothing"
        );
        assert!(a.covers(&g2.id, g2.generation, Duration::ZERO));
        assert!(
            !a.covers(&g2.id, g.generation, Duration::ZERO),
            "wrong generation"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn release_is_idempotent() {
        let a = Authority::new();
        let g = a.try_acquire(Origin::Front, None, "r").unwrap();
        assert!(a.release(&g.id, None));
        assert!(!a.release(&g.id, None));
        assert!(!a.held());
    }

    #[tokio::test(start_paused = true)]
    async fn restart_grants_no_stale_lease() {
        let before = Authority::new();
        let g = before.try_acquire(node("boot-a"), None, "t").unwrap();
        let after = Authority::new();
        assert_ne!(before.epoch(), after.epoch());
        assert!(!after.held());
        assert!(!after.renew(&g.id, Some(g.generation)));
        assert!(!after.covers(&g.id, g.generation, Duration::ZERO));
        assert!(after.try_acquire(Origin::Front, None, "r").is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_node_restart_drops_its_previous_boots_lease() {
        let a = Authority::new();
        let g = a.try_acquire(node("boot-a"), None, "t").unwrap();
        a.node_session("boot-a");
        assert!(a.held(), "same boot keeps it");
        a.node_session("boot-b");
        assert!(!a.held());
        assert!(!a.renew(&g.id, None));
        // A front lease is not Node's to drop.
        let f = a.try_acquire(Origin::Front, None, "r").unwrap();
        a.node_session("boot-c");
        assert!(a.covers(&f.id, f.generation, Duration::ZERO));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_acquires_grant_exactly_one() {
        let a = Authority::new();
        let start = Arc::new(tokio::sync::Barrier::new(64));
        let mut tasks = Vec::new();
        for i in 0..64 {
            let (a, start) = (a.clone(), Arc::clone(&start));
            tasks.push(tokio::spawn(async move {
                start.wait().await;
                let origin = if i % 2 == 0 { Origin::Front } else { node("n") };
                a.try_acquire(origin, None, "race").is_ok()
            }));
        }
        let mut won = 0;
        for t in tasks {
            won += usize::from(t.await.unwrap());
        }
        assert_eq!(won, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiter_gets_it_on_release_and_cancellation_takes_nothing() {
        let a = Authority::new();
        let g = a.try_acquire(Origin::Front, None, "r").unwrap();
        // Cancelled while waiting: nothing is taken, the holder is unchanged.
        let waiting = {
            let a = a.clone();
            tokio::spawn(async move {
                a.acquire(node("n"), None, "t", Duration::from_secs(30))
                    .await
            })
        };
        tokio::time::advance(Duration::from_secs(1)).await;
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert!(a.covers(&g.id, g.generation, Duration::ZERO));
        // A waiter is woken by the release.
        let waiter = {
            let a = a.clone();
            tokio::spawn(async move {
                a.acquire(node("n"), None, "t", Duration::from_secs(30))
                    .await
            })
        };
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!waiter.is_finished());
        a.release(&g.id, Some(g.generation));
        let got = waiter.await.unwrap().unwrap();
        assert!(got.generation > g.generation);
        // Bounded: a wait that outlasts nothing answers busy.
        let busy = a
            .acquire(Origin::Front, None, "r", Duration::from_secs(5))
            .await;
        assert_eq!(busy, Err(Refusal::Busy));
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiter_is_woken_by_expiry() {
        let a = Authority::with_timing(Duration::from_secs(3), HARD, MAX);
        let _g = a.try_acquire(Origin::Front, None, "r").unwrap();
        let got = a
            .acquire(node("n"), None, "t", Duration::from_secs(10))
            .await;
        assert!(got.is_ok());
    }
}
