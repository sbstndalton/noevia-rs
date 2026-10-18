//! noevia#1318: native models.ini writers. `PUT /api/v1/sections/{name}`,
//! `PUT /api/v1/models-ini`, `POST /api/v1/sections/{name}/rename`,
//! `DELETE /api/v1/sections/{name}` and `POST /api/v1/sections/{name}/safe-defaults` move
//! together, so models.ini keeps a single writer process (Python's `ini.WRITE_LOCK` only ever
//! serialised its own threads).
//!
//! Each route is services `api.py` + `ini.py` behaviour: the same validation order and messages,
//! the revision compare-and-swap, `_upsert_section` with the `_bound_cache_ram` cache-ram
//! bounds, the in-place `_render_preserving` edit (falling back to configparser's full render),
//! `_rename_header`, the roles file (`models.roles.json`), and `_replace_file`'s backups (the
//! `model-files backups` plan; MODEL_FILES_IMPL is retired, noevia#1957) and
//! fsynced atomic replace. Safe defaults need the GGUF hints, the draft-head search and the
//! built-in MTP layer count: those come from the Python child's private
//! `GET /api/v1/internal/safe-defaults-plan` (read-only there), asked while this side holds
//! the write lock, and the write itself is native.
pub mod cfg;

use cfg::{Dict, ParseError, Parser};
use model_files::json::Value as Json;
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

/// `api.MODELS_INI_MAX_BYTES`.
pub const MODELS_INI_MAX_BYTES: usize = 1024 * 1024;
/// `ini.BACKUPS_TO_KEEP` / `REVISION_BACKUPS_TO_KEEP`.
pub const BACKUPS_TO_KEEP: usize = 10;
/// Largest models.ini read.
pub const READ_CAP: u64 = 8 * 1024 * 1024;
/// The child's private plan call for safe defaults.
pub const PLAN_PATH: &str = "/api/v1/internal/safe-defaults-plan";
/// The fields the section form writes, in `ini.ALL_FIELDS` order, with their kind.
pub fn fields() -> Vec<(&'static str, &'static str)> {
    crate::sections::field_kinds()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Json(u16, Vec<u8>),
    /// Python raised: FastAPI's plain 500.
    Failed,
}

/// The child's safe-defaults answer.
pub enum Plan {
    /// A refusal `HTTPException(status, detail)`.
    Refused(u16, String),
    Values(Dict, bool),
}

pub type Clock = Arc<dyn Fn() -> std::time::SystemTime + Send + Sync>;

pub struct SectionWrites {
    pub ini: PathBuf,
    /// `settings.cache_ram_limits`: (cap, hard maximum) in MiB.
    pub cache: (i64, i64),
    clock: Clock,
    pub(crate) started: std::sync::atomic::AtomicBool,
    lock: Arc<tokio::sync::Mutex<()>>,
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn detail(status: u16, message: &str) -> Outcome {
    Outcome::Json(
        status,
        format!("{{\"detail\":{}}}", json_string(message)).into_bytes(),
    )
}

/// `str(value)` for a JSON value FastAPI handed the route.
fn py_str(value: &Json) -> Option<String> {
    Some(match value {
        Json::Str(s) => s.clone(),
        Json::Null => "None".into(),
        Json::Bool(b) => if *b { "True" } else { "False" }.into(),
        Json::Int(n) => n.to_string(),
        Json::Float(f) => gguf::py_float_repr(*f),
        Json::Arr(a) if a.is_empty() => "[]".into(),
        Json::Obj(o) if o.is_empty() => "{}".into(),
        other => crate::prompts::py_str(Some(other)).ok()?,
    })
}

/// `str(body.get(key) or "")`.
fn or_empty(body: &Json, key: &str) -> Option<String> {
    crate::prompts::py_str(body.get(key)).ok()
}

/// `re.fullmatch(r"[A-Za-z0-9._\-+]+", name)`: no trailing newline (noevia#1934).
pub fn valid_section_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-+".contains(&c))
}

