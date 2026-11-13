//! The KV cache policy (#1057, #1058, #1061): which cache types a tune tries, the precision-first
//! choice among those that passed, and the engine's own words for "this build has no bf16 cache".

use js_json::JValue;
use regex::Regex;
use std::sync::LazyLock;

/// The override that admits q4_0 (#190), on top of a model's own q5 opt-in.
pub const KV_FLOOR_OVERRIDE_ENV: &str = "NOEVIA_AUTOTUNE_ALLOW_BELOW_Q5_KV";

/// `allowBelowQ5Kv()` for the variable's value.
pub fn allow_below_q5(value: Option<&str>) -> bool {
    matches!(
        value.map(str::to_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// `kvCandidates({ allowQ5Kv })`: bf16 then q8_0; q5_1 and q5_0 with the model's opt-in; q4_0
/// also needs the operator's override.
pub fn candidates(allow_q5: bool, allow_below_q5: bool) -> Vec<&'static str> {
    let mut out = vec!["bf16", "q8_0"];
    if allow_q5 {
        out.extend(["q5_1", "q5_0"]);
        if allow_below_q5 {
            out.push("q4_0");
        }
    }
    out
}

/// `UNQUANTIZED.has(kv)`.
pub fn unquantized(kv: &str) -> bool {
    kv == "bf16" || kv == "f16"
}

/// `preferKv(passed, ceilings)`: the most precise passing type; a later one replaces it only when
/// its largest fitting context is at least twice the current choice's. `ceilings` maps a type to
/// its largest fitting rung (`null`: none) or is `None` when the model cannot be sized.
pub fn prefer(passed: &[String], ceilings: Option<&JValue>) -> Option<String> {
    let first = passed.first()?;
    let Some(ceilings) = ceilings else {
        return Some(first.clone());
    };
    let mut pick: Option<(&String, f64)> = None;
    for kv in passed {
        // `cap == null` skips both null and a missing key. The runner writes only numbers or
        // null here, so no other value needs JS's comparison coercion.
        let Some(cap) = ceilings.get(kv).as_num() else {
            continue;
        };
        if pick.is_none_or(|(_, best)| cap >= 2.0 * best) {
            pick = Some((kv, cap));
        }
    }
    Some(pick.map_or_else(|| first.clone(), |(kv, _)| kv.clone()))
}

pub const BF16_FALLBACK: &str = "This engine does not support a bf16 KV cache, so auto-tune uses f16 (also unquantized) in its place.";
pub const BF16_CRASH_FALLBACK: &str = "The engine failed to load a bf16 KV cache even at the smallest context without saying why, so auto-tune tries f16 (also unquantized) in its place.";

/// JS `.` without the `u` flag: one UTF-16 unit that is not a line terminator. Astral characters
/// are two units there, so [`bf16_unsupported`] widens each into two placeholders first.
const DOT: &str = r"[^\n\r\x{2028}\x{2029}]";

static BF16_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        "unsupported (kv )?cache type".to_string(),
        format!("cache type {DOT}{{0,24}}not supported"),
        format!("bf16{DOT}{{0,80}}(unsupported|not supported|missing op|no kernel)"),
        format!(
            "(unsupported|not supported|does not support|missing op|no kernel){DOT}{{0,80}}bf16"
        ),
    ]
    .iter()
    .map(|p| {
        #[allow(clippy::expect_used)] // constant patterns
        Regex::new(p).expect("bf16 pattern")
    })
    .collect()
});

/// A placeholder for one half of an astral character (a private-use code point no pattern names).
const HALF: char = '\u{e000}';

/// `bf16Unsupported(text)`: one line at a time (#1061), lower-cased, `_` and `-` as spaces, runs of
/// spaces and tabs as one space.
pub fn bf16_unsupported(text: Option<&str>) -> bool {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return false;
    };
    text.split(['\r', '\n'])
        .filter(|l| !l.is_empty())
        .any(|line| {
            let mut flat = String::with_capacity(line.len());
            let mut space = false;
            for c in line.to_lowercase().chars() {
                let c = if c == '_' || c == '-' { ' ' } else { c };
                if c == ' ' || c == '\t' {
                    if !space {
                        flat.push(' ');
                    }
                    space = true;
                    continue;
                }
                space = false;
                if c.len_utf16() == 2 {
                    flat.push(HALF);
                    flat.push(HALF);
                } else {
                    flat.push(c);
                }
            }
            BF16_PATTERNS.iter().any(|re| re.is_match(&flat))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_words() {
        assert!(bf16_unsupported(Some("error: unsupported cache type bf16")));
        assert!(bf16_unsupported(Some("Cache_Type-K bf16 not supported")));
        assert!(!bf16_unsupported(Some(
            "device supports bf16\nout of memory"
        )));
        assert!(!bf16_unsupported(None));
    }

    #[test]
    fn precision_first() {
        let passed = vec!["bf16".to_string(), "q8_0".to_string()];
        assert_eq!(prefer(&passed, None).as_deref(), Some("bf16"));
        let c = js_json::parse(r#"{"bf16":8192,"q8_0":16384}"#).unwrap_or(JValue::Null);
        assert_eq!(prefer(&passed, Some(&c)).as_deref(), Some("q8_0"));
        let c = js_json::parse(r#"{"bf16":8192,"q8_0":16383}"#).unwrap_or(JValue::Null);
        assert_eq!(prefer(&passed, Some(&c)).as_deref(), Some("bf16"));
        let c = js_json::parse(r#"{"bf16":null,"q8_0":4096}"#).unwrap_or(JValue::Null);
        assert_eq!(prefer(&passed, Some(&c)).as_deref(), Some("q8_0"));
    }
}
