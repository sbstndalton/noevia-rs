//! Per-task tokens with an idle TTL (`code-egress.cjs` `grant`/`authorize`/`sweep`).
//!
//! Time is a caller-supplied millisecond clock (`now_ms`), like the JS `now()`, so expiry is
//! testable without sleeping.

/// The JS default idle TTL: six hours.
pub const DEFAULT_TOKEN_TTL_MS: u64 = 6 * 60 * 60 * 1000;

/// What a task was granted. Fixed at creation; never widened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub token: String,
    pub task_id: String,
    pub domains: Vec<String>,
    /// Idle TTL in milliseconds (> 0).
    pub idle_ttl_ms: u64,
}

/// Opaque handle for a live grant; stays unique for the store's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GrantId(u64);

#[derive(Debug)]
struct Entry {
    id: GrantId,
    grant: Grant,
    last_used_ms: u64,
}

/// A grant dropped by [`TokenStore::sweep`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expired {
    pub id: GrantId,
    pub task_id: String,
}

/// Snapshot of the grant a token unlocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorized {
    pub id: GrantId,
    pub task_id: String,
    pub domains: Vec<String>,
}

#[derive(Debug, Default)]
pub struct TokenStore {
    entries: Vec<Entry>,
    next_id: u64,
}

impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a grant, dropping any earlier grant for the same task (as JS `grant()` does).
    /// Returns the new grant's id and the ids it replaced. A zero TTL falls back to the default.
    pub fn insert(&mut self, mut grant: Grant, now_ms: u64) -> (GrantId, Vec<GrantId>) {
        if grant.idle_ttl_ms == 0 {
            grant.idle_ttl_ms = DEFAULT_TOKEN_TTL_MS;
        }
        let mut replaced = Vec::new();
        self.entries.retain(|e| {
            let keep = e.grant.task_id != grant.task_id;
            if !keep {
                replaced.push(e.id);
            }
            keep
        });
        let id = GrantId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        self.entries.push(Entry {
            id,
            grant,
            last_used_ms: now_ms,
        });
        (id, replaced)
    }

    /// Removes every grant of `task_id`; returns their ids.
    pub fn revoke(&mut self, task_id: &str) -> Vec<GrantId> {
        let mut removed = Vec::new();
        self.entries.retain(|e| {
            let keep = e.grant.task_id != task_id;
            if !keep {
                removed.push(e.id);
            }
            keep
        });
        removed
    }

    /// Drops every grant idle for longer than its TTL (strictly greater, as in JS).
    pub fn sweep(&mut self, now_ms: u64) -> Vec<Expired> {
        let mut expired = Vec::new();
        self.entries.retain(|e| {
            let idle = now_ms.saturating_sub(e.last_used_ms);
            let keep = idle <= e.grant.idle_ttl_ms;
            if !keep {
                expired.push(Expired {
                    id: e.id,
                    task_id: e.grant.task_id.clone(),
                });
            }
            keep
        });
        expired
    }

    /// Whether the grant is still live (not revoked or expired).
    pub fn contains(&self, id: GrantId) -> bool {
        self.entries.iter().any(|e| e.id == id)
    }

    /// Records activity on a live grant.
    pub fn touch(&mut self, id: GrantId, now_ms: u64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.last_used_ms = e.last_used_ms.max(now_ms);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `Proxy-Authorization: Basic base64(anything:token)`. Sweeps first (returning what it
    /// dropped through `expired`), then compares the token against every live grant in constant
    /// time per candidate of equal length; a match refreshes the grant's idle clock.
    pub fn authorize(
        &mut self,
        header: Option<&[u8]>,
        now_ms: u64,
        expired: &mut Vec<Expired>,
    ) -> Option<Authorized> {
        let token = header.and_then(basic_token);
        expired.extend(self.sweep(now_ms));
        let wanted = token?;
        let mut found: Option<usize> = None;
        for (i, e) in self.entries.iter().enumerate() {
            if ct_eq(e.grant.token.as_bytes(), &wanted) && found.is_none() {
                found = Some(i);
            }
        }
        let e = self.entries.get_mut(found?)?;
        e.last_used_ms = now_ms;
        Some(Authorized {
            id: e.id,
            task_id: e.grant.task_id.clone(),
            domains: e.grant.domains.clone(),
        })
    }
}

/// Equal-length constant-time compare (a length mismatch returns early, as in the JS).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `/^Basic\s+(\S+)$/i`, base64-decoded; the token is what follows the first `:` (the whole
/// value when there is none). Returns the token bytes, or None for anything else.
fn basic_token(header: &[u8]) -> Option<Vec<u8>> {
    let scheme = header.get(..5)?;
    if !scheme.eq_ignore_ascii_case(b"basic") {
        return None;
    }
    let rest = header.get(5..)?;
    let start = rest.iter().position(|c| !c.is_ascii_whitespace())?;
    if start == 0 {
        return None; // \s+ needs at least one
    }
    let value = rest.get(start..)?;
    if value.is_empty() || value.iter().any(u8::is_ascii_whitespace) {
        return None;
    }
    let decoded = base64_decode(value)?;
    // JS decodes to a UTF-8 string first; lossy decoding mirrors Buffer#toString('utf8').
    let text = String::from_utf8_lossy(&decoded);
    let token = match text.find(':') {
        Some(i) => text.get(i + 1..)?,
        None => &text,
    };
    Some(token.as_bytes().to_vec())
}

/// Standard or URL-safe base64, padding optional. Stricter than Node's lenient decoder
/// (which skips junk): any other byte rejects the header, which can only refuse more.
fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let body = match input.iter().position(|&c| c == b'=') {
        Some(i) => {
            let (body, pad) = input.split_at(i);
            if pad.len() > 2 || pad.iter().any(|&c| c != b'=') {
                return None;
            }
            body
        }
        None => input,
    };
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in body {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    if body.len() % 4 == 1 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TokenStore {
        let mut s = TokenStore::new();
        s.insert(
            Grant {
                token: "tok-aaaaaaaaaaaaaaaa".into(),
                task_id: "task-1".into(),
                domains: vec!["example.com".into()],
                idle_ttl_ms: 1000,
            },
            0,
        );
        s
    }

    fn header(token: &str) -> Vec<u8> {
        let raw = format!("task:{token}");
        let mut out = String::from("Basic ");
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        for chunk in raw.as_bytes().chunks(3) {
            let b = [
                chunk.first().copied().unwrap_or(0),
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    let idx = ((n >> (18 - 6 * i)) & 63) as usize;
                    out.push(char::from(T.get(idx).copied().unwrap_or(b'A')));
                } else {
                    out.push('=');
                }
            }
        }
        out.into_bytes()
    }

    #[test]
    fn authorize_and_expire() {
        let mut s = store();
        let mut ex = Vec::new();
        let h = header("tok-aaaaaaaaaaaaaaaa");
        assert!(s.authorize(Some(&h), 500, &mut ex).is_some());
        // Idle 1000 ms exactly is still live; 1001 is not.
        assert!(s.authorize(Some(&h), 1500, &mut ex).is_some());
        assert!(ex.is_empty());
        assert!(s.authorize(Some(&h), 2501, &mut ex).is_none());
        assert_eq!(ex.len(), 1);
        assert!(s.is_empty());
    }

    #[test]
    fn rejects_bad_headers() {
        let mut s = store();
        let mut ex = Vec::new();
        for h in [
            &b"Bearer tok-aaaaaaaaaaaaaaaa"[..],
            b"Basic",
            b"Basic ",
            b"BasicdGFzazp0b2s=",
            b"Basic !!!!",
        ] {
            assert!(s.authorize(Some(h), 0, &mut ex).is_none());
        }
        assert!(s.authorize(None, 0, &mut ex).is_none());
        assert!(s
            .authorize(Some(&header("tok-aaaaaaaaaaaaaaab")), 0, &mut ex)
            .is_none());
    }

    #[test]
    fn regrant_replaces_and_revoke_removes() {
        let mut s = store();
        let (_, replaced) = s.insert(
            Grant {
                token: "tok-bbbbbbbbbbbbbbbb".into(),
                task_id: "task-1".into(),
                domains: vec![],
                idle_ttl_ms: 0,
            },
            0,
        );
        assert_eq!(replaced.len(), 1);
        assert_eq!(s.len(), 1);
        assert_eq!(s.revoke("task-1").len(), 1);
        assert!(s.is_empty());
    }
}
