//! R9 main mode, S4 (`DIARY_RUST_EMBEDDING=on` in the child, which then sends `embedding` in its
//! start-up config): this process owns the Diary's embedding configuration
//! (`embedding-config.json`, the same file and format Python writes), the start-up guard of the
//! seeded embeddings addresses, the re-embedding migration (`diary-retrieval` migration.rs, the port
//! of embedding_migration.py) and `GET/POST /api/internal/embedding`.
//!
//! The Python child keeps answering and indexing with the configuration it FOLLOWS: after every
//! change (an action, a guard refusal or release, a pass) this process writes the effective
//! snapshot (`EmbeddingConfig.get()` plus this run's `epoch`) to `embedding-config.effective.json`
//! beside the state file, atomically; the child reads it whenever the file changed
//! (embedding.py `follow_parent`). In-memory refusals of seeded addresses are part of the snapshot,
//! so the child never uses an address this process refused.
//!
//! Index writes of the migration run under the tenant's writer slot (the child's writer lock is a
//! lease on it, S3) and, for an app-managed index, inside the primary reservation (Python's
//! `tenant_writer.hold` + `index_publication`). A deleted (fenced) tenant's files are skipped.
//!
//! Known differences: replies are JSON-equal to Python's, with keys in sorted order (decision D5);
//! the pass and timers run on one driver thread instead of Python's thread + timers.

use crate::server::{self, App};
use crate::write::{Journal, Refused};
use axum::body::Body;
use axum::http::{HeaderMap, Method, Response};
use diary_answer::embedder::HttpEmbedder;
use diary_answer::model::Config as EngineConfig;
use diary_retrieval::embedding::{
    apply_recheck, check_embedding_address, guard_seeded_addresses, recheck_check, state_path_for,
    system_resolver, AddressCheck, CheckFailure, EmbeddingConfig, LogLevel, LogLine, RecheckNext,
    Source, RECHECK_S, RETRY_DELAYS_S,
};
use diary_retrieval::migration::{Embed, FileGuard, Migration, MigrationError, Options};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const ACTIONS: [&str; 6] = [
    "set-next",
    "clear-next",
    "switch",
    "rollback",
    "finish",
    "reindex",
];
const SLOT_WAIT: Duration = Duration::from_secs(30);

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default)]
struct Driver {
    running: bool,
    again: bool,
    retry_at: Option<Instant>,
    recheck_at: Option<Instant>,
}

pub struct Owner {
    config: &'static Arc<Mutex<EmbeddingConfig>>,
    migration: &'static Migration<'static>,
    effective: PathBuf,
    epoch: String,
    published: Mutex<Option<Value>>,
    driver: Mutex<Driver>,
    wake: Condvar,
}

/// The effective snapshot's path: `<state file stem>.effective.json` beside it.
pub fn effective_path(state: &Path) -> PathBuf {
    let stem = state
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    state.with_file_name(format!("{stem}.effective.json"))
}

fn num(cfg: &Value, dotted: &str) -> Option<f64> {
    diary_retrieval::embedding::target::cfg_get(cfg, dotted).and_then(|v| match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    })
}

fn log(lines: &[LogLine]) {
    for l in lines {
        let level = match l.level {
            LogLevel::Info => "INFO",
            LogLevel::Warning => "WARNING",
            LogLevel::Error => "ERROR",
        };
        eprintln!("noevia-diary: {level} {}", l.message);
    }
}

fn check(url: &str) -> Result<AddressCheck, CheckFailure> {
    Ok(check_embedding_address(url, &mut system_resolver))
}

/// Python `tenant_writer.hold(tenant_of(path))` + `index_publication(path)` for one index write.
struct SlotGuard {
    app: Arc<App>,
}

fn tenant_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if name != "managed-index.db" && name != "index.db" {
        return None;
    }
    let dir = path.parent()?;
    let uid = dir.file_name()?.to_str()?;
    (dir.parent()?.file_name()?.to_str()? == "users" && server::is_uuid(uid))
        .then(|| uid.to_string())
}

