//! Snapshots of Node-live state that Node publishes for the front (sbstndalton/noevia#2190 close-out):
//! `rag-status.json` (`GET /api/health`) and `auto-roles-snapshot.json` (`GET /api/auto-roles`).
//!
//! The file is the only channel: Node writes it atomically (a temporary file, fsync, rename, mode
//! 0600; core `published-snapshot.cjs`) on a timer and whenever the state changes, and the front reads
//! it on every request. A reader never trusts what it cannot prove current. The file carries
//! `{ version, kind, writtenAt, everyMs, ... }`; it is **unusable** when it is missing, is not a
//! regular file, does not parse, is of another version or kind, has no `writtenAt` or `everyMs`, was
//! written more than two intervals ago ([`STALE_AFTER_INTERVALS`], so one missed tick is not an outage
//! but a stopped Node is) or is dated in the future. An unusable snapshot makes the caller answer
//! "unavailable", never an old value.

use serde_json::Value;
use std::io::Read;
use std::path::Path;

/// The only format this reader knows (core `published-snapshot.cjs` `VERSION`).
pub const VERSION: u64 = 1;
/// A snapshot older than this many publishing intervals is stale.
pub const STALE_AFTER_INTERVALS: i64 = 2;
/// The interval a snapshot may declare: 1 s to 10 minutes. Outside it the file is corrupt.
const EVERY_MS: std::ops::RangeInclusive<i64> = 1_000..=600_000;
/// A clock a little ahead of Node's is not a future file.
const SKEW_MS: i64 = 5_000;
/// A snapshot is a few KiB; anything bigger is not one.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    Missing,
    Corrupt,
    Stale,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// The parsed file (serde_json sorts keys: use [`Snapshot::raw`] where order is the answer's).
    pub body: Value,
    /// The checked file's bytes, for a reader that must keep Node's key order (js_json).
    pub raw: Vec<u8>,
    pub written_at: i64,
    pub every_ms: i64,
}

/// Reads and checks `file`, which must hold a snapshot of `kind`, at `now_ms`.
pub fn read(file: &Path, kind: &str, now_ms: i64) -> Result<Snapshot, Unusable> {
    let meta = match std::fs::symlink_metadata(file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Unusable::Missing),
        Err(_) => return Err(Unusable::Corrupt),
    };
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err(Unusable::Corrupt);
    }
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .and_then(|f| f.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|_| Unusable::Corrupt)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Unusable::Corrupt);
    }
    parse(&bytes, kind, now_ms)
}

fn parse(bytes: &[u8], kind: &str, now_ms: i64) -> Result<Snapshot, Unusable> {
    let body: Value = serde_json::from_slice(bytes).map_err(|_| Unusable::Corrupt)?;
    let Some(object) = body.as_object() else {
        return Err(Unusable::Corrupt);
    };
    if object.get("version").and_then(Value::as_u64) != Some(VERSION)
        || object.get("kind").and_then(Value::as_str) != Some(kind)
    {
        return Err(Unusable::Corrupt);
    }
    let int = |key: &str| object.get(key).and_then(Value::as_i64);
    let (Some(written_at), Some(every_ms)) = (int("writtenAt"), int("everyMs")) else {
        return Err(Unusable::Corrupt);
    };
    if written_at <= 0 || !EVERY_MS.contains(&every_ms) {
        return Err(Unusable::Corrupt);
    }
    let age = now_ms.saturating_sub(written_at);
    if age < -SKEW_MS || age > every_ms.saturating_mul(STALE_AFTER_INTERVALS) {
        return Err(Unusable::Stale);
    }
    Ok(Snapshot {
        body,
        raw: bytes.to_vec(),
        written_at,
        every_ms,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn file(dir: &Path, text: &str) -> std::path::PathBuf {
        let path = dir.join("snap.json");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn good(extra: &str) -> String {
        format!(r#"{{"version":1,"kind":"k","writtenAt":{NOW},"everyMs":10000{extra}}}"#)
    }

    #[test]
    fn a_fresh_snapshot_of_the_kind_is_read() {
        let d = tempfile::tempdir().unwrap();
        let s = read(&file(d.path(), &good(r#","x":1"#)), "k", NOW + 4_000).unwrap();
        assert_eq!(s.body["x"], 1);
        assert_eq!((s.written_at, s.every_ms), (NOW, 10_000));
        // Two intervals old is still current; a hair past is stale.
        assert!(read(&file(d.path(), &good("")), "k", NOW + 20_000).is_ok());
        assert_eq!(
            read(&file(d.path(), &good("")), "k", NOW + 20_001),
            Err(Unusable::Stale)
        );
    }

    #[test]
    fn nothing_unprovable_is_usable() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            read(&d.path().join("none.json"), "k", NOW),
            Err(Unusable::Missing)
        );
        for (name, text) in [
            ("empty", String::new()),
            ("garbage", "{not json".to_owned()),
            ("array", "[]".to_owned()),
            (
                "version",
                good("").replace(r#""version":1"#, r#""version":2"#),
            ),
            ("no version", good("").replace(r#""version":1,"#, "")),
            (
                "kind",
                good("").replace(r#""kind":"k""#, r#""kind":"other""#),
            ),
            ("no writtenAt", good("").replace("writtenAt", "wroteAt")),
            ("float writtenAt", good("").replace(&NOW.to_string(), "1.5")),
            ("zero writtenAt", good("").replace(&NOW.to_string(), "0")),
            ("no everyMs", good("").replace("everyMs", "every")),
            ("tiny everyMs", good("").replace("10000", "10")),
            ("huge everyMs", good("").replace("10000", "99999999999")),
            (
                "text writtenAt",
                good("").replace(&NOW.to_string(), "\"now\""),
            ),
        ] {
            assert_eq!(
                read(&file(d.path(), &text), "k", NOW + 1_000),
                Err(Unusable::Corrupt),
                "{name}"
            );
        }
    }

    #[test]
    fn a_future_file_and_a_symlink_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = file(d.path(), &good(""));
        assert_eq!(read(&p, "k", NOW - 5_001), Err(Unusable::Stale));
        assert!(read(&p, "k", NOW - 5_000).is_ok());
        let link = d.path().join("link.json");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert_eq!(read(&link, "k", NOW), Err(Unusable::Corrupt));
        let big = d.path().join("big.json");
        std::fs::write(&big, vec![b' '; (MAX_BYTES + 1) as usize]).unwrap();
        assert_eq!(read(&big, "k", NOW), Err(Unusable::Corrupt));
        assert_eq!(read(d.path(), "k", NOW), Err(Unusable::Corrupt));
    }
}
