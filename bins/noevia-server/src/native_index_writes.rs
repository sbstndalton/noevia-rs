//! Native project index writes (council p6, continuing p1-4/p4-2's `project-index` crate): the three
//! writes core `projects.cjs` makes to a project's index (`rag.cjs` `indexProjectFile`,
//! `appendFileTail`, `deleteProjectFile`), answered here instead of in Node. Switch
//! `NOEVIA_RUST_INDEX_WRITES` (its own: `NOEVIA_RUST_RETRIEVAL` already serves the read side):
//! - `off` (the default; unset or empty too): every request defers, Node writes as before;
//! - `shadow`: Node writes the live index; the native writer runs the same write into a side copy
//!   (`<account>/rag-shadow/<project>.db`, never the live file) and answers its outcome, which Node's
//!   client compares with its own. The shadow never kicks the embedding migration;
//! - `on`: the native writer writes the live index (`<account>/rag/<project>.db`) and kicks the native
//!   embedding worker when a file's queue drains after an index run (`onIndexed`).
//!
//! The front's facts behind the crate's [`Host`]:
//! - the account: its `users` row exists and is not disabled (`userActive`), read per check;
//! - the project lifecycle: the account's index outbox (`index-outbox.db`, core
//!   `rust-index-outbox.cjs`): the generation is `meta.generation`, and any tombstone of the project
//!   means deleted, whenever the run started (project ids are never revived);
//! - the active embedding target and whether the admin turned the service off: the saved embedding
//!   configuration and the native worker's last snapshot, as native retrieval reads them;
//! - `foregroundBusy`: the native foreground counter (`embedding_worker::foreground`);
//! - the vectors: one `/v1/embeddings` request per batch to the target's own address over the
//!   worker's guarded HTTP (`project_index::http_embed`).
//!
//! Defers to Node (`{defer: reason}`): the switch is off, no account (the shared `RAG_DIR` index),
//! no data directory, the embedding configuration cannot be read, or the active target has no address
//! of its own (it embeds on the shared inference engine, whose admission is Node's).
//!
//! Named differences from Node:
//! - an unreadable outbox counts as the project deleted (Node's guard reads that as not deleted): a
//!   write is refused rather than risk writing a deleted project's index;
//! - Node's own foreground (a Node chat or search in flight) is not visible here; only the native
//!   counter is;
//! - Node's in-process wanted versions are not this process's: while `on`, the native searches (Node's
//!   through `/v1/retrieval/search`, and research) add this writer's ([`IndexWrites::search_versions`]);
//!   Node's own `searchProjectLocal` still filters by its own only, so the two switches flip together;
//! - those of `project_index` (`http_embed`, `request`).
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use embedding_worker::{http::Http, runtime, Config as EmbeddingConfig};
use project_index::embed::DocumentEmbedder;
use project_index::http_embed::{Endpoint, HttpEmbed};
use project_index::indexer::{Host, Indexer};
use project_index::slots::Target;
use project_retrieval::policy::{current_snapshot, embedding_off, observe, Observation};

/// How long one write may take on the route. An index run of a long file embeds many batches (each
/// up to 60 s); Node's own write has no deadline. On expiry the answer is a failure, never a
/// deferral: the native run may still finish, so Node must not write the same file beside it.
pub const STORE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(600);

/// The index directory of the shadow copy, beside the account's live `rag`.
pub const SHADOW_DIR: &str = "rag-shadow";

/// `NOEVIA_RUST_INDEX_WRITES`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Off,
    Shadow,
    On,
}

impl Mode {
    /// Case does not count; anything but off, shadow or on refuses the start, as the other switches do.
    pub fn parse(value: Option<&str>) -> Result<Mode, String> {
        match value
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "off" => Ok(Mode::Off),
            "shadow" => Ok(Mode::Shadow),
            "on" => Ok(Mode::On),
            _ => Err("NOEVIA_RUST_INDEX_WRITES must be off, shadow or on".to_owned()),
        }
    }
}

