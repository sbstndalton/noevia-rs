//! Signed per-task grants: the web↔proxy contract (noevia `docs/egress-grant-contract.md`,
//! schema `apps/web/contracts/egress-grant.v1.schema.json`).
//!
//! ```text
//! token   = "ngr1." base64url(payload) "." base64url(HMAC-SHA256(key, "ngr1." base64url(payload)))
//! payload = {"v":1,"act":"grant"|"revoke","task":…,"hosts":[…],"iat":…,"exp":…,"idle":…,"nonce":…}
//! ```
//! The payload is canonical JSON: exactly those keys in that order, no whitespace, ASCII only.
//! [`verify`] checks the MAC (constant time) **before** it parses anything, then requires the
//! payload to re-encode to the very same bytes, so there is exactly one valid encoding of a
//! grant. [`Ledger`] adds the stateful rules: a newer grant supersedes an older one for the same
//! task, a revoke kills every grant issued at or before it, and a grant that was dropped
//! (superseded, revoked, idle) can never be presented again until its `exp`.

use std::collections::HashMap;

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

/// The token prefix; it names the contract version.
pub const GRANT_PREFIX: &str = "ngr1.";
/// HKDF label the web derives the key with (`secretStore.derive(GRANT_KEY_LABEL)`).
pub const GRANT_KEY_LABEL: &str = "code-egress-grant";
/// How far a grant's `iat` may lie in the proxy's future (clock skew between containers).
pub const MAX_CLOCK_SKEW_MS: u64 = 60_000;
/// Longest `exp - iat` the proxy accepts: one day.
pub const MAX_LIFETIME_MS: u64 = 24 * 60 * 60 * 1000;
pub const MAX_HOSTS: usize = 64;
pub const MAX_TASK_LEN: usize = 128;
pub const MAX_HOST_LEN: usize = 253;
/// Upper bound on the encoded token; anything longer is refused unread.
pub const MAX_TOKEN_LEN: usize = 8192;
/// Tasks the ledger remembers at once; past it new tasks are refused (fail closed).
pub const MAX_LEDGER_TASKS: usize = 65_536;

/// The 32-byte HMAC key. Never printed: `Debug` is redacted.
#[derive(Clone)]
pub struct GrantKey([u8; 32]);

impl std::fmt::Debug for GrantKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrantKey(<redacted>)")
    }
}

impl GrantKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The key file format: exactly 64 hex digits, optionally followed by one newline.
    pub fn from_hex(text: &str) -> Result<Self, String> {
        let t = text.strip_suffix('\n').unwrap_or(text);
        let t = t.strip_suffix('\r').unwrap_or(t);
        if t.len() != 64 {
            return Err("grant key must be 64 hex digits".into());
        }
        let mut out = [0u8; 32];
        for (i, slot) in out.iter_mut().enumerate() {
            let pair = t
                .get(i * 2..i * 2 + 2)
                .ok_or("grant key must be 64 hex digits")?;
            *slot = u8::from_str_radix(pair, 16).map_err(|_| "grant key must be 64 hex digits")?;
        }
        Ok(Self(out))
    }

    /// HMAC accepts keys of any length, so this is always Some; no panic path either way.
    fn mac(&self, signed: &[u8]) -> Option<Hmac<Sha256>> {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.0).ok()?;
        m.update(signed);
        Some(m)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Grant,
    Revoke,
}

impl Act {
    fn as_str(self) -> &'static str {
        match self {
            Act::Grant => "grant",
            Act::Revoke => "revoke",
        }
    }
}

/// A decoded, MAC-checked grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedGrant {
    pub act: Act,
    pub task: String,
    pub hosts: Vec<String>,
    /// Issued at, Unix milliseconds.
    pub iat: u64,
    /// Absolute expiry, Unix milliseconds.
    pub exp: u64,
    /// Idle TTL, milliseconds.
    pub idle: u64,
    /// 16 random bytes, base64url without padding (22 characters).
    pub nonce: String,
}