impl FileGuard for SlotGuard {
    fn write(
        &self,
        path: &Path,
        write: &mut dyn FnMut() -> diary_retrieval::Result<()>,
    ) -> diary_retrieval::Result<()> {
        let Some(uid) = tenant_of(path) else {
            return write(); // the service index: no other writer coordinates on it
        };
        let managed = path.file_name().and_then(|n| n.to_str()) == Some("managed-index.db")
            && path.with_file_name("managed-diary.db").exists();
        let mut inner: Option<diary_retrieval::Error> = None;
        let r = self
            .app
            .writers
            .with_lock_as(&uid, None, Some(SLOT_WAIT), |held| {
                if !managed {
                    return write().map_err(|e| {
                        inner = Some(e);
                        crate::write::Error::other("index write")
                    });
                }
                let journal = Journal::open(path, held)?;
                journal.publish_effect(|| {
                    write().map_err(|e| {
                        inner = Some(e);
                        crate::write::Error::other("index write")
                    })
                })
            });
        match r {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(inner.take().unwrap_or_else(|| {
                if e.is_settlement_refusal() {
                    diary_retrieval::Error::Busy("backup settlement uncertain".into())
                } else {
                    diary_retrieval::Error::Busy(e.into_message())
                }
            })),
            Err(Refused::Busy) => Err(diary_retrieval::Error::Busy("busy".into())),
            Err(Refused::Fenced) => Err(diary_retrieval::Error::Busy("tenant deleted".into())),
        }
    }
}

impl Owner {
    /// Build the owner from the child's `embedding` config subset, publish its first snapshot and
    /// start the guard and the driver. Must be called on a Tokio runtime (the embedder's handle).
    pub fn start(app: Arc<App>, cfg: &Value, epoch: String) -> Result<Arc<Owner>, String> {
        let state = state_path_for(cfg).map_err(|e| e.to_string())?;
        let config = EmbeddingConfig::new(cfg, Some(&state)).map_err(|e| e.to_string())?;
        let config: &'static Arc<Mutex<EmbeddingConfig>> =
            Box::leak(Box::new(Arc::new(Mutex::new(config))));
        let engine = EngineConfig {
            base_url: app.llm_main.base_url.clone(),
            api_key: app.llm_main.api_key.clone(),
            timeout_s: num(cfg, "llm.timeout_s").unwrap_or(300.0),
            #[allow(clippy::cast_possible_truncation)]
            max_retries: num(cfg, "llm.max_retries").map_or(3, |v| v as i64),
            ..EngineConfig::default()
        };
        let handle = tokio::runtime::Handle::try_current().map_err(|e| e.to_string())?;
        // S6b: a native save re-indexes with the owned configuration (server.rs serve_write),
        // once this owner has started.
        let save_embedder = Arc::new(HttpEmbedder::new(
            engine.clone(),
            config.clone(),
            handle.clone(),
        ));
        let embed: &'static (dyn Embed + Sync) =
            Box::leak(Box::new(HttpEmbedder::new(engine, config.clone(), handle)));
        let guard: &'static (dyn FileGuard + Sync) =
            Box::leak(Box::new(SlotGuard { app: app.clone() }));
        let db = diary_retrieval::embedding::pystr::str_or_empty(
            diary_retrieval::embedding::target::cfg_get(cfg, "retrieval.db_path"),
        );
        let started = Instant::now();
        let app2 = app.clone();
        let migration: &'static Migration<'static> = Box::leak(Box::new(Migration::new(
            config,
            embed,
            guard,
            Path::new(&db),
            Options {
                duty: num(cfg, "retrieval.reindex_duty"),
                ..Options::default()
            },
            Box::new(move |uid| app2.writers.is_fenced(uid)),
            Box::new(move || started.elapsed().as_secs_f64()),
            Box::new(|s| std::thread::sleep(Duration::from_secs_f64(s.clamp(0.0, 3600.0)))),
        )));
        let owner = Arc::new(Owner {
            config,
            migration,
            effective: effective_path(&state),
            epoch,
            published: Mutex::new(None),
            driver: Mutex::new(Driver::default()),
            wake: Condvar::new(),
        });
        // The child follows nothing else: without a first snapshot this run must not start.
        if !owner.publish() {
            return Err("the embedding snapshot cannot be written".into());
        }
        let o = owner.clone();
        std::thread::Builder::new()
            .name("embedding-driver".into())
            .spawn(move || o.drive())
            .map_err(|e| e.to_string())?;
        let o = owner.clone();
        std::thread::Builder::new()
            .name("embedding-guard".into())
            .spawn(move || o.guard())
            .map_err(|e| e.to_string())?;
        let _ = app.save_embedder.set(save_embedder);
        Ok(owner)
    }

