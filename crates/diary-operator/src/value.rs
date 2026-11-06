//! Python scalar and ordered mapping semantics used by the two offline loaders.
//! Aliases share nodes until configuration overrides have finished.
use diary_backup_format::python_float;
use num_bigint_dig::BigInt;
use std::{cell::RefCell, rc::Rc};
pub type Result<T> = std::result::Result<T, String>;
#[derive(Clone, Debug)]
pub struct Value(pub(crate) Rc<RefCell<Kind>>);
#[derive(Clone, Debug)]
pub enum Kind {
    Null,
    Bool(bool),
    Int(String),
    Float(f64),
    Str(String),
    Surrogate(Vec<u16>),
    Seq(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Unsupported(&'static str),
}
impl Value {
    pub fn new(kind: Kind) -> Self {
        Self(Rc::new(RefCell::new(kind)))
    }
    pub fn null() -> Self {
        Self::new(Kind::Null)
    }
    pub fn string(s: impl Into<String>) -> Self {
        Self::new(Kind::Str(s.into()))
    }
    pub fn map() -> Self {
        Self::new(Kind::Map(Vec::new()))
    }
    pub fn kind(&self) -> Kind {
        self.0.borrow().clone()
    }
    pub fn type_name(&self) -> &'static str {
        match self.kind() {
            Kind::Null => "NoneType",
            Kind::Bool(_) => "bool",
            Kind::Int(_) => "int",
            Kind::Float(_) => "float",
            Kind::Str(_) | Kind::Surrogate(_) => "str",
            Kind::Seq(_) => "list",
            Kind::Map(_) => "dict",
            Kind::Unsupported(s) => s,
        }
    }
    pub fn truthy(&self) -> bool {
        match self.kind() {
            Kind::Null => false,
            Kind::Bool(b) => b,
            Kind::Int(s) => s != "0",
            Kind::Float(n) => n != 0.0,
            Kind::Str(s) => !s.is_empty(),
            Kind::Surrogate(s) => !s.is_empty(),
            Kind::Seq(a) => !a.is_empty(),
            Kind::Map(a) => !a.is_empty(),
            _ => true,
        }
    }
    pub fn as_str(&self) -> Option<String> {
        if let Kind::Str(s) = self.kind() {
            Some(s)
        } else {
            None
        }
    }
    pub fn get(&self, key: &str) -> Option<Self> {
        if let Kind::Map(rows) = self.kind() {
            rows.iter()
                .find(|(k, _)| k.as_str().as_deref() == Some(key))
                .map(|(_, v)| v.clone())
        } else {
            None
        }
    }
    pub fn entries(&self) -> Option<Vec<(Value, Value)>> {
        if let Kind::Map(a) = self.kind() {
            Some(a)
        } else {
            None
        }
    }
    pub fn as_array(&self) -> Option<Vec<Value>> {
        if let Kind::Seq(a) = self.kind() {
            Some(a)
        } else {
            None
        }
    }
    pub fn insert(&self, key: Value, value: Value) -> Result<()> {
        let type_name = self.type_name();
        let mut kind = self.0.borrow_mut();
        let Kind::Map(rows) = &mut *kind else {
            return Err(format!(
                "TypeError: '{}' object does not support item assignment",
                type_name
            ));
        };
        if let Some((_, v)) = rows.iter_mut().find(|(k, _)| k.key_eq(&key)) {
            *v = value;
        } else {
            rows.push((key, value));
        }
        Ok(())
    }
    pub fn set(&self, key: &str, value: Value) -> Result<()> {
        self.insert(Self::string(key), value)
    }
    fn integer_key(&self) -> Option<String> {
        match self.kind() {
            Kind::Int(s) => Some(s),
            Kind::Bool(b) => Some(if b { "1" } else { "0" }.into()),
            Kind::Float(n) if n.is_finite() && n.fract() == 0.0 => Some(if n == 0.0 {
                "0".into()
            } else {
                format!("{n:.0}")
            }),
            _ => None,
        }
    }
    fn key_eq(&self, other: &Self) -> bool {
        if Rc::ptr_eq(&self.0, &other.0) {
            return true;
        }
        if let (Some(a), Some(b)) = (self.integer_key(), other.integer_key()) {
            return a == b;
        }
        match (self.kind(), other.kind()) {
            (Kind::Null, Kind::Null) => true,
            (Kind::Str(a), Kind::Str(b)) => a == b,
            (Kind::Surrogate(a), Kind::Surrogate(b)) => a == b,
            (Kind::Float(a), Kind::Float(b)) => a == b,
            _ => false,
        }
    }
    pub fn pretty(&self) -> Result<String> {
        let mut seen = Vec::new();
        self.write(0, &mut seen)
    }
    fn write(&self, depth: usize, seen: &mut Vec<*const RefCell<Kind>>) -> Result<String> {
        let ptr = Rc::as_ptr(&self.0);
        if seen.contains(&ptr) {
            return Err("ValueError: Circular reference detected".into());
        }
        // CPython's serializer has a recursion limit too, rather than risking a native stack overflow.
        if depth > 500 {
            return Err(
                "RecursionError: maximum recursion depth exceeded while encoding a JSON object"
                    .into(),
            );
        }
        let padding = " ".repeat(depth * 2);
        let child = " ".repeat((depth + 1) * 2);
        match self.kind() {
            Kind::Null => Ok("null".into()),
            Kind::Bool(v) => Ok(if v { "true" } else { "false" }.into()),
            Kind::Int(v) => {
                if v.trim_start_matches('-').len() > 4300 {
                    Err("ValueError: Exceeds the limit (4300 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit".into())
                } else {
                    Ok(v)
                }
            }
            Kind::Float(v) => Ok(python_float(v)),
            Kind::Str(v) => Ok(quote(&v)),
            Kind::Surrogate(v) => Ok(quote_units(&v)),
            Kind::Seq(rows) => {
                if rows.is_empty() {
                    return Ok("[]".into());
                }
                seen.push(ptr);
                let result = rows
                    .iter()
                    .map(|v| v.write(depth + 1, seen).map(|s| format!("{child}{s}")))
                    .collect::<Result<Vec<_>>>();
                seen.pop();
                Ok(format!("[\n{}\n{padding}]", result?.join(",\n")))
            }
            Kind::Map(rows) => {
                if rows.is_empty() {
                    return Ok("{}".into());
                }
                seen.push(ptr);
                let result = rows
                    .iter()
                    .map(|(k, v)| {
                        let key = match k.kind() {
                            Kind::Str(s) => s,
                            Kind::Surrogate(s) => {
                                return Ok(format!(
                                    "{child}{}: {}",
                                    quote_units(&s),
                                    v.write(depth + 1, seen)?
                                ));
                            }
                            Kind::Null => "null".into(),
                            Kind::Bool(b) => if b { "true" } else { "false" }.into(),
                            Kind::Int(s) => s,
                            Kind::Float(n) => python_float(n),
                            _ => {
                                return Err(format!(
                                    "TypeError: keys must be str, int, float, bool or None, not {}",
                                    k.type_name()
                                ));
                            }
                        };
                        Ok(format!(
                            "{child}{}: {}",
                            quote(&key),
                            v.write(depth + 1, seen)?
                        ))
                    })
                    .collect::<Result<Vec<_>>>();
                seen.pop();
                Ok(format!("{{\n{}\n{padding}}}", result?.join(",\n")))
            }
            Kind::Unsupported(s) => Err(format!(
                "TypeError: Object of type {s} is not JSON serializable"
            )),
        }
    }
}
pub fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (' '..='~').contains(&c) => out.push(c),
            _ => {
                for u in c.encode_utf16(&mut [0; 2]) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
        }
    }
    out.push('"');
    out
}
pub fn repr(s: &str) -> String {
    let delimiter = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = delimiter.to_string();
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == delimiter => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            _ => out.push(c),
        }
    }
    out.push(delimiter);
    out
}
/// str.strip whitespace (Python also strips the four ASCII information separators).
pub fn strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}
/// Decimal digit blocks from the reference runtime's Unicode database.
fn decimal_digits(s: &str) -> Option<String> {
    const ZEROES: &[u32] = &[
        0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66,
        0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90,
        0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0,
        0xff10, 0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0,
        0x11650, 0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16a60,
        0x16ac0, 0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0,
        0x1e950, 0x1fbf0,
    ];
    s.chars()
        .map(|c| {
            if c == '_' || c == '-' || c == '+' {
                Some(c)
            } else {
                ZEROES
                    .iter()
                    .find(|&&z| (z..z + 10).contains(&(c as u32)))
                    .and_then(|&z| char::from_u32('0' as u32 + c as u32 - z))
            }
        })
        .collect()
}
pub fn integer(s: &str, radix: u32) -> Result<Value> {
    let normalized = if radix == 10 {
        decimal_digits(s).unwrap_or_else(|| s.to_owned())
    } else {
        s.to_owned()
    };
    let s = normalized.replace('_', "");
    let digits = s.trim_start_matches(['-', '+']).len();
    if radix == 10 && digits > 4300 {
        return Err(format!("ValueError: Exceeds the limit (4300 digits) for integer string conversion: value has {digits} digits; use sys.set_int_max_str_digits() to increase the limit"));
    }
    let value = BigInt::parse_bytes(s.as_bytes(), radix).ok_or_else(|| {
        format!(
            "ValueError: invalid literal for int() with base {radix}: {}",
            repr(&s)
        )
    })?;
    Ok(Value::new(Kind::Int(value.to_string())))
}
pub fn decode_error(codec: &str, bytes: &[u8], at: usize, len: usize, reason: &str) -> String {
    let detail = if len == 1 {
        format!(
            "byte 0x{:02x} in position {at}",
            bytes.get(at).copied().unwrap_or(0)
        )
    } else {
        format!("bytes in position {at}-{}", at + len.saturating_sub(1))
    };
    format!("UnicodeDecodeError: '{codec}' codec can't decode {detail}: {reason}")
}
pub fn decode_utf8(bytes: &[u8]) -> Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(|e| {
        let error = e.utf8_error();
        let at = error.valid_up_to();
        let b = bytes.get(at).copied().unwrap_or(0);
        let reason = if error.error_len().is_none() {
            "unexpected end of data"
        } else if (0xc2..=0xf4).contains(&b) {
            "invalid continuation byte"
        } else {
            "invalid start byte"
        };
        decode_error(
            "utf-8",
            bytes,
            at,
            error.error_len().unwrap_or(bytes.len() - at),
            reason,
        )
    })
}
fn quote_units(units: &[u16]) -> String {
    let mut out = String::from("\"");
    for &u in units {
        if let Some(c) =
            char::from_u32(u as u32).filter(|c| (' '..='~').contains(c) && *c != '"' && *c != '\\')
        {
            out.push(c);
        } else {
            match u {
                34 => out.push_str("\\\""),
                92 => out.push_str("\\\\"),
                10 => out.push_str("\\n"),
                13 => out.push_str("\\r"),
                9 => out.push_str("\\t"),
                8 => out.push_str("\\b"),
                12 => out.push_str("\\f"),
                _ => out.push_str(&format!("\\u{u:04x}")),
            }
        }
    }
    out.push('"');
    out
}
pub fn json(bytes: &[u8]) -> Result<Value> {
    crate::json::parse(bytes)
}