/// Why a grant was refused. The text is the refusal reason; it never contains the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantError {
    Malformed,
    WrongVersion,
    BadSignature,
    NotYetValid,
    Expired,
    Superseded,
    Revoked,
    Replayed,
    NotAGrant,
    LedgerFull,
}

impl GrantError {
    pub fn reason(self) -> &'static str {
        match self {
            GrantError::Malformed => "grant is malformed",
            GrantError::WrongVersion => "grant version is not supported",
            GrantError::BadSignature => "grant signature is invalid",
            GrantError::NotYetValid => "grant is not valid yet",
            GrantError::Expired => "grant has expired",
            GrantError::Superseded => "grant was superseded by a newer grant for this task",
            GrantError::Revoked => "grant was revoked",
            GrantError::Replayed => "grant was already retired",
            GrantError::NotAGrant => "token is not a grant",
            GrantError::LedgerFull => "too many live grants",
        }
    }
}

/// Whether a token claims to be a signed grant (any version), so the caller routes it here.
pub fn looks_signed(token: &[u8]) -> bool {
    token.starts_with(b"ngr")
}

fn valid_task(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= MAX_TASK_LEN
        && t.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'-'))
}

/// A granted domain as the web normalises it: lowercase LDH labels and dots, no leading or
/// trailing dot, no empty label.
fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= MAX_HOST_LEN
        && !h.starts_with('.')
        && !h.ends_with('.')
        && !h.contains("..")
        && h.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-')
}

