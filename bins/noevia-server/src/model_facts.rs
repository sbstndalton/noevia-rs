//! Effective model facts. Rust owns the boot/live budget; the settings row is only its boot input.
use serde_json::{json, Value};
use server_store::rusqlite::{Connection, OptionalExtension};
use std::{path::PathBuf, sync::Mutex};

pub const FILE: &str = "model-facts.json";
pub const PROTOCOL: &str = "model-facts/1";
pub const MAX_AGE_MS: i64 = 15_000;
#[derive(Debug)]
struct State {
    saved: Option<f64>,
    generation: u64,
}
type ScanReply = Option<Option<js_json::JValue>>;
type PendingScan = (String, tokio::sync::watch::Sender<ScanReply>);
#[derive(Debug)]
pub struct Authority {
    root: Option<PathBuf>,
    start: String,
    default: f64,
    meminfo: PathBuf,
    state: Mutex<State>,
    sidecar_names: Mutex<Option<(Vec<String>, String)>>,
    scan: Mutex<Option<PendingScan>>,
}
fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&Value::Null)
}
fn number(v: &str) -> Option<f64> {
    llamacpp_autoconfig::string_to_number(&v.encode_utf16().collect::<Vec<_>>()).ok()
}
fn round1(n: f64) -> f64 {
    llamacpp_autoconfig::js_round(n * 10.0) / 10.0
}
pub fn range(text: Option<&str>) -> Value {
    let host = text.and_then(|s| {
        s.lines().find_map(|l| {
            let mut p = l.split_whitespace();
            (p.next()? == "MemTotal:").then_some(())?;
            let n = p.next()?.parse::<f64>().ok()?;
            (n > 0.0 && p.next() == Some("kB")).then_some(round1(n / 1048576.0))
        })
    });
    json!({"minGib":2,"maxGib":host.map(|h|(h-4.0).floor().max(2.0)).unwrap_or(1024.0),"hostRamGib":host})
}
impl Authority {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Self {
        let root = get("UI_DATA_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let saved = root.as_ref().and_then(|p| {
            let c = Connection::open(p.join("cowork.db")).ok()?;
            let s: Option<String> = c
                .query_row(
                    "SELECT value FROM settings WHERE key='inference:memory-budget'",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .ok()?;
            let v: Value = serde_json::from_str(&s?).ok()?;
            let n = crate::model_maint::budget_watch::js_number(v.get("budgetGib"));
            (n > 0.0).then_some(n)
        });
        Self {
            root,
            start: get("NOEVIA_START_ID").unwrap_or_default(),
            default: get("INFERENCE_MEMORY_BUDGET_GIB")
                .and_then(|s| number(&s))
                .filter(|n| n.is_finite() && *n > 0.0)
                .unwrap_or(16.0),
            meminfo: get("NOEVIA_TEST_MEMINFO_PATH")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/proc/meminfo")),
            sidecar_names: Mutex::new(None),
            scan: Mutex::new(None),
            state: Mutex::new(State {
                saved,
                generation: 0,
            }),
        }
    }
    fn figure(&self, state: &State) -> Value {
        let text = std::fs::read_to_string(&self.meminfo).ok();
        let r = range(text.as_deref());
        let raw = state.saved.unwrap_or(self.default);
        let budget = raw
            .max(2.0)
            .min(field(&r, "maxGib").as_f64().unwrap_or(1024.0));
        json!({"budgetGib":budget,"source":if state.saved.is_some(){"admin"}else{"deployment"},"defaultGib":self.default,"minGib":field(&r, "minGib"),"maxGib":field(&r, "maxGib"),"hostRamGib":field(&r, "hostRamGib"),"limited":budget!=raw})
    }
    pub fn set_sidecars(&self, names: Option<(Vec<String>, String)>) {
        if let Ok(mut n) = self.sidecar_names.lock() {
            *n = names;
        }
    }
    pub fn budget(&self) -> Option<Value> {
        let snap = snapshot(self.root.as_ref()?, &self.start)?;
        let budget = snap.get("budget")?;
        budget
            .get("budgetGib")?
            .as_f64()
            .filter(|n| n.is_finite() && *n > 0.0)?;
        Some(budget.clone())
    }
    pub fn publish(&self) -> Result<(), String> {
        let state = self.state.lock().map_err(|_| "budget unavailable")?;
        self.publish_locked(&state)
    }
    fn publish_locked(&self, state: &State) -> Result<(), String> {
        let root = self.root.as_ref().ok_or("budget directory unavailable")?;
        if self.start.is_empty() {
            return Err("budget start unavailable".into());
        }
        let sidecars = self
            .sidecar_names
            .lock()
            .map_err(|_| "sidecar facts unavailable")?;
        let body = json!({"protocol":PROTOCOL,"start":self.start,"generation":state.generation,"at":crate::identity::now_ms(),"budget":self.figure(state),"sidecarNames":sidecars.as_ref().map(|n|&n.0),"configRevision":sidecars.as_ref().map(|n|&n.1)});
        std::fs::create_dir_all(root).map_err(|_| "budget directory unavailable")?;
        let tmp = root.join("model-facts.json.tmp");
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options
            .open(&tmp)
            .map_err(|_| "budget snapshot unavailable")?;
        f.write_all(body.to_string().as_bytes())
            .and_then(|_| f.sync_all())
            .map_err(|_| "budget snapshot unavailable")?;
        std::fs::rename(tmp, root.join(FILE)).map_err(|_| "budget snapshot unavailable".to_owned())
    }
    pub fn save(&self, body: &Value) -> Result<Value, (u16, Value)> {
        let unavailable = || {
            (
                503,
                json!({"error":"The inference memory budget is unavailable right now. Try again shortly."}),
            )
        };
        let mut state = self.state.lock().map_err(|_| unavailable())?;
        let r = self.figure(&state);
        let max = field(&r, "maxGib").as_f64().unwrap_or(1024.0);
        let n = body.get("budgetGib").and_then(Value::as_f64).filter(|n|n.is_finite() && round1(*n)==*n && *n>=2.0 && *n<=max).ok_or_else(||(400,json!({"error":format!("Enter a budget from 2 to {max} GiB, in steps of 0.1."),"errorId":"invalidBudget","minGib":2,"maxGib":max})))?;
        let root = self.root.as_ref().ok_or_else(unavailable)?;
        let c = Connection::open(root.join("cowork.db")).map_err(|_| unavailable())?;
        c.busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| unavailable())?;
        c.execute("INSERT INTO settings(key,value) VALUES('inference:memory-budget',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[json!({"budgetGib":n}).to_string()]).map_err(|_|unavailable())?;
        let old_saved = state.saved;
        let old_generation = state.generation;
        let next_generation = state.generation.checked_add(1).ok_or_else(unavailable)?;
        state.saved = Some(n);
        state.generation = next_generation;
        if self.publish_locked(&state).is_err() {
            state.saved = old_saved;
            state.generation = old_generation;
            return Err(unavailable());
        }
        Ok(self.figure(&state))
    }
}
pub fn snapshot(root: &std::path::Path, start: &str) -> Option<Value> {
    let bytes = std::fs::read(root.join(FILE)).ok()?;
    if bytes.len() > 1024 * 1024 {
        return None;
    }
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    let age = crate::identity::now_ms().checked_sub(v.get("at")?.as_i64()?)?;
    (field(&v, "protocol") == PROTOCOL
        && !start.is_empty()
        && field(&v, "start") == start
        && field(&v, "generation").as_u64().is_some()
        && (0..=MAX_AGE_MS).contains(&age))
    .then_some(v)
}

