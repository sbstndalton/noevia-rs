//! Sticky model-loader service view, separate from the embeddings endpoint's
//! served-model gate. Refresh is read-only and never requests model loading.
use crate::Task;
use serde_json::Value;
use std::collections::BTreeSet;
pub const REFRESH_MS: u64 = 10_000;
pub const TIMEOUT_MS: u64 = 6_000;
pub const STALE_MS: u64 = 15_000;
/// Transport supplies GET /api/v1/embedding-service, Content-Type application/json,
/// optional X-Model-Loader-Token, a 6000ms bound, and redirect/address policy.
/// Tokens belong to the adapter and must never appear in returned errors.
pub trait Loader: Sync {
    fn get(&self) -> Task<'_, Result<(u16, Value), String>>;
}
#[derive(Default)]
pub struct Service {
    last: Option<(u64, Value)>,
    last_error: Option<String>,
    next_refresh_ms: Option<u64>,
}
impl Service {
    pub fn managed(&self) -> bool {
        self.last.is_some()
    }
    pub fn enabled(&self) -> bool {
        self.last
            .as_ref()
            .is_some_and(|(_, b)| b.get("enabled") == Some(&Value::Bool(true)))
    }
    pub fn status(&self, now: u64) -> Option<Value> {
        let (at, body) = self.last.as_ref()?;
        let mut body = body.clone();
        if let Some(object) = body.as_object_mut() {
            object.insert(
                "stale".into(),
                Value::Bool(now.saturating_sub(*at) > STALE_MS),
            );
            if let Some(error) = &self.last_error {
                object.insert("lastError".into(), Value::String(error.clone()));
            }
        }
        Some(body)
    }
    /// One exclusive owner coalesces calls; callers can share a mutex around the
    /// service. A failed refresh retains the previous managed/enabled state.
    pub async fn refresh(&mut self, loader: &impl Loader, now: impl Fn() -> u64) -> Option<Value> {
        match loader.get().await {
            Ok((status, body))
                if (200..300).contains(&status)
                    && body.get("managed") == Some(&Value::Bool(true)) =>
            {
                self.last = Some((now(), body));
                self.last_error = None;
            }
            Ok((404, _)) if self.last.is_none() => self.last_error = Some("notManaged".into()),
            Ok((status, _)) => self.last_error = Some(format!("HTTP {status}")),
            Err(error) => self.last_error = Some(error.chars().take(200).collect()),
        }
        self.last.as_ref().map(|(_, body)| body.clone())
    }
    /// Call only after native readiness and only with MODEL_LOADER_URL configured.
    /// The first call refreshes immediately. No hidden task/timer survives shutdown.
    pub async fn tick(&mut self, loader: &impl Loader, now: impl Fn() -> u64) {
        let at = now();
        if self.next_refresh_ms.is_some_and(|next| at < next) {
            return;
        }
        self.next_refresh_ms = Some(at.saturating_add(REFRESH_MS));
        self.refresh(loader, now).await;
    }
    pub fn protected_names(&self) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let Some((_, body)) = &self.last else {
            return names;
        };
        for field in ["models", "previous"] {
            for entry in body
                .get(field)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(model) = entry
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    names.insert(model.into());
                }
                if let Some(file) = entry
                    .get("file")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    names.insert(file.into());
                    let stem = file.rsplit('/').next().unwrap_or(file);
                    let stem = if stem
                        .get(stem.len().saturating_sub(5)..)
                        .is_some_and(|s| s.eq_ignore_ascii_case(".gguf"))
                    {
                        stem.get(..stem.len() - 5).unwrap_or(stem)
                    } else {
                        stem
                    };
                    names.insert(stem.into());
                }
            }
        }
        for id in body
            .pointer("/running/loaded")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            names.insert(id.into());
        }
        names
    }
    pub fn file_of(&self, model: &str) -> Option<&str> {
        let (_, body) = self.last.as_ref()?;
        ["models", "previous"]
            .into_iter()
            .flat_map(|field| {
                body.get(field)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .find(|entry| entry.get("model").and_then(Value::as_str) == Some(model))?
            .get("file")?
            .as_str()
            .filter(|s| !s.is_empty())
    }
}
/// Trailing /v1 and slashes do not distinguish an embeddings server.
pub fn same_address(a: Option<&str>, b: Option<&str>) -> bool {
    fn normal(s: Option<&str>) -> &str {
        let s = s.unwrap_or_default().trim().trim_end_matches('/');
        s.strip_suffix("/v1").unwrap_or(s)
    }
    normal(a) == normal(b)
}
/// Decode only names actually reported served, including loaded/sleeping router
/// aliases and Ollama models. This deliberately bypasses UI served-model caches.
pub fn served_ids(body: &Value) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for row in body
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(status) = row
            .get("status")
            .filter(|v| v.is_object())
            .and_then(|s| s.get("value"))
        {
            if !matches!(status.as_str(), Some("loaded" | "sleeping")) {
                continue;
            }
        }
        if let Some(id) = row.get("id").and_then(Value::as_str) {
            ids.insert(id.into());
        }
        for alias in row
            .get("aliases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            ids.insert(alias.into());
        }
    }
    for row in body
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for key in ["name", "model"] {
            if let Some(id) = row.get(key).and_then(Value::as_str) {
                ids.insert(id.into());
            }
        }
    }
    ids.into_iter().collect()
}
