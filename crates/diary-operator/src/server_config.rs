//! The Diary's main-mode start-up configuration, resolved without the Python child (track R9,
//! shift 4, step 3).
//!
//! In main mode (`noevia-diary --main`) the Python child resolves config.yaml plus the
//! environment and POSTs one JSON object to its Rust parent: `rust_forward.server_config` plus
//! the lifespan's `native_routes`, `native_writers` and (with DIARY_RUST_EMBEDDING=on)
//! `embedding` (app.py). [`main_mode`] builds the same object from the loaded configuration
//! ([`crate::config::load`]) and the same environment, so the parent can do without that POST.
//! Recorded from the Python functions themselves (tests/generate_server_config_fixtures.py).
//!
//! Python semantics kept: raw YAML values pass through unchanged (`null`, numbers, lists), the
//! `x or ""` and `.strip()`/`.rstrip("/")` rules, `float()`/`int()` of the timeout and retry
//! values, `bool()` of `corpus.index_enabled`, `str()` of `retrieval.db_path`, posixpath
//! `dirname`, pathlib's normalisation and `expanduser` of DIARY_EXTERNAL_SOURCES.
//!
//! Named differences (each an `Err` or an absent key, never a different value):
//! - a value Python's start would refuse (`AttributeError`/`TypeError`/`ValueError` while it
//!   builds its clients or this object) is an `Err` here; the other start-up refusals of
//!   `AppState` (journal, index, prompts) are not modelled;
//! - JSON cannot carry what Python's `json.dumps` would write as `NaN`/`Infinity`, integers beyond
//!   64 bits or lone surrogates: an `Err` here;
//! - Python's own start refusals that are modelled: `retrieval.db_path` not a string (app.py's
//!   import), DIARY_CORPUS_WRITE_IMPL other than python/rust, a non-ASCII API key (httpx),
//!   `enforce_open_mode_policy`; `tenant_assertion.startup_checks` is not modelled;
//! - app.py's import-time default of `retrieval.embedding_state_path` is applied (it reaches the
//!   `embedding` subset);
//! - `~user` in DIARY_EXTERNAL_SOURCES, or `~` without HOME, needs the password database:
//!   `external_sources` is then absent (the route stays with the child), where Python resolves it.

use crate::config::get;
use crate::value::{integer, strip, Kind, Value};
use diary_backup_format::python_float;
use serde_json::{json, Map, Value as Json};
use std::collections::BTreeMap;

pub type Result<T> = std::result::Result<T, String>;

/// The per-start values the parent chooses itself.
pub struct Start<'a> {
    pub port: u16,
    pub control_token: &'a str,
    pub forward_secret: &'a str,
}

/// rust_forward.SUPPORTED plus MAIN_ONLY: the route names DIARY_RUST_ROUTES may name in main mode.
pub const ROUTES: &[&str] = &[
    "/api/health",
    "/api/months",
    "/api/day",
    "/api/files",
    "/api/storage-status",
    "/api/workspace-export",
    "/api/external-sources",
    "/api/file",
];

/// rust_parent.EMBEDDING_KEYS.
pub const EMBEDDING_KEYS: &[&str] = &[
    "llm.embed_base_url",
    "llm.embed_model",
    "llm.embed_profile",
    "llm.embed_next_base_url",
    "llm.embed_next_model",
    "llm.embed_next_profile",
    "llm.timeout_s",
    "llm.max_retries",
    "retrieval.db_path",
    "retrieval.embedding_state_path",
    "retrieval.reindex_duty",
];

/// diary_migration.SETTING_KEYS.
const SETTING_KEYS: &[&str] = &[
    "monthly_prefix",
    "entry_layout",
    "entries_prefix",
    "index_file",
    "month_file_template",
    "index_enabled",
];

/// llm.DEFAULT_MAX_REASONING_TOKENS.
const DEFAULT_MAX_REASONING_TOKENS: i64 = 4096;
/// workspace_export_native.BINARY.
const EXPORT_BINARY: &str = "/usr/local/bin/diary-workspace-export";

