//! The models.ini store, as the maintenance work writes it (core `llamacpp-presets.cjs`
//! `createPresetStore` `commit` / `snapshot`, `models-ini-writer.cjs`). Reads and parsing are
//! model_sizing.rs's [`read_presets`](crate::model_sizing::read_presets); this adds the commit.
//!
//! Two writers, as in Core. `MODELS_INI_WRITER=model-loader` (the deployed default) sends the
//! prepared whole-file text to the model-loader's compare-and-swap endpoint, which does the revision
//! check, backup and atomic rename. `web` (or unset) writes the file here: an immutable recovery
//! copy `<file>.noevia-backup-<base revision>` first, then a temp file in the same directory,
//! fsynced and renamed over the file, then the directory fsynced, then the newest [`BACKUP_KEEP`]
//! recovery copies kept. A write the model-loader may or may not have committed (transport loss,
//! timeout, 5xx, a malformed reply) is settled by reading the file back ([`Store::commit_reconciled`],
//! #339): committed is a success, still the base is a retryable 503, anything else a 409 that never
//! overwrites it.
//!
//! The Rust front is the only process that writes models.ini for the routes it owns; Node's
//! remaining writers (the preset editor, the folder sync, auto-tune) write through the model-loader
//! too, whose compare-and-swap is the arbiter between them.

use crate::model_sizing::{read_presets, Presets, Thrown};
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) type WriteAllowed = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

/// llamacpp-presets.cjs `BACKUP_KEEP`.
pub const BACKUP_KEEP: usize = 20;

const UNCERTAIN: &str = "Model Loader did not confirm whether models.ini was saved. Reload the presets to see the current file before retrying.";
const NOT_SAVED: &str = "Model Loader did not confirm the save, and models.ini still holds the previous settings, so the preset was not applied. Check that the model-loader service is running, then retry.";
const THIRD_PARTY: &str = "Model Loader did not confirm the save, and models.ini now holds changes from somewhere else. They were left untouched; reload the presets before retrying.";
const UNREADABLE: &str = "Model Loader did not confirm the save, and models.ini could not be read back to check. Reload the presets before retrying.";
const MISSING: &str = "Model Loader could not save models.ini; nothing was changed. Check that the model-loader service is running and up to date, then retry.";
const REJECTED: &str =
    "Model Loader rejected the token; check MODEL_LOADER_TOKEN. Nothing was changed.";
const READ_ONLY: &str = "models.ini is on a read-only mount while MODELS_INI_WRITER=web, so nothing was changed. Set MODELS_INI_WRITER=model-loader (the default), or remove :ro from the /llamacpp-config mount to roll back to the web writer, then recreate web.";

