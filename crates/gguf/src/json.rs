//! A small JSON tree with Python-compatible number formatting, so the summary prints the same
//! numbers `json.dumps` would (floats keep their `.0`, integers may exceed 64 bits).

use crate::pyfmt::py_float_repr;
use crate::raw::Value;

/// A Python `int`: anything an `i128` holds, or the decimal digits of a larger one (only
/// produced by `int()` of a huge float).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PyInt {
    Small(i128),
    Big(String),
}

impl PyInt {
    pub fn to_f64(&self) -> f64 {
        match self {
            PyInt::Small(i) => *i as f64,
            PyInt::Big(s) => s.parse::<f64>().unwrap_or(f64::NAN),
        }
    }
}

impl std::fmt::Display for PyInt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PyInt::Small(i) => write!(f, "{i}"),
            PyInt::Big(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(PyInt),
    /// Non-finite floats serialise as `null` (Python would emit the non-JSON `NaN`/`Infinity`).
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn object<const N: usize>(fields: [(&str, Json); N]) -> Json {
        Json::Object(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    /// Look up a field of an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(i) => out.push_str(&i.to_string()),
            Json::Float(f) if f.is_finite() => out.push_str(&py_float_repr(*f)),
            Json::Float(_) => out.push_str("null"),
            Json::Str(s) => write_str(s, out),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(fields) => {
                out.push('{');
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = String::new();
        self.write(&mut s);
        f.write_str(&s)
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

impl From<&Value> for Json {
    fn from(v: &Value) -> Json {
        match v {
            Value::Bool(b) => Json::Bool(*b),
            Value::Int(i) => Json::Int(PyInt::Small(*i)),
            Value::Float(f) => Json::Float(*f),
            Value::Str(s) => Json::Str(s.clone()),
            Value::List(items) => Json::Array(items.iter().map(Json::from).collect()),
            Value::ArraySummary { count, sample } => Json::object([
                ("_array", Json::Bool(true)),
                ("count", Json::Int(PyInt::Small(i128::from(*count)))),
                (
                    "sample",
                    Json::Array(sample.iter().map(Json::from).collect()),
                ),
            ]),
        }
    }
}

impl From<Option<&Value>> for Json {
    fn from(v: Option<&Value>) -> Json {
        v.map_or(Json::Null, Json::from)
    }
}
