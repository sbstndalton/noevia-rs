//! `POST /api/models/presets/reload` (core `routes/models.cjs`, `llamacpp-manager.cjs`
//! `reloadPresets` / `routerReload`, `preset-reload.cjs`): ask the llama.cpp router to re-read
//! models.ini, without unloading a model a client is using.
//!
//! The router reads models.ini at start and on `GET /models?reload=1`; a reload keeps every running
//! model whose effective preset is unchanged and unloads every running one whose preset changed or
//! was removed. So a reload with models loaded goes ahead only when all of these hold: this process
//! knows what the router last read (the baseline recorded after the last reload it made), the
//! router's own view of every model's preset is still what it reported then (so it has not re-read
//! the file since), and `preset-reload`'s check finds every loaded model's section, the `[*]`
//! section and the preamble unchanged. Anything unknown fails closed.
//!
//! The baseline lives in `native-preset-reload-<hash>.json` (`{text, view}`, or `{}` for
//! "unknown"). The front is its writer for the reloads it makes; Node's own reloads (the preset
//! editor, the folder sync, delete, prune, idle-unload, auto-tune) still record their baselines,
//! under the same lock file (`<file>.lock`, created exclusively; one holding it longer than
//! [`LOCK_STALE`] is taken over) and with the same atomic replace, and re-read the file on each use
//! (core `preset-reload.cjs`). Neither side keeps a copy in memory across calls.

use super::router::{rows, Router};
use super::{HoldError, Maint};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A lock file older than this belongs to a writer that died.
pub const LOCK_STALE: Duration = Duration::from_secs(30);
const LOCK_WAIT: Duration = Duration::from_secs(5);

const LOADED: &str = "A model is loaded. Unload it to apply the new settings.";
const IN_PROGRESS: &str = "Requests are in progress. Try again when chats finish.";
const NOT_RELOADED: &str = "The engine did not reload its settings. Check the Hardware tab.";
const COULD_NOT: &str = "Could not reload model profiles.";

