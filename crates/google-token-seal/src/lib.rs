//! The sealed token files of the Google Drive connections (sbstndalton/noevia#2190 round 7), in
//! the format noevia-core's `server/gdrive.cjs` (`sealer`) wrote, so a Node build can still read
//! what Rust saved and the other way round (rollback):
//!
//! - the AES key is `HKDF-SHA256(ikm = backupKey, salt = empty, info = "noevia google token v1")`;
//! - the file is `base64(iv(12) | tag(16) | body)` of AES-256-GCM with no associated data over the
//!   JSON text of the record (`JSON.stringify`, core's key order is the caller's business);
//! - `backupKey` is `secrets.cjs` `derive('google-drive-user')` for an account's own connection
//!   ([`derive_secret_key`]) and the raw 32 bytes of `OFFSITE_BACKUP_KEY_FILE` for the off-site
//!   backup's slot.
//!
//! The nonce is the caller's (random for a new seal, fixed in the golden vector), so this crate
//! needs no RNG. Failures are fixed codes; no key, token or ciphertext is ever in an error.
#![forbid(unsafe_code)]

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce, Tag};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

/// The HKDF info of the token file key (`gdrive.cjs` `sealer`).
pub const FILE_KEY_INFO: &[u8] = b"noevia google token v1";
/// The secrets.cjs label of an account's own connection key.
pub const USER_LABEL: &str = "google-drive-user";

/// Why a call failed. Codes are fixed and say nothing about the inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A key that is not 32 bytes.
    Key,
    /// Truncated, tampered, malformed or sealed under another key.
    Unopenable,
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::Key => "key",
            Error::Unopenable => "unopenable",
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// RFC 5869 HKDF-SHA256 with an empty salt (Node's `hkdfSync('sha256', ikm, Buffer.alloc(0), info,
/// 32)`): a zero-filled salt of the hash length, one expand block.
pub fn hkdf_sha256_32(ikm: &[u8], info: &[u8]) -> Zeroizing<[u8; 32]> {
    type H = Hmac<Sha256>;
    let mut out = Zeroizing::new([0u8; 32]);
    // HMAC accepts any key length, so these cannot fail; a failure leaves the zero key, which
    // opens nothing real (the seal/open callers check their own tags).
    let Ok(mut extract) = <H as Mac>::new_from_slice(&[0u8; 32]) else {
        return out;
    };
    extract.update(ikm);
    let mut prk = Zeroizing::new([0u8; 32]);
    prk.copy_from_slice(&extract.finalize().into_bytes());
    let Ok(mut expand) = <H as Mac>::new_from_slice(prk.as_slice()) else {
        return out;
    };
    expand.update(info);
    expand.update(&[1u8]);
    out.copy_from_slice(&expand.finalize().into_bytes());
    out
}

/// secrets.cjs `derive(label)` (32 bytes): `HKDF(secretsKey, salt = empty, info = "noevia:<label>")`.
pub fn derive_secret_key(secrets_key: &[u8], label: &str) -> Zeroizing<[u8; 32]> {
    hkdf_sha256_32(secrets_key, format!("noevia:{label}").as_bytes())
}

/// A token file's sealer for one backup key.
pub struct Sealer {
    key: Zeroizing<[u8; 32]>,
}

impl Sealer {
    /// `sealer(backupKey)`: the backup key must be 32 bytes (every caller's key is).
    pub fn new(backup_key: &[u8]) -> Result<Sealer, Error> {
        if backup_key.len() != 32 {
            return Err(Error::Key);
        }
        Ok(Sealer {
            key: hkdf_sha256_32(backup_key, FILE_KEY_INFO),
        })
    }

