//! Byte authentication for the controlled-proxy protocol.
//!
//! These pure helpers do not admit HTTP/JSON, enforce freshness/replay or prove a
//! live session. Runtime state must separately fence epoch/session/lease changes.
//! Random material is supplied by the caller; no entropy or I/O is invented here.
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::GrantKey;

pub const CAPABILITY: &str = "noevia-code-egress-control/1";
pub const MAX_REQUEST_BYTES: usize = 32 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Malformed,
    Capacity,
    Authentication,
    Internal,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "egress control {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Ready,
    Renew,
    Activity,
    Revoke,
    Forget,
}
impl Route {
    /// Exact origin-form paths only. Methods/framing/body semantics are runtime checks.
    pub fn parse(path: &str) -> Result<Self, Error> {
        match path {
            "/v1/control/ready" => Ok(Self::Ready),
            "/v1/control/renew" => Ok(Self::Renew),
            "/v1/control/activity" => Ok(Self::Activity),
            "/v1/control/revoke" => Ok(Self::Revoke),
            "/v1/control/forget" => Ok(Self::Forget),
            _ => Err(Error::Malformed),
        }
    }
    pub fn path(self) -> &'static str {
        match self {
            Self::Ready => "/v1/control/ready",
            Self::Renew => "/v1/control/renew",
            Self::Activity => "/v1/control/activity",
            Self::Revoke => "/v1/control/revoke",
            Self::Forget => "/v1/control/forget",
        }
    }
}

/// Parse a header deadline without accepting alternate numeric spellings.
/// A valid number still needs runtime wall-clock freshness checks.
pub fn parse_deadline_ms(text: &str) -> Result<u64, Error> {
    if text.is_empty()
        || text.len() > 16
        || text.starts_with('0')
        || !text.bytes().all(|c| c.is_ascii_digit())
    {
        return Err(Error::Malformed);
    }
    let value = text.parse::<u64>().map_err(|_| Error::Malformed)?;
    if value > MAX_SAFE_INTEGER {
        return Err(Error::Malformed);
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy)]
pub struct RequestMetadata<'a> {
    pub request_id: &'a str,
    pub deadline_ms: u64,
    /// Empty only for a semantically validated initial readiness bootstrap.
    pub boot_id: Option<&'a str>,
    pub key_fingerprint: &'a str,
}

#[derive(Clone)]
pub struct FileKey([u8; 32]);
impl std::fmt::Debug for FileKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileKey(<redacted>)")
    }
}
impl FileKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    /// Exactly lowercase64hex, optionally one LF. Filesystem safety is separate.
    pub fn from_hex(text: &str) -> Result<Self, Error> {
        parse_hex(text.strip_suffix('\n').unwrap_or(text)).map(Self)
    }
    pub fn fingerprint(&self) -> String {
        digest(&self.0)
    }
}

