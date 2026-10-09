//! Diary's per-request tenant assertion check, matching noevia-services'
//! `diary/agent/tenant_assertion.py` (`verify` without its nonce cache, and `storage_secret_ref`).
//!
//! The header is `X-Cowork-Tenant-Assertion: v2.<unix seconds>.<nonce hex>.<base64url HMAC-SHA256>`
//! over the newline-joined canonical string
//! `"cowork-diary-tenant-v2", user id (lower case), ts, nonce, METHOD, path,
//! "query=" + sha256(raw query), "body=" + body hash, sha256(storage), legacy owner, blocked`.
//!
//! This crate is only ever AND-composed with the Python check (TENANT_ASSERTION_IMPL=rust): the
//! caller accepts a request only when both accept, so this side may be stricter than Python but
//! must never be more lenient. Where it is deliberately stricter it answers
//! [`Reject::Unsupported`]:
//!
//! * the user id and the method must be ASCII (Python lower()/upper() them with Unicode rules;
//!   web only ever sends a UUID and an ASCII method);
//! * the key must be non-empty (Python never verifies without a key);
//! * the body hash must be 64 lowercase hex characters or `stream` (what the Python caller
//!   always computes);
//! * `now` must be finite;
//! * every field is bounded by [`MAX_FIELD_BYTES`].
//!
//! The nonce (replay) cache is state and stays in Python; it runs after both checks accept.
//!
//! Signature comparison is constant time ([`subtle::ConstantTimeEq`]), as Python's
//! `hmac.compare_digest`; like it, a length difference returns early. No function here panics,
//! and nothing here formats a key, a secret or a signature into an error.

#![forbid(unsafe_code)]

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Domain label of the canonical string (Python `LABEL`).
pub const LABEL: &str = "cowork-diary-tenant-v2";
/// Accepted clock skew in seconds (Python `SKEW_S`).
pub const SKEW_S: f64 = 60.0;
/// The body hash of a request whose body is not signed (Python `STREAM`).
pub const STREAM: &str = "stream";
/// Upper bound on any single input field. Real headers are far smaller (uvicorn caps them).
pub const MAX_FIELD_BYTES: usize = 256 * 1024;

type HmacSha256 = Hmac<Sha256>;

/// Why a request was refused. [`Reject::reason`] matches Python's reason strings where Python has
/// one; `Unsupported` marks the cases where this side is stricter than Python.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    MissingTenant,
    Malformed,
    Unsupported,
    BadSignature,
    OutsideWindow,
}

impl Reject {
    pub fn reason(self) -> &'static str {
        match self {
            Reject::MissingTenant => "missing tenant",
            Reject::Malformed => "missing or malformed assertion",
            Reject::Unsupported => "unsupported input",
            Reject::BadSignature => "bad signature",
            Reject::OutsideWindow => "outside clock window",
        }
    }
}

/// One request as the Python middleware sees it: header values already extracted (missing
/// headers are ""), the encoded wire path, the raw query bytes and the body hash it computed.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub key: &'a str,
    pub user_id: &'a str,
    pub assertion: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a [u8],
    pub body_hash: &'a str,
    pub storage: &'a str,
    pub legacy_owner: &'a str,
    pub blocked: &'a str,
    pub now: f64,
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        for nibble in [b >> 4, b & 15] {
            out.push(char::from(
                DIGITS.get(usize::from(nibble)).copied().unwrap_or(b'0'),
            ));
        }
    }
    out
}

/// Unpadded base64url, as Python's `urlsafe_b64encode(...).rstrip(b"=")`.
fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk.first().copied().unwrap_or(0);
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            let idx = (n >> (18 - 6 * i)) & 63;
            out.push(char::from(
                ALPHABET.get(idx as usize).copied().unwrap_or(b'A'),
            ));
        }
    }
    out
}

fn hmac_sha256(key: &str, message: &str) -> Option<[u8; 32]> {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).ok()?;
    mac.update(message.as_bytes());
    Some(mac.finalize().into_bytes().into())
}

/// The canonical string (Python `canonical`), for ASCII user ids and methods.
#[allow(clippy::too_many_arguments)]
pub fn canonical(
    user_id: &str,
    ts: u64,
    nonce: &str,
    method: &str,
    path: &str,
    query_hash: &str,
    body_hash: &str,
    storage: &str,
    legacy_owner: &str,
    blocked: &str,
) -> String {
    [
        LABEL.to_string(),
        user_id.to_ascii_lowercase(),
        ts.to_string(),
        nonce.to_string(),
        method.to_ascii_uppercase(),
        path.to_string(),
        format!("query={query_hash}"),
        format!("body={body_hash}"),
        sha256_hex(storage.as_bytes()),
        legacy_owner.to_string(),
        blocked.to_string(),
    ]
    .join("\n")
}

