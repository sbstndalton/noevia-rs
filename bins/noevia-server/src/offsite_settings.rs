//! The off-site backup's deployment settings and its small on-disk state (sbstndalton/noevia#2190
//! round 7), read the way noevia-core's `offsite-service.cjs` read `process.env`: the destination
//! (an S3 endpoint or a folder), the key file (`OFFSITE_BACKUP_KEY_FILE`, which is also the key of the
//! backup's Google slot), the backed-up paths, the hour, and the status file
//! (`offsite-backup-status.json`, mode 0600, written whole by rename). Nothing here prints a key.
use crate::gdrive::Fail;
use js_json::JValue;
use offsite_backup_format::schedule::Zone;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use zeroize::Zeroizing;

/// Core's ENV: an S3 destination needs all of these.
pub const ENV: [&str; 5] = [
    "OFFSITE_BACKUP_S3_ENDPOINT",
    "OFFSITE_BACKUP_S3_BUCKET",
    "OFFSITE_BACKUP_S3_ACCESS_KEY_ID",
    "OFFSITE_BACKUP_S3_SECRET_ACCESS_KEY",
    "OFFSITE_BACKUP_KEY_FILE",
];
/// Core's DIR_ENV: a folder destination (which Rust itself copies to Google Drive once connected).
pub const DIR_ENV: [&str; 2] = ["OFFSITE_BACKUP_DIR", "OFFSITE_BACKUP_KEY_FILE"];
pub const STATUS_FILE: &str = "offsite-backup-status.json";

#[derive(Clone)]
pub struct Settings {
    pub data_dir: PathBuf,
    /// NOEVIA_FEATURE_OFFSITE_BACKUP (overrides the admin setting while set).
    pub feature_env: Option<bool>,
    values: Vec<(&'static str, String)>,
    pub paths: Vec<String>,
    pub hour: u32,
    /// The `TZ` name the process runs with: the zone `OFFSITE_BACKUP_HOUR` is read in (core's
    /// `getHours()`). None is UTC, which is what a process without `TZ` runs in.
    pub tz: Option<String>,
}

/// The host's own zone when `OFFSITE_BACKUP_TIMEZONE` does not say: the Unraid host runs on Eastern
/// time while the web container has no `TZ`, so the appdata window is read there, never as UTC.
pub const DEFAULT_HOST_ZONE: Zone = Zone::NewYork;

impl std::fmt::Debug for Settings {
    /// Never prints a value: some are credentials.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("paths", &self.paths.len())
            .field("hour", &self.hour)
            .finish_non_exhaustive()
    }
}

const KEYS: [&str; 12] = [
    "OFFSITE_BACKUP_S3_ENDPOINT",
    "OFFSITE_BACKUP_S3_BUCKET",
    "OFFSITE_BACKUP_S3_ACCESS_KEY_ID",
    "OFFSITE_BACKUP_S3_SECRET_ACCESS_KEY",
    "OFFSITE_BACKUP_KEY_FILE",
    "OFFSITE_BACKUP_DIR",
    "OFFSITE_BACKUP_MIRROR",
    "OFFSITE_BACKUP_S3_REGION",
    "OFFSITE_BACKUP_S3_PREFIX",
    "OFFSITE_BACKUP_PATHS",
    "OFFSITE_BACKUP_HOUR",
    "OFFSITE_BACKUP_TIMEZONE",
];

