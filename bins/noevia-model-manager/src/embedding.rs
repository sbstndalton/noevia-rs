//! Settings-driven embedding service (sbstndalton/noevia#1668).
//!
//! The embedding container runs the stock llama.cpp server in router mode with its own preset
//! file (`--models-preset /config/embedding.ini --models-max 2`). This module owns that file.
//! noevia saves the set of models the service must serve in `DATA_DIR/embedding-service.json`:
//! usually one (the active model), two while a switch re-indexes (the active model keeps serving
//! search while the next one builds its vectors), none when embedding is off. The set is rendered
//! into the preset and the configured container, and only that container, is restarted (or
//! stopped when the set is empty) so the router picks it up. The container is separate from the
//! chat engine, so no chat model load can evict it.
//!
//! The environment (`EMBEDDING_MODEL` + `EMBED_MODEL_FILE`) is a seed only: it is read when no
//! saved state exists, and the saved state wins from then on (#1638).
//!
//! Nothing here is embedding-specific except names and defaults; a reranker service can reuse it
//! with its own container, preset and state file (#1665).
use crate::Job;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// Most models one service serves at once: the active one and the one a switch is building.
pub const MAX_MODELS: usize = 2;

/// Longest a restart or stop of the embedding container may take before the change is rolled back.
pub const ACT_TIMEOUT: Duration = Duration::from_secs(30);
/// #1677: how long a change waits for another change (or a delete check) to finish. The request
/// budget is ACT_TIMEOUT + LOCK_WAIT plus a margin, so a change that waited too long gives up
/// itself and can never be applied after its caller was told it failed.
pub const LOCK_WAIT: Duration = Duration::from_secs(15);

/// One served model. `model` is the router alias (the name clients send); `file` is the GGUF
/// path relative to the models directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub model: String,
    pub file: String,
}

/// What noevia asks the service to serve. Empty: embedding is off and the container is stopped.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Desired {
    pub models: Vec<Entry>,
}

/// Saved state. `previous` lists models that left the set; they stay protected from delete until
/// the router reports them unloaded (or the container is stopped).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub models: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previous: Vec<Entry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Restart,
    Stop,
}

/// Docker control scoped to exactly one container by construction.
pub trait ServiceControl: Send + Sync {
    fn act(&self, action: Action) -> Job<'_, ()>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Served {
    pub id: String,
    pub loaded: bool,
}

/// The embedding server's own view of what it serves.
pub trait Probe: Send + Sync {
    fn models(&self) -> Job<'_, Vec<Served>>;
    fn load<'a>(&'a self, model: &'a str) -> Job<'a, ()>;
}

pub struct EmbeddingService {
    pub container: String,
    models_dir: PathBuf,
    mount: String,
    preset: PathBuf,
    store: PathBuf,
    control: Arc<dyn ServiceControl>,
    probe: Arc<dyn Probe>,
    lock: tokio::sync::Mutex<()>,
    /// Set when boot could not read or write the saved state or the preset. While set, every
    /// delete is refused (fail closed) and status reports the error; a successful apply clears it.
    broken: std::sync::Mutex<Option<&'static str>>,
    act_timeout: Duration,
    lock_wait: Duration,
    /// #1678: bumped by every new warm-up and by a roll-back; an older warm-up stops.
    warm_generation: std::sync::atomic::AtomicU64,
    warm_interval: Duration,
    warm_attempts: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Invalid(&'static str),
    Unavailable(&'static str),
}

pub fn valid_alias(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        && !value.starts_with('.')
}

fn valid_relative(value: &str) -> bool {
    let path = Path::new(value);
    value.len() <= 1024
        && value.ends_with(".gguf")
        && !value.contains(['\n', '\r'])
        && path.components().count() <= 4
        && path.components().all(|c| matches!(c, Component::Normal(_)))
}

impl EmbeddingService {
    pub fn new(
        container: String,
        models_dir: PathBuf,
        mount: String,
        preset: PathBuf,
        store: PathBuf,
        control: Arc<dyn ServiceControl>,
        probe: Arc<dyn Probe>,
    ) -> Self {
        Self {
            container,
            models_dir,
            mount,
            preset,
            store,
            control,
            probe,
            lock: tokio::sync::Mutex::new(()),
            broken: std::sync::Mutex::new(None),
            act_timeout: ACT_TIMEOUT,
            lock_wait: LOCK_WAIT,
            warm_generation: std::sync::atomic::AtomicU64::new(0),
            warm_interval: Duration::from_secs(2),
            warm_attempts: 90,
        }
    }

    /// Test hook: shorten the warm-up poll.
    #[must_use]
    pub fn with_warm(mut self, interval: Duration, attempts: u32) -> Self {
        self.warm_interval = interval;
        self.warm_attempts = attempts;
        self
    }

    /// Test hook: shorten the wait for the change lock.
    #[must_use]
    pub fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }

    /// Test hook: shorten the Docker action timeout.
    #[must_use]
    pub fn with_act_timeout(mut self, timeout: Duration) -> Self {
        self.act_timeout = timeout;
        self
    }

    fn broken(&self) -> Option<&'static str> {
        *self
            .broken
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_broken(&self, value: Option<&'static str>) {
        *self
            .broken
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
    }

    /// Serialises delete checks (and the delete itself) with `apply`, so a model cannot be chosen
    /// for the service while its file is being deleted, nor deleted while it is being chosen.
    pub async fn hold(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lock.lock().await
    }

    pub fn saved(&self) -> Result<Option<Saved>, &'static str> {
        match std::fs::read(&self.store) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map(Some)
                .map_err(|_| "embedding service settings unreadable"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("embedding service settings unreadable"),
        }
    }

