//! Credential envelopes (noevia#979): the Rust port of noevia-core's `server/secret-envelope.cjs`
//! (`versionOf`, `decryptWith`, `open`, `encrypt`, moved there unchanged from `secrets.cjs`).
//!
//! Formats (the text after the prefix is base64url of `iv(12) | tag(16) | body`):
//! - `enc:v1:` AES-256-GCM with no associated data (legacy, still opens);
//! - `enc:v2:` AES-256-GCM with AAD `noevia:user:<userId>`, so a value copied to another account
//!   fails to open.
//!
//! Key derivation and storage stay in Node: every call takes the key bytes (and, for [`seal`], the
//! 12-byte nonce from Node's `crypto.randomBytes`), so this crate needs no RNG and the WebAssembly
//! module keeps zero imports. AES-GCM, including the constant-time tag check, is RustCrypto's
//! `aes-gcm`; nothing here implements a cipher mode.
//!
//! Compatibility with Node, for every stored value:
//! - an envelope is read as UTF-16 code units, exactly as JS sees the string: the prefix test is
//!   on units, and the base64url text is decoded like Node's `Buffer.from(s, 'base64url')`
//!   ([`decode_base64_node`]: the low byte of each unit, either alphabet, other characters
//!   skipped, `=` ends the data, a trailing partial group kept);
//! - plaintext crosses as bytes (Node encodes it to UTF-8 and decodes the result, so lossy
//!   conversion of lone surrogates or invalid UTF-8 happens on the Node side, identically).
//!
//! One deliberate difference: a decoded envelope shorter than 28 bytes (a tag shorter than 16
//! bytes) is refused. Node would check such a truncated tag; Node never writes one.
//!
//! Failures carry only a fixed code ([`Error::code`]); no key, plaintext or ciphertext is ever in
//! an error. Decrypted plaintext is held in [`Zeroizing`] buffers, so a failed tag check (and every
//! intermediate copy) is wiped.
#![forbid(unsafe_code)]

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce, Tag};
use std::fmt;
use zeroize::Zeroizing;

pub use zeroize;

/// Key length (AES-256).
pub const KEY_BYTES: usize = 32;
/// Nonce (IV) length.
pub const NONCE_BYTES: usize = 12;
/// GCM tag length.
pub const TAG_BYTES: usize = 16;
/// Largest plaintext [`seal`] accepts: 8 MiB (credentials are kilobytes).
pub const MAX_PLAIN_BYTES: usize = 8 * 1024 * 1024;
/// Largest value [`open`] accepts, in UTF-16 units: room for any [`seal`] output plus slack.
pub const MAX_ENVELOPE_UNITS: usize = 12 * 1024 * 1024;
/// Largest user id (UTF-8 bytes).
pub const MAX_USER_BYTES: usize = 64 * 1024;

const AAD_PREFIX: &[u8] = b"noevia:user:";

/// Why a call failed. Codes are fixed and say nothing about the inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A `enc:v2:` value without a user id (JS: 'credential is bound to an account').
    Bound,
    /// No key opens the value: wrong key, wrong user, truncated, tampered or malformed.
    Unopenable,
    /// An input over its cap.
    TooLarge,
    /// A key or nonce of the wrong length.
    Input,
}

impl Error {
    /// A stable machine-readable code, used across the WebAssembly boundary.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Bound => "bound",
            Error::Unopenable => "unopenable",
            Error::TooLarge => "too_large",
            Error::Input => "input",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// JS `versionOf`: 2 for `enc:v2:`, 1 for `enc:v1:`, else 0.
pub fn version_of(units: &[u16]) -> u8 {
    let starts =
        |p: &[u8]| units.len() >= p.len() && units.iter().zip(p).all(|(u, b)| *u == u16::from(*b));
    if starts(b"enc:v2:") {
        2
    } else if starts(b"enc:v1:") {
        1
    } else {
        0
    }
}

fn b64_value(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some(u32::from(c - b'A')),
        b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Node's `Buffer.from(s, 'base64url')` on a JS string given as UTF-16 units.
pub fn decode_base64_node(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len() / 4 * 3 + 3);
    let (mut acc, mut n) = (0u32, 0u8);
    for &u in units {
        // Node reads a two-byte string's units through their low byte.
        let c = (u & 0xff) as u8;
        if c == b'=' {
            break;
        }
        let Some(v) = b64_value(c) else { continue };
        acc = (acc << 6) | v;
        n += 1;
        if n == 4 {
            out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]);
            (acc, n) = (0, 0);
        }
    }
    match n {
        2 => out.push((acc >> 4) as u8),
        3 => out.extend_from_slice(&[(acc >> 10) as u8, (acc >> 2) as u8]),
        _ => {}
    }
    out
}

