//! noevia#1747: idle chat models unload themselves.
//!
//! llama.cpp b11610 has this natively: `--sleep-idle-seconds N` makes a server free its model,
//! KV cache and contexts after N seconds without a task, and the next request reloads it
//! (tools/server/README.md "Sleeping on Idle"). In router mode every preset key is a child
//! argument and the `[*]` section is the global preset every model inherits, so one key there
//! covers every chat model while a model's own section can still override it.
//!
//! The model manager owns that one key. [`apply`] rewrites only the `sleep-idle-seconds` line of
//! the `[*]` section of the CHAT router's preset (`MODELS_INI_PATH`); the embedding router's
//! `embedding.ini` is never touched, so embedding models stay warm (#1717).
//!
//! A busy model is never put to sleep by llama.cpp itself: while any slot is generating, the
//! server posts a `NEXT_RESPONSE` task every loop, which resets the idle timer, and it only
//! checks for sleep when its task queue is empty. The manager never calls unload or reload.
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The setting. Empty or unset means [`DEFAULT_SECONDS`]; `0` disables idle unload.
pub const ENV: &str = "CHAT_MODEL_IDLE_UNLOAD_SECONDS";
pub const DEFAULT_SECONDS: u32 = 900;
/// One week. llama.cpp takes an `int`; anything longer is a typo, not a policy.
pub const MAX_SECONDS: u32 = 7 * 24 * 60 * 60;
/// llama.cpp's own preset key for `--sleep-idle-seconds` (it rejects 0, so 0 removes the key).
pub const KEY: &str = "sleep-idle-seconds";
const GLOBAL_SECTION: &str = "[*]";
const FILE_CAP: u64 = 4 * 1024 * 1024;

/// `Ok(Some(n))`: unload after n idle seconds. `Ok(None)`: disabled.
pub fn parse(raw: Option<&str>) -> Result<Option<u32>, &'static str> {
    let raw = raw.map(str::trim).unwrap_or_default();
    if raw.is_empty() {
        return Ok(Some(DEFAULT_SECONDS));
    }
    if raw.len() > 10 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(
            "CHAT_MODEL_IDLE_UNLOAD_SECONDS must be a whole number of seconds (0 disables)",
        );
    }
    match raw.parse::<u32>() {
        Ok(0) => Ok(None),
        Ok(n) if n <= MAX_SECONDS => Ok(Some(n)),
        _ => Err("CHAT_MODEL_IDLE_UNLOAD_SECONDS must be at most 604800 (one week)"),
    }
}

fn header(line: &str) -> Option<&str> {
    let t = line.trim();
    (t.starts_with('[') && t.ends_with(']')).then_some(t)
}

fn is_key_line(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with(';') || t.starts_with('#') {
        return false;
    }
    let Some((key, _)) = t.split_once(['=', ':']) else {
        return false;
    };
    key.trim().eq_ignore_ascii_case(KEY)
}

/// The preset text with the global idle key set to `setting` (or removed when disabled), or
/// `None` when the text already matches. An existing key is replaced where it stands; every
/// other line, comment and section (including a model's own `sleep-idle-seconds`) is kept.
pub fn apply(text: &str, setting: Option<u32>) -> Option<String> {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let wanted = setting.map(|n| format!("{KEY} = {n}"));
    let mut lines: Vec<String> = Vec::new();
    let mut in_global = false;
    let mut global_at: Option<usize> = None;
    let mut first_header: Option<usize> = None;
    let mut placed = false;
    for line in text.lines() {
        if let Some(h) = header(line) {
            first_header.get_or_insert(lines.len());
            in_global = h == GLOBAL_SECTION;
            if in_global {
                global_at.get_or_insert(lines.len());
            }
        } else if in_global && is_key_line(line) {
            if let (Some(w), false) = (&wanted, placed) {
                lines.push(w.clone());
                placed = true;
            }
            continue;
        }
        lines.push(line.to_owned());
    }
    if let (Some(w), false) = (wanted, placed) {
        match global_at {
            Some(at) => lines.insert(at + 1, w),
            None => {
                // Keys above the first header belong to llama.cpp's default section; keep them.
                let at = first_header.unwrap_or(lines.len());
                let mut block = vec![GLOBAL_SECTION.to_owned(), w];
                if first_header.is_some() {
                    block.push(String::new());
                } else if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                    block.insert(0, String::new());
                }
                lines.splice(at..at, block);
            }
        }
    }
    let mut out = lines.join(newline);
    if !lines.is_empty() && (text.is_empty() || text.ends_with('\n')) {
        out.push_str(newline);
    }
    (out != text).then_some(out)
}

