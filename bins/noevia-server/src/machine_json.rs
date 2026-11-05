//! Machine-local JSON codec.
//!
//! * Literal Unicode and lone UTF-16 surrogates have disjoint representations: the escape
//!   character escapes itself, and introduces a surrogate unit.
//! * `JSON.parse` accepts any nesting depth and any finite or overflowing number, so this parser
//!   validates the whole grammar in one linear, iterative pass (explicit stacks, no recursion and
//!   no re-reading of nested text).
//! * Only the first [`BUILD_DEPTH`] container levels become a `serde_json::Value`: every consumer
//!   reads shallow fields, and the tree's recursive `Drop`, `Clone` and walks must stay bounded.
//!   A subtree below that is collapsed to a flat stand-in holding the only things Node does with
//!   such a value: its `String(value)` text (or that coercion's TypeError) and, for arrays, the
//!   nesting height that decides V8's recursion limit. Deep ignored fields therefore cost
//!   validation time only.
use serde_json::{Map, Value};
const ESC: char = '\u{e000}';
/// Container levels built as real `Value` nodes; deeper subtrees collapse to a stand-in.
const BUILD_DEPTH: usize = 32;
/// Nested arrays `String()` follows before V8 throws RangeError (measured on Node 22.23.3: 2622
/// pass and 2623 fail in a bare process; a request handler has a few frames less).
pub(super) const NODE_ARRAY_NESTING: usize = 2600;
// Keys start with the escape character, which an external key can never do (string() doubles it).
const STAND_ARRAY: &str = "\u{e000}C";
const STAND_HEIGHT: &str = "\u{e000}H";
const STAND_THROWS: &str = "\u{e000}E";
const STAND_OBJECT: &str = "\u{e000}O";

pub(super) fn len(s: &str) -> usize {
    let mut chars = s.chars();
    let mut n = 0;
    while let Some(c) = chars.next() {
        n += if c == ESC {
            chars.next();
            1
        } else {
            c.len_utf16()
        };
    }
    n
}

/// A collapsed subtree (see the module comment).
pub(super) enum StandIn<'a> {
    /// `String(array)`: its text, its array-nesting height (the array itself counts 1) and
    /// whether an object with an own `toString` made the coercion throw.
    Array {
        text: &'a str,
        height: usize,
        throws: bool,
    },
    /// An object: `String(object)` throws exactly when it has an own `toString`.
    Object { throws: bool },
}
pub(super) fn stand_in(value: &Value) -> Option<StandIn<'_>> {
    let object = value.as_object()?;
    if let Some(text) = object.get(STAND_ARRAY) {
        return Some(StandIn::Array {
            text: text.as_str()?,
            height: usize::try_from(object.get(STAND_HEIGHT)?.as_u64()?).ok()?,
            throws: object.get(STAND_THROWS)?.as_bool()?,
        });
    }
    Some(StandIn::Object {
        throws: object.get(STAND_OBJECT)?.as_bool()?,
    })
}

