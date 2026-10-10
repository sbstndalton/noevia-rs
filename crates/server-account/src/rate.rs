//! core auth.cjs `createRateLimiter`: fixed windows per key, swept every minute, at most
//! `max_entries` keys (expired ones are reclaimed first, then the oldest-inserted go). The map
//! keeps JS `Map` insertion order: a key keeps its place when its window is renewed.

use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Copy)]
struct Entry {
    count: u64,
    reset: i64,
    seq: u64,
}

/// What [`RateLimiter::charge`] counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Charge {
    pub limited: bool,
    /// The window the call was counted in (its reset time).
    pub window: i64,
}

#[derive(Debug)]
pub struct RateLimiter {
    map: HashMap<String, Entry>,
    order: BTreeMap<u64, String>,
    next_seq: u64,
    last_sweep: i64,
    sweep_ms: i64,
    max_entries: usize,
}

impl RateLimiter {
    /// `createRateLimiter()` with its defaults (sweep every 60 s, 10,000 keys), started at `now`.
    pub fn new(now: i64) -> Self {
        Self::with(now, 60 * 1000, 10_000)
    }

    pub fn with(now: i64, sweep_ms: i64, max_entries: usize) -> Self {
        RateLimiter {
            map: HashMap::new(),
            order: BTreeMap::new(),
            next_seq: 0,
            last_sweep: now,
            sweep_ms,
            max_entries,
        }
    }

    fn remove(&mut self, key: &str) {
        if let Some(e) = self.map.remove(key) {
            self.order.remove(&e.seq);
        }
    }

    fn sweep(&mut self, now: i64) {
        let expired: Vec<String> = self
            .order
            .values()
            .filter(|k| self.map.get(*k).is_some_and(|e| e.reset <= now))
            .cloned()
            .collect();
        for k in expired {
            self.remove(&k);
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// `charge(key, limit, windowMs)` at `now`.
    pub fn charge(&mut self, key: &str, limit: u64, window_ms: i64, now: i64) -> Charge {
        if now - self.last_sweep >= self.sweep_ms {
            self.last_sweep = now;
            self.sweep(now);
        }
        if !self.map.contains_key(key) && self.map.len() >= self.max_entries {
            self.sweep(now);
        }
        if !self.map.contains_key(key) && self.map.len() >= self.max_entries {
            while self.map.len() >= self.max_entries {
                let Some((_, oldest)) = self.order.pop_first() else {
                    break;
                };
                self.map.remove(&oldest);
            }
        }
        match self.map.get_mut(key) {
            Some(e) if e.reset > now => {
                e.count += 1;
                Charge {
                    limited: e.count > limit,
                    window: e.reset,
                }
            }
            Some(e) => {
                // `map.set(key, entry)` on an existing key keeps its insertion place.
                e.count = 1;
                e.reset = now + window_ms;
                Charge {
                    limited: false,
                    window: e.reset,
                }
            }
            None => {
                let seq = self.next_seq;
                self.next_seq += 1;
                let reset = now + window_ms;
                self.map.insert(
                    key.to_string(),
                    Entry {
                        count: 1,
                        reset,
                        seq,
                    },
                );
                self.order.insert(seq, key.to_string());
                Charge {
                    limited: false,
                    window: reset,
                }
            }
        }
    }

    /// `rateLimited(key, limit, windowMs)`.
    pub fn limited(&mut self, key: &str, limit: u64, window_ms: i64, now: i64) -> bool {
        self.charge(key, limit, window_ms, now).limited
    }

    /// `blocked(key, limit)`: over the limit in the current window, without counting.
    pub fn blocked(&self, key: &str, limit: u64, now: i64) -> bool {
        self.map
            .get(key)
            .is_some_and(|e| e.reset > now && e.count > limit)
    }

    /// `clear(key)`.
    pub fn clear(&mut self, key: &str) {
        self.remove(key);
    }

    /// `release(key, window)`: give back one call counted in `window`, if that window is current.
    pub fn release(&mut self, key: &str, window: i64, now: i64) {
        if let Some(e) = self.map.get_mut(key) {
            if e.reset == window && e.reset > now && e.count > 0 {
                e.count -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_windows_like_node() {
        let mut r = RateLimiter::new(0);
        for i in 1..=5 {
            assert!(!r.limited("k", 5, 1000, 10), "{i}");
        }
        assert!(r.limited("k", 5, 1000, 10));
        assert!(r.blocked("k", 5, 10));
        assert!(!r.blocked("k", 6, 10));
        // A new window at reset.
        assert!(!r.limited("k", 5, 1000, 1010));
        let c = r.charge("k", 5, 1000, 1011);
        assert_eq!(c.window, 2010);
        r.release("k", c.window, 1011);
        r.release("k", 999, 1011);
        r.clear("k");
        assert!(!r.blocked("k", 0, 1011));
    }

    #[test]
    fn bounded_like_node() {
        let mut r = RateLimiter::with(0, 60_000, 3);
        r.charge("a", 5, 100, 1);
        r.charge("b", 5, 10_000, 1);
        r.charge("c", 5, 10_000, 1);
        // Full: "a" expired by now, reclaimed first.
        r.charge("d", 5, 10_000, 200);
        assert_eq!(r.len(), 3);
        assert!(!r.map.contains_key("a"));
        // Still full with live keys: the oldest-inserted goes.
        r.charge("e", 5, 10_000, 201);
        assert!(!r.map.contains_key("b"));
        assert!(r.map.contains_key("e"));
        // Renewing a key keeps its place: "c" is still the oldest.
        r.charge("c", 5, 10, 20_000);
        r.charge("f", 5, 10, 20_000);
        // Full again: the expired "d" and "e" are swept, "c" (renewed) and "f" stay.
        assert_eq!(r.len(), 2);
        assert!(r.map.contains_key("c") && r.map.contains_key("f"));
    }
}
