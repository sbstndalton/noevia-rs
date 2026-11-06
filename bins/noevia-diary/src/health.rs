//! The process part of GET /api/health: the served-model resolution of llm.py
//! (`LLMClient.resolve_chat_model`, #112/#2099) for the chat and aux clients, sharing one listing
//! per engine per check. This lists models only; it never loads one.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const SERVED_TTL: Duration = Duration::from_secs(60);
pub const LIST_TIMEOUT: Duration = Duration::from_secs(3);
const NON_CHAT_FLAGS: [&str; 4] = ["--embedding", "--embeddings", "--reranking", "--rerank"];

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClientConfig {
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub chat_model: String,
    #[serde(default)]
    pub fallback_model: String,
    pub setting_key: String,
    pub fallback_setting_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    Unreachable,
    NotServed,
    NoneServed,
}

#[derive(Debug, Clone)]
pub enum Listing {
    Entries(Vec<(String, bool)>),
    Unreachable,
    /// Listed, but not usable (HTTP status, bad JSON or shape): the configured id is kept.
    Unusable,
}

pub struct Client {
    pub cfg: ClientConfig,
    base: String,
    resolved: Mutex<Option<(Instant, String, &'static str)>>,
}

/// Is every model endpoint a plain http:// URL this process can list (the deployed engines
/// are)? Otherwise health stays with Python.
pub fn supported(clients: &[&ClientConfig]) -> bool {
    clients.iter().all(|c| {
        let b = c.base_url.trim_end_matches('/');
        b.starts_with("http://") && b.len() > "http://".len() && !b.contains(char::is_whitespace)
    })
}

impl Client {
    pub fn new(cfg: ClientConfig) -> Self {
        let base = cfg.base_url.trim_end_matches('/').to_string();
        Client {
            cfg,
            base,
            resolved: Mutex::new(None),
        }
    }

    fn memo_key(&self) -> (String, String) {
        let auth = if self.cfg.api_key.is_empty() {
            String::new()
        } else {
            format!("Bearer {}", self.cfg.api_key)
        };
        (self.base.clone(), auth)
    }
}

/// `_served_entries`: (id, chat) pairs, or None for an unexpected shape.
pub fn served_entries(data: &Value) -> Option<Vec<(String, bool)>> {
    let items = data.as_object()?.get("data")?.as_array()?;
    let mut out = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let Some(id) = obj
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let args = obj
            .get("status")
            .and_then(Value::as_object)
            .and_then(|s| s.get("args"))
            .and_then(Value::as_array);
        let non_chat = args.is_some_and(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .any(|s| NON_CHAT_FLAGS.contains(&s))
        });
        out.push((id.to_string(), !non_chat));
    }
    Some(out)
}

async fn fetch(http: &reqwest::Client, client: &Client) -> Listing {
    let mut req = http
        .get(format!("{}/models", client.base))
        .timeout(LIST_TIMEOUT);
    if !client.cfg.api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", client.cfg.api_key));
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(_) => return Listing::Unreachable,
    };
    if !resp.status().is_success() {
        return Listing::Unusable;
    }
    let Ok(bytes) = resp.bytes().await else {
        return Listing::Unreachable;
    };
    match serde_json::from_slice::<Value>(&bytes)
        .ok()
        .as_ref()
        .and_then(served_entries)
    {
        Some(entries) => Listing::Entries(entries),
        None => Listing::Unusable,
    }
}

pub type Memo = HashMap<(String, String), Listing>;

async fn listing(http: &reqwest::Client, client: &Client, memo: &mut Memo) -> Listing {
    let key = client.memo_key();
    if let Some(l) = memo.get(&key) {
        return l.clone();
    }
    let l = fetch(http, client).await;
    memo.insert(key, l.clone());
    l
}

fn cached(client: &Client) -> Option<(String, &'static str)> {
    let guard = client.resolved.lock().ok()?;
    match guard.as_ref() {
        Some((until, model, source)) if *until > Instant::now() => Some((model.clone(), source)),
        _ => None,
    }
}