/// `clamp_cache_ram(value, hard)`.
pub fn clamp_cache_ram(value: &str, hard: i64) -> String {
    let text = cfg::strip(value);
    if !int_text(text) {
        return text.to_owned();
    }
    match text.parse::<i128>() {
        Ok(n) if n < 0 || n > i128::from(hard) => hard.to_string(),
        Ok(n) => n.to_string(),
        // Python's int() is unbounded: anything past i128 is above the hard maximum.
        Err(_) if text.starts_with('-') => hard.to_string(),
        Err(_) => hard.to_string(),
    }
}

/// `re.fullmatch(r"-?\d+", text)`; `\d` is any Unicode decimal digit, which int() also reads,
/// but cache-ram values outside ASCII are left as they are here (see the docs).
fn int_text(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

const CACHE_RAM_KEYS: [&str; 3] = ["cache-ram", "cram", "llama_arg_cache_ram"];

fn cache_ram_keys(cp: &Parser, name: &str) -> Vec<String> {
    cp.items(name)
        .map(|d| {
            d.0.into_iter()
                .map(|(k, _)| k)
                .filter(|k| {
                    CACHE_RAM_KEYS.contains(
                        &cfg::strip(k)
                            .trim_start_matches('-')
                            .to_lowercase()
                            .as_str(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn effective_cache_ram(cp: &Parser, name: &str) -> Option<String> {
    let values: Vec<String> = cache_ram_keys(cp, name)
        .iter()
        .filter_map(|k| cp.get(name, k))
        .map(|v| cfg::strip(&v).to_owned())
        .filter(|v| !v.is_empty())
        .collect();
    if values.is_empty() {
        return None;
    }
    let ints: Vec<&String> = values.iter().filter(|v| int_text(v)).collect();
    if ints.is_empty() {
        return values.last().cloned();
    }
    let parsed: Vec<i128> = ints
        .iter()
        .map(|v| v.parse().unwrap_or(i128::MAX))
        .collect();
    if ints
        .iter()
        .any(|v| v.starts_with('-') && !v.trim_start_matches(['-', '0']).is_empty())
    {
        return Some("-1".into());
    }
    let max = ints
        .iter()
        .zip(&parsed)
        .max_by_key(|(_, n)| **n)
        .map(|(v, n)| {
            if *n == i128::MAX {
                (*v).clone()
            } else {
                n.to_string()
            }
        })?;
    Some(max)
}

fn truthy_flag(cp: &Parser, name: &str, key: &str) -> bool {
    matches!(
        cp.get(name, key)
            .map(|v| cfg::strip(&v).to_lowercase())
            .as_deref(),
        Some("true" | "1" | "on")
    )
}

fn non_chat(cp: &Parser, name: &str) -> bool {
    if name == "*" || !cp.has_section(name) {
        return false;
    }
    if ["embedding", "embeddings", "reranking", "rerank"]
        .iter()
        .any(|k| truthy_flag(cp, name, k))
    {
        return true;
    }
    let low = name.to_lowercase();
    low.contains("embed") || low.contains("rerank")
}

/// `_bound_cache_ram(cp, name)`.
pub fn bound_cache_ram(cp: &mut Parser, name: &str, cache: (i64, i64)) {
    let (cap, hard) = cache;
    let own = effective_cache_ram(cp, name);
    let keys = cache_ram_keys(cp, name);
    if let Some(section) = cp.section_mut(name) {
        for k in &keys {
            section.remove(k);
        }
    }
    if let Some(own) = own {
        if let Some(section) = cp.section_mut(name) {
            section.set("cache-ram", &clamp_cache_ram(&own, hard));
        }
        return;
    }
    if name == "*" {
        return;
    }
    if non_chat(cp, name) {
        if let Some(section) = cp.section_mut(name) {
            section.set("cache-ram", "0");
        }
        return;
    }
    if let Some(inherited) = effective_cache_ram(cp, "*") {
        if clamp_cache_ram(&inherited, hard) == inherited {
            return;
        }
    }
    if let Some(section) = cp.section_mut(name) {
        section.set("cache-ram", &cap.to_string());
    }
}

/// `_upsert_section(name, values, extras)` on a parser; `false` where Python raises.
pub fn upsert(cp: &mut Parser, name: &str, values: &Dict, extras: &str, cache: (i64, i64)) -> bool {
    cp.remove_section(name);
    if !cp.add_section(name) {
        return false;
    }
    let mut section = Dict::default();
    for (key, _) in fields() {
        if let Some(v) = values.get(key).filter(|v| !v.is_empty()) {
            section.set(key, v);
        }
    }
    for line in cfg::splitlines(extras) {
        let s = cfg::strip(line);
        if s.is_empty() || s.starts_with(['#', ';']) {
            continue;
        }
        let Some((k, v)) = s.split_once('=') else {
            continue;
        };
        let (k, v) = (cfg::strip(k), cfg::strip(v));
        if k.is_empty() {
            continue;
        }
        section.set(k, v);
    }
    if let Some(slot) = cp.section_mut(name) {
        *slot = section;
    }
    bound_cache_ram(cp, name, cache);
    true
}

/// `models.roles.json` beside models.ini: `ini.read_roles` (a missing or unusable file is {}).
fn read_roles(path: &Path) -> Vec<(String, String)> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Vec::new();
    };
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let Ok(Json::Obj(pairs)) = model_files::json::parse(&text) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in pairs {
        // dict(): a repeated key keeps its first place and its last value.
        let value = match &v {
            Json::Str(s) if !cfg::strip(s).is_empty() => Some(cfg::strip(s).to_lowercase()),
            _ => None,
        };
        let pos = out.iter().position(|(key, _)| *key == k);
        match (pos, value) {
            (Some(i), Some(val)) => {
                if let Some(slot) = out.get_mut(i) {
                    slot.1 = val;
                }
            }
            (Some(i), None) => {
                out.remove(i);
            }
            (None, Some(val)) => out.push((k, val)),
            (None, None) => {}
        }
    }
    out
}

/// `json.dumps(roles, indent=2, sort_keys=True) + "\n"` (ASCII-escaped, as Python's default).
fn roles_text(roles: &[(String, String)]) -> String {
    let esc = |s: &str| {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                    let mut buf = [0u16; 2];
                    for unit in c.encode_utf16(&mut buf) {
                        out.push_str(&format!("\\u{unit:04x}"));
                    }
                }
                c => out.push(c),
            }
        }
        out.push('"');
        out
    };
    let mut sorted: Vec<&(String, String)> = roles.iter().collect();
    sorted.sort_by(|a, b| a.0.chars().cmp(b.0.chars()));
    let body: Vec<String> = sorted
        .iter()
        .map(|(k, v)| format!("  {}: {}", esc(k), esc(v)))
        .collect();
    format!("{{\n{}\n}}\n", body.join(",\n"))
}

/// `ini._forget_role(name, new_name)`.
fn forget_role(ini: &Path, name: &str, new_name: Option<&str>) -> std::io::Result<()> {
    let path = ini.with_file_name("models.roles.json");
    let mut roles = read_roles(&path);
    let Some(i) = roles.iter().position(|(k, _)| k == name) else {
        return Ok(());
    };
    let (_, role) = roles.remove(i);
    if let Some(new) = new_name.filter(|n| !n.is_empty()) {
        match roles.iter_mut().find(|(k, _)| k == new) {
            Some(slot) => slot.1 = role,
            None => roles.push((new.to_owned(), role)),
        }
    }
    if roles.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let tmp = path.with_file_name("models.roles.json.tmp");
    std::fs::write(&tmp, roles_text(&roles))?;
    std::fs::rename(&tmp, &path)
}

/// The current file: its bytes (`None` when it does not exist) and revision.
fn current(path: &Path) -> Result<(Option<Vec<u8>>, String), ()> {
    let bytes = match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(()),
        Ok(m) if m.len() > READ_CAP => return Err(()),
        Ok(_) => Some(std::fs::read(path).map_err(|_| ())?),
    };
    let revision = format!("{:x}", Sha256::digest(bytes.as_deref().unwrap_or_default()));
    Ok((bytes, revision))
}

/// `read_ini(for_write=...)`.
fn read_ini(bytes: Option<&[u8]>, for_write: bool) -> Result<Parser, Outcome> {
    let Some(bytes) = bytes else {
        return Ok(Parser {
            source: Some(String::new()),
            ..Parser::default()
        });
    };
    let text = std::str::from_utf8(bytes).map_err(|_| Outcome::Failed)?;
    // Path.read_text: universal newlines.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let parsed = if for_write {
        cfg::parse(&text)
    } else {
        cfg::parse_allowing(&text)
    };
    parsed.map_err(|e| match e.message() {
        Some(m) => detail(
            409,
            &format!("{m}. Repair the file (remove the repeated section) before saving."),
        ),
        None => Outcome::Failed,
    })
}

fn require_revision(base: Option<&Json>, current: &str) -> Result<(), Outcome> {
    match base {
        Some(b) if b.truthy() => {
            if matches!(b, Json::Str(s) if s == current) {
                Ok(())
            } else {
                Err(detail(
                    409,
                    "models.ini changed since you loaded it. Reload, review, then save again.",
                ))
            }
        }
        _ => Err(detail(400, "baseRevision is required")),
    }
}

impl SectionWrites {
    pub fn new(ini: &Path, cache: (i64, i64)) -> Self {
        Self::with_clock(ini, cache, Arc::new(std::time::SystemTime::now))
    }
    pub fn with_clock(ini: &Path, cache: (i64, i64), clock: Clock) -> Self {
        Self {
            ini: ini.to_owned(),
            cache,
            clock,
            started: std::sync::atomic::AtomicBool::new(false),
            lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub async fn lock(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.lock.clone().lock_owned().await
    }

    /// `time.strftime("%Y%m%d-%H%M%S")` in UTC (the native writer requires TZ unset or UTC).
    fn stamp(&self) -> String {
        let secs = (self.clock)()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let days = i64::try_from(secs / 86_400).unwrap_or(0);
        let rem = secs % 86_400;
        let (y, m, d) = civil(days);
        format!(
            "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
            rem / 3600,
            (rem % 3600) / 60,
            rem % 60
        )
    }

    fn rotating_name(&self, dir: &Path, name: &str) -> String {
        let ts = self.stamp();
        let mut candidate = format!("{name}.bak-{ts}");
        let mut n = 1;
        while dir.join(&candidate).exists() {
            candidate = format!("{name}.bak-{ts}-{n}");
            n += 1;
        }
        candidate
    }

    /// `_replace_file(path, text, base_revision=..., backup=...)`; `Err` is Python's OSError
    /// (nothing written).
    pub fn replace_file(
        &self,
        text: &str,
        base: Option<&str>,
        backup: bool,
    ) -> std::io::Result<()> {
        let path = &self.ini;
        let dir = path
            .parent()
            .ok_or_else(|| std::io::Error::other("no parent"))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| std::io::Error::other("bad name"))?;
        std::fs::create_dir_all(dir)?;
        let mut mode = 0o644;
        let mut prune: Vec<PathBuf> = Vec::new();
        if let Ok(meta) = std::fs::metadata(path) {
            mode = meta.permissions().mode() & 0o777;
            let mut existing = Vec::new();
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let meta = entry.metadata()?;
                if meta.file_type().is_file() {
                    let Ok(n) = entry.file_name().into_string() else {
                        continue;
                    };
                    let ns =
                        i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec());
                    existing.push(model_files::backups::Listed {
                        name: n,
                        mtime_ns: u128::try_from(ns.max(0)).unwrap_or(0),
                    });
                }
            }
            let request = model_files::backups::Request {
                file: name.to_owned(),
                base_revision: base.map(str::to_owned),
                backup,
                rotating_name: Some(self.rotating_name(dir, name)),
                keep_revisions: BACKUPS_TO_KEEP,
                existing,
            };
            let plan = model_files::backups::plan(&request);
            if plan.revision.is_some() {
                immutable_backup(path, base.unwrap_or_default())?;
            }
            if let Some(rot) = plan.rotating {
                copy(path, &dir.join(rot))?;
            }
            prune = plan
                .prune
                .iter()
                .filter(|n| own_copy(name, n))
                .map(|n| dir.join(n))
                .collect();
        }
        let nanos = std::time::Instant::now().elapsed().as_nanos()
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
        let tmp = dir.join(format!("{name}.tmp-{}-{nanos}", std::process::id()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.flush()?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)?;
            std::fs::File::open(dir)?.sync_all()
        })();
        let _ = std::fs::remove_file(&tmp);
        result?;
        for old in prune {
            let _ = std::fs::remove_file(old);
        }
        Ok(())
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

fn own_copy(file: &str, name: &str) -> bool {
    model_files::backups::is_own_rotating_copy(file, name)
        || model_files::backups::is_revision_copy(file, name)
}

/// `shutil.copy`: content and permission bits, a new mtime.
fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::copy(from, to).map(|_| ())
}

