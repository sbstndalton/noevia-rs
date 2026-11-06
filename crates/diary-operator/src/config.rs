//! Standalone startup loader; the Python live service retains ownership until D8.
use serde_json::{Map, Value};
use std::{collections::BTreeMap, path::Path};
pub type Result<T> = std::result::Result<T, String>;
#[derive(Debug)]
pub struct Config {
    pub data: Value,
    pub warnings: Vec<String>,
}
pub fn get<'a>(data: &'a Value, dotted: &str) -> Option<&'a Value> {
    dotted
        .split('.')
        .try_fold(data, |node, key| node.as_object()?.get(key))
}
fn set(data: &mut Value, dotted: &str, value: Value) -> Result<()> {
    let mut node = data;
    let mut parts = dotted.split('.').peekable();
    while let Some(key) = parts.next() {
        if parts.peek().is_none() {
            node.as_object_mut()
                .ok_or("TypeError: config section does not support item assignment")?
                .insert(key.into(), value);
            return Ok(());
        }
        node = node
            .as_object_mut()
            .ok_or("AttributeError: 'non-dict' object has no attribute 'setdefault'")?
            .entry(key)
            .or_insert_with(|| Value::Object(Map::new()));
    }
    Ok(())
}
pub const ENV: &[(&str, &str)] = &[
    ("LLM_BASE_URL", "llm.base_url"),
    ("LLM_API_KEY", "llm.api_key"),
    ("LLM_CHAT_MODEL", "llm.chat_model"),
    ("LLM_EMBED_MODEL", "llm.embed_model"),
    ("LLM_EMBED_BASE_URL", "llm.embed_base_url"),
    ("LLM_EMBED_PROFILE", "llm.embed_profile"),
    ("LLM_EMBED_NEXT_MODEL", "llm.embed_next_model"),
    ("LLM_EMBED_NEXT_BASE_URL", "llm.embed_next_base_url"),
    ("LLM_EMBED_NEXT_PROFILE", "llm.embed_next_profile"),
    ("EMBEDDING_REINDEX_DUTY", "retrieval.reindex_duty"),
    ("LLM_CHAT_FALLBACK_MODEL", "llm.chat_fallback_model"),
    ("LLM_AUX_FALLBACK_MODEL", "llm.aux.fallback_model"),
    ("LLM_AUX_BASE_URL", "llm.aux.base_url"),
    ("LLM_AUX_MODEL", "llm.aux.model"),
    ("LLM_AUX_API_KEY", "llm.aux.api_key"),
    ("LLM_MAX_ANSWER_TOKENS", "llm.max_answer_tokens"),
    ("LLM_MAX_REASONING_TOKENS", "llm.max_reasoning_tokens"),
    ("CORPUS_BACKEND", "corpus.backend"),
    ("CORPUS_ROOT", "corpus.root"),
    ("CORPUS_LOCAL_ROOT", "corpus.local.root"),
    ("WEBDAV_BASE_URL", "corpus.webdav.base_url"),
    ("WEBDAV_USERNAME", "corpus.webdav.username"),
    ("WEBDAV_PASSWORD", "corpus.webdav.password"),
    ("S3_ENDPOINT_URL", "corpus.s3.endpoint_url"),
    ("S3_BUCKET", "corpus.s3.bucket"),
    ("S3_ACCESS_KEY_ID", "corpus.s3.access_key"),
    ("S3_SECRET_ACCESS_KEY", "corpus.s3.secret_key"),
    ("S3_SESSION_TOKEN", "corpus.s3.session_token"),
    ("S3_REGION", "corpus.s3.region"),
    ("S3_PREFIX", "corpus.s3.prefix"),
    ("CORPUS_REMOTE_ROOT", "corpus.root"),
    ("DIARY_MONTH_FILE_TEMPLATE", "corpus.month_file_template"),
    ("DIARY_ENTRY_LAYOUT", "corpus.entry_layout"),
    ("DIARY_ENTRIES_PREFIX", "corpus.entries_prefix"),
    ("DIARY_INDEX_ENABLED", "corpus.index_enabled"),
    ("DB_PATH", "retrieval.db_path"),
    ("DIARY_PORT", "ui.port"),
    ("DIARY_AUTH_TOKEN", "ui.auth_token"),
];
/// `config_dir` is the installed config directory, independent of the current directory.
pub fn load(
    path: Option<&Path>,
    config_dir: &Path,
    env: &BTreeMap<String, String>,
) -> Result<Config> {
    let default = config_dir.join("config.yaml");
    let path = path
        .or_else(|| env.get("DIARY_CONFIG").map(Path::new))
        .unwrap_or(&default);
    let mut data = if path.exists() {
        load_yaml(path)?
    } else {
        Value::Object(Map::new())
    };
    if data.is_null()
        || data == Value::Bool(false)
        || data == Value::String(String::new())
        || data == serde_json::json!([])
    {
        data = Value::Object(Map::new());
    }
    let mut warnings = Vec::new();
    for &(key, dotted) in ENV {
        if let Some(value) = env.get(key) {
            let converted = match key {
                "DIARY_PORT" => serde_json::json!(value.trim().parse::<i64>().map_err(
                    |_| format!("ValueError: invalid literal for int() with base 10: {value:?}")
                )?),
                "DIARY_INDEX_ENABLED" => Value::Bool(
                    !["0", "false", "no", "off", ""]
                        .contains(&value.trim().to_lowercase().as_str()),
                ),
                _ => Value::String(value.clone()),
            };
            set(&mut data, dotted, converted)?;
        }
    }
    if env.contains_key("CORPUS_REMOTE_ROOT") && !env.contains_key("CORPUS_ROOT") {
        warnings.push("CORPUS_REMOTE_ROOT is deprecated; use CORPUS_ROOT".into());
    }
    let aux = get(&data, "llm.aux.model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let chat = get(&data, "llm.chat_model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if (aux.is_empty() || aux == "default") && !chat.is_empty() && chat != "default" {
        set(&mut data, "llm.aux.model", Value::String(chat.clone()))?;
        warnings.push(format!("DIARY_AUX_MODEL is unset (or 'default'); falling back to the chat model '{chat}'. Set DIARY_AUX_MODEL to a smaller model the endpoint serves to make summarising and skip-classification cheaper."));
    }
    Ok(Config { data, warnings })
}
pub fn load_yaml(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| e.to_string())?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    serde_yaml::from_str(&text).map_err(|e| e.to_string())
}
pub const OPEN_WARNING: &str =
    "DIARY_ALLOW_OPEN=1: serving without DIARY_AUTH_TOKEN or DIARY_TENANT_KEY (LAN-only mode)";
pub fn enforce_open_mode_policy(
    auth_token: &str,
    env: &BTreeMap<String, String>,
) -> Result<Option<&'static str>> {
    if !auth_token.is_empty()
        || env
            .get("DIARY_TENANT_KEY")
            .is_some_and(|v| !v.trim().is_empty())
    {
        return Ok(None);
    }
    if env.get("DIARY_ALLOW_OPEN").is_some_and(|v| v.trim() == "1") {
        return Ok(Some(OPEN_WARNING));
    }
    Err("Refusing to start: DIARY_AUTH_TOKEN and DIARY_TENANT_KEY are both empty. Set them, or set DIARY_ALLOW_OPEN=1 to run the LAN-only open mode deliberately.".into())
}
pub fn system_prompt(data: &Value, config_dir: &Path) -> Result<String> {
    let custom = get(data, "prompts.system_path")
        .and_then(Value::as_str)
        .map(Path::new);
    let default = config_dir.join("prompts/system.md");
    let path = custom.filter(|p| p.exists()).unwrap_or(&default);
    std::fs::read_to_string(path)
        .map(|s| s.replace("\r\n", "\n").replace('\r', "\n"))
        .map_err(|e| e.to_string())
}