/// The assertion header for `req`'s fields at (`ts`, `nonce`) (Python `sign`). For tests and
/// tooling; `req.assertion` and `req.now` are ignored. None only if HMAC refuses the key, which
/// HMAC-SHA256 never does.
pub fn sign(req: &Request<'_>, ts: u64, nonce: &str) -> Option<String> {
    let message = canonical(
        req.user_id,
        ts,
        nonce,
        req.method,
        req.path,
        &sha256_hex(req.query),
        req.body_hash,
        req.storage,
        req.legacy_owner,
        req.blocked,
    );
    let tag = hmac_sha256(req.key, &message)?;
    Some(format!("v2.{ts}.{nonce}.{}", b64url(&tag)))
}

/// `v2\.(\d{1,12})\.([0-9a-f]{32})\.([A-Za-z0-9_-]{43})` as a full match, ASCII digits only.
fn parse(raw: &str) -> Option<(u64, &str)> {
    let rest = raw.strip_prefix("v2.")?;
    let mut parts = rest.split('.');
    let ts = parts.next()?;
    let nonce = parts.next()?;
    let sig = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
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
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some((ts.parse().ok()?, nonce))
}

fn body_hash_ok(h: &str) -> bool {
    h == STREAM || (h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

fn fields_bounded(req: &Request<'_>) -> bool {
    [
        req.key.len(),
        req.user_id.len(),
        req.assertion.len(),
        req.method.len(),
        req.path.len(),
        req.query.len(),
        req.body_hash.len(),
        req.storage.len(),
        req.legacy_owner.len(),
        req.blocked.len(),
    ]
    .iter()
    .all(|&n| n <= MAX_FIELD_BYTES)
}

/// Python `verify` without the nonce cache: Ok when the assertion is valid for `req.user_id`.
/// Checks run in Python's order (tenant, shape, signature, clock) with the stricter input checks
/// between shape and signature, so a reason differs from Python's only when Python would accept
/// or this side is stricter.
pub fn verify(req: &Request<'_>) -> Result<(), Reject> {
    if req.user_id.is_empty() {
        return Err(Reject::MissingTenant);
    }
    let (ts, nonce) = parse(req.assertion).ok_or(Reject::Malformed)?;
    if !fields_bounded(req)
        || req.key.is_empty()
        || !req.user_id.is_ascii()
        || !req.method.is_ascii()
        || !body_hash_ok(req.body_hash)
        || !req.now.is_finite()
    {
        return Err(Reject::Unsupported);
    }
    let expected = sign(req, ts, nonce).ok_or(Reject::Unsupported)?;
    if !bool::from(expected.as_bytes().ct_eq(req.assertion.as_bytes())) {
        return Err(Reject::BadSignature);
    }
    // ts has at most 12 digits, so it converts to f64 exactly, as Python's int - float does.
    #[allow(clippy::cast_precision_loss)]
    let skew = (req.now - ts as f64).abs();
    if skew > SKEW_S || skew.is_nan() {
        return Err(Reject::OutsideWindow);
    }
    Ok(())
}

/// Python `storage_secret_ref`: the first 32 hex characters of
/// HMAC-SHA256(key, "<LABEL>:storage-secret\n<user id lower>\n<secret>"). None for inputs this
/// side refuses (empty key, non-ASCII user id, oversized field).
pub fn storage_secret_ref(key: &str, user_id: &str, secret: &str) -> Option<String> {
    if key.is_empty()
        || !user_id.is_ascii()
        || key.len() > MAX_FIELD_BYTES
        || user_id.len() > MAX_FIELD_BYTES
        || secret.len() > MAX_FIELD_BYTES
    {
        return None;
    }
    let msg = format!(
        "{LABEL}:storage-secret\n{}\n{secret}",
        user_id.to_ascii_lowercase()
    );
    let tag = hmac_sha256(key, &msg)?;
    let mut h = hex(&tag);
    h.truncate(32);
    Some(h)
}

/// app.py's secretRef check: does `secret_ref` match the derived ref (constant time)? False for
/// anything this side refuses.
pub fn secret_ref_matches(key: &str, user_id: &str, secret: &str, secret_ref: &str) -> bool {
    match storage_secret_ref(key, user_id, secret) {
        Some(expected) => bool::from(expected.as_bytes().ct_eq(secret_ref.as_bytes())),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_matches_known_vectors() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(b64url(&[0u8; 32]).len(), 43);
    }

    #[test]
    fn parse_rejects_non_ascii_digits_and_extra_parts() {
        let ok = format!("v2.1.{}.{}", "a".repeat(32), "A".repeat(43));
        assert!(parse(&ok).is_some());
        assert!(parse(&format!("{ok}.x")).is_none());
        assert!(parse(&ok.replace("v2.1.", "v2.\u{661}.")).is_none());
        assert!(parse(&ok.replace("v2.1.", "v2.1234567890123.")).is_none());
        assert!(parse(&ok.replace("v2.1.", "v2..")).is_none());
    }
}
