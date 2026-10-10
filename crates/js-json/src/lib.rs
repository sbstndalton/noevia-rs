//! JSON with JavaScript's semantics, for Rust ports of Node route handlers (full-Rust migration
//! M3): what `JSON.parse` gives a handler, what `JSON.stringify` sends back, and the coercions
//! handlers apply to body fields (`String(v)`, `!!v`, `typeof`).
//!
//! - [`parse`] keeps object keys in insertion order (a later duplicate replaces the value in the
//!   first one's place, as `JSON.parse` does) and every number as an `f64`. Stricter than
//!   `JSON.parse`, as refusals: strings with lone surrogate escapes, numbers outside the `f64`
//!   range (JS: `Infinity`), and nesting deeper than serde_json's 128 levels.
//! - [`stringify`] is `JSON.stringify(value)` byte for byte, `undefined` included (dropped in
//!   objects, `null` in arrays, nothing at the top).
//! - [`to_js_string`] is `String(value)`.

use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use std::fmt::{self, Write as _};

/// A JavaScript value that came out of `JSON.parse`, or one a handler builds to send.
#[derive(Debug, Clone, PartialEq)]
pub enum JValue {
    /// `undefined`: a missing property, or a field a handler leaves out.
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<JValue>),
    /// Keys in insertion order.
    Obj(Vec<(String, JValue)>),
}

static UNDEFINED: JValue = JValue::Undefined;

impl JValue {
    /// `value[key]` for a plain-data object; `undefined` for anything else or a missing key.
    /// (Arrays and strings have `length`; nothing here reads it.)
    pub fn get(&self, key: &str) -> &JValue {
        match self {
            JValue::Obj(items) => items
                .iter()
                .find(|(k, _)| k == key)
                .map_or(&UNDEFINED, |(_, v)| v),
            _ => &UNDEFINED,
        }
    }

    /// `value?.[key]`: the same as [`JValue::get`] (reading a property of `null`/`undefined`
    /// throws in JS; callers that need that check use [`JValue::is_nullish`] first).
    pub fn opt(&self, key: &str) -> &JValue {
        self.get(key)
    }

    pub fn is_nullish(&self) -> bool {
        matches!(self, JValue::Undefined | JValue::Null)
    }

