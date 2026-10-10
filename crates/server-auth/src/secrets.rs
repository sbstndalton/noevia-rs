//! core secrets.cjs key loading and `open`, read-only: `UI_DATA_DIR/secrets.key` (raw 32 bytes)
//! and the retired key for rotation, `secrets.key.previous` (raw 32 bytes) or else
//! `SECRETS_KEY_PREVIOUS` (64 hex digits, else Node-lenient base64). A previous key equal to the
//! current one is dropped. Envelopes (`enc:v1:`, `enc:v2:` bound to `noevia:user:<id>`) are
//! opened by the shared `secret-envelope` crate, current key first, then the previous one.
//!
//! Unlike Node this never creates `secrets.key` (Node does, on its first start): a missing key is
//! an error. Keys are zeroed on drop and never printed.

use secret_envelope::KeyUsed;
use std::path::Path;
use zeroize::Zeroizing;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretsError {
    /// `secrets.key` is missing (Node has not started) or unreadable.
    NoKey,
    /// `invalid credential-encryption key at <file>` / `invalid previous credential-encryption key`.
    BadKey(&'static str),
    /// No key opens the value (or it is bound and no user was given).
    Unreadable,
}

impl std::fmt::Display for SecretsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretsError::NoKey => write!(f, "secrets.key is missing"),
            SecretsError::BadKey(which) => write!(
                f,
                "invalid {which} credential-encryption key (need 32 bytes)"
            ),
            SecretsError::Unreadable => write!(
                f,
                "the credential cannot be opened with the configured keys"
            ),
        }
    }
}

impl std::error::Error for SecretsError {}

pub struct SecretKeys {
    current: Zeroizing<Vec<u8>>,
    previous: Option<Zeroizing<Vec<u8>>>,
}

impl std::fmt::Debug for SecretKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretKeys")
            .field("previous", &self.previous.is_some())
            .finish_non_exhaustive()
    }
}

fn decode_previous_env(text: &str) -> Vec<u8> {
    let hex = text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit());
    if hex {
        return text
            .as_bytes()
            .chunks(2)
            .filter_map(|p| {
                std::str::from_utf8(p)
                    .ok()
                    .and_then(|s| u8::from_str_radix(s, 16).ok())
            })
            .collect();
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    secret_envelope::decode_base64_node(&units)
}

impl SecretKeys {
    /// Loads as createSecretStore does (without generating). `previous_env` is
    /// `SECRETS_KEY_PREVIOUS`.
    pub fn load(data_dir: &Path, previous_env: Option<&str>) -> Result<Self, SecretsError> {
        let current = Zeroizing::new(
            std::fs::read(data_dir.join("secrets.key")).map_err(|_| SecretsError::NoKey)?,
        );
        if current.len() != secret_envelope::KEY_BYTES {
            return Err(SecretsError::BadKey("current"));
        }
        let prev_file = data_dir.join("secrets.key.previous");
        let previous = if prev_file.exists() {
            Some(Zeroizing::new(
                std::fs::read(&prev_file).map_err(|_| SecretsError::BadKey("previous"))?,
            ))
        } else {
            match previous_env {
                // Node: `else if (env.SECRETS_KEY_PREVIOUS)`, then String(...).trim().
                Some(v) if !v.is_empty() => {
                    let units: Vec<u16> = v.encode_utf16().collect();
                    let text = String::from_utf16_lossy(policy_leaves::js_trim(&units));
                    Some(Zeroizing::new(decode_previous_env(&text)))
                }
                _ => None,
            }
        };
        if previous
            .as_ref()
            .is_some_and(|p| p.len() != secret_envelope::KEY_BYTES)
        {
            return Err(SecretsError::BadKey("previous"));
        }
        let previous = previous.filter(|p| p.as_slice() != current.as_slice());
        Ok(SecretKeys { current, previous })
    }

    pub fn has_previous(&self) -> bool {
        self.previous.is_some()
    }