    /// Startup: the env seeds the saved state only when none exists; afterwards the saved state
    /// wins. The preset is always re-rendered from the saved state, so a hand edit or a lost file
    /// can never make the router serve something noevia did not choose.
    ///
    /// An empty or invalid seed is not saved: the service is off for this boot, and a later boot
    /// with the env filled in still seeds it (a first boot with the env unset must not turn
    /// embedding off for good).
    ///
    /// On failure the service stays up in an error state (see `broken`) instead of taking the
    /// whole model manager down; deletes are refused until a later apply succeeds.
    pub fn seed(&self, seed_model: &str, seed_file: &str) -> Result<Saved, &'static str> {
        let result = self.seed_inner(seed_model, seed_file);
        self.set_broken(result.as_ref().err().copied());
        result
    }

    fn seed_inner(&self, seed_model: &str, seed_file: &str) -> Result<Saved, &'static str> {
        let saved = match self.saved()? {
            Some(saved) => saved,
            None => {
                if valid_alias(seed_model) && valid_relative(seed_file) {
                    let saved = Saved {
                        models: vec![Entry {
                            model: seed_model.to_owned(),
                            file: seed_file.to_owned(),
                        }],
                        previous: Vec::new(),
                    };
                    self.write_store(&saved)?;
                    saved
                } else {
                    Saved::default()
                }
            }
        };
        self.render(&saved.models)?;
        Ok(saved)
    }

    /// Boot (and any restart of the container behind our back): load the saved models so the
    /// router reports them `loaded`, which is what noevia's identity check requires.
    pub fn start_warm(self: &Arc<Self>) {
        let Ok(Some(saved)) = self.saved() else {
            return;
        };
        if saved.models.is_empty() {
            return;
        }
        self.spawn_warm(saved.models.iter().map(|e| e.model.clone()).collect());
    }

    /// Start a warm-up for `models`; any earlier one stops at its next step.
    fn spawn_warm(self: &Arc<Self>, models: Vec<String>) {
        let generation = self.next_warm_generation();
        let this = self.clone();
        tokio::spawn(async move { this.warm_generation_run(&models, generation).await });
    }

    fn next_warm_generation(&self) -> u64 {
        self.warm_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .wrapping_add(1)
    }

    fn warm_current(&self, generation: u64) -> bool {
        self.warm_generation
            .load(std::sync::atomic::Ordering::SeqCst)
            == generation
    }

    fn write_store(&self, saved: &Saved) -> Result<(), &'static str> {
        let raw = serde_json::to_vec_pretty(saved).map_err(|_| "settings encode failed")?;
        atomic_write(&self.store, &raw)
    }

    pub fn preset_text(&self, models: &[Entry]) -> String {
        let mut text = String::from(
            "version = 1\n; Written by noevia-model-manager (#1668). Change the embedding model on\n; noevia's Models page; edits here are overwritten.\n",
        );
        for entry in models {
            text.push_str(&format!(
                "\n[{}]\nmodel = {}/{}\n",
                entry.model,
                self.mount.trim_end_matches('/'),
                entry.file
            ));
        }
        text
    }

    fn render(&self, models: &[Entry]) -> Result<(), &'static str> {
        atomic_write(&self.preset, self.preset_text(models).as_bytes())
    }

    fn validate(&self, desired: &Desired) -> Result<(), Refusal> {
        if desired.models.len() > MAX_MODELS {
            return Err(Refusal::Invalid("at most two embedding models at once"));
        }
        let root = if desired.models.is_empty() {
            return Ok(());
        } else {
            self.models_dir
                .canonicalize()
                .map_err(|_| Refusal::Unavailable("models directory unavailable"))?
        };
        for (i, entry) in desired.models.iter().enumerate() {
            if !valid_alias(&entry.model) {
                return Err(Refusal::Invalid("model name refused"));
            }
            if desired
                .models
                .iter()
                .take(i)
                .any(|e| e.model == entry.model)
            {
                return Err(Refusal::Invalid("model listed twice"));
            }
            if !valid_relative(&entry.file) {
                return Err(Refusal::Invalid("file path refused"));
            }
            let full = root
                .join(&entry.file)
                .canonicalize()
                .map_err(|_| Refusal::Invalid("model file not found"))?;
            if !full.starts_with(&root) || !full.is_file() {
                return Err(Refusal::Invalid("model file not found"));
            }
        }
        Ok(())
    }

    /// Serve exactly `desired`: save, render, then restart the one container (or stop it when the
    /// set is empty). Returns before the models load; callers poll `status` until the router
    /// reports each alias `loaded` (noevia-core does, before it re-indexes).
    ///
    /// The change runs in its own task, so a caller that gives up (request timeout, closed
    /// connection) can never leave it half done.
    pub async fn apply(self: &Arc<Self>, desired: Desired) -> Result<Saved, Refusal> {
        let this = self.clone();
        tokio::spawn(async move { this.apply_held(desired).await })
            .await
            .map_err(|_| Refusal::Unavailable("embedding service change failed"))?
    }

    async fn apply_held(self: &Arc<Self>, desired: Desired) -> Result<Saved, Refusal> {
        let Ok(_guard) = tokio::time::timeout(self.lock_wait, self.lock.lock()).await else {
            return Err(Refusal::Unavailable(
                "another embedding change is still running; nothing was changed",
            ));
        };
        // Validate under the lock: a delete holding it may just have removed the file.
        self.validate(&desired)?;
        let current = self
            .saved()
            .map_err(Refusal::Unavailable)?
            .unwrap_or_default();
        // Keep protecting whatever the service may still hold until it is proven gone.
        let previous = unique(
            current
                .previous
                .iter()
                .chain(current.models.iter())
                .filter(|e| !desired.models.contains(e)),
        );
        let saved = Saved {
            models: desired.models.clone(),
            previous,
        };
        self.write_store(&saved).map_err(Refusal::Unavailable)?;
        if let Err(e) = self.render(&saved.models) {
            self.roll_back(&current, &saved);
            return Err(Refusal::Unavailable(e));
        }
        let action = if saved.models.is_empty() {
            Action::Stop
        } else {
            Action::Restart
        };
        let acted = match tokio::time::timeout(self.act_timeout, self.control.act(action)).await {
            Ok(result) => result,
            Err(_) => Err("embedding container did not answer in time"),
        };
        if let Err(e) = acted {
            self.roll_back(&current, &saved);
            return Err(Refusal::Unavailable(e));
        }
        self.set_broken(None);
        if saved.models.is_empty() {
            // #1696: a warm-up of the set before the stop must not load anything later.
            self.next_warm_generation();
            // A stopped container holds no model file.
            let saved = Saved::default();
            self.write_store(&saved).map_err(Refusal::Unavailable)?;
            return Ok(saved);
        }
        self.spawn_warm(saved.models.iter().map(|e| e.model.clone()).collect());
        Ok(saved)
    }

    /// A failed or timed-out Docker action: go back to the models that were set, but keep every
    /// model either set may have loaded protected, because Docker may still carry the action out.
    fn roll_back(self: &Arc<Self>, current: &Saved, attempted: &Saved) {
        // #1696: stop any running warm-up first, also when the restored set is empty (off): a
        // late Docker restart onto the attempted preset must not be loaded by a stale warm-up.
        self.next_warm_generation();
        let back = Saved {
            models: current.models.clone(),
            previous: unique(
                current
                    .previous
                    .iter()
                    .chain(attempted.previous.iter())
                    .chain(attempted.models.iter())
                    .filter(|e| !current.models.contains(e)),
            ),
        };
        let stored = self.write_store(&back);
        let rendered = self.render(&back.models);
        if let Err(e) = stored.and(rendered) {
            self.set_broken(Some(e));
        }
        // #1682: the bump above also stopped the warm-up of the restored set; start a fresh one
        // so the restored models get loaded and `previous` is released once only they are loaded.
        if !back.models.is_empty() {
            self.spawn_warm(back.models.iter().map(|e| e.model.clone()).collect());
        }
    }

    /// Load every alias once the restarted router answers, then release `previous` when the router
    /// reports nothing else loaded.
    pub async fn warm(&self, wanted: &[String]) {
        let generation = self.next_warm_generation();
        self.warm_generation_run(wanted, generation).await;
    }

    async fn warm_generation_run(&self, wanted: &[String], generation: u64) {
        for _ in 0..self.warm_attempts {
            tokio::time::sleep(self.warm_interval).await;
            if !self.warm_current(generation) {
                return;
            }
            let Ok(models) = self.probe.models().await else {
                continue;
            };
            let loaded = |id: &str| models.iter().any(|m| m.id == id && m.loaded);
            if wanted.iter().all(|w| loaded(w)) {
                if models.iter().all(|m| !m.loaded || wanted.contains(&m.id)) {
                    let _guard = self.lock.lock().await;
                    if !self.warm_current(generation) {
                        return;
                    }
                    if let Ok(Some(mut saved)) = self.saved() {
                        let current: Vec<&String> = saved.models.iter().map(|e| &e.model).collect();
                        if current.len() == wanted.len()
                            && wanted.iter().all(|w| current.contains(&w))
                        {
                            saved.previous.clear();
                            let _ = self.write_store(&saved);
                        }
                    }
                }
                return;
            }
            for w in wanted {
                if !loaded(w) && models.iter().any(|m| &m.id == w) {
                    let _ = self.probe.load(w).await;
                }
            }
        }
    }

    pub async fn status(&self) -> Value {
        let running = match self.probe.models().await {
            Ok(models) => json!({
                "reachable": true,
                "loaded": models.iter().filter(|m| m.loaded).map(|m| m.id.clone()).collect::<Vec<_>>(),
                "listed": models.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            }),
            Err(_) => json!({"reachable": false, "loaded": [], "listed": []}),
        };
        if let Some(e) = self.broken() {
            return json!({"managed": true, "container": self.container, "error": e, "running": running});
        }
        match self.saved() {
            Ok(saved) => {
                let saved = saved.unwrap_or_default();
                json!({
                    "managed": true,
                    "container": self.container,
                    "enabled": !saved.models.is_empty(),
                    "models": saved.models,
                    "previous": saved.previous,
                    "running": running,
                })
            }
            Err(e) => {
                json!({"managed": true, "container": self.container, "error": e, "running": running})
            }
        }
    }

    /// True when a delete key (models-dir-relative route key, directory, file name or alias) names
    /// a model the service is set to serve, may still hold, or reports loaded. Fails closed when the
    /// saved state is unreadable or boot left the service in an error state. Callers that act on
    /// the answer hold `hold()` across the check and the action.
    pub async fn protects(&self, key: &str) -> bool {
        if self.broken().is_some() {
            return true;
        }
        let Ok(saved) = self.saved() else {
            return true;
        };
        let saved = saved.unwrap_or_default();
        let key = key.trim_matches('/');
        if saved
            .models
            .iter()
            .chain(saved.previous.iter())
            .any(|e| names_entry(key, e))
        {
            return true;
        }
        // A model the router reports loaded is held even if the saved state forgot it.
        match self.probe.models().await {
            Ok(models) => models.iter().any(|m| m.loaded && m.id == key),
            Err(_) => false,
        }
    }
}

