//! Python `str(value or '').strip()` without JSON serialization substitution.
use super::{literals::NONPRINTABLE, Error, BODY_CAP, NAME_CAP};
use model_files::json::{parse_with_limits, Limits, Value};
use std::{
    collections::{HashMap, HashSet},
    fmt::Write,
};

pub const LIMITS: Limits = Limits {
    bytes: 1_048_576,
    depth: 32,
    nodes: 65_536,
    string_chars: 1_048_576,
    string_bytes: 1_048_576,
    number_bytes: 4096,
};

pub fn parse(bytes: &[u8]) -> Result<Value, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Input)?;
    let value = parse_with_limits(text, LIMITS).map_err(|_| Error::Input)?;
    Ok(value)
}
pub fn fields(value: &Value) -> Result<(String, String), Error> {
    if !matches!(value, Value::Obj(_)) {
        if contains_nonfinite(value) {
            return Err(Error::Input);
        }
        return Err(Error::Object);
    }
    let name = normalized(value.get("name"), NAME_CAP)?;
    let body = normalized(value.get("body"), BODY_CAP)?;
    if name.is_empty() || body.is_empty() {
        return Err(Error::Required);
    }
    Ok((name, body))
}
fn contains_nonfinite(value: &Value) -> bool {
    match value {
        Value::Float(n) => !n.is_finite(),
        Value::Arr(items) => items.iter().any(contains_nonfinite),
        Value::Obj(items) => items.iter().any(|(_, value)| contains_nonfinite(value)),
        _ => false,
    }
}
fn normalized(value: Option<&Value>, cap: usize) -> Result<String, Error> {
    let Some(value) = value.filter(|v| v.truthy()) else {
        return Ok(String::new());
    };
    let mut out = Text {
        text: String::new(),
        cap: 1_048_576,
    };
    if let Value::Str(s) = value {
        out.add(s)?;
    } else {
        repr(value, &mut out)?;
    }
    let stripped = model_autoconfig::pyval::py_strip(&out.text);
    if stripped.len() > cap {
        return Err(Error::Text);
    }
    let mut result = String::new();
    result
        .try_reserve_exact(stripped.len())
        .map_err(|_| Error::Limit)?;
    result.push_str(stripped);
    Ok(result)
}
struct Text {
    text: String,
    cap: usize,
}
impl Text {
    fn add(&mut self, s: &str) -> Result<(), Error> {
        if self.text.len().saturating_add(s.len()) > self.cap {
            return Err(Error::Text);
        }
        self.text.try_reserve(s.len()).map_err(|_| Error::Limit)?;
        self.text.push_str(s);
        Ok(())
    }
    fn character(&mut self, c: char) -> Result<(), Error> {
        self.add(c.encode_utf8(&mut [0; 4]))
    }
}
impl Write for Text {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.add(s).map_err(|_| std::fmt::Error)
    }
}
fn string(value: &str, out: &mut Text) -> Result<(), Error> {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    out.character(quote)?;
    for c in value.chars() {
        match c {
            '\\' => out.add("\\\\")?,
            '\t' => out.add("\\t")?,
            '\n' => out.add("\\n")?,
            '\r' => out.add("\\r")?,
            c if c == quote => {
                out.add("\\")?;
                out.character(c)?;
            }
            c => {
                let cp = u32::from(c);
                let at = NONPRINTABLE.partition_point(|(_, end)| *end < cp);
                if NONPRINTABLE
                    .get(at)
                    .is_some_and(|(start, end)| *start <= cp && cp <= *end)
                {
                    if cp <= 255 {
                        write!(out, "\\x{cp:02x}")
                    } else if cp <= 65535 {
                        write!(out, "\\u{cp:04x}")
                    } else {
                        write!(out, "\\U{cp:08x}")
                    }
                    .map_err(|_| Error::Text)?;
                } else {
                    out.character(c)?;
                }
            }
        }
    }
    out.character(quote)
}
fn repr(value: &Value, out: &mut Text) -> Result<(), Error> {
    match value {
        Value::Null => out.add("None"),
        Value::Bool(true) => out.add("True"),
        Value::Bool(false) => out.add("False"),
        Value::Int(n) => write!(out, "{n}").map_err(|_| Error::Text),
        Value::Float(n) => out.add(&gguf::py_float_repr(*n)),
        Value::Str(s) => string(s, out),
        Value::Arr(items) => {
            out.add("[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.add(", ")?;
                }
                repr(item, out)?;
            }
            out.add("]")
        }
        Value::Obj(items) => {
            let mut values = HashMap::new();
            values.try_reserve(items.len()).map_err(|_| Error::Limit)?;
            let mut seen = HashSet::new();
            seen.try_reserve(items.len()).map_err(|_| Error::Limit)?;
            for (key, value) in items {
                values.insert(key.as_str(), value);
            }
            out.add("{")?;
            let mut first = true;
            for (key, _) in items {
                if !seen.insert(key.as_str()) {
                    continue;
                }
                if !first {
                    out.add(", ")?;
                }
                first = false;
                string(key, out)?;
                out.add(": ")?;
                repr(values.get(key.as_str()).ok_or(Error::Input)?, out)?;
            }
            out.add("}")
        }
    }
}

/// `str(value or "")` (not stripped), for other native routes that read a FastAPI `dict` body
/// the same way (noevia#1318 badges). Bounded like the prompt fields.
pub fn py_str(value: Option<&Value>) -> Result<String, Error> {
    let Some(value) = value.filter(|v| v.truthy()) else {
        return Ok(String::new());
    };
    let mut out = Text {
        text: String::new(),
        cap: 1_048_576,
    };
    if let Value::Str(s) = value {
        out.add(s)?;
    } else {
        repr(value, &mut out)?;
    }
    Ok(out.text)
}

/// Python's `repr(text)` for a str, e.g. for `f"... {category!r}"` messages.
pub fn py_repr_str(text: &str) -> Result<String, Error> {
    let mut out = Text {
        text: String::new(),
        cap: 1_048_576,
    };
    string(text, &mut out)?;
    Ok(out.text)
}