/// JS `Number.parseInt(text, 10)` for the hour: leading whitespace, optional sign, digits.
fn parse_int(text: &str) -> Option<i64> {
    let t = text.trim_start();
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let n: i64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>, data_dir: PathBuf) -> Settings {
        let values: Vec<(&'static str, String)> = KEYS
            .iter()
            .filter_map(|k| get(k).map(|v| (*k, v)))
            .collect();
        let raw = |k: &str| {
            values
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.as_str())
        };
        // `String(env.OFFSITE_BACKUP_PATHS || dataDir).split(',')...filter(Boolean)`
        let paths_text = raw("OFFSITE_BACKUP_PATHS")
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| data_dir.display().to_string());
        let paths = paths_text
            .split(',')
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect();
        // `Math.min(23, Math.max(0, Number.parseInt(env.OFFSITE_BACKUP_HOUR ?? '3', 10) || 0))`
        let hour = u32::try_from(
            parse_int(raw("OFFSITE_BACKUP_HOUR").unwrap_or("3"))
                .unwrap_or(0)
                .clamp(0, 23),
        )
        .unwrap_or(0);
        let spec = server_features::spec(server_features::Feature::OffsiteBackup);
        Settings {
            data_dir,
            feature_env: server_features::parse_env(get(spec.env).as_deref()).unwrap_or(None),
            values,
            paths,
            hour,
            tz: get("TZ"),
        }
    }

    /// The zone `OFFSITE_BACKUP_HOUR` is read in, as core read it (`getHours()` on the process
    /// clock): `TZ`, UTC without one. The second value is a warning for an unknown `TZ` name, which
    /// is read as UTC exactly as Node did, but never silently.
    pub fn hour_zone(&self) -> (Zone, Option<String>) {
        let (zone, known) = Zone::from_tz(self.tz.as_deref());
        let warning = (!known).then(|| {
            format!(
                "TZ is not a zone the off-site backup schedule knows (it knows America/New_York and UTC); OFFSITE_BACKUP_HOUR is read as {} time",
                zone.name()
            )
        });
        (zone, warning)
    }

    /// The zone of the host's own clock, for the appdata backup's quiet window:
    /// `OFFSITE_BACKUP_TIMEZONE`, else [`DEFAULT_HOST_ZONE`]. A name the schedule does not know is
    /// warned about and replaced by the default, never by UTC.
    pub fn quiet_zone(&self) -> (Zone, Option<String>) {
        let Some(name) = self
            .raw("OFFSITE_BACKUP_TIMEZONE")
            .map(str::trim)
            .filter(|n| !n.is_empty())
        else {
            return (DEFAULT_HOST_ZONE, None);
        };
        match Zone::from_tz(Some(name)) {
            (zone, true) => (zone, None),
            _ => (
                DEFAULT_HOST_ZONE,
                Some(format!(
                    "OFFSITE_BACKUP_TIMEZONE is not a zone the off-site backup schedule knows (it knows America/New_York and UTC); the appdata quiet window is read as {} time",
                    DEFAULT_HOST_ZONE.name()
                )),
            ),
        }
    }

    fn raw(&self, key: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(n, _)| *n == key)
            .map(|(_, v)| v.as_str())
    }

    /// `String(env[key] || '').trim()`.
    pub fn text(&self, key: &str) -> String {
        self.raw(key).unwrap_or("").trim().to_owned()
    }

    /// `env.OFFSITE_BACKUP_DIR` as a folder destination, or none (`useDir()`).
    pub fn dir(&self) -> Option<String> {
        let d = self.text("OFFSITE_BACKUP_DIR");
        (!d.is_empty()).then_some(d)
    }

    pub fn use_dir(&self) -> bool {
        self.dir().is_some()
    }

    /// `env.OFFSITE_BACKUP_MIRROR || null`.
    pub fn mirror(&self) -> Option<String> {
        self.raw("OFFSITE_BACKUP_MIRROR")
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    }

    /// The variables the chosen destination still needs.
    pub fn missing(&self) -> Vec<&'static str> {
        let list: &[&'static str] = if self.use_dir() { &DIR_ENV } else { &ENV };
        list.iter()
            .copied()
            .filter(|k| self.text(k).is_empty())
            .collect()
    }

    pub fn s3_endpoint(&self) -> Option<&str> {
        self.raw("OFFSITE_BACKUP_S3_ENDPOINT")
            .filter(|v| !v.is_empty())
    }

    /// `loadKey(env.OFFSITE_BACKUP_KEY_FILE, paths)`: 32 bytes from a 64-hex-digit file that is not
    /// inside a backed-up path. Every failure is a fixed message; the key is never in one.
    pub fn load_key(&self) -> Result<Zeroizing<[u8; 32]>, KeyError> {
        let file = self
            .raw("OFFSITE_BACKUP_KEY_FILE")
            .filter(|v| !v.is_empty())
            .ok_or(KeyError::Unset)?;
        let real = std::fs::canonicalize(file).map_err(|_| KeyError::Unreadable)?;
        for root in &self.paths {
            let Ok(r) = std::fs::canonicalize(root) else {
                continue;
            };
            if real == r || real.starts_with(&r) {
                return Err(KeyError::Inside);
            }
        }
        // Bytes that are not text (32 raw key bytes, say) are a malformed key, not an unreadable file.
        let bytes = Zeroizing::new(std::fs::read(&real).map_err(|_| KeyError::Unreadable)?);
        let text = std::str::from_utf8(&bytes).map_err(|_| KeyError::Format)?;
        let hex = text.trim();
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(KeyError::Format);
        }
        let mut key = Zeroizing::new([0u8; 32]);
        for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
            let byte = std::str::from_utf8(pair)
                .ok()
                .and_then(|p| u8::from_str_radix(p, 16).ok())
                .ok_or(KeyError::Format)?;
            if let Some(slot) = key.get_mut(i) {
                *slot = byte;
            }
        }
        Ok(key)
    }

    /// `driveUsable()`: a backup folder and a readable key.
    pub fn drive_usable(&self) -> bool {
        self.use_dir() && self.load_key().is_ok()
    }

    /// `checkDestination(dir, paths)`: the folder must exist and not overlap what is backed up.
    pub fn check_destination(&self, dir: &str) -> Result<PathBuf, Fail> {
        let real = std::fs::canonicalize(dir)
            .map_err(|_| Fail::new(format!("The backup folder {dir} does not exist."), 409))?;
        for root in &self.paths {
            let Ok(r) = std::fs::canonicalize(root) else {
                continue;
            };
            if real == r || real.starts_with(&r) || r.starts_with(&real) {
                return Err(Fail::new(
                    "The backup folder must not overlap the folders being backed up.",
                    409,
                ));
            }
        }
        Ok(real)
    }
}

