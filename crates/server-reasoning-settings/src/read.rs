//! Native `GET /api/reasoning-settings[?projectId]` (track R12): core
//! `server/routes/reasoning-settings.cjs`, the account default, the effort a project resolves to,
//! and how the provider will be asked (`off`, `real` or `hint`).
//!
//! The pure part, in Node's terms:
//! - [`Defaults`]: index.cjs's environment-derived default provider row (`DEFAULT_PROVIDER_ID`,
//!   `INFERENCE_BASE`, `engineCapabilities(MODEL_MANAGER_KIND)`) and the two raw variables
//!   reasoning-effort.cjs `localEngine` reads at call time;
//! - [`provider`]: workspace.cjs's merged provider view (the default row, then the sound
//!   shared-providers.json rows, then the account's own providers.json rows) and providers.cjs
//!   `getProvider(id)` with `effectiveCapabilities`;
//! - [`mode_for`]: reasoning-effort.cjs `modeFor`, and [`answer`] the reply.
//!
//! Difference from Node (documented, not a refusal): Node remembers, for its process's life, a
//! provider and model that refused the effort parameter during a chat turn (`rejected`) and then
//! reports `hint`; that memory is in Node's process and is not read here, so the front reports what
//! the provider row and model say (`real`). The first turn after a restart behaves the same way in
//! Node, and the chat turn itself still falls back and warns.
//!
//! Fail closed (503) where Node would read a value this port cannot hold (a number past the `f64`
//! range, a character of the lone-surrogate range, nesting past 128) in either provider file.

use js_json::{to_js_string, wtf16, JValue};

/// index.cjs's environment-derived default provider and reasoning-effort.cjs's raw reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Defaults {
    /// `process.env.DEFAULT_PROVIDER_ID || 'default'`.
    pub id: String,
    /// `INFERENCE_BASE`: `INFERENCE_BASE_URL || LEMONADE_BASE_URL || 'http://host.docker.internal:11434'`.
    pub base_url: String,
    /// `engineCapabilities(MODEL_MANAGER_KIND)`: `{ jsonSchemaParam: true }` for llama.cpp, else none.
    pub capabilities: Option<JValue>,
    /// `process.env.MODEL_MANAGER_KIND` as set (localEngine reads it raw).
    pub manager_kind_env: Option<String>,
    /// `process.env.INFERENCE_BASE_URL` as set (localEngine reads it raw).
    pub inference_base_url_env: Option<String>,
}

/// `process.env.X || fallback`: unset and empty are the same.
fn or<'a>(value: &'a Option<String>, fallback: &'a str) -> &'a str {
    value
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(fallback)
}

impl Defaults {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let lemonade = get("LEMONADE_BASE_URL");
        let inference = get("INFERENCE_BASE_URL");
        let kind_env = get("MODEL_MANAGER_KIND");
        let lemonade_set = lemonade.as_deref().is_some_and(|v| !v.is_empty());
        let kind = or(&kind_env, if lemonade_set { "lemonade" } else { "none" });
        let base_url = or(
            &inference,
            or(&lemonade, "http://host.docker.internal:11434"),
        )
        .to_string();
        Defaults {
            id: or(&get("DEFAULT_PROVIDER_ID"), "default").to_string(),
            base_url,
            capabilities: (kind == "llamacpp")
                .then(|| JValue::obj([("jsonSchemaParam", JValue::Bool(true))])),
            manager_kind_env: kind_env,
            inference_base_url_env: inference,
        }
    }

    /// `{ ...defaultProvider, shared: true }` as loadShared puts it first (the key itself is not
    /// needed here and is left out).
    fn row(&self) -> JValue {
        let mut items = vec![
            ("id".to_string(), JValue::Str(self.id.clone())),
            ("baseUrl".to_string(), JValue::Str(self.base_url.clone())),
            ("shared".to_string(), JValue::Bool(true)),
        ];
        if let Some(caps) = &self.capabilities {
            items.push(("capabilities".to_string(), caps.clone()));
        }
        JValue::Obj(items)
    }
}

/// A provider file as the reader found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum File {
    Missing,
    /// It exists but could not be read (a directory, a permission error).
    Unreadable,
    Bytes(Vec<u8>),
}

/// A provider file this port cannot hold the way Node reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unholdable;

/// `JSON.parse(fs.readFileSync(file, 'utf8'))`: `Ok(None)` where JSON.parse throws.
fn parsed(bytes: &[u8]) -> Result<Option<JValue>, Unholdable> {
    match wtf16::parse(&String::from_utf8_lossy(bytes)) {
        Ok(v) => Ok(Some(v)),
        Err(wtf16::Refusal::Invalid) => Ok(None),
        Err(wtf16::Refusal::Unsupported) => Err(Unholdable),
    }
}

