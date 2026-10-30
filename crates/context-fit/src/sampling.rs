//! core sampling-presets.cjs `selectSamplingParams` (issue #194), checked against Node's outputs.
//! backend-runtime's unwired candidate keeps its own copy until that candidate goes. Routing supplies the role; this never selects a model. Deep
//! research sends no sampling values (core research-model.cjs), so only the chat turn will call this.
use js_json::JValue;
use std::sync::OnceLock;

const KEYS: [&str; 4] = ["temperature", "top_p", "top_k", "repeat_penalty"];

/// The preset id, core `classifyTaskForSampling`.
pub fn classify(role: Option<&str>, message: &str) -> &'static str {
    static CREATIVE: OnceLock<Option<regex::Regex>> = OnceLock::new();
    let creative = CREATIVE.get_or_init(|| {
        // ECMAScript non-Unicode /i has ASCII word boundaries; its whitespace set
        // includes BOM and excludes NEL, unlike Rust's generic Unicode whitespace.
        let s = r"[\t\n\x0b\x0c\r \u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]";
        regex::Regex::new(&format!(r"(?-u:\b)(write|compose|draft){s}+(a|an|some)?{s}*(short{s}+)?(story|poem|poetry|song|lyrics|screenplay|scene|fiction|novel|haiku|sonnet)(?-u:\b)|(?-u:\b)creative{s}+writing(?-u:\b)|(?-u:\b)brainstorm{s}+(names|titles|taglines)(?-u:\b)")).ok()
    });
    if role == Some("creative") {
        return "creative";
    }
    if role != Some("code")
        && creative
            .as_ref()
            .is_some_and(|r| r.is_match(&message.to_ascii_lowercase()))
    {
        return "creative";
    }
    match role {
        Some("code") => "coding",
        Some("smart") => "reasoning",
        _ => "general",
    }
}

fn preset(id: &str) -> Vec<(&'static str, f64)> {
    match id {
        "creative" => vec![("temperature", 0.9), ("top_p", 0.95)],
        "coding" => vec![
            ("temperature", 0.2),
            ("top_p", 0.9),
            ("repeat_penalty", 1.05),
        ],
        "reasoning" => vec![("temperature", 0.5)],
        _ => Vec::new(),
    }
}

fn valid(key: &str, v: f64) -> bool {
    v.is_finite()
        && match key {
            "temperature" => (0.0..=2.0).contains(&v),
            "top_p" => v > 0.0 && v <= 1.0,
            "top_k" => v >= 0.0 && v.fract() == 0.0,
            "repeat_penalty" => (0.0..=3.0).contains(&v),
            _ => false,
        }
}

/// core `selectSamplingParams`' `params`, in core's key order: the preset's keys (when `auto`), then the
/// explicit keys it did not have; an explicit value always wins, an invalid one is dropped.
pub fn select(role: Option<&str>, message: &str, explicit: &JValue, auto: bool) -> JValue {
    let mut params: Vec<(String, JValue)> = Vec::new();
    if auto {
        for (k, v) in preset(classify(role, message)) {
            params.push((k.to_owned(), JValue::Num(v)));
        }
    }
    for key in KEYS {
        if let Some(n) = explicit.get(key).as_num().filter(|n| valid(key, *n)) {
            js_json::wtf16::set(&mut params, key, JValue::Num(n));
        }
    }
    JValue::Obj(params)
}
