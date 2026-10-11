//! Bounded JSON scanner preserving JavaScript UTF-16 strings and number overflow.
//! Parsing uses an explicit container stack. Conversion follows String(value || '').
use crate::{Failure, BODY_BYTES, JSON_DEPTH};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(Vec<u16>),
    Array(Vec<Value>),
    Object(Vec<(Vec<u16>, Value)>),
}

enum Frame {
    Array {
        values: Vec<Value>,
        state: u8,
    },
    Object {
        values: Vec<(Vec<u16>, Value)>,
        key: Vec<u16>,
        state: u8,
    },
}

fn invalid() -> Failure {
    Failure::new(400, "invalid JSON")
}

struct Scanner<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl Scanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }
    fn whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.at += 1;
        }
    }
    fn consume(&mut self, byte: u8) -> Result<(), Failure> {
        self.whitespace();
        if self.peek() != Some(byte) {
            return Err(invalid());
        }
        self.at += 1;
        Ok(())
    }
    fn string(&mut self) -> Result<Vec<u16>, Failure> {
        self.consume(b'"')?;
        let mut units = Vec::new();
        loop {
            match self.peek().ok_or_else(invalid)? {
                b'"' => {
                    self.at += 1;
                    return Ok(units);
                }
                b'\\' => {
                    self.at += 1;
                    let escape = self.peek().ok_or_else(invalid)?;
                    self.at += 1;
                    let unit = match escape {
                        b'"' => 34,
                        b'\\' => 92,
                        b'/' => 47,
                        b'b' => 8,
                        b'f' => 12,
                        b'n' => 10,
                        b'r' => 13,
                        b't' => 9,
                        b'u' => {
                            let bytes = self.bytes.get(self.at..self.at + 4).ok_or_else(invalid)?;
                            let mut n = 0u16;
                            for b in bytes {
                                let digit = match b {
                                    b'0'..=b'9' => b - b'0',
                                    b'a'..=b'f' => b - b'a' + 10,
                                    b'A'..=b'F' => b - b'A' + 10,
                                    _ => return Err(invalid()),
                                };
                                n = n * 16 + u16::from(digit);
                            }
                            self.at += 4;
                            n
                        }
                        _ => return Err(invalid()),
                    };
                    units.push(unit);
                }
                0..=31 => return Err(invalid()),
                _ => {
                    let width = match self.peek() {
                        Some(0..=127) => 1,
                        Some(192..=223) => 2,
                        Some(224..=239) => 3,
                        Some(240..=247) => 4,
                        _ => return Err(invalid()),
                    };
                    let rest = std::str::from_utf8(
                        self.bytes
                            .get(self.at..self.at + width)
                            .ok_or_else(invalid)?,
                    )
                    .map_err(|_| invalid())?;
                    let c = rest.chars().next().ok_or_else(invalid)?;
                    let mut buf = [0u16; 2];
                    units.extend_from_slice(c.encode_utf16(&mut buf));
                    self.at += c.len_utf8();
                }
            }
        }
    }
    fn primitive(&mut self) -> Result<Value, Failure> {
        self.whitespace();
        for (literal, value) in [
            (b"null".as_slice(), Value::Null),
            (b"true".as_slice(), Value::Bool(true)),
            (b"false".as_slice(), Value::Bool(false)),
        ] {
            if self.bytes.get(self.at..self.at + literal.len()) == Some(literal) {
                self.at += literal.len();
                return Ok(value);
            }
        }
        if self.peek() == Some(b'"') {
            return self.string().map(Value::String);
        }
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.at += 1;
                }
            }
            _ => return Err(invalid()),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            let digits = self.at;
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.at += 1;
            }
            if self.at == digits {
                return Err(invalid());
            }
        }
        if self.peek().is_some_and(|b| matches!(b, b'e' | b'E')) {
            self.at += 1;
            if self.peek().is_some_and(|b| matches!(b, b'+' | b'-')) {
                self.at += 1;
            }
            let digits = self.at;
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.at += 1;
            }
            if self.at == digits {
                return Err(invalid());
            }
        }
        let text = std::str::from_utf8(self.bytes.get(start..self.at).ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
        text.parse::<f64>()
            .map(Value::Number)
            .map_err(|_| invalid())
    }
}