enum Scalar {
    Null,
    Bool(bool),
    Num(f64),
}
impl Scalar {
    fn value(&self) -> Option<Value> {
        Some(match self {
            Scalar::Null => Value::Null,
            Scalar::Bool(b) => Value::Bool(*b),
            // The common case, without a text round trip: serde reads such text as an integer.
            Scalar::Num(n) if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 => {
                Value::from(*n as i64)
            }
            Scalar::Num(n) => match serde_json::Number::from_f64(*n) {
                Some(_) => serde_json::from_str(&js_json::number_to_string(*n)).ok()?,
                // This key cannot come from an external object: every literal escape
                // character in a key is doubled by string().
                None => serde_json::json!({"\u{e000}N": n.is_sign_negative()}),
            },
        })
    }
    /// The text `String(value)` gives for a scalar array element (nullish is empty).
    fn text(&self) -> String {
        match self {
            Scalar::Null => String::new(),
            Scalar::Bool(b) => b.to_string(),
            Scalar::Num(n) if n.is_infinite() => {
                let word = if *n < 0.0 { "-Infinity" } else { "Infinity" };
                word.into()
            }
            Scalar::Num(n) => js_json::number_to_string(*n),
        }
    }
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            at: 0,
        }
    }
    fn ws(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }
    fn eat(&mut self, byte: u8) -> bool {
        let hit = self.peek() == Some(byte);
        self.at += usize::from(hit);
        hit
    }
    fn hex4(&self, at: usize) -> Option<u16> {
        let digits = self.bytes.get(at..at + 4)?;
        if !digits.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        u16::from_str_radix(self.text.get(at..at + 4)?, 16).ok()
    }
    /// Reads a string (at its opening quote). `out` receives the decoded text, in this codec's
    /// representation, when wanted; the string is validated either way.
    fn string(&mut self, mut out: Option<&mut String>) -> Option<()> {
        self.at += 1;
        loop {
            let start = self.at;
            while self
                .peek()
                .is_some_and(|b| b != b'"' && b != b'\\' && b >= 0x20)
            {
                self.at += 1;
            }
            if let Some(out) = out.as_deref_mut() {
                let run = self.text.get(start..self.at)?;
                if run.contains(ESC) {
                    out.push_str(&run.replace(ESC, "\u{e000}\u{e000}"));
                } else {
                    out.push_str(run);
                }
            }
            let escape = self.peek()?;
            self.at += 1;
            match escape {
                b'"' => return Some(()),
                b'\\' => (),
                _ => return None,
            }
            let kind = self.peek()?;
            self.at += 1;
            let plain = match kind {
                b'"' => '"',
                b'\\' => '\\',
                b'/' => '/',
                b'b' => '\u{8}',
                b'f' => '\u{c}',
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'u' => {
                    let unit = self.hex4(self.at)?;
                    self.at += 4;
                    let low = (0xd800..=0xdbff).contains(&unit)
                        && self.bytes.get(self.at..self.at + 2) == Some(&b"\\u"[..]);
                    let low = low
                        .then(|| self.hex4(self.at + 2))
                        .flatten()
                        .filter(|u| (0xdc00..=0xdfff).contains(u));
                    if let Some(low) = low {
                        self.at += 6;
                        let pair = 0x10000
                            + ((u32::from(unit) - 0xd800) << 10)
                            + (u32::from(low) - 0xdc00);
                        char::from_u32(pair)?
                    } else if (0xd800..=0xdfff).contains(&unit) {
                        if let Some(out) = out.as_deref_mut() {
                            out.push(ESC);
                        }
                        js_json::wtf16::lone_char(unit)?
                    } else if unit == ESC as u16 {
                        if let Some(out) = out.as_deref_mut() {
                            out.push(ESC);
                        }
                        ESC
                    } else {
                        char::from_u32(u32::from(unit))?
                    }
                }
                _ => return None,
            };
            if let Some(out) = out.as_deref_mut() {
                out.push(plain);
            }
        }
    }
    fn decoded(&mut self) -> Option<String> {
        let mut out = String::new();
        self.string(Some(&mut out))?;
        Some(out)
    }
    /// An object key and its colon.
    fn key(&mut self) -> Option<String> {
        if self.peek()? != b'"' {
            return None;
        }
        let key = self.decoded()?;
        self.ws();
        self.eat(b':').then_some(key)
    }
    fn scalar(&mut self) -> Option<Scalar> {
        let rest = self.bytes.get(self.at..)?;
        for (word, scalar) in [
            (&b"null"[..], Scalar::Null),
            (&b"true"[..], Scalar::Bool(true)),
            (&b"false"[..], Scalar::Bool(false)),
        ] {
            if rest.starts_with(word) {
                self.at += word.len();
                return Some(scalar);
            }
        }
        let start = self.at;
        self.eat(b'-');
        match self.peek()? {
            b'0' => self.at += 1,
            b'1'..=b'9' => self.digits(),
            _ => return None,
        }
        if self.eat(b'.') {
            self.digits1()?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            self.digits1()?;
        }
        self.text
            .get(start..self.at)?
            .parse::<f64>()
            .ok()
            .map(Scalar::Num)
    }
    fn digits(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.at += 1;
        }
    }
    fn digits1(&mut self) -> Option<()> {
        let start = self.at;
        self.digits();
        (self.at > start).then_some(())
    }
    fn finished(&mut self) -> bool {
        self.ws();
        self.at == self.bytes.len()
    }
}
enum Frame {
    Array(Vec<Value>),
    Object(Map<String, Value>, String),
}

pub(super) fn parse(raw: &str) -> Option<Value> {
    let mut p = Parser::new(raw);
    let mut stack: Vec<Frame> = Vec::new();
    loop {
        p.ws();
        let mut done = match p.peek()? {
            b'[' | b'{' => {
                let array = p.peek() == Some(b'[');
                p.at += 1;
                if stack.len() >= BUILD_DEPTH {
                    collapse(&mut p, array)?
                } else {
                    p.ws();
                    if p.eat(if array { b']' } else { b'}' }) {
                        if array {
                            Value::Array(Vec::new())
                        } else {
                            Value::Object(Map::new())
                        }
                    } else if array {
                        stack.push(Frame::Array(Vec::new()));
                        continue;
                    } else {
                        let key = p.key()?;
                        stack.push(Frame::Object(Map::new(), key));
                        continue;
                    }
                }
            }
            b'"' => Value::String(p.decoded()?),
            _ => p.scalar()?.value()?,
        };
        loop {
            match stack.last_mut() {
                None => return p.finished().then_some(done),
                Some(Frame::Array(rows)) => {
                    rows.push(done);
                    p.ws();
                    match p.peek()? {
                        b',' => {
                            p.at += 1;
                            break;
                        }
                        b']' => {
                            p.at += 1;
                            done = Value::Array(std::mem::take(rows));
                        }
                        _ => return None,
                    }
                }
                Some(Frame::Object(map, key)) => {
                    map.insert(std::mem::take(key), done);
                    p.ws();
                    match p.peek()? {
                        b',' => {
                            p.at += 1;
                            p.ws();
                            *key = p.key()?;
                            break;
                        }
                        b'}' => {
                            p.at += 1;
                            done = Value::Object(std::mem::take(map));
                        }
                        _ => return None,
                    }
                }
            }
            stack.pop();
        }
    }
}

