//! The off-site backup's store format (`noevia-offsite-v1`), ported from noevia-core's
//! `server/offsite-backup.cjs` and `offsite-dir.cjs`, byte-compatible both ways so a store written
//! by Node is read by Rust and the other way round (rollback). No filesystem, network or clock.
//!
//! Layout in the store (every object is encrypted; names reveal nothing about content):
//!
//! - `config`                 sealed `{format, created}`: proves the key before any write;
//! - `data/<aa>/<id>`         one sealed chunk (4 MiB of a file); `id` = HMAC-SHA256(idKey, plaintext);
//! - `snapshots/<time>-<rnd>` a sealed manifest `{format, time, roots, files:[{root, path, size,
//!   mode, mtime, sha256, chunks}], incomplete}`.
//!
//! Crypto: AES-256-GCM, object = `nonce(12) | ciphertext | tag(16)`, no associated data; the two
//! keys are `HMAC-SHA256(master, "noevia-offsite-v1:encrypt")` and `"...:chunk-id"`. The nonce is
//! the caller's (random in production, fixed in the golden vectors), so there is no RNG here.
//! Failures are fixed codes: no key, plaintext or object ever appears in an error.
#![forbid(unsafe_code)]

pub mod schedule;

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce, Tag};
use hmac::{Hmac, Mac};
use js_json::JValue;
use sha2::Sha256;
use std::collections::HashSet;
use zeroize::Zeroizing;

pub const FORMAT: &str = "noevia-offsite-v1";
/// A file is stored in pieces of this size.
pub const CHUNK: usize = 4 * 1024 * 1024;

/// How many snapshots each retention bucket keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub daily: usize,
    pub weekly: usize,
    pub monthly: usize,
}
pub const RETENTION: Retention = Retention {
    daily: 7,
    weekly: 4,
    monthly: 6,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The master key is not 32 bytes (core: "The backup key must be 32 bytes (64 hex characters).").
    KeyLength,
    /// An object shorter than nonce + tag (core 422: "A backup object is truncated.").
    Truncated,
    /// Wrong key or tampered (core 422: "A backup object failed authentication ...").
    Authentication,
    /// A manifest or config that is not what this format writes.
    Invalid,
    /// The cipher refused to encrypt (no object is produced; a sealed object without its tag would
    /// be unreadable).
    Seal,
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::KeyLength => "key_length",
            Error::Truncated => "truncated",
            Error::Authentication => "authentication",
            Error::Invalid => "invalid",
            Error::Seal => "seal",
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// The two keys derived from the master key.
pub struct Keys {
    enc: Zeroizing<[u8; 32]>,
    id: Zeroizing<[u8; 32]>,
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut out = [0u8; 32];
    if let Ok(mut mac) = <Hmac<Sha256> as Mac>::new_from_slice(key) {
        for p in parts {
            mac.update(p);
        }
        out.copy_from_slice(&mac.finalize().into_bytes());
    }
    out
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl Keys {
    /// `deriveKeys(master)`.
    pub fn derive(master: &[u8]) -> Result<Keys, Error> {
        if master.len() != 32 {
            return Err(Error::KeyLength);
        }
        let label =
            |l: &str| Zeroizing::new(hmac_sha256(master, &[format!("{FORMAT}:{l}").as_bytes()]));
        Ok(Keys {
            enc: label("encrypt"),
            id: label("chunk-id"),
        })
    }

    /// `seal(keys, plaintext)` with the caller's nonce: `nonce | ciphertext | tag`. An encryption
    /// error yields no object at all: a body without its tag would be written and only fail at
    /// restore time.
    pub fn seal(&self, plain: &[u8], nonce: &[u8; 12]) -> Result<Vec<u8>, Error> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(self.enc.as_slice()));
        let mut body = plain.to_vec();
        let tag = cipher
            .encrypt_in_place_detached(Nonce::from_slice(nonce), b"", &mut body)
            .map_err(|_| Error::Seal)?;
        let mut out = Vec::with_capacity(28 + body.len());
        out.extend_from_slice(nonce);
        out.extend_from_slice(&body);
        out.extend_from_slice(tag.as_slice());
        Ok(out)
    }

    /// `open(keys, sealed)`.
    pub fn open(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
        if sealed.len() < 28 {
            return Err(Error::Truncated);
        }
        let (nonce, rest) = sealed.split_at(12);
        let (body, tag) = rest.split_at(rest.len() - 16);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(self.enc.as_slice()));
        let mut plain = Zeroizing::new(body.to_vec());
        cipher
            .decrypt_in_place_detached(
                Nonce::from_slice(nonce),
                b"",
                plain.as_mut_slice(),
                Tag::from_slice(tag),
            )
            .map_err(|_| Error::Authentication)?;
        Ok(plain)
    }

    /// A chunk's id: `HMAC-SHA256(idKey, plaintext)` in hex.
    pub fn chunk_id(&self, plain: &[u8]) -> String {
        hex(&hmac_sha256(self.id.as_slice(), &[plain]))
    }
}

