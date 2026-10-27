//! Ownership/security records outlive response replay and report-body eviction.
//! Controls may prove ownership but never install a network grant.
use super::session::OwnershipVerifier;
use egress::control::MAX_SAFE_INTEGER;
use egress::{Act, SignedGrant};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};
pub(super) const SECURITY_LIMIT: usize = 65_536;
const REPORT_LIMIT: usize = 500;
const HOST_LIMIT: usize = 50;
const SUMMARY_BYTES: usize = 48 * 1024;
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct Scope {
    pub session: String,
    pub task: String,
    pub nonce: String,
}
impl Scope {
    pub fn report_tag(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"noevia-egress-report/1\0");
        for field in [&self.session, &self.task, &self.nonce] {
            h.update((field.len() as u64).to_be_bytes());
            h.update(field.as_bytes());
        }
        h.finalize().into()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    Capacity,
    Ownership,
    Unavailable,
    Snapshot,
    Entropy,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Retirement {
    Revoked,
    Interrupted,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Location {
    Held,
    Evicted,
    Forgotten,
}
#[derive(Clone)]
pub(super) struct Snapshot {
    pub id: String,
    pub state: &'static str,
    pub final_report: bool,
    pub barrier_installed: bool,
    pub summary: Option<Value>,
    pub partial: bool,
    pub overflow: bool,
}
struct Record {
    digest: [u8; 32],
    iat: u64,
    exp: u64,
    owner: OwnershipVerifier,
    retired: Option<Retirement>,
    drained: bool,
    location: Location,
    frozen: Option<Arc<Snapshot>>,
    ack: Option<String>,
}
#[derive(Clone, Serialize)]
struct Row {
    host: String,
    allowed: u64,
    refused: u64,
    reason: Option<String>,
}
#[derive(Clone, Default)]
struct Summary {
    rows: Vec<Row>,
    partial: bool,
    overflow: bool,
}
impl Summary {
    fn value(&self, max: bool) -> Value {
        let mut rows = self.rows.clone();
        rows.sort_by(|a, b| {
            b.refused
                .cmp(&a.refused)
                .then(b.allowed.cmp(&a.allowed))
                .then(a.host.cmp(&b.host))
        });
        if max {
            for r in &mut rows {
                r.allowed = MAX_SAFE_INTEGER;
                r.refused = MAX_SAFE_INTEGER;
            }
        }
        let sum = |allowed: bool| {
            rows.iter().fold(0u64, |n, r| {
                n.saturating_add(if allowed { r.allowed } else { r.refused })
                    .min(MAX_SAFE_INTEGER)
            })
        };
        json!({"allowed":sum(true),"refused":sum(false),"hosts":rows})
    }
    fn bound(&self) -> bool {
        serde_json::to_vec(&self.value(true)).is_ok_and(|b| b.len() <= SUMMARY_BYTES)
    }
    fn event(&mut self, host: &str, allowed: bool, reason: Option<&str>) {
        if host.is_empty() {
            return;
        }
        let filtered = reason.and_then(controlled_reason);
        if reason.is_some_and(|v| !v.is_empty()) && filtered.is_none() {
            self.partial = true;
        }
        if escaped_json_bytes(host).is_none() {
            self.partial = true;
            return;
        }
        let pos = self.rows.iter().position(|r| r.host == host);
        if pos.is_none() && self.rows.len() >= HOST_LIMIT {
            self.partial = true;
            return;
        }
        // Cost-check the exact escaped identity/reason before retaining it. Counters
        // are budgeted at their largest representation, not just today's small values.
        let mut candidate = self.clone();
        let index = match pos {
            Some(i) => i,
            None => {
                candidate.rows.push(Row {
                    host: host.into(),
                    allowed: 0,
                    refused: 0,
                    reason: None,
                });
                candidate.rows.len() - 1
            }
        };
        if let Some(row) = candidate.rows.get_mut(index) {
            if !allowed && (filtered.is_some() || reason.is_none_or(str::is_empty)) {
                row.reason = filtered.clone();
            }
        }
        if !candidate.bound() {
            self.partial = true;
            if pos.is_none() {
                return;
            }
            candidate = self.clone(); // retain previous bounded reason and existing row
        }
        if let Some(row) = candidate.rows.get_mut(index) {
            let n = if allowed {
                &mut row.allowed
            } else {
                &mut row.refused
            };
            if *n == MAX_SAFE_INTEGER {
                candidate.partial = true;
                candidate.overflow = true;
            } else {
                *n += 1;
            }
        }
        for allowed in [true, false] {
            let total = candidate.rows.iter().fold(0u64, |n, r| {
                n.saturating_add(if allowed { r.allowed } else { r.refused })
            });
            if total > MAX_SAFE_INTEGER {
                candidate.partial = true;
                candidate.overflow = true;
            }
        }
        candidate.partial |= self.partial;
        candidate.overflow |= self.overflow;
        *self = candidate;
    }
}
// Exact JSON string escaping cost, stopping before copying unbounded host input.
fn escaped_json_bytes(s: &str) -> Option<usize> {
    let mut n = 2usize;
    for b in s.bytes() {
        let extra = match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
            0..=31 => 6,
            _ => 1,
        };
        n = n.checked_add(extra)?;
        if n > SUMMARY_BYTES {
            return None;
        }
    }
    Some(n)
}
fn controlled_reason(s: &str) -> Option<String> {
    const FIXED: &[&str] = &[
        "no valid task token",
        "unreadable target",
        "host is not on this task’s list",
        "host resolves to a private address",
        "host does not resolve",
        "task token was revoked",
        "grant is malformed",
        "grant version is not supported",
        "grant signature is invalid",
        "grant is not valid yet",
        "grant has expired",
        "grant was superseded by a newer grant for this task",
        "grant was revoked",
        "grant was already retired",
        "token is not a grant",
        "too many live grants",
    ];
    if FIXED.contains(&s) {
        return Some(s.into());
    }
    for (prefix, suffix, max) in [
        ("port ", " is not allowed", 65535u64),
        ("task has ", " connections open", 256u64),
    ] {
        if let Some(n) = s.strip_prefix(prefix).and_then(|x| x.strip_suffix(suffix)) {
            if let Ok(v) = n.parse::<u64>() {
                if v <= max && v.to_string() == n {
                    return Some(s.into());
                }
            }
        }
    }
    None
}
#[derive(Default)]
pub(super) struct Book {
    records: HashMap<Scope, Record>,
    reports: HashMap<Scope, Summary>,
    fifo: VecDeque<Scope>,
}
impl Book {
    pub fn known(&self, s: &Scope) -> bool {
        self.records.contains_key(s)
    }
    pub fn remember(
        &mut self,
        s: &Scope,
        claims: &SignedGrant,
        token: &str,
        owner: OwnershipVerifier,
    ) -> Result<(), Error> {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if let Some(r) = self.records.get(s) {
            if r.digest != digest || r.iat != claims.iat || r.exp != claims.exp {
                return Err(Error::Ownership);
            }
            return Ok(());
        }
        if self.records.len() >= SECURITY_LIMIT {
            return Err(Error::Capacity);
        }
        if claims.act != Act::Grant || claims.task != s.task || claims.nonce != s.nonce {
            return Err(Error::Ownership);
        }
        if self.reports.len() >= REPORT_LIMIT {
            if let Some(old) = self.fifo.pop_front() {
                self.reports.remove(&old);
                if let Some(r) = self.records.get_mut(&old) {
                    r.location = Location::Evicted;
                    r.frozen = None;
                }
            }
        }
        self.records.insert(
            s.clone(),
            Record {
                digest,
                iat: claims.iat,
                exp: claims.exp,
                owner,
                retired: None,
                drained: false,
                location: Location::Held,
                frozen: None,
                ack: None,
            },
        );
        self.reports.insert(s.clone(), Summary::default());
        self.fifo.push_back(s.clone());
        Ok(())
    }
    pub fn proof(&self, s: &Scope, token: &str, iat: Option<u64>) -> Result<(), Error> {
        let r = self.records.get(s).ok_or(Error::Unavailable)?;
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if r.digest != digest || iat.is_some_and(|v| v != r.iat) {
            return Err(Error::Ownership);
        }
        Ok(())
    }
    pub fn forgotten(&self, s: &Scope) -> bool {
        self.records
            .get(s)
            .is_some_and(|r| r.location == Location::Forgotten)
    }
    pub fn active(&self, s: &Scope) -> bool {
        self.records.get(s).is_some_and(|r| r.retired.is_none())
    }
    pub fn issued_at(&self, s: &Scope) -> Result<u64, Error> {
        self.records.get(s).map(|r| r.iat).ok_or(Error::Unavailable)
    }
    pub fn exp(&self, s: &Scope) -> Result<u64, Error> {
        self.records.get(s).map(|r| r.exp).ok_or(Error::Unavailable)
    }
    pub fn verify_revoke(
        &self,
        s: &Scope,
        token: &str,
        iat: u64,
        now: u64,
    ) -> Result<SignedGrant, Error> {
        let r = self.records.get(s).ok_or(Error::Unavailable)?;
        let revoke = r
            .owner
            .verify(token.as_bytes(), now)
            .map_err(|_| Error::Ownership)?;
        if r.iat != iat
            || revoke.act != Act::Revoke
            || revoke.task != s.task
            || !revoke.hosts.is_empty()
            || revoke.iat < r.iat
        {
            return Err(Error::Ownership);
        }
        Ok(revoke)
    }
    pub fn retire(&mut self, s: &Scope, why: Retirement) -> Result<(), Error> {
        let r = self.records.get_mut(s).ok_or(Error::Unavailable)?;
        if r.retired.is_none() {
            r.retired = Some(why);
        }
        Ok(())
    }
    pub fn retire_session(&mut self, session: &str) {
        for (s, r) in &mut self.records {
            if s.session == session && r.retired.is_none() {
                r.retired = Some(Retirement::Interrupted);
            }
        }
    }
    pub fn finalize(&mut self, s: &Scope) -> Result<(), Error> {
        let Some(r) = self.records.get_mut(s) else {
            return Ok(());
        };
        if r.retired.is_none() {
            return Ok(());
        }
        r.drained = true;
        if r.frozen.is_none() && r.location != Location::Forgotten {
            let snap = make_snapshot(r, self.reports.get(s), true)?;
            r.frozen = Some(Arc::new(snap));
        }
        Ok(())
    }
    pub fn event(&mut self, s: &Scope, host: &str, allowed: bool, reason: Option<&str>) {
        if self
            .records
            .get(s)
            .is_none_or(|r| r.frozen.is_some() || r.location != Location::Held)
        {
            return;
        }
        if let Some(report) = self.reports.get_mut(s) {
            report.event(host, allowed, reason);
        }
    }
    pub fn snapshot(&mut self, s: &Scope) -> Result<Snapshot, Error> {
        let r = self.records.get_mut(s).ok_or(Error::Unavailable)?;
        if let Some(f) = &r.frozen {
            return Ok((**f).clone());
        }
        if r.retired.is_some() && r.drained && r.location != Location::Forgotten {
            let snap = make_snapshot(r, self.reports.get(s), true)?;
            r.frozen = Some(Arc::new(snap.clone()));
            return Ok(snap);
        }
        make_snapshot(r, self.reports.get(s), r.retired.is_some() && r.drained)
    }
    pub fn forget(&mut self, s: &Scope, id: &str) -> Result<(), Error> {
        let r = self.records.get_mut(s).ok_or(Error::Unavailable)?;
        if r.location == Location::Forgotten && r.ack.as_deref() == Some(id) {
            return Ok(());
        }
        if !r.drained
            || r.retired.is_none()
            || r.frozen
                .as_ref()
                .is_none_or(|v| !v.final_report || v.id != id)
        {
            return Err(Error::Snapshot);
        }
        r.location = Location::Forgotten;
        r.ack = Some(id.into());
        r.frozen = None;
        self.reports.remove(s);
        self.fifo.retain(|x| x != s);
        Ok(())
    }
}
fn make_snapshot(
    r: &Record,
    summary: Option<&Summary>,
    final_report: bool,
) -> Result<Snapshot, Error> {
    if r.location == Location::Held && summary.is_none() {
        return Err(Error::Unavailable);
    }
    let id = if r.location == Location::Forgotten {
        r.ack.clone().ok_or(Error::Snapshot)?
    } else {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| Error::Entropy)?;
        if bytes.iter().all(|v| *v == 0) {
            return Err(Error::Entropy);
        }
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    };
    let state = match r.location {
        Location::Evicted => "evicted",
        Location::Forgotten => "forgotten",
        Location::Held => {
            if r.retired == Some(Retirement::Interrupted) {
                "interrupted"
            } else if summary.is_some_and(|s| s.partial) {
                "partial"
            } else {
                "complete"
            }
        }
    };
    Ok(Snapshot {
        id,
        state,
        final_report,
        barrier_installed: r.retired.is_some(),
        summary: if r.location == Location::Held {
            summary.map(|s| s.value(false))
        } else {
            None
        },
        partial: summary.is_some_and(|s| s.partial),
        overflow: summary.is_some_and(|s| s.overflow),
    })
}