#[derive(Clone)]
pub struct ControlKey([u8; 32]);
impl std::fmt::Debug for ControlKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControlKey(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub struct EpochKeys {
    pub control: ControlKey,
    pub grant: GrantKey,
}

pub fn derive_web_file_key(master: &FileKey, nonce: &[u8; 32]) -> Result<FileKey, Error> {
    let mut input = b"noevia-code-egress-control/1:web-key\0".to_vec();
    input.extend_from_slice(nonce);
    keyed(&master.0, &input).map(FileKey)
}
pub fn derive_control_key(file: &FileKey) -> Result<ControlKey, Error> {
    keyed(&file.0, b"noevia-code-egress-control/1:control-key\0").map(ControlKey)
}
pub fn derive_epoch_keys(
    file: &FileKey,
    boot: &[u8; 16],
    session: &[u8; 16],
) -> Result<EpochKeys, Error> {
    let mut input = b"noevia-code-egress-control/1:grant-key\0".to_vec();
    input.extend_from_slice(boot);
    input.extend_from_slice(session);
    Ok(EpochKeys {
        control: derive_control_key(file)?,
        grant: GrantKey::from_bytes(keyed(&file.0, &input)?),
    })
}

impl ControlKey {
    pub fn sign_request(
        &self,
        route: Route,
        meta: RequestMetadata<'_>,
        body: &[u8],
    ) -> Result<String, Error> {
        keyed(&self.0, &request_transcript(route, meta, body)?).map(|v| hex(&v))
    }
    pub fn verify_request(
        &self,
        route: Route,
        meta: RequestMetadata<'_>,
        body: &[u8],
        tag: &str,
    ) -> Result<(), Error> {
        let tag = parse_hex(tag)?;
        verify_tag(&self.0, &request_transcript(route, meta, body)?, &tag)
    }
    pub fn sign_response(
        &self,
        request_tag: &str,
        status: u16,
        body: &[u8],
    ) -> Result<String, Error> {
        keyed(&self.0, &response_transcript(request_tag, status, body)?).map(|v| hex(&v))
    }
    pub fn verify_response(
        &self,
        request_tag: &str,
        status: u16,
        body: &[u8],
        tag: &str,
    ) -> Result<(), Error> {
        let tag = parse_hex(tag)?;
        verify_tag(
            &self.0,
            &response_transcript(request_tag, status, body)?,
            &tag,
        )
    }
}

fn request_transcript(
    route: Route,
    meta: RequestMetadata<'_>,
    body: &[u8],
) -> Result<Vec<u8>, Error> {
    if body.len() > MAX_REQUEST_BYTES {
        return Err(Error::Capacity);
    }
    let _: [u8; 16] = parse_hex(meta.request_id)?;
    let _: [u8; 32] = parse_hex(meta.key_fingerprint)?;
    match meta.boot_id {
        Some(boot) => {
            let _: [u8; 16] = parse_hex(boot)?;
        }
        None if route == Route::Ready => {}
        None => return Err(Error::Malformed),
    }
    if meta.deadline_ms == 0 || meta.deadline_ms > MAX_SAFE_INTEGER {
        return Err(Error::Malformed);
    }
    Ok(format!(
        "{CAPABILITY}:request\nPOST\n{}\n{}\n{}\n{}\n{}\n{}\n",
        route.path(),
        meta.request_id,
        meta.deadline_ms,
        meta.boot_id.unwrap_or(""),
        meta.key_fingerprint,
        digest(body)
    )
    .into_bytes())
}
fn response_transcript(tag: &str, status: u16, body: &[u8]) -> Result<Vec<u8>, Error> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(Error::Capacity);
    }
    let _: [u8; 32] = parse_hex(tag)?;
    if !(100..=599).contains(&status) {
        return Err(Error::Malformed);
    }
    Ok(format!("{CAPABILITY}:response\n{tag}\n{status}\n{}\n", digest(body)).into_bytes())
}
fn keyed(key: &[u8; 32], input: &[u8]) -> Result<[u8; 32], Error> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).map_err(|_| Error::Internal)?;
    mac.update(input);
    Ok(mac.finalize().into_bytes().into())
}
fn verify_tag(key: &[u8; 32], input: &[u8], tag: &[u8; 32]) -> Result<(), Error> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).map_err(|_| Error::Internal)?;
    mac.update(input);
    mac.verify_slice(tag).map_err(|_| Error::Authentication)
}
fn digest(input: &[u8]) -> String {
    hex(&Sha256::digest(input))
}
fn hex(input: &[u8]) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(input.len() * 2);
    for byte in input {
        // String formatting cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}
fn parse_hex<const N: usize>(text: &str) -> Result<[u8; N], Error> {
    if text.len() != N * 2 {
        return Err(Error::Malformed);
    }
    let nibble = |v: u8| match v {
        b'0'..=b'9' => Ok(v - b'0'),
        b'a'..=b'f' => Ok(v - b'a' + 10),
        _ => Err(Error::Malformed),
    };
    let mut bytes = [0; N];
    let (pairs, remainder) = text.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(Error::Malformed);
    }
    for (slot, [high, low]) in bytes.iter_mut().zip(pairs) {
        *slot = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Ok(bytes)
}
