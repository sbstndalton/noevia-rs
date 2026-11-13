//! Embeddings for the Diary index over HTTP (port of `Embedder` in `diary/agent/embedding.py`):
//! text framed for the target's profile, a document the server refuses as too long cut and
//! averaged (`embed_fitting`), one client per endpoint. The engine credential goes to the engine
//! only; a dedicated embeddings server never receives it.
//!
//! The index and the migration are synchronous; they call this from a blocking context (the
//! exchange runs the assembler under `block_in_place`, the migration pass runs on a blocking
//! task), and each HTTP call is driven on the runtime through the handle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use diary_retrieval::embedding::config::EmbeddingConfig;
use diary_retrieval::embedding::text::{embed_fitting, frame, FitError, Kinds, SendError};
use diary_retrieval::index::{Embedder, Kind};
use diary_retrieval::migration::Embed;
use diary_retrieval::Target;

use crate::model::{Client, Config, EmbedError};

/// Where the active target comes from: the shared configuration, or a value the caller read from
/// the configuration Python holds in memory (its refusals and runtime changes included).
enum Active {
    Config(Arc<Mutex<EmbeddingConfig>>),
    Fixed(Target),
}

pub struct HttpEmbedder {
    /// `llm.base_url`, `llm.api_key`, `llm.timeout_s`, `llm.max_retries`.
    engine: Config,
    active: Active,
    clients: Mutex<HashMap<String, Arc<Client>>>,
    handle: tokio::runtime::Handle,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl HttpEmbedder {
    /// `engine` carries the chat engine's address, credential, timeout and retries; the model and
    /// embeddings address come from each target.
    pub fn new(
        engine: Config,
        config: Arc<Mutex<EmbeddingConfig>>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self { engine, active: Active::Config(config), clients: Mutex::new(HashMap::new()), handle }
    }

    /// An embedder whose active target is `active` (Python's live configuration, sent per call).
    pub fn fixed(engine: Config, active: Target, handle: tokio::runtime::Handle) -> Self {
        Self { engine, active: Active::Fixed(active), clients: Mutex::new(HashMap::new()), handle }
    }

    fn client(&self, target: &Target) -> Result<Arc<Client>, String> {
        let key = target.base_url.clone().unwrap_or_default();
        let mut clients = locked(&self.clients);
        if let Some(c) = clients.get(&key) {
            return Ok(c.clone());
        }
        let client = Arc::new(
            Client::new(Config {
                base_url: self.engine.base_url.clone(),
                // The engine key goes to the engine only; a dedicated embeddings server never gets it.
                api_key: if target.base_url.is_none() {
                    self.engine.api_key.clone()
                } else {
                    String::new()
                },
                embed_base_url: key.clone(),
                timeout_s: self.engine.timeout_s,
                max_retries: self.engine.max_retries,
                retry_backoff_s: self.engine.retry_backoff_s,
                ..Config::default()
            })
            .map_err(|e| e.to_string())?,
        );
        clients.insert(key, client.clone());
        Ok(client)
    }

    /// Python `Embedder.embed(target, texts, kind)`.
    pub fn embed_kind(
        &self,
        target: &Target,
        texts: &[String],
        kind: &str,
    ) -> Result<Vec<Vec<f64>>, String> {
        // Fail closed on an unknown profile before any call.
        frame::<String>(&target.profile, &[], Kinds::One(kind), None).map_err(|e| e.to_string())?;
        let client = self.client(target)?;
        let mut send = |texts: &[String],
                        kinds: &[String],
                        titles: &[Option<String>]|
         -> Result<Vec<Vec<f64>>, SendError<String>> {
            let k: Vec<&str> = kinds.iter().map(String::as_str).collect();
            let t: Vec<Option<&str>> = titles.iter().map(Option::as_deref).collect();
            let framed = frame(&target.profile, texts, Kinds::Each(&k), Some(&t))
                .map_err(|e| SendError::Other(e.to_string()))?;
            match self
                .handle
                .block_on(client.embed(&framed, Some(&target.model)))
            {
                Ok(v) => Ok(v),
                Err(EmbedError::TooLarge(t)) => Err(SendError::TooLarge {
                    tokens: t.tokens,
                    limit: t.limit,
                }),
                Err(EmbedError::Redirected(s)) => Err(SendError::Other(format!(
                    "embeddings server redirected (HTTP {s}); redirects are refused"
                ))),
                Err(EmbedError::Retry(m) | EmbedError::Failed(m)) => Err(SendError::Other(m)),
            }
        };
        embed_fitting(&mut send, texts, Kinds::One(kind), None).map_err(|e| match e {
            FitError::Send(SendError::Other(m)) => m,
            FitError::Send(SendError::TooLarge { .. }) => {
                "embeddings refused as too long".to_owned()
            }
            FitError::Malformed(m) => m.to_owned(),
        })
    }
}

impl Embedder for HttpEmbedder {
    fn active(&self) -> Target {
        match &self.active {
            Active::Config(c) => locked(c).active(),
            Active::Fixed(t) => t.clone(),
        }
    }

    fn embed(&self, target: &Target, texts: &[&str], kind: Kind) -> Result<Vec<Vec<f64>>, String> {
        let owned: Vec<String> = texts.iter().map(|t| (*t).to_owned()).collect();
        self.embed_kind(target, &owned, kind.as_str())
    }
}

impl Embed for HttpEmbedder {
    fn embed(&self, target: &Target, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
        self.embed_kind(target, texts, "document")
    }
}