fn valid_nonce(n: &str) -> bool {
    n.len() == 22
        && n.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// The one canonical payload encoding. Every string field is restricted to characters that
/// need no JSON escaping, so plain concatenation is exact.
pub fn canonical_payload(g: &SignedGrant) -> String {
    let hosts = g
        .hosts
        .iter()
        .map(|h| format!("\"{h}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"v\":1,\"act\":\"{}\",\"task\":\"{}\",\"hosts\":[{}],\"iat\":{},\"exp\":{},\"idle\":{},\"nonce\":\"{}\"}}",
        g.act.as_str(),
        g.task,
        hosts,
        g.iat,
        g.exp,
        g.idle,
        g.nonce
    )
}

/// Field rules that hold for every grant regardless of the clock.
fn well_formed(g: &SignedGrant) -> bool {
    let hosts_ok = g.hosts.len() <= MAX_HOSTS
        && g.hosts.iter().all(|h| valid_host(h))
        && g.hosts
            .iter()
            .enumerate()
            .all(|(i, h)| !g.hosts.iter().take(i).any(|o| o == h));
    let act_ok = match g.act {
        Act::Grant => !g.hosts.is_empty(),
        Act::Revoke => g.hosts.is_empty(),
    };
    valid_task(&g.task)
        && hosts_ok
        && act_ok
        && valid_nonce(&g.nonce)
        && g.iat > 0
        && g.exp > g.iat
        && g.exp - g.iat <= MAX_LIFETIME_MS
        && g.idle > 0
        && g.idle <= g.exp - g.iat
}

/// Signs a grant (the web does this in Node; Rust mints only in tests and test vectors).
/// Returns None when the grant breaks a field rule.
pub fn mint(key: &GrantKey, g: &SignedGrant) -> Option<String> {
    if !well_formed(g) {
        return None;
    }
    let signed = format!(
        "{GRANT_PREFIX}{}",
        b64url_encode(canonical_payload(g).as_bytes())
    );
    let tag = key.mac(signed.as_bytes())?.finalize().into_bytes();
    Some(format!("{signed}.{}", b64url_encode(&tag)))
}

/// Verifies a token's MAC, encoding, fields and validity window at `now_ms` (Unix ms).
/// Stateless: replay and supersession are [`Ledger`]'s job.
pub fn verify(key: &GrantKey, token: &[u8], now_ms: u64) -> Result<SignedGrant, GrantError> {
    if token.len() > MAX_TOKEN_LEN {
        return Err(GrantError::Malformed);
    }
    if !token.starts_with(GRANT_PREFIX.as_bytes()) {
        return Err(if looks_signed(token) {
            GrantError::WrongVersion
        } else {
            GrantError::NotAGrant
        });
    }
    let dot = token
        .iter()
        .rposition(|&c| c == b'.')
        .ok_or(GrantError::Malformed)?;
    let (signed, tag) = token.split_at(dot);
    let tag = tag.get(1..).ok_or(GrantError::Malformed)?;
    if signed.len() <= GRANT_PREFIX.len() {
        return Err(GrantError::Malformed);
    }
    let tag = b64url_decode(tag).ok_or(GrantError::Malformed)?;
    // Constant-time comparison (hmac's verify_slice uses subtle::ConstantTimeEq).
    key.mac(signed)
        .ok_or(GrantError::BadSignature)?
        .verify_slice(&tag)
        .map_err(|_| GrantError::BadSignature)?;
    let body = signed
        .get(GRANT_PREFIX.len()..)
        .ok_or(GrantError::Malformed)?;
    let payload = b64url_decode(body).ok_or(GrantError::Malformed)?;
    let g = parse_payload(&payload)?;
    if canonical_payload(&g).as_bytes() != payload.as_slice() || !well_formed(&g) {
        return Err(GrantError::Malformed);
    }
    if g.iat > now_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
        return Err(GrantError::NotYetValid);
    }
    if now_ms >= g.exp {
        return Err(GrantError::Expired);
    }
    Ok(g)
}

fn parse_payload(bytes: &[u8]) -> Result<SignedGrant, GrantError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|_| GrantError::Malformed)?;
    let o = v.as_object().ok_or(GrantError::Malformed)?;
    match o.get("v").and_then(Value::as_u64) {
        Some(1) => {}
        Some(_) => return Err(GrantError::WrongVersion),
        None => return Err(GrantError::Malformed),
    }
    let s = |k: &str| {
        o.get(k)
            .and_then(Value::as_str)
            .ok_or(GrantError::Malformed)
    };
    let n = |k: &str| {
        o.get(k)
            .and_then(Value::as_u64)
            .ok_or(GrantError::Malformed)
    };
    let act = match s("act")? {
        "grant" => Act::Grant,
        "revoke" => Act::Revoke,
        _ => return Err(GrantError::Malformed),
    };
    let hosts = o
        .get("hosts")
        .and_then(Value::as_array)
        .ok_or(GrantError::Malformed)?
        .iter()
        .map(|h| h.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or(GrantError::Malformed)?;
    Ok(SignedGrant {
        act,
        task: s("task")?.to_owned(),
        hosts,
        iat: n("iat")?,
        exp: n("exp")?,
        idle: n("idle")?,
        nonce: s("nonce")?.to_owned(),
    })
}

/// What [`Ledger::admit`] decided for a valid grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// First sight of the newest grant for its task: install it (dropping older ones).
    New,
    /// The task's current grant, already installed.
    Current,
}

#[derive(Debug, Default)]
struct TaskState {
    /// The current grant's (iat, nonce).
    current: Option<(u64, String)>,
    /// Every grant with iat <= this was revoked.
    revoked_through: Option<u64>,
    /// Keep the entry until then (the largest exp seen for the task).
    keep_until: u64,
}

/// The proxy's memory of grants, bounded by their `exp`.
#[derive(Debug, Default)]
pub struct Ledger {
    tasks: HashMap<String, TaskState>,
    /// Nonces of grants that were dropped while still inside their `exp` -> that `exp`.
    retired: HashMap<String, u64>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets everything whose `exp` has passed: such a grant fails [`verify`] anyway.
    pub fn prune(&mut self, now_ms: u64) {
        self.tasks.retain(|_, t| t.keep_until > now_ms);
        self.retired.retain(|_, exp| *exp > now_ms);
    }