/// The object the Python child POSTs to `/internal/child/config` in main mode, for `data` (the
/// loaded configuration) and `env` (the process environment both processes share).
pub fn main_mode(data: &Value, env: &BTreeMap<String, String>, start: &Start<'_>) -> Result<Json> {
    let cfg = |dotted: &str| get(data, dotted);
    let env_str = |key: &str| env.get(key).map(String::as_str);
    // app.py at import, before anything else reads the configuration.
    let state_path = embedding_state_default(data)?;

    // AppState: the two clients (llm.LLMClient.__init__ and what server_config reads of them).
    // float()/int() as AppState converts them (a refusal stops the start); whether the value
    // fits JSON matters only where it is sent (`answer`).
    let timeout_s = py_float(&cfg("llm.timeout_s").unwrap_or_else(|| int(300)))?;
    let max_retries = py_int(&cfg("llm.max_retries").unwrap_or_else(|| int(3)))?;
    rstrip_slash(&or_empty(cfg("llm.embed_base_url")))?;
    // Python's start: corpus_write_native.implementation() in AppState.
    let write_impl = match env_str("DIARY_CORPUS_WRITE_IMPL") {
        Some(v) if !v.is_empty() => strip(v).to_lowercase(),
        _ => "python".to_owned(),
    };
    if write_impl != "python" && write_impl != "rust" {
        return Err("ValueError: DIARY_CORPUS_WRITE_IMPL must be python or rust".into());
    }
    let main = client(
        &cfg,
        [
            "llm.base_url",
            "llm.api_key",
            "llm.chat_model",
            "llm.chat_fallback_model",
        ],
        ["LLM_CHAT_MODEL", "LLM_CHAT_FALLBACK_MODEL"],
    )?;
    let aux = client(
        &cfg,
        [
            "llm.aux.base_url",
            "llm.aux.api_key",
            "llm.aux.model",
            "llm.aux.fallback_model",
        ],
        ["LLM_AUX_MODEL", "LLM_AUX_FALLBACK_MODEL"],
    )?;
    // `prefer_model_from` is the main client exactly when the two base URLs are equal (both are
    // strings here: anything else stopped above).
    let aux_prefers_main = py_eq(cfg("llm.aux.base_url"), cfg("llm.base_url"));
    let auth_token = strip_or_empty(cfg("ui.auth_token"))?;

    let db_path = db_path_str(data)?;
    let data_root = if !db_path.is_empty() && db_path != ":memory:" {
        dirname(&db_path)
    } else {
        ".".to_owned()
    };
    let mut corpus = Map::new();
    for key in [
        "monthly_prefix",
        "entry_layout",
        "entries_prefix",
        "index_file",
        "month_file_template",
    ] {
        corpus.insert(key.into(), raw(cfg(&format!("corpus.{key}")))?);
    }
    let index_enabled = match cfg("corpus.index_enabled") {
        Some(v) => v.truthy(),
        None => true,
    };
    corpus.insert("index_enabled".into(), Json::Bool(index_enabled));
    let mut settings = Map::new();
    for key in SETTING_KEYS {
        settings.insert((*key).into(), raw(cfg(&format!("corpus.{key}")))?);
    }
    let export_impl = match env_str("WORKSPACE_EXPORT_IMPL") {
        Some(v) if !strip(v).is_empty() => v.to_owned(),
        _ => "python".to_owned(),
    };
    // The lifespan's enforce_open_mode_policy, before the child sends anything.
    crate::config::enforce_open_mode_policy(&auth_token, env)?;

    let mut out = Map::new();
    out.insert("port".into(), json!(start.port));
    out.insert("control_token".into(), json!(start.control_token));
    out.insert("forward_secret".into(), json!(start.forward_secret));
    out.insert("auth_token".into(), json!(auth_token));
    out.insert(
        "tenant_key".into(),
        json!(strip(env_str("DIARY_TENANT_KEY").unwrap_or(""))),
    );
    out.insert("data_root".into(), json!(data_root));
    out.insert(
        "legacy_user_id".into(),
        json!(env_str("DIARY_LEGACY_USER_ID").unwrap_or("")),
    );
    out.insert("corpus".into(), Json::Object(corpus));
    out.insert(
        "llm".into(),
        json!({"main": main, "aux": aux, "aux_prefers_main": aux_prefers_main,
               "configured": raw(cfg("llm.chat_model"))?}),
    );
    out.insert("corpus_write_impl".into(), json!(write_impl));
    out.insert(
        "workspace_export".into(),
        json!({"impl": export_impl, "binary": EXPORT_BINARY, "settings": settings}),
    );
    if let Some(sources) = external_sources(env) {
        out.insert("external_sources".into(), json!(sources));
    }
    if mode(env_str("DIARY_RUST_ANSWER"), &["shadow", "on"])
        || mode(env_str("DIARY_RUST_INDEX"), &["on", "shadow"])
    {
        if !timeout_s.is_finite() {
            return Err("a non-finite float cannot be sent as JSON".into());
        }
        let max_retries = max_retries.ok_or_else(too_large)?;
        out.insert(
            "answer".into(),
            json!({
                "embedding_seed": {"llm": embed_seed(cfg("llm"))?},
                "timeout_s": timeout_s,
                "max_retries": max_retries,
                "reasoning_tokens": reasoning_tokens(cfg("llm.max_reasoning_tokens"))?,
            }),
        );
    }
    out.insert(
        "native_routes".into(),
        json!(routes(env_str("DIARY_RUST_ROUTES").unwrap_or(""))),
    );
    out.insert(
        "native_writers".into(),
        json!(writers(env_str("DIARY_RUST_WRITES").unwrap_or(""))),
    );
    if strip(env_str("DIARY_RUST_EMBEDDING").unwrap_or("")).to_lowercase() == "on" {
        out.insert("embedding".into(), embedding_config(data, state_path)?);
    }
    Ok(Json::Object(out))
}

