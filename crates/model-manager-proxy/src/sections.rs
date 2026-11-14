//! The models.ini section guards' reading of a PUT body: the file-path options and their
//! llama-server spellings (`PATH_OPTION_ALIASES`, #1233, #1235, #1249) and the options a section
//! write would leave, as `presetRefusal` is asked about them (#697).

use crate::js::{is_space, prop, string_or_empty, trim};
use crate::Thrown;
use js_json::{wtf16, JValue};

/// What `PATH_OPTION_ALIASES[name]` gives: a canonical file-path option, or (a Node bug, kept) a
/// property every plain object inherits, which the refusal then names by its `String()` text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alias {
    Path(&'static str),
    Inherited(&'static str),
}

impl Alias {
    /// `${key}` in the refusal.
    pub fn text(self) -> String {
        match self {
            Alias::Path(name) => name.to_string(),
            Alias::Inherited("__proto__") => "[object Object]".to_string(),
            Alias::Inherited("constructor") => "function Object() { [native code] }".to_string(),
            Alias::Inherited(name) => format!("function {name}() {{ [native code] }}"),
        }
    }
}

/// `Object.prototype`'s own properties (what a lookup on the alias table finds when it misses).
const INHERITED: [&str; 12] = [
    "constructor",
    "__defineGetter__",
    "__defineSetter__",
    "hasOwnProperty",
    "__lookupGetter__",
    "__lookupSetter__",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toString",
    "valueOf",
    "__proto__",
    "toLocaleString",
];

/// `PATH_OPTION_ALIASES[String(rawKey).trim().replace(/^-+/, '')]`.
pub fn alias(raw_key: &str) -> Option<Alias> {
    let name = trim(raw_key).trim_start_matches('-');
    Some(match name {
        "model" | "m" | "LLAMA_ARG_MODEL" => Alias::Path("model"),
        "mmproj" | "mm" | "LLAMA_ARG_MMPROJ" => Alias::Path("mmproj"),
        "model-draft" | "md" | "LLAMA_ARG_MODEL_DRAFT" => Alias::Path("model-draft"),
        _ => Alias::Inherited(INHERITED.iter().find(|n| **n == name)?),
    })
}

/// `Object.entries(value)` of an object or array.
fn entries(value: &JValue) -> Vec<(String, JValue)> {
    match value {
        JValue::Obj(items) => items.clone(),
        JValue::Arr(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// The path-option check of a PUT `sections/<name>` body (`sent`: the body as parsed, `{}` when it
/// did not parse): the refusal's message, if any.
pub fn path_refusal(sent: &JValue) -> Option<String> {
    let values = sent.get("values");
    let vals = if sent.truthy() && values.type_of() == "object" && values.truthy() {
        entries(values)
    } else {
        Vec::new()
    };
    for (raw_key, value) in vals {
        let key = alias(&raw_key);
        if key == Some(Alias::Path("model"))
            && (matches!(value, JValue::Null)
                || value.as_str().is_some_and(|s| trim(s).is_empty()))
        {
            return Some("Not saved: model must be a text path".to_string());
        }
        if let Some(key) = key {
            if !value.is_nullish() && value.as_str().is_none() {
                return Some(format!("Not saved: {} must be a text path", key.text()));
            }
        }
    }
    None
}

/// One `extras` line through `/^\s*([^#;=\s][^=]*?)\s*=\s*(.*?)\s*$/`: the key and the value.
pub fn extras_line(line: &str) -> Option<(&str, &str)> {
    let start = line.find(|c: char| !is_space(c))?;
    let from = line.get(start..)?;
    if matches!(from.chars().next()?, '#' | ';' | '=') {
        return None;
    }
    let eq = from.find('=')?;
    let key = from.get(..eq)?.trim_end_matches(is_space);
    let value = from.get(eq + 1..)?.trim_matches(is_space);
    // `.` matches no line terminator; one left inside the value fails the match.
    if value.contains(['\r', '\n', '\u{2028}', '\u{2029}']) {
        return None;
    }
    Some((key, value))
}

/// `options[key] = value` on a plain object: `__proto__` sets an own property of that name only when
/// there is one (else it is the prototype setter, which ignores a string).
fn assign(options: &mut Vec<(String, JValue)>, key: &str, value: &str) {
    if key == "__proto__" && !options.iter().any(|(k, _)| k == key) {
        return;
    }
    wtf16::set(options, key, JValue::from(value));
}

/// The options `presetRefusal(name, options)` is asked about: the body's `values` (an object or
/// array, spread), each `extras` line's `key = value` over them, then only the non-empty strings.
/// A `null` body throws (`payload.values`), as does an `extras` with an own `toString`.
pub fn preset_options(payload: &JValue) -> Result<JValue, Thrown> {
    let values = prop(payload, "values")?;
    let mut options = if values.truthy() && values.type_of() == "object" {
        entries(values)
    } else {
        Vec::new()
    };
    let extras = string_or_empty(prop(payload, "extras")?)?;
    for line in extras.split('\n') {
        if let Some((key, value)) = extras_line(line) {
            assign(&mut options, key, value);
        }
    }
    options.retain(|(_, v)| v.as_str().is_some_and(|s| !s.is_empty()));
    Ok(JValue::Obj(options))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extras_lines() {
        assert_eq!(extras_line("  a b = c d  "), Some(("a b", "c d")));
        assert_eq!(extras_line("k ="), Some(("k", "")));
        assert_eq!(extras_line("a = b\rc"), None);
        assert_eq!(extras_line("a = b\r"), Some(("a", "b")));
        assert_eq!(extras_line("#a=1"), None);
        assert_eq!(extras_line("noequals"), None);
        assert_eq!(extras_line("a==b"), Some(("a", "=b")));
    }
}