    /// Applies the replay and supersession rules to a verified grant.
    /// A revoke is always admitted (it can only take access away) and returns `Current`.
    pub fn admit(&mut self, g: &SignedGrant, now_ms: u64) -> Result<Admission, GrantError> {
        self.prune(now_ms);
        if self.retired.contains_key(&g.nonce) {
            return Err(GrantError::Replayed);
        }
        if !self.tasks.contains_key(&g.task) && self.tasks.len() >= MAX_LEDGER_TASKS {
            return Err(GrantError::LedgerFull);
        }
        let t = self.tasks.entry(g.task.clone()).or_default();
        t.keep_until = t.keep_until.max(g.exp);
        if g.act == Act::Revoke {
            t.revoked_through = Some(t.revoked_through.unwrap_or(0).max(g.iat));
            if let Some((iat, nonce)) = t.current.take() {
                if iat <= g.iat {
                    self.retired.insert(nonce, g.exp.max(now_ms));
                } else {
                    t.current = Some((iat, nonce));
                }
            }
            return Ok(Admission::Current);
        }
        if t.revoked_through.is_some_and(|r| g.iat <= r) {
            return Err(GrantError::Revoked);
        }
        match &t.current {
            Some((iat, nonce)) if *iat == g.iat && *nonce == g.nonce => Ok(Admission::Current),
            Some((iat, nonce)) if (*iat, nonce.as_str()) > (g.iat, g.nonce.as_str()) => {
                Err(GrantError::Superseded)
            }
            _ => {
                if let Some((_, old)) = t.current.replace((g.iat, g.nonce.clone())) {
                    // The old grant's exp is unknown here; keep it for the longest lifetime.
                    self.retired
                        .insert(old, now_ms.saturating_add(MAX_LIFETIME_MS));
                }
                Ok(Admission::New)
            }
        }
    }

    /// Records that the current grant of `task` with `nonce` was dropped (idle TTL, absolute
    /// expiry, revocation by the operator), so the same token cannot bring it back.
    pub fn retire(&mut self, task: &str, nonce: &str, exp: u64) {
        if let Some(t) = self.tasks.get_mut(task) {
            if t.current.as_ref().is_some_and(|(_, n)| n == nonce) {
                t.current = None;
            }
        }
        self.retired.insert(nonce.to_owned(), exp);
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url without padding (RFC 4648 §5).
pub fn b64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk.first().copied().unwrap_or(0);
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        for i in 0..=chunk.len() {
            let idx = ((n >> (18 - 6 * i)) & 63) as usize;
            out.push(char::from(B64URL.get(idx).copied().unwrap_or(b'A')));
        }
    }
    out
}

