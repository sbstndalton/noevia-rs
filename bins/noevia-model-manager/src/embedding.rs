//! Settings-driven embedding service (sbstndalton/noevia#1668).
//!
//! The embedding container runs the stock llama.cpp server in router mode with its own preset
//! file (`--models-preset /config/embedding.ini --models-max 1`). This module owns that file:
//! the admin's choice is saved in `DATA_DIR/embedding-service.json`, rendered into the preset,
//! and the configured container (and only that container) is restarted or stopped so the
//! router picks it up. The container is separate from the chat engine, so no chat load can
//! evict it.
//!
//! The environment (`EMBEDDING_MODEL` + `EMBED_MODEL_FILE`) is a seed only: it is read when no
//! saved choice exists, and the saved choice wins from then on (#1638).
use crate::Job;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// What the admin chose. `model` is the router alias (the name clients send); `file` is the
/// GGUF path relative to the models directory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// Saved state. `previous` is the model the service ran before the last switch; it stays
/// protected from delete until the router reports the new model loaded (or the service is off).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    #[serde(flatten)]
    pub choice: Choice,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<Choice>,
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
        && !value.contains('\n')
        && !value.contains('\r')
        && path.components().count() <= 4
        && path.components().all(|c| matches!(c, Component::Normal(_)))
}

impl EmbeddingService {
    #[allow(clippy::too_many_arguments)]
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

