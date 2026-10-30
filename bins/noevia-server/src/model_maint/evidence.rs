//! Configuration-scoped qualification evidence (core `evidence.cjs`, `evidence-limitations.cjs`,
//! `sampling-recommendation.cjs`, `sampling-presets.cjs`, spec-agent-execution section 1), the
//! store the front writes and the derivations over it.
//!
//! The store is `evidence-<hash>/evidence.jsonl` (one JSON record per line, append-only, compacted
//! when it passes its caps), in the format Core writes and reads: an old Core reads what this writes
//! and the reverse. The Rust front is its only writer: Node's producers (a chat's vision probe and
//! MTP acceptance, a benchmark run, auto-tune, the recheck route, a finished download) hand their
//! records over through the inbox directory (`evidence-inbox-<hash>/`, see `inbox.rs`), which the
//! front drains in order. Records are `JValue`s, so key order and number text survive a read and
//! write exactly as JavaScript's `JSON.parse` / `JSON.stringify` keep them.
//!
//! Derived states are computed on read against the live identity (artifact fingerprint, preset hash,
//! build, endpoint); see [`derive`].

use js_json::{parse, stringify, JValue};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const CATEGORIES: [&str; 4] = ["context_capacity", "vision", "mtp_acceptance", "throughput"];

pub fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `sorted(value)`: object keys in code-unit order, recursively.
pub fn sorted(v: &JValue) -> JValue {
    match v {
        JValue::Arr(items) => JValue::Arr(items.iter().map(sorted).collect()),
        JValue::Obj(items) => {
            let mut keys: Vec<&(String, JValue)> = items.iter().collect();
            keys.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            JValue::Obj(
                keys.into_iter()
                    .map(|(k, v)| (k.clone(), sorted(v)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// `identityHash(identity)`: sha256 of `JSON.stringify(sorted(identity))`.
pub fn identity_hash(v: &JValue) -> String {
    sha(stringify(&sorted(v)).unwrap_or_default().as_bytes())
}

const SAMPLE: u64 = 1024 * 1024;

/// `fileFingerprint(file)`: size, mtime in whole milliseconds and a hash of the first and last MiB.
pub fn file_fingerprint(file: &Path) -> Option<String> {
    let meta = std::fs::metadata(file).ok()?;
    if !meta.is_file() {
        return None;
    }
    let size = meta.len();
    let mut f = std::fs::File::open(file).ok()?;
    let mut read = |at: u64, len: u64| -> Option<Vec<u8>> {
        let mut buf = vec![0u8; usize::try_from(len).ok()?];
        f.seek(SeekFrom::Start(at)).ok()?;
        let mut filled = 0;
        while filled < buf.len() {
            let n = f.read(buf.get_mut(filled..)?).ok()?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        Some(buf)
    };
    let mut bytes = read(0, size.min(SAMPLE))?;
    if size > SAMPLE {
        bytes.extend(read(size.saturating_sub(SAMPLE), SAMPLE)?);
    }
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let hash: String = sha(&bytes).chars().take(32).collect();
    Some(format!("{size}:{mtime}:{hash}"))
}

/// `presetHash(options)`: the section's options without the file paths, as strings.
pub fn preset_hash(options: &[(String, String)]) -> String {
    let clean: Vec<(String, JValue)> = options
        .iter()
        .filter(|(k, v)| {
            !matches!(k.as_str(), "model" | "mmproj" | "spec-draft-model") && !v.is_empty()
        })
        .map(|(k, v)| (k.clone(), JValue::Str(v.clone())))
        .collect();
    identity_hash(&JValue::Obj(dedup(clean)))
}

/// A JS object keeps one slot per key, in the place the key first appeared.
fn dedup(pairs: Vec<(String, JValue)>) -> Vec<(String, JValue)> {
    let mut out: Vec<(String, JValue)> = Vec::new();
    for (k, v) in pairs {
        match out.iter_mut().find(|(ek, _)| *ek == k) {
            Some(slot) => slot.1 = v,
            None => out.push((k, v)),
        }
    }
    out
}

// ── The store ───────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub max_bytes: u64,
    pub keep_per_key: usize,
    pub max_keys: usize,
    pub max_records: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            max_bytes: 1024 * 1024,
            keep_per_key: 50,
            max_keys: 200,
            max_records: 2000,
        }
    }
}

/// Why a record was not stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// `Invalid evidence record`.
    Invalid,
    /// `Evidence must not contain credentials`.
    Credentials,
    /// A task record: the front stores none (no producer exists).
    Task,
    Io,
}

/// A credential-shaped string: core's `/Bearer\s+[A-Za-z0-9._~+/=-]{8,}|\bhf_[A-Za-z0-9]{20,}|
/// \bsk-[A-Za-z0-9_-]{16,}|"(?:api_?key|password|secret|access_?token|authorization)"\s*:/i`.
pub fn credential_shaped(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let b = lower.as_bytes();
    let word_before = |i: usize| {
        i > 0
            && b.get(i - 1)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
    };
    let run = |from: usize, ok: &dyn Fn(u8) -> bool| {
        b.get(from..)
            .map_or(0, |s| s.iter().take_while(|c| ok(**c)).count())
    };
    let mut i = 0;
    while i < b.len() {
        let rest = b.get(i..).unwrap_or(&[]);
        if rest.starts_with(b"bearer") {
            let ws = run(i + 6, &|c| c.is_ascii_whitespace() || c == 0x0b);
            if ws > 0
                && run(i + 6 + ws, &|c| {
                    c.is_ascii_alphanumeric() || b"._~+/=-".contains(&c)
                }) >= 8
            {
                return true;
            }
        }
        if rest.starts_with(b"hf_")
            && !word_before(i)
            && run(i + 3, &|c| c.is_ascii_alphanumeric()) >= 20
        {
            return true;
        }
        if rest.starts_with(b"sk-")
            && !word_before(i)
            && run(i + 3, &|c| {
                c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
            }) >= 16
        {
            return true;
        }
        if rest.first() == Some(&b'"') {
            for key in [
                "api_key",
                "apikey",
                "password",
                "secret",
                "access_token",
                "accesstoken",
                "authorization",
            ] {
                if rest.get(1..).is_some_and(|r| r.starts_with(key.as_bytes())) {
                    let after = i + 1 + key.len();
                    if b.get(after) == Some(&b'"') {
                        let ws = run(after + 1, &|c| c.is_ascii_whitespace() || c == 0x0b);
                        if b.get(after + 1 + ws) == Some(&b':') {
                            return true;
                        }
                    }
                }
            }
        }
        i += 1;
    }
    false
}

pub struct Store {
    dir: PathBuf,
    caps: Caps,
    /// The only writer: appends and compactions of this process do not interleave.
    write: Mutex<Option<usize>>,
    /// Held across "read the newest, then append": a drain and an HTTP import cannot both decide to add.
    decide: Mutex<()>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("dir", &self.dir).finish()
    }
}

fn get_str<'a>(r: &'a JValue, k: &str) -> Option<&'a str> {
    r.get(k).as_str()
}

