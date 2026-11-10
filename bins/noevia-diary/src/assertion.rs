//! Tenant assertion checks, ported from tenant_assertion.py (shape, restart guard, bounded nonce
//! cache) on top of the `tenant-assertion` crate. The verifier of every request Python forwards
//! with `x-noevia-diary-verify: 1` (server.rs): Python does not check those itself; it records the
//! nonce of each one this side reports verified in its own cache, so a nonce consumed here is
//! refused by Python's verifier too.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;

pub const SKEW_S: f64 = 60.0;
pub const RESTART_SKEW_S: i64 = 1;
pub const NONCE_CAP: usize = 100_000;
/// The body hash signed for a non-JSON body (tenant_assertion.STREAM).
pub const STREAM: &str = "stream";

/// tenant_assertion.body_is_hashed: JSON or untyped bodies are hashed, any other media type signs
/// STREAM.
pub fn body_is_hashed(content_type: &str) -> bool {
    let media = content_type.split(';').next().unwrap_or("");
    let media = crate::py::strip(media).to_lowercase();
    media.is_empty() || media == "application/json"
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The header fields the assertion covers, as latin-1 strings (Starlette's decoding).
pub struct Signed<'a> {
    pub user_id: &'a str,
    pub assertion: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a [u8],
    pub body_hash: &'a str,
    pub storage: &'a str,
    pub legacy_owner: &'a str,
    pub blocked: &'a str,
}

/// `v2.(\d{1,12}).([0-9a-f]{32}).([A-Za-z0-9_-]{43})` (ASCII digits: the pattern is applied to
/// a str, but `\d` here must also parse with `int`, and the leaf refuses non-ASCII anyway; a
/// non-ASCII digit makes Python spawn the leaf, which refuses, so both answer 401).
fn shape(raw: &str) -> Option<(i64, &str)> {
    let rest = raw.strip_prefix("v2.")?;
    let mut parts = rest.splitn(3, '.');
    let ts = parts.next()?;
    let nonce = parts.next()?;
    let sig = parts.next()?;
    if ts.is_empty() || ts.len() > 12 || !ts.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    if sig.len() != 43
        || !sig
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some((ts.parse().ok()?, nonce))
}

#[derive(Default)]
struct Cache {
    seen: BTreeMap<i64, HashSet<String>>,
    count: usize,
    floor: i64,
    lost: HashSet<i64>,
}

impl Cache {
    fn drop_second(&mut self, second: i64, now: f64) {
        if let Some(bucket) = self.seen.remove(&second) {
            self.count -= bucket.len();
        }
        #[allow(clippy::cast_precision_loss)]
        if second as f64 > now {
            self.lost.insert(second);
        } else {
            self.floor = self.floor.max(second);
        }
    }

    fn remember(&mut self, nonce: &str, ts: i64, now: f64, cap: usize) -> Result<(), &'static str> {
        #[allow(clippy::cast_precision_loss)]
        while let Some((&first, _)) = self.seen.iter().next() {
            if (first as f64) < now - SKEW_S {
                self.drop_second(first, now);
            } else {
                break;
            }
        }
        #[allow(clippy::cast_precision_loss)]
        self.lost.retain(|s| (*s as f64) >= now - SKEW_S);
        if ts <= self.floor || self.lost.contains(&ts) {
            return Err("replayed or older than the replay cache");
        }
        let bucket = self.seen.entry(ts).or_default();
        if bucket.contains(nonce) {
            return Err("replayed");
        }
        bucket.insert(nonce.to_string());
        self.count += 1;
        while self.count > cap && self.seen.len() > 1 {
            let first = self.seen.keys().next().copied();
            let last = self.seen.keys().next_back().copied();
            #[allow(clippy::cast_precision_loss)]
            let far = [first, last]
                .into_iter()
                .flatten()
                .filter(|s| *s != ts)
                .max_by(|a, b| {
                    let da = (*a as f64 - now).abs();
                    let db = (*b as f64 - now).abs();
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                });
            match far {
                Some(second) => self.drop_second(second, now),
                None => break,
            }
        }
        Ok(())
    }
}

pub struct Verifier {
    key: String,
    started_at: f64,
    cap: usize,
    cache: Mutex<Cache>,
}

impl Verifier {
    pub fn new(key: String, started_at: f64) -> Self {
        Self::with_cap(key, started_at, NONCE_CAP)
    }

    pub fn with_cap(key: String, started_at: f64, cap: usize) -> Self {
        Verifier {
            key,
            started_at,
            cap,
            cache: Mutex::new(Cache {
                floor: -1,
                ..Cache::default()
            }),
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// Refuse every assertion signed at or before `second` from now on.
    pub fn raise_floor(&self, second: i64) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.floor = cache.floor.max(second);
        }
    }

