//! The turn-bound call binding (track R8 decision 19's design): at the cut the turn's call is
//! pinned to one user, one provider row, one address, one model and one chat key, for a limited
//! time. A relay (or the front's own call path) takes only the binding, never a URL or a provider
//! id, and re-checks it on every request against the row as it is now.
//!
//! Decisions (named behaviour differences from Node, which keeps the captured provider object for
//! the whole turn):
//! - The row fingerprint covers the row id, its `baseUrl` and whether it is shared, not the key: a
//!   rotated key is picked up on the next request (the headers are rebuilt from the unsealed key
//!   each time), but a row moved to another address, or from shared to private, ends the turn.
//! - A row that is gone, a user no longer active, or a member origin no longer approved also ends
//!   the turn (fail closed); Node would finish it on the old values.
//! - The model in the body is forced to the pinned one ([`pin_model`]).
//!
//! The token is `v1.<hex of the binding's JSON>.<hex of HMAC-SHA256 over "v1." + that hex>`; the
//! MAC key is a per-process secret the minting side holds. Verification is constant time.

use hmac::{Hmac, Mac};
use js_json::JValue;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const PREFIX: &str = "v1.";
/// A token longer than this is refused unread (a binding is a few hundred bytes).
pub const TOKEN_CAP: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub user: String,
    pub provider_id: String,
    pub url: String,
    pub fingerprint: String,
    pub model: String,
    pub chat_key: Option<String>,
    /// Milliseconds since the epoch after which the binding is refused.
    pub expires_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    Malformed,
    BadMac,
    Expired,
}

/// The row's fingerprint: SHA-256 (hex) over the id, `baseUrl` and shared flag, each
/// length-prefixed so no two rows collide by concatenation. Non-text ids and addresses are
/// fingerprinted as their JSON (a hand-edited row cannot alias a text one).
pub fn fingerprint(row: &JValue) -> String {
    let mut h = Sha256::new();
    for part in [
        field(row.get("id")),
        field(row.get("baseUrl")),
        if row.get("shared").truthy() { "1".to_string() } else { "0".to_string() },
    ] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part.as_bytes());
    }
    hex(&h.finalize())
}

fn field(v: &JValue) -> String {
    match v {
        JValue::Str(s) => format!("s{s}"),
        other => format!("j{}", js_json::stringify(other).unwrap_or_default()),
    }
}

fn hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(D[usize::from(b >> 4)]));
        s.push(char::from(D[usize::from(b & 15)]));
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    s.as_bytes()
        .chunks(2)
        .map(|p| match p {
            [a, b] => Some(nib(*a)? << 4 | nib(*b)?),
            _ => None,
        })
        .collect()
}

fn mac(secret: &[u8], signed: &str) -> Option<HmacSha256> {
    let mut m = HmacSha256::new_from_slice(secret).ok()?;
    m.update(signed.as_bytes());
    Some(m)
}

fn to_json(b: &Binding) -> JValue {
    JValue::obj([
        ("user", JValue::Str(b.user.clone())),
        ("provider", JValue::Str(b.provider_id.clone())),
        ("url", JValue::Str(b.url.clone())),
        ("fingerprint", JValue::Str(b.fingerprint.clone())),
        ("model", JValue::Str(b.model.clone())),
        (
            "chatKey",
            b.chat_key.clone().map(JValue::Str).unwrap_or(JValue::Null),
        ),
        ("expires", JValue::Num(b.expires_ms)),
    ])
}

fn from_json(v: &JValue) -> Option<Binding> {
    let text = |k: &str| v.get(k).as_str().map(str::to_string);
    let chat_key = match v.get("chatKey") {
        JValue::Null => None,
        JValue::Str(s) => Some(s.clone()),
        _ => return None,
    };
    let expires_ms = v.get("expires").as_num().filter(|n| n.is_finite())?;
    Some(Binding {
        user: text("user")?,
        provider_id: text("provider")?,
        url: text("url")?,
        fingerprint: text("fingerprint")?,
        model: text("model")?,
        chat_key,
        expires_ms,
    })
}

