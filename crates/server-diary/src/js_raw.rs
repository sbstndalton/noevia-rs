//! `JSON.stringify(JSON.parse(text))` without the limits of a parsed tree: no nesting limit, and every
//! number JavaScript can read (`1e400` is `Infinity`, which `JSON.stringify` writes as `null`).
//!
//! [`valid`] is `JSON.parse`'s acceptance: serde_json validates a raw value iteratively, with no
//! depth limit, and accepts any `\u` escape (lone surrogates included), as `JSON.parse` does.
//! [`read`] then rewrites a valid text in one iterative pass with an explicit stack: whitespace
//! dropped, strings re-quoted as `JSON.stringify` quotes them (lone surrogates kept, via
//! [`js_json::wtf16`]), numbers written as JavaScript writes them, and every object in JavaScript's
//! property order (array-index keys first, ascending; a later duplicate replaces the value in the
//! first one's place). A top-level object is handed back as its entries, so a caller can read or
//! drop a property and write the rest exactly as Node would.
use js_json::{wtf16, JValue};
use serde_json::value::RawValue;
use std::collections::HashMap;

/// Whether `JSON.parse(text)` succeeds.
pub(crate) fn valid(text: &str) -> bool {
    serde_json::from_str::<&RawValue>(text).is_ok()
}

/// A parsed and re-written text.
pub(crate) enum Top {
    /// A top-level object: `(key, canonical value)` in JavaScript's property order.
    Object(Vec<(String, String)>),
    /// Anything else, already canonical.
    Other(String),
}

#[derive(Default)]
struct Obj {
    entries: Vec<(String, String)>,
    at: HashMap<String, usize>,
    /// The key read, waiting for its value.
    key: Option<String>,
}

impl Obj {
    fn set(&mut self, key: String, value: String) {
        if let Some(slot) = self.at.get(&key).and_then(|i| self.entries.get_mut(*i)) {
            slot.1 = value;
            return;
        }
        self.at.insert(key.clone(), self.entries.len());
        self.entries.push((key, value));
    }
    fn finish(self) -> Vec<(String, String)> {
        let mut entries = self.entries;
        // A stable sort: index keys ascending first, the rest in the order they came.
        entries.sort_by_key(|(k, _)| match wtf16::array_index(k) {
            Some(n) => (0u8, n),
            None => (1u8, 0),
        });
        entries
    }
}

enum Frame {
    Arr { out: String, first: bool },
    Obj(Obj),
}

/// `JSON.stringify` of one string key or value.
fn quoted(s: String) -> Option<String> {
    wtf16::stringify(&JValue::Str(s))
}

/// `{"k":v,...}` of entries [`read`] gave.
pub(crate) fn join<'a>(entries: impl IntoIterator<Item = &'a (String, String)>) -> Option<String> {
    let mut out = String::from("{");
    for (i, (k, v)) in entries.into_iter().enumerate() {
        if i != 0 {
            out.push(',');
        }
        out.push_str(&quoted(k.clone())?);
        out.push(':');
        out.push_str(v);
    }
    out.push('}');
    Some(out)
}

/// The decoded string of a canonical value, when it is a string.
pub(crate) fn string_of(value: &str) -> Option<String> {
    if !value.starts_with('"') {
        return None;
    }
    match wtf16::parse(value).ok()? {
        JValue::Str(s) => Some(s),
        _ => None,
    }
}

