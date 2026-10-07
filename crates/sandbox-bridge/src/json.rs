//! ECMAScript `JSON.parse` acceptance, and a shallow value tree with `JSON.parse`'s semantics.
//!
//! [`validate`] accepts exactly the texts `JSON.parse` accepts: whitespace is only space, tab, LF
//! and CR; numbers follow the JSON grammar (no leading `+`, no leading zeros, no bare `.`); strings
//! refuse raw U+0000..U+001F and allow `\uXXXX` escapes for lone surrogates. It is iterative, so
//! (like V8's parser) nesting depth is limited only by the input size.
//!
//! [`parse`] builds a [`Value`] for the parts a caller looks at. Objects keep `JSON.parse`'s
//! duplicate-key rule (the last value wins, at the position of the first occurrence) and treat
//! `__proto__` as an ordinary own key. Strings are UTF-16 code units, so a lone surrogate survives.
//! Numbers keep their source lexeme, so re-emitting a value and parsing it again in JS yields the
//! same number (including `-0` and values that overflow to `Infinity`). Containers deeper than
//! [`TREE_DEPTH`] stay as their validated source text ([`Value::Raw`]).

use std::collections::HashMap;

/// Containers nested deeper than this stay [`Value::Raw`] (bounded recursion in [`parse`]).
pub const TREE_DEPTH: usize = 64;