    /// Python `verify`: Ok when the request carries a valid, unseen assertion.
    pub fn verify(&self, req: &Signed<'_>, now: f64) -> Result<(), &'static str> {
        if req.user_id.is_empty() {
            return Err("missing tenant");
        }
        let (ts, nonce) = shape(req.assertion).ok_or("missing or malformed assertion")?;
        #[allow(clippy::cast_possible_truncation)]
        let started = self.started_at.trunc() as i64;
        if ts < started - RESTART_SKEW_S {
            return Err("signed before this process started");
        }
        tenant_assertion::verify(&tenant_assertion::Request {
            key: &self.key,
            user_id: req.user_id,
            assertion: req.assertion,
            method: req.method,
            path: req.path,
            query: req.query,
            body_hash: req.body_hash,
            storage: req.storage,
            legacy_owner: req.legacy_owner,
            blocked: req.blocked,
            now,
        })
        .map_err(|_| "rust refused")?;
        let mut cache = self.cache.lock().map_err(|_| "cache unavailable")?;
        cache.remember(nonce, ts, now, self.cap)
    }

    pub fn secret_ref_matches(&self, user_id: &str, secret: &str, secret_ref: &str) -> bool {
        tenant_assertion::secret_ref_matches(&self.key, user_id, secret, secret_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "k-synthetic";
    const UID: &str = "11111111-2222-4333-8444-555555555555";

    fn signed(ts: u64, nonce: &str) -> String {
        let empty = tenant_assertion_body("");
        let req = tenant_assertion::Request {
            key: KEY,
            user_id: UID,
            assertion: "",
            method: "GET",
            path: "/api/months",
            query: b"",
            body_hash: &empty,
            storage: "",
            legacy_owner: "",
            blocked: "",
            now: 0.0,
        };
        tenant_assertion::sign(&req, ts, nonce).unwrap_or_default()
    }

    fn tenant_assertion_body(body: &str) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(body.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn check(v: &Verifier, assertion: &str, now: f64) -> Result<(), &'static str> {
        let empty = tenant_assertion_body("");
        v.verify(
            &Signed {
                user_id: UID,
                assertion,
                method: "GET",
                path: "/api/months",
                query: b"",
                body_hash: &empty,
                storage: "",
                legacy_owner: "",
                blocked: "",
            },
            now,
        )
    }

    #[test]
    fn accepts_once_then_refuses_replay() {
        let v = Verifier::new(KEY.into(), 1_000.0);
        let a = signed(1_000, "0123456789abcdef0123456789abcdef");
        assert_eq!(check(&v, &a, 1_000.5), Ok(()));
        assert_eq!(check(&v, &a, 1_000.6), Err("replayed"));
    }

    #[test]
    fn restart_guard_and_floor() {
        let v = Verifier::new(KEY.into(), 1_000.0);
        let old = signed(998, "0123456789abcdef0123456789abcdef");
        assert_eq!(
            check(&v, &old, 1_000.0),
            Err("signed before this process started")
        );
        let a = signed(1_005, "0123456789abcdef0123456789abcdea");
        v.raise_floor(1_005);
        assert_eq!(
            check(&v, &a, 1_005.2),
            Err("replayed or older than the replay cache")
        );
        let b = signed(1_006, "0123456789abcdef0123456789abcdeb");
        assert_eq!(check(&v, &b, 1_006.2), Ok(()));
    }

    #[test]
    fn bad_signature_and_window_refused() {
        let v = Verifier::new(KEY.into(), 1_000.0);
        let a = signed(1_000, "0123456789abcdef0123456789abcdef");
        assert_eq!(check(&v, &a, 1_100.0), Err("rust refused"));
        let mut bad = a.clone();
        bad.pop();
        bad.push(if a.ends_with('A') { 'B' } else { 'A' });
        assert_eq!(check(&v, &bad, 1_000.0), Err("rust refused"));
        assert_eq!(
            check(&v, "v2.1000.zz.x", 1_000.0),
            Err("missing or malformed assertion")
        );
    }

    #[test]
    fn full_cache_drops_farthest_second_and_raises_floor() {
        let v = Verifier::with_cap(KEY.into(), 1_000.0, 2);
        for (ts, n) in [(1_001u64, "a"), (1_002, "b"), (1_003, "c")] {
            let nonce = format!("{:0>32}", n);
            assert_eq!(check(&v, &signed(ts, &nonce), 1_003.0), Ok(()));
        }
        // 1001 was dropped (farthest from now) and raised the floor.
        let again = format!("{:0>32}", "d");
        assert_eq!(
            check(&v, &signed(1_001, &again), 1_003.0),
            Err("replayed or older than the replay cache")
        );
    }

    #[test]
    fn clock_window_boundaries_are_inclusive_at_sixty_seconds() {
        let v = Verifier::new(KEY.into(), 0.0);
        let a = signed(10_000, "0123456789abcdef0123456789abcde1");
        assert_eq!(check(&v, &a, 10_060.0), Ok(()));
        let b = signed(10_000, "0123456789abcdef0123456789abcde2");
        assert_eq!(check(&v, &b, 10_060.001), Err("rust refused"));
        let c = signed(10_100, "0123456789abcdef0123456789abcde3");
        assert_eq!(check(&v, &c, 10_040.0), Ok(()));
        let d = signed(10_100, "0123456789abcdef0123456789abcde4");
        assert_eq!(check(&v, &d, 10_039.999), Err("rust refused"));
    }

    #[test]
    fn concurrent_verifies_of_one_nonce_accept_exactly_once() {
        let v = std::sync::Arc::new(Verifier::new(KEY.into(), 0.0));
        let a = signed(20_000, "0123456789abcdef0123456789abcde5");
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let (v, a) = (v.clone(), a.clone());
                std::thread::spawn(move || check(&v, &a, 20_000.5).is_ok())
            })
            .collect();
        let ok = handles
            .into_iter()
            .map(|h| h.join().unwrap_or(false))
            .filter(|ok| *ok)
            .count();
        assert_eq!(ok, 1);
    }

    #[test]
    fn body_media_rule_matches_python() {
        assert!(body_is_hashed(""));
        assert!(body_is_hashed(" Application/JSON ; charset=utf-8"));
        assert!(!body_is_hashed("application/zip"));
        assert!(!body_is_hashed("multipart/form-data; boundary=x"));
    }
}