/// Why the key file could not be used (core `loadKey`'s fail() messages).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    Unset,
    Inside,
    Format,
    /// A missing or unreadable file: core threw the raw filesystem error, which has no public text.
    Unreadable,
}

impl KeyError {
    /// The message core showed (`publicMessage`), if it had one.
    pub fn public(self) -> Option<&'static str> {
        match self {
            KeyError::Unset => {
                Some("Set OFFSITE_BACKUP_KEY_FILE to a file holding a 64-character hex key.")
            }
            KeyError::Inside => Some("The backup key must be kept outside the backed-up folders."),
            KeyError::Format => {
                Some("The backup key file must hold 64 hex characters (openssl rand -hex 32).")
            }
            KeyError::Unreadable => None,
        }
    }

    pub fn fail(self) -> Fail {
        match self.public() {
            Some(m) => Fail::new(m, 400),
            // A missing or unreadable file is the key's problem, not the destination's (#2231).
            None => Fail::new(
                "The backup key file could not be read. Check OFFSITE_BACKUP_KEY_FILE.",
                400,
            ),
        }
    }
}

/// `offsite-backup-status.json`: read whole, patched by key, written whole by rename.
pub struct StatusFile {
    path: PathBuf,
    lock: Mutex<()>,
}

impl StatusFile {
    pub fn new(data_dir: &Path) -> StatusFile {
        StatusFile {
            path: data_dir.join(STATUS_FILE),
            lock: Mutex::new(()),
        }
    }