/// A JSON value as JS sees it after `JSON.parse`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value<'a> {
    Null,
    Bool(bool),
    /// The number's source lexeme (valid JSON number syntax).
    Num(&'a str),
    /// UTF-16 code units.
    Str(Vec<u16>),
    Arr(Vec<Value<'a>>),
    /// Own keys in `JSON.parse` order (first occurrence), last value wins.
    Obj(Vec<(Vec<u16>, Value<'a>)>),
    /// A validated container below [`TREE_DEPTH`], as source text.
    Raw(&'a str),
}

impl<'a> Value<'a> {
    /// An own key of an object (`None` for anything that is not an object).
    pub fn get(&self, key: &str) -> Option<&Value<'a>> {
        let Value::Obj(fields) = self else {
            return None;
        };
        let key: Vec<u16> = key.encode_utf16().collect();
        fields.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    /// The string's code units, if this is a string.
    pub fn as_str16(&self) -> Option<&[u16]> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

const fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl Cur<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn ws(&mut self) {
        while matches!(self.peek(), Some(c) if is_ws(c)) {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn digits(&mut self) -> usize {
        let start = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        self.i - start
    }
    fn literal(&mut self, lit: &[u8]) -> bool {
        if self.b.get(self.i..self.i + lit.len()) == Some(lit) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }
    fn hex4(&mut self) -> Option<u16> {
        let s = self.b.get(self.i..self.i + 4)?;
        let mut v: u16 = 0;
        for &c in s {
            let d = (c as char).to_digit(16)?;
            v = (v << 4) | d as u16;
        }
        self.i += 4;
        Some(v)
    }
    /// A string at `"`; with `out`, its decoded UTF-16 code units.
    fn string(&mut self, mut out: Option<&mut Vec<u16>>) -> bool {
        if !self.eat(b'"') {
            return false;
        }
        loop {
            let Some(c) = self.peek() else {
                return false;
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return true;
                }
                b'\\' => {
                    self.i += 1;
                    let Some(e) = self.peek() else {
                        return false;
                    };
                    self.i += 1;
                    let unit = match e {
                        b'"' => 0x22,
                        b'\\' => 0x5c,
                        b'/' => 0x2f,
                        b'b' => 0x08,
                        b'f' => 0x0c,
                        b'n' => 0x0a,
                        b'r' => 0x0d,
                        b't' => 0x09,
                        b'u' => match self.hex4() {
                            Some(v) => v,
                            None => return false,
                        },
                        _ => return false,
                    };
                    if let Some(o) = out.as_deref_mut() {
                        o.push(unit);
                    }
                }
                0x00..=0x1f => return false,
                _ => {
                    // One UTF-8 sequence (the text is a &str, so it is well formed).
                    let len = match c {
                        0x00..=0x7f => 1,
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        _ => 4,
                    };
                    let Some(seq) = self.b.get(self.i..self.i + len) else {
                        return false;
                    };
                    if let Some(o) = out.as_deref_mut() {
                        let Ok(s) = std::str::from_utf8(seq) else {
                            return false;
                        };
                        o.extend(s.encode_utf16());
                    }
                    self.i += len;
                }
            }
        }
    }
    fn number(&mut self) -> bool {
        self.eat(b'-');
        if self.eat(b'0') {
        } else if matches!(self.peek(), Some(b'1'..=b'9')) {
            self.digits();
        } else {
            return false;
        }
        if self.eat(b'.') && self.digits() == 0 {
            return false;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            if self.digits() == 0 {
                return false;
            }
        }
        true
    }
    /// `"key"` ws `:` ws
    fn key(&mut self) -> bool {
        if !self.string(None) {
            return false;
        }
        self.ws();
        if !self.eat(b':') {
            return false;
        }
        self.ws();
        true
    }
    /// One scalar value (not a container).
    fn scalar(&mut self) -> bool {
        match self.peek() {
            Some(b'"') => self.string(None),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => false,
        }
    }
}

/// Whether ECMAScript `JSON.parse(text)` would succeed.
pub fn validate(text: &str) -> bool {
    let mut c = Cur {
        b: text.as_bytes(),
        i: 0,
    };
    let mut stack: Vec<u8> = Vec::new();
    c.ws();
    'value: loop {
        match c.peek() {
            Some(b'{') => {
                c.i += 1;
                c.ws();
                if !c.eat(b'}') {
                    stack.push(b'{');
                    if !c.key() {
                        return false;
                    }
                    continue 'value;
                }
            }
            Some(b'[') => {
                c.i += 1;
                c.ws();
                if !c.eat(b']') {
                    stack.push(b'[');
                    continue 'value;
                }
            }
            _ => {
                if !c.scalar() {
                    return false;
                }
            }
        }
        loop {
            c.ws();
            let Some(&open) = stack.last() else {
                return c.i == c.b.len();
            };
            match c.peek() {
                Some(b',') => {
                    c.i += 1;
                    c.ws();
                    if open == b'{' && !c.key() {
                        return false;
                    }
                    continue 'value;
                }
                Some(b'}') if open == b'{' => {
                    c.i += 1;
                    stack.pop();
                }
                Some(b']') if open == b'[' => {
                    c.i += 1;
                    stack.pop();
                }
                _ => return false,
            }
        }
    }
}

/// `JSON.parse(text)` as a shallow [`Value`], or `None` where `JSON.parse` would throw.
pub fn parse(text: &str) -> Option<Value<'_>> {
    if !validate(text) {
        return None;
    }
    let mut c = Cur {
        b: text.as_bytes(),
        i: 0,
    };
    c.ws();
    let v = tree(&mut c, text, 0)?;
    c.ws();
    (c.i == text.len()).then_some(v)
}

/// Skip one validated container starting at `[` or `{`.
fn skip_container(c: &mut Cur<'_>) -> Option<()> {
    let mut depth = 0usize;
    loop {
        match c.peek()? {
            b'"' => {
                if !c.string(None) {
                    return None;
                }
                continue;
            }
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    c.i += 1;
                    return Some(());
                }
            }
            _ => {}
        }
        c.i += 1;
    }
}

fn tree<'a>(c: &mut Cur<'a>, text: &'a str, depth: usize) -> Option<Value<'a>> {
    match c.peek()? {
        b'[' | b'{' if depth >= TREE_DEPTH => {
            let start = c.i;
            skip_container(c)?;
            Some(Value::Raw(text.get(start..c.i)?))
        }
        b'{' => {
            c.i += 1;
            c.ws();
            let mut fields: Vec<(Vec<u16>, Value<'a>)> = Vec::new();
            let mut index: HashMap<Vec<u16>, usize> = HashMap::new();
            if c.eat(b'}') {
                return Some(Value::Obj(fields));
            }
            loop {
                c.ws();
                let mut key = Vec::new();
                if !c.string(Some(&mut key)) {
                    return None;
                }
                c.ws();
                if !c.eat(b':') {
                    return None;
                }
                c.ws();
                let v = tree(c, text, depth + 1)?;
                match index.get(&key) {
                    Some(&at) => {
                        if let Some(slot) = fields.get_mut(at) {
                            slot.1 = v;
                        }
                    }
                    None => {
                        index.insert(key.clone(), fields.len());
                        fields.push((key, v));
                    }
                }
                c.ws();
                if c.eat(b',') {
                    continue;
                }
                if c.eat(b'}') {
                    return Some(Value::Obj(fields));
                }
                return None;
            }
        }
        b'[' => {
            c.i += 1;
            c.ws();
            let mut items = Vec::new();
            if c.eat(b']') {
                return Some(Value::Arr(items));
            }
            loop {
                c.ws();
                items.push(tree(c, text, depth + 1)?);
                c.ws();
                if c.eat(b',') {
                    continue;
                }
                if c.eat(b']') {
                    return Some(Value::Arr(items));
                }
                return None;
            }
        }
        b'"' => {
            let mut s = Vec::new();
            c.string(Some(&mut s)).then_some(Value::Str(s))
        }
        b't' => c.literal(b"true").then_some(Value::Bool(true)),
        b'f' => c.literal(b"false").then_some(Value::Bool(false)),
        b'n' => c.literal(b"null").then_some(Value::Null),
        _ => {
            let start = c.i;
            if !c.number() {
                return None;
            }
            Some(Value::Num(text.get(start..c.i)?))
        }
    }
}

/// Append `s` (UTF-16) as a JSON string literal exactly as `JSON.stringify` writes it
/// (QuoteJSONString: short escapes, other controls and lone surrogates as `\uXXXX`).
pub fn write_str16(s: &[u16], out: &mut String) {
    out.push('"');
    for r in char::decode_utf16(s.iter().copied()) {
        match r {
            Ok('"') => out.push_str("\\\""),
            Ok('\\') => out.push_str("\\\\"),
            Ok('\n') => out.push_str("\\n"),
            Ok('\r') => out.push_str("\\r"),
            Ok('\t') => out.push_str("\\t"),
            Ok('\u{8}') => out.push_str("\\b"),
            Ok('\u{c}') => out.push_str("\\f"),
            Ok(ch) if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            Ok(ch) => out.push(ch),
            Err(e) => out.push_str(&format!("\\u{:04x}", e.unpaired_surrogate())),
        }
    }
    out.push('"');
}

/// Append `s` as a JSON string literal.
pub fn write_str(s: &str, out: &mut String) {
    let units: Vec<u16> = s.encode_utf16().collect();
    write_str16(&units, out);
}

/// Append `v` as JSON text that JS `JSON.parse` turns back into the same value.
pub fn write_value(v: &Value<'_>, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Num(n) | Value::Raw(n) => out.push_str(n),
        Value::Str(s) => write_str16(s, out),
        Value::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Obj(fields) => {
            out.push('{');
            for (i, (k, val)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str16(k, out);
                out.push(':');
                write_value(val, out);
            }
            out.push('}');
        }
    }
}

/// Whether `key` is an array index (canonical decimal below 2^32 - 1): V8 lists those own keys
/// first, in ascending order, before the string keys in insertion order.
fn array_index(key: &[u16]) -> Option<u32> {
    let s = String::from_utf16(key).ok()?;
    if s.is_empty() || (s.len() > 1 && s.starts_with('0')) || !s.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let n: u64 = s.parse().ok()?;
    (n < u64::from(u32::MAX)).then_some(n as u32)
}

/// `JSON.stringify(JSON.parse(text of v))`: the canonical bytes JS would send for this value
/// (numbers as Number::toString, non-finite as `null`, array-index keys first). For tests and
/// differential comparison; `None` past [`crate::js::MAX_COERCE_DEPTH`] levels of raw nesting.
pub fn js_stringify(v: &Value<'_>) -> Option<String> {
    let mut out = String::new();
    stringify_into(v, &mut out, 0)?;
    Some(out)
}

fn stringify_into(v: &Value<'_>, out: &mut String, depth: usize) -> Option<()> {
    if depth > crate::js::MAX_COERCE_DEPTH {
        return None;
    }
    match v {
        Value::Num(n) => {
            let x = n.parse::<f64>().ok()?;
            if x.is_finite() {
                out.push_str(&crate::js::number_to_string(x));
            } else {
                out.push_str("null");
            }
        }
        Value::Raw(text) => stringify_into(&parse(text)?, out, depth + 1)?,
        Value::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                stringify_into(item, out, depth + 1)?;
            }
            out.push(']');
        }
        Value::Obj(fields) => {
            let mut indexed: Vec<(u32, usize)> = Vec::new();
            let mut named: Vec<usize> = Vec::new();
            for (i, (k, _)) in fields.iter().enumerate() {
                match array_index(k) {
                    Some(n) => indexed.push((n, i)),
                    None => named.push(i),
                }
            }
            indexed.sort_unstable();
            out.push('{');
            let order = indexed.iter().map(|(_, i)| *i).chain(named);
            for (n, i) in order.enumerate() {
                let (k, val) = fields.get(i)?;
                if n > 0 {
                    out.push(',');
                }
                write_str16(k, out);
                out.push(':');
                stringify_into(val, out, depth + 1)?;
            }
            out.push('}');
        }
        other => write_value(other, out),
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar() {
        for ok in [
            "0",
            "-0",
            "1.5e+3",
            "\"\\ud800\"",
            " {\"a\" : [1, {}]}\r\n",
            "null",
            "[[[]]]",
        ] {
            assert!(validate(ok), "{ok}");
        }
        for bad in [
            "",
            "01",
            "+1",
            ".5",
            "1.",
            "1e",
            "[1,]",
            "{\"a\":1,}",
            "\u{feff}1",
            "\"\t\"",
            "'a'",
            "[1]x",
            "{a:1}",
            "\"\\x\"",
            "\u{a0}1",
            "NaN",
            "\"\\u12\"",
        ] {
            assert!(!validate(bad), "{bad:?}");
        }
    }

    #[test]
    fn duplicate_keys_last_wins_first_position() {
        let v = parse("{\"a\":1,\"b\":2,\"a\":3,\"__proto__\":4}").unwrap_or(Value::Null);
        let mut out = String::new();
        write_value(&v, &mut out);
        assert_eq!(out, "{\"a\":3,\"b\":2,\"__proto__\":4}");
    }
}