fn same_key(a: &JValue, model: &str, category: &str, hash: &str) -> bool {
    get_str(a, "model") == Some(model)
        && get_str(a, "category") == Some(category)
        && get_str(a, "identityHash") == Some(hash)
}

/// `JSON.stringify(a ?? null) === JSON.stringify(b ?? null)`, but key order does not matter.
fn same_value(a: &JValue, b: &JValue) -> bool {
    fn norm(v: &JValue) -> JValue {
        sorted(&match v {
            JValue::Undefined => JValue::Null,
            other => other.clone(),
        })
    }
    stringify(&norm(a)) == stringify(&norm(b))
}

impl Store {
    pub fn new(dir: PathBuf) -> Store {
        Store {
            dir,
            caps: Caps::default(),
            write: Mutex::new(None),
            decide: Mutex::new(()),
        }
    }

    pub fn with_caps(dir: PathBuf, caps: Caps) -> Store {
        Store {
            dir,
            caps,
            write: Mutex::new(None),
            decide: Mutex::new(()),
        }
    }

    pub fn file(&self) -> PathBuf {
        self.dir.join("evidence.jsonl")
    }

    /// Every readable record, oldest first; a line that does not parse is skipped.
    pub fn list(&self) -> Vec<JValue> {
        let Ok(text) = std::fs::read_to_string(self.file()) else {
            return Vec::new();
        };
        text.split('\n')
            .filter(|l| !l.is_empty())
            .filter_map(|l| parse(l).ok())
            .collect()
    }

    /// `append(record)`: the entry as stored. `id`, `at` and an empty `limitations` come first.
    pub fn append(&self, record: &JValue, at_ms: i64) -> Result<JValue, StoreError> {
        let JValue::Obj(rest) = record else {
            return Err(StoreError::Invalid);
        };
        if rest.iter().any(|(k, _)| k == "taskId") {
            return Err(StoreError::Task);
        }
        let mut id = [0u8; 16];
        {
            use rand_core::RngCore;
            rand_core::OsRng
                .try_fill_bytes(&mut id)
                .map_err(|_| StoreError::Io)?;
        }
        // `ev_` + a version-4 UUID, as `crypto.randomUUID()` makes one.
        id[6] = (id[6] & 0x0f) | 0x40;
        id[8] = (id[8] & 0x3f) | 0x80;
        let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
        let uuid = format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        );
        let mut entry: Vec<(String, JValue)> = vec![
            ("id".into(), JValue::Str(format!("ev_{uuid}"))),
            ("at".into(), JValue::Num(at_ms as f64)),
            ("limitations".into(), JValue::Arr(Vec::new())),
        ];
        for (k, v) in rest {
            if k == "revision" {
                continue;
            }
            match entry.iter_mut().find(|(ek, _)| ek == k) {
                Some(slot) => slot.1 = v.clone(),
                None => entry.push((k.clone(), v.clone())),
            }
        }
        let entry = JValue::Obj(entry);
        let valid = get_str(&entry, "category").is_some_and(|c| !c.is_empty())
            && get_str(&entry, "model").is_some_and(|m| !m.is_empty())
            && matches!(
                get_str(&entry, "result"),
                Some("passed" | "failed" | "reported")
            );
        if !valid {
            return Err(StoreError::Invalid);
        }
        let line = stringify(&entry).ok_or(StoreError::Invalid)?;
        if credential_shaped(&line) {
            return Err(StoreError::Credentials);
        }
        let mut lines = self.write.lock().map_err(|_| StoreError::Io)?;
        create_private_dir(&self.dir).map_err(|_| StoreError::Io)?;
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(self.file())
                .map_err(|_| StoreError::Io)?;
            f.write_all(format!("{line}\n").as_bytes())
                .map_err(|_| StoreError::Io)?;
        }
        // Counted once per process and kept up to date by append and compact.
        let count = match *lines {
            Some(n) => n + 1,
            None => self.list().len(),
        };
        *lines = Some(count);
        let size = std::fs::metadata(self.file()).map(|m| m.len()).unwrap_or(0);
        if count > self.caps.max_records + self.caps.max_records.div_ceil(4)
            || size > self.caps.max_bytes
        {
            // Best-effort: a failed compaction never fails the append.
            if let Ok(n) = self.compact() {
                *lines = Some(n);
            }
        }
        Ok(entry)
    }

    /// Skips a record when the newest one for the same model, category and identity says the same.
    pub fn append_if_changed(&self, record: &JValue, at_ms: i64) -> Result<JValue, StoreError> {
        let _deciding = self.decide.lock().map_err(|_| StoreError::Io)?;
        let (model, category, hash) = keys_of(record);
        let newest = self
            .list()
            .into_iter()
            .rev()
            .find(|r| same_key(r, &model, &category, &hash));
        if let Some(n) = newest {
            if get_str(&n, "result") == get_str(record, "result")
                && same_value(n.get("value"), record.get("value"))
            {
                return Ok(n);
            }
        }
        self.append(record, at_ms)
    }

    /// Reported rates drift a little on every reply; one record per meaningful change or per day.
    pub fn append_reported_rate(&self, record: &JValue, at_ms: i64) -> Result<JValue, StoreError> {
        let _deciding = self.decide.lock().map_err(|_| StoreError::Io)?;
        let (model, category, hash) = keys_of(record);
        let newest = self
            .list()
            .into_iter()
            .rev()
            .find(|r| same_key(r, &model, &category, &hash));
        let rate = record.get("value").get("rate").as_num();
        if let (Some(n), Some(rate)) = (newest.as_ref(), rate) {
            let prev = n.get("value").get("rate").as_num();
            let at = n.get("at").as_num().unwrap_or(f64::NEG_INFINITY);
            if prev.is_some_and(|p| (rate - p).abs() < 0.05 && (at_ms as f64) - at < 86_400_000.0) {
                return Ok(n.clone());
            }
        }
        self.append(record, at_ms)
    }

    /// Rewrites the log keeping the newest `keep_per_key` records for each model/category/identity,
    /// then at most `max_keys` identities, `max_records` records and three quarters of `max_bytes`
    /// (#796); the newest identity is never dropped. Returns the records kept.
    fn compact(&self) -> Result<usize, ()> {
        let records = self.list();
        let key_of = |r: &JValue| {
            format!(
                "{}|{}|{}",
                get_str(r, "model").unwrap_or("undefined"),
                get_str(r, "category").unwrap_or("undefined"),
                get_str(r, "identityHash").unwrap_or("undefined")
            )
        };
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut keep = vec![false; records.len()];
        for (i, r) in records.iter().enumerate().rev() {
            let n = counts.entry(key_of(r)).or_insert(0);
            if *n < self.caps.keep_per_key {
                if let Some(k) = keep.get_mut(i) {
                    *k = true;
                }
                *n += 1;
            }
        }
        // key -> (last index, count, bytes), in order of each key's newest record
        let mut groups: Vec<(String, usize, usize, usize)> = Vec::new();
        for (i, r) in records.iter().enumerate() {
            if !keep.get(i).copied().unwrap_or(false) {
                continue;
            }
            let key = key_of(r);
            let bytes = stringify(r).map_or(0, |s| s.len()) + 1;
            match groups.iter_mut().find(|g| g.0 == key) {
                Some(g) => {
                    g.1 = i;
                    g.2 += 1;
                    g.3 += bytes;
                }
                None => groups.push((key, i, 1, bytes)),
            }
        }
        let mut order: Vec<usize> = (0..groups.len()).collect();
        order.sort_by_key(|g| groups.get(*g).map_or(0, |g| g.1));
        let (mut total, mut bytes) = groups
            .iter()
            .fold((0usize, 0usize), |a, g| (a.0 + g.2, a.1 + g.3));
        let target = (self.caps.max_bytes as f64 * 0.75).floor() as usize;
        let mut dropped: Vec<&str> = Vec::new();
        for g in order {
            let Some(g) = groups.get(g) else { continue };
            if groups.len() - dropped.len() <= 1 {
                break;
            }
            if groups.len() - dropped.len() <= self.caps.max_keys
                && total <= self.caps.max_records
                && bytes <= target
            {
                break;
            }
            dropped.push(g.0.as_str());
            total -= g.2;
            bytes -= g.3;
        }
        let mut out = String::new();
        let mut kept = 0;
        for (i, r) in records.iter().enumerate() {
            if keep.get(i).copied().unwrap_or(false) && !dropped.contains(&key_of(r).as_str()) {
                if let Some(line) = stringify(r) {
                    out.push_str(&line);
                    out.push('\n');
                    kept += 1;
                }
            }
        }
        let tmp = self
            .dir
            .join(format!("evidence.jsonl.{}.tmp", std::process::id()));
        let write = || -> std::io::Result<()> {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(out.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, self.file())
        };
        write().map_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(kept)
    }
}