/// Canonical unpadded base64url (Node's `toString('base64url')`).
pub fn encode_base64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let ch = |v: u32| char::from(A.get((v & 63) as usize).copied().unwrap_or(b'A'));
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let v = (b(0) << 16) | (b(1) << 8) | b(2);
        out.push(ch(v >> 18));
        out.push(ch(v >> 12));
        if chunk.len() > 1 {
            out.push(ch(v >> 6));
        }
        if chunk.len() > 2 {
            out.push(ch(v));
        }
    }
    out
}

fn aad(version: u8, user: Option<&[u8]>) -> Zeroizing<Vec<u8>> {
    let mut a = Zeroizing::new(Vec::new());
    if version == 2 {
        if let Some(u) = user {
            a.extend_from_slice(AAD_PREFIX);
            a.extend_from_slice(u);
        }
    }
    a
}

fn cipher(key: &[u8]) -> Result<Aes256Gcm, Error> {
    if key.len() != KEY_BYTES {
        return Err(Error::Input);
    }
    Ok(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)))
}

/// JS `decryptWith`: open the decoded envelope `raw` (`iv | tag | body`) under `key`. `user` is
/// the AAD's user id, used only for version 2.
pub fn decrypt_with(
    key: &[u8],
    raw: &[u8],
    version: u8,
    user: Option<&[u8]>,
) -> Result<Zeroizing<Vec<u8>>, Error> {
    let gcm = cipher(key)?;
    let (Some(iv), Some(tag), Some(body)) = (
        raw.get(..NONCE_BYTES),
        raw.get(NONCE_BYTES..NONCE_BYTES + TAG_BYTES),
        raw.get(NONCE_BYTES + TAG_BYTES..),
    ) else {
        return Err(Error::Unopenable);
    };
    // Wrapped before decrypting: a failed tag check must not leave bytes behind.
    let mut buf = Zeroizing::new(body.to_vec());
    gcm.decrypt_in_place_detached(
        Nonce::from_slice(iv),
        &aad(version, user),
        &mut buf,
        Tag::from_slice(tag),
    )
    .map_err(|_| Error::Unopenable)?;
    Ok(buf)
}

/// Which key opened a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyUsed {
    /// Not an envelope: the caller keeps the value as it is.
    None,
    Current,
    Previous,
}

impl KeyUsed {
    /// Wire tag.
    pub fn tag(self) -> u8 {
        match self {
            KeyUsed::None => 0,
            KeyUsed::Current => 1,
            KeyUsed::Previous => 2,
        }
    }
}

/// JS `open`: `units` is the stored value as UTF-16 units; `user` is `Some(String(userId))` when
/// the JS `hasUser(userId)`. Tries `current`, then `previous`. Plaintext is empty for
/// [`KeyUsed::None`].
pub fn open(
    current: &[u8],
    previous: Option<&[u8]>,
    units: &[u16],
    user: Option<&[u8]>,
) -> Result<(KeyUsed, Zeroizing<Vec<u8>>), Error> {
    if units.len() > MAX_ENVELOPE_UNITS || user.is_some_and(|u| u.len() > MAX_USER_BYTES) {
        return Err(Error::TooLarge);
    }
    if current.len() != KEY_BYTES || previous.is_some_and(|k| k.len() != KEY_BYTES) {
        return Err(Error::Input);
    }
    let version = version_of(units);
    if version == 0 {
        return Ok((KeyUsed::None, Zeroizing::new(Vec::new())));
    }
    if version == 2 && user.is_none() {
        return Err(Error::Bound);
    }
    let raw = decode_base64_node(units.get(7..).unwrap_or(&[]));
    match decrypt_with(current, &raw, version, user) {
        Ok(plain) => Ok((KeyUsed::Current, plain)),
        Err(e) => match previous {
            Some(prev) => decrypt_with(prev, &raw, version, user).map(|p| (KeyUsed::Previous, p)),
            None => Err(e),
        },
    }
}

