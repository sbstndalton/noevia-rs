//! Closed, bounded operation-body admission. This is not an authenticated request:
//! HTTP framing, MAC verification, freshness and current epoch/session/lease state
//! must be checked separately before a controller can execute anything.
use egress::control::{RequestMetadata, Route, MAX_REQUEST_BYTES, MAX_SAFE_INTEGER};
use serde::{de, Deserialize, Deserializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Capacity,
    Malformed,
    Version,
    Identity,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "control body {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReadyMode {
    Initial,
    Replace,
}

#[derive(Clone)]
enum Previous {
    None,
    Session(String),
}
impl Previous {
    fn value(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Session(v) => Some(v),
        }
    }
}
// A required nullable field, unlike Option's implicit missing-field default.
impl<'de> Deserialize<'de> for Previous {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = Previous;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("null or a canonical session identity")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Previous, E> {
                Ok(Previous::None)
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Previous, E> {
                if !lower_hex(v, 32) {
                    return Err(E::custom("invalid session identity"));
                }
                Ok(Previous::Session(v.into()))
            }
        }
        d.deserialize_any(Visitor)
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Ready {
    version: u8,
    mode: ReadyMode,
    challenge: String,
    expected_key_fingerprint: String,
    prior_session: Previous,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Renew {
    version: u8,
    session_id: String,
    sequence: u64,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Activity {
    version: u8,
    session_id: String,
    task_id: String,
    lease_nonce: String,
    signed_grant: String,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Revoke {
    version: u8,
    session_id: String,
    task_id: String,
    lease_nonce: String,
    grant_issued_at: u64,
    signed_grant: String,
    signed_revoke: String,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Forget {
    version: u8,
    session_id: String,
    task_id: String,
    lease_nonce: String,
    snapshot_id: String,
}
#[derive(Clone)]
enum Body {
    Ready(Ready),
    Renew(Renew),
    Activity(Activity),
    Revoke(Revoke),
    Forget(Forget),
}

/// Immutable body produced only by strict admission. A valid body is not proof
/// of authentication, grant validity, renewal ordering or a live session.
#[derive(Clone)]
pub struct Request {
    body: Body,
}
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlBody")
            .field("route", &self.route())
            .finish_non_exhaustive()
    }
}
pub struct ReadyView<'a> {
    pub mode: ReadyMode,
    pub challenge: &'a str,
    pub expected_key_fingerprint: &'a str,
    pub prior_session: Option<&'a str>,
}
pub struct Lease<'a> {
    pub session_id: &'a str,
    pub task_id: &'a str,
    pub nonce: &'a str,
}

impl Request {
    pub fn decode(route: Route, meta: RequestMetadata<'_>, bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(Error::Capacity);
        }
        if !lower_hex(meta.request_id, 32)
            || !lower_hex(meta.key_fingerprint, 64)
            || !positive_safe(meta.deadline_ms)
            || meta.boot_id.is_some_and(|v| !lower_hex(v, 32))
            || (route != Route::Ready && meta.boot_id.is_none())
        {
            return Err(Error::Malformed);
        }
        let body = match route {
            Route::Ready => {
                let r: Ready = parse(bytes)?;
                version(r.version)?;
                if !lower_hex(&r.challenge, 32) || !lower_hex(&r.expected_key_fingerprint, 64) {
                    return Err(Error::Malformed);
                }
                if r.expected_key_fingerprint != meta.key_fingerprint
                    || (r.mode == ReadyMode::Initial
                        && (meta.boot_id.is_some() || r.prior_session.value().is_some()))
                    || (r.mode == ReadyMode::Replace && meta.boot_id.is_none())
                {
                    return Err(Error::Identity);
                }
                Body::Ready(r)
            }
            Route::Renew => {
                let r: Renew = parse(bytes)?;
                version(r.version)?;
                if !lower_hex(&r.session_id, 32) || !positive_safe(r.sequence) {
                    return Err(Error::Malformed);
                }
                Body::Renew(r)
            }
            Route::Activity => {
                let r: Activity = parse(bytes)?;
                version(r.version)?;
                validate_lease(&r.session_id, &r.task_id, &r.lease_nonce)?;
                token_syntax(&r.signed_grant)?;
                Body::Activity(r)
            }
            Route::Revoke => {
                let r: Revoke = parse(bytes)?;
                version(r.version)?;
                validate_lease(&r.session_id, &r.task_id, &r.lease_nonce)?;
                if !positive_safe(r.grant_issued_at) {
                    return Err(Error::Malformed);
                }
                token_syntax(&r.signed_grant)?;
                token_syntax(&r.signed_revoke)?;
                Body::Revoke(r)
            }
            Route::Forget => {
                let r: Forget = parse(bytes)?;
                version(r.version)?;
                validate_lease(&r.session_id, &r.task_id, &r.lease_nonce)?;
                if !lower_hex(&r.snapshot_id, 32) {
                    return Err(Error::Malformed);
                }
                Body::Forget(r)
            }
        };
        Ok(Self { body })
    }
    pub fn route(&self) -> Route {
        match self.body {
            Body::Ready(_) => Route::Ready,
            Body::Renew(_) => Route::Renew,
            Body::Activity(_) => Route::Activity,
            Body::Revoke(_) => Route::Revoke,
            Body::Forget(_) => Route::Forget,
        }
    }
    pub fn ready(&self) -> Option<ReadyView<'_>> {
        match &self.body {
            Body::Ready(r) => Some(ReadyView {
                mode: r.mode,
                challenge: &r.challenge,
                expected_key_fingerprint: &r.expected_key_fingerprint,
                prior_session: r.prior_session.value(),
            }),
            _ => None,
        }
    }
    pub fn renewal(&self) -> Option<(&str, u64)> {
        match &self.body {
            Body::Renew(r) => Some((&r.session_id, r.sequence)),
            _ => None,
        }
    }
    pub fn lease(&self) -> Option<Lease<'_>> {
        let (session, task, nonce) = match &self.body {
            Body::Activity(r) => (&r.session_id, &r.task_id, &r.lease_nonce),
            Body::Revoke(r) => (&r.session_id, &r.task_id, &r.lease_nonce),
            Body::Forget(r) => (&r.session_id, &r.task_id, &r.lease_nonce),
            _ => return None,
        };
        Some(Lease {
            session_id: session,
            task_id: task,
            nonce,
        })
    }
    /// Opaque syntax only; controller must verify its MAC and all lease claims.
    pub fn signed_grant(&self) -> Option<&str> {
        match &self.body {
            Body::Activity(r) => Some(&r.signed_grant),
            Body::Revoke(r) => Some(&r.signed_grant),
            _ => None,
        }
    }
    pub fn revocation(&self) -> Option<(u64, &str)> {
        match &self.body {
            Body::Revoke(r) => Some((r.grant_issued_at, &r.signed_revoke)),
            _ => None,
        }
    }
    pub fn snapshot_id(&self) -> Option<&str> {
        match &self.body {
            Body::Forget(r) => Some(&r.snapshot_id),
            _ => None,
        }
    }
}
fn parse<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, Error> {
    serde_json::from_slice(bytes).map_err(|_| Error::Malformed)
}
fn version(v: u8) -> Result<(), Error> {
    if v == 1 {
        Ok(())
    } else {
        Err(Error::Version)
    }
}
fn positive_safe(v: u64) -> bool {
    v != 0 && v <= MAX_SAFE_INTEGER
}
fn lower_hex(v: &str, length: usize) -> bool {
    v.len() == length
        && v.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn validate_lease(session: &str, task: &str, nonce: &str) -> Result<(), Error> {
    if !lower_hex(session, 32)
        || task.is_empty()
        || task.len() > 128
        || !task
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || b"._:-".contains(&v))
        || nonce.len() != 22
        || !egress::b64url_decode(nonce.as_bytes())
            .is_some_and(|v| v.len() == 16 && egress::b64url_encode(&v) == nonce)
    {
        return Err(Error::Malformed);
    }
    Ok(())
}
fn token_syntax(token: &str) -> Result<(), Error> {
    if token.len() > 8192 {
        return Err(Error::Capacity);
    }
    let mut parts = token.split('.');
    let url_chars = |v: &str| {
        !v.is_empty()
            && v.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
    };
    if parts.next() != Some("ngr1")
        || !parts.next().is_some_and(url_chars)
        || !parts.next().is_some_and(|v| {
            v.len() == 43
                && url_chars(v)
                && egress::b64url_decode(v.as_bytes())
                    .is_some_and(|b| b.len() == 32 && egress::b64url_encode(&b) == v)
        })
        || parts.next().is_some()
    {
        return Err(Error::Malformed);
    }
    Ok(())
}