fn keys_of(record: &JValue) -> (String, String, String) {
    let s = |k: &str| get_str(record, k).unwrap_or("undefined").to_owned();
    (s("model"), s("category"), s("identityHash"))
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

// ── Derived states ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Derived {
    pub state: &'static str,
    pub record: Option<JValue>,
}

/// `derive(records, { model, category, liveHash })`.
pub fn derive(records: &[JValue], model: &str, category: &str, live_hash: Option<&str>) -> Derived {
    let mine: Vec<&JValue> = records
        .iter()
        .filter(|r| get_str(r, "model") == Some(model) && get_str(r, "category") == Some(category))
        .collect();
    let Some(live) = live_hash else {
        return Derived {
            state: "unavailable",
            record: mine.last().map(|r| (*r).clone()),
        };
    };
    let newest = mine
        .iter()
        .rev()
        .find(|r| get_str(r, "identityHash") == Some(live));
    if let Some(n) = newest {
        let state = match get_str(n, "result") {
            Some("passed") => Some("verified"),
            Some("failed") => Some("failed"),
            Some("reported") => Some("reported"),
            _ => None,
        };
        if let Some(state) = state {
            return Derived {
                state,
                record: Some((**n).clone()),
            };
        }
    }
    if let Some(older) = mine
        .iter()
        .rev()
        .find(|r| get_str(r, "result") != Some("reported"))
    {
        return Derived {
            state: "stale",
            record: Some((**older).clone()),
        };
    }
    Derived {
        state: "unverified",
        record: None,
    }
}

/// `artifactIdentityHash(artifact)`: external evidence is scoped to the artifact alone.
pub fn artifact_identity_hash(artifact: &str) -> String {
    identity_hash(&JValue::obj([("artifact", JValue::Str(artifact.into()))]))
}

/// `deriveExternal`.
pub fn derive_external(records: &[JValue], model: &str, artifact_hash: Option<&str>) -> Derived {
    let mine: Vec<&JValue> = records
        .iter()
        .filter(|r| {
            get_str(r, "model") == Some(model)
                && get_str(r, "category") == Some("external_model_card")
        })
        .collect();
    let Some(hash) = artifact_hash else {
        return Derived {
            state: "unavailable",
            record: mine.last().map(|r| (*r).clone()),
        };
    };
    if let Some(n) = mine
        .iter()
        .rev()
        .find(|r| get_str(r, "identityHash") == Some(hash))
    {
        return Derived {
            state: "reported",
            record: Some((**n).clone()),
        };
    }
    match mine.last() {
        Some(o) => Derived {
            state: "stale",
            record: Some((**o).clone()),
        },
        None => Derived {
            state: "unverified",
            record: None,
        },
    }
}