struct Open {
    array: bool,
    items: usize,
}
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Just past an opening bracket: the container may close right here.
    Fresh,
    /// An element (an object's key first) must follow.
    Element,
    /// A value just ended: a comma or the closing bracket follows.
    After,
}
/// Reads the rest of a container opened below [`BUILD_DEPTH`] (the parser is just past its
/// bracket) and returns its stand-in. Array elements are written to `text` in the order and with
/// the separators `Array.prototype.join` would use. `String()` never looks inside an object, so
/// only arrays and objects directly in the root array's chain count (`objects == 0`), and only
/// the keys of an object directly in that chain can make the coercion throw.
fn collapse(p: &mut Parser<'_>, array: bool) -> Option<Value> {
    let mut stack = vec![Open { array, items: 0 }];
    let mut objects = usize::from(!array);
    let mut text = String::new();
    let (mut height, mut throws) = (1_usize, false);
    let mut mode = Mode::Fresh;
    loop {
        p.ws();
        let in_array = stack.last()?.array;
        let closer = if in_array { b']' } else { b'}' };
        let close = match mode {
            Mode::Fresh if p.peek() == Some(closer) => true,
            Mode::Fresh | Mode::Element => {
                if !in_array {
                    throws |= p.key()? == "toString" && objects == 1;
                    p.ws();
                }
                let live = objects == 0;
                if live {
                    let top = stack.last_mut()?;
                    if top.items > 0 {
                        text.push(',');
                    }
                    top.items += 1;
                }
                match p.peek()? {
                    b'[' => {
                        p.at += 1;
                        stack.push(Open {
                            array: true,
                            items: 0,
                        });
                        if objects == 0 {
                            height = height.max(stack.len());
                        }
                        mode = Mode::Fresh;
                    }
                    b'{' => {
                        p.at += 1;
                        if live {
                            text.push_str("[object Object]");
                        }
                        stack.push(Open {
                            array: false,
                            items: 0,
                        });
                        objects += 1;
                        mode = Mode::Fresh;
                    }
                    b'"' => {
                        p.string(live.then_some(&mut text))?;
                        mode = Mode::After;
                    }
                    _ => {
                        let scalar = p.scalar()?;
                        if live {
                            text.push_str(&scalar.text());
                        }
                        mode = Mode::After;
                    }
                }
                false
            }
            Mode::After => match p.peek()? {
                b',' => {
                    p.at += 1;
                    mode = Mode::Element;
                    false
                }
                b if b == closer => true,
                _ => return None,
            },
        };
        if close {
            p.at += 1;
            if !stack.pop()?.array {
                objects -= 1;
            }
            if stack.is_empty() {
                break;
            }
            mode = Mode::After;
        }
    }
    Some(if array {
        serde_json::json!({STAND_ARRAY: text, STAND_HEIGHT: height as u64, STAND_THROWS: throws})
    } else {
        serde_json::json!({STAND_OBJECT: throws})
    })
}

pub(super) fn infinity(value: &Value) -> Option<f64> {
    value
        .as_object()?
        .get("\u{e000}N")?
        .as_bool()
        .map(|negative| {
            if negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        })
}
fn js_value(value: &Value) -> js_json::JValue {
    use js_json::JValue as J;
    if let Some(n) = infinity(value) {
        return J::Num(n);
    }
    if stand_in(value).is_some() {
        // Never part of a saved or sent document: only request/answer fields below the build
        // depth become stand-ins, and no handler copies those into stored state.
        return J::Null;
    }
    match value {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Number(n) => J::Num(n.as_f64().unwrap_or_default()),
        Value::String(s) => J::Str(s.clone()),
        Value::Array(a) => J::Arr(a.iter().map(js_value).collect()),
        Value::Object(o) => J::Obj(o.iter().map(|(k, v)| (k.clone(), js_value(v))).collect()),
    }
}
pub(super) fn encode(value: &Value) -> String {
    // Write JavaScript numbers and encoded strings, then decode string escapes at
    // the JSON token boundary. The escape character cannot occur in other tokens.
    let text = js_json::stringify(&js_value(value)).unwrap_or_default();
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == ESC {
            match chars.next() {
                Some(ESC) => out.push(ESC),
                Some(unit) => {
                    if let Some(unit) = js_json::wtf16::lone_unit(unit) {
                        out.push_str(&format!("\\u{unit:04x}"));
                    }
                }
                None => out.push(ESC),
            }
        } else {
            out.push(c);
        }
    }
    out
}
