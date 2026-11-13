//! Task-aware sampling presets (issue #194): noevia-core `server/sampling-presets.cjs` in full
//! (`PRESETS`, `PRESETS_VERSION`, `SAMPLING_KEYS`, `classifyTaskForSampling`, `creativeRoute`,
//! `validSamplingValue`, `sanitizeExplicitSampling`, `selectSamplingParams`), slice P5a.
//!
//! `context-fit`'s `sampling` ports `selectSamplingParams`' `params` only; its creative-writing
//! regex and value rules are copied here (the crates do not depend on each other), and the preset
//! id, source and version are added. `CREATIVE_RE` is a non-Unicode `/i` JS regex: ASCII-only
//! case folding (`ſ`, the Kelvin sign and a dotless `ı` never fold to ASCII), ASCII word
//! boundaries, and JS `\s` (U+FEFF is space, U+0085 is not). A lone surrogate (held as a
//! private-use character in `wtf16` text) is neither a word character nor space, in both. The
//! message is lower-cased in ASCII only and matched with those classes spelt out.
//!
//! Replayed byte for byte on Node's output (`tests/fixtures/node-route.v1.json`, written by
//! `tests/record_route.cjs`).

use std::sync::OnceLock;

use js_json::JValue;
use regex::Regex;

/// `PRESETS_VERSION`.
pub const PRESETS_VERSION: u32 = 1;

/// `SAMPLING_KEYS`, in the order the explicit values are read.
pub const SAMPLING_KEYS: [&str; 4] = ["temperature", "top_p", "top_k", "repeat_penalty"];

/// A preset id of `PRESETS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    General,
    Coding,
    Creative,
    Reasoning,
}

impl Preset {
    pub fn as_str(self) -> &'static str {
        match self {
            Preset::General => "general",
            Preset::Coding => "coding",
            Preset::Creative => "creative",
            Preset::Reasoning => "reasoning",
        }
    }

    /// `PRESETS[id]`, in its key order. `general` sends nothing: the engine's defaults apply.
    pub fn params(self) -> &'static [(&'static str, f64)] {
        match self {
            Preset::General => &[],
            Preset::Coding => &[
                ("temperature", 0.2),
                ("top_p", 0.9),
                ("repeat_penalty", 1.05),
            ],
            Preset::Creative => &[("temperature", 0.9), ("top_p", 0.95)],
            Preset::Reasoning => &[("temperature", 0.5)],
        }
    }
}

fn creative_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        // JS `\s`: WhiteSpace and LineTerminator (crate::js::is_js_space).
        let s = r"[\t\n\x0b\x0c\r \u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]";
        let b = r"(?-u:\b)";
        Regex::new(&format!(
            "{b}(write|compose|draft){s}+(a|an|some)?{s}*(short{s}+)?(story|poem|poetry|song|lyrics|screenplay|scene|fiction|novel|haiku|sonnet){b}\
             |{b}creative{s}+writing{b}|{b}brainstorm{s}+(names|titles|taglines){b}"
        ))
        .ok()
    })
    .as_ref()
}

/// `CREATIVE_RE.test(text)`. (The pattern is a constant; were it ever refused, nothing would be
/// creative by text, and the recorded cases would fail.)
fn creative_text(text: &str) -> bool {
    creative_re().is_some_and(|re| re.is_match(&text.to_ascii_lowercase()))
}

/// `classifyTaskForSampling({ routedRole, message })`. `routed_role` is `None` for
/// `null`/`undefined`; `message` is the turn's (chat.cjs has checked it is a string).
pub fn classify(routed_role: Option<&str>, message: &str) -> Preset {
    match routed_role {
        Some("creative") => Preset::Creative,
        Some("code") => Preset::Coding,
        _ if creative_text(message) => Preset::Creative,
        Some("smart") => Preset::Reasoning,
        _ => Preset::General,
    }
}

/// `creativeRoute({ routedRole, message, roles })`: true (Node: `'creative'`) when the turn moves
/// to the Creative role, false (`null`) otherwise. `roles` is the Automatic roles object.
pub fn creative_route(routed_role: Option<&str>, message: &str, roles: &JValue) -> bool {
    let assigned = matches!(roles.get("creative"), JValue::Str(s) if !s.is_empty());
    assigned && classify(routed_role, message) == Preset::Creative
}