    /// `typeof value === 'string'`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// `typeof value === 'number'`.
    pub fn as_num(&self) -> Option<f64> {
        match self {
            JValue::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// `typeof value === 'boolean'`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// A non-null, non-array object (http.cjs `isJsonObject`).
    pub fn is_object(&self) -> bool {
        matches!(self, JValue::Obj(_))
    }

    /// `!!value`.
    pub fn truthy(&self) -> bool {
        match self {
            JValue::Undefined | JValue::Null => false,
            JValue::Bool(b) => *b,
            JValue::Num(n) => *n != 0.0 && !n.is_nan(),
            JValue::Str(s) => !s.is_empty(),
            JValue::Arr(_) | JValue::Obj(_) => true,
        }
    }

    /// `typeof value`.
    pub fn type_of(&self) -> &'static str {
        match self {
            JValue::Undefined => "undefined",
            JValue::Null | JValue::Arr(_) | JValue::Obj(_) => "object",
            JValue::Bool(_) => "boolean",
            JValue::Num(_) => "number",
            JValue::Str(_) => "string",
        }
    }

    /// An object from `(key, value)` pairs, in order.
    pub fn obj<K: Into<String>>(pairs: impl IntoIterator<Item = (K, JValue)>) -> JValue {
        JValue::Obj(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
}

impl From<&str> for JValue {
    fn from(s: &str) -> Self {
        JValue::Str(s.to_string())
    }
}
impl From<String> for JValue {
    fn from(s: String) -> Self {
        JValue::Str(s)
    }
}
impl From<bool> for JValue {
    fn from(b: bool) -> Self {
        JValue::Bool(b)
    }
}
impl From<i64> for JValue {
    fn from(n: i64) -> Self {
        JValue::Num(n as f64)
    }
}
impl From<u32> for JValue {
    fn from(n: u32) -> Self {
        JValue::Num(f64::from(n))
    }
}
impl From<f64> for JValue {
    fn from(n: f64) -> Self {
        JValue::Num(n)
    }
}
impl<T: Into<JValue>> From<Option<T>> for JValue {
    /// `None` is `null` (what better-sqlite3 gives for SQL NULL).
    fn from(v: Option<T>) -> Self {
        v.map_or(JValue::Null, Into::into)
    }
}
impl From<Vec<JValue>> for JValue {
    fn from(v: Vec<JValue>) -> Self {
        JValue::Arr(v)
    }
}

/// Why [`parse`] refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError;

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid JSON")
    }
}

impl std::error::Error for ParseError {}

struct Seed;

impl<'de> DeserializeSeed<'de> for Seed {
    type Value = JValue;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<JValue, D::Error> {
        d.deserialize_any(Seed)
    }
}

impl<'de> Visitor<'de> for Seed {
    type Value = JValue;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON")
    }
    fn visit_unit<E>(self) -> Result<JValue, E> {
        Ok(JValue::Null)
    }
    fn visit_bool<E>(self, v: bool) -> Result<JValue, E> {
        Ok(JValue::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<JValue, E> {
        Ok(JValue::Num(v as f64))
    }
    fn visit_u64<E>(self, v: u64) -> Result<JValue, E> {
        Ok(JValue::Num(v as f64))
    }
    fn visit_f64<E>(self, v: f64) -> Result<JValue, E> {
        Ok(JValue::Num(v))
    }
    fn visit_str<E>(self, v: &str) -> Result<JValue, E> {
        Ok(JValue::Str(v.to_string()))
    }
    fn visit_string<E>(self, v: String) -> Result<JValue, E> {
        Ok(JValue::Str(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<JValue, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(Seed)? {
            out.push(v);
        }
        Ok(JValue::Arr(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<JValue, A::Error> {
        let mut out: Vec<(String, JValue)> = Vec::new();
        while let Some(k) = map.next_key::<String>()? {
            let v = map.next_value_seed(Seed)?;
            // JSON.parse: a repeated key keeps its first position and takes the last value.
            if let Some(slot) = out.iter_mut().find(|(key, _)| *key == k) {
                slot.1 = v;
            } else {
                out.push((k, v));
            }
        }
        Ok(JValue::Obj(out))
    }
}

/// `JSON.parse(text)`.
pub fn parse(text: &str) -> Result<JValue, ParseError> {
    let mut de = serde_json::Deserializer::from_str(text);
    let v = Seed.deserialize(&mut de).map_err(|_| ParseError)?;
    de.end().map_err(|_| ParseError)?;
    Ok(v)
}

/// `Number.prototype.toString()`.
pub fn number_to_string(x: f64) -> String {
    sandbox_bridge::js::number_to_string(x)
}

/// JSON.stringify's string quoting.
pub fn quote(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn emit(v: &JValue, out: &mut String) {
    match v {
        JValue::Undefined | JValue::Null => out.push_str("null"),
        JValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JValue::Num(n) if n.is_finite() => out.push_str(&number_to_string(*n)),
        JValue::Num(_) => out.push_str("null"),
        JValue::Str(s) => quote(s, out),
        JValue::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                emit(item, out);
            }
            out.push(']');
        }
        JValue::Obj(items) => {
            out.push('{');
            let mut first = true;
            for (k, item) in items {
                if matches!(item, JValue::Undefined) {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                quote(k, out);
                out.push(':');
                emit(item, out);
            }
            out.push('}');
        }
    }
}

/// `JSON.stringify(value)`; `None` for `undefined` (JS returns `undefined`).
pub fn stringify(v: &JValue) -> Option<String> {
    if matches!(v, JValue::Undefined) {
        return None;
    }
    let mut out = String::new();
    emit(v, &mut out);
    Some(out)
}

/// `JSON.stringify(value, null, 2)`, the bytes core `workspace.cjs` `atomicJson` writes; `None`
/// for `undefined`. Empty arrays and objects (also objects whose every value is `undefined`) are
/// `[]` / `{}` on one line, as in JS.
pub fn stringify_pretty(v: &JValue) -> Option<String> {
    if matches!(v, JValue::Undefined) {
        return None;
    }
    let mut out = String::new();
    emit_pretty(v, &mut out, 0);
    Some(out)
}

fn newline(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn emit_pretty(v: &JValue, out: &mut String, depth: usize) {
    match v {
        JValue::Arr(items) if !items.is_empty() => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                emit_pretty(item, out, depth + 1);
            }
            newline(out, depth);
            out.push(']');
        }
        JValue::Obj(items) if items.iter().any(|(_, x)| !matches!(x, JValue::Undefined)) => {
            out.push('{');
            let mut first = true;
            for (k, item) in items {
                if matches!(item, JValue::Undefined) {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                newline(out, depth + 1);
                quote(k, out);
                out.push_str(": ");
                emit_pretty(item, out, depth + 1);
            }
            newline(out, depth);
            out.push('}');
        }
        JValue::Obj(_) => out.push_str("{}"),
        other => emit(other, out),
    }
}

/// `String(value)` throws a TypeError for an object with an own `toString` (not callable in
/// JSON data), and V8 gives up on arrays nested thousands deep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoerceError;

/// `String(value)`.
pub fn to_js_string(v: &JValue) -> Result<String, CoerceError> {
    coerce(v, 0)
}

fn coerce(v: &JValue, depth: usize) -> Result<String, CoerceError> {
    if depth > 1000 {
        return Err(CoerceError);
    }
    Ok(match v {
        JValue::Undefined => "undefined".into(),
        JValue::Null => "null".into(),
        JValue::Bool(b) => b.to_string(),
        JValue::Num(n) => number_to_string(*n),
        JValue::Str(s) => s.clone(),
        JValue::Obj(items) => {
            if items.iter().any(|(k, _)| k == "toString" || k == "valueOf") {
                // An own non-callable toString/valueOf: ToPrimitive throws (valueOf alone
                // still falls through to toString in JS; refusing is the safe side).
                return Err(CoerceError);
            }
            "[object Object]".into()
        }
        JValue::Arr(items) => {
            let mut out = String::new();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if !item.is_nullish() {
                    out.push_str(&coerce(item, depth + 1)?);
                }
            }
            out
        }
    })
}

/// The number of UTF-16 code units in `s` (JS `s.length`).
pub fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s.slice(0, n)` by UTF-16 code units; a surrogate pair cut in half keeps the lone high
/// surrogate in JS, which this port drops instead (it cannot be held in a Rust `String`; the
/// JS value would not survive a round trip through UTF-8 storage either).
pub fn js_slice(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut units = 0;
    for ch in s.chars() {
        let w = ch.len_utf16();
        if units + w > n {
            break;
        }
        units += w;
        out.push(ch);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Node's own output (tests/fixtures/stringify-pretty.v1.json, written by node from the inputs).
    #[test]
    fn stringify_pretty_is_nodes_atomic_json() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/stringify-pretty.v1.json"))
                .unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert!(cases.len() >= 6);
        for case in cases {
            let input = case["input"].as_str().unwrap();
            let v = parse(input).unwrap();
            assert_eq!(
                stringify_pretty(&v).unwrap(),
                case["pretty"].as_str().unwrap(),
                "{input}"
            );
        }
        assert_eq!(stringify_pretty(&JValue::Undefined), None);
        let all_undefined = JValue::obj([("a", JValue::Undefined)]);
        assert_eq!(stringify_pretty(&all_undefined).unwrap(), "{}");
        let arr = JValue::Arr(vec![
            JValue::Undefined,
            JValue::obj([("a", JValue::Undefined), ("b", JValue::Num(1.0))]),
        ]);
        assert_eq!(
            stringify_pretty(&arr).unwrap(),
            "[\n  null,\n  {\n    \"b\": 1\n  }\n]"
        );
    }

    #[test]
    fn parse_keeps_js_order_and_duplicates() {
        let v = parse(r#" {"b":1,"a":[true,null,"x"],"b":2} "#).unwrap();
        assert_eq!(stringify(&v).unwrap(), r#"{"b":2,"a":[true,null,"x"]}"#);
        assert_eq!(v.get("missing"), &JValue::Undefined);
        assert!(parse("").is_err());
        assert!(parse("{} x").is_err());
        assert!(parse("\u{feff}{}").is_err());
        assert_eq!(
            parse("12345678901234567890").unwrap(),
            JValue::Num(1.2345678901234567e19)
        );
        assert!(parse(r#""\ud800""#).is_err());
    }

    #[test]
    fn stringify_like_js() {
        let v = JValue::obj([
            ("a", JValue::Undefined),
            (
                "b",
                JValue::Arr(vec![
                    JValue::Undefined,
                    JValue::Num(f64::NAN),
                    JValue::Num(-0.0),
                ]),
            ),
            ("c", JValue::from("q\"\\\u{1}\u{2028}")),
            ("d", JValue::Num(1e21)),
            ("e", JValue::Num(0.1)),
        ]);
        assert_eq!(
            stringify(&v).unwrap(),
            "{\"b\":[null,null,0],\"c\":\"q\\\"\\\\\\u0001\u{2028}\",\"d\":1e+21,\"e\":0.1}"
        );
        assert_eq!(stringify(&JValue::Undefined), None);
    }

    #[test]
    fn string_coercion_like_js() {
        let s = |t: &str| to_js_string(&parse(t).unwrap()).unwrap();
        assert_eq!(s("null"), "null");
        assert_eq!(s("[1,[2,null],\"x\"]"), "1,2,,x");
        assert_eq!(s("{}"), "[object Object]");
        assert_eq!(s("1e21"), "1e+21");
        assert_eq!(s("true"), "true");
        assert!(to_js_string(&parse(r#"{"toString":1}"#).unwrap()).is_err());
        assert_eq!(to_js_string(&JValue::Undefined).unwrap(), "undefined");
    }

    #[test]
    fn utf16_lengths() {
        assert_eq!(js_len("a😀"), 3);
        assert_eq!(js_slice("ab😀c", 3), "ab");
        assert_eq!(js_slice("ab😀c", 4), "ab😀");
    }
}