/// `deriveSampling`.
pub fn derive_sampling(records: &[JValue], model: &str, artifact_hash: Option<&str>) -> Derived {
    let mine: Vec<&JValue> = records
        .iter()
        .filter(|r| {
            get_str(r, "model") == Some(model)
                && get_str(r, "category") == Some("external_sampling_config")
        })
        .collect();
    let Some(hash) = artifact_hash else {
        return Derived {
            state: "unavailable",
            record: mine.last().map(|r| (*r).clone()),
        };
    };
    if let Some(n) = mine
        .iter()
        .rev()
        .find(|r| get_str(r, "identityHash") == Some(hash))
    {
        let state = if matches!(n.get("value"), JValue::Null) {
            "unverified"
        } else {
            "reported"
        };
        return Derived {
            state,
            record: Some((**n).clone()),
        };
    }
    match mine.last() {
        Some(o) => Derived {
            state: "stale",
            record: Some((**o).clone()),
        },
        None => Derived {
            state: "unverified",
            record: None,
        },
    }
}

// ── Limitation ids (evidence-limitations.cjs) ───────────────────────────────────────────────

fn num_text(s: &str) -> bool {
    let (int, frac) = s.split_once('.').map_or((s, None), |(a, b)| (a, Some(b)));
    !int.is_empty()
        && int.bytes().all(|c| c.is_ascii_digit())
        && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|c| c.is_ascii_digit()))
}

/// `{ id, params }` for a limitation sentence, or `None` (JSON null) when it has no id.
pub fn limitation_key(text: &str) -> Option<JValue> {
    let fixed = [
        (
            "autotune-quality",
            "Three deterministic quality smoke probes, not a general quality benchmark",
        ),
        (
            "autotune-budget",
            "120 s default prompt budget; existing MTP head only",
        ),
        (
            "vision-probe",
            "1\u{d7}1 image accepted; not an accuracy test",
        ),
        ("single-reply", "single reply; depends on content"),
        (
            "source-unverified",
            "unverified, from source: published by the model repository, not measured locally",
        ),
    ];
    let id_only = |id: &str| {
        JValue::obj([
            ("id", JValue::Str(id.into())),
            ("params", JValue::Obj(Vec::new())),
        ])
    };
    // The patterns are tried in the order of the table; only one can match a given sentence.
    if let Some(rest) =
        text.strip_prefix("Probes not used (failed at the model's reference settings): ")
    {
        if !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ',' || c == ' ' || c == '-')
        {
            return Some(JValue::obj([
                ("id", "autotune-baseline-skipped".into()),
                (
                    "params",
                    JValue::obj([("probes", JValue::Str(rest.into()))]),
                ),
            ]));
        }
    }
    for (id, sentence) in fixed {
        if text == sentence {
            return Some(id_only(id));
        }
    }
    if let Some(n) = text
        .strip_prefix("prompt budget ")
        .and_then(|r| r.strip_suffix(" s"))
    {
        if num_text(n) {
            return Some(JValue::obj([
                ("id", "calibration-budget".into()),
                (
                    "params",
                    JValue::obj([("seconds", JValue::Num(n.parse().unwrap_or(0.0)))]),
                ),
            ]));
        }
    }
    if let Some(n) = text.strip_prefix("median of ").and_then(|r| {
        r.strip_suffix(" warm, uncontended requests; depends on prompt mix and max tokens")
    }) {
        if !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()) {
            return Some(JValue::obj([
                ("id", "benchmark-median".into()),
                (
                    "params",
                    JValue::obj([("n", JValue::Num(n.parse().unwrap_or(0.0)))]),
                ),
            ]));
        }
    }
    None
}

/// `limitationKeys(limitations)`: one entry per limitation, `null` for a non-string or no id.
pub fn limitation_keys(limitations: &JValue) -> JValue {
    match limitations {
        JValue::Arr(items) => JValue::Arr(
            items
                .iter()
                .map(|t| t.as_str().and_then(limitation_key).unwrap_or(JValue::Null))
                .collect(),
        ),
        _ => JValue::Arr(Vec::new()),
    }
}

// ── Sampling recommendation (sampling-recommendation.cjs, sampling-presets.cjs) ──────────────

const KEYS: [&str; 5] = ["temperature", "top_p", "top_k", "min_p", "repeat_penalty"];

fn valid_sampling(key: &str, v: &JValue) -> bool {
    let Some(n) = v.as_num().filter(|n| n.is_finite()) else {
        return false;
    };
    match key {
        "temperature" => (0.0..=2.0).contains(&n),
        "top_p" => n > 0.0 && n <= 1.0,
        "top_k" => n.fract() == 0.0 && n >= 0.0,
        "repeat_penalty" => (0.0..=3.0).contains(&n),
        "min_p" => (0.0..=1.0).contains(&n),
        _ => false,
    }
}

/// `clean(values)`: only the recognised, valid keys, in the table's order.
fn clean(values: &JValue) -> Vec<(String, JValue)> {
    KEYS.iter()
        .filter(|k| valid_sampling(k, values.get(k)))
        .map(|k| ((*k).to_owned(), values.get(k).clone()))
        .collect()
}

#[derive(Clone, Copy)]
enum Tok {
    Lit(char),
    /// `[-_. ]?`
    Sep,
    /// `(?:^|[^a-z0-9])`
    StartOrOther,
    /// `(?:\d|[^a-z0-9]|$)`
    DigitOrOtherOrEnd,
}

fn other(c: char) -> bool {
    !(c.is_ascii_alphanumeric())
}

fn match_at(s: &[char], i: usize, pat: &[Tok]) -> bool {
    let Some((tok, rest)) = pat.split_first() else {
        return true;
    };
    match *tok {
        Tok::Lit(c) => {
            s.get(i).is_some_and(|x| x.to_ascii_lowercase() == c) && match_at(s, i + 1, rest)
        }
        Tok::Sep => {
            (s.get(i).is_some_and(|x| matches!(x, '-' | '_' | '.' | ' '))
                && match_at(s, i + 1, rest))
                || match_at(s, i, rest)
        }
        Tok::StartOrOther => {
            (i == 0 && match_at(s, i, rest))
                || (s.get(i).is_some_and(|c| other(*c)) && match_at(s, i + 1, rest))
        }
        Tok::DigitOrOtherOrEnd => {
            if i >= s.len() {
                return match_at(s, i, rest);
            }
            s.get(i).is_some_and(|c| c.is_ascii_digit() || other(*c)) && match_at(s, i + 1, rest)
        }
    }
}

fn find(model: &str, pat: &[Tok]) -> bool {
    let s: Vec<char> = model.chars().collect();
    (0..=s.len()).any(|i| match_at(&s, i, pat))
}