pub fn sha(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What `commit` fails with (core: an `Error` with `status`, `publicMessage`, `uncertain`, `retryable`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniError {
    pub status: u16,
    pub message: String,
    /// The model-loader may have committed the write.
    pub uncertain: bool,
    pub retryable: bool,
}

impl IniError {
    fn new(status: u16, message: &str) -> IniError {
        IniError {
            status,
            message: message.to_owned(),
            uncertain: false,
            retryable: false,
        }
    }
}

impl From<Thrown> for IniError {
    fn from(t: Thrown) -> IniError {
        IniError::new(t.status.unwrap_or(500), &t.message)
    }
}

/// The text a commit replaces and the text it writes.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub base_revision: String,
    pub text: String,
    pub before: String,
}

/// The model-loader's single-writer endpoint.
#[derive(Clone)]
pub struct LoaderWriter {
    endpoint: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl LoaderWriter {
    /// `MODEL_LOADER_URL` and `MODEL_LOADER_TOKEN` (never printed).
    pub fn new(url: &str, token: Option<&str>) -> Result<LoaderWriter, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| format!("model-loader client: {e}"))?;
        Ok(LoaderWriter {
            endpoint: format!("{}/api/v1/models-ini", url.trim_end_matches('/')),
            token: token.filter(|t| !t.is_empty()).map(str::to_owned),
            client,
        })
    }

    async fn write(&self, c: &Candidate, backup: bool) -> Result<(), IniError> {
        let body = if backup {
            serde_json::json!({ "baseRevision": c.base_revision, "text": c.text })
        } else {
            serde_json::json!({ "baseRevision": c.base_revision, "text": c.text, "backup": false })
        };
        let mut req = self
            .client
            .put(&self.endpoint)
            .header("content-type", "application/json")
            .timeout(Duration::from_secs(30))
            .body(body.to_string());
        if let Some(mut t) = self
            .token
            .as_deref()
            .and_then(|t| HeaderValue::from_str(t).ok())
        {
            t.set_sensitive(true);
            req = req.header(HeaderName::from_static("x-model-loader-token"), t);
        }
        let uncertain = || IniError {
            status: 503,
            message: UNCERTAIN.into(),
            uncertain: true,
            retryable: false,
        };
        let Ok(res) = req.send().await else {
            return Err(uncertain());
        };
        let status = res.status().as_u16();
        let ok = res.status().is_success();
        let Ok(bytes) = res.bytes().await else {
            return Err(uncertain());
        };
        let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
        if ok && parsed.as_ref().and_then(|b| b.get("ok")) == Some(&Value::Bool(true)) {
            return Ok(());
        }
        match status {
            401 | 403 => Err(IniError::new(503, REJECTED)),
            404 | 405 => Err(IniError::new(503, MISSING)),
            409 => Err(IniError::new(
                409,
                "Presets changed while applying. Reload before retrying.",
            )),
            400 | 413 => {
                let detail = parsed
                    .as_ref()
                    .and_then(|b| b.get("detail"))
                    .and_then(Value::as_str)
                    .map(|d| d.chars().take(200).collect::<String>())
                    .unwrap_or_else(|| "invalid text".into());
                Err(IniError::new(
                    status,
                    &format!("Model Loader rejected the preset file: {detail}"),
                ))
            }
            _ => Err(uncertain()),
        }
    }
}

#[derive(Clone)]
pub struct Store {
    file: PathBuf,
    writer: Option<LoaderWriter>,
    backup_keep: usize,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("file", &self.file)
            .field("writer", &self.writer.as_ref().map(|_| "model-loader"))
            .finish()
    }
}

/// The file's text and its revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub text: String,
    pub revision: String,
}