    /// `readStatus()`: `{}` when missing or unreadable.
    pub fn read(&self) -> JValue {
        match std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| js_json::parse(&t).ok())
        {
            Some(v @ JValue::Obj(_)) => v,
            _ => JValue::Obj(Vec::new()),
        }
    }

    /// `writeStatus(patch)`: `{...readStatus(), ...patch}`; returns the new document.
    pub fn patch(&self, patch: Vec<(&str, JValue)>) -> JValue {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let JValue::Obj(mut pairs) = self.read() else {
            return JValue::Null;
        };
        for (k, v) in patch {
            match pairs.iter_mut().find(|(n, _)| n == k) {
                Some(slot) => slot.1 = v,
                None => pairs.push((k.to_owned(), v)),
            }
        }
        let doc = JValue::Obj(pairs);
        if let Some(text) = js_json::stringify_pretty(&doc) {
            let _ = crate::gdrive::write_atomic(&self.path, &text);
        }
        doc
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, &str)], dir: &Path) -> Settings {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Settings::from_lookup(&|k: &str| map.get(k).cloned(), dir.to_path_buf())
    }

    #[test]
    fn hour_is_cores_parse() {
        let d = Path::new("/data");
        assert_eq!(settings(&[], d).hour, 3);
        assert_eq!(settings(&[("OFFSITE_BACKUP_HOUR", "22")], d).hour, 22);
        assert_eq!(settings(&[("OFFSITE_BACKUP_HOUR", "99")], d).hour, 23);
        assert_eq!(settings(&[("OFFSITE_BACKUP_HOUR", "-4")], d).hour, 0);
        assert_eq!(settings(&[("OFFSITE_BACKUP_HOUR", "abc")], d).hour, 0);
        // Set but empty: `'' ?? '3'` is '', parseInt('') is NaN, `|| 0` is 0.
        assert_eq!(settings(&[("OFFSITE_BACKUP_HOUR", "")], d).hour, 0);
        assert_eq!(settings(&[("OFFSITE_BACKUP_HOUR", "7pm")], d).hour, 7);
    }

    #[test]
    fn the_hour_and_the_windows_are_read_in_named_zones() {
        let d = Path::new("/data");
        // No TZ: the hour is read on UTC (as Node's getHours() did in the container), and the
        // appdata window on the host's Eastern time. Neither is a guess and neither warns.
        let plain = settings(&[], d);
        assert_eq!(plain.hour_zone(), (Zone::Utc, None));
        assert_eq!(plain.quiet_zone(), (Zone::NewYork, None));
        let utc_host = settings(&[("OFFSITE_BACKUP_TIMEZONE", "UTC")], d);
        assert_eq!(utc_host.quiet_zone(), (Zone::Utc, None));
        let ny_proc = settings(&[("TZ", "America/New_York")], d);
        assert_eq!(ny_proc.hour_zone(), (Zone::NewYork, None));
        // Unknown names warn, and a bad host zone falls to the default, not to UTC.
        let (z, w) = settings(&[("TZ", "Mars/Olympus")], d).hour_zone();
        assert_eq!(z, Zone::Utc);
        assert!(w.unwrap().contains("TZ is not a zone"));
        let (z, w) = settings(&[("OFFSITE_BACKUP_TIMEZONE", "Mars/Olympus")], d).quiet_zone();
        assert_eq!(z, Zone::NewYork);
        assert!(w.unwrap().contains("OFFSITE_BACKUP_TIMEZONE"));
        assert_eq!(
            settings(&[("OFFSITE_BACKUP_TIMEZONE", " ")], d).quiet_zone(),
            (Zone::NewYork, None)
        );
    }

    #[test]
    fn missing_names_the_destinations_gaps() {
        let d = Path::new("/data");
        assert_eq!(settings(&[], d).missing().len(), 5);
        let s = settings(&[("OFFSITE_BACKUP_DIR", "/backups")], d);
        assert!(s.use_dir());
        assert_eq!(s.missing(), ["OFFSITE_BACKUP_KEY_FILE"]);
        assert_eq!(settings(&[], d).paths, ["/data"]);
        assert_eq!(
            settings(&[("OFFSITE_BACKUP_PATHS", " /a, ,/b ")], d).paths,
            ["/a", "/b"]
        );
    }

    #[test]
    fn the_key_must_be_hex_and_outside_the_backed_up_folders() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let hex = "ab".repeat(32);
        std::fs::write(outside.join("key"), format!("{hex}\n")).unwrap();
        std::fs::write(data.join("key"), &hex).unwrap();
        std::fs::write(outside.join("short"), "abcd").unwrap();
        let with = |f: &Path| settings(&[("OFFSITE_BACKUP_KEY_FILE", f.to_str().unwrap())], &data);
        assert_eq!(with(&outside.join("key")).load_key().unwrap()[0], 0xab);
        assert_eq!(
            with(&data.join("key")).load_key().err(),
            Some(KeyError::Inside)
        );
        assert_eq!(
            with(&outside.join("short")).load_key().err(),
            Some(KeyError::Format)
        );
        assert_eq!(
            with(&outside.join("nope")).load_key().err(),
            Some(KeyError::Unreadable)
        );
        assert_eq!(settings(&[], &data).load_key().err(), Some(KeyError::Unset));
    }

    #[test]
    fn the_destination_may_not_overlap_the_backed_up_folders() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(data.join("inner")).unwrap();
        let other = root.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let s = settings(&[], &data);
        assert!(s.check_destination(other.to_str().unwrap()).is_ok());
        assert_eq!(
            s.check_destination(data.to_str().unwrap())
                .unwrap_err()
                .status,
            409
        );
        assert_eq!(
            s.check_destination(data.join("inner").to_str().unwrap())
                .unwrap_err()
                .status,
            409
        );
        assert_eq!(
            s.check_destination(root.path().to_str().unwrap())
                .unwrap_err()
                .status,
            409
        );
        assert!(s
            .check_destination("/definitely/not/here")
            .unwrap_err()
            .message
            .contains("does not exist"));
    }

    #[test]
    fn the_status_file_merges_by_key() {
        let d = tempfile::tempdir().unwrap();
        let f = StatusFile::new(d.path());
        assert!(matches!(f.read(), JValue::Obj(p) if p.is_empty()));
        f.patch(vec![("snapshots", JValue::Num(3.0))]);
        let doc = f.patch(vec![
            ("lastError", JValue::Null),
            ("snapshots", JValue::Num(4.0)),
        ]);
        assert_eq!(doc.get("snapshots").as_num(), Some(4.0));
        assert!(f.read().get("lastError").is_nullish());
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(d.path().join(STATUS_FILE))
                .unwrap()
                .permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
    }
}
