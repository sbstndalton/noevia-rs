//! `server/account-instructions.cjs`: Settings, Assistant, Custom instructions and response style.
//! [`read`] is `read(dir)` given the bytes of `account-instructions.json` (or `None` when the file
//! is missing or unreadable); [`part`] is `systemPart(settings)`, the account part of the system
//! message.

use crate::{js, Thrown};
use js_json::JValue;
use regex::Regex;
use std::sync::OnceLock;

pub const MAX_CHARS: usize = 4000;
pub const MAX_LANGUAGE_CHARS: usize = 40;

/// STYLES: the preset and its line (`default` adds none).
const STYLES: [(&str, Option<&str>); 3] = [
    ("default", None),
    (
        "concise",
        Some("Keep replies short and direct: lead with the answer, skip preamble, and expand only when asked."),
    ),
    (
        "detailed",
        Some("Be thorough: explain reasoning, cover edge cases and give examples where they help."),
    ),
];

/// ADVANCED: each control's two opinions and their lines ('auto' adds nothing).
const ADVANCED: [(&str, [(&str, &str); 2]); 4] = [
    (
        "length",
        [
            (
                "short",
                "Prefer brief answers, a few sentences where possible.",
            ),
            ("long", "Prefer complete, longer answers."),
        ],
    ),
    (
        "tone",
        [
            ("casual", "Use a relaxed, conversational tone."),
            ("formal", "Use a formal, professional tone."),
        ],
    ),
    (
        "formatting",
        [
            (
                "minimal",
                "Write in plain paragraphs; avoid headings and bullet lists unless asked.",
            ),
            (
                "structured",
                "Organise longer answers with headings and bullet lists.",
            ),
        ],
    ),
    (
        "emoji",
        [
            ("none", "Do not use emoji."),
            ("some", "An occasional emoji is fine where it fits."),
        ],
    ),
];

const LEAD: &str = "The user's custom instructions for all chats (project instructions, and any format or language the user asks for in a message, take precedence where they conflict):";

/// `Object.hasOwn(table, key)` where the table's own keys are `names`: the key is `String(key)`
/// (ToPropertyKey), which throws for an object whose own `toString` is not callable.
fn has_own<'a>(mut names: impl Iterator<Item = &'a str>, key: &JValue) -> Result<bool, Thrown> {
    let key = js_json::to_js_string(key).map_err(|_| Thrown)?;
    Ok(names.any(|n| n == key))
}

fn advanced_default() -> JValue {
    JValue::obj(ADVANCED.iter().map(|(k, _)| (*k, JValue::from("auto"))))
}

/// `cleanAdvanced(value)`: each control keeps a value naming one of its opinions (as given, so
/// `["short"]` stays an array), else 'auto'.
fn clean_advanced(value: &JValue) -> Result<JValue, Thrown> {
    if !value.truthy() || !matches!(value, JValue::Obj(_) | JValue::Arr(_)) {
        return Ok(advanced_default());
    }
    let mut out = Vec::with_capacity(ADVANCED.len());
    for (key, opinions) in &ADVANCED {
        let v = value.get(key);
        let kept = if has_own(opinions.iter().map(|(o, _)| *o), v)? {
            v.clone()
        } else {
            JValue::from("auto")
        };
        out.push((*key, kept));
    }
    Ok(JValue::obj(out))
}

fn language_pattern() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\p{L}[\p{L}\p{M} ()'\-]*$").ok())
        .as_ref()
}

/// `cleanLanguage(value)`: a language name of at most 40 units and four words, or ''.
pub fn clean_language(value: &JValue) -> String {
    let Some(s) = value.as_str() else {
        return String::new();
    };
    // value.replace(/\s+/g, ' ').trim()
    let mut collapsed = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if js::is_js_space(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    let trimmed = js::trim(&collapsed);
    let ok = js::len(trimmed) <= MAX_LANGUAGE_CHARS
        && trimmed.split(' ').count() <= 4
        && language_pattern().is_some_and(|re| re.is_match(trimmed));
    if ok {
        trimmed.to_string()
    } else {
        String::new()
    }
}

/// `empty()`: the record of a missing or unreadable file.
pub fn empty() -> JValue {
    JValue::obj([
        ("text", JValue::from("")),
        ("style", JValue::from("default")),
        ("advanced", advanced_default()),
        ("language", JValue::from("")),
        ("updatedAt", JValue::Null),
    ])
}

/// `read(dir)` given the file's bytes (`None`: no file). Anything JSON.parse or the record's
/// reads throw on gives [`empty`], as Node's `catch` does.
pub fn read(file: Option<&[u8]>) -> JValue {
    file.and_then(js::parse_file)
        .and_then(|data| record(&data).ok())
        .unwrap_or_else(empty)
}

fn record(data: &JValue) -> Result<JValue, Thrown> {
    if data.is_nullish() {
        return Err(Thrown); // data.text of null
    }
    let text = data
        .get("text")
        .as_str()
        .map(|t| js::slice(t, MAX_CHARS))
        .unwrap_or_default();
    let style = if has_own(STYLES.iter().map(|(s, _)| *s), data.get("style"))? {
        data.get("style").clone()
    } else {
        JValue::from("default")
    };
    let advanced = clean_advanced(data.get("advanced"))?;
    let updated = match data.get("updatedAt") {
        JValue::Num(n) if n.is_finite() => JValue::Num(*n),
        _ => JValue::Null,
    };
    Ok(JValue::obj([
        ("text", JValue::from(text)),
        ("style", style),
        ("advanced", advanced),
        (
            "language",
            JValue::from(clean_language(data.get("language"))),
        ),
        ("updatedAt", updated),
    ]))
}

/// `styleLines(settings)`: the preset, the advanced controls, then the language line.
pub fn style_lines(settings: &JValue) -> Result<Vec<String>, Thrown> {
    let advanced = clean_advanced(settings.get("advanced"))?;
    let language = clean_language(settings.get("language"));
    let style_key = js_json::to_js_string(settings.get("style")).map_err(|_| Thrown)?;
    let mut lines = Vec::new();
    if let Some((_, Some(line))) = STYLES.iter().find(|(s, _)| *s == style_key) {
        lines.push((*line).to_string());
    }
    for (key, opinions) in &ADVANCED {
        let chosen = js_json::to_js_string(advanced.get(key)).map_err(|_| Thrown)?;
        if let Some((_, line)) = opinions.iter().find(|(o, _)| *o == chosen) {
            lines.push((*line).to_string());
        }
    }
    if !language.is_empty() {
        lines.push(format!(
            "Reply in {language} unless the user writes in or asks for another language."
        ));
    }
    Ok(lines)
}

/// `systemPart(textOrSettings)`: the account part of the system message, `None` when it says
/// nothing. An object (or array) is the settings; anything else is the old `(text)` form.
/// [`Thrown`] where Node throws a TypeError (a settings value whose `String()` throws; never for
/// a [`read`] record).
pub fn part(settings: &JValue) -> Result<Option<String>, Thrown> {
    let wrapped;
    let settings = if settings.truthy() && matches!(settings, JValue::Obj(_) | JValue::Arr(_)) {
        settings
    } else {
        wrapped = JValue::obj([
            ("text", settings.clone()),
            ("style", JValue::from("default")),
        ]);
        &wrapped
    };
    let mut lines = style_lines(settings)?;
    if let Some(text) = settings.get("text").as_str().filter(|t| !t.is_empty()) {
        lines.push(text.to_string());
    }
    if lines.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!("{LEAD}\n{}", lines.join("\n"))))
}