/// `===` between two JSON primitives (objects and arrays are never the same object here).
fn strict_eq(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Str(x), JValue::Str(y)) => x == y,
        (JValue::Num(x), JValue::Num(y)) => x == y,
        (JValue::Bool(x), JValue::Bool(y)) => x == y,
        (JValue::Null, JValue::Null) | (JValue::Undefined, JValue::Undefined) => true,
        _ => false,
    }
}

/// workspace.cjs `sharedRows`: a damaged file (not JSON, not `{ providers: [objects] }`) is no rows.
fn shared_rows(file: &File) -> Result<Vec<JValue>, Unholdable> {
    let bytes = match file {
        File::Missing | File::Unreadable => return Ok(Vec::new()),
        File::Bytes(b) => b,
    };
    let Some(doc) = parsed(bytes)? else {
        return Ok(Vec::new());
    };
    if !matches!(doc, JValue::Obj(_)) {
        return Ok(Vec::new());
    }
    let rows = match doc.get("providers") {
        JValue::Undefined | JValue::Null => return Ok(Vec::new()),
        JValue::Arr(rows) => rows.clone(),
        _ => return Ok(Vec::new()),
    };
    if rows.iter().all(|r| matches!(r, JValue::Obj(_))) {
        Ok(rows)
    } else {
        Ok(Vec::new())
    }
}

/// workspace.cjs `privateRows` load: `readJson(file, { providers: [] }).providers || []`, each row's
/// key opened (an assignment that throws on anything but an object or array, so such a list loads
/// as no rows, the reread's fallback), the `lemonade` id read as the default's, the first row of
/// each id, without the default's id; then `mergeProviders` drops rows marked `shared`.
fn private_rows(file: &File, defaults: &Defaults) -> Result<Vec<JValue>, Unholdable> {
    let bytes = match file {
        File::Missing | File::Unreadable => return Ok(Vec::new()),
        File::Bytes(b) => b,
    };
    let stored = match parsed(bytes)? {
        None => return Ok(Vec::new()),
        Some(doc) => doc.get("providers").clone(),
    };
    let mut rows = match stored {
        v if !v.truthy() => return Ok(Vec::new()),
        JValue::Arr(rows) => rows,
        // A string iterates its characters and the key assignment throws; anything else is not
        // iterable: either way the load throws and the fallback is no rows.
        _ => return Ok(Vec::new()),
    };
    if !rows
        .iter()
        .all(|r| matches!(r, JValue::Obj(_) | JValue::Arr(_)))
    {
        return Ok(Vec::new());
    }
    let default_id = JValue::Str(defaults.id.clone());
    for row in rows.iter_mut() {
        if let JValue::Obj(items) = row {
            if items
                .iter()
                .any(|(k, v)| k == "id" && v.as_str() == Some("lemonade"))
            {
                wtf16::set(items, "id", default_id.clone());
            }
        }
    }
    let mut unique: Vec<JValue> = Vec::new();
    for row in rows {
        if !unique.iter().any(|u| strict_eq(u.get("id"), row.get("id"))) {
            unique.push(row);
        }
    }
    Ok(unique
        .into_iter()
        .filter(|p| !strict_eq(p.get("id"), &default_id) && !p.get("shared").truthy())
        .collect())
}

/// providers.cjs `getProvider(id)` over workspace.cjs's merged view, with the row's effective
/// capabilities in place of its stored ones. Never `None` in Node's merged view (the default row is
/// always first), kept as an `Option` for its `|| null`.
pub fn provider(
    defaults: &Defaults,
    shared: &File,
    private: &File,
    id: &JValue,
) -> Result<Option<JValue>, Unholdable> {
    let default_id = JValue::Str(defaults.id.clone());
    let mut view = vec![defaults.row()];
    view.extend(
        shared_rows(shared)?
            .into_iter()
            .filter(|p| !strict_eq(p.get("id"), &default_id)),
    );
    view.extend(private_rows(private, defaults)?);
    let normalized = if id.as_str() == Some("lemonade") {
        &default_id
    } else {
        id
    };
    let row = view
        .iter()
        .find(|p| strict_eq(p.get("id"), normalized))
        .or_else(|| view.iter().find(|p| strict_eq(p.get("id"), &default_id)));
    Ok(row.map(|r| {
        let mut out = r.clone();
        if let JValue::Obj(items) = &mut out {
            wtf16::set(items, "capabilities", effective_capabilities(r));
        }
        out
    }))
}

