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

/// llama.cpp's preset grammar is `ident ws "=" ws value`, keys are case-sensitive, and an
/// argument name may keep its leading dashes. `key: value` is not a llama.cpp assignment.
fn is_key_line(line: &str) -> bool {
    let Some((key, _)) = line.trim_start().split_once('=') else {
        return false;
    };
    let key = key.trim_end();
    let key = key
        .strip_prefix("--")
        .or_else(|| key.strip_prefix('-'))
        .unwrap_or(key);
    key == KEY
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

/// The Python-owned routes that write models.ini and answer with its `revision` (api.py:
/// `PUT /models-ini`, `PUT /sections/{name}`, `POST /sections/{name}/safe-defaults|rename`,
/// `DELETE /sections/{name}`). No other route is reconciled or has its response touched.
pub fn writes_models_ini(method: &str, path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/v1/") else {
        return false;
    };
    let segments: Vec<&str> = rest.split('/').collect();
    let named = |name: &&str| !name.is_empty();
    match (method, segments.as_slice()) {
        ("PUT", ["models-ini"]) => true,
        ("PUT" | "DELETE", ["sections", name]) => named(name),
        ("POST", ["sections", name, "safe-defaults" | "rename"]) => named(name),
        _ => false,
    }
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

/// Attempts per reconcile when another writer keeps winning the race.
const ATTEMPTS: u32 = 3;

/// Test seams: where a concurrent writer is simulated, and whether the atomic exchange is used.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stage {
    BeforeCommit,
    AfterExchange,
}

enum Round {
    Done(Option<String>),
    Lost,
}

/// Never creates the preset: a missing file means the chat router is not configured yet.
///
/// The Python editor has no cross-process lock (its `WRITE_LOCK` is an in-process `RLock`), so a
/// round is made safe on its own: the new file is swapped in with an atomic exchange
/// (`RENAME_EXCHANGE` / `RENAME_SWAP`) and the file it displaced is compared with what was read.
/// Anything else means another writer won: the exchange is undone so the newest text stays, and
/// the round is retried (at most [`ATTEMPTS`] times). Where exchange is unsupported, the file's
/// bytes are compared right before an ordinary rename (a much smaller window).
pub fn reconcile_file(path: &Path, setting: Option<u32>) -> Result<Option<String>, &'static str> {
    reconcile_with(path, setting, true, &mut |_, _| {})
}

fn reconcile_with(
    path: &Path,
    setting: Option<u32>,
    exchange: bool,
    hook: &mut dyn FnMut(u32, Stage),
) -> Result<Option<String>, &'static str> {
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(25 * u64::from(attempt)));
        }
        match round(path, setting, exchange, &mut |stage| hook(attempt, stage))? {
            Round::Done(revision) => return Ok(revision),
            Round::Lost => continue,
        }
    }
    Err("chat preset kept changing; idle unload key not written this time")
}

fn round(
    path: &Path,
    setting: Option<u32>,
    exchange: bool,
    hook: &mut dyn FnMut(Stage),
) -> Result<Round, &'static str> {
    let Some(before) = read_capped(path)? else {
        return Ok(Round::Done(None));
    };
    let text = std::str::from_utf8(&before).map_err(|_| "chat preset is not UTF-8")?;
    let Some(next) = apply(text, setting) else {
        return Ok(Round::Done(None));
    };
    let revision = hex(&Sha256::digest(next.as_bytes()));
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
    hook(Stage::BeforeCommit);
    if exchange && swap(tmp.path(), path).is_ok() {
        hook(Stage::AfterExchange);
        // `tmp` now names the file we displaced; dropping it removes whatever it holds.
        if read_capped(tmp.path())?.as_deref() == Some(before.as_slice()) {
            return Ok(Round::Done(Some(revision)));
        }
        // Lost: put the displaced text back. If a write also landed after our exchange, the
        // swap brings back the older text, so swap once more to keep the newest.
        swap(tmp.path(), path).map_err(|_| "chat preset restore failed")?;
        if read_capped(tmp.path())?.as_deref() != Some(next.as_bytes()) {
            swap(tmp.path(), path).map_err(|_| "chat preset restore failed")?;
        }
        return Ok(Round::Lost);
    }
    if read_capped(path)?.as_deref() != Some(before.as_slice()) {
        return Ok(Round::Lost);
    }
    tmp.persist(path).map_err(|_| "chat preset write failed")?;
    Ok(Round::Done(Some(revision)))
}