fn unique<'a>(entries: impl Iterator<Item = &'a Entry>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for entry in entries {
        if !out.contains(entry) {
            out.push(entry.clone());
        }
    }
    out
}

/// `name-00001-of-00003.gguf` -> `name.gguf`: the model manager keys a sharded model by its shard
/// base (`shard_key` in services.py), so both sides compare without the shard suffix.
fn unshard(value: &str) -> String {
    let Some(body) = value.strip_suffix(".gguf") else {
        return value.to_owned();
    };
    // "-NNNNN-of-NNNNN" is 15 ASCII bytes.
    let split = body.len().saturating_sub(15);
    if let (Some(base), Some(tail)) = (body.get(..split), body.get(split..)) {
        let digits = |t: Option<&str>| {
            t.is_some_and(|t| t.len() == 5 && t.bytes().all(|c| c.is_ascii_digit()))
        };
        let shard = !base.is_empty()
            && tail.len() == 15
            && tail.starts_with('-')
            && tail.get(6..10) == Some("-of-")
            && digits(tail.get(1..6))
            && digits(tail.get(10..));
        if shard {
            return format!("{base}.gguf");
        }
    }
    value.to_owned()
}

fn stem(value: &str) -> &str {
    let name = value.rsplit('/').next().unwrap_or(value);
    name.strip_suffix(".gguf").unwrap_or(name)
}