fn remember(client: &Client, value: Option<(String, &'static str)>) {
    if let Ok(mut guard) = client.resolved.lock() {
        *guard = value.map(|(m, s)| (Instant::now() + SERVED_TTL, m, s));
    }
}

/// `resolve_chat_model` for one client; `partner` is the chat client for an aux client on the
/// same engine (`prefer_model_from`).
pub async fn resolve(
    http: &reqwest::Client,
    client: &Client,
    partner: Option<&Client>,
    memo: &mut Memo,
) -> Result<(String, &'static str), Failure> {
    if let Some(hit) = cached(client) {
        return Ok(hit);
    }
    let entries = match listing(http, client, memo).await {
        Listing::Unreachable => {
            remember(client, None);
            return Err(Failure::Unreachable);
        }
        Listing::Unusable => {
            if crate::py::strip(&client.cfg.chat_model).is_empty() {
                remember(client, None);
                return Err(Failure::NotServed);
            }
            let chosen = (client.cfg.chat_model.clone(), "unverified");
            remember(client, Some(chosen.clone()));
            return Ok(chosen);
        }
        Listing::Entries(e) => e,
    };
    let chat: Vec<&str> = entries
        .iter()
        .filter(|(_, c)| *c)
        .map(|(id, _)| id.as_str())
        .collect();
    if chat.is_empty() {
        return Err(Failure::NoneServed);
    }
    let chosen = if chat.contains(&client.cfg.chat_model.as_str()) {
        Some((client.cfg.chat_model.clone(), "configured"))
    } else if !client.cfg.fallback_model.is_empty()
        && chat.contains(&client.cfg.fallback_model.as_str())
    {
        Some((client.cfg.fallback_model.clone(), "fallback"))
    } else {
        let mut partner_model = String::new();
        if let Some(p) = partner {
            if let Ok((m, _)) = Box::pin(resolve(http, p, None, memo)).await {
                partner_model = m;
            }
        }
        (!partner_model.is_empty() && chat.contains(&partner_model.as_str()))
            .then_some((partner_model, "fallback"))
    };
    match chosen {
        Some(c) => {
            remember(client, Some(c.clone()));
            Ok(c)
        }
        None => {
            remember(client, None);
            Err(Failure::NotServed)
        }
    }
}

#[derive(Serialize)]
pub struct Health {
    pub ok: bool,
    pub auth_required: bool,
    pub model: Value,
    pub model_configured: Value,
    pub model_source: &'static str,
    pub model_warnings: Vec<String>,
}

pub struct Models {
    pub main: Client,
    pub aux: Client,
    pub aux_prefers_main: bool,
    pub configured: Value,
}

/// `_health_model` plus the two process fields.
pub async fn health(http: &reqwest::Client, models: &Models, auth_required: bool) -> Health {
    let mut memo = Memo::new();
    let main = resolve(http, &models.main, None, &mut memo).await;
    let partner = models.aux_prefers_main.then_some(&models.main);
    let aux = resolve(http, &models.aux, partner, &mut memo).await;
    let mut warnings = Vec::new();
    for (client, outcome) in [(&models.main, &main), (&models.aux, &aux)] {
        if matches!(outcome, Err(Failure::NotServed)) || matches!(outcome, Ok((_, "fallback"))) {
            warnings.push(client.cfg.setting_key.clone());
        }
    }
    let (model, source) = match main {
        Ok((m, s)) => (Value::String(m), s),
        Err(f) => (
            models.configured.clone(),
            match f {
                Failure::Unreachable => "unreachable",
                Failure::NotServed => "not_served",
                Failure::NoneServed => "none_served",
            },
        ),
    };
    Health {
        ok: true,
        auth_required,
        model,
        model_configured: models.configured.clone(),
        model_source: source,
        model_warnings: warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_follow_python_shape() {
        let v: Value = serde_json::from_str(
            r#"{"data":[{"id":"a"},{"id":""},{"id":"e","status":{"value":"loaded","args":["--embedding"]}},7,{"id":"b","status":{"args":"x"}}]}"#,
        )
        .unwrap_or_default();
        assert_eq!(
            served_entries(&v),
            Some(vec![
                ("a".into(), true),
                ("e".into(), false),
                ("b".into(), true)
            ])
        );
        assert_eq!(served_entries(&serde_json::json!({"data": {}})), None);
    }
}

/// Runtime facts remain owned by the embedding service until its port. No counts or slot
/// metadata are trusted from the forwarder; all persisted detail is read here without creation.
#[derive(Deserialize)]
struct RetrievalFacts {
    model: String,
    profile: String,
    vec_available: bool,
}
#[derive(Serialize)]
pub struct RetrievalStats {
    chunks: i64,
    vec_available: bool,
    dim: Option<Value>,
    model: String,
    profile: String,
}
#[derive(Serialize)]
pub struct TenantDetail {
    journal_pending: i64,
    journal_quarantined: i64,
    retrieval: RetrievalStats,
}
#[derive(Serialize)]
pub struct Response {
    #[serde(flatten)]
    pub process: Health,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<TenantDetail>,
}
pub fn tenant_detail(path: &std::path::Path, facts: &str) -> Option<TenantDetail> {
    use rusqlite::{Connection, OpenFlags, OptionalExtension};
    use sha2::{Digest, Sha256};
    let facts: RetrievalFacts = serde_json::from_str(facts).ok()?;
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let count = |sql| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).ok();
    let journal_pending = count("SELECT COUNT(*) FROM journal WHERE applied = 0")?;
    let journal_quarantined = count(
        "SELECT COUNT(*) FROM journal WHERE applied = 1 AND last_error LIKE 'quarantined:%'",
    )?;
    let chunks = count("SELECT COUNT(*) FROM chunks")?;
    let mut dim = None;
    if facts.vec_available {
        let id = format!(
            "{:x}",
            Sha256::digest(format!("{}\0{}", facts.profile, facts.model).as_bytes())
        );
        let raw: Option<String> = conn
            .query_row(
                "SELECT value FROM vec_meta WHERE key = ?",
                [format!("slot:{}", id.get(..16)?)],
                |r| r.get(0),
            )
            .optional()
            .ok()?
            .flatten();
        if let Some(raw) = raw {
            // Python accepts unusual JSON numbers and types: decline anything outside the
            // ordinary slot schema rather than return a different dimension.
            let slot: Value = serde_json::from_str(&raw).ok()?;
            let obj = slot.as_object()?;
            let table = obj.get("table").and_then(Value::as_str).unwrap_or("");
            let valid_name = |name: &str, prefix: &str| {
                name.strip_prefix(prefix).is_some_and(|s| {
                    s.len() == 16
                        && s.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
            };
            let valid_table = table == "vec_items" || valid_name(table, "vec_");
            let valid_map = match obj.get("map") {
                None | Some(Value::Null) => true,
                Some(Value::String(s)) => valid_name(s, "vecmap_"),
                _ => return None,
            };
            let d = obj.get("dim").and_then(Value::as_i64);
            if !valid_table || !valid_map {
                return None;
            }
            if d.is_some_and(|d| d > 0 && d <= 65536) || obj.get("dim") == Some(&Value::Bool(true))
            {
                dim = obj.get("dim").cloned();
            } else {
                return None;
            }
        }
    }
    Some(TenantDetail {
        journal_pending,
        journal_quarantined,
        retrieval: RetrievalStats {
            chunks,
            vec_available: facts.vec_available,
            dim,
            model: facts.model,
            profile: facts.profile,
        },
    })
}