/// `data/<aa>/<id>`.
pub fn data_key(id: &str) -> String {
    format!("data/{}/{id}", id.get(..2).unwrap_or(id))
}

/// The sealed `config` object's plaintext.
pub fn config_json(created: i64) -> String {
    js_json::stringify(&JValue::obj([
        ("format", JValue::Str(FORMAT.into())),
        ("created", JValue::Num(created as f64)),
    ]))
    .unwrap_or_default()
}

/// Whether an opened `config` is this format (`ensureConfig`).
pub fn config_is_current(plain: &[u8]) -> Result<bool, Error> {
    let v = js_json::parse(&String::from_utf8_lossy(plain)).map_err(|_| Error::Invalid)?;
    Ok(v.get("format").as_str() == Some(FORMAT))
}

/// One file of a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct FileEntry {
    pub root: usize,
    pub path: String,
    pub size: u64,
    pub mode: u32,
    pub mtime: i64,
    pub sha256: String,
    pub chunks: Vec<String>,
}

/// A snapshot's manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub time: i64,
    pub roots: usize,
    pub files: Vec<FileEntry>,
    pub incomplete: bool,
}

impl Manifest {
    /// `JSON.stringify` of core's manifest, in core's key order.
    pub fn to_json(&self) -> String {
        let files = self
            .files
            .iter()
            .map(|f| {
                JValue::obj([
                    ("root", JValue::Num(f.root as f64)),
                    ("path", JValue::Str(f.path.clone())),
                    ("size", JValue::Num(f.size as f64)),
                    ("mode", JValue::Num(f64::from(f.mode))),
                    ("mtime", JValue::Num(f.mtime as f64)),
                    ("sha256", JValue::Str(f.sha256.clone())),
                    (
                        "chunks",
                        JValue::Arr(f.chunks.iter().map(|c| JValue::Str(c.clone())).collect()),
                    ),
                ])
            })
            .collect();
        js_json::stringify(&JValue::obj([
            ("format", JValue::Str(FORMAT.into())),
            ("time", JValue::Num(self.time as f64)),
            ("roots", JValue::Num(self.roots as f64)),
            ("files", JValue::Arr(files)),
            ("incomplete", JValue::Bool(self.incomplete)),
        ]))
        .unwrap_or_default()
    }