/// `[a-zA-Z0-9_-]`-ish ids only: never a path (as native retrieval and the embedding worker check).
pub fn safe_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('.') && !id.contains(['/', '\\', '\0'])
}

/// The front's side of the indexer.
pub struct FrontHost {
    data_dir: PathBuf,
    /// `RAG_DIR` (the shared index of a write without an account; the route defers those).
    rag_dir: Option<PathBuf>,
    /// The environment's embedding target, used while no saved configuration exists.
    seed: Option<EmbeddingConfig>,
    service_url: Option<String>,
    start: String,
    shadow: bool,
    observed: Mutex<Option<Observation>>,
    kick: OnceLock<embedding_worker::Control>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> f64 {
    crate::identity::now_ms() as f64
}

/// What the account's index outbox says: `(generation, project tombstoned)`; `None` when it exists
/// but cannot be read. No outbox: generation 0, nothing deleted. Read-only, never created.
pub fn outbox_facts(user_dir: &Path, project: &str) -> Option<(u64, bool)> {
    use server_store::rusqlite::{Connection, OpenFlags, OptionalExtension};
    let path = server_projects::outbox::path(user_dir);
    if !path.exists() {
        return Some((0, false));
    }
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    db.busy_timeout(std::time::Duration::from_secs(5)).ok()?;
    let generation: i64 = db
        .query_row("SELECT value FROM meta WHERE key = 'generation'", [], |r| {
            r.get(0)
        })
        .optional()
        .ok()?
        .unwrap_or(0);
    let dead: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM tombstones WHERE project = ?1)",
            [project],
            |r| r.get(0),
        )
        .ok()?;
    Some((u64::try_from(generation).unwrap_or(0), dead))
}

impl FrontHost {
    pub fn new(
        data_dir: PathBuf,
        rag_dir: Option<PathBuf>,
        seed: Option<EmbeddingConfig>,
        service_url: Option<String>,
        start: String,
        shadow: bool,
    ) -> FrontHost {
        FrontHost {
            data_dir,
            rag_dir,
            seed,
            service_url,
            start,
            shadow,
            observed: Mutex::new(None),
            kick: OnceLock::new(),
        }
    }

    /// The saved embedding configuration (or the environment's while none is saved).
    pub fn config(&self) -> Result<EmbeddingConfig, String> {
        let seed = self.seed.clone().unwrap_or_else(runtime::invalid_seed);
        runtime::load_config(&self.data_dir, &seed)
    }

    fn user_dir(&self, user: &str) -> Option<PathBuf> {
        server_projects::store::user_dir(&self.data_dir, user).ok()
    }