/// Keeps the chat preset's global idle key in line with the setting.
pub struct Reconciler {
    path: PathBuf,
    setting: Option<u32>,
    lock: tokio::sync::Mutex<()>,
}

impl Reconciler {
    pub fn new(path: PathBuf, setting: Option<u32>) -> Self {
        Self {
            path,
            setting,
            lock: tokio::sync::Mutex::new(()),
        }
    }
    pub fn setting(&self) -> Option<u32> {
        self.setting
    }
    /// `Ok(Some(revision))` when the file was rewritten (sha256 hex of the new bytes, the same
    /// revision the Python editor reports), `Ok(None)` when nothing changed or no preset exists.
    pub async fn reconcile(&self) -> Result<Option<String>, &'static str> {
        let _guard = self.lock.lock().await;
        let path = self.path.clone();
        let setting = self.setting;
        tokio::task::spawn_blocking(move || reconcile_file(&path, setting))
            .await
            .map_err(|_| "idle unload reconcile failed")?
    }
}

fn read_capped(path: &Path) -> Result<Option<Vec<u8>>, &'static str> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("chat preset unreadable"),
    };
    let mut bytes = Vec::new();
    file.take(FILE_CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "chat preset unreadable")?;
    if bytes.len() as u64 > FILE_CAP {
        return Err("chat preset exceeded limit");
    }
    Ok(Some(bytes))
}