/// server_config's `client(c, api_key_setting)` for one LLMClient built as AppState builds it.
fn client(
    cfg: &dyn Fn(&str) -> Option<Value>,
    [base_url, api_key, model, fallback]: [&str; 4],
    [setting_key, fallback_setting_key]: [&str; 2],
) -> Result<Json> {
    // LLMClient: base_url.rstrip("/") (None → AttributeError), (fallback or "").strip().
    let base = match cfg(base_url) {
        Some(v) => rstrip_slash(&v)?,
        None => return Err(attribute_error("NoneType", "rstrip")),
    };
    let key = or_empty(cfg(api_key));
    if key.as_str().is_some_and(|k| !k.is_ascii()) {
        // httpx encodes the Authorization header as ASCII: the client cannot be built.
        return Err("UnicodeEncodeError: the API key is not ASCII".into());
    }
    Ok(json!({
        "base_url": base,
        "api_key": raw(Some(key))?,
        "chat_model": raw(Some(or_empty(cfg(model))))?,
        "fallback_model": strip_or_empty(cfg(fallback))?,
        "setting_key": setting_key,
        "fallback_setting_key": fallback_setting_key,
    }))
}

/// rust_answer/rust_index `mode() != ""`: the stripped, lower-cased value is one of `modes`.
fn mode(value: Option<&str>, modes: &[&str]) -> bool {
    let v = strip(value.unwrap_or("")).to_lowercase();
    modes.contains(&v.as_str())
}

/// rust_answer.answer_config's seed: the `embed_*` keys of `llm` when it is a mapping.
fn embed_seed(llm: Option<Value>) -> Result<Json> {
    let mut seed = Map::new();
    if let Some(rows) = llm.filter(Value::truthy).and_then(|v| v.entries()) {
        for (k, v) in rows {
            if let Some(name) = k.as_str().filter(|n| n.starts_with("embed_")) {
                seed.insert(name, to_json(&v)?);
            }
        }
    }
    Ok(Json::Object(seed))
}

/// app.reasoning_token_allowance: int(str(raw).strip()), the default when that fails or is < 0.
/// A valid value beyond 64 bits cannot be sent here (Err).
fn reasoning_tokens(raw: Option<Value>) -> Result<i64> {
    let literal = match raw.map(|v| v.kind()) {
        None => Literal::Value(DEFAULT_MAX_REASONING_TOKENS),
        Some(Kind::Int(s)) => int_literal(&s),
        Some(Kind::Str(s)) => int_literal(strip(&s)),
        // str() of None, a bool, a float, a list or a mapping is never an int literal.
        Some(_) => Literal::Invalid,
    };
    match literal {
        Literal::Value(v) if v >= 0 => Ok(v),
        Literal::Value(_) | Literal::Invalid => Ok(DEFAULT_MAX_REASONING_TOKENS),
        Literal::Negative => Ok(DEFAULT_MAX_REASONING_TOKENS),
        Literal::TooLarge => Err(too_large()),
    }
}