    /// `readManifest`'s checks: this format and a `files` array, nothing more. Core only ever
    /// checked those two (every other field was read on use), and the first Node releases wrote
    /// manifests without `incomplete` and, in a pre-release shape, without some of `root`, `size`,
    /// `mode`, `mtime` or `time`. An entry missing a field takes a value that makes a restore refuse
    /// the file (`size` 0 against its chunks, an empty `sha256`, an empty `path`) rather than making
    /// the whole manifest unreadable: retention reads every kept manifest for its chunk list, and
    /// a manifest counted as corrupt there is a manifest whose chunks are not protected. A missing
    /// `time` is 0; the caller takes it from the snapshot's id ([`snapshot_time`]).
    pub fn parse(plain: &[u8]) -> Result<Manifest, Error> {
        let v = js_json::parse(&String::from_utf8_lossy(plain)).map_err(|_| Error::Invalid)?;
        if v.get("format").as_str() != Some(FORMAT) {
            return Err(Error::Invalid);
        }
        let JValue::Arr(items) = v.get("files") else {
            return Err(Error::Invalid);
        };
        let mut files = Vec::with_capacity(items.len());
        for f in items {
            // Chunk ids that are not strings reference nothing a store holds.
            let chunks = match f.get("chunks") {
                JValue::Arr(c) => c
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect(),
                _ => Vec::new(),
            };
            files.push(FileEntry {
                root: whole(f.get("root"))
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or(0),
                path: f.get("path").as_str().unwrap_or_default().to_owned(),
                size: whole(f.get("size"))
                    .and_then(|n| u64::try_from(n).ok())
                    .unwrap_or(0),
                mode: whole(f.get("mode"))
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0o600),
                mtime: whole(f.get("mtime")).unwrap_or(0),
                sha256: f.get("sha256").as_str().unwrap_or_default().to_owned(),
                chunks,
            });
        }
        Ok(Manifest {
            time: whole(v.get("time")).unwrap_or(0),
            roots: whole(v.get("roots"))
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(0),
            files,
            incomplete: v.get("incomplete").as_bool().unwrap_or(false),
        })
    }
}

fn whole(v: &JValue) -> Option<i64> {
    let n = v.as_num()?;
    (n.is_finite() && n.fract() == 0.0 && n.abs() < 9.0e15).then_some(n as i64)
}

/// `^[\w-]{1,80}$`.
pub fn valid_snapshot_id(id: &str) -> bool {
    (1..=80).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `^[a-f0-9]{64}$`.
pub fn valid_chunk_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A path inside a snapshot: relative, no empty, `.` or `..` segment (restore's check).
pub fn safe_relative_path(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
}

/// A key the folder store accepts (`offsite-dir.cjs` KEY): no empty, dot-led or `..` segment.
pub fn valid_dir_key(key: &str) -> bool {
    let first = |s: &str| s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric());
    !key.is_empty()
        && key.split('/').all(|seg| {
            first(seg)
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        })
        && !key.split('/').any(|s| s == "..")
}

// ---------------------------------------------------------------------------------------------
// Dates, as JS `Date` in UTC.

/// Days since 1970-01-01 to (year, month 1-12, day 1-31) (Howard Hinnant's civil_from_days).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) -> days since 1970-01-01.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `new Date(ms).toISOString().replace(/[:.]/g, '-')`: `YYYY-MM-DDTHH-mm-ss-SSSZ`.
pub fn iso_dashed(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}-{:02}-{:02}-{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

/// A new snapshot's id: the dashed time and 4 random bytes in hex.
pub fn snapshot_id(time_ms: i64, random: [u8; 4]) -> String {
    format!("{}-{}", iso_dashed(time_ms), hex(&random))
}

/// The time a snapshot id starts with (`YYYY-MM-DDTHH-mm-ss-SSSZ-xxxxxxxx`), for a manifest that
/// does not carry its own.
pub fn snapshot_time(id: &str) -> Option<i64> {
    let stamp = id.get(..24)?;
    let b = stamp.as_bytes();
    let n = |from: usize, to: usize| -> Option<i64> { stamp.get(from..to)?.parse().ok() };
    let shape = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b'-'),
        (16, b'-'),
        (19, b'-'),
        (23, b'Z'),
    ];
    if shape.iter().any(|(i, c)| b.get(*i) != Some(c)) {
        return None;
    }
    let (y, mo, d) = (
        n(0, 4)?,
        u32::try_from(n(5, 7)?).ok()?,
        u32::try_from(n(8, 10)?).ok()?,
    );
    let (h, mi, s, ms) = (n(11, 13)?, n(14, 16)?, n(17, 19)?, n(20, 23)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400_000 + h * 3_600_000 + mi * 60_000 + s * 1000 + ms)
}

