//! Bounded JSON values for replies. Integer text is never routed through f64.
//! Kept local to avoid changing serde_json features for other workspace crates.
use super::{Fault, MAX_RESPONSE};
use diary_backup_format::{Limits, PythonJson};
use serde_json::Value;
use std::collections::BTreeMap;
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}
#[derive(Debug, Clone, PartialEq)]
pub enum Number {
    Integer(String),
    Float(f64),
}
impl Number {
    pub fn as_integer(&self) -> Option<&str> {
        if let Self::Integer(v) = self {
            Some(v)
        } else {
            None
        }
    }

    pub fn is_f64(&self) -> bool {
        matches!(self, Self::Float(_))
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float(v) => Some(*v),
            Self::Integer(_) => None,
        }
    }
}
fn limits() -> Limits {
    Limits {
        manifest_bytes: MAX_RESPONSE,
        files: 0,
        directories: 0,
        json_depth: 128,
        json_nodes: 100000,
        string_bytes: MAX_RESPONSE,
        payload_bytes: MAX_RESPONSE,
    }
}
impl Json {
    pub fn get(&self, key: &str) -> Option<&Self> {
        self.as_object()?.get(key)
    }
    pub fn as_object(&self) -> Option<&BTreeMap<String, Self>> {
        if let Self::Object(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_array(&self) -> Option<&Vec<Self>> {
        if let Self::Array(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        if let Self::String(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(v) = self {
            Some(*v)
        } else {
            None
        }
    }
    pub fn as_number(&self) -> Option<&Number> {
        if let Self::Number(v) = self {
            Some(v)
        } else {
            None
        }
    }
    /// A checked convenience conversion for consumers requiring serde Value.
    /// Large integers refuse instead of returning a rounded approximation.
    pub fn to_value(&self) -> Result<Value, Fault> {
        Ok(match self {
            Self::Null => Value::Null,
            Self::Bool(v) => Value::Bool(*v),
            Self::String(v) => Value::String(v.clone()),
            Self::Number(Number::Integer(v)) => {
                if let Ok(n) = v.parse::<i64>() {
                    Value::from(n)
                } else if let Ok(n) = v.parse::<u64>() {
                    Value::from(n)
                } else {
                    return Err(Fault::Capacity);
                }
            }
            Self::Number(Number::Float(v)) => {
                Value::Number(serde_json::Number::from_f64(*v).ok_or(Fault::Internal)?)
            }
            Self::Array(v) => Value::Array(v.iter().map(Self::to_value).collect::<Result<_, _>>()?),
            Self::Object(v) => Value::Object(
                v.iter()
                    .map(|(k, v)| Ok((k.clone(), v.to_value()?)))
                    .collect::<Result<_, Fault>>()?,
            ),
        })
    }
    // Check numeric tokens before object construction can discard duplicates.
    // This is not a second JSON parser: syntax/UTF-8/depth remain the parser's
    // responsibility. Quoted digits and exponent/fraction tokens are not ints.
    pub(super) fn integer_tokens(bytes: &[u8]) -> Result<(), Fault> {
        let mut cursor = 0;
        let mut quoted = false;
        let mut escaped = false;
        while let Some(&byte) = bytes.get(cursor) {
            if quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = false;
                }
                cursor += 1;
                continue;
            }
            if byte == b'"' {
                quoted = true;
                cursor += 1;
                continue;
            }
            if byte == b'-' || byte.is_ascii_digit() {
                let start = cursor;
                cursor += 1;
                while bytes.get(cursor).is_some_and(|b| {
                    b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E')
                }) {
                    cursor += 1;
                }
                let token = bytes.get(start..cursor).ok_or(Fault::Schema)?;
                if !token.iter().any(|b| matches!(b, b'.' | b'e' | b'E')) {
                    let digits = token.strip_prefix(b"-").unwrap_or(token);
                    if digits.len() > 4300 {
                        return Err(Fault::Schema);
                    }
                }
            } else {
                cursor += 1;
            }
        }
        Ok(())
    }
    pub(super) fn parse(bytes: &[u8]) -> Result<Self, Fault> {
        Self::integer_tokens(bytes)?;
        let parsed = PythonJson::parse(bytes, limits()).map_err(|_| Fault::Schema)?;
        Self::from_python(parsed)
    }
    pub(super) fn from_python(value: PythonJson) -> Result<Self, Fault> {
        Ok(match value {
            PythonJson::Null => Self::Null,
            PythonJson::Bool(v) => Self::Bool(v),
            PythonJson::String(v) => Self::String(v),
            PythonJson::Integer(v) => {
                if v.trim_start_matches('-').len() > 4300 {
                    return Err(Fault::Schema);
                }
                Self::Number(Number::Integer(v))
            }
            PythonJson::Float(v) => {
                if !v.is_finite() {
                    return Err(Fault::Schema);
                }
                Self::Number(Number::Float(v))
            }
            PythonJson::Array(v) => Self::Array(
                v.into_iter()
                    .map(Self::from_python)
                    .collect::<Result<_, _>>()?,
            ),
            PythonJson::Object(v) => Self::Object(
                v.into_iter()
                    .map(|(k, v)| Ok((k, Self::from_python(v)?)))
                    .collect::<Result<_, Fault>>()?,
            ),
        })
    }
    pub(super) fn encode(&self) -> Result<Vec<u8>, Fault> {
        use std::io::Write;
        fn bytes(out: &mut super::BoundedOutput, raw: &[u8]) -> Result<(), Fault> {
            out.write_all(raw).map_err(|_| Fault::Capacity)
        }
        fn string(out: &mut super::BoundedOutput, text: &str) -> Result<(), Fault> {
            if text.len() > MAX_RESPONSE {
                return Err(Fault::Capacity);
            }
            serde_json::to_writer(&mut *out, text).map_err(|_| {
                if out.exceeded {
                    Fault::Capacity
                } else {
                    Fault::Internal
                }
            })
        }
        fn write(
            value: &Json,
            out: &mut super::BoundedOutput,
            depth: usize,
            nodes: &mut usize,
        ) -> Result<(), Fault> {
            *nodes = nodes.checked_add(1).ok_or(Fault::Capacity)?;
            if depth > 128 || *nodes > 100000 {
                return Err(Fault::Capacity);
            }
            match value {
                Json::Null => bytes(out, b"null"),
                Json::Bool(v) => bytes(out, if *v { b"true" } else { b"false" }),
                Json::String(v) => string(out, v),
                Json::Number(Number::Integer(v)) => {
                    let digits = v.strip_prefix('-').unwrap_or(v);
                    if digits.is_empty()
                        || digits.len() > 4300
                        || !digits.bytes().all(|b| b.is_ascii_digit())
                        || (digits.len() > 1 && digits.starts_with('0'))
                    {
                        return Err(Fault::Internal);
                    }
                    bytes(out, if v == "-0" { b"0" } else { v.as_bytes() })
                }
                Json::Number(Number::Float(v)) => {
                    if !v.is_finite() {
                        return Err(Fault::Internal);
                    }
                    bytes(out, diary_backup_format::python_float(*v).as_bytes())
                }
                Json::Array(v) => {
                    bytes(out, b"[")?;
                    for (i, item) in v.iter().enumerate() {
                        if i > 0 {
                            bytes(out, b",")?
                        }
                        write(item, out, depth + 1, nodes)?;
                    }
                    bytes(out, b"]")
                }
                Json::Object(v) => {
                    bytes(out, b"{")?;
                    for (i, (key, item)) in v.iter().enumerate() {
                        if i > 0 {
                            bytes(out, b",")?
                        }
                        string(out, key)?;
                        bytes(out, b":")?;
                        write(item, out, depth + 1, nodes)?;
                    }
                    bytes(out, b"}")
                }
            }
        }
        let mut out = super::BoundedOutput {
            bytes: Vec::new(),
            exceeded: false,
        };
        write(self, &mut out, 0, &mut 0)?;
        Ok(out.bytes)
    }
}
impl From<Value> for Json {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(v) => Self::Bool(v),
            Value::String(v) => Self::String(v),
            Value::Number(v) => {
                let raw = v.to_string();
                if raw.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) {
                    // An unsupported/nonfinite source number fails encoding;
                    // integer text, including future arbitrary-precision Value
                    // profiles, never takes this conversion.
                    Self::Number(Number::Float(v.as_f64().unwrap_or(f64::NAN)))
                } else {
                    Self::Number(Number::Integer(raw))
                }
            }
            Value::Array(v) => Self::Array(v.into_iter().map(Self::from).collect()),
            Value::Object(v) => {
                Self::Object(v.into_iter().map(|(k, v)| (k, Self::from(v))).collect())
            }
        }
    }
}
impl PartialEq<Value> for Json {
    fn eq(&self, other: &Value) -> bool {
        self == &Self::from(other.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_finite_frozen_python_floats_roundtrip_through_reply_json(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../diary-backup-format/tests/fixtures/python.json"
        ))?;
        for row in fixture
            .get("floats")
            .and_then(Value::as_array)
            .ok_or("floats")?
        {
            let bits =
                u64::from_str_radix(row.get("bits").and_then(Value::as_str).ok_or("bits")?, 16)?;
            let value = f64::from_bits(bits);
            if value.is_finite() {
                let encoded = Json::Number(Number::Float(value)).encode()?;
                assert_eq!(
                    std::str::from_utf8(&encoded)?,
                    row.get("python").and_then(Value::as_str).ok_or("python")?
                );
                let Json::Number(Number::Float(parsed)) = Json::parse(&encoded)? else {
                    return Err("float type changed".into());
                };
                assert_eq!(parsed.to_bits(), bits);
            } else {
                assert_eq!(
                    Json::Number(Number::Float(value)).encode(),
                    Err(Fault::Internal)
                );
            }
        }
        Ok(())
    }
    #[test]
    fn reply_json_bounds_and_numeric_types_do_not_round_or_emit_invalid_json(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let text = "9".repeat(4300);
        let large = Json::parse(text.as_bytes())?;
        assert_eq!(large.encode()?, text.as_bytes());
        assert_eq!(large.to_value(), Err(Fault::Capacity));
        assert_eq!(Json::parse("9".repeat(4301).as_bytes()), Err(Fault::Schema));
        for bad in ["NaN", "Infinity", "-Infinity", "01"] {
            assert_eq!(Json::parse(bad.as_bytes()), Err(Fault::Schema));
        }
        for bad in ["+1", "01", "--1", ""] {
            assert_eq!(
                Json::Number(Number::Integer(bad.into())).encode(),
                Err(Fault::Internal)
            );
        }
        assert_eq!(Json::Number(Number::Integer("-0".into())).encode()?, b"0");
        assert_eq!(
            Json::String("\0".repeat(MAX_RESPONSE / 6 + 1)).encode(),
            Err(Fault::Capacity)
        );
        let mut deep = Json::Null;
        for _ in 0..130 {
            deep = Json::Array(vec![deep]);
        }
        assert_eq!(deep.encode(), Err(Fault::Capacity));
        assert_eq!(
            Json::Array(vec![Json::Null; 100001]).encode(),
            Err(Fault::Capacity)
        );
        Ok(())
    }
}

#[cfg(test)]
mod duplicate_integer_tokens {
    use super::*;
    #[test]
    fn overwritten_integer_tokens_are_checked_but_quoted_digits_are_not(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let over = "9".repeat(4301);
        for raw in [
            format!("{{\"x\":{over},\"x\":0}}"),
            format!("{{\"x\":{{\"v\":-{over}}},\"x\":null}}"),
            format!("{{\"x\":{over},\"\\u0078\":0}}"),
        ] {
            assert_eq!(Json::parse(raw.as_bytes()), Err(Fault::Schema));
        }
        let allowed = format!("{{\"x\":{},\"x\":0}}", "9".repeat(4300));
        assert_eq!(
            Json::parse(allowed.as_bytes())?
                .get("x")
                .and_then(Json::as_number)
                .and_then(Number::as_integer),
            Some("0")
        );
        let text = format!("escaped \" {} \\", "9".repeat(5000));
        let quoted = serde_json::to_vec(&text)?;
        assert_eq!(Json::parse(&quoted)?.as_str(), Some(text.as_str()));
        assert!(Json::parse(format!("0.{}", "9".repeat(5000)).as_bytes()).is_ok());
        Ok(())
    }
}
