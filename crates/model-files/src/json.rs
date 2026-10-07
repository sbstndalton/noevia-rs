//! A small bounded JSON reader that keeps what Python's `json.loads` keeps: integer literals
//! exactly (any length up to the cap), floats as `f64`, `NaN`/`Infinity`/`-Infinity`, and the
//! last of duplicate object keys. Lone surrogates, which Rust strings cannot hold, are refused.

use crate::bigint::BigInt;
use crate::Error;

/// Deepest nesting of arrays/objects accepted.
pub const MAX_DEPTH: usize = 64;
/// Longest string (in characters, after unescaping) accepted anywhere in the input.
pub const MAX_STRING_CHARS: usize = 4096;
/// Longest number literal (in bytes) accepted.
pub const MAX_NUMBER_LEN: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(BigInt),
    Float(f64),
    Str(String),
    Arr(Vec<Value>),
    /// Key/value pairs in input order; lookups take the last match, as a Python dict does.
    Obj(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(pairs) => pairs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Python truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(i) => !i.is_zero(),
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Arr(a) => !a.is_empty(),
            Value::Obj(o) => !o.is_empty(),
        }
    }
}

pub fn parse(text: &str) -> Result<Value, Error> {
    let mut p = Parser {
        t: text,
        s: text.as_bytes(),
        i: 0,
    };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return Err(Error::Json("extra data after the value"));
    }
    Ok(v)
}

struct Parser<'a> {
    t: &'a str,
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &[u8]) -> bool {
        if self.s.get(self.i..self.i + lit.len()) == Some(lit) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, Error> {
        match self.peek() {
            Some(b'{') => self.object(depth + 1),
            Some(b'[') => self.array(depth + 1),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b'n') if self.eat(b"null") => Ok(Value::Null),
            Some(b't') if self.eat(b"true") => Ok(Value::Bool(true)),
            Some(b'f') if self.eat(b"false") => Ok(Value::Bool(false)),
            Some(b'N') if self.eat(b"NaN") => Ok(Value::Float(f64::NAN)),
            Some(b'I') if self.eat(b"Infinity") => Ok(Value::Float(f64::INFINITY)),
            Some(b'-') if self.eat(b"-Infinity") => Ok(Value::Float(f64::NEG_INFINITY)),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(Error::Json("expected a value")),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        self.i += 1;
        let mut out = Vec::new();
        self.ws();
        if self.eat(b"]") {
            return Ok(Value::Arr(out));
        }
        loop {
            self.ws();
            out.push(self.value(depth)?);
            self.ws();
            if self.eat(b"]") {
                return Ok(Value::Arr(out));
            }
            if !self.eat(b",") {
                return Err(Error::Json("expected ',' or ']'"));
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        self.i += 1;
        let mut out = Vec::new();
        self.ws();
        if self.eat(b"}") {
            return Ok(Value::Obj(out));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return Err(Error::Json("expected a string key"));
            }
            let key = self.string()?;
            self.ws();
            if !self.eat(b":") {
                return Err(Error::Json("expected ':'"));
            }
            self.ws();
            let v = self.value(depth)?;
            out.push((key, v));
            self.ws();
            if self.eat(b"}") {
                return Ok(Value::Obj(out));
            }
            if !self.eat(b",") {
                return Err(Error::Json("expected ',' or '}'"));
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, Error> {
        let h = self
            .s
            .get(self.i..self.i + 4)
            .ok_or(Error::Json("short \\u escape"))?;
        let h = std::str::from_utf8(h).map_err(|_| Error::Json("bad \\u escape"))?;
        if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Json("bad \\u escape"));
        }
        let v = u32::from_str_radix(h, 16).map_err(|_| Error::Json("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, Error> {
        self.i += 1; // opening quote
        let mut out = String::new();
        let mut chars = 0usize;
        loop {
            // `i` only ever moves by whole characters, so it is always a char boundary.
            let c = self
                .t
                .get(self.i..)
                .ok_or(Error::Json("unterminated string"))?
                .chars()
                .next()
                .ok_or(Error::Json("unterminated string"))?;
            self.i += c.len_utf8();
            let c = match c {
                '"' => return Ok(out),
                '\\' => {
                    let e = self.peek().ok_or(Error::Json("unterminated string"))?;
                    self.i += 1;
                    match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) && self.eat(b"\\u") {
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(Error::LoneSurrogate);
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                hi
                            };
                            char::from_u32(cp).ok_or(Error::LoneSurrogate)?
                        }
                        _ => return Err(Error::Json("bad escape")),
                    }
                }
                c if (c as u32) < 0x20 => return Err(Error::Json("control character in string")),
                c => c,
            };
            chars += 1;
            if chars > MAX_STRING_CHARS {
                return Err(Error::TooLong);
            }
            out.push(c);
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        self.i - start
    }

    fn number(&mut self) -> Result<Value, Error> {
        let start = self.i;
        self.eat(b"-");
        if !self.eat(b"0") && self.digits() == 0 {
            return Err(Error::Json("expected a value"));
        }
        let mut is_float = false;
        let save = self.i;
        if self.eat(b".") {
            if self.digits() == 0 {
                self.i = save; // Python stops the number before a bare '.'
            } else {
                is_float = true;
            }
        }
        let save = self.i;
        if self.eat(b"e") || self.eat(b"E") {
            let _sign = self.eat(b"+") || self.eat(b"-");
            if self.digits() == 0 {
                self.i = save;
            } else {
                is_float = true;
            }
        }
        let lit = self.s.get(start..self.i).ok_or(Error::Json("bad number"))?;
        if lit.len() > MAX_NUMBER_LEN {
            return Err(Error::TooLong);
        }
        let lit = std::str::from_utf8(lit).map_err(|_| Error::Json("bad number"))?;
        if is_float {
            lit.parse::<f64>()
                .map(Value::Float)
                .map_err(|_| Error::Json("bad number"))
        } else {
            BigInt::from_ascii_decimal(lit)
                .map(Value::Int)
                .ok_or(Error::Json("bad number"))
        }
    }
}