impl Store {
    pub fn new(file: PathBuf, writer: Option<LoaderWriter>) -> Store {
        Store {
            file,
            writer,
            backup_keep: BACKUP_KEEP,
        }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Blocking: `read()`.
    pub fn read(&self) -> Result<Presets, Thrown> {
        read_presets(&self.file)
    }

    /// Blocking: `snapshot()`, the text and revision, or the read error.
    pub fn snapshot(&self) -> Result<Snapshot, Thrown> {
        let p = self.read()?;
        Ok(Snapshot {
            text: p.text.clone(),
            revision: p.revision.clone(),
        })
    }

    /// `commit(candidate, { backup })`.
    pub async fn commit(&self, c: &Candidate, backup: bool) -> Result<(), IniError> {
        self.commit_if(c, backup, std::sync::Arc::new(|| true))
            .await
    }

    pub(super) async fn commit_if(
        &self,
        c: &Candidate,
        backup: bool,
        allowed: WriteAllowed,
    ) -> Result<(), IniError> {
        let store = self.clone();
        let base = c.base_revision.clone();
        let current = tokio::task::spawn_blocking(move || store.read())
            .await
            .map_err(|_| IniError::new(500, "Internal error"))??;
        if current.revision != base {
            return Err(IniError::new(
                409,
                "Presets changed while applying. Reload before retrying.",
            ));
        }
        if !allowed() {
            return Err(IniError::new(409, "Calibration finalization failed."));
        }
        if let Some(w) = &self.writer {
            return w.write(c, backup).await;
        }
        let store = self.clone();
        let c = c.clone();
        let before = current.text;
        tokio::task::spawn_blocking(move || store.web_write(&c, backup, &before, &*allowed))
            .await
            .map_err(|_| IniError::new(500, "Internal error"))?
    }

    /// The web writer: backup, temp file, rename, directory fsync, prune.
    fn web_write(
        &self,
        c: &Candidate,
        backup: bool,
        before: &str,
        allowed: &dyn Fn() -> bool,
    ) -> Result<(), IniError> {
        let dir = self
            .file
            .parent()
            .ok_or_else(|| IniError::new(500, "Internal error"))?;
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&self.file)
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0o600)
        };
        let mut nonce = [0u8; 8];
        {
            use rand_core::RngCore;
            rand_core::OsRng
                .try_fill_bytes(&mut nonce)
                .map_err(|_| IniError::new(500, "Internal error"))?;
        }
        let temp = PathBuf::from(format!(
            "{}.noevia-{}",
            self.file.display(),
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ));
        let result = (|| {
            use std::os::unix::fs::OpenOptionsExt;
            // The temporary file is also the read-only probe: nothing (no backup) is written first.
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&temp)
                .map_err(|e| {
                    if read_only(&e) {
                        IniError::new(503, READ_ONLY)
                    } else {
                        IniError::new(500, "Internal error")
                    }
                })?;
            if backup {
                let backup_file = PathBuf::from(format!(
                    "{}.noevia-backup-{}",
                    self.file.display(),
                    c.base_revision
                ));
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&backup_file)
                {
                    Ok(mut b) => {
                        b.write_all(before.as_bytes())
                            .and_then(|()| b.sync_all())
                            .map_err(|_| IniError::new(500, "Internal error"))?;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err(IniError::new(500, "Internal error")),
                }
            }
            f.write_all(c.text.as_bytes())
                .and_then(|()| f.sync_all())
                .map_err(|_| IniError::new(500, "Internal error"))?;
            drop(f);
            if !allowed() {
                return Err(IniError::new(409, "Calibration finalization failed."));
            }
            std::fs::rename(&temp, &self.file).map_err(|_| IniError::new(500, "Internal error"))?;
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
            Ok(())
        })();
        let _ = std::fs::remove_file(&temp);
        result?;
        // Only after the new file is in place; best-effort, never fails the write.
        prune_backups(&self.file, self.backup_keep);
        Ok(())
    }

    /// `commitReconciled`: settles an uncertain model-loader write by reading the file back (#339).
    pub async fn commit_reconciled(&self, c: &Candidate, backup: bool) -> Result<(), IniError> {
        let err = match self.commit(c, backup).await {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if !err.uncertain {
            return Err(err);
        }
        let store = self.clone();
        let current = tokio::task::spawn_blocking(move || store.snapshot())
            .await
            .ok()
            .and_then(Result::ok);
        let Some(current) = current else {
            return Err(IniError {
                status: 503,
                message: UNREADABLE.into(),
                uncertain: true,
                retryable: false,
            });
        };
        if current.revision == sha(&c.text) {
            return Ok(());
        }
        if current.revision == c.base_revision {
            return Err(IniError {
                status: 503,
                message: NOT_SAVED.into(),
                uncertain: false,
                retryable: true,
            });
        }
        Err(IniError::new(409, THIRD_PARTY))
    }
}

/// EROFS (30) or EACCES: the directory cannot be written.
fn read_only(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(30)
}

// ── prepare (llamacpp-presets.cjs `prepare`) ───────────────────────────────────

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn thrown(status: u16, message: impl Into<String>) -> Thrown {
    Thrown {
        status: Some(status),
        message: message.into(),
    }
}

/// `hardMaxOf(limits)` as the port takes it: a safe non-negative integer, else none (the port then
/// refuses a cache-ram value, stricter than the JS).
fn hard_max_of(n: f64) -> Option<u64> {
    (n.is_finite()
        && n >= 0.0
        && n.fract() == 0.0
        && n <= llamacpp_presets::MAX_SAFE_INTEGER as f64)
        .then_some(n as u64)
}

/// `optionValue(key, value, limits)`: the value as written, or `None` (refused).
fn option_value(key: &str, value: &str, hard_max: f64) -> Option<String> {
    let v = llamacpp_presets::checked_value(&units(key), &units(value), hard_max_of(hard_max))?;
    String::from_utf16(&v).ok()
}

/// `String(n)` for the cap (a whole number of MiB in every configuration).
fn js_string(n: f64) -> String {
    js_json::wtf16::stringify(&js_json::JValue::Num(n)).unwrap_or_else(|| "null".into())
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| u16::try_from(u32::from(c)).is_ok_and(server_stats::js::is_space))
}