#[cfg(test)]
mod tests {
    use super::super::session::Boot;
    use super::*;
    use egress::control::FileKey;
    fn owner() -> Result<OwnershipVerifier, Error> {
        Boot::generate()
            .map_err(|_| Error::Entropy)?
            .start_session(&FileKey::from_bytes([7; 32]))
            .map(|s| s.ownership_verifier())
            .map_err(|_| Error::Entropy)
    }
    fn scope(n: usize) -> Scope {
        Scope {
            session: "0123456789abcdef0123456789abcdef".into(),
            task: format!("task.{n}"),
            nonce: "AAECAwQFBgcICQoLDA0ODw".into(),
        }
    }
    // Unit bookkeeping fixture. Engine tests separately prove prior cryptographic
    // admission; Book has no public or unverified network-authority constructor.
    fn remember(b: &mut Book, s: &Scope) -> Result<(), Error> {
        let g = SignedGrant {
            act: Act::Grant,
            task: s.task.clone(),
            hosts: vec!["example.com".into()],
            iat: 1,
            exp: 2,
            idle: 1,
            nonce: s.nonce.clone(),
        };
        b.remember(s, &g, "synthetic-proof", owner()?)
    }
    #[test]
    fn retained_decision_counts_sort_and_freeze_without_late_mutation() -> Result<(), Error> {
        let mut b = Book::default();
        let s = scope(1);
        remember(&mut b, &s)?;
        b.event(&s, "b.example", true, None);
        b.event(&s, "b.example", true, None);
        b.event(&s, "a.example", false, Some("host does not resolve"));
        let active = b.snapshot(&s)?;
        assert!(!active.final_report);
        assert_eq!(active.state, "complete");
        let data = active.summary.ok_or(Error::Unavailable)?;
        assert_eq!(data.get("allowed"), Some(&json!(2)));
        assert_eq!(data.get("refused"), Some(&json!(1)));
        let rows = data
            .get("hosts")
            .and_then(Value::as_array)
            .ok_or(Error::Unavailable)?;
        assert_eq!(
            rows.first().and_then(|r| r.get("host")),
            Some(&json!("a.example"))
        );
        b.retire(&s, Retirement::Revoked)?;
        b.finalize(&s)?;
        let frozen = b.snapshot(&s)?;
        assert!(frozen.final_report);
        assert!(frozen.barrier_installed);
        b.event(&s, "late.example", true, None);
        let after = b.snapshot(&s)?;
        assert_eq!(frozen.id, after.id);
        assert_eq!(frozen.summary, after.summary);
        Ok(())
    }
    #[test]
    fn report_eviction_and_final_forget_do_not_erase_ownership_or_retirement() -> Result<(), Error>
    {
        let mut b = Book::default();
        let old = scope(0);
        remember(&mut b, &old)?;
        for n in 1..=REPORT_LIMIT {
            remember(&mut b, &scope(n))?;
        }
        b.proof(&old, "synthetic-proof", Some(1))?;
        let evicted = b.snapshot(&old)?;
        assert_eq!(evicted.state, "evicted");
        assert!(evicted.summary.is_none());
        b.retire(&old, Retirement::Interrupted)?;
        b.finalize(&old)?;
        let final_report = b.snapshot(&old)?;
        assert!(final_report.final_report);
        assert_eq!(b.forget(&old, "wrong"), Err(Error::Snapshot));
        b.forget(&old, &final_report.id)?;
        b.forget(&old, &final_report.id)?;
        assert!(!b.active(&old));
        b.proof(&old, "synthetic-proof", None)?;
        assert_eq!(b.snapshot(&old)?.state, "forgotten");
        assert_eq!(b.snapshot(&old)?.id, final_report.id);
        assert_eq!(b.proof(&old, "changed-proof", None), Err(Error::Ownership));
        Ok(())
    }
    #[test]
    fn host_and_json_byte_caps_preserve_identity_and_mark_partial() -> Result<(), Error> {
        let mut s = Summary::default();
        for n in 0..HOST_LIMIT {
            s.event(&format!("h{n}.example"), true, None);
        }
        s.event("omitted.example", true, None);
        assert!(s.partial);
        assert_eq!(s.rows.len(), HOST_LIMIT);
        assert_eq!(s.value(false).get("allowed"), Some(&json!(50)));
        let mut s = Summary::default();
        let escaped = "a\"\\".repeat(11_000);
        s.event(&escaped, false, Some("host does not resolve"));
        assert!(s.partial);
        assert!(s.rows.is_empty());
        let host = "a".repeat(24_000);
        s.event(&host, true, None);
        s.event(&format!("{host}b"), true, None);
        s.event(&format!("{host}c"), true, None);
        assert!(s.partial);
        assert!(
            serde_json::to_vec(&s.value(false))
                .map_err(|_| Error::Unavailable)?
                .len()
                <= SUMMARY_BYTES
        );
        let unsafe_reason = "https://secret.invalid/path?token=".repeat(1000);
        s.event("safe.example", false, Some(&unsafe_reason));
        let encoded = s.value(false).to_string();
        assert!(!encoded.contains("token="));
        assert!(s.partial);
        Ok(())
    }
    #[test]
    fn row_and_aggregate_counter_overflow_saturate_without_wrapping() {
        let mut s = Summary {
            rows: vec![
                Row {
                    host: "a.example".into(),
                    allowed: MAX_SAFE_INTEGER,
                    refused: 0,
                    reason: None,
                },
                Row {
                    host: "b.example".into(),
                    allowed: 1,
                    refused: MAX_SAFE_INTEGER,
                    reason: None,
                },
            ],
            partial: false,
            overflow: false,
        };
        s.event("a.example", true, None);
        s.event("b.example", false, Some("host does not resolve"));
        assert!(s.partial && s.overflow);
        assert_eq!(
            s.value(false).get("allowed"),
            Some(&json!(MAX_SAFE_INTEGER))
        );
        assert_eq!(
            s.value(false).get("refused"),
            Some(&json!(MAX_SAFE_INTEGER))
        );
    }
    #[test]
    fn actual_security_record_limit_refuses_without_evicting_live_ownership() -> Result<(), Error> {
        let mut b = Book::default();
        let owner = owner()?;
        for n in 0..SECURITY_LIMIT {
            let s = scope(n);
            let g = SignedGrant {
                act: Act::Grant,
                task: s.task.clone(),
                hosts: vec!["example.com".into()],
                iat: 1,
                exp: 2,
                idle: 1,
                nonce: s.nonce.clone(),
            };
            b.remember(&s, &g, "synthetic-proof", owner.clone())?;
        }
        let s = scope(SECURITY_LIMIT);
        let g = SignedGrant {
            act: Act::Grant,
            task: s.task.clone(),
            hosts: vec!["example.com".into()],
            iat: 1,
            exp: 2,
            idle: 1,
            nonce: s.nonce.clone(),
        };
        assert_eq!(SECURITY_LIMIT, 65_536);
        assert_eq!(
            b.remember(&s, &g, "synthetic-proof", owner),
            Err(Error::Capacity)
        );
        assert_eq!(b.records.len(), 65_536);
        assert_eq!(b.reports.len(), REPORT_LIMIT);
        b.proof(&scope(0), "synthetic-proof", Some(1))?;
        Ok(())
    }
    #[test]
    fn aggregate_overflow_is_flagged_even_without_individual_counter_overflow() {
        let mut s = Summary {
            rows: vec![
                Row {
                    host: "a.example".into(),
                    allowed: MAX_SAFE_INTEGER - 1,
                    refused: 0,
                    reason: None,
                },
                Row {
                    host: "b.example".into(),
                    allowed: 2,
                    refused: 0,
                    reason: None,
                },
            ],
            partial: false,
            overflow: false,
        };
        s.event("b.example", false, Some("host does not resolve"));
        assert!(s.partial && s.overflow);
        assert_eq!(
            s.value(false).get("allowed"),
            Some(&json!(MAX_SAFE_INTEGER))
        );
        assert!(s
            .rows
            .iter()
            .all(|r| r.allowed < MAX_SAFE_INTEGER && r.refused < MAX_SAFE_INTEGER));
    }
    #[test]
    fn missing_held_report_never_becomes_complete_empty_success() -> Result<(), Error> {
        let mut b = Book::default();
        let s = scope(1);
        remember(&mut b, &s)?;
        b.reports.remove(&s);
        assert_eq!(b.snapshot(&s).err(), Some(Error::Unavailable));
        b.retire(&s, Retirement::Revoked)?;
        assert_eq!(b.finalize(&s), Err(Error::Unavailable));
        assert_eq!(b.snapshot(&s).err(), Some(Error::Unavailable));
        Ok(())
    }
}