    /// `base64(iv | tag | body)` of `plain` under `iv` (standard alphabet, padded, as Node's
    /// `Buffer#toString('base64')`).
    pub fn seal(&self, plain: &[u8], iv: &[u8; 12]) -> Result<String, Error> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(self.key.as_slice()));
        let mut body = Zeroizing::new(plain.to_vec());
        let tag = cipher
            .encrypt_in_place_detached(Nonce::from_slice(iv), b"", body.as_mut_slice())
            .map_err(|_| Error::Unopenable)?;
        let mut raw = Vec::with_capacity(28 + body.len());
        raw.extend_from_slice(iv);
        raw.extend_from_slice(tag.as_slice());
        raw.extend_from_slice(body.as_slice());
        Ok(base64::engine::general_purpose::STANDARD.encode(raw))
    }

    /// The plaintext of a sealed file's text (read like Node's `Buffer.from(text, 'base64')`:
    /// either alphabet, other characters skipped).
    pub fn open(&self, text: &str) -> Result<Zeroizing<Vec<u8>>, Error> {
        let units: Vec<u16> = text.encode_utf16().collect();
        let raw = secret_envelope::decode_base64_node(&units);
        if raw.len() < 28 {
            return Err(Error::Unopenable);
        }
        let (iv, rest) = raw.split_at(12);
        let (tag, body) = rest.split_at(16);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(self.key.as_slice()));
        let mut plain = Zeroizing::new(body.to_vec());
        cipher
            .decrypt_in_place_detached(
                Nonce::from_slice(iv),
                b"",
                plain.as_mut_slice(),
                Tag::from_slice(tag),
            )
            .map_err(|_| Error::Unopenable)?;
        Ok(plain)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    // Computed with Node's own crypto.hkdfSync / createCipheriv (scratch vectors.cjs, the
    // algorithm of gdrive.cjs `sealer` and secrets.cjs `derive`), independently of this crate.
    const SECRETS_KEY: [u8; 32] = [7; 32];
    const USER_KEY_HEX: &str = "d007234e22afa7f8461024aaad95649026139bf3eaff590a6ea0b40a71e012ab";
    const FILE_KEY_HEX: &str = "d157dabe0cfb863085f88d0ea5c817a3d2dfdec0b422d15a0c8a02b571caac0f";
    const USER_JSON: &str = r#"{"refreshToken":"synthetic-refresh","email":"ada@example.test","connectedAt":1700000000000,"owner":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"}"#;
    const USER_SEALED: &str = "AQIDBAUGBwgJCgsMgIgxgDi+UwWrjb2pgZHTVk5Oe2S+sxp0cQhofG56mOyqJu4MKObbx2YjiOIp95XFlLRpoFsgDgJ1/IeS38BvXz/+MZVPwozLiOOvRCG99kgyZhbaVBN47NmtMEBpqgbKbH3pg0zMV/11xgL0BXiHW9DA/DYPs9ifgguMcyvGCrLfU3M/fg5aQqY2xrOOpQtYrhaJqw43qj4B6g==";
    const BACKUP_JSON: &str =
        r#"{"refreshToken":"synthetic-refresh","email":null,"connectedAt":1700000000000}"#;
    const BACKUP_SEALED: &str = "AQIDBAUGBwgJCgsMrHkYwn/pjtr/t8EoEy1obkSdr1QTL/KERkyb1Pd0H+oOAOmzf58OuFgPVcNTUJjp7N7a5B6V2Wd0lCMnWan7RJrvaJqdAS1LxTXMCRAte2oRr1UiimyzIqusKhhq";
    const IV: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn derive_is_secrets_cjs_derive() {
        assert_eq!(
            hex(derive_secret_key(&SECRETS_KEY, USER_LABEL).as_slice()),
            USER_KEY_HEX
        );
    }

    #[test]
    fn the_file_key_is_the_sealer_key() {
        let key = derive_secret_key(&SECRETS_KEY, USER_LABEL);
        assert_eq!(
            hex(hkdf_sha256_32(key.as_slice(), FILE_KEY_INFO).as_slice()),
            FILE_KEY_HEX
        );
    }

    #[test]
    fn an_accounts_file_is_node_bytes() {
        let key = derive_secret_key(&SECRETS_KEY, USER_LABEL);
        let sealer = Sealer::new(key.as_slice()).unwrap();
        assert_eq!(sealer.seal(USER_JSON.as_bytes(), &IV).unwrap(), USER_SEALED);
        assert_eq!(
            sealer.open(USER_SEALED).unwrap().as_slice(),
            USER_JSON.as_bytes()
        );
    }

    #[test]
    fn the_backup_slot_is_node_bytes_under_its_own_key() {
        let sealer = Sealer::new(&[9u8; 32]).unwrap();
        assert_eq!(
            sealer.seal(BACKUP_JSON.as_bytes(), &IV).unwrap(),
            BACKUP_SEALED
        );
        assert_eq!(
            sealer.open(BACKUP_SEALED).unwrap().as_slice(),
            BACKUP_JSON.as_bytes()
        );
        // The account key does not open the backup slot, and the other way round.
        let other = Sealer::new(derive_secret_key(&SECRETS_KEY, USER_LABEL).as_slice()).unwrap();
        assert_eq!(other.open(BACKUP_SEALED).unwrap_err(), Error::Unopenable);
    }

    #[test]
    fn damage_and_short_input_are_refused() {
        let sealer = Sealer::new(&[9u8; 32]).unwrap();
        assert!(sealer.open("").is_err());
        assert!(sealer.open("AQID").is_err());
        let mut raw = BACKUP_SEALED.to_owned().into_bytes();
        raw[60] = if raw[60] == b'A' { b'B' } else { b'A' };
        assert!(sealer.open(&String::from_utf8(raw).unwrap()).is_err());
        assert_eq!(Sealer::new(&[1u8; 31]).err(), Some(Error::Key));
    }

    #[test]
    fn the_url_alphabet_and_whitespace_are_read_like_node() {
        let sealer = Sealer::new(&[9u8; 32]).unwrap();
        let spaced = format!("{}\n", BACKUP_SEALED.replace('/', "_").replace('+', "-"));
        assert_eq!(
            sealer.open(&spaced).unwrap().as_slice(),
            BACKUP_JSON.as_bytes()
        );
    }
}
