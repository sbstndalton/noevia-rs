//! The few JavaScript operations the Node runner applies to its job record and to engine text,
//! with the same results: object mutation in JS property order (so the state file's bytes match
//! `JSON.stringify`), `String.prototype.trim`/`\s`, and `toLocaleString('en-US')` for whole numbers.

use js_json::{wtf16, JValue};

/// ECMAScript WhiteSpace and LineTerminator: what `trim()` removes and what `\s` matches (without
/// the `u` flag the set is the same). Not U+0085 (NEL) or U+180E, which Rust's `char::is_whitespace`
/// treats differently.
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `s.trim()`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// `n.toLocaleString('en-US')` for a safe integer: digits grouped by commas.
pub fn locale_int(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `String(n)` for a JS number.
pub fn num(n: f64) -> String {
    js_json::number_to_string(n)
}

/// `Math.round(x)`: halves round towards +Infinity (and 0.49999999999999994 stays 0, which
/// `(x + 0.5).floor()` gets wrong).
pub fn round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// An object's entries, or `None` for anything else.
pub fn entries(v: &JValue) -> Option<&Vec<(String, JValue)>> {
    match v {
        JValue::Obj(items) => Some(items),
        _ => None,
    }
}

/// An object's entries for mutation, or `None` for anything else.
pub fn entries_mut(v: &mut JValue) -> Option<&mut Vec<(String, JValue)>> {
    match v {
        JValue::Obj(items) => Some(items),
        _ => None,
    }
}

/// `obj[key] = value` (an existing key keeps its place; `undefined` stays a key, as in JS, and is
/// dropped only when written). A no-op on a non-object.
pub fn set(obj: &mut JValue, key: &str, value: impl Into<JValue>) {
    if let Some(items) = entries_mut(obj) {
        wtf16::set(items, key, value.into());
    }
}

/// `delete obj[key]`.
pub fn del(obj: &mut JValue, key: &str) {
    if let Some(items) = entries_mut(obj) {
        items.retain(|(k, _)| k != key);
    }
}

/// `Object.hasOwn(obj, key)`.
pub fn has(obj: &JValue, key: &str) -> bool {
    entries(obj).is_some_and(|items| items.iter().any(|(k, _)| k == key))
}

/// `obj[key]` for mutation (an own key only).
pub fn get_mut<'a>(obj: &'a mut JValue, key: &str) -> Option<&'a mut JValue> {
    entries_mut(obj)?
        .iter_mut()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// `obj[key] ||= value`: sets it when the current value is falsy; returns the slot.
pub fn or_insert<'a>(obj: &'a mut JValue, key: &str, value: JValue) -> Option<&'a mut JValue> {
    if !obj.get(key).truthy() {
        set(obj, key, value);
    }
    get_mut(obj, key)
}

/// `{ ...a, ...b }` for two plain objects (non-objects spread nothing).
pub fn spread(parts: &[&JValue]) -> JValue {
    let mut out = JValue::Obj(Vec::new());
    for part in parts {
        if let Some(items) = entries(part) {
            for (k, v) in items {
                set(&mut out, k, v.clone());
            }
        }
    }
    out
}

/// An array's items, or an empty slice for anything else.
pub fn items(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        _ => &[],
    }
}

/// An array's items for mutation (`None` for anything else).
pub fn items_mut(v: &mut JValue) -> Option<&mut Vec<JValue>> {
    match v {
        JValue::Arr(items) => Some(items),
        _ => None,
    }
}

/// `typeof v === 'string' ? v : fallback`.
pub fn str_or<'a>(v: &'a JValue, fallback: &'a str) -> &'a str {
    v.as_str().unwrap_or(fallback)
}

/// `JSON.parse(JSON.stringify(v))`: drops `undefined` members (and turns them into `null` in
/// arrays), as the Node runner's copies do.
pub fn json_copy(v: &JValue) -> JValue {
    match v {
        JValue::Obj(items) => JValue::Obj(
            items
                .iter()
                .filter(|(_, x)| !matches!(x, JValue::Undefined))
                .map(|(k, x)| (k.clone(), json_copy(x)))
                .collect(),
        ),
        JValue::Arr(items) => JValue::Arr(
            items
                .iter()
                .map(|x| match x {
                    JValue::Undefined => JValue::Null,
                    other => json_copy(other),
                })
                .collect(),
        ),
        JValue::Num(n) if !n.is_finite() => JValue::Null,
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_groups_thousands() {
        assert_eq!(locale_int(0), "0");
        assert_eq!(locale_int(999), "999");
        assert_eq!(locale_int(8192), "8,192");
        assert_eq!(locale_int(1_048_576), "1,048,576");
    }

    #[test]
    fn js_trim_set() {
        assert_eq!(trim("\u{feff} a \u{3000}"), "a");
        assert_eq!(trim("\u{85}a"), "\u{85}a");
    }

    #[test]
    fn round_half_up() {
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -2.0);
        assert_eq!(round(64.99), 65.0);
        assert_eq!(round(0.499_999_999_999_999_94), 0.0);
    }
}