/// Never creates the preset: a missing file means the chat router is not configured yet.
pub fn reconcile_file(path: &Path, setting: Option<u32>) -> Result<Option<String>, &'static str> {
    let Some(before) = read_capped(path)? else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&before).map_err(|_| "chat preset is not UTF-8")?;
    let Some(next) = apply(text, setting) else {
        return Ok(None);
    };
    let dir = path.parent().ok_or("chat preset has no directory")?;
    let permissions = std::fs::metadata(path)
        .map_err(|_| "chat preset unreadable")?
        .permissions();
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|_| "chat preset write failed")?;
    std::io::Write::write_all(&mut tmp, next.as_bytes()).map_err(|_| "chat preset write failed")?;
    tmp.as_file()
        .sync_all()
        .map_err(|_| "chat preset write failed")?;
    std::fs::set_permissions(tmp.path(), permissions).map_err(|_| "chat preset write failed")?;
    // Another writer (the Python editor) won the race: leave its text alone, retry next time.
    if read_capped(path)?.as_deref() != Some(before.as_slice()) {
        return Ok(None);
    }
    tmp.persist(path).map_err(|_| "chat preset write failed")?;
    Ok(Some(hex(&Sha256::digest(next.as_bytes()))))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn default_is_fifteen_minutes() {
        assert_eq!(DEFAULT_SECONDS, 900);
        assert_eq!(parse(None), Ok(Some(900)));
        assert_eq!(parse(Some("")), Ok(Some(900)));
        assert_eq!(parse(Some("  ")), Ok(Some(900)));
    }

    #[test]
    fn zero_disables() {
        assert_eq!(parse(Some("0")), Ok(None));
        assert_eq!(parse(Some(" 0 ")), Ok(None));
        let text = "[*]\nsleep-idle-seconds = 900\nparallel = 1\n[chat]\nmodel = a.gguf\n";
        assert_eq!(
            apply(text, None).unwrap(),
            "[*]\nparallel = 1\n[chat]\nmodel = a.gguf\n"
        );
        // Disabled and nothing set: the file is left byte for byte.
        assert_eq!(apply("[chat]\nmodel = a.gguf\n", None), None);
    }

    #[test]
    fn rejects_bad_values() {
        for bad in ["-1", "abc", "1.5", "604801", "99999999999", "+5"] {
            assert!(parse(Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(parse(Some("604800")), Ok(Some(604_800)));
        assert_eq!(parse(Some("60")), Ok(Some(60)));
    }

    #[test]
    fn adds_global_section_before_first_model() {
        let text = "; managed\n[gemma]\nmodel = g.gguf\nctx-size = 8192\n";
        assert_eq!(
            apply(text, Some(900)).unwrap(),
            "; managed\n[*]\nsleep-idle-seconds = 900\n\n[gemma]\nmodel = g.gguf\nctx-size = 8192\n"
        );
    }

    #[test]
    fn updates_existing_global_key_only() {
        let text = "[*]\nparallel = 1\nSleep-Idle-Seconds: 60\n[gemma]\nsleep-idle-seconds = 30\n";
        assert_eq!(
            apply(text, Some(900)).unwrap(),
            "[*]\nparallel = 1\nsleep-idle-seconds = 900\n[gemma]\nsleep-idle-seconds = 30\n"
        );
        // Duplicates collapse into the first one.
        assert_eq!(
            apply(
                "[*]\nsleep-idle-seconds = 5\nsleep-idle-seconds = 6\n",
                Some(900)
            )
            .unwrap(),
            "[*]\nsleep-idle-seconds = 900\n"
        );
    }

    #[test]
    fn idempotent_when_already_set() {
        let text = "[*]\nsleep-idle-seconds = 900\n[gemma]\nmodel = g.gguf\n";
        assert_eq!(apply(text, Some(900)), None);
        let once = apply("[gemma]\nmodel = g.gguf\n", Some(120)).unwrap();
        assert_eq!(apply(&once, Some(120)), None);
    }

    #[test]
    fn comments_naming_the_key_are_kept() {
        let text = "[*]\n; sleep-idle-seconds = 5\n";
        assert_eq!(
            apply(text, Some(900)).unwrap(),
            "[*]\nsleep-idle-seconds = 900\n; sleep-idle-seconds = 5\n"
        );
    }

    #[test]
    fn keys_above_the_first_header_stay_in_the_default_section() {
        assert_eq!(
            apply("version = 1\n", Some(900)).unwrap(),
            "version = 1\n\n[*]\nsleep-idle-seconds = 900\n"
        );
        assert_eq!(
            apply("", Some(900)).unwrap(),
            "[*]\nsleep-idle-seconds = 900\n"
        );
    }

    #[test]
    fn crlf_is_preserved() {
        assert_eq!(
            apply("[gemma]\r\nmodel = g.gguf\r\n", Some(900)).unwrap(),
            "[*]\r\nsleep-idle-seconds = 900\r\n\r\n[gemma]\r\nmodel = g.gguf\r\n"
        );
    }

    #[test]
    fn reconcile_writes_once_and_reports_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.ini");
        std::fs::write(&path, "[gemma]\nmodel = g.gguf\n").unwrap();
        let revision = reconcile_file(&path, Some(900)).unwrap().unwrap();
        let written = std::fs::read(&path).unwrap();
        assert_eq!(revision, hex(&Sha256::digest(&written)));
        assert!(String::from_utf8(written)
            .unwrap()
            .starts_with("[*]\nsleep-idle-seconds = 900\n"));
        assert_eq!(reconcile_file(&path, Some(900)).unwrap(), None);
    }

    #[test]
    fn reconcile_never_creates_a_missing_preset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.ini");
        assert_eq!(reconcile_file(&path, Some(900)).unwrap(), None);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_keeps_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.ini");
        std::fs::write(&path, "[gemma]\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        reconcile_file(&path, Some(900)).unwrap().unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }
}