/// Strict base64url without padding: only the URL-safe alphabet, no `=`, and the unused low
/// bits of the last character must be zero (so each byte string has one encoding).
pub fn b64url_decode(input: &[u8]) -> Option<Vec<u8>> {
    if input.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in input {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = ((acc << 6) | u32::from(v)) & 0xffff;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    if bits > 0 && acc & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn key() -> GrantKey {
        GrantKey::from_bytes([7u8; 32])
    }

    fn grant(task: &str, iat: u64, nonce: &str) -> SignedGrant {
        SignedGrant {
            act: Act::Grant,
            task: task.into(),
            hosts: vec!["registry.example.test".into(), "example.org".into()],
            iat,
            exp: iat + 3_600_000,
            idle: 600_000,
            nonce: nonce.into(),
        }
    }

    const N1: &str = "AAAAAAAAAAAAAAAAAAAAAA";
    const N2: &str = "BBBBBBBBBBBBBBBBBBBBBA";

    #[test]
    fn round_trip() {
        let g = grant("task-1", NOW, N1);
        let t = mint(&key(), &g).unwrap();
        assert!(t.starts_with("ngr1."));
        assert_eq!(verify(&key(), t.as_bytes(), NOW + 1).unwrap(), g);
    }

    #[test]
    fn forged_with_another_key() {
        let t = mint(&GrantKey::from_bytes([8u8; 32]), &grant("t", NOW, N1)).unwrap();
        assert_eq!(
            verify(&key(), t.as_bytes(), NOW),
            Err(GrantError::BadSignature)
        );
    }

    #[test]
    fn tampered_fields_fail_the_mac() {
        let g = grant("task-1", NOW, N1);
        let t = mint(&key(), &g).unwrap();
        let (signed, tag) = t.rsplit_once('.').unwrap();
        let payload = b64url_decode(&signed.as_bytes()[5..]).unwrap();
        let text = String::from_utf8(payload).unwrap();
        for (from, to) in [
            ("example.org", "example.com"),
            ("task-1", "task-2"),
            ("\"idle\":600000", "\"idle\":600001"),
            ("\"act\":\"grant\"", "\"act\":\"revoke\""),
        ] {
            let forged = format!(
                "ngr1.{}.{tag}",
                b64url_encode(text.replace(from, to).as_bytes())
            );
            assert_eq!(
                verify(&key(), forged.as_bytes(), NOW),
                Err(GrantError::BadSignature),
                "{from}"
            );
        }
        // A flipped tag byte.
        let mut bad = t.clone().into_bytes();
        let last = t.rfind('.').unwrap() + 1;
        bad[last] = if bad[last] == b'A' { b'B' } else { b'A' };
        assert_eq!(verify(&key(), &bad, NOW), Err(GrantError::BadSignature));
        // A truncated tag.
        assert_eq!(
            verify(&key(), &t.as_bytes()[..t.len() - 3], NOW),
            Err(GrantError::BadSignature)
        );
    }

    #[test]
    fn non_canonical_payload_is_malformed_even_when_signed() {
        let k = key();
        let sign = |payload: &str| {
            let signed = format!("ngr1.{}", b64url_encode(payload.as_bytes()));
            let tag = k.mac(signed.as_bytes()).unwrap().finalize().into_bytes();
            format!("{signed}.{}", b64url_encode(&tag))
        };
        let canon = canonical_payload(&grant("t", NOW, N1));
        let spaced = canon.replacen(",", ", ", 1);
        let reordered = canon.replacen(
            "{\"v\":1,\"act\":\"grant\",",
            "{\"act\":\"grant\",\"v\":1,",
            1,
        );
        let upper = canon.replace("example.org", "Example.org");
        let extra = canon.replacen("{\"v\":1,", "{\"v\":1,\"x\":1,", 1);
        let dup = canon.replace("\"example.org\"", "\"example.org\",\"example.org\"");
        for p in [spaced, reordered, upper, extra, dup] {
            assert_eq!(
                verify(&k, sign(&p).as_bytes(), NOW),
                Err(GrantError::Malformed),
                "{p}"
            );
        }
        assert_eq!(verify(&k, sign(&canon).as_bytes(), NOW).map(|_| ()), Ok(()));
        let v2 = canon.replacen("\"v\":1", "\"v\":2", 1);
        assert_eq!(
            verify(&k, sign(&v2).as_bytes(), NOW),
            Err(GrantError::WrongVersion)
        );
    }

    #[test]
    fn wrong_version_prefix() {
        let t = mint(&key(), &grant("t", NOW, N1)).unwrap();
        let v2 = t.replacen("ngr1.", "ngr2.", 1);
        assert_eq!(
            verify(&key(), v2.as_bytes(), NOW),
            Err(GrantError::WrongVersion)
        );
        assert_eq!(
            verify(&key(), b"tok-plain-0123456789", NOW),
            Err(GrantError::NotAGrant)
        );
    }

    #[test]
    fn validity_window() {
        let g = grant("t", NOW, N1);
        let t = mint(&key(), &g).unwrap();
        assert_eq!(
            verify(&key(), t.as_bytes(), g.exp),
            Err(GrantError::Expired)
        );
        assert!(verify(&key(), t.as_bytes(), g.exp - 1).is_ok());
        assert_eq!(
            verify(&key(), t.as_bytes(), NOW - MAX_CLOCK_SKEW_MS - 1),
            Err(GrantError::NotYetValid)
        );
        assert!(verify(&key(), t.as_bytes(), NOW - MAX_CLOCK_SKEW_MS).is_ok());
    }

    #[test]
    fn field_rules() {
        let k = key();
        let base = grant("t", NOW, N1);
        let mut bad = Vec::new();
        let mut g = base.clone();
        g.hosts.clear();
        bad.push(g);
        let mut g = base.clone();
        g.task = "has space".into();
        bad.push(g);
        let mut g = base.clone();
        g.exp = g.iat + MAX_LIFETIME_MS + 1;
        bad.push(g);
        let mut g = base.clone();
        g.idle = 0;
        bad.push(g);
        let mut g = base.clone();
        g.nonce = "short".into();
        bad.push(g);
        let mut g = base.clone();
        g.hosts = vec![".example.org".into()];
        bad.push(g);
        let mut g = base.clone();
        g.act = Act::Revoke;
        bad.push(g);
        for g in bad {
            assert!(mint(&k, &g).is_none(), "{g:?}");
        }
    }

    #[test]
    fn ledger_supersede_revoke_replay() {
        let mut l = Ledger::new();
        let old = grant("task-1", NOW, N1);
        let new = grant("task-1", NOW + 10, N2);
        assert_eq!(l.admit(&old, NOW), Ok(Admission::New));
        assert_eq!(l.admit(&old, NOW + 1), Ok(Admission::Current));
        assert_eq!(l.admit(&new, NOW + 20), Ok(Admission::New));
        // The older grant cannot come back.
        assert_eq!(l.admit(&old, NOW + 30), Err(GrantError::Replayed));
        // Revoke at a later iat kills the current grant and anything issued before it.
        let mut rv = grant("task-1", NOW + 40, "CCCCCCCCCCCCCCCCCCCCCA");
        rv.act = Act::Revoke;
        rv.hosts.clear();
        assert_eq!(l.admit(&rv, NOW + 40), Ok(Admission::Current));
        assert_eq!(l.admit(&new, NOW + 50), Err(GrantError::Replayed));
        let late = grant("task-1", NOW + 35, "DDDDDDDDDDDDDDDDDDDDDA");
        assert_eq!(l.admit(&late, NOW + 50), Err(GrantError::Revoked));
        // A grant minted after the revoke is a new grant.
        let after = grant("task-1", NOW + 60, "EEEEEEEEEEEEEEEEEEEEEA");
        assert_eq!(l.admit(&after, NOW + 60), Ok(Admission::New));
        // Idle-dropped grants are retired.
        l.retire("task-1", &after.nonce, after.exp);
        assert_eq!(l.admit(&after, NOW + 70), Err(GrantError::Replayed));
        // Everything is forgotten once expired.
        l.prune(NOW + MAX_LIFETIME_MS + 100);
        assert!(l.is_empty());
    }

    #[test]
    fn ledger_older_iat_is_superseded() {
        let mut l = Ledger::new();
        assert_eq!(l.admit(&grant("t", NOW + 10, N2), NOW), Ok(Admission::New));
        assert_eq!(
            l.admit(&grant("t", NOW, N1), NOW),
            Err(GrantError::Superseded)
        );
    }

    #[test]
    fn key_hex() {
        let hex = "07".repeat(32);
        assert!(GrantKey::from_hex(&hex).is_ok());
        assert!(GrantKey::from_hex(&format!("{hex}\n")).is_ok());
        assert!(GrantKey::from_hex(&hex[..62]).is_err());
        assert!(GrantKey::from_hex(&"zz".repeat(32)).is_err());
        assert_eq!(format!("{:?}", key()), "GrantKey(<redacted>)");
    }

    #[test]
    fn b64url_strict() {
        for n in 0..40u8 {
            let bytes: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37)).collect();
            let e = b64url_encode(&bytes);
            assert_eq!(b64url_decode(e.as_bytes()).unwrap(), bytes);
        }
        assert!(b64url_decode(b"AB==").is_none());
        assert!(b64url_decode(b"AB+/").is_none());
        assert!(b64url_decode(b"AB").is_none()); // non-zero trailing bits
        assert!(b64url_decode(b"AA").is_some());
    }
}
