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
    pub fn seed(&self, seed_model: &str, seed_file: &str) -> Result<Saved, &'static str> {
        let saved = match self.saved()? {
            Some(saved) => saved,
            None => {
                let models = if valid_alias(seed_model) && valid_relative(seed_file) {
                    vec![Entry {
                        model: seed_model.to_owned(),
                        file: seed_file.to_owned(),
                    }]
                } else {
                    Vec::new()
                };
                let saved = Saved {
                    models,
                    previous: Vec::new(),
                };
                self.write_store(&saved)?;
                saved
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
        let this = self.clone();
        let models: Vec<String> = saved.models.iter().map(|e| e.model.clone()).collect();
        tokio::spawn(async move { this.warm(&models).await });
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
    pub async fn apply(self: &Arc<Self>, desired: Desired) -> Result<Saved, Refusal> {
        self.validate(&desired)?;
        let _guard = self.lock.lock().await;
        let current = self
            .saved()
            .map_err(Refusal::Unavailable)?
            .unwrap_or_default();
        // Keep protecting whatever the service may still hold until it is proven gone.
        let mut previous: Vec<Entry> = current
            .previous
            .into_iter()
            .chain(current.models)
            .filter(|e| !desired.models.contains(e))
            .collect();
        previous.dedup();
        let saved = Saved {
            models: desired.models.clone(),
            previous,
        };
        self.write_store(&saved).map_err(Refusal::Unavailable)?;
        self.render(&saved.models).map_err(Refusal::Unavailable)?;
        if saved.models.is_empty() {
            self.control
                .act(Action::Stop)
                .await
                .map_err(Refusal::Unavailable)?;
            // A stopped container holds no model file.
            let saved = Saved::default();
            self.write_store(&saved).map_err(Refusal::Unavailable)?;
            return Ok(saved);
        }
        self.control
            .act(Action::Restart)
            .await
            .map_err(Refusal::Unavailable)?;
        let this = self.clone();
        let models: Vec<String> = saved.models.iter().map(|e| e.model.clone()).collect();
        tokio::spawn(async move { this.warm(&models).await });
        Ok(saved)
    }

    /// Load every alias once the restarted router answers, then release `previous` when the router
    /// reports nothing else loaded.
    pub async fn warm(&self, wanted: &[String]) {
        for _ in 0..self.warm_attempts {
            tokio::time::sleep(self.warm_interval).await;
            let Ok(models) = self.probe.models().await else {
                continue;
            };
            let loaded = |id: &str| models.iter().any(|m| m.id == id && m.loaded);
            if wanted.iter().all(|w| loaded(w)) {
                if models.iter().all(|m| !m.loaded || wanted.contains(&m.id)) {
                    let _guard = self.lock.lock().await;
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
    /// saved state is unreadable.
    pub async fn protects(&self, key: &str) -> bool {
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

fn stem(value: &str) -> &str {
    let name = value.rsplit('/').next().unwrap_or(value);
    name.strip_suffix(".gguf").unwrap_or(name)
}

fn names_entry(key: &str, entry: &Entry) -> bool {
    if key.is_empty() {
        return false;
    }
    let dir = entry.file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    key == entry.model
        || key == entry.file
        || stem(key) == stem(&entry.file)
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