fn names_entry(key: &str, entry: &Entry) -> bool {
    if key.is_empty() {
        return false;
    }
    let dir = entry.file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let (key_base, file_base) = (unshard(key), unshard(&entry.file));
    key == entry.model
        || key == entry.file
        || key_base == file_base
        || stem(&key_base) == stem(&file_base)
        || (!dir.is_empty() && (key == dir || entry.file.starts_with(&format!("{key}/"))))
}

fn atomic_write(path: &Path, raw: &[u8]) -> Result<(), &'static str> {
    let dir = path.parent().ok_or("settings path refused")?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|_| "settings write failed")?;
    std::io::Write::write_all(&mut tmp, raw).map_err(|_| "settings write failed")?;
    tmp.as_file()
        .sync_all()
        .map_err(|_| "settings write failed")?;
    // The embed container reads the preset as another user; keep it world-readable.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o644));
    }
    tmp.persist(path).map_err(|_| "settings write failed")?;
    Ok(())
}

/// HTTP probe of the embedding router (`/v1/models`, `/models/load`).
pub struct HttpProbe {
    base: String,
    client: reqwest::Client,
}

impl HttpProbe {
    pub fn new(base: &str) -> Result<Self, reqwest::Error> {
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()?,
        })
    }
}

/// `/v1/models` from either a router (each entry has `status.value`) or a single-model server (no
/// status: whatever it lists is loaded).
pub fn parse_models(raw: &[u8]) -> Result<Vec<Served>, &'static str> {
    let value: Value = serde_json::from_slice(raw).map_err(|_| "models response refused")?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or("models response refused")?;
    Ok(data
        .iter()
        .take(256)
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_owned();
            let loaded = match m.get("status").and_then(|s| s.get("value")) {
                Some(v) => v.as_str() == Some("loaded"),
                None => true,
            };
            Some(Served { id, loaded })
        })
        .collect())
}

impl Probe for HttpProbe {
    fn models(&self) -> Job<'_, Vec<Served>> {
        Box::pin(async move {
            let response = self
                .client
                .get(format!("{}/v1/models", self.base))
                .send()
                .await
                .map_err(|_| "embedding service unreachable")?;
            if !response.status().is_success() {
                return Err("embedding service unreachable");
            }
            let raw = response
                .bytes()
                .await
                .map_err(|_| "embedding service unreachable")?;
            if raw.len() > crate::LOG_CAP {
                return Err("models response exceeded limit");
            }
            parse_models(&raw)
        })
    }
    fn load<'a>(&'a self, model: &'a str) -> Job<'a, ()> {
        Box::pin(async move {
            self.client
                .post(format!("{}/models/load", self.base))
                .header("content-type", "application/json")
                .body(json!({ "model": model }).to_string())
                .send()
                .await
                .map_err(|_| "embedding service unreachable")?;
            Ok(())
        })
    }
}