/// `_immutable_backup(path, base)`: 0600, exclusive create, never overwritten.
fn immutable_backup(path: &Path, base: &str) -> std::io::Result<()> {
    if base.len() != 64 || !base.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(std::io::Error::other(
            "baseRevision must be a sha256 hex digest",
        ));
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| std::io::Error::other("bad name"))?;
    let backup = path.with_file_name(format!("{name}.noevia-backup-{base}"));
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&backup)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e),
    };
    file.write_all(&std::fs::read(path)?)?;
    file.flush()?;
    file.sync_all()
}

/// `{"ok":true,"revision":...}` plus extra fields already rendered.
fn ok_body(revision: &str, extra: &str) -> Vec<u8> {
    format!(
        "{{\"ok\":true,\"revision\":{}{extra}}}",
        json_string(revision)
    )
    .into_bytes()
}

fn section_json(cp: &Parser, name: &str) -> String {
    match cp.items(name) {
        Some(d) => format!(
            "{{{}}}",
            d.0.iter()
                .map(|(k, v)| format!("{}:{}", json_string(k), json_string(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
        None => "null".into(),
    }
}

/// What a finished write leaves for the answer: the revision and the section as `get_section`
/// reads it, both from the file as written.
fn after_write(path: &Path, name: Option<&str>) -> Result<(String, String), Outcome> {
    let (bytes, revision) = current(path).map_err(|_| Outcome::Failed)?;
    let section = match name {
        Some(name) => {
            let cp = read_ini(bytes.as_deref(), false)?;
            section_json(&cp, name)
        }
        None => String::new(),
    };
    Ok((revision, section))
}

/// The parts of a `PUT /sections/{name}` body the handler reads, already validated.
pub struct SectionBody {
    pub values: Dict,
    pub extras: String,
}

/// `save_section`'s checks before the lock, in Python's order.
pub fn section_body(name: &str, body: &Json) -> Result<SectionBody, Outcome> {
    if !valid_section_name(name) {
        return Err(detail(400, "invalid section name"));
    }
    if !body.get("baseRevision").is_some_and(Json::truthy) {
        return Err(detail(400, "baseRevision is required"));
    }
    let raw = body.get("values").filter(|v| v.truthy());
    let raw_pairs: Vec<(String, Json)> = match raw {
        None => Vec::new(),
        Some(Json::Obj(pairs)) => pairs.clone(),
        // `raw.get` on a list or scalar raises AttributeError.
        Some(_) => return Err(Outcome::Failed),
    };
    let lookup = |key: &str| -> Option<&Json> {
        raw_pairs
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    };
    let mut values = Dict::default();
    for (key, kind) in fields() {
        let v = lookup(key);
        let text = if kind == "bool" {
            let on = match v {
                Some(Json::Bool(true)) => true,
                Some(Json::Int(n)) => n.to_string() == "1",
                Some(Json::Float(f)) => *f == 1.0,
                Some(Json::Str(s)) => matches!(s.as_str(), "true" | "on" | "1"),
                _ => false,
            };
            if on {
                "true".to_owned()
            } else {
                String::new()
            }
        } else {
            match v {
                None | Some(Json::Null) => String::new(),
                Some(v) => cfg::strip(&py_str(v).ok_or(Outcome::Failed)?).to_owned(),
            }
        };
        values.0.push((key.to_owned(), text));
    }
    let extras = or_empty(body, "extras").ok_or(Outcome::Failed)?;
    if values.0.iter().any(|(_, v)| v.contains(['\n', '\r'])) {
        return Err(detail(400, "values must be single lines"));
    }
    if cfg::splitlines(&extras)
        .iter()
        .any(|l| cfg::strip(l).starts_with('['))
    {
        return Err(detail(400, "extra lines cannot start a new section"));
    }
    Ok(SectionBody { values, extras })
}

impl SectionWrites {
    /// `PUT /api/v1/sections/{name}` (the caller holds [`Self::lock`]).
    pub fn save_section(&self, name: &str, body: &Json, parsed: &SectionBody) -> Outcome {
        let Ok((bytes, revision)) = current(&self.ini) else {
            return Outcome::Failed;
        };
        if let Err(o) = require_revision(body.get("baseRevision"), &revision) {
            return o;
        }
        let mut cp = match read_ini(bytes.as_deref(), true) {
            Ok(cp) => cp,
            Err(o) => return o,
        };
        if !upsert(&mut cp, name, &parsed.values, &parsed.extras, self.cache) {
            return Outcome::Failed;
        }
        if self.replace_file(&cfg::render(&cp), None, true).is_err() {
            return Outcome::Failed;
        }
        match after_write(&self.ini, Some(name)) {
            Ok((rev, section)) => {
                Outcome::Json(200, ok_body(&rev, &format!(",\"section\":{section}")))
            }
            Err(o) => o,
        }
    }

    /// `PUT /api/v1/models-ini`, after the text checks (the caller holds the lock).
    pub fn replace_models_ini(&self, text: &str, base: Option<&Json>, backup: bool) -> Outcome {
        let Ok((_, revision)) = current(&self.ini) else {
            return Outcome::Failed;
        };
        if let Err(o) = require_revision(base, &revision) {
            return o;
        }
        let base = match base {
            Some(Json::Str(s)) => Some(s.as_str()),
            _ => None,
        };
        if self.replace_file(text, base, backup).is_err() {
            return detail(
                500,
                "models.ini could not be written safely; nothing was changed",
            );
        }
        match after_write(&self.ini, None) {
            Ok((rev, _)) => Outcome::Json(200, ok_body(&rev, "")),
            Err(o) => o,
        }
    }

    /// `POST /api/v1/sections/{name}/rename` (the caller holds the lock).
    pub fn rename(&self, old: &str, new: &str, base: Option<&Json>) -> Outcome {
        let Ok((bytes, revision)) = current(&self.ini) else {
            return Outcome::Failed;
        };
        if let Err(o) = require_revision(base, &revision) {
            return o;
        }
        let names = match read_ini(bytes.as_deref(), false) {
            Ok(cp) => cp
                .names()
                .iter()
                .map(|n| (*n).to_owned())
                .collect::<Vec<_>>(),
            Err(o) => return o,
        };
        if names.iter().any(|n| n == new) {
            return detail(409, "a section with that name already exists");
        }
        let mut cp = match read_ini(bytes.as_deref(), true) {
            Ok(cp) => cp,
            Err(o) => return o,
        };
        if !cp.has_section(old) || cp.has_section(new) {
            return detail(404, "section not found");
        }
        let text = match cp
            .source
            .as_deref()
            .and_then(|s| cfg::rename_header(s, &cp, old, new))
        {
            Some(text) => text,
            None => {
                // noevia#1921: the section's own keys only (Python copied DEFAULT's in too).
                let items = cp.section(old).cloned().unwrap_or_default();
                cp.remove_section(old);
                cp.add_section(new);
                if let Some(slot) = cp.section_mut(new) {
                    *slot = items;
                }
                cfg::render(&cp)
            }
        };
        if self.replace_file(&text, None, true).is_err() {
            return Outcome::Failed;
        }
        if forget_role(&self.ini, old, Some(new)).is_err() {
            return Outcome::Failed;
        }
        match after_write(&self.ini, None) {
            Ok((rev, _)) => Outcome::Json(200, ok_body(&rev, "")),
            Err(o) => o,
        }
    }

    /// `DELETE /api/v1/sections/{name}?baseRevision=` (the caller holds the lock).
    pub fn delete(&self, name: &str, base: &str) -> Outcome {
        let Ok((bytes, revision)) = current(&self.ini) else {
            return Outcome::Failed;
        };
        let base = Json::Str(base.to_owned());
        if let Err(o) = require_revision(Some(&base), &revision) {
            return o;
        }
        let mut cp = match read_ini(bytes.as_deref(), true) {
            Ok(cp) => cp,
            Err(o) => return o,
        };
        if !cp.remove_section(name) {
            return detail(404, "section not found");
        }
        if self.replace_file(&cfg::render(&cp), None, true).is_err() {
            return Outcome::Failed;
        }
        if forget_role(&self.ini, name, None).is_err() {
            return Outcome::Failed;
        }
        match after_write(&self.ini, None) {
            Ok((rev, _)) => Outcome::Json(200, ok_body(&rev, "")),
            Err(o) => o,
        }
    }

    /// `POST /api/v1/sections/{name}/safe-defaults`, given the child's plan (the caller holds
    /// the lock while asking for it).
    pub fn safe_defaults(&self, name: &str, plan: Plan) -> Outcome {
        let (values, mtp) = match plan {
            Plan::Refused(status, message) => return detail(status, &message),
            Plan::Values(values, mtp) => (values, mtp),
        };
        let Ok((bytes, _)) = current(&self.ini) else {
            return Outcome::Failed;
        };
        let mut cp = match read_ini(bytes.as_deref(), true) {
            Ok(cp) => cp,
            Err(o) => return o,
        };
        if !upsert(&mut cp, name, &values, "", self.cache) {
            return Outcome::Failed;
        }
        if self.replace_file(&cfg::render(&cp), None, true).is_err() {
            return Outcome::Failed;
        }
        match after_write(&self.ini, Some(name)) {
            Ok((rev, section)) => Outcome::Json(
                200,
                ok_body(&rev, &format!(",\"section\":{section},\"mtp\":{mtp}")),
            ),
            Err(o) => o,
        }
    }
}

/// The system routing role: its value in `models.roles.json` (`ini.LAYA_ROLE`), which the
/// deprecated name/path rule also matches as a name prefix.
const SYSTEM_ROUTING_ROLE: &str = "laya";

/// `re.match(r"^<role>(?:[_.-]|$)", part, re.I)` for [`SYSTEM_ROUTING_ROLE`]; `$` also matches
/// before one final newline.
fn system_routing_name(part: &str) -> bool {
    let role = SYSTEM_ROUTING_ROLE.as_bytes();
    let bytes = part.as_bytes();
    if !bytes
        .get(..role.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(role))
    {
        return false;
    }
    matches!(
        bytes.get(role.len()..),
        Some([] | [b'\n'] | [b'_' | b'.' | b'-', ..])
    )
}

/// `ini._is_laya_section(cp, name)`: the system routing role from `models.roles.json` when the
/// section has a role, else the deprecated name/path rule (logged, as Python logs it).
fn system_routing_section(cp: &Parser, name: &str, roles: &[(String, String)]) -> bool {
    if name == "*" || !cp.has_section(name) {
        return false;
    }
    if let Some((_, role)) = roles.iter().find(|(k, _)| k == name) {
        return role == SYSTEM_ROUTING_ROLE;
    }
    let model = cp
        .get(name, "model")
        .or_else(|| cp.get(name, "m"))
        .unwrap_or_default();
    let matched = std::iter::once(name)
        .chain(model.split(['\\', '/']))
        .filter(|part| !part.is_empty())
        .any(system_routing_name);
    if matched {
        // Once per section: the migration runs once per parent start. The wording is this
        // side's own (no test compares it with Python's log line).
        eprintln!(
            "models.ini [{name}] is treated as a system routing section by the deprecated name/path \
             rule; mark it in models.roles.json ('{{\"{name}\": \"{SYSTEM_ROUTING_ROLE}\"}}'), not in \
             models.ini, which the inference engine also reads (#1179, #1596)"
        );
    }
    matched
}

impl SectionWrites {
    /// `ini.migrate_cache_ram()` (#697, noevia#2010): every chat, embedding and reranking
    /// section gets an explicit, bounded cache-ram (`_bound_cache_ram`), and the file is written
    /// once through the normal backed-up writer when any section changed. `*`, `DEFAULT` and
    /// sections with the system routing role are left alone. A section counts as changed when its values (as
    /// `cp.items` reads them, in any order) differ. Returns the changed sections in file order.
    /// `Err` is a file the migration skips: nothing was written. The caller holds [`Self::lock`].
    pub fn migrate_cache_ram(&self) -> Result<Vec<String>, &'static str> {
        if !self.ini.exists() {
            return Ok(Vec::new());
        }
        let (bytes, _) = current(&self.ini).map_err(|()| "models.ini could not be read")?;
        let mut cp = read_ini(bytes.as_deref(), true).map_err(|o| match o {
            Outcome::Json(409, _) => "models.ini has repeated sections",
            _ => "models.ini could not be parsed",
        })?;
        let roles = read_roles(&self.ini.with_file_name("models.roles.json"));
        let names: Vec<String> = cp.names().iter().map(|n| (*n).to_owned()).collect();
        let mut changed = Vec::new();
        for name in names {
            // `not _is_chat_section and not _is_non_chat_section`, evaluated in Python's order.
            if !non_chat(&cp, &name) && (name == "*" || system_routing_section(&cp, &name, &roles))
            {
                continue;
            }
            let before = cp.items(&name).unwrap_or_default();
            bound_cache_ram(&mut cp, &name, self.cache);
            if !cp.items(&name).unwrap_or_default().same(&before) {
                changed.push(name);
            }
        }
        if !changed.is_empty() {
            self.replace_file(&cfg::render(&cp), None, true)
                .map_err(|_| "models.ini could not be written safely; nothing was changed")?;
        }
        Ok(changed)
    }

    /// The parent's startup migration: once, under the writers' lock, which the blocking work
    /// keeps until the write has returned (the Python child no longer runs it, noevia#2010).
    pub async fn migrate_cache_ram_at_startup(
        self: Arc<Self>,
    ) -> Result<Vec<String>, &'static str> {
        let guard = self.lock().await;
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            self.migrate_cache_ram()
        })
        .await
        .map_err(|_| "the cache-ram migration stopped")?
    }
}