/// rust_forward.configured_routes(main=True), its keys sorted.
fn routes(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = raw
        .split(',')
        .map(strip)
        .filter(|name| ROUTES.contains(name))
        .map(str::to_owned)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// rust_forward.configured_writers, as app.py sends it: `["*"]`, or the UUIDs sorted.
fn writers(raw: &str) -> Vec<String> {
    let names: Vec<String> = raw
        .split(',')
        .map(|p| strip(p).to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    if names.iter().any(|n| n == "*") {
        return vec!["*".into()];
    }
    let mut out: Vec<String> = names.into_iter().filter(|n| is_uuid(n)).collect();
    out.sort();
    out.dedup();
    out
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(&c),
        })
}

/// app.py's import-time default: without a (truthy) `retrieval.embedding_state_path` it is set to
/// `str(Path(db).parent / "embedding-config.json")` (the current directory for no or an in-memory
/// database). A `retrieval` that is not a mapping fails that assignment, and the child's import.
fn embedding_state_default(data: &Value) -> Result<Option<String>> {
    if get(data, "retrieval.embedding_state_path").is_some_and(|v| v.truthy()) {
        return Ok(None);
    }
    let db = db_path_str(data)?;
    if let Some(v) = data.get("retrieval").filter(|v| v.entries().is_none()) {
        return Err(format!(
            "TypeError: '{}' object does not support item assignment",
            v.type_name()
        ));
    }
    let (root, mut parts) = if !db.is_empty() && db != ":memory:" {
        let (root, mut parts) = parse_path(&db);
        parts.pop();
        (root, parts)
    } else {
        ("", Vec::new())
    };
    parts.push("embedding-config.json");
    Ok(Some(format_path(root, &parts)))
}

/// rust_parent.embedding_config: the nested subset of EMBEDDING_KEYS that are not None, with
/// app.py's default state path (`state_path`) in place.
fn embedding_config(data: &Value, state_path: Option<String>) -> Result<Json> {
    let mut out = Map::new();
    for &key in EMBEDDING_KEYS {
        let Some(value) = get(data, key).filter(|v| !matches!(v.kind(), Kind::Null)) else {
            continue;
        };
        let (parent, leaf) = key.split_once('.').unwrap_or(("", key));
        let node = out
            .entry(parent.to_owned())
            .or_insert_with(|| Json::Object(Map::new()));
        if let Json::Object(map) = node {
            map.insert(leaf.to_owned(), to_json(&value)?);
        }
    }
    if let Some(path) = state_path {
        let node = out
            .entry("retrieval".to_owned())
            .or_insert_with(|| Json::Object(Map::new()));
        if let Json::Object(map) = node {
            map.insert("embedding_state_path".into(), Json::String(path));
        }
    }
    Ok(Json::Object(out))
}

/// rust_forward._external_sources: external_source_paths(), or None when it cannot be resolved.
fn external_sources(env: &BTreeMap<String, String>) -> Option<Vec<String>> {
    let raw = env.get("DIARY_EXTERNAL_SOURCES").map_or("", String::as_str);
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let p = strip(part);
        if p.is_empty() {
            continue;
        }
        let resolved = expanduser(p, env.get("HOME").map(String::as_str))?;
        if !out.contains(&resolved) {
            out.push(resolved);
        }
    }
    Some(out)
}

/// `str(Path(p).expanduser())` (PosixPath, CPython 3.12). None where Python would consult the
/// password database (`~user`, or `~` without HOME).
fn expanduser(p: &str, home: Option<&str>) -> Option<String> {
    let (root, tail) = parse_path(p);
    match tail.first() {
        Some(first) if root.is_empty() && first.starts_with('~') => {
            if *first != "~" {
                return None;
            }
            // os.path.expanduser("~"): HOME without trailing slashes, or "/" when that is empty.
            let home = home?.trim_end_matches('/');
            let home = if home.is_empty() { "/" } else { home };
            if home.starts_with('~') {
                // pathlib: "Could not determine home directory" (the key is then absent).
                return None;
            }
            let (home_root, mut parts) = parse_path(home);
            parts.extend(tail.iter().skip(1).copied());
            Some(format_path(home_root, &parts))
        }
        _ => Some(format_path(root, &tail)),
    }
}

/// PurePosixPath parsing: the root ("", "/" or exactly "//") and the parts without "" and ".".
fn parse_path(p: &str) -> (&'static str, Vec<&str>) {
    let root = if p.starts_with("//") && !p.starts_with("///") {
        "//"
    } else if p.starts_with('/') {
        "/"
    } else {
        ""
    };
    let tail = p
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    (root, tail)
}