/// Raw configuration remains with its owners; all derived protection names are computed here.
pub async fn sidecars(
    app: &crate::App,
    first: Option<&str>,
) -> Option<(Vec<String>, Vec<String>, String)> {
    let (status, bytes) = app
        .config
        .research
        .node_call(
            "/internal/model-config",
            &json!({"firstLoaded":first}),
            std::time::Duration::from_secs(4),
        )
        .await?;
    if status != 200 {
        return None;
    }
    let raw: Value = serde_json::from_slice(&bytes).ok()?;
    let env = field(&raw, "env");
    let mut names = std::collections::BTreeSet::new();
    let add = |set: &mut std::collections::BTreeSet<String>, v: &Value| {
        if let Some(s) = v
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("default"))
        {
            set.insert(s.to_owned());
        }
    };
    let embed = field(&raw, "embedding");
    let fresh = embed.is_object() && field(field(embed, "status"), "stale") == false;
    if !fresh {
        let seed = if field(env, "EMBEDDING_MODEL")
            .as_str()
            .is_some_and(|s| !s.is_empty())
        {
            field(env, "EMBEDDING_MODEL")
        } else {
            field(env, "EMBED_MODEL")
        };
        add(&mut names, seed);
        add(&mut names, field(env, "EMBEDDING_NEXT_MODEL"));
        if let Some(a) = field(field(&raw, "config"), "names").as_array() {
            for v in a {
                add(&mut names, v);
            }
        }
    } else if let Some(a) = field(field(&raw, "config"), "targets").as_array() {
        let norm = |v: &Value| {
            v.as_str()
                .unwrap_or("")
                .trim()
                .trim_end_matches('/')
                .trim_end_matches("/v1")
                .to_owned()
        };
        for v in a {
            if norm(field(v, "baseUrl")) != norm(field(embed, "baseUrl")) {
                add(&mut names, field(v, "model"));
            }
        }
    }
    if let Some(a) = field(embed, "protected").as_array() {
        for v in a {
            add(&mut names, v);
        }
    }
    let on = |v: &Value| {
        let text = match v {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        };
        matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on"
        )
    };
    let add_rerank = |set: &mut std::collections::BTreeSet<String>, v: &Value| {
        if let Some(s) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            set.insert(s.to_owned());
        }
    };
    if on(field(env, "NOEVIA_FEATURE_RAG_RERANK")) {
        add_rerank(&mut names, field(env, "RERANK_MODEL"));
    }
    let rerank = field(&raw, "rerank");
    if !rerank.is_null() {
        let flag = field(rerank, "NOEVIA_FEATURE_RAG_RERANK");
        if !rerank.is_object()
            || (!flag.is_null() && !flag.is_string() && !flag.is_boolean() && !flag.is_number())
            || (!field(rerank, "RERANK_MODEL").is_null()
                && !field(rerank, "RERANK_MODEL").is_string())
        {
            return None;
        }
        if on(flag) {
            add_rerank(&mut names, field(rerank, "RERANK_MODEL"));
        }
    }
    let loaded = if fresh && field(field(embed, "status"), "enabled") != false {
        field(field(field(embed, "status"), "running"), "loaded")
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Some((
        names.into_iter().collect(),
        loaded,
        raw.get("revision")?.as_str()?.to_owned(),
    ))
}