/// `new RegExp(`^\\s*-{0,2}${k}\\s*=\\s*(true|1|on)\\s*$`, 'i')`: a mode flag line.
fn flag_line(line: &str, k: &str) -> bool {
    let mut rest = js_trim(line);
    for _ in 0..2 {
        if let Some(r) = rest.strip_prefix('-') {
            rest = r;
        }
    }
    let Some(head) = rest.get(..k.len()) else {
        return false;
    };
    if !head.eq_ignore_ascii_case(k) {
        return false;
    }
    let after = js_trim(&rest[k.len()..]);
    let Some(value) = after.strip_prefix('=') else {
        return false;
    };
    let value = js_trim(value);
    ["true", "1", "on"]
        .iter()
        .any(|v| value.eq_ignore_ascii_case(v))
}

/// `/^\s*([^=\s]+)\s*=/`: the key of an option line (any key, editable or not).
fn line_key(line: &str) -> Option<&str> {
    let js_space = |c: char| u16::try_from(u32::from(c)).is_ok_and(server_stats::js::is_space);
    let rest = line.trim_start_matches(js_space);
    let end = rest.find(|c: char| c == '=' || js_space(c))?;
    if end == 0 {
        return None;
    }
    rest[end..]
        .trim_start_matches(js_space)
        .starts_with('=')
        .then(|| &rest[..end])
}

fn put(list: &mut Vec<(String, String)>, key: &str, value: String) {
    match list.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = value,
        None => list.push((key.to_owned(), value)),
    }
}

