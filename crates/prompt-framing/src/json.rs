//! `JSON.parse` as a value tree over UTF-16 code units (mcp-frame's ECMAScript-exact scanner), with
//! the property order `Object.keys` gives such a value: array-index keys (canonical integers up to
//! 2^32 - 2) ascending first, then the other keys in first-insertion order; a duplicate key keeps
//! its first position and takes the last value. Values nested deeper than a caller's cap become
//! [`Value::Deep`] (their contents are never built), so no tree is deeper than the cap.

use mcp_frame::json::{self, Scalar, Sink};
use std::collections::HashMap;

/// One JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// A number, as `JSON.parse` reads it.
    Num(f64),
    /// A string, as code units (lone surrogates kept).
    Str(Vec<u16>),
    /// An array.
    Arr(Vec<Value>),
    /// An object, in `Object.keys` order.
    Obj(Vec<(Vec<u16>, Value)>),
    /// A value deeper than the cap it was parsed with (contents not kept).
    Deep,
}

impl Value {
    /// The member `key` of an object.
    pub fn get(&self, key: &str) -> Option<&Value> {
        let Value::Obj(m) = self else { return None };
        m.iter()
            .find(|(k, _)| k.iter().copied().eq(key.encode_utf16()))
            .map(|(_, v)| v)
    }

    /// The string value, if this is one.
    pub fn as_str(&self) -> Option<&[u16]> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Whether `k` is an array index (a canonical numeric string of an integer below 2^32 - 1).
fn is_index(k: &[u16]) -> Option<u32> {
    if k.is_empty() || k.len() > 10 || (k.len() > 1 && k.first() == Some(&0x30)) {
        return None;
    }
    let mut n: u64 = 0;
    for &c in k {
        if !(0x30..=0x39).contains(&c) {
            return None;
        }
        n = n * 10 + u64::from(c - 0x30);
    }
    u32::try_from(n).ok().filter(|&n| n != u32::MAX)
}

struct Frame {
    array: bool,
    items: Vec<Value>,
    keys: Vec<Vec<u16>>,
    at: HashMap<Vec<u16>, usize>,
    pending: Option<Vec<u16>>,
}

struct Builder<'a> {
    s: &'a [u16],
    cap: usize,
    stack: Vec<Frame>,
    // Containers opened past the cap whose contents are being skipped.
    skip: usize,
    root: Option<Value>,
}

impl Builder<'_> {
    fn put(&mut self, v: Value) {
        let Some(top) = self.stack.last_mut() else {
            self.root = Some(v);
            return;
        };
        if top.array {
            top.items.push(v);
            return;
        }
        let key = top.pending.take().unwrap_or_default();
        if let Some(&i) = top.at.get(&key) {
            if let Some(slot) = top.items.get_mut(i) {
                *slot = v;
            }
        } else {
            top.at.insert(key.clone(), top.items.len());
            top.keys.push(key);
            top.items.push(v);
        }
    }
}

impl Sink for Builder<'_> {
    fn begin(&mut self, array: bool, _: usize) {
        if self.skip > 0 {
            self.skip += 1;
        } else if self.stack.len() >= self.cap {
            self.skip = 1;
        } else {
            self.stack.push(Frame {
                array,
                items: Vec::new(),
                keys: Vec::new(),
                at: HashMap::new(),
                pending: None,
            });
        }
    }
    fn key(&mut self, start: usize, end: usize) {
        if self.skip == 0 {
            if let Some(top) = self.stack.last_mut() {
                top.pending = Some(json::decode_string(self.s, start, end));
            }
        }
    }
    fn scalar(&mut self, kind: Scalar, start: usize, end: usize) {
        if self.skip > 0 {
            return;
        }
        let v = match kind {
            Scalar::Null => Value::Null,
            Scalar::True => Value::Bool(true),
            Scalar::False => Value::Bool(false),
            Scalar::Number => Value::Num(json::number_value(self.s, start, end)),
            Scalar::String => Value::Str(json::decode_string(self.s, start, end)),
        };
        self.put(v);
    }
    fn end(&mut self, _: usize) {
        if self.skip > 0 {
            self.skip -= 1;
            if self.skip == 0 {
                self.put(Value::Deep);
            }
            return;
        }
        let Some(f) = self.stack.pop() else { return };
        let v = if f.array {
            Value::Arr(f.items)
        } else {
            let mut pairs: Vec<(Vec<u16>, Value)> = f.keys.into_iter().zip(f.items).collect();
            // Stable: index keys ascending, the rest in insertion order.
            pairs.sort_by_key(|(k, _)| is_index(k).map_or(u64::MAX, u64::from));
            Value::Obj(pairs)
        };
        self.put(v);
    }
}

/// `JSON.parse(s)`, or `None` where it throws. Containers at depth `cap` or deeper (the root is
/// depth 0) are [`Value::Deep`]; scalars there are kept.
pub fn parse(s: &[u16], cap: usize) -> Option<Value> {
    let mut b = Builder {
        s,
        cap,
        stack: Vec::new(),
        skip: 0,
        root: None,
    };
    if !json::parse(s, &mut b) {
        return None;
    }
    b.root
}

/// Like [`parse`] over UTF-8 text.
pub fn parse_utf8(s: &[u8], cap: usize) -> Option<Value> {
    let text = std::str::from_utf8(s).ok()?;
    parse(&text.encode_utf16().collect::<Vec<u16>>(), cap)
}

/// Append `units` as a JSON string literal, as `JSON.stringify` writes it (lone surrogates as
/// lowercase `\udxxx`).
pub fn push_str(out: &mut Vec<u8>, units: &[u16]) {
    json::push_json_string(out, units);
}

/// Append an ASCII string as a JSON string literal.
pub fn push_ascii(out: &mut Vec<u8>, s: &str) {
    json::push_json_string(out, &s.encode_utf16().collect::<Vec<u16>>());
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::js::units;

    fn keys(v: &Value) -> Vec<String> {
        let Value::Obj(m) = v else { panic!() };
        m.iter()
            .map(|(k, _)| String::from_utf16(k).unwrap())
            .collect()
    }

    #[test]
    fn js_key_order() {
        let v = parse(
            &units(r#"{"b":1,"4294967295":2,"4294967294":3,"01":5,"7":6,"a":4,"b":9}"#),
            8,
        )
        .unwrap();
        assert_eq!(keys(&v), ["7", "4294967294", "b", "4294967295", "01", "a"]);
        assert_eq!(v.get("b"), Some(&Value::Num(9.0)));
    }

    #[test]
    fn depth_cap() {
        assert_eq!(
            parse(&units("[[1]]"), 1),
            Some(Value::Arr(vec![Value::Deep]))
        );
        assert_eq!(
            parse(&units("[[1],2]"), 1),
            Some(Value::Arr(vec![Value::Deep, Value::Num(2.0)]))
        );
        assert_eq!(parse(&units("{}"), 0), Some(Value::Deep));
        assert_eq!(parse(&units("\"x\""), 0), Some(Value::Str(units("x"))));
        let deep = "[".repeat(50_000) + &"]".repeat(50_000);
        assert!(parse(&units(&deep), 4).is_some());
        assert_eq!(parse(&units("[1,"), 4), None);
        assert_eq!(
            parse(&[0x22, 0x5c, 0x75, 0x64, 0x38, 0x30, 0x30, 0x22], 1),
            Some(Value::Str(vec![0xd800]))
        );
    }
}