// Coalesce only in-flight cold reads. G3 remains the sole persistent cache/generation owner.
async fn shared_scan(app: &crate::App, token: &str) -> Option<js_json::JValue> {
    let mut receiver = {
        let mut pending = app.config.model_facts.scan.lock().ok()?;
        if let Some((_, sender)) = pending
            .as_ref()
            .filter(|(generation, sender)| generation == token && sender.borrow().is_none())
        {
            sender.subscribe()
        } else {
            let (sender, receiver) = tokio::sync::watch::channel(None);
            *pending = Some((token.to_owned(), sender.clone()));
            let authority = std::sync::Arc::clone(&app.config.model_facts);
            let loader = app.config.model_manager_scan.clone();
            tokio::spawn(async move {
                let result = loader
                    .facts_get("/api/v1/models", 8)
                    .await
                    .filter(|(s, _)| s.is_success())
                    .and_then(|(_, v)| v)
                    .map(|v| v.get("models").clone());
                let _ = sender.send(Some(result));
                if let Ok(mut pending) = authority.scan.lock() {
                    if pending
                        .as_ref()
                        .is_some_and(|(_, current)| current.same_channel(&sender))
                    {
                        *pending = None;
                    }
                };
            });
            receiver
        }
    };
    let result = receiver
        .wait_for(|result| result.is_some())
        .await
        .ok()?
        .clone()
        .flatten();
    drop(receiver);
    result
}

pub async fn installed(
    app: &crate::App,
    ids: &[String],
    scan: bool,
    first: Option<&str>,
) -> Option<js_json::JValue> {
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        installed_inner(app, ids, scan, first),
    )
    .await
    .ok()
    .flatten()
}

async fn installed_inner(
    app: &crate::App,
    ids: &[String],
    scan: bool,
    first: Option<&str>,
) -> Option<js_json::JValue> {
    use js_json::JValue;
    let (names, mut loaded, revision) = sidecars(app, first).await?;
    if ids.iter().any(|id| names.iter().any(|n| n == id.trim())) {
        if let Some((status, Some(body))) = app
            .config
            .model_manager_scan
            .facts_get("/api/v1/backends", 4)
            .await
        {
            if status.is_success() {
                if let JValue::Arr(rows) = body.get("backends") {
                    for row in rows {
                        if row.get("status").as_str() == Some("running") {
                            if let Some(s) = row.get("loaded_model").as_str() {
                                loaded.extend(
                                    s.split(',')
                                        .map(str::trim)
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_owned),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    let mut files = JValue::Null;
    if scan {
        let before = crate::model_manager_scan::cache(app, "snapshot", &json!({})).await?;
        let token = before.get("token").as_str()?;
        let hit = before.get("hit").get("body").get("models");
        let candidate = if matches!(hit, JValue::Arr(_)) {
            Some(hit.clone())
        } else {
            shared_scan(app, token).await
        };
        if sidecars(app, None).await?.2 != revision {
            return None;
        }
        // This is the final await before projection: invalidation during any earlier read wins.
        let after = crate::model_manager_scan::cache(app, "snapshot", &json!({})).await?;
        if Some(token) == after.get("token").as_str() {
            if let Some(JValue::Arr(rows)) = candidate {
                if !rows.is_empty() {
                    files = JValue::Arr(
                        rows.into_iter()
                            .map(|f| {
                                JValue::obj([
                                    ("modelId", f.get("modelId").clone()),
                                    ("sections", f.get("sections").clone()),
                                    ("bytes", f.get("bytes").clone()),
                                    ("shape", f.get("shape").clone()),
                                ])
                            })
                            .collect(),
                    );
                }
            }
        }
    }
    // Configuration owners can commit while backend/scan reads are in flight. Never publish
    // protection derived from a superseded configuration to an admission consumer.
    if !scan && sidecars(app, None).await?.2 != revision {
        return None;
    }
    Some(JValue::obj([
        (
            "sidecarNames",
            JValue::Arr(names.into_iter().map(JValue::Str).collect()),
        ),
        (
            "sidecarLoaded",
            JValue::Arr(loaded.into_iter().map(JValue::Str).collect()),
        ),
        ("scan", files),
    ]))
}
