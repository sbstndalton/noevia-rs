//! Bounded pre-mutation reservation and exact result replay for the native controller.
//! Authenticate the request before access. Cached results are historical, not proof
//! of live readiness. Security grant/revoke tombstones must live elsewhere.
use egress::control::{MAX_RESPONSE_BYTES, MAX_SAFE_INTEGER};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
pub const MAX_ENTRIES: usize = 4096;
pub const MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;
pub const RETENTION: Duration = Duration::from_secs(60);
pub const MAX_FRESHNESS_MS: u64 = 30_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Malformed,
    Stale,
    Conflict,
    Capacity,
    Clock,
    Ticket,
    Result,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "control replay {self:?}")
    }
}
impl std::error::Error for Error {}
/// Single-use capability for one reservation, not a network or live-session capability.
/// Dropping it deliberately leaves Pending: cancellation may already have mutated state.
pub struct Ticket {
    seal: Arc<()>,
    id: String,
    generation: u64,
}
impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReplayTicket(<redacted>)")
    }
}
#[derive(Clone)]
pub struct Cached {
    pub status: u16,
    pub body: Arc<[u8]>,
}
impl std::fmt::Debug for Cached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cached")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}
#[derive(Debug)]
pub enum Admission {
    Execute(Ticket),
    Pending,
    Cached(Cached),
    Unavailable,
}
enum Outcome {
    Pending,
    Cached(Cached),
    Unavailable,
}
struct Entry {
    request_mac: String,
    generation: u64,
    expires: Instant,
    deadline_ms: u64,
    outcome: Outcome,
    report_scope: Option<[u8; 32]>,
    charge: usize,
}
/// One owner serializes access with controller mutation admission. No live entry
/// eviction or clearing on key/session replacement; reserve before executing.
pub struct Replay {
    seal: Arc<()>,
    entries: HashMap<String, Entry>,
    charge: usize,
    generation: u64,
    last: Instant,
    wall_high_water: u64,
    entry_limit: usize,
    byte_limit: usize,
}
impl Default for Replay {
    fn default() -> Self {
        Self::new()
    }
}
impl Replay {
    pub fn new() -> Self {
        Self::with_limits(Instant::now(), MAX_ENTRIES, MAX_RETAINED_BYTES)
    }
    fn with_limits(now: Instant, entry_limit: usize, byte_limit: usize) -> Self {
        Self {
            seal: Arc::new(()),
            entries: HashMap::new(),
            charge: 0,
            generation: 0,
            last: now,
            wall_high_water: 0,
            entry_limit,
            byte_limit,
        }
    }
    pub fn begin(
        &mut self,
        id: &str,
        request_mac: &str,
        deadline_ms: u64,
        wall_ms: u64,
    ) -> Result<Admission, Error> {
        self.begin_at(id, request_mac, deadline_ms, wall_ms, Instant::now())
    }
    fn begin_at(
        &mut self,
        id: &str,
        request_mac: &str,
        deadline_ms: u64,
        wall_ms: u64,
        now: Instant,
    ) -> Result<Admission, Error> {
        if !hex(id, 32)
            || !hex(request_mac, 64)
            || wall_ms == 0
            || wall_ms > MAX_SAFE_INTEGER
            || deadline_ms > MAX_SAFE_INTEGER
        {
            return Err(Error::Malformed);
        }
        let remaining = deadline_ms.checked_sub(wall_ms).ok_or(Error::Stale)?;
        if remaining == 0 || remaining > MAX_FRESHNESS_MS {
            return Err(Error::Stale);
        }
        self.purge_at(now, Some(wall_ms))?;
        if let Some(e) = self.entries.get(id) {
            if e.request_mac != request_mac {
                return Err(Error::Conflict);
            }
            return Ok(match &e.outcome {
                Outcome::Pending => Admission::Pending,
                Outcome::Cached(c) => Admission::Cached(c.clone()),
                Outcome::Unavailable => Admission::Unavailable,
            });
        }
        let next_charge = self
            .charge
            .checked_add(MAX_RESPONSE_BYTES)
            .ok_or(Error::Capacity)?;
        if self.entries.len() >= self.entry_limit || next_charge > self.byte_limit {
            return Err(Error::Capacity);
        }
        let generation = self.generation.checked_add(1).ok_or(Error::Capacity)?;
        let expires = now.checked_add(RETENTION).ok_or(Error::Clock)?;
        self.generation = generation;
        self.charge = next_charge;
        self.entries.insert(
            id.into(),
            Entry {
                request_mac: request_mac.into(),
                generation,
                expires,
                deadline_ms,
                outcome: Outcome::Pending,
                report_scope: None,
                charge: MAX_RESPONSE_BYTES,
            },
        );
        Ok(Admission::Execute(Ticket {
            seal: Arc::clone(&self.seal),
            id: id.into(),
            generation,
        }))
    }
    /// Complete after mutation. Any invalid output becomes a retained Unavailable
    /// outcome, so retries cannot run the mutation again. Sign that explicit failure.
    pub fn complete(&mut self, ticket: Ticket, status: u16, body: Vec<u8>) -> Result<(), Error> {
        self.complete_at(ticket, status, body, Instant::now())
    }
    pub(super) fn complete_scoped(
        &mut self,
        ticket: Ticket,
        status: u16,
        body: Vec<u8>,
        scope: [u8; 32],
    ) -> Result<(), Error> {
        self.complete_tagged_at(ticket, status, body, Some(scope), Instant::now())
    }
    /// Acknowledged report privacy deletion keeps the execution fence intact.
    /// Future retries are explicitly unavailable, never successful empty output.
    pub(super) fn redact_reports(&mut self, scope: [u8; 32]) {
        for e in self
            .entries
            .values_mut()
            .filter(|e| e.report_scope == Some(scope))
        {
            if matches!(e.outcome, Outcome::Cached(_)) {
                self.charge = self.charge.saturating_sub(e.charge);
                e.charge = 0;
                e.outcome = Outcome::Unavailable;
            }
        }
    }
    fn complete_at(
        &mut self,
        ticket: Ticket,
        status: u16,
        body: Vec<u8>,
        now: Instant,
    ) -> Result<(), Error> {
        self.complete_tagged_at(ticket, status, body, None, now)
    }
    fn complete_tagged_at(
        &mut self,
        ticket: Ticket,
        status: u16,
        body: Vec<u8>,
        scope: Option<[u8; 32]>,
        now: Instant,
    ) -> Result<(), Error> {
        self.purge_at(now, None)?;
        if !Arc::ptr_eq(&ticket.seal, &self.seal) {
            return Err(Error::Ticket);
        }
        let e = self.entries.get_mut(&ticket.id).ok_or(Error::Ticket)?;
        if e.generation != ticket.generation || !matches!(e.outcome, Outcome::Pending) {
            return Err(Error::Ticket);
        }
        self.charge = self.charge.checked_sub(e.charge).ok_or(Error::Result)?;
        e.charge = 0;
        e.outcome = Outcome::Unavailable;
        if !(100..=599).contains(&status) || body.len() > MAX_RESPONSE_BYTES {
            return Err(Error::Result);
        }
        e.charge = body.len();
        self.charge = self.charge.checked_add(e.charge).ok_or(Error::Result)?;
        e.report_scope = scope;
        e.outcome = Outcome::Cached(Cached {
            status,
            body: Arc::from(body.into_boxed_slice()),
        });
        Ok(())
    }
    /// Explicit uncertain outcome after cancellation; retains ID/MAC conflict fence.
    pub fn unavailable(&mut self, ticket: Ticket) -> Result<(), Error> {
        self.unavailable_at(ticket, Instant::now())
    }
    fn unavailable_at(&mut self, ticket: Ticket, now: Instant) -> Result<(), Error> {
        self.purge_at(now, None)?;
        if !Arc::ptr_eq(&ticket.seal, &self.seal) {
            return Err(Error::Ticket);
        }
        let e = self.entries.get_mut(&ticket.id).ok_or(Error::Ticket)?;
        if e.generation != ticket.generation || !matches!(e.outcome, Outcome::Pending) {
            return Err(Error::Ticket);
        }
        self.charge = self.charge.checked_sub(e.charge).ok_or(Error::Result)?;
        e.charge = 0;
        e.outcome = Outcome::Unavailable;
        Ok(())
    }
    fn purge_at(&mut self, now: Instant, wall_ms: Option<u64>) -> Result<(), Error> {
        if now < self.last || wall_ms.is_some_and(|v| v < self.wall_high_water) {
            return Err(Error::Clock);
        }
        self.last = now;
        if let Some(wall) = wall_ms {
            self.wall_high_water = wall;
        }
        let mut charge = 0usize;
        self.entries.retain(|_, e| {
            if now >= e.expires && wall_ms.is_some_and(|wall| wall >= e.deadline_ms) {
                false
            } else {
                charge = charge.saturating_add(e.charge);
                true
            }
        });
        self.charge = charge;
        Ok(())
    }
}
fn hex(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "00112233445566778899aabbccddeeff";
    const MAC: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    fn ticket(a: Admission) -> Result<Ticket, Error> {
        if let Admission::Execute(t) = a {
            Ok(t)
        } else {
            Err(Error::Ticket)
        }
    }
    #[test]
    fn pending_completion_and_exact_body_replay_never_execute_twice() -> Result<(), Error> {
        let now = Instant::now();
        let mut r = Replay::with_limits(now, 2, MAX_RETAINED_BYTES);
        let t = ticket(r.begin_at(ID, MAC, 31_000, 1000, now)?)?;
        assert!(matches!(
            r.begin_at(ID, MAC, 31_000, 1001, now)?,
            Admission::Pending
        ));
        assert_eq!(
            r.begin_at(ID, &"0".repeat(64), 31_000, 1001, now).err(),
            Some(Error::Conflict)
        );
        let body = vec![0, 255, 10, 13];
        r.complete_at(t, 409, body.clone(), now)?;
        for _ in 0..3 {
            if let Admission::Cached(c) = r.begin_at(ID, MAC, 31_000, 1001, now)? {
                assert_eq!(c.status, 409);
                assert_eq!(&*c.body, body.as_slice());
            } else {
                return Err(Error::Result);
            }
        }
        assert_eq!(r.charge, 4);
        assert_eq!(r.entries.len(), 1);
        Ok(())
    }
    #[test]
    fn capacity_is_reserved_before_mutation_without_evicting_live_results() -> Result<(), Error> {
        let now = Instant::now();
        let mut r = Replay::with_limits(now, 3, MAX_RESPONSE_BYTES);
        let t = ticket(r.begin_at(ID, MAC, 2000, 1000, now)?)?;
        assert_eq!(
            r.begin_at(&"0".repeat(32), MAC, 2000, 1000, now).err(),
            Some(Error::Capacity)
        );
        r.complete_at(t, 200, vec![1; MAX_RESPONSE_BYTES], now)?;
        assert_eq!(
            r.begin_at(&"0".repeat(32), MAC, 2000, 1000, now).err(),
            Some(Error::Capacity)
        );
        assert!(matches!(
            r.begin_at(ID, MAC, 2000, 1000, now)?,
            Admission::Cached(_)
        ));
        let mut r = Replay::with_limits(now, 1, MAX_RETAINED_BYTES);
        let t = ticket(r.begin_at(ID, MAC, 2000, 1000, now)?)?;
        r.complete_at(t, 200, vec![], now)?;
        assert_eq!(
            r.begin_at(&"0".repeat(32), MAC, 2000, 1000, now).err(),
            Some(Error::Capacity)
        );
        assert_eq!(MAX_ENTRIES, 4096);
        assert_eq!(MAX_RETAINED_BYTES, 8 * 1024 * 1024);
        Ok(())
    }
    #[test]
    fn dropped_and_uncertain_tickets_never_allow_retry_execution() -> Result<(), Error> {
        let now = Instant::now();
        let mut r = Replay::with_limits(now, 3, MAX_RETAINED_BYTES);
        drop(ticket(r.begin_at(ID, MAC, 2000, 1000, now)?)?);
        assert!(matches!(
            r.begin_at(ID, MAC, 2000, 1001, now)?,
            Admission::Pending
        ));
        let id = "0".repeat(32);
        let t = ticket(r.begin_at(&id, MAC, 2000, 1001, now)?)?;
        r.unavailable_at(t, now)?;
        assert!(matches!(
            r.begin_at(&id, MAC, 2000, 1001, now)?,
            Admission::Unavailable
        ));
        assert_eq!(
            r.begin_at(&id, &"0".repeat(64), 2000, 1001, now).err(),
            Some(Error::Conflict)
        );
        Ok(())
    }
    #[test]
    fn malformed_output_and_cross_cache_tickets_fail_without_reexecution() -> Result<(), Error> {
        let now = Instant::now();
        for (status, len) in [(99, 1), (600, 1), (200, MAX_RESPONSE_BYTES + 1)] {
            let mut r = Replay::with_limits(now, 3, MAX_RETAINED_BYTES);
            let t = ticket(r.begin_at(ID, MAC, 2000, 1000, now)?)?;
            assert_eq!(
                r.complete_at(t, status, vec![0; len], now),
                Err(Error::Result)
            );
            assert!(matches!(
                r.begin_at(ID, MAC, 2000, 1001, now)?,
                Admission::Unavailable
            ));
            assert_eq!(r.charge, 0);
        }
        let mut a = Replay::with_limits(now, 3, MAX_RETAINED_BYTES);
        let mut b = Replay::with_limits(now, 3, MAX_RETAINED_BYTES);
        let ta = ticket(a.begin_at(ID, MAC, 2000, 1000, now)?)?;
        let tb = ticket(b.begin_at(ID, MAC, 2000, 1000, now)?)?;
        assert_eq!(b.complete_at(ta, 200, vec![], now), Err(Error::Ticket));
        b.complete_at(tb, 200, vec![1], now)?;
        assert!(matches!(
            a.begin_at(ID, MAC, 2000, 1001, now)?,
            Admission::Pending
        ));
        Ok(())
    }
    #[test]
    fn retention_does_not_slide_and_expired_original_requests_cannot_reexecute() -> Result<(), Error>
    {
        let now = Instant::now();
        let mut r = Replay::with_limits(now, 3, MAX_RETAINED_BYTES);
        let t = ticket(r.begin_at(ID, MAC, 31_000, 1000, now)?)?;
        r.complete_at(t, 200, vec![1], now)?;
        assert!(matches!(
            r.begin_at(ID, MAC, 31_000, 30_999, now + Duration::from_secs(29))?,
            Admission::Cached(_)
        ));
        assert_eq!(
            r.begin_at(ID, MAC, 31_000, 31_000, now + RETENTION).err(),
            Some(Error::Stale)
        );
        r.purge_at(now + RETENTION, Some(61_000))?;
        assert_eq!(r.charge, 0);
        assert!(r.entries.is_empty());
        assert_eq!(
            r.begin_at(ID, MAC, 31_000, 61_000, now + RETENTION).err(),
            Some(Error::Stale)
        );
        assert!(matches!(
            r.begin_at(ID, &"0".repeat(64), 62_000, 61_000, now + RETENTION)?,
            Admission::Execute(_)
        ));
        Ok(())
    }
    #[test]
    fn frozen_or_regressed_wall_time_cannot_revive_a_purged_mutation() -> Result<(), Error> {
        let now = Instant::now();
        let mut r = Replay::with_limits(now, 3, MAX_RETAINED_BYTES);
        let t = ticket(r.begin_at(ID, MAC, 31_000, 1000, now)?)?;
        r.complete_at(t, 200, vec![1], now)?;
        // Native retention passed, but the original request is still wall-fresh:
        // keep its exact result; do not evict/re-execute on a stalled wall clock.
        assert!(matches!(
            r.begin_at(ID, MAC, 31_000, 1001, now + RETENTION)?,
            Admission::Cached(_)
        ));
        let next = "0".repeat(32);
        assert!(matches!(
            r.begin_at(&next, MAC, 62_000, 61_000, now + RETENTION)?,
            Admission::Execute(_)
        ));
        assert!(!r.entries.contains_key(ID));
        assert_eq!(
            r.begin_at(ID, MAC, 31_000, 1000, now + RETENTION).err(),
            Some(Error::Clock)
        );
        Ok(())
    }
    #[test]
    fn freshness_clock_and_generation_bounds_fail_closed() -> Result<(), Error> {
        let now = Instant::now();
        let mut r = Replay::with_limits(now, 3, MAX_RETAINED_BYTES);
        for (deadline, wall) in [
            (1000, 1000),
            (999, 1000),
            (31_001, 1000),
            (MAX_SAFE_INTEGER + 1, 1000),
            (1, 0),
        ] {
            assert!(r.begin_at(ID, MAC, deadline, wall, now).is_err());
        }
        assert_eq!(
            r.begin_at("invalid", MAC, 2000, 1000, now).err(),
            Some(Error::Malformed)
        );
        assert_eq!(
            r.begin_at(ID, "invalid", 2000, 1000, now).err(),
            Some(Error::Malformed)
        );
        let t = ticket(r.begin_at(ID, MAC, 2000, 1000, now + Duration::from_secs(1))?)?;
        assert_eq!(r.complete_at(t, 200, vec![], now), Err(Error::Clock));
        assert!(matches!(
            r.begin_at(ID, MAC, 2000, 1001, now + Duration::from_secs(2))?,
            Admission::Pending
        ));
        r.generation = u64::MAX;
        assert_eq!(
            r.begin_at(
                &"0".repeat(32),
                MAC,
                2000,
                1001,
                now + Duration::from_secs(2)
            )
            .err(),
            Some(Error::Capacity)
        );
        Ok(())
    }
    #[test]
    fn acknowledged_scoped_body_redaction_preserves_fences_and_unrelated_results(
    ) -> Result<(), Error> {
        let mut r = Replay::new();
        let a = ticket(r.begin(ID, MAC, 2000, 1000)?)?;
        r.complete_scoped(a, 200, b"report".to_vec(), [1; 32])?;
        let other = "0".repeat(32);
        let b = ticket(r.begin(&other, MAC, 2000, 1000)?)?;
        r.complete_scoped(b, 200, b"keep".to_vec(), [2; 32])?;
        r.redact_reports([1; 32]);
        assert_eq!(r.charge, 4);
        assert!(matches!(
            r.begin(ID, MAC, 2000, 1001)?,
            Admission::Unavailable
        ));
        assert_eq!(
            r.begin(ID, &"0".repeat(64), 2000, 1001).err(),
            Some(Error::Conflict)
        );
        assert!(matches!(
            r.begin(&other, MAC, 2000, 1001)?,
            Admission::Cached(_)
        ));
        Ok(())
    }
}
