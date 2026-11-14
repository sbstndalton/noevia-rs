//! `providers.cjs`: the default row's engine capabilities, a row's effective capabilities, the
//! registry's lookup and request headers; and `provider-egress.cjs` `providerProjection`, what the
//! external verdict reads of a row.
//!
//! The merged view the lookup runs over (the deployment's default row, the shared rows, then the
//! account's own, `workspace.cjs` `mergeProviders`) is the front's to build: it already reads both
//! provider files (bins/noevia-server `provider_routes.rs` `view`).

use crate::{strict_eq, Thrown};
use chat_turn::js;
use js_json::{wtf16, JValue};

/// `CAPABILITY_KEYS`.
const CAPABILITY_KEYS: [&str; 4] = [
    "reasoningEffortParam",
    "reasoningEffortModels",
    "tokenBudgetField",
    "jsonSchemaParam",
];
const MODELS: &str = "reasoningEffortModels must be a list of up to 50 model ids";

/// `engineCapabilities(kind)`: what the default row accepts by the engine kind the deployment
/// declares (`MODEL_MANAGER_KIND`, matched exactly); `None` for a kind that declares nothing.
pub fn engine_capabilities(kind: &str) -> Option<JValue> {
    (kind == "llamacpp").then(|| JValue::obj([("jsonSchemaParam", JValue::Bool(true))]))
}

/// `parseCapabilities(input)`: the clean object, or the message.
pub fn parse_capabilities(input: &JValue) -> Result<JValue, String> {
    let JValue::Obj(items) = input else {
        return Err("capabilities must be an object".into());
    };
    if let Some((key, _)) = items
        .iter()
        .find(|(k, _)| !CAPABILITY_KEYS.contains(&k.as_str()))
    {
        return Err(format!("unknown capability: {key}"));
    }
    let mut out: Vec<(String, JValue)> = Vec::new();
    match input.get("reasoningEffortParam") {
        JValue::Undefined => {}
        JValue::Bool(b) => out.push(("reasoningEffortParam".into(), JValue::Bool(*b))),
        _ => return Err("reasoningEffortParam must be true or false".into()),
    }
    match input.get("reasoningEffortModels") {
        JValue::Undefined => {}
        JValue::Arr(models) if models.len() <= 50 => {
            let mut clean = Vec::with_capacity(models.len());
            for m in models {
                match m {
                    JValue::Str(s) if !js::trim(s).is_empty() && wtf16::len(s) <= 200 => {
                        clean.push(JValue::Str(js::trim(s).to_string()));
                    }
                    _ => return Err(MODELS.into()),
                }
            }
            out.push(("reasoningEffortModels".into(), JValue::Arr(clean)));
        }
        _ => return Err(MODELS.into()),
    }
    match input.get("tokenBudgetField") {
        JValue::Undefined => {}
        JValue::Str(s) if s == "max_tokens" || s == "max_completion_tokens" => {
            out.push(("tokenBudgetField".into(), JValue::Str(s.clone())));
        }
        _ => {
            return Err("tokenBudgetField must be one of max_tokens, max_completion_tokens".into())
        }
    }
    match input.get("jsonSchemaParam") {
        JValue::Undefined => {}
        JValue::Bool(b) => out.push(("jsonSchemaParam".into(), JValue::Bool(*b))),
        _ => return Err("jsonSchemaParam must be true or false".into()),
    }
    Ok(JValue::Obj(out))
}

/// `legacyPresetCapabilities(baseUrl)`: a row that predates capabilities and points at the one
/// built-in preset's address keeps that preset's definition; anything else (an address that does
/// not parse included) has none.
pub fn legacy_preset_capabilities(base_url: &JValue) -> JValue {
    let empty = || JValue::Obj(Vec::new());
    let Ok(text) = js_json::to_js_string(base_url) else {
        return empty();
    };
    let Ok(url) = url::Url::parse(&text) else {
        return empty();
    };
    if url.origin().ascii_serialization() != "https://api.openai.com" {
        return empty();
    }
    let mut out = vec![(
        "tokenBudgetField".to_string(),
        JValue::Str("max_completion_tokens".into()),
    )];
    // `!url.search && !url.hash && !url.username && !url.password`: an empty `?` or `#` is ''.
    let plain = url.query().is_none_or(str::is_empty)
        && url.fragment().is_none_or(str::is_empty)
        && url.username().is_empty()
        && url.password().is_none_or(str::is_empty);
    if plain && matches!(url.path(), "" | "/" | "/v1" | "/v1/") {
        out.push(("reasoningEffortParam".into(), JValue::Bool(true)));
        out.push((
            "reasoningEffortModels".into(),
            JValue::Arr(vec![JValue::Str("gpt-5.4".into())]),
        ));
    }
    JValue::Obj(out)
}