/// JS `encrypt` after its empty-value check: `enc:v2:` (AAD bound to `user`) when `user` is
/// `Some`, else `enc:v1:`. `nonce` must be 12 fresh random bytes from the caller's CSPRNG.
pub fn seal(key: &[u8], nonce: &[u8], plain: &[u8], user: Option<&[u8]>) -> Result<String, Error> {
    if plain.len() > MAX_PLAIN_BYTES || user.is_some_and(|u| u.len() > MAX_USER_BYTES) {
        return Err(Error::TooLarge);
    }
    if nonce.len() != NONCE_BYTES {
        return Err(Error::Input);
    }
    let gcm = cipher(key)?;
    let version = if user.is_some() { 2 } else { 1 };
    let mut raw = Vec::with_capacity(NONCE_BYTES + TAG_BYTES + plain.len());
    raw.extend_from_slice(nonce);
    raw.extend_from_slice(&[0; TAG_BYTES]);
    raw.extend_from_slice(plain);
    let tag = {
        let body = raw.get_mut(NONCE_BYTES + TAG_BYTES..).ok_or(Error::Input)?;
        gcm.encrypt_in_place_detached(Nonce::from_slice(nonce), &aad(version, user), body)
            .map_err(|_| Error::TooLarge)?
    };
    if let Some(slot) = raw.get_mut(NONCE_BYTES..NONCE_BYTES + TAG_BYTES) {
        slot.copy_from_slice(&tag);
    }
    Ok(format!("enc:v{version}:{}", encode_base64url(&raw)))
}

/// UTF-16LE bytes to units (the wire form of a JS string); `None` for an odd length.
pub fn units_from_le(bytes: &[u8]) -> Option<Vec<u16>> {
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    Some(pairs.iter().map(|p| u16::from_le_bytes(*p)).collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn versions_and_base64() {
        assert_eq!(version_of(&u("enc:v2:x")), 2);
        assert_eq!(version_of(&u("enc:v1:")), 1);
        assert_eq!(version_of(&u("enc:v3:x")), 0);
        assert_eq!(version_of(&u("enc:v1")), 0);
        assert_eq!(decode_base64_node(&u("YQ")), b"a");
        assert_eq!(decode_base64_node(&u("aGVsb=G8")), b"hel");
        assert_eq!(
            decode_base64_node(&u("aGVs\u{161}bG8")),
            [0x68, 0x65, 0x6c, 0x69, 0xb1, 0xbc]
        );
        assert_eq!(encode_base64url(b"hello world!!"), "aGVsbG8gd29ybGQhIQ");
    }

    #[test]
    fn round_trip_and_refusals() {
        let k = [7u8; 32];
        let other = [8u8; 32];
        let n = [1u8; 12];
        let v2 = seal(&k, &n, "s\u{e9}cret".as_bytes(), Some(b"u1")).unwrap();
        assert!(v2.starts_with("enc:v2:"));
        let (used, p) = open(&k, None, &u(&v2), Some(b"u1")).unwrap();
        assert_eq!(
            (used, p.as_slice()),
            (KeyUsed::Current, "s\u{e9}cret".as_bytes())
        );
        assert_eq!(
            open(&k, None, &u(&v2), Some(b"u2")).unwrap_err(),
            Error::Unopenable
        );
        assert_eq!(open(&k, None, &u(&v2), None).unwrap_err(), Error::Bound);
        assert_eq!(
            open(&other, Some(&k), &u(&v2), Some(b"u1")).unwrap().0,
            KeyUsed::Previous
        );
        assert_eq!(
            open(&other, None, &u(&v2), Some(b"u1")).unwrap_err(),
            Error::Unopenable
        );
        let v1 = seal(&k, &n, b"", None).unwrap();
        assert_eq!(open(&k, None, &u(&v1), Some(b"x")).unwrap().1.len(), 0);
        assert_eq!(open(&k, None, &u("plain"), None).unwrap().0, KeyUsed::None);
        assert_eq!(
            open(&k, None, &u("enc:v1:"), None).unwrap_err(),
            Error::Unopenable
        );
        assert_eq!(seal(&k, &[0; 11], b"a", None).unwrap_err(), Error::Input);
        assert_eq!(seal(&k[..31], &n, b"a", None).unwrap_err(), Error::Input);
    }
}