fn format_path(root: &str, parts: &[&str]) -> String {
    let s = format!("{root}{}", parts.join("/"));
    if s.is_empty() {
        ".".into()
    } else {
        s
    }
}

/// posixpath.dirname.
fn dirname(p: &str) -> String {
    let head = p.rfind('/').map_or("", |i| p.get(..=i).unwrap_or(""));
    if !head.is_empty() && !head.bytes().all(|b| b == b'/') {
        head.trim_end_matches('/').to_owned()
    } else {
        head.to_owned()
    }
}

fn int(n: i64) -> Value {
    Value::new(Kind::Int(n.to_string()))
}

/// Python `x or ""`.
fn or_empty(v: Option<Value>) -> Value {
    v.filter(Value::truthy).unwrap_or_else(|| Value::string(""))
}

fn attribute_error(type_name: &str, attribute: &str) -> String {
    format!("AttributeError: '{type_name}' object has no attribute '{attribute}'")
}

/// `(x or "").strip()`.
fn strip_or_empty(v: Option<Value>) -> Result<String> {
    let v = or_empty(v);
    v.as_str()
        .map(|s| strip(&s).to_owned())
        .ok_or_else(|| attribute_error(v.type_name(), "strip"))
}

/// `x.rstrip("/")`.
fn rstrip_slash(v: &Value) -> Result<String> {
    v.as_str()
        .map(|s| s.trim_end_matches('/').to_owned())
        .ok_or_else(|| attribute_error(v.type_name(), "rstrip"))
}

/// Python `==` of two raw values that are strings or missing (None).
fn py_eq(a: Option<Value>, b: Option<Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => match (a.kind(), b.kind()) {
            (Kind::Null, Kind::Null) => true,
            (Kind::Str(x), Kind::Str(y)) => x == y,
            _ => false,
        },
        (Some(v), None) | (None, Some(v)) => matches!(v.kind(), Kind::Null),
    }
}

/// `retrieval.db_path` as app.py's import uses it (`Path(...)`: a string, else TypeError), then
/// `str(value or "")`.
fn db_path_str(data: &Value) -> Result<String> {
    match get(data, "retrieval.db_path").map(|v| v.kind()) {
        Some(Kind::Str(s)) => Ok(s),
        other => Err(format!(
            "TypeError: argument should be a str or an os.PathLike object, not '{}'",
            other.map_or("NoneType", |k| Value::new(k).type_name())
        )),
    }
}

fn too_large() -> String {
    "an integer beyond 64 bits cannot be sent here".into()
}

/// What Python's `int(text)` makes of a string.
enum Literal {
    Invalid,
    Value(i64),
    Negative,
    TooLarge,
}

/// Python `int(text)` for a str argument: an optional sign, decimal digits (any Unicode decimal
/// digit block, as the shared loader converts them), single underscores between digits
/// (surrounding whitespace already stripped).
fn int_literal(s: &str) -> Literal {
    let digits = s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('+'))
        .unwrap_or(s);
    if digits.is_empty() || !underscores_between_digits(digits) {
        return Literal::Invalid;
    }
    let Ok(value) = integer(s, 10) else {
        return Literal::Invalid;
    };
    match value.kind() {
        Kind::Int(n) => match n.parse::<i64>() {
            Ok(v) if v < 0 => Literal::Negative,
            Ok(v) => Literal::Value(v),
            Err(_) if n.starts_with('-') => Literal::Negative,
            Err(_) => Literal::TooLarge,
        },
        _ => Literal::Invalid,
    }
}

fn underscores_between_digits(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    chars.iter().enumerate().all(|(i, c)| {
        *c != '_'
            || (i > 0
                && i + 1 < chars.len()
                && chars.get(i - 1).is_some_and(|c| c.is_numeric())
                && chars.get(i + 1).is_some_and(|c| c.is_numeric()))
    })
}