/// `validSamplingValue(key, value)`: a finite number in range for the key.
pub fn valid_value(key: &str, value: &JValue) -> bool {
    let Some(v) = value.as_num().filter(|v| v.is_finite()) else {
        return false;
    };
    match key {
        "temperature" => (0.0..=2.0).contains(&v),
        "top_p" => v > 0.0 && v <= 1.0,
        "top_k" => v.fract() == 0.0 && v >= 0.0,
        "repeat_penalty" => (0.0..=3.0).contains(&v),
        _ => false,
    }
}

/// `sanitizeExplicitSampling(raw)`: the recognized, valid keys in `SAMPLING_KEYS` order; `None`
/// (Node: `undefined`) when there is none, or `raw` is not an object.
pub fn sanitize_explicit(raw: &JValue) -> Option<Vec<(&'static str, f64)>> {
    // `!raw || typeof raw !== 'object'`: an array passes, and has none of the keys.
    let out: Vec<(&'static str, f64)> = SAMPLING_KEYS
        .iter()
        .filter_map(|&key| {
            let value = raw.get(key);
            valid_value(key, value)
                .then(|| value.as_num().map(|v| (key, v)))
                .flatten()
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

/// `selectSamplingParams(...).source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Explicit,
    Auto,
    None,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Explicit => "explicit",
            Source::Auto => "auto",
            Source::None => "none",
        }
    }
}

/// What `selectSamplingParams` returns.
#[derive(Debug, Clone, PartialEq)]
pub struct Sampling {
    /// `params`, in Node's key order: the preset's keys, then the explicit keys it lacks.
    pub params: Vec<(&'static str, f64)>,
    /// `presetId`: `None` (Node: `null`) when the presets are off.
    pub preset: Option<Preset>,
    pub source: Source,
    /// `version` ([`PRESETS_VERSION`]).
    pub version: u32,
}

impl Sampling {
    /// `params` as the JS object.
    pub fn params_value(&self) -> JValue {
        JValue::obj(self.params.iter().map(|&(k, v)| (k, JValue::Num(v))))
    }

    /// `{ params, presetId, source, version }`.
    pub fn to_value(&self) -> JValue {
        JValue::obj([
            ("params", self.params_value()),
            (
                "presetId",
                self.preset
                    .map_or(JValue::Null, |p| JValue::from(p.as_str())),
            ),
            ("source", JValue::from(self.source.as_str())),
            ("version", JValue::Num(f64::from(self.version))),
        ])
    }
}

/// `selectSamplingParams({ routedRole, message, explicit, autoEnabled })`: an explicit value (the
/// project's `sampling`) always wins, key by key; the preset fills in the rest, and only when
/// `auto_enabled`.
pub fn select(
    routed_role: Option<&str>,
    message: &str,
    explicit: &JValue,
    auto_enabled: bool,
) -> Sampling {
    let explicit = sanitize_explicit(explicit).unwrap_or_default();
    let has_explicit = !explicit.is_empty();
    if !auto_enabled {
        return Sampling {
            source: if has_explicit {
                Source::Explicit
            } else {
                Source::None
            },
            params: explicit,
            preset: None,
            version: PRESETS_VERSION,
        };
    }
    let preset = classify(routed_role, message);
    // `{ ...preset, ...explicitParams }`: an explicit key keeps the preset's place; one the preset
    // lacks goes last (none of the keys is an array index).
    let mut params = preset.params().to_vec();
    for &(key, value) in &explicit {
        match params.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => params.push((key, value)),
        }
    }
    let source = if params.is_empty() {
        Source::None
    } else if has_explicit
        && preset
            .params()
            .iter()
            .all(|(k, _)| explicit.iter().any(|(e, _)| e == k))
    {
        Source::Explicit
    } else {
        Source::Auto
    };
    Sampling {
        params,
        preset: Some(preset),
        source,
        version: PRESETS_VERSION,
    }
}