fn lits(word: &str) -> Vec<Tok> {
    word.chars().map(Tok::Lit).collect()
}

fn join(parts: &[&[Tok]]) -> Vec<Tok> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

/// One model family's published sampling values (FAMILIES, in the table's order).
pub struct Family {
    pub id: &'static str,
    pub label: &'static str,
    values: &'static [(&'static str, f64)],
    pub quirks: &'static [(&'static str, Quirk)],
    pub note: Option<(&'static str, &'static str)>,
}

#[derive(Clone, Copy)]
pub enum Quirk {
    Bool(bool),
    Str(&'static str),
    Num(f64),
}

const FAMILIES: &[Family] = &[
    Family {
        id: "gpt-oss",
        label: "GPT-OSS",
        values: &[("temperature", 1.0), ("top_p", 1.0), ("top_k", 0.0)],
        quirks: &[
            ("harmony", Quirk::Bool(true)),
            ("reasoningEffort", Quirk::Str("low")),
            ("qualityBudget", Quirk::Num(512.0)),
        ],
        note: None,
    },
    Family {
        id: "deepseek-r1",
        label: "DeepSeek R1 distill",
        values: &[("temperature", 0.6), ("top_p", 0.95)],
        quirks: &[],
        note: None,
    },
    Family {
        id: "qwen3.5",
        label: "Qwen3.5",
        values: &[
            ("temperature", 1.0),
            ("top_p", 0.95),
            ("top_k", 20.0),
            ("min_p", 0.0),
        ],
        quirks: &[],
        note: Some(("thinking-mode values", "thinking-mode")),
    },
    Family {
        id: "qwen3",
        label: "Qwen3",
        values: &[
            ("temperature", 0.6),
            ("top_p", 0.95),
            ("top_k", 20.0),
            ("min_p", 0.0),
        ],
        quirks: &[],
        note: Some(("thinking-mode values", "thinking-mode")),
    },
    Family {
        id: "llama-3",
        label: "Llama 3",
        values: &[("temperature", 0.6), ("top_p", 0.9)],
        quirks: &[],
        note: None,
    },
    Family {
        id: "gemma-4",
        label: "Gemma 4",
        values: &[("temperature", 1.0), ("top_p", 0.95), ("top_k", 64.0)],
        quirks: &[],
        note: None,
    },
    Family {
        id: "gemma-3",
        label: "Gemma 3",
        values: &[
            ("temperature", 1.0),
            ("top_p", 0.95),
            ("top_k", 64.0),
            ("min_p", 0.0),
        ],
        quirks: &[],
        note: None,
    },
    Family {
        id: "devstral",
        label: "Devstral / Mistral",
        values: &[("temperature", 0.2)],
        quirks: &[],
        note: None,
    },
    Family {
        id: "phi-4",
        label: "Phi-4",
        values: &[("temperature", 0.8), ("top_p", 0.95), ("top_k", 50.0)],
        quirks: &[],
        note: None,
    },
];

fn family_matches(id: &str, model: &str) -> bool {
    let sep = [Tok::Sep];
    match id {
        "gpt-oss" => find(
            model,
            &join(&[
                &[Tok::StartOrOther],
                &lits("gpt"),
                &sep,
                &lits("oss"),
                &[Tok::DigitOrOtherOrEnd],
            ]),
        ),
        "deepseek-r1" => find(model, &join(&[&lits("deepseek"), &sep, &lits("r1")])),
        "qwen3.5" => find(
            model,
            &join(&[&lits("qwen"), &sep, &lits("3"), &sep, &lits("5")]),
        ),
        "qwen3" => find(model, &join(&[&lits("qwen"), &sep, &lits("3")])),
        "llama-3" => find(model, &join(&[&lits("llama"), &sep, &lits("3")])),
        "gemma-4" => find(model, &join(&[&lits("gemma"), &sep, &lits("4")])),
        "gemma-3" => find(model, &join(&[&lits("gemma"), &sep, &lits("3")])),
        "devstral" => find(model, &lits("devstral")) || find(model, &lits("mistral")),
        "phi-4" => find(model, &join(&[&lits("phi"), &sep, &lits("4")])),
        _ => false,
    }
}

pub fn family_of(model: &str) -> Option<&'static Family> {
    FAMILIES.iter().find(|f| family_matches(f.id, model))
}

fn preset_values(id: &str) -> Vec<(String, JValue)> {
    let n = |k: &str, v: f64| (k.to_owned(), JValue::Num(v));
    match id {
        "coding" => vec![
            n("temperature", 0.2),
            n("top_p", 0.9),
            n("repeat_penalty", 1.05),
        ],
        "creative" => vec![n("temperature", 0.9), n("top_p", 0.95)],
        "reasoning" => vec![n("temperature", 0.5)],
        _ => Vec::new(),
    }
}