/// `effectiveCapabilities(row)`: the stored capabilities re-validated (`{}` when they do not
/// validate), or the preset's for a row that has none.
pub fn effective_capabilities(row: &JValue) -> JValue {
    match row.get("capabilities") {
        JValue::Undefined => legacy_preset_capabilities(row.get("baseUrl")),
        stored => parse_capabilities(stored).unwrap_or_else(|_| JValue::Obj(Vec::new())),
    }
}

/// `{ ...value }`: an object's own properties, an array's or a string's indices; nothing for any
/// other value.
fn spread(value: &JValue) -> Vec<(String, JValue)> {
    match value {
        JValue::Obj(items) => items.clone(),
        JValue::Arr(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        JValue::Str(s) => js::to_units(s)
            .iter()
            .enumerate()
            .map(|(i, u)| (i.to_string(), JValue::Str(js::from_units(&[*u]))))
            .collect(),
        _ => Vec::new(),
    }
}

/// The registry's `getProvider(id)`: `lemonade` is the default's old name; an unknown id falls back
/// to the default row; `None` when neither is in the view. The row carries its effective
/// capabilities (`{ ...row, capabilities: effectiveCapabilities(row) }`).
pub fn get_provider(view: &[JValue], id: &JValue, default_id: &str) -> Option<JValue> {
    let default = JValue::Str(default_id.to_string());
    let normalized = if id.as_str() == Some("lemonade") {
        &default
    } else {
        id
    };
    let row = view
        .iter()
        .find(|pr| strict_eq(pr.get("id"), normalized))
        .or_else(|| view.iter().find(|pr| strict_eq(pr.get("id"), &default)))?;
    let mut items = spread(row);
    wtf16::set(&mut items, "capabilities", effective_capabilities(row));
    Some(JValue::Obj(items))
}

/// The registry's `providerHeaders(provider, extra)`: JSON, the caller's extra headers, and the
/// provider's key as a bearer token unless it is empty or `local`. The key is in the answer: the
/// caller sends it and never logs it.
pub fn headers(provider: &JValue, extra: &JValue) -> Result<JValue, Thrown> {
    let mut h = vec![(
        "Content-Type".to_string(),
        JValue::Str("application/json".into()),
    )];
    if extra.truthy() {
        for (k, v) in spread(extra) {
            wtf16::set(&mut h, &k, v);
        }
    }
    let key = provider.get("apiKey");
    if key.truthy() && key.as_str() != Some("local") {
        let text = js_json::to_js_string(key).map_err(|_| Thrown)?;
        wtf16::set(
            &mut h,
            "Authorization",
            JValue::Str(format!("Bearer {text}")),
        );
    }
    Ok(JValue::Obj(h))
}

/// `withoutUserinfo(url)`: an address that parses and names a user or a password, without them;
/// anything else as it is.
fn without_userinfo(text: String) -> String {
    match url::Url::parse(&text) {
        Ok(mut u) if !u.username().is_empty() || u.password().is_some_and(|p| !p.is_empty()) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        _ => text,
    }
}

/// `providerProjection(provider)`: what the external verdict reads of a row; `Ok(None)` for no row.
/// `Err` where Node throws (a label `String()` cannot write), which the verdict reads as external.
pub fn projection(provider: Option<&JValue>) -> Result<Option<provider_egress::Provider>, Thrown> {
    let Some(p) = provider.filter(|p| p.truthy()) else {
        return Ok(None);
    };
    // `String(provider.baseUrl)`; a throw reads as ''.
    let base = js_json::to_js_string(p.get("baseUrl"))
        .map(without_userinfo)
        .unwrap_or_default();
    let label = p.get("label");
    let label = if label.truthy() {
        js::to_units(&js_json::to_js_string(label).map_err(|_| Thrown)?)
    } else {
        Vec::new()
    };
    Ok(Some(provider_egress::Provider {
        kind: p.get("kind").as_str().map(js::to_units),
        external: p.get("external") == &JValue::Bool(true),
        base_url: js::to_units(&base),
        label,
    }))
}

/// `isExternalProvider(provider)`: the port's verdict on the projection; unknown, or a projection
/// Node could not make, counts as external.
pub fn is_external(provider: Option<&JValue>) -> bool {
    match projection(provider) {
        Ok(p) => provider_egress::external_strict(p.as_ref()),
        Err(Thrown) => true,
    }
}