/// JS `\s`: what `String.prototype.trim` strips (as server-projects `js::is_space`).
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

const CAPABILITY_KEYS: [&str; 4] = [
    "reasoningEffortParam",
    "reasoningEffortModels",
    "tokenBudgetField",
    "jsonSchemaParam",
];

/// providers.cjs `parseCapabilities(input)`: the clean value, or `None` for its `{ error }`.
fn parse_capabilities(input: &JValue) -> Option<JValue> {
    let JValue::Obj(items) = input else {
        return None;
    };
    if items
        .iter()
        .any(|(k, _)| !CAPABILITY_KEYS.contains(&k.as_str()))
    {
        return None;
    }
    let mut out: Vec<(String, JValue)> = Vec::new();
    match input.get("reasoningEffortParam") {
        JValue::Undefined => {}
        JValue::Bool(b) => out.push(("reasoningEffortParam".into(), JValue::Bool(*b))),
        _ => return None,
    }
    match input.get("reasoningEffortModels") {
        JValue::Undefined => {}
        JValue::Arr(models) => {
            if models.len() > 50 {
                return None;
            }
            let mut clean = Vec::with_capacity(models.len());
            for m in models {
                let JValue::Str(s) = m else { return None };
                let trimmed = s.trim_matches(is_space);
                if trimmed.is_empty() || wtf16::len(s) > 200 {
                    return None;
                }
                clean.push(JValue::Str(trimmed.to_string()));
            }
            out.push(("reasoningEffortModels".into(), JValue::Arr(clean)));
        }
        _ => return None,
    }
    match input.get("tokenBudgetField") {
        JValue::Undefined => {}
        v @ JValue::Str(s) if s == "max_tokens" || s == "max_completion_tokens" => {
            out.push(("tokenBudgetField".into(), v.clone()))
        }
        _ => return None,
    }
    match input.get("jsonSchemaParam") {
        JValue::Undefined => {}
        JValue::Bool(b) => out.push(("jsonSchemaParam".into(), JValue::Bool(*b))),
        _ => return None,
    }
    Some(JValue::Obj(out))
}

/// `new URL(String(value))`, or `None` where it throws.
fn url_of(value: &JValue) -> Option<url::Url> {
    url::Url::parse(&to_js_string(value).ok()?).ok()
}

/// providers.cjs `legacyPresetCapabilities(baseUrl)`.
fn legacy_preset_capabilities(base_url: &JValue) -> JValue {
    let Some(url) = url_of(base_url) else {
        return JValue::obj(Vec::<(String, JValue)>::new());
    };
    if url.origin().ascii_serialization() != "https://api.openai.com" {
        return JValue::obj(Vec::<(String, JValue)>::new());
    }
    let mut out = vec![(
        "tokenBudgetField".to_string(),
        JValue::Str("max_completion_tokens".into()),
    )];
    // `url.search` and `url.hash` are '' for an empty query or fragment too.
    let plain = url.query().is_none_or(str::is_empty)
        && url.fragment().is_none_or(str::is_empty)
        && url.username().is_empty()
        && url.password().is_none_or(str::is_empty);
    if plain && ["", "/", "/v1", "/v1/"].contains(&url.path()) {
        out.push(("reasoningEffortParam".into(), JValue::Bool(true)));
        out.push((
            "reasoningEffortModels".into(),
            JValue::Arr(vec![JValue::Str("gpt-5.4".into())]),
        ));
    }
    JValue::Obj(out)
}

/// providers.cjs `effectiveCapabilities(row)`.
pub fn effective_capabilities(row: &JValue) -> JValue {
    match row.get("capabilities") {
        JValue::Undefined => legacy_preset_capabilities(row.get("baseUrl")),
        caps => {
            parse_capabilities(caps).unwrap_or_else(|| JValue::obj(Vec::<(String, JValue)>::new()))
        }
    }
}

/// reasoning-effort.cjs `documented(provider, model)`.
fn documented(provider: Option<&JValue>, model: &JValue) -> bool {
    let Some(caps) = provider.map(|p| p.get("capabilities")) else {
        return false;
    };
    if !caps.truthy() || caps.get("reasoningEffortParam") != &JValue::Bool(true) {
        return false;
    }
    match caps.get("reasoningEffortModels") {
        JValue::Arr(models) => models.iter().any(|m| strict_eq(m, model)),
        _ => true,
    }
}

/// `new URL(value).href.replace(/\/+$/, '')`.
fn normalize(value: &JValue) -> Option<String> {
    let href = url_of(value)?.to_string();
    Some(href.trim_end_matches('/').to_string())
}