    /// secrets.cjs `open(value, userId)`: `(plain, keyUsed)`. A value that is not an envelope is
    /// returned as it is with [`KeyUsed::None`]. `user_id` is bound only when non-empty
    /// (secret-envelope.cjs `hasUser`).
    pub fn open(
        &self,
        value: &str,
        user_id: Option<&str>,
    ) -> Result<(Zeroizing<String>, KeyUsed), SecretsError> {
        let units: Vec<u16> = value.encode_utf16().collect();
        let user = user_id.filter(|u| !u.is_empty()).map(str::as_bytes);
        let (used, plain) = secret_envelope::open(
            &self.current,
            self.previous.as_deref().map(Vec::as_slice),
            &units,
            user,
        )
        .map_err(|_| SecretsError::Unreadable)?;
        if used == KeyUsed::None {
            return Ok((Zeroizing::new(value.to_string()), KeyUsed::None));
        }
        // Buffer#toString('utf8'): invalid sequences become U+FFFD.
        Ok((
            Zeroizing::new(String::from_utf8_lossy(&plain).into_owned()),
            used,
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn dir_with(current: &[u8], previous: Option<&[u8]>) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("secrets.key"), current).unwrap();
        if let Some(p) = previous {
            std::fs::write(d.path().join("secrets.key.previous"), p).unwrap();
        }
        d
    }

    #[test]
    fn rotation_semantics() {
        let (k1, k2) = ([1u8; 32], [2u8; 32]);
        let nonce = [9u8; 12];
        let old_v2 = secret_envelope::seal(&k1, &nonce, b"s3cret", Some(b"u-1")).unwrap();
        let old_v1 = secret_envelope::seal(&k1, &nonce, b"plain-v1", None).unwrap();
        // After rotation: k2 current, k1 previous (file).
        let d = dir_with(&k2, Some(&k1));
        let keys = SecretKeys::load(d.path(), None).unwrap();
        assert!(keys.has_previous());
        let (p, used) = keys.open(&old_v2, Some("u-1")).unwrap();
        assert_eq!((p.as_str(), used), ("s3cret", KeyUsed::Previous));
        assert_eq!(keys.open(&old_v1, None).unwrap().1, KeyUsed::Previous);
        // Bound to another account, or no account: refused.
        assert_eq!(
            keys.open(&old_v2, Some("u-2")).err(),
            Some(SecretsError::Unreadable)
        );
        assert_eq!(
            keys.open(&old_v2, Some("")).err(),
            Some(SecretsError::Unreadable)
        );
        // Not an envelope: as is.
        assert_eq!(keys.open("not-enc", None).unwrap().1, KeyUsed::None);
        // Previous removed: the old value is unreadable.
        let d = dir_with(&k2, None);
        let keys = SecretKeys::load(d.path(), None).unwrap();
        assert!(!keys.has_previous());
        assert_eq!(
            keys.open(&old_v2, Some("u-1")).err(),
            Some(SecretsError::Unreadable)
        );
        // SECRETS_KEY_PREVIOUS as hex (trimmed) and as base64, when no previous file exists.
        let hex: String = k1.iter().map(|b| format!("{b:02x}")).collect();
        let keys = SecretKeys::load(d.path(), Some(&format!(" {hex}\n"))).unwrap();
        assert_eq!(
            keys.open(&old_v2, Some("u-1")).unwrap().1,
            KeyUsed::Previous
        );
        let b64 = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
        let keys = SecretKeys::load(d.path(), Some(b64)).unwrap();
        assert_eq!(keys.open(&old_v1, None).unwrap().1, KeyUsed::Previous);
        // Current key opens current values first.
        let new_v2 = secret_envelope::seal(&k2, &nonce, b"fresh", Some(b"u-1")).unwrap();
        assert_eq!(keys.open(&new_v2, Some("u-1")).unwrap().1, KeyUsed::Current);
    }

    #[test]
    fn key_errors() {
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            SecretKeys::load(empty.path(), None).err(),
            Some(SecretsError::NoKey)
        );
        assert!(
            !empty.path().join("secrets.key").exists(),
            "never generated"
        );
        let d = dir_with(&[1u8; 31], None);
        assert_eq!(
            SecretKeys::load(d.path(), None).err(),
            Some(SecretsError::BadKey("current"))
        );
        let d = dir_with(&[1u8; 32], Some(&[2u8; 33]));
        assert_eq!(
            SecretKeys::load(d.path(), None).err(),
            Some(SecretsError::BadKey("previous"))
        );
        let d = dir_with(&[1u8; 32], None);
        assert_eq!(
            SecretKeys::load(d.path(), Some("abcd")).err(),
            Some(SecretsError::BadKey("previous"))
        );
        // Equal previous is ignored.
        let d = dir_with(&[1u8; 32], Some(&[1u8; 32]));
        assert!(!SecretKeys::load(d.path(), None).unwrap().has_previous());
        let dbg = format!("{:?}", SecretKeys::load(d.path(), None).unwrap());
        assert!(!dbg.contains("[1"), "{dbg}");
    }
}