/// A token for `binding` under `secret`; `None` for an empty secret or a binding that does not
/// serialize (a non-finite expiry).
pub fn mint(secret: &[u8], binding: &Binding) -> Option<String> {
    if secret.is_empty() || !binding.expires_ms.is_finite() {
        return None;
    }
    let body = hex(js_json::stringify(&to_json(binding))?.as_bytes());
    let signed = format!("{PREFIX}{body}");
    let tag = hex(&mac(secret, &signed)?.finalize().into_bytes());
    Some(format!("{signed}.{tag}"))
}

/// The binding a token carries, when its MAC is good under `secret` and it has not expired at
/// `now_ms` (an expiry equal to now is expired).
pub fn open(secret: &[u8], token: &str, now_ms: f64) -> Result<Binding, TokenError> {
    if secret.is_empty() || token.len() > TOKEN_CAP {
        return Err(TokenError::Malformed);
    }
    let rest = token.strip_prefix(PREFIX).ok_or(TokenError::Malformed)?;
    let (body, tag) = rest.split_once('.').ok_or(TokenError::Malformed)?;
    let tag = unhex(tag).ok_or(TokenError::Malformed)?;
    let signed = format!("{PREFIX}{body}");
    mac(secret, &signed)
        .ok_or(TokenError::Malformed)?
        .verify_slice(&tag)
        .map_err(|_| TokenError::BadMac)?;
    let bytes = unhex(body).ok_or(TokenError::Malformed)?;
    let text = String::from_utf8(bytes).map_err(|_| TokenError::Malformed)?;
    let v = js_json::parse(&text).map_err(|_| TokenError::Malformed)?;
    let b = from_json(&v).ok_or(TokenError::Malformed)?;
    if !(now_ms < b.expires_ms) {
        return Err(TokenError::Expired);
    }
    Ok(b)
}

/// The per-request re-check's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recheck {
    Ok,
    /// The user is not the binding's, or no longer active.
    UserChanged,
    /// The row is not in the user's view any more.
    RowGone,
    /// The row's address or sharing changed (or the address no longer builds the bound URL).
    RowChanged,
    /// A member's own row whose origin is no longer approved.
    OriginRefused,
}

/// What the re-check reads, all as of this request.
#[derive(Debug, Clone)]
pub struct Now<'a> {
    pub user: &'a str,
    pub user_active: bool,
    /// The row with the bound id, looked up as this user (exactly, not the default fallback).
    pub row: Option<&'a JValue>,
    /// `memberOwnProvider` for that row and this user, and `endpointApproved` of the bound URL.
    pub member_own: bool,
    pub origin_approved: bool,
}

/// Re-check a binding before one request: same active user, the row still there with the same
/// fingerprint and address, and the member origin still approved where it applies. Checked in
/// that order; the first failure is the answer.
pub fn recheck(b: &Binding, now: &Now<'_>) -> Recheck {
    if now.user != b.user || !now.user_active {
        return Recheck::UserChanged;
    }
    let Some(row) = now.row else {
        return Recheck::RowGone;
    };
    if row.get("id").as_str() != Some(b.provider_id.as_str()) {
        return Recheck::RowGone;
    }
    if fingerprint(row) != b.fingerprint
        || crate::target::upstream_url(row.get("baseUrl")).as_deref() != Some(b.url.as_str())
    {
        return Recheck::RowChanged;
    }
    if now.member_own && !now.origin_approved {
        return Recheck::OriginRefused;
    }
    Recheck::Ok
}

/// Force the request body's `model` to the bound one (a body that is not an object is left as it
/// is: the caller refuses it).
pub fn pin_model(body: &mut JValue, model: &str) {
    if let JValue::Obj(items) = body {
        match items.iter_mut().find(|(k, _)| k == "model") {
            Some((_, v)) => *v = JValue::Str(model.to_string()),
            None => items.push(("model".to_string(), JValue::Str(model.to_string()))),
        }
    }
}