/// Statuses the router counts as running (server-models.h `is_running()`).
pub fn live_ids(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .filter(|m| {
            matches!(
                m.get("status")
                    .and_then(|s| s.get("value"))
                    .and_then(Value::as_str),
                Some("loaded" | "loading" | "sleeping")
            )
        })
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// `engineView(rows)`: the router's view (model id to its reported preset text) as the canonical
/// string `JSON.stringify(ids.sort().map(id => [id, preset]))`. `None` (unknown) when any row has
/// no string id or `status.preset`.
pub fn engine_view(listing: &[Value]) -> Option<String> {
    // JS `sort()` orders by UTF-16 code unit; the last row for an id wins.
    let mut out: BTreeMap<Vec<u16>, (String, String)> = BTreeMap::new();
    for row in listing {
        let id = row.get("id")?.as_str()?;
        let preset = row.get("status")?.get("preset")?.as_str()?;
        out.insert(
            id.encode_utf16().collect(),
            (id.to_owned(), preset.to_owned()),
        );
    }
    let pairs: Vec<[&str; 2]> = out
        .values()
        .map(|(i, p)| [i.as_str(), p.as_str()])
        .collect();
    serde_json::to_string(&pairs).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Baseline {
    text: String,
    view: String,
}

/// May the router reload now without unloading any loaded model?
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub safe: bool,
    pub reason: String,
}

fn unsafe_because(reason: &str) -> Verdict {
    Verdict {
        safe: false,
        reason: reason.into(),
    }
}

/// The baseline file. Cheap to clone; keeps nothing in memory between calls.
#[derive(Debug, Clone)]
pub struct Guard {
    file: Option<PathBuf>,
}

impl Guard {
    pub fn new(file: Option<PathBuf>) -> Guard {
        Guard { file }
    }

    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// The saved baseline, or `None` (unknown): no file, unreadable, or the `{}` tombstone.
    fn load(&self) -> Option<Baseline> {
        let raw = std::fs::read_to_string(self.file.as_ref()?).ok()?;
        let v: Value = serde_json::from_str(&raw).ok()?;
        Some(Baseline {
            text: v.get("text")?.as_str()?.to_owned(),
            view: v.get("view")?.as_str()?.to_owned(),
        })
    }

    fn persist(&self, baseline: Option<&Baseline>) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        // Forgetting writes a tombstone instead of deleting, so a failed delete can never leave an
        // old baseline behind for the next start (#1040).
        let body = match baseline {
            Some(b) => json!({ "text": b.text, "view": b.view }).to_string(),
            None => "{}".to_owned(),
        };
        if let Err(e) = write_locked(file, body.as_bytes()) {
            eprintln!("[models] could not save the preset reload baseline: {e}");
        }
    }

    /// After a reload made here: the text read just before it and the router's rows just after.
    /// `after` is the file re-read after the rows; if it moved, the router may have read either.
    pub fn record(&self, before: Option<&str>, listing: &[Value], after: Option<&str>) {
        let view = engine_view(listing);
        let baseline = match (before, after, view) {
            (Some(b), Some(a), Some(view)) if b == a => Some(Baseline {
                text: b.to_owned(),
                view,
            }),
            _ => None,
        };
        self.persist(baseline.as_ref());
    }

    pub fn forget(&self) {
        self.persist(None);
    }

    pub fn known(&self) -> bool {
        self.load().is_some()
    }

    /// The models.ini text the router last read, while that is still knowable.
    pub fn router_text(&self, listing: &[Value]) -> Option<String> {
        let b = self.load()?;
        (engine_view(listing).as_deref() == Some(b.view.as_str())).then_some(b.text)
    }

    /// `verdict(currentText, rows, loadedIds)`.
    pub fn verdict(
        &self,
        current: Option<&str>,
        listing: Option<&[Value]>,
        loaded: &[String],
    ) -> Verdict {
        let Some(base) = self.load() else {
            return unsafe_because("unknown");
        };
        let Some(view) = listing.and_then(engine_view) else {
            return unsafe_because("unknown");
        };
        if view != base.view {
            return unsafe_because("engine_moved");
        }
        let Some(current) = current else {
            return unsafe_because("unreadable");
        };
        let request =
            json!({ "baseline": base.text, "current": current, "loaded": loaded }).to_string();
        let (status, reply) = preset_reload::check_json(&request);
        if status != 0 {
            eprintln!("[models] preset reload check failed closed: {reply}");
            return unsafe_because("check_failed");
        }
        let v: Value = serde_json::from_str(&reply).unwrap_or(Value::Null);
        if v.get("safe") == Some(&Value::Bool(true)) {
            return Verdict {
                safe: true,
                reason: "unchanged".into(),
            };
        }
        let reason = v
            .get("reason")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
            .unwrap_or("changed");
        unsafe_because(reason)
    }
}

/// The lock file beside a state file.
fn lock_path(file: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", file.display()))
}

struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn take_lock(file: &Path) -> std::io::Result<Lock> {
    let path = lock_path(file);
    let started = Instant::now();
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                let _ = write!(f, "{}", std::process::id());
                return Ok(Lock(path));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > LOCK_STALE);
                if stale {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if started.elapsed() > LOCK_WAIT {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Atomic replace of `file` with `bytes` (0600) while holding its lock file.
fn write_locked(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let _lock = take_lock(file)?;
    let temp = PathBuf::from(format!("{}.{}.tmp", file.display(), std::process::id()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&temp, file)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// The answer: status and the JSON text, in Node's key order.
pub struct Outcome {
    pub status: u16,
    pub body: String,
}

fn out(status: u16, body: &Value) -> Outcome {
    Outcome {
        status,
        body: body.to_string(),
    }
}

fn message(status: u16, text: &str) -> Outcome {
    out(status, &json!({ "error": text }))
}

fn refuse(ids: &[String], reason: Option<&str>) -> Outcome {
    let mut body = format!("{{\"error\":{},\"loaded\":{}", json!(LOADED), json!(ids));
    if let Some(r) = reason {
        body.push_str(&format!(",\"reason\":{}", json!(r)));
    }
    body.push('}');
    Outcome { status: 409, body }
}

/// A router answer that is not ok, passed on as the route does (`json(res, status, body)`).
fn passed_on(status: u16, body: &Option<Value>) -> Outcome {
    Outcome {
        status,
        body: body.clone().unwrap_or(Value::Null).to_string(),
    }
}

impl Maint {
    /// The models.ini text now, or `None` when it cannot be read (no preset path, a symlink, a
    /// duplicate section, over the limit).
    pub(super) async fn ini_text(&self) -> Option<String> {
        let store = self.ini.clone()?;
        tokio::task::spawn_blocking(move || store.snapshot().ok().map(|s| s.text))
            .await
            .ok()
            .flatten()
    }

    pub(super) async fn guard_op<T: Send + 'static>(
        &self,
        f: impl FnOnce(Guard) -> T + Send + 'static,
    ) -> Option<T> {
        let g = self.guard.clone();
        tokio::task::spawn_blocking(move || f(g)).await.ok()
    }

    /// `routerReload(120000, { expectText })`: `Ok(Ok(reply))` or `Ok(Err(refused))`; `Err` when the
    /// request threw (the baseline is forgotten first).
    async fn router_reload(
        &self,
        router: &Router,
        hold: Option<&str>,
        expect: Option<&str>,
    ) -> Result<Result<super::router::Reply, &'static str>, ()> {
        let before = self.ini_text().await;
        if let Some(expect) = expect {
            if before.as_deref() != Some(expect) {
                return Ok(Err("file_changed"));
            }
        }
        let result = router
            .get("/models?reload=1", hold, Duration::from_secs(120))
            .await;
        let reply = match result {
            Ok(r) => r,
            Err(_) => {
                self.guard_op(|g| g.forget()).await;
                return Err(());
            }
        };
        if !reply.ok {
            self.guard_op(|g| g.forget()).await;
            return Ok(Ok(reply));
        }
        match router.models(hold).await {
            Ok(after) if after.ok => {
                let after_text = self.ini_text().await;
                match (rows(&after.body), before) {
                    (Some(listing), Some(before)) => {
                        let listing = listing.clone();
                        self.guard_op(move |g| {
                            g.record(Some(&before), &listing, after_text.as_deref())
                        })
                        .await;
                    }
                    _ => {
                        self.guard_op(|g| g.forget()).await;
                    }
                }
            }
            _ => {
                self.guard_op(|g| g.forget()).await;
            }
        }
        Ok(Ok(reply))
    }

    /// `reloadPresets({ unload })`, under the maintenance hold. Runs to its end whatever happens to
    /// the client that asked: callers spawn it.
    pub async fn reload_presets(&self, unload: bool) -> Outcome {
        let Some(router) = self.router.as_ref() else {
            return message(404, "This engine does not use a preset file");
        };
        let hold = match self.acquire("", Duration::from_secs(300), false).await {
            Ok(h) => h,
            Err(HoldError::Busy) => return message(409, IN_PROGRESS),
            Err(HoldError::Gate { status, message: m }) => return message(status, &m),
            Err(HoldError::NeedsGate) => return message(500, COULD_NOT),
        };
        let token = hold.token();
        let held = token.as_deref();
        let outcome = self.reload_inner(router, held, unload).await;
        hold.release().await;
        outcome
    }

    async fn reload_inner(&self, router: &Router, hold: Option<&str>, unload: bool) -> Outcome {
        let listing = match router.models(hold).await {
            Ok(l) => l,
            Err(_) => return message(500, COULD_NOT),
        };
        if !listing.ok {
            return passed_on(listing.status, &listing.body);
        }
        let first: Vec<Value> = rows(&listing.body).cloned().unwrap_or_default();
        let loaded = live_ids(&first);
        let mut kept: Vec<String> = Vec::new();
        let mut expect: Option<String> = None;
        if !loaded.is_empty() && !unload {
            let judged = self.ini_text().await;
            let v = self
                .verdict_of(judged.clone(), Some(first.clone()), loaded.clone())
                .await;
            if !v.safe {
                return refuse(&loaded, Some(&v.reason));
            }
            // Right before the call: list again; a model that started loading meanwhile, or a
            // moved router, refuses (#1040). The file itself is re-read inside the reload.
            let again = match router.models(hold).await {
                Ok(a) if a.ok => a,
                _ => return refuse(&loaded, Some("unknown")),
            };
            let Some(again_rows) = rows(&again.body).cloned() else {
                return refuse(&loaded, Some("unknown"));
            };
            let now = live_ids(&again_rows);
            if now.iter().any(|id| !loaded.contains(id)) {
                return refuse(&now, Some("loaded_changed"));
            }
            let recheck = self
                .verdict_of(judged.clone(), Some(again_rows), now.clone())
                .await;
            if !recheck.safe {
                return refuse(&now, Some(&recheck.reason));
            }
            kept = now;
            expect = judged;
        }
        if kept.is_empty() {
            for id in &loaded {
                let _ = router.unload(id, hold).await;
            }
        }
        let result = match self.router_reload(router, hold, expect.as_deref()).await {
            Ok(r) => r,
            Err(()) => return message(500, COULD_NOT),
        };
        match result {
            Err(why) => refuse(&kept, Some(why)),
            Ok(reply) if !reply.ok => message(502, NOT_RELOADED),
            Ok(_) => {
                let unloaded: Vec<String> = if kept.is_empty() { loaded } else { Vec::new() };
                let mut body = format!("{{\"reloaded\":true,\"unloaded\":{}", json!(unloaded));
                if !kept.is_empty() {
                    body.push_str(&format!(",\"kept\":{}", json!(kept)));
                }
                body.push('}');
                Outcome { status: 200, body }
            }
        }
    }

    async fn verdict_of(
        &self,
        current: Option<String>,
        listing: Option<Vec<Value>>,
        loaded: Vec<String>,
    ) -> Verdict {
        self.guard_op(move |g| g.verdict(current.as_deref(), listing.as_deref(), &loaded))
            .await
            .unwrap_or_else(|| unsafe_because("unknown"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn row(id: &str, status: &str, preset: &str) -> Value {
        json!({"id": id, "status": {"value": status, "preset": preset}})
    }

    #[test]
    fn live_models_are_loaded_loading_and_sleeping() {
        let r = vec![
            row("a", "loaded", "x"),
            row("b", "unloaded", "x"),
            row("c", "sleeping", "x"),
            row("d", "loading", "x"),
            json!({"id": "e"}),
            Value::Null,
        ];
        assert_eq!(live_ids(&r), ["a", "c", "d"]);
    }

    #[test]
    fn the_view_is_js_json_of_the_sorted_pairs_and_unknown_on_a_gap() {
        let r = vec![row("b", "loaded", "pb"), row("a", "loaded", "p\"a\n")];
        assert_eq!(engine_view(&r).unwrap(), r#"[["a","p\"a\n"],["b","pb"]]"#);
        assert_eq!(engine_view(&[]).unwrap(), "[]");
        // A later row for the same id wins, as in the JS object.
        let dup = vec![row("a", "loaded", "1"), row("a", "loaded", "2")];
        assert_eq!(engine_view(&dup).unwrap(), r#"[["a","2"]]"#);
        // UTF-16 order, not UTF-8: U+FF5E sorts after U+1F600 in UTF-8, before it in UTF-16.
        let order = vec![
            row("\u{1F600}", "loaded", "x"),
            row("\u{FF5E}", "loaded", "y"),
        ];
        assert!(engine_view(&order).unwrap().starts_with("[[\"\u{1F600}\""));
        assert!(engine_view(&[json!({"id": "a", "status": {}})]).is_none());
        assert!(engine_view(&[json!({"id": 1, "status": {"preset": "x"}})]).is_none());
    }

    const BASE: &str = "[*]\ncache-ram = 512\n\n[A]\nmodel = /models/a.gguf\n";

    fn guard(dir: &Path) -> Guard {
        Guard::new(Some(dir.join("native-preset-reload-x.json")))
    }

    #[test]
    fn a_recorded_baseline_is_read_back_and_judges_an_unchanged_file_safe() {
        let d = tempfile::tempdir().unwrap();
        let g = guard(d.path());
        let rows = vec![row("A", "loaded", "p")];
        assert!(!g.known());
        g.record(Some(BASE), &rows, Some(BASE));
        assert!(g.known());
        // Another handle on the same file (the other process) sees it: nothing is cached.
        assert!(guard(d.path()).known());
        let loaded = vec!["A".to_owned()];
        assert!(g.verdict(Some(BASE), Some(&rows), &loaded).safe);
        let changed = BASE.replace("a.gguf", "b.gguf");
        let v = g.verdict(Some(&changed), Some(&rows), &loaded);
        assert_eq!((v.safe, v.reason.as_str()), (false, "changed"));
        let moved = vec![row("A", "loaded", "other")];
        assert_eq!(
            g.verdict(Some(BASE), Some(&moved), &loaded).reason,
            "engine_moved"
        );
        assert_eq!(g.verdict(None, Some(&rows), &loaded).reason, "unreadable");
        assert_eq!(g.verdict(Some(BASE), None, &loaded).reason, "unknown");
        assert_eq!(g.router_text(&rows).as_deref(), Some(BASE));
        assert_eq!(g.router_text(&moved), None);
    }

    #[test]
    fn a_file_that_moved_during_the_reload_forgets_the_baseline() {
        let d = tempfile::tempdir().unwrap();
        let g = guard(d.path());
        let rows = vec![row("A", "loaded", "p")];
        g.record(Some(BASE), &rows, Some(BASE));
        g.record(Some(BASE), &rows, Some("[A]\n"));
        assert!(!g.known());
        // The tombstone is `{}`, as Core writes it, and mode 0600.
        let file = d.path().join("native-preset-reload-x.json");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{}");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        g.record(None, &rows, None);
        assert!(!g.known());
    }

    #[test]
    fn a_file_core_wrote_is_the_same_format() {
        let d = tempfile::tempdir().unwrap();
        let g = guard(d.path());
        let rows = vec![row("A", "loaded", "p")];
        let view = engine_view(&rows).unwrap();
        std::fs::write(
            d.path().join("native-preset-reload-x.json"),
            serde_json::to_string(&json!({"text": BASE, "view": view})).unwrap(),
        )
        .unwrap();
        assert!(g.verdict(Some(BASE), Some(&rows), &["A".into()]).safe);
        // Anything else is unknown.
        for junk in ["", "not json", "[]", r#"{"text":1,"view":"x"}"#] {
            std::fs::write(d.path().join("native-preset-reload-x.json"), junk).unwrap();
            assert!(!g.known(), "{junk:?}");
        }
    }

    #[test]
    fn a_stale_lock_is_taken_over_and_a_live_one_is_waited_for() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("s.json");
        let lock = lock_path(&file);
        std::fs::write(&lock, "999999").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(120))
            .unwrap();
        write_locked(&file, b"{}").unwrap();
        assert!(!lock.exists(), "the lock is released");
        // A live lock released by its holder lets the write through.
        std::fs::write(&lock, "1").unwrap();
        let l2 = lock.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            std::fs::remove_file(l2).unwrap();
        });
        write_locked(&file, b"{\"a\":1}").unwrap();
        t.join().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"a\":1}");
    }
}