fn bucket(time_ms: i64, kind: usize) -> String {
    let days = time_ms.div_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    match kind {
        0 => format!("{y:04}-{m:02}-{d:02}"),
        2 => format!("{y:04}-{m:02}"),
        _ => {
            // ISO week, as core's JS: shift to the Thursday of the week, then count weeks from Jan 1.
            let weekday = (days + 4).rem_euclid(7); // 0 = Sunday
            let day = if weekday == 0 { 7 } else { weekday };
            let thursday = days + 4 - day;
            let (ty, _, _) = civil_from_days(thursday);
            let year_start = days_from_civil(ty, 1, 1);
            let n = (thursday - year_start + 1 + 6) / 7; // ceil((delta+1)/7) for whole days
            format!("{ty}-W{n}")
        }
    }
}

/// `retain(snapshots, policy)`: the ids to keep (the newest of each day, ISO week and month
/// bucket, up to the policy, and always the newest snapshot).
pub fn retain(snapshots: &[(String, i64)], policy: Retention) -> HashSet<String> {
    let all: Vec<(String, i64, bool)> = snapshots
        .iter()
        .map(|(id, time)| (id.clone(), *time, true))
        .collect();
    retain_complete(&all, policy)
}

/// [`retain`] when some snapshots are `incomplete` (a skipped file, an unreadable folder). A bucket
/// keeps its newest complete snapshot when it has one (a second, worse run on the same day must not
/// displace the good one), and the newest complete snapshot overall is always kept beside the newest
/// snapshot, so a run of bad nights never leaves nothing restorable. Items are `(id, time, complete)`.
pub fn retain_complete(snapshots: &[(String, i64, bool)], policy: Retention) -> HashSet<String> {
    let mut sorted: Vec<&(String, i64, bool)> = snapshots.iter().collect();
    sorted.sort_by_key(|a| std::cmp::Reverse(a.1));
    let mut keep = HashSet::new();
    if let Some(first) = sorted.first() {
        keep.insert(first.0.clone());
    }
    if let Some(good) = sorted.iter().find(|s| s.2) {
        keep.insert(good.0.clone());
    }
    for (kind, limit) in [(0, policy.daily), (1, policy.weekly), (2, policy.monthly)] {
        let mut order: Vec<String> = Vec::new();
        let mut best: std::collections::HashMap<String, (&str, bool)> =
            std::collections::HashMap::new();
        for (id, time, complete) in &sorted {
            let b = bucket(*time, kind);
            match best.get_mut(&b) {
                Some(slot) => {
                    if !slot.1 && *complete {
                        *slot = (id.as_str(), true);
                    }
                }
                None => {
                    if order.len() >= limit {
                        break;
                    }
                    order.push(b.clone());
                    best.insert(b, (id.as_str(), *complete));
                }
            }
        }
        keep.extend(best.values().map(|(id, _)| (*id).to_owned()));
    }
    keep
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    // Computed with Node: offsite-backup.cjs deriveKeys/seal (crypto.randomBytes fixed to 1..12),
    // the HMAC chunk id, JSON.stringify and the real retain() (scratch vectors.cjs).
    const NONCE: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    const ENC_HEX: &str = "68f6ae5995092ad37acf6c2e61d6bf0e232de1c7ccc49b15da7239cd77e7a90e";
    const ID_HEX: &str = "eeeceff08e4a29017e30a2aa5f81e5dbbb6b05f202b739c0d9ceb53d0e198c77";
    const CONFIG_SEALED_B64: &str = "AQIDBAUGBwgJCgsMWOeLaN4qfTJGIxurey2KhrY5w3uWNXJ6/Lgk3umCvRo2UODl1EBaJlsjP9l8BGDTZ/dMrg+psp8/0HYNcIJzGLiygeYXiA==";
    const HELLO_ID: &str = "365572de215cae8a16117c1d112b38b9d523a76802382b1193c4be70f3537690";
    const MANIFEST_JSON: &str = r#"{"format":"noevia-offsite-v1","time":1700000000000,"roots":1,"files":[{"root":0,"path":"a/b.txt","size":5,"mode":384,"mtime":1700000000000,"sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","chunks":["365572de215cae8a16117c1d112b38b9d523a76802382b1193c4be70f3537690"]}],"incomplete":false}"#;
    const MANIFEST_SEALED_B64: &str = "AQIDBAUGBwgJCgsMWOeLaN4qfTJGIxurey2KhrY5w3uWNXJ6/Lgk3umCvQ0tWOSzixVPLFokP9l8BGDTZ/dQvE27GZVGjXHRLt0bTNuRFdIZL5H6jlfMVrI01qN3+sh2XrhJqv3VnLdQadLpmcnRfMNzJul5V49l6oG093SE6FBt1bxiNVz7XmCD9jn4BQK7rk08oM8mZ0YOTFJd2hCBp4QhsvJLWNbr0enu1Yy7y5W6xs9GKXXj6X+HR8h5OgASMjF+MPPzEbgF15bQu5UWA6WSXPRm90+L33AKQcFdyLv7gYEUuSL8ZIgyNpxrJVogkbODKn8HfajbnR/0bLy6EiTkbMFfiCEUBqa5Vm+ljzFg1fs1VoMNXo3RUQdpwgGzNG4O1+3JoX+1Qtt6OqTUjayP/FcW2pl1IJU1hQhXfvfeA5x1UQf0OEd/ZWa3bNxoBAABqjfW";

    fn b64(text: &str) -> Vec<u8> {
        // A tiny standard-alphabet decoder: the vectors above are unpadded or padded base64.
        let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let (mut acc, mut n, mut out) = (0u32, 0, Vec::new());
        for c in text.bytes().filter(|c| *c != b'=') {
            acc = (acc << 6) | table.iter().position(|t| *t == c).unwrap() as u32;
            n += 6;
            if n >= 8 {
                n -= 8;
                out.push((acc >> n) as u8);
                acc &= (1 << n) - 1;
            }
        }
        out
    }

    #[test]
    fn derived_keys_are_cores() {
        let k = Keys::derive(&[9; 32]).unwrap();
        assert_eq!(hex(k.enc.as_slice()), ENC_HEX);
        assert_eq!(hex(k.id.as_slice()), ID_HEX);
        assert_eq!(k.chunk_id(b"hello"), HELLO_ID);
        assert_eq!(Keys::derive(&[9; 31]).err(), Some(Error::KeyLength));
    }

    #[test]
    fn config_and_manifest_are_node_bytes() {
        let k = Keys::derive(&[9; 32]).unwrap();
        assert_eq!(
            k.seal(config_json(1_700_000_000_000).as_bytes(), &NONCE)
                .unwrap(),
            b64(CONFIG_SEALED_B64)
        );
        assert!(config_is_current(&k.open(&b64(CONFIG_SEALED_B64)).unwrap()).unwrap());
        let m = Manifest {
            time: 1_700_000_000_000,
            roots: 1,
            files: vec![FileEntry {
                root: 0,
                path: "a/b.txt".into(),
                size: 5,
                mode: 0o600,
                mtime: 1_700_000_000_000,
                sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
                chunks: vec![HELLO_ID.into()],
            }],
            incomplete: false,
        };
        assert_eq!(m.to_json(), MANIFEST_JSON);
        assert_eq!(
            k.seal(m.to_json().as_bytes(), &NONCE).unwrap(),
            b64(MANIFEST_SEALED_B64)
        );
        let opened = k.open(&b64(MANIFEST_SEALED_B64)).unwrap();
        assert_eq!(Manifest::parse(&opened).unwrap(), m);
    }

    #[test]
    fn wrong_key_truncation_and_damage_are_refused() {
        let k = Keys::derive(&[9; 32]).unwrap();
        let other = Keys::derive(&[8; 32]).unwrap();
        let sealed = b64(CONFIG_SEALED_B64);
        assert_eq!(other.open(&sealed).err(), Some(Error::Authentication));
        assert_eq!(k.open(&sealed[..27]).err(), Some(Error::Truncated));
        let mut bad = sealed.clone();
        bad[20] ^= 1;
        assert_eq!(k.open(&bad).err(), Some(Error::Authentication));
    }

    #[test]
    fn manifests_of_another_format_or_shape_are_invalid() {
        assert!(Manifest::parse(br#"{"format":"x","files":[]}"#).is_err());
        assert!(Manifest::parse(br#"{"format":"noevia-offsite-v1","time":1}"#).is_err());
        assert!(Manifest::parse(br#"{"format":"noevia-offsite-v1","files":{}}"#).is_err());
        assert!(Manifest::parse(b"nope").is_err());
        assert!(!config_is_current(br#"{"format":"x"}"#).unwrap());
    }

    #[test]
    fn an_older_node_manifest_missing_fields_still_lists_its_chunks() {
        // The first Node release wrote no `incomplete`; a pre-release shape lacked more. Retention
        // protects the chunks a manifest names, so none of these may count as corrupt.
        let chunk = "a".repeat(64);
        let old = format!(
            r#"{{"format":"noevia-offsite-v1","files":[{{"path":"x","sha256":"s","chunks":["{chunk}",7]}},{{"chunks":["{chunk}"]}},"junk"]}}"#
        );
        let m = Manifest::parse(old.as_bytes()).unwrap();
        assert_eq!(m.time, 0, "the caller takes it from the id");
        assert!(!m.incomplete);
        assert_eq!(m.roots, 0);
        assert_eq!(m.files.len(), 3);
        assert_eq!(m.files[0].root, 0);
        assert_eq!(m.files[0].size, 0, "a restore of it refuses on the size");
        assert_eq!(m.files[0].chunks, std::slice::from_ref(&chunk));
        assert_eq!(m.files[1].path, "");
        assert!(m.files[2].chunks.is_empty());
        // A file naming a fractional or negative size is read as absent too, never fatal.
        let odd = br#"{"format":"noevia-offsite-v1","time":1.5,"files":[{"size":-3,"root":1.2,"chunks":[]}]}"#;
        assert_eq!(Manifest::parse(odd).unwrap().files[0].size, 0);
    }

    #[test]
    fn a_snapshot_id_carries_its_time() {
        for t in [0, 1_700_000_000_123, 1_793_000_000_999] {
            assert_eq!(snapshot_time(&snapshot_id(t, [1, 2, 3, 4])), Some(t));
        }
        for bad in [
            "",
            "nope",
            "2023-11-14T22-13-20-123",
            "2023-13-14T22-13-20-123Z-ab",
            "2023-11-14X22-13-20-123Z-ab",
        ] {
            assert_eq!(snapshot_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn retention_keeps_the_newest_complete_snapshot_and_prefers_it_within_a_day() {
        let day = 86_400_000i64;
        let t0 = 1_767_268_800_000i64; // 2026-01-01T12:00:00Z
                                       // Two runs on the same day: the later one is incomplete. The day keeps the good one.
        let same_day = vec![
            ("good".to_owned(), t0, true),
            ("bad".to_owned(), t0 + 3_600_000, false),
        ];
        let keep = retain_complete(&same_day, RETENTION);
        assert!(keep.contains("good") && keep.contains("bad"), "{keep:?}");
        let old_ids: Vec<(String, i64, bool)> = (0..12)
            .map(|i| (format!("d{i}"), t0 + i * day, i == 0))
            .collect();
        // Eleven incomplete nights after one complete one, far beyond the daily limit: the complete
        // snapshot is still kept, because it is the newest one a restore could use.
        let keep = retain_complete(
            &old_ids,
            Retention {
                daily: 3,
                weekly: 0,
                monthly: 0,
            },
        );
        assert!(
            keep.contains("d0"),
            "the newest complete snapshot survives: {keep:?}"
        );
        assert!(keep.contains("d11"), "and the newest overall");
        assert_eq!(keep.len(), 4);
        // With every snapshot complete this is exactly retain().
        let plain: Vec<(String, i64)> = (0..90).map(|i| (format!("s{i}"), t0 + i * day)).collect();
        let flagged: Vec<(String, i64, bool)> =
            plain.iter().map(|(i, t)| (i.clone(), *t, true)).collect();
        assert_eq!(
            retain(&plain, RETENTION),
            retain_complete(&flagged, RETENTION)
        );
        // Within a day the complete run wins even when it is the older of the two, and the worse
        // later run on that day is the one forgotten.
        let three = vec![
            ("good".to_owned(), t0, true),
            ("bad".to_owned(), t0 + 3_600_000, false),
            ("next".to_owned(), t0 + day, true),
        ];
        let keep = retain_complete(
            &three,
            Retention {
                daily: 2,
                weekly: 0,
                monthly: 0,
            },
        );
        let mut ids: Vec<&String> = keep.iter().collect();
        ids.sort();
        assert_eq!(ids, ["good", "next"]);
    }

    #[test]
    fn retention_is_the_node_retain() {
        let t0 = 1_767_268_800_000i64; // 2026-01-01T12:00:00Z, as Date.UTC(2026, 0, 1, 12)
        let snaps: Vec<(String, i64)> = (0..90)
            .map(|i| (format!("s{i}"), t0 + i * 86_400_000 + (i % 3) * 3_600_000))
            .collect();
        let keep = retain(&snaps, RETENTION);
        let mut ids: Vec<i64> = keep.iter().map(|s| s[1..].parse().unwrap()).collect();
        ids.sort_unstable();
        assert_eq!(ids, [30, 58, 73, 80, 83, 84, 85, 86, 87, 88, 89]);
        assert_eq!(retain(&[], RETENTION).len(), 0);
    }

    #[test]
    fn dates_are_js_dates() {
        assert_eq!(iso_dashed(1_700_000_000_123), "2023-11-14T22-13-20-123Z");
        assert_eq!(
            snapshot_id(0, [0xab, 0xcd, 0, 1]),
            "1970-01-01T00-00-00-000Z-abcd0001"
        );
        assert!(valid_snapshot_id(&snapshot_id(
            1_700_000_000_123,
            [1, 2, 3, 4]
        )));
        // ISO weeks across a year boundary: 2021-01-03 is week 53 of 2020 (a Sunday), 2021-01-04 week 1.
        let d = |y, m, dd| days_from_civil(y, m, dd) * 86_400_000;
        assert_eq!(bucket(d(2021, 1, 3), 1), "2020-W53");
        assert_eq!(bucket(d(2021, 1, 4), 1), "2021-W1");
        assert_eq!(bucket(d(2026, 12, 31), 2), "2026-12");
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn key_and_path_checks_match_the_store_and_the_restore() {
        for ok in [
            "config",
            "data/ab/abcdef",
            "snapshots/2023-11-14T22-13-20-123Z-abcd0001",
        ] {
            assert!(valid_dir_key(ok), "{ok}");
        }
        for bad in [
            "", "/x", "a//b", ".tmp-x", "a/../b", "a/.b", "a b", "../x", "a/",
        ] {
            assert!(!valid_dir_key(bad), "{bad}");
        }
        assert!(safe_relative_path("a/b.txt"));
        for bad in ["", "/a", "a//b", "a/./b", "a/../b", ".."] {
            assert!(!safe_relative_path(bad), "{bad}");
        }
        assert!(valid_chunk_id(HELLO_ID) && !valid_chunk_id("ABC"));
        assert_eq!(data_key(HELLO_ID), format!("data/36/{HELLO_ID}"));
    }
}