pub fn parse(bytes: &[u8]) -> Result<Value, Failure> {
    if bytes.len() > BODY_BYTES {
        return Err(Failure::new(413, "Request exceeds size limit"));
    }
    // Node readJson decodes its Buffer with replacement before JSON.parse.
    let decoded = String::from_utf8_lossy(bytes);
    let mut scan = Scanner {
        bytes: decoded.as_bytes(),
        at: 0,
    };
    let mut stack = Vec::<Frame>::new();
    let mut ready = None;
    loop {
        scan.whitespace();
        if let Some(value) = ready.take() {
            match stack.last_mut() {
                Some(Frame::Array { values, state }) => {
                    values.push(value);
                    *state = 1;
                }
                Some(Frame::Object { values, key, state }) => {
                    values.push((std::mem::take(key), value));
                    *state = 3;
                }
                None => {
                    if scan.peek().is_some() {
                        return Err(invalid());
                    }
                    return Ok(value);
                }
            }
        }
        let mut want_value = stack.is_empty();
        let mut close = false;
        match stack.last_mut() {
            Some(Frame::Array { state, .. }) => match *state {
                0 | 2 => {
                    if *state == 0 && scan.peek() == Some(b']') {
                        scan.at += 1;
                        close = true;
                    } else {
                        want_value = true;
                    }
                }
                _ => match scan.peek() {
                    Some(b']') => {
                        scan.at += 1;
                        close = true;
                    }
                    Some(b',') => {
                        scan.at += 1;
                        *state = 2;
                    }
                    _ => return Err(invalid()),
                },
            },
            Some(Frame::Object { state, key, .. }) => match *state {
                0 | 4 => {
                    if *state == 0 && scan.peek() == Some(b'}') {
                        scan.at += 1;
                        close = true;
                    } else {
                        *key = scan.string()?;
                        *state = 1;
                    }
                }
                1 => {
                    scan.consume(b':')?;
                    *state = 2;
                }
                2 => want_value = true,
                _ => match scan.peek() {
                    Some(b'}') => {
                        scan.at += 1;
                        close = true;
                    }
                    Some(b',') => {
                        scan.at += 1;
                        *state = 4;
                    }
                    _ => return Err(invalid()),
                },
            },
            None => {}
        }
        if close {
            ready = match stack.pop() {
                Some(Frame::Array { values, .. }) => Some(Value::Array(values)),
                Some(Frame::Object { values, .. }) => Some(Value::Object(values)),
                None => return Err(invalid()),
            };
        } else if want_value {
            scan.whitespace();
            match scan.peek() {
                Some(b'[' | b'{') => {
                    if stack.len() >= JSON_DEPTH {
                        return Err(invalid());
                    }
                    let frame = if scan.peek() == Some(b'[') {
                        Frame::Array {
                            values: Vec::new(),
                            state: 0,
                        }
                    } else {
                        Frame::Object {
                            values: Vec::new(),
                            key: Vec::new(),
                            state: 0,
                        }
                    };
                    scan.at += 1;
                    stack.push(frame);
                }
                _ => ready = Some(scan.primitive()?),
            }
        }
    }
}

fn field<'a>(pairs: &'a [(Vec<u16>, Value)], key: &str) -> Option<&'a Value> {
    pairs
        .iter()
        .rev()
        .find(|(k, _)| k.iter().copied().eq(key.encode_utf16()))
        .map(|(_, v)| v)
}

pub fn path(bytes: &[u8]) -> Result<Vec<u16>, Failure> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let Value::Object(pairs) = parse(bytes)? else {
        return Err(Failure::new(400, "request body must be a JSON object"));
    };
    field(&pairs, "path").map_or_else(|| Ok(Vec::new()), coerce_path)
}

/// `String(value || '')`, distinguishing array element null from path null.
pub fn coerce_path(value: &Value) -> Result<Vec<u16>, Failure> {
    if matches!(value, Value::Null | Value::Bool(false))
        || matches!(value, Value::Number(n) if *n == 0.0)
    {
        return Ok(Vec::new());
    }
    to_string(value)
}

fn to_string(value: &Value) -> Result<Vec<u16>, Failure> {
    let text = match value {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_json::number_to_string(*n),
        Value::String(s) => return Ok(s.clone()),
        Value::Object(pairs) => {
            // JSON values cannot contain a callable hook. Default Object toString
            // succeeds even with a noncallable valueOf; an own toString blocks it.
            if field(pairs, "toString").is_some() {
                return Err(Failure::new(
                    502,
                    "Cannot convert object to primitive value",
                ));
            }
            "[object Object]".to_owned()
        }
        Value::Array(values) => {
            let mut out = Vec::new();
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    out.push(44);
                }
                if !matches!(value, Value::Null) {
                    out.extend(to_string(value)?);
                }
            }
            return Ok(out);
        }
    };
    Ok(text.encode_utf16().collect())
}

pub fn quote_units(out: &mut String, units: &[u16]) {
    out.push('"');
    for decoded in char::decode_utf16(units.iter().copied()) {
        match decoded {
            Ok('"') => out.push_str("\\\""),
            Ok('\\') => out.push_str("\\\\"),
            Ok('\u{8}') => out.push_str("\\b"),
            Ok('\u{c}') => out.push_str("\\f"),
            Ok('\n') => out.push_str("\\n"),
            Ok('\r') => out.push_str("\\r"),
            Ok('\t') => out.push_str("\\t"),
            Ok(c) if c < '\u{20}' => escape_unit(out, c as u16),
            Ok(c) => out.push(c),
            Err(e) => escape_unit(out, e.unpaired_surrogate()),
        }
    }
    out.push('"');
}

fn escape_unit(out: &mut String, unit: u16) {
    out.push_str("\\u");
    for shift in [12, 8, 4, 0] {
        let digit = u32::from((unit >> shift) & 15);
        out.push(char::from_digit(digit, 16).unwrap_or('0'));
    }
}
