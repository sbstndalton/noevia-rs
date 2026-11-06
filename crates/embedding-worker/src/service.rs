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
    fn put<'a>(&'a self, _models: &'a Value) -> Task<'a, Result<(u16, Value), String>> {
        Box::pin(async { Err("embedding service unavailable".into()) })
    }
    fn get(&self) -> Task<'_, Result<(u16, Value), String>>;
}
#[derive(Default)]
pub struct Service {
    last: Option<(u64, Value)>,
    operations: bool,
    last_error: Option<String>,
    next_refresh_ms: Option<u64>,
}
impl Service {
    pub fn supports_operations(&self) -> bool {
        self.operations
    }
    fn observe(&mut self, now: u64, mut body: Value) {
        self.operations =
            body.get("maintenanceProtocol").and_then(Value::as_str) == Some("embedding-service/1");
        if let Some(body) = body.as_object_mut() {
            body.remove("maintenanceProtocol");
        }
        self.last = Some((now, body));
    }
    pub fn observed_at_ms(&self) -> Option<u64> {
        self.last.as_ref().map(|(at, _)| *at)
    }
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
                self.observe(now(), body);
                self.last_error = None;
            }
            Ok((404, _)) if self.last.is_none() => self.last_error = Some("notManaged".into()),
            Ok((status, _)) => self.last_error = Some(format!("HTTP {status}")),
            Err(_) => self.last_error = Some("embedding HTTP unavailable".into()),
        }
        self.last.as_ref().map(|(_, body)| body.clone())
    }
    /// Mutation and subsequent polling share the same exclusive actor turn.
    /// Remote bodies/errors are deliberately not copied to public diagnostics.
    pub async fn serve(
        &mut self,
        loader: &impl Loader,
        models: &Value,
        now: impl Fn() -> u64,
    ) -> Result<Option<Value>, MutationError> {
        let (status, body) = loader.put(models).await.map_err(|_| MutationError {
            status: 503,
            code: "serviceRefused",
            message: "The embedding service could not be changed.".into(),
        })?;
        if !(200..300).contains(&status) {
            return Err(MutationError {
                status: match status {
                    400 | 503 => status,
                    _ => 502,
                },
                code: "serviceRefused",
                message: format!("The embedding service could not be changed: HTTP {status}"),
            });
        }
        if body.get("managed") == Some(&Value::Bool(true)) {
            self.observe(now(), body);
        }
        Ok(self.status(now()))
    }
    /// Deadline includes polling sleeps. Each loader GET is independently bounded
    /// by its transport. Cancellation of a caller never cancels the owner turn.
    pub async fn wait_loaded(
        &mut self,
        loader: &impl Loader,
        names: &[String],
        timeout: std::time::Duration,
        now: impl Fn() -> u64,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let body = self.refresh(loader, &now).await;
            if body.as_ref().is_some_and(|b| {
                b.pointer("/running/loaded")
                    .and_then(Value::as_array)
                    .is_some_and(|loaded| {
                        names
                            .iter()
                            .all(|name| loaded.iter().any(|v| v.as_str() == Some(name)))
                    })
            }) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + std::time::Duration::from_secs(2)),
            )
            .await;
        }
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

/// Node resolveModelFile parity: companion rows and non-GGUF files are ignored;
/// aliases, ids, names and stems are accepted only from the loader inventory.
pub fn installed_file(body: &Value, model: &str) -> Option<String> {
    let rows = body
        .as_array()
        .or_else(|| body.get("models").and_then(Value::as_array))?;
    rows.iter().find_map(|row| {
        if row.get("companion").is_some_and(|v| match v {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().is_some_and(|v| v != 0.0),
            Value::String(s) => !s.is_empty(),
            _ => true,
        }) {
            return None;
        }
        let file = row.get("file")?.as_str()?;
        if !file
            .get(file.len().saturating_sub(5)..)
            .is_some_and(|s| s.eq_ignore_ascii_case(".gguf"))
        {
            return None;
        }
        let matches = ["modelId", "name", "stem"]
            .into_iter()
            .any(|key| row.get(key).and_then(Value::as_str) == Some(model))
            || row
                .get("sections")
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|v| v.as_str() == Some(model)));
        matches.then(|| file.to_owned())
    })
}

#[derive(Debug)]
pub struct MutationError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

/// Preserve every model either projects or the Diary may still search with.
/// Unknown-address Diary targets are conservatively retained. Required active
/// and next targets may overflow (None); rollback targets use remaining room.
/// Call under the same owner turn as the eventual serve operation.
pub fn served_set(
    service: &Service,
    base: Option<&str>,
    projects: &Value,
    diary: &Value,
    diary_defaults: bool,
) -> Option<Vec<Value>> {
    let mut out = Vec::new();
    let mut add = |target: &Value, diary_side: bool| {
        let Some(model) = target
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return true;
        };
        let address = target
            .get("baseUrl")
            .or_else(|| target.get("base_url"))
            .and_then(Value::as_str);
        let on_service =
            base.is_some_and(|base| !base.is_empty() && same_address(Some(base), address));
        let unknown_diary =
            diary_side && diary_defaults && address.is_none_or(|s| s.trim().is_empty());
        if !on_service && !unknown_diary {
            return true;
        }
        if out
            .iter()
            .any(|v: &Value| v.get("model").and_then(Value::as_str) == Some(model))
        {
            return true;
        }
        if let Some(file) = service.file_of(model) {
            out.push(serde_json::json!({"model":model,"file":file}));
        }
        out.len() <= 2
    };
    for (state, field, diary_side) in [
        (projects, "active", false),
        (diary, "active", true),
        (projects, "next", false),
        (diary, "next", true),
    ] {
        if !add(state.get(field).unwrap_or(&Value::Null), diary_side) {
            return None;
        }
    }
    for (state, diary_side) in [(projects, false), (diary, true)] {
        if out.len() >= 2 {
            break;
        }
        let target = state.get("previous").unwrap_or(&Value::Null);
        let Some(model) = target
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let address = target
            .get("baseUrl")
            .or_else(|| target.get("base_url"))
            .and_then(Value::as_str);
        if !(base.is_some_and(|base| !base.is_empty() && same_address(Some(base), address))
            || diary_side && diary_defaults && address.is_none_or(|s| s.trim().is_empty()))
        {
            continue;
        }
        if !out
            .iter()
            .any(|v| v.get("model").and_then(Value::as_str) == Some(model))
        {
            if let Some(file) = service.file_of(model) {
                out.push(serde_json::json!({"model":model,"file":file}));
            }
        }
    }
    Some(out)
}