fn string_end(b: &[u8], start: usize) -> Option<usize> {
    let mut j = start + 1;
    loop {
        match *b.get(j)? {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
}

fn scalar_end(b: &[u8], start: usize) -> usize {
    let mut j = start;
    while let Some(c) = b.get(j) {
        if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
            break;
        }
        j += 1;
    }
    j
}

fn emit(stack: &mut [Frame], done: &mut Option<Top>, text: String) -> Option<()> {
    match stack.last_mut() {
        None => *done = Some(Top::Other(text)),
        Some(Frame::Arr { out, first }) => {
            if !*first {
                out.push(',');
            }
            *first = false;
            out.push_str(&text);
        }
        Some(Frame::Obj(obj)) => {
            let key = obj.key.take()?;
            obj.set(key, text);
        }
    }
    Some(())
}

/// [`valid`] text, re-written. `None` only for a text [`valid`] refused.
pub(crate) fn read(text: &str) -> Option<Top> {
    if !valid(text) {
        return None;
    }
    let b = text.as_bytes();
    let mut i = 0usize;
    let mut stack: Vec<Frame> = Vec::new();
    let mut done: Option<Top> = None;
    while let Some(&c) = b.get(i) {
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' => i += 1,
            b'{' => {
                stack.push(Frame::Obj(Obj::default()));
                i += 1;
            }
            b'[' => {
                stack.push(Frame::Arr {
                    out: String::from("["),
                    first: true,
                });
                i += 1;
            }
            b'}' => {
                i += 1;
                let Some(Frame::Obj(obj)) = stack.pop() else {
                    return None;
                };
                let entries = obj.finish();
                if stack.is_empty() {
                    done = Some(Top::Object(entries));
                } else {
                    emit(&mut stack, &mut done, join(&entries)?)?;
                }
            }
            b']' => {
                i += 1;
                let Some(Frame::Arr { mut out, .. }) = stack.pop() else {
                    return None;
                };
                out.push(']');
                emit(&mut stack, &mut done, out)?;
            }
            b'"' => {
                let end = string_end(b, i)?;
                let s = string_of(text.get(i..end)?)?;
                i = end;
                if let Some(Frame::Obj(obj)) = stack.last_mut() {
                    if obj.key.is_none() {
                        obj.key = Some(s);
                        continue;
                    }
                }
                emit(&mut stack, &mut done, quoted(s)?)?;
            }
            _ => {
                let end = scalar_end(b, i);
                let literal = text.get(i..end)?;
                i = end;
                let out = match literal {
                    "true" | "false" | "null" => literal.to_owned(),
                    number => wtf16::stringify(&JValue::Num(number.parse::<f64>().ok()?))?,
                };
                emit(&mut stack, &mut done, out)?;
            }
        }
    }
    if !stack.is_empty() {
        return None;
    }
    done
}

/// `JSON.stringify(JSON.parse(text))`, for a [`valid`] text.
pub(crate) fn canonical(text: &str) -> Option<String> {
    match read(text)? {
        Top::Object(entries) => join(&entries),
        Top::Other(out) => Some(out),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn what_node_writes_back() {
        for (input, expected) in [
            (
                " { \"b\" : 1.50 , \"2\":0,\"1\":[ 1e400 ,-0, 1E2],\"b\":true } ",
                r#"{"1":[null,0,100],"2":0,"b":true}"#,
            ),
            (r#""A\ud800""#, r#""A\ud800""#),
            ("[]", "[]"),
            ("{}", "{}"),
            (
                r#"{"a":{"z":null,"0":"x"},"4294967295":1,"10":2,"9":3}"#,
                r#"{"9":3,"10":2,"a":{"0":"x","z":null},"4294967295":1}"#,
            ),
            ("12345678901234567890", "12345678901234567000"),
        ] {
            assert_eq!(canonical(input).as_deref(), Some(expected), "{input}");
            // Where the parsed tree can hold the value, both give Node's bytes.
            if !input.contains("e400") {
                let tree = wtf16::stringify(&wtf16::parse(input).unwrap());
                assert_eq!(tree.as_deref(), Some(expected), "{input}");
            }
        }
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\" 1}",
            "01",
            "\"\u{1}\"",
            "{} x",
            "\u{feff}{}",
        ] {
            assert!(read(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn no_nesting_limit() {
        let depth = 100_000;
        let text = format!("{{\"a\":{}1{}}}", "[".repeat(depth), "]".repeat(depth));
        assert!(valid(&text));
        assert_eq!(canonical(&text).unwrap(), text);
    }
}