/// `resolveSamplingRecommendation({ model, sourceValues, sourceProvenance })` with the default
/// (`general`) task preset: the object the evidence answer calls `samplingPlan`, in Node's key order.
pub fn sampling_plan(model: &str, source_values: &JValue, source_provenance: &JValue) -> JValue {
    let family = family_of(model);
    let quirks = JValue::Obj(
        family
            .map(|f| {
                f.quirks
                    .iter()
                    .map(|(k, q)| {
                        (
                            (*k).to_owned(),
                            match q {
                                Quirk::Bool(b) => JValue::Bool(*b),
                                Quirk::Str(s) => JValue::Str((*s).into()),
                                Quirk::Num(n) => JValue::Num(*n),
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
    );
    let card = clean(source_values);
    let family_values: Vec<(String, JValue)> = family
        .map(|f| {
            clean(&JValue::Obj(
                f.values
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), JValue::Num(*v)))
                    .collect(),
            ))
        })
        .unwrap_or_default();
    // tier, source, sourceId, noteId, values, note, provenance
    let (tier, source, source_id, note_id, values, note, provenance) = if !card.is_empty() {
        (
            "model-card".to_owned(),
            "generation_config.json".to_owned(),
            "generation-config",
            JValue::Null,
            card,
            JValue::Null,
            source_provenance.clone(),
        )
    } else if family.is_some() && !family_values.is_empty() {
        let f = family;
        (
            "family".to_owned(),
            format!("{} family table", f.map_or("", |f| f.label)),
            "family-table",
            f.and_then(|f| f.note)
                .map_or(JValue::Null, |(_, id)| JValue::Str(id.into())),
            family_values,
            f.and_then(|f| f.note)
                .map_or(JValue::Null, |(n, _)| JValue::Str(n.into())),
            JValue::Null,
        )
    } else {
        (
            "preset".to_owned(),
            "Task preset: general".to_owned(),
            "task-preset",
            JValue::Null,
            clean(&JValue::Obj(preset_values("general"))),
            JValue::Null,
            JValue::Null,
        )
    };
    let presets_id = (tier == "preset").then(|| JValue::Str("general".into()));
    let mut out: Vec<(String, JValue)> = vec![
        (
            "familyId".into(),
            family.map_or(JValue::Null, |f| JValue::Str(f.id.into())),
        ),
        (
            "familyLabel".into(),
            family.map_or(JValue::Null, |f| JValue::Str(f.label.into())),
        ),
        ("quirks".into(), quirks),
        (
            "versions".into(),
            JValue::obj([
                ("familyTable", JValue::Num(1.0)),
                ("presets", JValue::Num(1.0)),
            ]),
        ),
        ("tier".into(), JValue::Str(tier)),
        ("source".into(), JValue::Str(source)),
        ("sourceId".into(), JValue::Str(source_id.into())),
    ];
    if let Some(p) = presets_id {
        out.push(("presetId".into(), p));
    }
    out.push(("values".into(), JValue::Obj(values)));
    out.push(("note".into(), note));
    out.push(("noteId".into(), note_id));
    out.push(("provenance".into(), provenance));
    JValue::Obj(out)
}

// ── Tests ───────────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writers_of_the_same_result_store_it_once() {
        let d = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(Store::new(d.path().join("evidence")));
        for round in 0..20 {
            let record = rec("m", "vision", &format!("h{round}"), "passed", JValue::Null);
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    let (store, record) = (std::sync::Arc::clone(&store), record.clone());
                    std::thread::spawn(move || store.append_if_changed(&record, 1000).unwrap())
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
        }
        assert_eq!(store.list().len(), 20);
    }

    fn rec(model: &str, category: &str, hash: &str, result: &str, value: JValue) -> JValue {
        JValue::obj([
            ("model", model.into()),
            ("identityHash", hash.into()),
            ("category", category.into()),
            ("result", result.into()),
            ("value", value),
        ])
    }

    fn num(n: f64) -> JValue {
        JValue::Num(n)
    }

    #[test]
    fn the_identity_hash_is_the_sha_of_the_sorted_json() {
        // sha256('{"a":1,"b":{"c":2,"d":3}}') however the keys were ordered.
        let v = JValue::obj([
            ("b", JValue::obj([("d", num(3.0)), ("c", num(2.0))])),
            ("a", num(1.0)),
        ]);
        assert_eq!(identity_hash(&v), sha(br#"{"a":1,"b":{"c":2,"d":3}}"#));
        // A string identity (the endpoint) hashes its JSON quoting.
        assert_eq!(
            identity_hash(&JValue::Str("http://e:1".into())),
            sha(br#""http://e:1""#)
        );
    }

    #[test]
    fn a_preset_hash_ignores_file_paths_and_empty_values() {
        let a = preset_hash(&[
            ("ctx-size".into(), "8192".into()),
            ("model".into(), "/models/a.gguf".into()),
            ("parallel".into(), String::new()),
        ]);
        let b = preset_hash(&[("ctx-size".into(), "8192".into())]);
        assert_eq!(a, b);
        assert_ne!(a, preset_hash(&[("ctx-size".into(), "4096".into())]));
        assert_eq!(b, sha(br#"{"ctx-size":"8192"}"#));
    }

    #[test]
    fn a_fingerprint_is_size_mtime_and_a_hash_of_the_ends() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("m.gguf");
        std::fs::write(&f, b"abc").unwrap();
        let fp = file_fingerprint(&f).unwrap();
        let parts: Vec<&str> = fp.split(':').collect();
        assert_eq!((parts[0], parts[2]), ("3", &sha(b"abc")[..32]));
        assert!(parts[1].parse::<u64>().unwrap() > 1_000_000_000_000);
        assert!(file_fingerprint(d.path()).is_none(), "a directory has none");
        assert!(file_fingerprint(&d.path().join("none")).is_none());
        // Past 2 MiB: the first and the last MiB only.
        let big = d.path().join("big");
        let mut bytes = vec![1u8; 3 * 1024 * 1024];
        std::fs::write(&big, &bytes).unwrap();
        let before = file_fingerprint(&big).unwrap();
        bytes[1_500_000] = 9; // in the middle: not sampled
        std::fs::write(&big, &bytes).unwrap();
        let after = file_fingerprint(&big).unwrap();
        assert_eq!(before.rsplit(':').next(), after.rsplit(':').next());
    }

    #[test]
    fn credential_shaped_text_is_refused_like_cores_pattern() {
        for bad in [
            "Bearer abcdefgh1234",
            "x hf_abcdefghijklmnopqrstu y",
            "sk-abcdefghijklmnop",
            r#"{"api_key": "x"}"#,
            r#"{"password":"x"}"#,
            r#""accessToken" :1"#,
            "BEARER   ABCDEFGH12",
        ] {
            assert!(credential_shaped(bad), "{bad}");
        }
        for ok in [
            "Bearer short",
            "hf_short",
            "task-abcdefghijklmnop",
            r#"{"keyboard":1}"#,
            "password",
            "xhf_abcdefghijklmnopqrstu",
            "no secrets here",
        ] {
            assert!(!credential_shaped(ok), "{ok}");
        }
    }

    #[test]
    fn append_stores_core_lines_and_dedupes_like_core() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path().join("evidence-x"));
        let r = rec("m", "vision", "h1", "passed", JValue::Null);
        let e = s.append(&r, 1_700_000_000_000).unwrap();
        assert!(e.get("id").as_str().unwrap().starts_with("ev_"));
        let line = std::fs::read_to_string(s.file()).unwrap();
        assert!(
            line.starts_with(r#"{"id":"ev_"#),
            "id, at, limitations first: {line}"
        );
        assert!(
            line.contains(r#""at":1700000000000,"limitations":[],"model":"m""#),
            "{line}"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(s.file()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(d.path().join("evidence-x"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        // Unchanged: the newest record is returned and nothing is appended.
        let again = s.append_if_changed(&r, 1_700_000_001_000).unwrap();
        assert_eq!(again.get("id"), e.get("id"));
        assert_eq!(s.list().len(), 1);
        // A different result is a change; key order of an object value is not.
        let a = rec(
            "m",
            "context_capacity",
            "h1",
            "passed",
            JValue::obj([("ctx", num(1.0)), ("slots", num(2.0))]),
        );
        let b = rec(
            "m",
            "context_capacity",
            "h1",
            "passed",
            JValue::obj([("slots", num(2.0)), ("ctx", num(1.0))]),
        );
        s.append_if_changed(&a, 1).unwrap();
        s.append_if_changed(&b, 2).unwrap();
        assert_eq!(
            s.list().len(),
            2,
            "same value in another key order is not a change"
        );
        s.append_if_changed(&rec("m", "vision", "h1", "failed", JValue::Null), 3)
            .unwrap();
        assert_eq!(s.list().len(), 3);
    }

    #[test]
    fn invalid_and_credential_records_are_refused_and_nothing_is_written() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path().join("e"));
        assert_eq!(
            s.append(&rec("m", "vision", "h", "maybe", JValue::Null), 1),
            Err(StoreError::Invalid)
        );
        assert_eq!(
            s.append(&rec("", "vision", "h", "passed", JValue::Null), 1),
            Err(StoreError::Invalid)
        );
        assert_eq!(s.append(&JValue::Null, 1), Err(StoreError::Invalid));
        let mut with_secret = rec("m", "vision", "h", "passed", JValue::Null);
        if let JValue::Obj(o) = &mut with_secret {
            o.push((
                "limitations".into(),
                JValue::Arr(vec!["Bearer abcdefgh12345".into()]),
            ));
        }
        assert_eq!(s.append(&with_secret, 1), Err(StoreError::Credentials));
        let mut task = rec("m", "vision", "h", "passed", JValue::Null);
        if let JValue::Obj(o) = &mut task {
            o.push(("taskId".into(), "t".into()));
        }
        assert_eq!(s.append(&task, 1), Err(StoreError::Task));
        assert!(!s.file().exists());
    }

    #[test]
    fn reported_rates_are_kept_per_meaningful_change_or_per_day() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path().join("e"));
        let rate = |r: f64| {
            rec(
                "m",
                "mtp_acceptance",
                "h",
                "reported",
                JValue::obj([("rate", num(r))]),
            )
        };
        s.append_reported_rate(&rate(0.50), 1_000).unwrap();
        s.append_reported_rate(&rate(0.52), 2_000).unwrap();
        assert_eq!(
            s.list().len(),
            1,
            "a drift under 0.05 within a day is not kept"
        );
        s.append_reported_rate(&rate(0.60), 3_000).unwrap();
        assert_eq!(s.list().len(), 2);
        s.append_reported_rate(&rate(0.61), 3_000 + 86_400_001)
            .unwrap();
        assert_eq!(s.list().len(), 3, "and one a day later is");
    }

    #[test]
    fn compaction_keeps_the_newest_per_key_and_drops_the_oldest_identities() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::with_caps(
            d.path().join("e"),
            Caps {
                max_bytes: 1 << 30,
                keep_per_key: 3,
                max_keys: 2,
                max_records: 8,
            },
        );
        // Three identities, five records each, one after the other. The cap is 8 records plus a
        // quarter of headroom (10): the 11th append compacts.
        for (n, h) in ["old", "mid", "new"].iter().enumerate() {
            for round in 0..5 {
                let at = i64::try_from(n * 10 + round).unwrap();
                s.append(
                    &rec(
                        "m",
                        "throughput",
                        h,
                        "reported",
                        JValue::obj([("rate", num(at as f64))]),
                    ),
                    at,
                )
                .unwrap();
            }
        }
        let kept = s.list();
        assert!(kept.len() <= 10, "{}", kept.len());
        let count = |h: &str| {
            kept.iter()
                .filter(|r| r.get("identityHash").as_str() == Some(h))
                .count()
        };
        assert_eq!(
            count("old"),
            0,
            "the identity whose newest record is oldest goes first"
        );
        assert_eq!(
            count("mid"),
            3,
            "and an identity that stays keeps its newest three"
        );
        assert!(count("new") >= 1, "the newest identity is never dropped");
        // The kept records of an identity are its newest, in order.
        let mid: Vec<f64> = kept
            .iter()
            .filter(|r| r.get("identityHash").as_str() == Some("mid"))
            .map(|r| r.get("value").get("rate").as_num().unwrap())
            .collect();
        assert_eq!(mid, [12.0, 13.0, 14.0]);
    }

    #[test]
    fn a_log_with_unparseable_lines_is_still_read() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path().join("e"));
        std::fs::create_dir_all(d.path().join("e")).unwrap();
        std::fs::write(
            s.file(),
            "{\"model\":\"m\",\"x\":1}\nnot json\n\n{\"model\":\"n\"}\n",
        )
        .unwrap();
        assert_eq!(s.list().len(), 2);
    }

    #[test]
    fn derive_follows_cores_states() {
        let r = vec![
            rec("m", "vision", "old", "passed", JValue::Null),
            rec("m", "vision", "live", "failed", JValue::Null),
            rec("m", "mtp_acceptance", "old", "reported", JValue::Null),
            rec("m", "throughput", "live", "reported", JValue::Null),
            rec("other", "context_capacity", "live", "passed", JValue::Null),
        ];
        assert_eq!(derive(&r, "m", "vision", Some("live")).state, "failed");
        assert_eq!(
            derive(&r, "m", "vision", Some("new")).state,
            "stale",
            "an older non-reported record"
        );
        assert_eq!(
            derive(&r, "m", "mtp_acceptance", Some("new")).state,
            "unverified",
            "a reported-only history is not stale"
        );
        assert_eq!(
            derive(&r, "m", "throughput", Some("live")).state,
            "reported"
        );
        assert_eq!(
            derive(&r, "m", "context_capacity", Some("live")).state,
            "unverified"
        );
        let none = derive(&r, "m", "vision", None);
        assert_eq!(none.state, "unavailable");
        assert_eq!(
            none.record.unwrap().get("identityHash").as_str(),
            Some("live"),
            "the newest record is still shown"
        );
        assert_eq!(derive_external(&[], "m", Some("a")).state, "unverified");
        let card = |h: &str| rec("m", "external_model_card", h, "reported", JValue::Null);
        assert_eq!(
            derive_external(&[card("a")], "m", Some("a")).state,
            "reported"
        );
        assert_eq!(derive_external(&[card("a")], "m", Some("b")).state, "stale");
        assert_eq!(
            derive_external(&[card("a")], "m", None).state,
            "unavailable"
        );
        let samp = |h: &str, v: JValue| rec("m", "external_sampling_config", h, "reported", v);
        assert_eq!(
            derive_sampling(&[samp("a", JValue::Null)], "m", Some("a")).state,
            "unverified",
            "a retired value"
        );
        assert_eq!(
            derive_sampling(
                &[samp("a", JValue::obj([("top_p", num(0.9))]))],
                "m",
                Some("a")
            )
            .state,
            "reported"
        );
        assert_eq!(
            derive_sampling(&[samp("a", JValue::Obj(Vec::new()))], "m", Some("b")).state,
            "stale"
        );
    }

    #[test]
    fn limitation_sentences_get_their_ids() {
        let k = |t: &str| stringify(&limitation_key(t).unwrap_or(JValue::Null)).unwrap();
        assert_eq!(
            k("prompt budget 120 s"),
            r#"{"id":"calibration-budget","params":{"seconds":120}}"#
        );
        assert_eq!(
            k("prompt budget 7.5 s"),
            r#"{"id":"calibration-budget","params":{"seconds":7.5}}"#
        );
        assert_eq!(k("prompt budget x s"), "null");
        assert_eq!(
            k("median of 5 warm, uncontended requests; depends on prompt mix and max tokens"),
            r#"{"id":"benchmark-median","params":{"n":5}}"#
        );
        assert_eq!(
            k("single reply; depends on content"),
            r#"{"id":"single-reply","params":{}}"#
        );
        assert_eq!(
            k("1\u{d7}1 image accepted; not an accuracy test"),
            r#"{"id":"vision-probe","params":{}}"#
        );
        assert_eq!(k("Probes not used (failed at the model's reference settings): arithmetic, extraction"), r#"{"id":"autotune-baseline-skipped","params":{"probes":"arithmetic, extraction"}}"#);
        assert_eq!(k("a probe's own failure text"), "null");
        assert_eq!(
            stringify(&limitation_keys(&JValue::Arr(vec![
                "single reply; depends on content".into(),
                num(1.0)
            ])))
            .unwrap(),
            r#"[{"id":"single-reply","params":{}},null]"#
        );
        assert_eq!(stringify(&limitation_keys(&JValue::Null)).unwrap(), "[]");
    }

    #[test]
    fn model_families_match_cores_patterns() {
        let id = |m: &str| family_of(m).map(|f| f.id);
        assert_eq!(id("Qwen3.5-9B-Instruct"), Some("qwen3.5"));
        assert_eq!(id("qwen-3-5"), Some("qwen3.5"));
        assert_eq!(id("Qwen3-32B"), Some("qwen3"));
        assert_eq!(id("gpt-oss-20b"), Some("gpt-oss"));
        assert_eq!(id("openai/gpt_oss"), Some("gpt-oss"));
        assert_eq!(
            id("mygpt-ossify"),
            None,
            "gpt-oss must start a word and not run on into a letter"
        );
        assert_eq!(id("xgpt-oss-1"), None);
        assert_eq!(id("DeepSeek R1 Distill"), Some("deepseek-r1"));
        assert_eq!(id("Llama.3.1-8B"), Some("llama-3"));
        assert_eq!(id("gemma-4-E2B"), Some("gemma-4"));
        assert_eq!(id("gemma3"), Some("gemma-3"));
        assert_eq!(id("Devstral-Small"), Some("devstral"));
        assert_eq!(id("mistral-7b"), Some("devstral"));
        assert_eq!(id("phi 4 mini"), Some("phi-4"));
        assert_eq!(id("qwen2"), None);
        assert_eq!(id(""), None);
        assert_eq!(
            id("gemma-34"),
            Some("gemma-3"),
            "the pattern has no word boundary after the digit"
        );
    }

    #[test]
    fn the_sampling_plan_takes_the_card_then_the_family_then_the_preset() {
        let plan =
            |m: &str, card: JValue| stringify(&sampling_plan(m, &card, &JValue::Null)).unwrap();
        // A family value set, with its note.
        assert_eq!(
            plan("Qwen3-8B", JValue::Null),
            r#"{"familyId":"qwen3","familyLabel":"Qwen3","quirks":{},"versions":{"familyTable":1,"presets":1},"tier":"family","source":"Qwen3 family table","sourceId":"family-table","values":{"temperature":0.6,"top_p":0.95,"top_k":20,"min_p":0},"note":"thinking-mode values","noteId":"thinking-mode","provenance":null}"#
        );
        // No family: the general preset carries no value.
        assert_eq!(
            plan("some-model", JValue::Null),
            r#"{"familyId":null,"familyLabel":null,"quirks":{},"versions":{"familyTable":1,"presets":1},"tier":"preset","source":"Task preset: general","sourceId":"task-preset","presetId":"general","values":{},"note":null,"noteId":null,"provenance":null}"#
        );
        // A usable card wins as a whole; invalid keys are dropped.
        let card = JValue::obj([
            ("temperature", num(0.3)),
            ("top_p", num(5.0)),
            ("min_p", num(0.1)),
        ]);
        let p = stringify(&sampling_plan(
            "gemma-4-x",
            &card,
            &JValue::obj([("revision", "r".into())]),
        ))
        .unwrap();
        assert!(p.contains(r#""tier":"model-card","source":"generation_config.json","sourceId":"generation-config","values":{"temperature":0.3,"min_p":0.1}"#), "{p}");
        assert!(p.ends_with(r#""provenance":{"revision":"r"}}"#));
        // An all-invalid card falls through to the family.
        let bad = JValue::obj([("temperature", num(9.0))]);
        assert!(stringify(&sampling_plan("phi-4", &bad, &JValue::Null))
            .unwrap()
            .contains(r#""tier":"family""#));
        // gpt-oss carries its quirks.
        assert!(plan("gpt-oss-20b", JValue::Null)
            .contains(r#""quirks":{"harmony":true,"reasoningEffort":"low","qualityBudget":512}"#));
    }
}