/// `presets.prepare({ model, baseRevision, options })` over the file as it is now: the candidate
/// whole-file text, with the same errors in the same order. `options` in insertion order; `None` is
/// a value that is not a string. `limits` is `cacheRamLimits()`: `(capMib, hardMaxMib)`.
///
/// #697: every write leaves the model with an explicit, bounded prompt cache (an unset value would
/// mean llama-server's 8 GiB default); embedding and reranking profiles get an explicit 0.
pub fn prepare(
    file: &Path,
    model: &str,
    base_revision: &str,
    options: &[(String, Option<String>)],
    limits: (f64, f64),
) -> Result<Candidate, Thrown> {
    let (cap_mib, hard_max) = limits;
    if !llamacpp_presets::model_name_ok(&units(model)) {
        return Err(thrown(400, "Invalid preset model name"));
    }
    let mut updates: Vec<(String, String)> = Vec::new();
    for (key, raw) in options {
        let value = raw
            .as_deref()
            .and_then(|raw| option_value(key, raw, hard_max))
            .ok_or_else(|| thrown(400, format!("Invalid preset option: {key}")))?;
        put(&mut updates, key, value);
    }
    if updates.is_empty() {
        return Err(thrown(400, "No preset changes supplied"));
    }
    let data = read_presets(file)?;
    if base_revision != data.revision {
        return Err(thrown(
            409,
            "Presets changed. Reload the profile before saving; your draft is retained.",
        ));
    }
    let section = data.span(model);
    let defaults: &[(&'static str, String)] = data.span("*").map_or(&[], |(_, _, o)| o);
    let mut lines: Vec<String> = data.lines().to_vec();
    {
        let own = updates
            .iter()
            .find(|(k, _)| k == "cache-ram")
            .map(|(_, v)| v.clone())
            .or_else(|| {
                section.and_then(|(_, _, o)| {
                    o.iter()
                        .find(|(k, _)| *k == "cache-ram")
                        .map(|(_, v)| v.clone())
                })
            });
        let inherited = defaults
            .iter()
            .find(|(k, _)| *k == "cache-ram")
            .map(|(_, v)| v.clone());
        let flag = |k: &str| {
            section.is_some_and(|(start, end, _)| {
                lines
                    .get(start + 1..end)
                    .into_iter()
                    .flatten()
                    .any(|l| flag_line(l, k))
            })
        };
        let lower = model.to_ascii_lowercase();
        let no_cache = flag("embedding")
            || flag("embeddings")
            || flag("reranking")
            || flag("rerank")
            || lower.contains("embed")
            || lower.contains("rerank");
        let unset = |v: &Option<String>| v.as_deref().is_none_or(str::is_empty);
        let effective = if unset(&own) { inherited } else { own.clone() };
        let checked = |value: &str| {
            option_value("cache-ram", value, hard_max)
                .ok_or_else(|| thrown(400, "Invalid preset option: cache-ram"))
        };
        if no_cache && unset(&own) {
            put(&mut updates, "cache-ram", checked("0")?);
        } else if unset(&effective) {
            put(&mut updates, "cache-ram", checked(&js_string(cap_mib))?);
        } else if let Some(effective) = effective {
            let c = checked(&effective)?;
            if c != js_trim(&effective) {
                put(&mut updates, "cache-ram", c);
            }
        }
    }
    let additions: Vec<String> = updates
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k} = {v}"))
        .collect();
    if let Some((start, end, _)) = section {
        let content: Vec<String> = lines
            .get(start + 1..end)
            .into_iter()
            .flatten()
            .filter(|line| {
                line_key(line)
                    .and_then(crate::model_sizing::canonical)
                    .is_none_or(|k| !updates.iter().any(|(u, _)| u == k))
            })
            .cloned()
            .collect();
        lines.splice(start + 1..end, content.into_iter().chain(additions));
    } else {
        lines.push(String::new());
        lines.push(format!("[{model}]"));
        lines.extend(additions);
    }
    let text = format!("{}\n", lines.join("\n").trim_end_matches('\n'));
    let mut effective: Vec<(String, String)> = defaults
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    let mut local: Vec<(String, String)> = section
        .map(|(_, _, o)| {
            o.iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    for (k, v) in &updates {
        if v.is_empty() {
            local.retain(|(l, _)| l != k);
        } else {
            put(&mut local, k, v.clone());
        }
    }
    for (k, v) in local {
        put(&mut effective, &k, v);
    }
    let get = |k: &str| {
        effective
            .iter()
            .find(|(e, _)| e == k)
            .map(|(_, v)| units(v))
    };
    let batch = get("batch-size")
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| units("2048"));
    match llamacpp_presets::micro_batch_exceeds(get("ubatch-size").as_deref(), Some(&batch)) {
        Some(false) => {}
        Some(true) => return Err(thrown(400, "Micro batch cannot exceed batch size")),
        None => return Err(thrown(400, "Invalid preset option: ubatch-size")),
    }
    Ok(Candidate {
        base_revision: data.revision.clone(),
        text,
        before: data.text.clone(),
    })
}

/// llamacpp-presets.cjs `pruneBackups`: keeps the newest `keep` `<file>.noevia-backup-<64 hex>`.
pub fn prune_backups(file: &Path, keep: usize) -> Vec<String> {
    if keep == 0 {
        return Vec::new();
    }
    let Some(dir) = file.parent() else {
        return Vec::new();
    };
    let Some(prefix) = file
        .file_name()
        .map(|n| format!("{}.noevia-backup-", n.to_string_lossy()))
    else {
        return Vec::new();
    };
    let Ok(names) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<(String, std::time::SystemTime)> = Vec::new();
    for entry in names.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(tail) = name.strip_prefix(&prefix) else {
            continue;
        };
        if tail.len() != 64
            || !tail
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            continue;
        }
        if let Ok(meta) = std::fs::symlink_metadata(entry.path()) {
            if meta.is_file() {
                rows.push((name, meta.modified().unwrap_or(std::time::UNIX_EPOCH)));
            }
        }
    }
    // Newest first; the name breaks a tie.
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut removed = Vec::new();
    for (name, _) in rows.into_iter().skip(keep) {
        if std::fs::remove_file(dir.join(&name)).is_ok() {
            removed.push(name);
        }
    }
    removed
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn store(dir: &Path, text: &str) -> (Store, PathBuf) {
        let file = dir.join("models.ini");
        std::fs::write(&file, text).unwrap();
        (Store::new(file.clone(), None), file)
    }

    #[tokio::test]
    async fn the_web_writer_keeps_a_recovery_copy_and_replaces_the_file_atomically() {
        let d = tempfile::tempdir().unwrap();
        let (s, file) = store(d.path(), "[a]\nmodel = /models/a.gguf\n");
        let snap = s.snapshot().unwrap();
        let c = Candidate {
            base_revision: snap.revision.clone(),
            text: "[a]\nmodel = /models/a.gguf\nctx-size = 4096\n".into(),
            before: snap.text.clone(),
        };
        s.commit(&c, true).await.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), c.text);
        let backup = d
            .path()
            .join(format!("models.ini.noevia-backup-{}", snap.revision));
        assert_eq!(std::fs::read_to_string(backup).unwrap(), snap.text);
        let leftovers: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("models.ini.noevia-")
                    && !e.file_name().to_string_lossy().contains("backup")
            })
            .collect();
        assert!(leftovers.is_empty(), "no temp file is left");
        // The base moved on: a second commit on the old revision is refused and changes nothing.
        let again = s.commit(&c, true).await.unwrap_err();
        assert_eq!(
            (again.status, again.message.as_str()),
            (
                409,
                "Presets changed while applying. Reload before retrying."
            )
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), c.text);
    }

    #[tokio::test]
    async fn backup_false_writes_no_recovery_copy() {
        let d = tempfile::tempdir().unwrap();
        let (s, _) = store(d.path(), "[a]\n");
        let snap = s.snapshot().unwrap();
        s.commit(
            &Candidate {
                base_revision: snap.revision,
                text: "[a]\nctx-size = 1\n".into(),
                before: snap.text,
            },
            false,
        )
        .await
        .unwrap();
        assert!(!std::fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("backup")));
    }

    #[test]
    fn only_the_newest_backups_are_kept_and_other_files_are_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("models.ini");
        std::fs::write(&file, "x").unwrap();
        for i in 0..25u32 {
            let name = format!("models.ini.noevia-backup-{i:064x}");
            std::fs::write(d.path().join(&name), "b").unwrap();
            let t = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000 + u64::from(i));
            std::fs::File::options()
                .write(true)
                .open(d.path().join(&name))
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        std::fs::write(d.path().join("models.ini.noevia-backup-nothex"), "keep").unwrap();
        let removed = prune_backups(&file, BACKUP_KEEP);
        assert_eq!(removed.len(), 5);
        assert!(removed.contains(&format!("models.ini.noevia-backup-{:064x}", 0)));
        assert!(d.path().join("models.ini.noevia-backup-nothex").exists());
        assert!(file.exists());
    }

    #[test]
    fn a_missing_regular_file_is_a_read_error_not_an_empty_file() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path().join("none.ini"), None);
        assert!(s.snapshot().is_err());
        std::fs::write(d.path().join("real.ini"), "[a]\n").unwrap();
        std::os::unix::fs::symlink("real.ini", d.path().join("link.ini")).unwrap();
        let e = Store::new(d.path().join("link.ini"), None)
            .snapshot()
            .unwrap_err();
        assert_eq!(e.status, Some(409));
    }

    /// Core's actual `presets.prepare` over synthetic files (tests/fixtures/preset-prepare-node.json):
    /// the same text, or the same status and message, for every case.
    #[test]
    fn prepare_matches_the_actual_node_store() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/preset-prepare-node.json"
        ))
        .unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert!(cases.len() >= 18);
        for case in cases {
            let d = tempfile::tempdir().unwrap();
            let file = d.path().join("models.ini");
            let ini = case["ini"].as_str().unwrap();
            std::fs::write(&file, ini).unwrap();
            let revision = if case["stale"] == Value::Bool(true) {
                "0".repeat(64)
            } else {
                sha(ini)
            };
            let options: Vec<(String, Option<String>)> = case["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| {
                    (
                        o[0].as_str().unwrap().to_owned(),
                        o[1].as_str().map(str::to_owned),
                    )
                })
                .collect();
            let model = case["model"].as_str().unwrap();
            let got = prepare(&file, model, &revision, &options, (1024.0, 2048.0));
            let want = &case["result"];
            let name = case["name"].as_str().unwrap();
            match got {
                Ok(c) => {
                    assert_eq!(Some(c.text.as_str()), want["text"].as_str(), "{name}");
                    assert_eq!(
                        Some(c.base_revision.as_str()),
                        want["baseRevision"].as_str(),
                        "{name}"
                    );
                    assert_eq!(c.before, ini, "{name}");
                }
                Err(t) => {
                    assert_eq!(Some(t.message.as_str()), want["error"].as_str(), "{name}");
                    assert_eq!(t.status.map(u64::from), want["status"].as_u64(), "{name}");
                }
            }
        }
    }
}