/// `replace_models_ini`'s checks on the body, before the lock.
pub fn models_ini_body(body: &Json) -> Result<(String, bool), Outcome> {
    let text = match body.get("text") {
        Some(Json::Str(s)) if !cfg::strip(s).is_empty() => s.clone(),
        _ => return Err(detail(400, "text is required")),
    };
    if text.len() > MODELS_INI_MAX_BYTES {
        return Err(detail(413, "models.ini exceeds the editor limit"));
    }
    match cfg::parse(&text) {
        Ok(_) => {}
        Err(ParseError::Duplicate(names)) => {
            return Err(detail(
                400,
                &ParseError::Duplicate(names).message().unwrap_or_default(),
            ))
        }
        Err(ParseError::Invalid) => return Err(detail(400, "models.ini text does not parse")),
    }
    let backup = !matches!(body.get("backup"), Some(Json::Bool(false)));
    Ok((text, backup))
}

/// `rename_section`'s check on the new name, before the lock.
pub fn rename_body(body: &Json) -> Result<String, Outcome> {
    let new = cfg::strip(&or_empty(body, "newName").ok_or(Outcome::Failed)?).to_owned();
    if !valid_section_name(&new) {
        return Err(detail(400, "invalid new name"));
    }
    Ok(new)
}

/// `register_safe_defaults`' checks on the name, before the lock.
pub fn safe_defaults_name(name: &str) -> Result<(), Outcome> {
    if !valid_section_name(name) {
        return Err(detail(400, "invalid section name"));
    }
    if name.to_lowercase().contains("mmproj") {
        return Err(detail(
            400,
            "draft heads and projectors are not registered on their own",
        ));
    }
    Ok(())
}

/// The child's plan answer: `{"status":200,"values":{...},"mtp":bool}` or
/// `{"status":4xx,"detail":"..."}`. `None` when it is not one of those.
pub fn parse_plan(bytes: &[u8]) -> Option<Plan> {
    let text = std::str::from_utf8(bytes).ok()?;
    let value = model_files::json::parse(text).ok()?;
    let status: u16 = match value.get("status")? {
        Json::Int(n) => n.to_string().parse().ok()?,
        _ => return None,
    };
    if status == 200 {
        let Json::Obj(pairs) = value.get("values")? else {
            return None;
        };
        let mut values = Dict::default();
        for (k, v) in pairs {
            // upsert skips None (and "") values.
            if matches!(v, Json::Null) {
                values.remove(k);
                continue;
            }
            values.set(k, &py_str(v)?);
        }
        let mtp = matches!(value.get("mtp")?, Json::Bool(true));
        Some(Plan::Values(values, mtp))
    } else if (400..500).contains(&status) {
        let Json::Str(message) = value.get("detail")? else {
            return None;
        };
        Some(Plan::Refused(status, message.clone()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