    /// Write the effective snapshot when it changed (atomically; mode 0600 like the state file).
    /// The configuration is read under the publication lock, so publications never overtake each
    /// other (an older snapshot can never replace a newer one). False when it could not be written.
    fn publish(&self) -> bool {
        let mut last = locked(&self.published);
        let mut snap = locked(self.config).get().to_json();
        if let Some(map) = snap.as_object_mut() {
            map.remove("version");
            map.insert("epoch".into(), json!(self.epoch));
        }
        if last.as_ref() == Some(&snap) {
            return true;
        }
        let tmp = self.effective.with_file_name(format!(
            "{}.{}.tmp",
            self.effective
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            std::process::id()
        ));
        let written = (|| -> std::io::Result<()> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(dir) = self.effective.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(snap.to_string().as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.effective)
        })();
        match written {
            Ok(()) => {
                *last = Some(snap);
                true
            }
            Err(e) => {
                eprintln!(
                    "noevia-diary: embedding snapshot not written ({})",
                    e.kind()
                );
                false
            }
        }
    }

    pub fn kick(&self) {
        locked(&self.driver).again = true;
        self.wake.notify_all();
    }

    fn running(&self) -> bool {
        let d = locked(&self.driver);
        d.running || d.again
    }

    /// Python `_start_embedding`: the first round of the address guard, its re-checks, then a pass.
    fn guard(self: Arc<Self>) {
        let snap = locked(self.config).get();
        let outcome = guard_seeded_addresses(
            &snap,
            &mut check,
            &mut |s| std::thread::sleep(Duration::from_secs_f64(s)),
            &RETRY_DELAYS_S,
            RECHECK_S,
        );
        outcome.apply(&mut locked(self.config));
        log(&outcome.logs);
        let _ = self.publish();
        for pending in outcome.rechecks {
            let o = self.clone();
            let _ = std::thread::Builder::new()
                .name("embedding-recheck".into())
                .spawn(move || loop {
                    std::thread::sleep(Duration::from_secs_f64(RECHECK_S));
                    if locked(o.config).source() != Source::Environment {
                        return;
                    }
                    let result = recheck_check(&pending, &mut check);
                    let step = apply_recheck(&mut locked(o.config), &pending, result);
                    log(&step.logs);
                    let _ = o.publish();
                    if step.next == RecheckNext::Stop {
                        return;
                    }
                });
        }
        // #1768: always, not only for a re-index.
        self.kick();
    }

    /// The pass loop: one pass per kick (a kick during a pass runs one more), plus the retry and
    /// re-check timers the migration asks for.
    fn drive(self: Arc<Self>) {
        loop {
            let mut d = locked(&self.driver);
            loop {
                if d.again {
                    break;
                }
                let now = Instant::now();
                if d.retry_at.is_some_and(|t| t <= now) {
                    d.retry_at = None;
                    self.migration.retry_fired();
                    break;
                }
                if d.recheck_at.is_some_and(|t| t <= now) {
                    d.recheck_at = None;
                    self.migration.recheck_fired();
                    break;
                }
                let next = [d.retry_at, d.recheck_at].into_iter().flatten().min();
                d = match next {
                    Some(at) => {
                        self.wake
                            .wait_timeout(d, at.saturating_duration_since(now))
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .0
                    }
                    None => self
                        .wake
                        .wait(d)
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                };
            }
            d.again = false;
            d.running = true;
            drop(d);
            let timers = self.migration.run_once(&|| crate::write::now_epoch());
            if let Some(p) = locked(&self.migration.last_pass).as_ref() {
                let embedded = p.get("embedded").and_then(Value::as_i64).unwrap_or(0);
                let failed = p.get("failed").and_then(Value::as_i64).unwrap_or(0);
                if embedded != 0 || failed != 0 {
                    let error = p
                        .get("error")
                        .and_then(Value::as_str)
                        .map(|e| format!(" ({e})"))
                        .unwrap_or_default();
                    eprintln!("noevia-diary: INFO embedding re-index pass: {embedded} vectors stored, {failed} file(s) incomplete{error}");
                }
            }
            let _ = self.publish();
            let mut d = locked(&self.driver);
            d.running = false;
            let now = Instant::now();
            if let Some(s) = timers.retry {
                d.retry_at = Some(now + Duration::from_secs_f64(s.clamp(0.0, 86_400.0)));
            }
            if let Some(s) = timers.recheck {
                d.recheck_at = Some(now + Duration::from_secs_f64(s.clamp(0.0, 86_400.0)));
            }
        }
    }

    /// GET/POST /api/internal/embedding (app.py `_embedding_guard`, `embedding_status`,
    /// `embedding_action`), after the tenant assertion was checked.
    pub async fn route(
        self: &Arc<Self>,
        app: &App,
        method: &Method,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Response<Body> {
        let mutating = method == Method::POST;
        match server::check_auth(app, headers) {
            Err(()) => return server::internal_error(),
            Ok(false) => return server::json_response(401, &json!({"error": "unauthorized"})),
            Ok(true) => {}
        }
        if mutating && app.auth_token.is_empty() {
            return server::json_response(
                403,
                &json!({"error": "set DIARY_AUTH_TOKEN to change the embedding model"}),
            );
        }
        if headers
            .get("x-cowork-user-id")
            .is_some_and(|v| !v.as_bytes().is_empty())
        {
            return server::json_response(
                400,
                &json!({"error": "service-wide endpoint: no tenant"}),
            );
        }
        let me = self.clone();
        if !mutating {
            let r = tokio::task::spawn_blocking(move || me.migration.status(me.running())).await;
            return match r {
                Ok(Ok(v)) => server::json_response(200, &v),
                _ => server::internal_error(),
            };
        }
        let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
            return server::json_response(400, &json!({"error": "invalid JSON"}));
        };
        let action = parsed
            .as_object()
            .and_then(|m| m.get("action"))
            .and_then(Value::as_str)
            .filter(|a| ACTIONS.contains(a))
            .map(str::to_owned);
        let Some(action) = action else {
            return server::json_response(
                400,
                &json!({"error": format!("action must be one of {}", ACTIONS.join(", "))}),
            );
        };
        let act = action.clone();
        let r = tokio::task::spawn_blocking(move || {
            let m = me.migration;
            let r = match act.as_str() {
                "set-next" => m.set_next(parsed.get("target").unwrap_or(&Value::Null)),
                "clear-next" => m.clear_next(),
                "switch" => m.switch(parsed.get("force") == Some(&Value::Bool(true))),
                "rollback" => m.rollback(),
                "reindex" => {
                    m.clear_refusals();
                    Ok(Value::Null)
                }
                _ => m.finish(),
            };
            let _ = me.publish();
            if r.is_ok() && matches!(act.as_str(), "set-next" | "switch" | "rollback" | "reindex") {
                me.kick();
            }
            match r {
                // The action's own status (one coverage scan, as Python), with `running` as Python
                // reports it after the kick; `reindex` reads it now.
                Ok(Value::Null) => m.status(me.running()),
                Ok(mut v) => {
                    if let Some(map) = v.as_object_mut() {
                        map.insert("running".into(), json!(me.running()));
                    }
                    Ok(v)
                }
                Err(e) => Err(e),
            }
        })
        .await;
        match r {
            Ok(Ok(v)) => {
                let active = v.get("active");
                let field = |k: &str| {
                    active
                        .and_then(|a| a.get(k))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned()
                };
                eprintln!(
                    "noevia-diary: INFO embedding {action}: active {}/{}",
                    field("profile"),
                    field("model")
                );
                server::json_response(200, &v)
            }
            Ok(Err(MigrationError::Config(e, coverage))) => {
                let mut out = json!({"error": e.message, "code": e.code});
                if let (Some(c), Some(map)) =
                    (coverage.filter(|c| !c.is_null()), out.as_object_mut())
                {
                    map.insert("coverage".into(), c);
                }
                server::json_response(e.status, &out)
            }
            _ => server::internal_error(),
        }
    }
}
