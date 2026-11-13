//! Standalone startup loader, including Python YAML and environment semantics.
use crate::value::{integer, repr, strip, Kind, Value};
use std::{collections::BTreeMap, path::Path};
pub type Result<T> = std::result::Result<T, String>;
#[derive(Debug)]
pub struct Config {
    pub data: Value,
    pub warnings: Vec<String>,
}
pub fn get(data: &Value, dotted: &str) -> Option<Value> {
    dotted
        .split('.')
        .try_fold(data.clone(), |node, key| node.get(key))
}
fn set(data: &Value, dotted: &str, value: Value) -> Result<()> {
    let mut node = data.clone();
    let mut parts = dotted.split('.').peekable();
    while let Some(key) = parts.next() {
        if parts.peek().is_none() {
            return node.set(key, value);
        }
        if node.entries().is_none() {
            return Err(format!(
                "AttributeError: '{}' object has no attribute 'setdefault'",
                node.type_name()
            ));
        }
        let child = match node.get(key) {
            Some(v) => v,
            None => {
                let v = Value::map();
                node.set(key, v.clone())?;
                v
            }
        };
        node = child;
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
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    let mut data = if exists(path)? {
        load_yaml(path)?
    } else {
        Value::map()
    };
    if !data.truthy() {
        data = Value::map();
    }
    let mut warnings = Vec::new();
    let decimal_pattern = regex::Regex::new(r"^[\p{Nd}_]+$").ok();
    for &(key, dotted) in ENV {
        if let Some(value) = env.get(key) {
            let converted = match key {
                "DIARY_PORT" => {
                    let s = value.trim();
                    let digits = s
                        .strip_prefix('-')
                        .or_else(|| s.strip_prefix('+'))
                        .unwrap_or(s);
                    if digits.is_empty()
                        || digits.starts_with('_')
                        || digits.ends_with('_')
                        || digits.contains("__")
                        || !decimal_pattern.as_ref().is_some_and(|r| r.is_match(digits))
                    {
                        return Err(format!(
                            "ValueError: invalid literal for int() with base 10: {}",
                            repr(value)
                        ));
                    }
                    integer(s, 10)?
                }
                "DIARY_INDEX_ENABLED" => Value::new(Kind::Bool(
                    !["0", "false", "no", "off", ""]
                        .contains(&strip(value).to_lowercase().as_str()),
                )),
                _ => Value::string(value),
            };
            set(&data, dotted, converted)?;
        }
    }
    if env.contains_key("CORPUS_REMOTE_ROOT") && !env.contains_key("CORPUS_ROOT") {
        warnings.push("CORPUS_REMOTE_ROOT is deprecated; use CORPUS_ROOT".into());
    }
    fn model(data: &Value, path: &str) -> Result<String> {
        let v = get(data, path).unwrap_or_else(Value::null);
        if !v.truthy() {
            return Ok(String::new());
        }
        v.as_str().map(|s| strip(&s).to_owned()).ok_or_else(|| {
            format!(
                "AttributeError: '{}' object has no attribute 'strip'",
                v.type_name()
            )
        })
    }
    let with_warnings = |error: String| {
        format!(
            "{}{error}",
            warnings
                .iter()
                .map(|w| format!("{w}\n"))
                .collect::<String>()
        )
    };
    let aux = model(&data, "llm.aux.model").map_err(with_warnings)?;
    let chat = model(&data, "llm.chat_model").map_err(with_warnings)?;
    if (aux.is_empty() || aux == "default") && !chat.is_empty() && chat != "default" {
        set(&data, "llm.aux.model", Value::string(&chat)).map_err(with_warnings)?;
        warnings.push(format!("DIARY_AUX_MODEL is unset (or 'default'); falling back to the chat model {}. Set DIARY_AUX_MODEL to a smaller model the endpoint serves to make summarising and skip-classification cheaper.",repr(&chat)));
    }
    Ok(Config { data, warnings })
}
fn exists(path: &Path) -> Result<bool> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) || e.raw_os_error() == Some(40) =>
        {
            Ok(false)
        }
        Err(e) => Err(crate::restore::io(e, path)),
    }
}
pub fn load_yaml(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path).map_err(|e| crate::restore::io(e, path))?;
    crate::yaml::load(&bytes)
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
            .is_some_and(|v| !strip(v).is_empty())
    {
        return Ok(None);
    }
    if env.get("DIARY_ALLOW_OPEN").is_some_and(|v| strip(v) == "1") {
        return Ok(Some(OPEN_WARNING));
    }
    Err("Refusing to start: DIARY_AUTH_TOKEN and DIARY_TENANT_KEY are both empty. Set them, or set DIARY_ALLOW_OPEN=1 to run the LAN-only open mode deliberately.".into())
}
pub fn system_prompt(data: &Value, config_dir: &Path) -> Result<String> {
    let custom=match get(data,"prompts.system_path").filter(Value::truthy){
        Some(value)=>Some(value.as_str().ok_or_else(||format!("TypeError: argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{}'",value.type_name()))?),
        None=>None,
    };
    let custom = custom.as_deref().map(Path::new);
    let default = config_dir.join("prompts/system.md");
    let path = match custom {
        Some(p) if exists(p)? => p,
        _ => &default,
    };
    crate::value::decode_utf8(&std::fs::read(path).map_err(|e| crate::restore::io(e, path))?)
        .map(|s| s.replace("\r\n", "\n").replace('\r', "\n"))
}