    /// Startup: the env seeds the saved choice only when none exists; afterwards the saved
    /// choice wins. The preset is always re-rendered from the saved choice so a hand edit or
    /// a lost file can never make the router serve something noevia did not choose.
    pub fn seed(&self, seed_model: &str, seed_file: &str) -> Result<Saved, &'static str> {
        let saved = match self.saved()? {
            Some(saved) => saved,
            None => {
                let choice = if valid_alias(seed_model) && valid_relative(seed_file) {
                    Choice {
                        enabled: true,
                        model: Some(seed_model.to_owned()),
                        file: Some(seed_file.to_owned()),
                    }
                } else {
                    Choice::default()
                };
                let saved = Saved {
                    choice,
                    previous: None,
                };
                self.write_store(&saved)?;
                saved
            }
        };
        self.render(&saved.choice)?;
        Ok(saved)
    }

    fn write_store(&self, saved: &Saved) -> Result<(), &'static str> {
        let raw = serde_json::to_vec_pretty(saved).map_err(|_| "settings encode failed")?;
        atomic_write(&self.store, &raw)
    }

    pub fn preset_text(&self, choice: &Choice) -> String {
        let mut text = String::from(
            "version = 1\n; Written by noevia-model-manager (#1668). Change the embedding model on\n; noevia's Models page; edits here are overwritten.\n",
        );
        if let (true, Some(model), Some(file)) = (choice.enabled, &choice.model, &choice.file) {
            text.push_str(&format!(
                "\n[{model}]\nmodel = {}/{file}\n",
                self.mount.trim_end_matches('/')
            ));
        }
        text
    }

    fn render(&self, choice: &Choice) -> Result<(), &'static str> {
        atomic_write(&self.preset, self.preset_text(choice).as_bytes())
    }

    fn validate(&self, choice: &Choice) -> Result<(), Refusal> {
        if !choice.enabled {
            return Ok(());
        }
        let model = choice
            .model
            .as_deref()
            .ok_or(Refusal::Invalid("model is required"))?;
        if !valid_alias(model) {
            return Err(Refusal::Invalid("model name refused"));
        }
        let file = choice
            .file
            .as_deref()
            .ok_or(Refusal::Invalid("file is required"))?;
        if !valid_relative(file) {
            return Err(Refusal::Invalid("file path refused"));
        }
        let root = self
            .models_dir
            .canonicalize()
            .map_err(|_| Refusal::Unavailable("models directory unavailable"))?;
        let full = root
            .join(file)
            .canonicalize()
            .map_err(|_| Refusal::Invalid("model file not found"))?;
        if !full.starts_with(&root) || !full.is_file() {
            return Err(Refusal::Invalid("model file not found"));
        }
        Ok(())
    }

    /// Apply an admin choice: save, render, then restart (on) or stop (off) the one container.
    /// Switching on returns before the model is loaded; callers poll `status` until the router
    /// reports the alias `loaded` (noevia-core does, before it re-indexes).
    pub async fn apply(self: &Arc<Self>, choice: Choice) -> Result<Saved, Refusal> {
        self.validate(&choice)?;
        let _guard = self.lock.lock().await;
        let current = self
            .saved()
            .map_err(Refusal::Unavailable)?
            .unwrap_or_default();
        let choice = if choice.enabled {
            choice
        } else {
            Choice::default()
        };
        // Keep protecting whatever the service may still hold until it is proven gone.
        let previous = if current.choice.enabled && current.choice != choice {
            Some(current.choice.clone())
        } else {
            current.previous.clone()
        };
        let saved = Saved {
            choice: choice.clone(),
            previous,
        };
        self.write_store(&saved).map_err(Refusal::Unavailable)?;
        self.render(&choice).map_err(Refusal::Unavailable)?;
        let action = if choice.enabled {
            Action::Restart
        } else {
            Action::Stop
        };
        self.control
            .act(action)
            .await
            .map_err(Refusal::Unavailable)?;
        if choice.enabled {
            let this = self.clone();
            let model = choice.model.clone().unwrap_or_default();
            tokio::spawn(async move { this.warm(&model).await });
            Ok(saved)
        } else {
            // A stopped container holds no model file.
            let saved = Saved {
                choice,
                previous: None,
            };
            self.write_store(&saved).map_err(Refusal::Unavailable)?;
            Ok(saved)
        }
    }

    /// Load the chosen alias once the restarted router answers, then release `previous`.
    pub async fn warm(&self, model: &str) {
        for _ in 0..self.warm_attempts {
            tokio::time::sleep(self.warm_interval).await;
            let Ok(models) = self.probe.models().await else {
                continue;
            };
            if models.iter().any(|m| m.id == model && m.loaded) {
                if models.iter().all(|m| m.id == model || !m.loaded) {
                    let _guard = self.lock.lock().await;
                    if let Ok(Some(mut saved)) = self.saved() {
                        if saved.choice.model.as_deref() == Some(model) {
                            saved.previous = None;
                            let _ = self.write_store(&saved);
                        }
                    }
                }
                return;
            }
            if models.iter().any(|m| m.id == model) {
                let _ = self.probe.load(model).await;
            }
        }
    }

    pub async fn status(&self) -> Value {
        let saved = self.saved();
        let running = match self.probe.models().await {
            Ok(models) => json!({
                "reachable": true,
                "loaded": models.iter().filter(|m| m.loaded).map(|m| m.id.clone()).collect::<Vec<_>>(),
                "listed": models.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            }),
            Err(_) => json!({"reachable": false, "loaded": [], "listed": []}),
        };
        match saved {
            Ok(saved) => {
                let saved = saved.unwrap_or_default();
                json!({
                    "managed": true,
                    "container": self.container,
                    "enabled": saved.choice.enabled,
                    "model": saved.choice.model,
                    "file": saved.choice.file,
                    "previous": saved.previous,
                    "running": running,
                })
            }
            Err(e) => {
                json!({"managed": true, "container": self.container, "error": e, "running": running})
            }
        }
    }

    /// Delete keys (models-dir-relative route keys, directory names, file names or aliases)
    /// that name a model the service is set to run or may still hold. Fails closed when the
    /// saved state is unreadable.
    pub async fn protects(&self, key: &str) -> bool {
        let Ok(saved) = self.saved() else {
            return true;
        };
        let saved = saved.unwrap_or_default();
        let mut held: Vec<Choice> = Vec::new();
        if saved.choice.enabled {
            held.push(saved.choice.clone());
        }
        if let Some(previous) = saved.previous {
            held.push(previous);
        }
        // A model the router reports loaded is held even if the saved state forgot it.
        if let Ok(models) = self.probe.models().await {
            for m in models.into_iter().filter(|m| m.loaded) {
                held.push(Choice {
                    enabled: true,
                    model: Some(m.id),
                    file: None,
                });
            }
        }
        let key = key.trim_matches('/');
        held.iter().any(|c| names_choice(key, c))
    }
}

fn stem(value: &str) -> &str {
    let name = value.rsplit('/').next().unwrap_or(value);
    name.strip_suffix(".gguf").unwrap_or(name)
}

fn names_choice(key: &str, choice: &Choice) -> bool {
    if key.is_empty() {
        return false;
    }
    if choice.model.as_deref() == Some(key) {
        return true;
    }
    let Some(file) = choice.file.as_deref() else {
        return false;
    };
    let dir = file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    key == file
        || stem(key) == stem(file)
        || (!dir.is_empty() && (key == dir || file.starts_with(&format!("{key}/"))))
}

fn atomic_write(path: &Path, raw: &[u8]) -> Result<(), &'static str> {
    let dir = path.parent().ok_or("settings path refused")?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|_| "settings write failed")?;
    std::io::Write::write_all(&mut tmp, raw).map_err(|_| "settings write failed")?;
    tmp.as_file()
        .sync_all()
        .map_err(|_| "settings write failed")?;
    // The embed container reads the preset as another user; keep it world-readable.
    #[cfg(unix)]
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

/// `/v1/models` from either a router (each entry has `status.value`) or a single-model
/// server (no status: whatever it lists is loaded).
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
                .body(json!({"model": model}).to_string())
                .send()
                .await
                .map_err(|_| "embedding service unreachable")?;
            Ok(())
        })
    }
}