fn swap(a: &Path, b: &Path) -> rustix::io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        a,
        rustix::fs::CWD,
        b,
        rustix::fs::RenameFlags::EXCHANGE,
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn only_models_ini_save_routes_are_reconciled() {
        for (method, path) in [
            ("PUT", "/api/v1/models-ini"),
            ("PUT", "/api/v1/sections/gemma"),
            ("DELETE", "/api/v1/sections/gemma"),
            ("POST", "/api/v1/sections/gemma/safe-defaults"),
            ("POST", "/api/v1/sections/gemma/rename"),
        ] {
            assert!(writes_models_ini(method, path), "{method} {path}");
        }
        for (method, path) in [
            ("GET", "/api/v1/models-ini"),
            ("PUT", "/api/v1/settings"),
            ("PUT", "/api/v1/badges"),
            ("POST", "/api/v1/prompts"),
            ("POST", "/api/v1/benchmark/start"),
            ("POST", "/api/v1/models/delete"),
            ("POST", "/api/v1/sections/gemma/draft-heads/download"),
            ("PUT", "/api/v1/sections/"),
            ("PUT", "/api/v1/models-ini/x"),
        ] {
            assert!(!writes_models_ini(method, path), "{method} {path}");
        }
    }

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
        let text = "[*]\nparallel = 1\n--sleep-idle-seconds=60\n[gemma]\nsleep-idle-seconds = 30\n";
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
    fn only_llama_assignments_match() {
        // `key: value`, other cases and longer names are not llama.cpp's key; they are kept.
        let text = "[*]\nsleep-idle-seconds: 5\nSLEEP-IDLE-SECONDS = 6\nsleep-idle-seconds-x = 7\n";
        assert_eq!(
            apply(text, Some(900)).unwrap(),
            format!("[*]\nsleep-idle-seconds = 900\n{}", &text[4..])
        );
        assert_eq!(
            apply("[*]\n-sleep-idle-seconds = 60\n", Some(900)).unwrap(),
            "[*]\nsleep-idle-seconds = 900\n"
        );
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

    const PY1: &str = "[python-one]\nmodel = one.gguf\n";
    const PY2: &str = "[python-two]\nmodel = two.gguf\n";

    fn race(exchange: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.ini");
        std::fs::write(&path, "[gemma]\n").unwrap();
        let mut hook = |attempt: u32, stage: Stage| {
            if attempt == 0 && stage == Stage::BeforeCommit {
                std::fs::write(&path, PY1).unwrap();
            }
        };
        let revision = reconcile_with(&path, Some(900), exchange, &mut hook)
            .unwrap()
            .unwrap();
        // Python's save is kept and the key is added on the retry; nothing of ours was lost.
        let now = std::fs::read_to_string(&path).unwrap();
        assert_eq!(now, apply(PY1, Some(900)).unwrap());
        assert_eq!(revision, hex(&Sha256::digest(now.as_bytes())));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_save_landing_before_the_exchange_is_kept_and_retried() {
        race(true);
    }

    #[test]
    fn a_save_landing_before_the_fallback_rename_is_kept_and_retried() {
        race(false);
    }

    #[test]
    fn a_save_landing_after_the_exchange_keeps_the_newest_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.ini");
        std::fs::write(&path, "[gemma]\n").unwrap();
        let mut hook = |attempt: u32, stage: Stage| match (attempt, stage) {
            (0, Stage::BeforeCommit) => std::fs::write(&path, PY1).unwrap(),
            (0, Stage::AfterExchange) => std::fs::write(&path, PY2).unwrap(),
            _ => {}
        };
        reconcile_with(&path, Some(900), true, &mut hook).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            apply(PY2, Some(900)).unwrap()
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_writer_that_always_wins_is_never_overwritten() {
        for exchange in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("models.ini");
            std::fs::write(&path, "[gemma]\n").unwrap();
            let mut hook = |attempt: u32, stage: Stage| {
                if stage == Stage::BeforeCommit {
                    std::fs::write(&path, format!("[python-{attempt}]\n")).unwrap();
                }
            };
            assert!(reconcile_with(&path, Some(900), exchange, &mut hook).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "[python-2]\n");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn unreadable_presets_are_errors_not_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.ini");
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(
            reconcile_file(&path, Some(900)),
            Err("chat preset is not UTF-8")
        );
        assert_eq!(std::fs::read(&path).unwrap(), vec![0xff, 0xfe]);
        assert!(reconcile_file(dir.path(), Some(900)).is_err());
    }
}