    /// `observeNative()`: the embedding service as the native worker last reported it.
    fn observation(&self) -> Option<Observation> {
        let status = std::fs::read(self.data_dir.join("embedding-worker-status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
        let now = now_ms();
        let snapshot = status
            .as_ref()
            .and_then(|s| current_snapshot(s, &self.start, now));
        let mut observed = lock(&self.observed);
        *observed = observe(observed.take(), snapshot, now);
        observed.clone()
    }
}

impl Host for FrontHost {
    fn index_path(&self, project: &str, user: Option<&str>) -> PathBuf {
        let dir = if self.shadow { SHADOW_DIR } else { "rag" };
        // A bad id gets a path under a directory that never holds an index (the route refuses
        // those before; this is the last guard).
        if !safe_id(project) {
            return self.data_dir.join(".refused").join("invalid.db");
        }
        match user {
            Some(u) => match self.user_dir(u) {
                Some(d) => d.join(dir).join(format!("{project}.db")),
                None => self.data_dir.join(".refused").join("invalid.db"),
            },
            None if self.shadow => self.data_dir.join(SHADOW_DIR).join(format!("{project}.db")),
            None => self
                .rag_dir
                .clone()
                .unwrap_or_else(|| self.data_dir.join("rag"))
                .join(format!("{project}.db")),
        }
    }

    fn user_active(&self, user: &str) -> bool {
        crate::project_file_read::account_active(&self.data_dir, user)
    }

    fn lifecycle_generation(&self, user: Option<&str>) -> u64 {
        let Some(dir) = user.and_then(|u| self.user_dir(u)) else {
            return 0;
        };
        // `rust-index-outbox.cjs` `generation`: a project id is enough to name its tombstone; the
        // generation itself only orders runs.
        outbox_facts(&dir, "").map_or(0, |(g, _)| g)
    }

    fn deleted_since(&self, project: &str, user: Option<&str>, _seen: u64) -> bool {
        let Some(user) = user else { return false };
        let Some(dir) = self.user_dir(user) else {
            return true;
        };
        // Named difference: unreadable counts as deleted.
        outbox_facts(&dir, project).is_none_or(|(_, dead)| dead)
    }

    fn active_target(&self) -> Target {
        match self.config() {
            Ok(c) => Target {
                profile: c.active.profile,
                model: c.active.model,
            },
            // The route defers before an unreadable configuration reaches the indexer.
            Err(_) => Target {
                profile: String::new(),
                model: String::new(),
            },
        }
    }

    fn embedding_off(&self, target: &Target) -> bool {
        let base = self.config().ok().and_then(|c| {
            (c.active.profile == target.profile && c.active.model == target.model)
                .then_some(c.active.base_url)
                .flatten()
        });
        embedding_off(
            self.observation().as_ref(),
            now_ms(),
            self.service_url.as_deref(),
            base.as_deref(),
        )
    }

    fn foreground_busy(&self) -> bool {
        embedding_worker::foreground::busy()
    }

    fn rag_available(&self) -> bool {
        embedding_worker::sqlite_vec_loader::register().is_ok()
    }

    fn after_index(&self) {
        if self.shadow {
            return;
        }
        if let Some(control) = self.kick.get() {
            control.kick();
        }
    }
}

/// The process's native index writer.
pub struct IndexWrites {
    pub mode: Mode,
    indexer: Option<Arc<Indexer<FrontHost>>>,
    http: Option<Http>,
}

impl std::fmt::Debug for IndexWrites {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexWrites")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl IndexWrites {
    pub fn off() -> IndexWrites {
        IndexWrites {
            mode: Mode::Off,
            indexer: None,
            http: None,
        }
    }

    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<IndexWrites, String> {
        let mode = Mode::parse(get("NOEVIA_RUST_INDEX_WRITES").as_deref())?;
        if mode == Mode::Off {
            return Ok(IndexWrites::off());
        }
        let Some(data_dir) = get("UI_DATA_DIR").filter(|s| !s.is_empty()) else {
            eprintln!("noevia-server: native index writes have no data directory; every write defers to Node");
            return Ok(IndexWrites {
                mode,
                indexer: None,
                http: None,
            });
        };
        let inference = get("INFERENCE_BASE_URL")
            .filter(|s| !s.is_empty())
            .or_else(|| get("LEMONADE_BASE_URL").filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "http://host.docker.internal:11434".into());
        // Only targets with their own address are written natively, so no inference key.
        let http = Http::new("", None, &inference, None).ok();
        if http.is_none() {
            eprintln!("noevia-server: native index writes have no embedding client; every write defers to Node");
        }
        let host = FrontHost::new(
            PathBuf::from(data_dir),
            get("RAG_DIR").filter(|s| !s.is_empty()).map(PathBuf::from),
            runtime::seed(get).ok(),
            get("EMBEDDING_SERVICE_URL"),
            get("NOEVIA_START_ID").unwrap_or_default(),
            mode == Mode::Shadow,
        );
        Ok(IndexWrites {
            mode,
            indexer: Some(Arc::new(Indexer::new(host))),
            http,
        })
    }

    /// The native embedding worker's control, kicked after an index run (`on` only). Set once, at boot.
    pub fn set_kick(&self, control: embedding_worker::Control) {
        if let Some(ix) = &self.indexer {
            let _ = ix.host().kick.set(control);
        }
    }

    /// The versions a search must filter by (`fileVersions`) for one project, while this process owns
    /// the writes.
    pub fn wanted_versions(
        &self,
        project: &str,
        user: Option<&str>,
    ) -> Vec<(String, Option<String>)> {
        self.indexer
            .as_ref()
            .map(|ix| ix.wanted_versions(project, user))
            .unwrap_or_default()
    }

    /// What a native search must filter by besides the caller's: this writer's wanted versions while
    /// it owns the live index (`on`), none otherwise (Node's own come with its request; a shadow
    /// writes only its side copy).
    pub fn search_versions(
        &self,
        project: &str,
        user: &str,
    ) -> project_retrieval::search::Versions {
        if self.mode != Mode::On {
            return Default::default();
        }
        self.wanted_versions(project, Some(user))
            .into_iter()
            .collect()
    }

    /// One write, on the calling (blocking) thread: `{defer: reason}` when Node must write it.
    pub fn answer_blocking(
        &self,
        handle: tokio::runtime::Handle,
        op: &str,
        body: &serde_json::Value,
    ) -> serde_json::Value {
        use serde_json::{json, Value};
        let defer = |why: &str| json!({ "defer": why });
        if self.mode == Mode::Off {
            return defer("off");
        }
        let (Some(ix), Some(http)) = (&self.indexer, &self.http) else {
            return defer("unavailable");
        };
        let user = match body.get("userId") {
            Some(Value::String(u)) if !u.is_empty() => u.as_str(),
            Some(Value::String(_) | Value::Null) | None => return defer("shared index"),
            Some(_) => return json!({ "ok": false, "reason": "bad-request" }),
        };
        let project = body.get("projectId").and_then(Value::as_str).unwrap_or("");
        if !safe_id(project) || ix.host().user_dir(user).is_none() {
            return json!({ "ok": false, "reason": "bad-request" });
        }
        let Ok(config) = ix.host().config() else {
            return defer("embedding configuration unavailable");
        };
        let Some(base) = config.active.base_url.clone().filter(|b| !b.is_empty()) else {
            return defer("shared engine target");
        };
        let embed = HttpEmbed::new(http.clone(), handle, move |_: &Target| {
            Ok(Endpoint::Explicit(base.clone()))
        });
        let host = ix.host();
        let mut embedder = DocumentEmbedder {
            call: |t: &Target, input: &[String]| embed.call(t, input),
            off: |t: &Target| host.embedding_off(t),
        };
        project_index::request::answer(&**ix, op, body, &mut embedder)
    }
}

/// `POST /v1/index/{file,tail,delete}` on the store listener (`--features` line
/// `rust-index-writes-v1`): Node's `projects.cjs` writes through a core client when its
/// `NOEVIA_RUST_INDEX_WRITES` says so. The body is `project_index::request`'s; the answer is the
/// write's outcome as Node returns it, `{ok: false, reason: "bad-request"}`, or `{defer: reason}`, on
/// which Node writes itself. The caller has checked the per-start secret.
pub async fn store_answer(app: Arc<crate::App>, op: &'static str, raw: &[u8]) -> (u16, String) {
    use serde_json::{json, Value};
    let Ok(body) = serde_json::from_slice::<Value>(raw) else {
        return (400, json!({ "error": "invalid JSON" }).to_string());
    };
    if app.config.index_writes.mode == Mode::Off {
        return (200, json!({ "defer": "off" }).to_string());
    }
    let handle = tokio::runtime::Handle::current();
    let writes = Arc::clone(&app.config.index_writes);
    let run = tokio::task::spawn_blocking(move || writes.answer_blocking(handle, op, &body));
    match tokio::time::timeout(STORE_DEADLINE, run).await {
        Ok(Ok(answer)) => (200, answer.to_string()),
        Ok(Err(_)) => (200, json!({ "ok": false, "reason": "failed" }).to_string()),
        // Never a deferral: the run goes on and Node must not write the same file beside it.
        Err(_) => (
            200,
            json!({ "ok": false, "reason": "deadline" }).to_string(),
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_set_and_refuses_anything_else() {
        assert_eq!(Mode::parse(None), Ok(Mode::Off));
        assert_eq!(Mode::parse(Some(" ")), Ok(Mode::Off));
        assert_eq!(Mode::parse(Some("OFF")), Ok(Mode::Off));
        assert_eq!(Mode::parse(Some("Shadow")), Ok(Mode::Shadow));
        assert_eq!(Mode::parse(Some("on")), Ok(Mode::On));
        assert!(Mode::parse(Some("1")).is_err());
        let off = IndexWrites::from_lookup(&|_: &str| None).unwrap();
        assert_eq!(off.mode, Mode::Off);
        assert!(off.indexer.is_none());
    }

    #[test]
    fn only_a_writer_that_owns_the_live_index_adds_wanted_versions() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_str().unwrap().to_owned();
        let user = "0123abcd-0000-4000-8000-0000000000a2";
        for (mode, owns) in [("off", false), ("shadow", false), ("on", true)] {
            let env = |k: &str| match k {
                "NOEVIA_RUST_INDEX_WRITES" => Some(mode.to_owned()),
                "UI_DATA_DIR" => Some(data.clone()),
                _ => None,
            };
            let w = IndexWrites::from_lookup(&env).unwrap();
            assert_eq!(w.mode != Mode::Off, w.indexer.is_some(), "{mode}");
            // A delete makes no version current: the file is filtered until its rows are gone.
            if let Some(ix) = &w.indexer {
                let _ = ix.delete_file("qa-p", "qa.md", Some(user), false);
            }
            let versions = w.search_versions("qa-p", user);
            assert_eq!(versions.contains_key("qa.md"), owns, "{mode}: {versions:?}");
            assert!(w.search_versions("qa-other", user).is_empty());
        }
    }

    #[test]
    fn ids_that_could_name_a_path_are_not_safe() {
        for bad in ["", ".", "..", ".x", "a/b", "a\\b", "a\0b"] {
            assert!(!safe_id(bad), "{bad:?}");
        }
        assert!(safe_id("qa-proj-1"));
    }

    #[test]
    fn the_outbox_names_the_generation_and_any_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(outbox_facts(dir.path(), "qa-p"), Some((0, false)));
        let db =
            server_store::rusqlite::Connection::open(server_projects::outbox::path(dir.path()))
                .unwrap();
        db.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             CREATE TABLE tombstones (project TEXT PRIMARY KEY, generation INTEGER NOT NULL, at INTEGER NOT NULL);
             INSERT INTO meta VALUES ('generation', 7);
             INSERT INTO tombstones VALUES ('qa-gone', 3, 1);",
        )
        .unwrap();
        assert_eq!(outbox_facts(dir.path(), "qa-p"), Some((7, false)));
        assert_eq!(outbox_facts(dir.path(), "qa-gone"), Some((7, true)));
        // Unreadable: a file that is not a database.
        let bad = tempfile::tempdir().unwrap();
        std::fs::write(server_projects::outbox::path(bad.path()), b"not sqlite").unwrap();
        assert_eq!(outbox_facts(bad.path(), "qa-p"), None);
    }

    #[test]
    fn shadow_and_live_paths_never_meet() {
        let dir = tempfile::tempdir().unwrap();
        let user = "0123abcd-0000-4000-8000-0000000000a2";
        let live = FrontHost::new(dir.path().into(), None, None, None, String::new(), false);
        let shadow = FrontHost::new(dir.path().into(), None, None, None, String::new(), true);
        let a = live.index_path("qa-p", Some(user));
        let b = shadow.index_path("qa-p", Some(user));
        assert!(a.ends_with(Path::new("rag").join("qa-p.db")), "{a:?}");
        assert!(b.ends_with(Path::new(SHADOW_DIR).join("qa-p.db")), "{b:?}");
        assert_eq!(a.parent().unwrap().parent(), b.parent().unwrap().parent());
        assert!(live
            .index_path("../x", Some(user))
            .starts_with(dir.path().join(".refused")));
    }
}