/// Python `float(x)` (the timeout). Infinite and NaN values are returned (Python starts with
/// them); the caller refuses them where they would be sent.
fn py_float(v: &Value) -> Result<f64> {
    let invalid = |s: &str| {
        format!(
            "ValueError: could not convert string to float: {}",
            crate::value::repr(s)
        )
    };
    match v.kind() {
        Kind::Int(s) => match s.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(f),
            _ => Err("OverflowError: int too large to convert to float".into()),
        },
        Kind::Float(f) => Ok(f),
        Kind::Bool(b) => Ok(f64::from(u8::from(b))),
        Kind::Str(s) => {
            let t = strip(&s);
            let lower = t.to_ascii_lowercase();
            let body = lower
                .strip_prefix('-')
                .or_else(|| lower.strip_prefix('+'))
                .unwrap_or(&lower);
            let named = matches!(body, "inf" | "infinity" | "nan");
            let numeric = !body.is_empty()
                && body
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'_' | b'+' | b'-'))
                && underscores_between_digits(body);
            if !named && !numeric {
                return Err(invalid(&s));
            }
            t.replace('_', "").parse::<f64>().map_err(|_| invalid(&s))
        }
        _ => Err(format!(
            "TypeError: float() argument must be a string or a real number, not '{}'",
            v.type_name()
        )),
    }
}

/// Python `int(x)` (the retry count): None for a value beyond 64 bits (Python starts with it;
/// the caller refuses it where it would be sent).
fn py_int(v: &Value) -> Result<Option<i64>> {
    match v.kind() {
        Kind::Int(s) => Ok(s.parse::<i64>().ok()),
        Kind::Bool(b) => Ok(Some(i64::from(b))),
        Kind::Float(f) if f.is_nan() => {
            Err("ValueError: cannot convert float NaN to integer".into())
        }
        Kind::Float(f) if f.is_infinite() => {
            Err("OverflowError: cannot convert float infinity to integer".into())
        }
        Kind::Float(f) => {
            let t = f.trunc();
            // [-2^63, 2^63): exactly the floats that convert to an i64.
            let limit = 2f64.powi(63);
            if t >= -limit && t < limit {
                Ok(Some(t as i64))
            } else {
                Ok(None)
            }
        }
        Kind::Str(s) => match int_literal(strip(&s)) {
            Literal::Value(v) => Ok(Some(v)),
            Literal::Negative => {
                // A negative literal: its value, when it fits.
                Ok(integer(strip(&s), 10)
                    .ok()
                    .and_then(|v| match v.kind() {
                        Kind::Int(n) => n.parse::<i64>().ok(),
                        _ => None,
                    }))
            }
            Literal::TooLarge => Ok(None),
            Literal::Invalid => Err(format!(
                "ValueError: invalid literal for int() with base 10: {}",
                crate::value::repr(&s)
            )),
        },
        _ => Err(format!(
            "TypeError: int() argument must be a string, a bytes-like object or a real number, not '{}'",
            v.type_name()
        )),
    }
}

/// A raw configuration value as Python's `json.dumps` writes it (None when missing).
fn raw(v: Option<Value>) -> Result<Json> {
    v.map_or(Ok(Json::Null), |v| to_json(&v))
}

/// `json.dumps` of a loaded value.
pub fn to_json(v: &Value) -> Result<Json> {
    Ok(match v.kind() {
        Kind::Null => Json::Null,
        Kind::Bool(b) => Json::Bool(b),
        Kind::Int(s) => {
            if let Ok(n) = s.parse::<i64>() {
                json!(n)
            } else if let Ok(n) = s.parse::<u64>() {
                json!(n)
            } else {
                return Err(too_large());
            }
        }
        Kind::Float(f) => serde_json::Number::from_f64(f)
            .map(Json::Number)
            .ok_or_else(|| "a non-finite float cannot be sent as JSON".to_string())?,
        Kind::Str(s) => Json::String(s),
        Kind::Seq(items) => Json::Array(items.iter().map(to_json).collect::<Result<_>>()?),
        Kind::Map(rows) => {
            let mut map = Map::new();
            for (k, val) in rows {
                let key = match k.kind() {
                    Kind::Str(s) | Kind::Int(s) => s,
                    Kind::Bool(b) => if b { "true" } else { "false" }.into(),
                    Kind::Null => "null".into(),
                    Kind::Float(f) => python_float(f),
                    _ => return Err(format!("a {} key cannot be sent as JSON", k.type_name())),
                };
                map.insert(key, to_json(&val)?);
            }
            Json::Object(map)
        }
        Kind::Surrogate(_) | Kind::Unsupported(_) | Kind::EmptyUnsupported(_) => {
            return Err(format!("a {} value cannot be sent here", v.type_name()))
        }
    })
}