/// reasoning-effort.cjs `localEngine(provider)`.
fn local_engine(provider: &JValue, defaults: &Defaults) -> bool {
    let kind = defaults.manager_kind_env.as_deref();
    if !matches!(kind, Some("lemonade" | "llamacpp")) {
        return false;
    }
    let Some(env) = &defaults.inference_base_url_env else {
        return false;
    };
    match (
        normalize(provider.get("baseUrl")),
        normalize(&JValue::Str(env.clone())),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// `/(?:^|[/_])qwen3(?:\.5)?[-_]/i.test(model)`.
fn qwen3(model: &str) -> bool {
    let chars: Vec<char> = model.chars().collect();
    let at = |i: usize, lower: char| {
        chars
            .get(i)
            .is_some_and(|c| c.to_ascii_lowercase() == lower)
    };
    (0..chars.len()).any(|start| {
        let boundary = start == 0 || matches!(chars.get(start - 1), Some('/' | '_'));
        if !boundary || !"qwen3".chars().enumerate().all(|(k, c)| at(start + k, c)) {
            return false;
        }
        let after = start + 5;
        let sep = |i: usize| matches!(chars.get(i), Some('-' | '_'));
        sep(after)
            || (chars.get(after) == Some(&'.')
                && chars.get(after + 1) == Some(&'5')
                && sep(after + 2))
    })
}

/// reasoning-effort.cjs `nativeThinking(provider, model)`.
fn native_thinking(provider: Option<&JValue>, model: &JValue, defaults: &Defaults) -> bool {
    let Some(p) = provider else { return false };
    let local = local_engine(p, defaults) && to_js_string(model).is_ok_and(|m| qwen3(&m));
    local || p.get("machine").get("thinkingSwitch") == &JValue::Bool(true)
}

/// reasoning-effort.cjs `modeFor(provider, model, effort)` without the process's `rejected` memory.
pub fn mode_for(
    provider: Option<&JValue>,
    model: &JValue,
    effort: &str,
    defaults: &Defaults,
) -> &'static str {
    if effort == "default" {
        "off"
    } else if documented(provider, model) || native_thinking(provider, model, defaults) {
        "real"
    } else {
        "hint"
    }
}

/// reasoning-effort.cjs `resolveEffort(project, globalDefault)`.
pub fn resolve_effort(project: Option<&JValue>, global_default: &str) -> String {
    let valid = |v: &str| matches!(v, "default" | "low" | "high");
    match project.map(|p| p.get("reasoningEffort")) {
        Some(JValue::Str(e)) if valid(e) => e.clone(),
        _ if valid(global_default) => global_default.to_string(),
        _ => "default".to_string(),
    }
}

/// `url.searchParams.has('projectId') ? url.searchParams.get('projectId') : absent` for the request
/// target's query (WHATWG `application/x-www-form-urlencoded`: `+` is a space, bad escapes stay as
/// written, bytes decode as UTF-8 with U+FFFD).
pub fn project_param(query: Option<&str>) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes())
        .find(|(k, _)| k == "projectId")
        .map(|(_, v)| v.into_owned())
}

/// `{ default, effort, mode, admin }` as `JSON.stringify` writes it.
pub fn answer(default: &str, effort: &str, mode: &str, admin: bool) -> String {
    let text = |s: &str| wtf16::stringify(&JValue::Str(s.to_string())).unwrap_or_default();
    format!(
        "{{\"default\":{},\"effort\":{},\"mode\":{},\"admin\":{admin}}}",
        text(default),
        text(effort),
        text(mode)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn qwen3_pattern() {
        for yes in ["qwen3-8b", "org/Qwen3_x", "a_QWEN3.5-b", "qwen3.5_"] {
            assert!(qwen3(yes), "{yes}");
        }
        for no in [
            "qwen3",
            "xqwen3-8b",
            "qwen3.6-x",
            "qwen30-x",
            "qwen3.5",
            "a.qwen3-x",
        ] {
            assert!(!qwen3(no), "{no}");
        }
    }

    #[test]
    fn defaults_follow_index_cjs() {
        let d = Defaults::from_lookup(|k| match k {
            "LEMONADE_BASE_URL" => Some("http://qa-lemonade:1".into()),
            "DEFAULT_PROVIDER_ID" => Some(String::new()),
            _ => None,
        });
        assert_eq!(d.id, "default");
        assert_eq!(d.base_url, "http://qa-lemonade:1");
        assert_eq!(d.capabilities, None);
        let d = Defaults::from_lookup(|k| (k == "MODEL_MANAGER_KIND").then(|| "llamacpp".into()));
        assert_eq!(d.base_url, "http://host.docker.internal:11434");
        assert!(d.capabilities.is_some());
    }
}
