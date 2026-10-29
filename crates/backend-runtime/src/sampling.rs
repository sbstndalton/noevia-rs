//! Original sampling-presets.cjs policy. Routing supplies its role; this policy never selects a model.
use crate::{Fault, Result};
use serde_json::{json, Map, Value};
use std::sync::OnceLock;
pub fn select(
    role: Option<&str>,
    message: &str,
    explicit: Option<&Value>,
    enabled: bool,
) -> Result<Value> {
    static CREATIVE: OnceLock<std::result::Result<regex::Regex, regex::Error>> = OnceLock::new();
    let creative = CREATIVE.get_or_init(|| {
        // ECMAScript non-Unicode /i has ASCII word boundaries; its whitespace set
        // includes BOM and excludes NEL, unlike Rust's generic Unicode whitespace.
        let s = r"[\t\n\x0b\x0c\r \u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]";
        regex::Regex::new(&format!(r"(?-u:\b)(write|compose|draft){s}+(a|an|some)?{s}*(short{s}+)?(story|poem|poetry|song|lyrics|screenplay|scene|fiction|novel|haiku|sonnet)(?-u:\b)|(?-u:\b)creative{s}+writing(?-u:\b)|(?-u:\b)brainstorm{s}+(names|titles|taglines)(?-u:\b)"))
    }).as_ref().map_err(|_| Fault::Unavailable)?;
    let preset = if role == Some("creative")
        || role != Some("code") && creative.is_match(&message.to_ascii_lowercase())
    {
        "creative"
    } else if role == Some("code") {
        "coding"
    } else if role == Some("smart") {
        "reasoning"
    } else {
        "general"
    };
    let mut params: Map<String, Value> = if enabled {
        match preset {
            "creative" => serde_json::from_value(json!({"temperature":0.9,"top_p":0.95}))?,
            "coding" => serde_json::from_value(
                json!({"temperature":0.2,"top_p":0.9,"repeat_penalty":1.05}),
            )?,
            "reasoning" => serde_json::from_value(json!({"temperature":0.5}))?,
            _ => Map::new(),
        }
    } else {
        Map::new()
    };
    for key in ["temperature", "top_p", "top_k", "repeat_penalty"] {
        if let Some(value) = explicit.and_then(|v| v.get(key)) {
            if let Some(n) = value.as_f64().filter(|v| v.is_finite()) {
                let valid = match key {
                    "temperature" => (0.0..=2.0).contains(&n),
                    "top_p" => n > 0.0 && n <= 1.0,
                    "top_k" => n >= 0.0 && n.fract() == 0.0,
                    "repeat_penalty" => (0.0..=3.0).contains(&n),
                    _ => false,
                };
                if valid {
                    params.insert(key.into(), value.clone());
                }
            }
        }
    }
    Ok(Value::Object(params))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_classification_overrides_and_disabled_setting() -> Result<()> {
        assert_eq!(
            select(
                None,
                "WRITE\u{feff}a short poem",
                Some(&json!({"temperature":0.1,"top_k":8,"model":"injected"})),
                true
            )?,
            json!({"temperature":0.1,"top_p":0.95,"top_k":8})
        );
        assert_eq!(
            select(Some("code"), "write a poem", None, true)?,
            json!({"temperature":0.2,"top_p":0.9,"repeat_penalty":1.05})
        );
        assert_eq!(select(None, "write\u{0085}a poem", None, true)?, json!({}));
        assert_eq!(
            select(
                None,
                "write a poem",
                Some(&json!({"temperature":false,"top_p":0,"top_k":2.5,"repeat_penalty":3})),
                false
            )?,
            json!({"repeat_penalty":3})
        );
        assert_eq!(select(None, "rewrite a poem", None, true)?, json!({}));
        Ok(())
    }
}
