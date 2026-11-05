//! `JSON.parse` as Node (V8, the Node 22 line production runs) does it, for the replies the MTP
//! artifact check reads: one parser that accepts exactly what V8 accepts and, on failure, gives
//! V8's own `SyntaxError` message as UTF-16 code units. Strings stay UTF-16 ([`W::Str`]), so a lone
//! surrogate escape in any field is kept as V8 keeps it, never rejected or replaced. A duplicate
//! key replaces the value and keeps the first position, as `JSON.parse` does.
//!
//! [`stringify`] is `JSON.stringify` for these values (array-index keys first, a lone surrogate as
//! `\udxxx`). Parsing, dropping and stringifying are iterative (explicit stacks), so nesting is bounded
//! only by the reply caps the callers apply (8 MiB list, 4 MiB revision), never by the call stack.
//! tests/fixtures/json-parse-errors-node.json holds Node's own messages for the grammar's error
//! points.
use std::collections::HashMap;

const WS: [u16; 4] = [0x20, 0x09, 0x0a, 0x0d];

/// A JSON value as `JSON.parse` returns it (`Undefined` only for a missing property).
pub enum W {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(Vec<u16>),
    Arr(Vec<W>),
    Obj(Vec<(Vec<u16>, W)>),
}

static UNDEFINED: W = W::Undefined;

impl Drop for W {
    fn drop(&mut self) {
        let mut stack: Vec<W> = Vec::new();
        match self {
            W::Arr(items) => stack.append(items),
            W::Obj(items) => stack.extend(items.drain(..).map(|(_, v)| v)),
            _ => return,
        }
        while let Some(mut v) = stack.pop() {
            match &mut v {
                W::Arr(items) => stack.append(items),
                W::Obj(items) => stack.extend(items.drain(..).map(|(_, v)| v)),
                _ => {}
            }
        }
    }
}

impl W {
    /// `value.key` for a property read on a JSON value (primitives and arrays have none of the
    /// keys the check reads).
    pub fn get(&self, key: &str) -> &W {
        let want: Vec<u16> = key.encode_utf16().collect();
        match self {
            W::Obj(items) => items
                .iter()
                .find(|(k, _)| *k == want)
                .map_or(&UNDEFINED, |(_, v)| v),
            _ => &UNDEFINED,
        }
    }
    /// Moves a property out (leaving `undefined`), so a large value is never copied.
    pub fn take(&mut self, key: &str) -> W {
        let want: Vec<u16> = key.encode_utf16().collect();
        match self {
            W::Obj(items) => items
                .iter_mut()
                .find(|(k, _)| *k == want)
                .map_or(W::Undefined, |(_, v)| std::mem::replace(v, W::Undefined)),
            _ => W::Undefined,
        }
    }
    pub fn is_nullish(&self) -> bool {
        matches!(self, W::Undefined | W::Null)
    }
    pub fn truthy(&self) -> bool {
        match self {
            W::Undefined | W::Null => false,
            W::Bool(b) => *b,
            W::Num(n) => *n != 0.0 && !n.is_nan(),
            W::Str(s) => !s.is_empty(),
            W::Arr(_) | W::Obj(_) => true,
        }
    }
    /// `typeof value === 'string'`: the units.
    pub fn units(&self) -> Option<&[u16]> {
        match self {
            W::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn str(s: &str) -> W {
        W::Str(s.encode_utf16().collect())
    }
}

enum Fault {
    /// Nothing left where a token was needed: "Unexpected end of JSON input".
    End,
    /// The generic token message, for the code unit at the position.
    Token(usize),
    /// "Unexpected number in JSON at position ...".
    Number(usize),
    /// A fixed message with a position.
    At(&'static str, usize),
}

enum Frame {
    Array(Vec<W>),
    Object {
        items: Vec<(Vec<u16>, W)>,
        index: HashMap<Vec<u16>, usize>,
        key: Vec<u16>,
    },
}

struct Scan<'a> {
    s: &'a [u16],
}

fn is_digit(c: Option<u16>) -> bool {
    c.is_some_and(|c| (0x30..=0x39).contains(&c))
}

fn hex_value(c: Option<u16>) -> Option<u16> {
    match c? {
        c @ 0x30..=0x39 => Some(c - 0x30),
        c @ 0x41..=0x46 => Some(c - 0x41 + 10),
        c @ 0x61..=0x66 => Some(c - 0x61 + 10),
        _ => None,
    }
}

impl Scan<'_> {
    fn at(&self, i: usize) -> Option<u16> {
        self.s.get(i).copied()
    }
    fn ws(&self, mut i: usize) -> usize {
        while self.at(i).is_some_and(|c| WS.contains(&c)) {
            i += 1;
        }
        i
    }
    /// A string at `i` (its opening quote): its units and the position after it.
    fn string(&self, i: usize) -> Result<(Vec<u16>, usize), Fault> {
        let mut out = Vec::new();
        let mut j = i + 1;
        loop {
            let Some(c) = self.at(j) else {
                return Err(Fault::At("Unterminated string", j));
            };
            match c {
                0x22 => return Ok((out, j + 1)),
                0x5c => {
                    let Some(e) = self.at(j + 1) else {
                        return Err(Fault::End);
                    };
                    let simple = match e {
                        0x22 => Some(0x22),
                        0x5c => Some(0x5c),
                        0x2f => Some(0x2f),
                        0x62 => Some(0x08),
                        0x66 => Some(0x0c),
                        0x6e => Some(0x0a),
                        0x72 => Some(0x0d),
                        0x74 => Some(0x09),
                        _ => None,
                    };
                    if let Some(u) = simple {
                        out.push(u);
                        j += 2;
                    } else if e == 0x75 {
                        let mut unit: u16 = 0;
                        for k in 0..4 {
                            let Some(h) = hex_value(self.at(j + 2 + k)) else {
                                return Err(Fault::At("Bad Unicode escape", j + 2 + k));
                            };
                            unit = unit * 16 + h;
                        }
                        out.push(unit);
                        j += 6;
                    } else {
                        return Err(Fault::At("Bad escaped character", j + 1));
                    }
                }
                c if c < 0x20 => {
                    return Err(Fault::At("Bad control character in string literal", j))
                }
                c => {
                    out.push(c);
                    j += 1;
                }
            }
        }
    }
    fn number(&self, i: usize) -> Result<(f64, usize), Fault> {
        let mut j = i;
        if self.at(j) == Some(0x2d) {
            j += 1;
            if !is_digit(self.at(j)) {
                return Err(Fault::At("No number after minus sign", j));
            }
        }
        if self.at(j) == Some(0x30) {
            j += 1;
            if is_digit(self.at(j)) {
                return Err(Fault::Number(j));
            }
        } else {
            while is_digit(self.at(j)) {
                j += 1;
            }
        }
        if self.at(j) == Some(0x2e) {
            j += 1;
            if !is_digit(self.at(j)) {
                return Err(Fault::At("Unterminated fractional number", j));
            }
            while is_digit(self.at(j)) {
                j += 1;
            }
        }
        if matches!(self.at(j), Some(0x65 | 0x45)) {
            j += 1;
            if matches!(self.at(j), Some(0x2b | 0x2d)) {
                j += 1;
            }
            if !is_digit(self.at(j)) {
                return Err(Fault::At("Exponent part is missing a number", j));
            }
            while is_digit(self.at(j)) {
                j += 1;
            }
        }
        // The slice is ASCII JSON number grammar, which Rust's parser reads with the same rounding.
        let text: String = self
            .s
            .get(i..j)
            .unwrap_or_default()
            .iter()
            .map(|&u| char::from(u as u8))
            .collect();
        Ok((text.parse::<f64>().unwrap_or(f64::NAN), j))
    }
    fn literal(&self, i: usize, word: &str) -> Result<usize, Fault> {
        for (k, w) in word.encode_utf16().enumerate() {
            match self.at(i + k) {
                None => return Err(Fault::End),
                Some(c) if c != w => return Err(Fault::Token(i + k)),
                Some(_) => {}
            }
        }
        Ok(i + word.len())
    }
    /// A property name and its colon at `j` (after whitespace): the name and where its value starts.
    fn key(&self, j: usize, missing: &'static str) -> Result<(Vec<u16>, usize), Fault> {
        if self.at(j) != Some(0x22) {
            return Err(Fault::At(missing, j));
        }
        let (name, after) = self.string(j)?;
        let k = self.ws(after);
        if self.at(k) != Some(0x3a) {
            return Err(Fault::At("Expected ':' after property name", k));
        }
        Ok((name, self.ws(k + 1)))
    }
    fn run(&self) -> Result<W, Fault> {
        let mut stack: Vec<Frame> = Vec::new();
        let mut i = self.ws(0);
        'value: loop {
            let (mut val, mut end) = match self.at(i) {
                None => return Err(Fault::End),
                Some(0x22) => {
                    let (s, e) = self.string(i)?;
                    (W::Str(s), e)
                }
                Some(c) if c == 0x2d || (0x30..=0x39).contains(&c) => {
                    let (n, e) = self.number(i)?;
                    (W::Num(n), e)
                }
                Some(0x74) => (W::Bool(true), self.literal(i, "true")?),
                Some(0x66) => (W::Bool(false), self.literal(i, "false")?),
                Some(0x6e) => (W::Null, self.literal(i, "null")?),
                Some(0x5b) => {
                    let j = self.ws(i + 1);
                    if self.at(j) == Some(0x5d) {
                        (W::Arr(Vec::new()), j + 1)
                    } else {
                        stack.push(Frame::Array(Vec::new()));
                        i = j;
                        continue 'value;
                    }
                }
                Some(0x7b) => {
                    let j = self.ws(i + 1);
                    if self.at(j) == Some(0x7d) {
                        (W::Obj(Vec::new()), j + 1)
                    } else {
                        let (key, start) = self.key(j, "Expected property name or '}'")?;
                        stack.push(Frame::Object {
                            items: Vec::new(),
                            index: HashMap::new(),
                            key,
                        });
                        i = start;
                        continue 'value;
                    }
                }
                Some(_) => return Err(Fault::Token(i)),
            };
            // After a value: attach it, then close containers or move to the next element.
            loop {
                let j = self.ws(end);
                match stack.last_mut() {
                    None => {
                        if j < self.s.len() {
                            return Err(Fault::At(
                                "Unexpected non-whitespace character after JSON",
                                j,
                            ));
                        }
                        return Ok(val);
                    }
                    Some(Frame::Array(items)) => {
                        items.push(val);
                        match self.at(j) {
                            Some(0x2c) => {
                                i = self.ws(j + 1);
                                continue 'value;
                            }
                            Some(0x5d) => {
                                let Some(Frame::Array(items)) = stack.pop() else {
                                    return Err(Fault::End);
                                };
                                val = W::Arr(items);
                                end = j + 1;
                            }
                            _ => {
                                return Err(Fault::At("Expected ',' or ']' after array element", j))
                            }
                        }
                    }
                    Some(Frame::Object { items, index, key }) => {
                        let name = std::mem::take(key);
                        match index.get(&name) {
                            Some(&at) => {
                                if let Some(slot) = items.get_mut(at) {
                                    slot.1 = val;
                                }
                            }
                            None => {
                                index.insert(name.clone(), items.len());
                                items.push((name, val));
                            }
                        }
                        match self.at(j) {
                            Some(0x2c) => {
                                let (k, start) = self
                                    .key(self.ws(j + 1), "Expected double-quoted property name")?;
                                *key = k;
                                i = start;
                                continue 'value;
                            }
                            Some(0x7d) => {
                                let Some(Frame::Object { items, .. }) = stack.pop() else {
                                    return Err(Fault::End);
                                };
                                val = W::Obj(items);
                                end = j + 1;
                            }
                            _ => {
                                return Err(Fault::At(
                                    "Expected ',' or '}' after property value",
                                    j,
                                ))
                            }
                        }
                    }
                }
            }
        }
    }
}

/// `(line N column M)` of a position: `\n`, a lone `\r`, U+2028 and U+2029 end a line.
fn line_column(s: &[u16], pos: usize) -> (usize, usize) {
    let (mut line, mut start) = (1, 0);
    for (k, &c) in s.iter().enumerate().take(pos) {
        let ends =
            c == 0x0a || (c == 0x0d && s.get(k + 1) != Some(&0x0a)) || c == 0x2028 || c == 0x2029;
        if ends {
            line += 1;
            start = k + 1;
        }
    }
    (line, pos - start + 1)
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

const CONTEXT: usize = 10;

fn message(s: &[u16], fault: Fault) -> Vec<u16> {
    let positioned = |msg: &str, pos: usize| {
        let (l, c) = line_column(s, pos);
        // "after JSON" already names it: V8's template has no second "in JSON".
        let place = if msg.ends_with("after JSON") {
            ""
        } else {
            " in JSON"
        };
        units(&format!(
            "{msg}{place} at position {pos} (line {l} column {c})"
        ))
    };
    match fault {
        Fault::End => units("Unexpected end of JSON input"),
        Fault::Number(pos) => positioned("Unexpected number", pos),
        Fault::At(msg, pos) => positioned(msg, pos),
        Fault::Token(pos) => {
            if ["NaN", "Infinity", "undefined", "[object Object]"]
                .iter()
                .any(|w| units(w) == s)
            {
                let mut out = units("\"");
                out.extend_from_slice(s);
                out.extend(units("\" is not valid JSON"));
                return out;
            }
            let mut out = units("Unexpected token '");
            out.extend(s.get(pos).copied());
            out.extend(units("', "));
            let n = s.len();
            let (from, to, before, after) = if n <= 2 * CONTEXT {
                (0, n, "", "")
            } else if pos < CONTEXT {
                (0, pos + CONTEXT, "", "...")
            } else if pos < n - CONTEXT {
                (pos - CONTEXT, pos + CONTEXT, "...", "...")
            } else {
                (pos - CONTEXT, n, "...", "")
            };
            out.extend(units(before));
            out.extend(units("\""));
            out.extend_from_slice(s.get(from..to.min(n)).unwrap_or_default());
            out.extend(units("\""));
            out.extend(units(after));
            out.extend(units(" is not valid JSON"));
            out
        }
    }
}

/// `JSON.parse(text)`: the value, or V8's `SyntaxError` message.
pub fn parse(s: &[u16]) -> Result<W, Vec<u16>> {
    Scan { s }.run().map_err(|f| message(s, f))
}

/// The `SyntaxError` message of `JSON.parse(text)`, or `None` when it parses.
pub fn parse_error(s: &[u16]) -> Option<Vec<u16>> {
    parse(s).err()
}

/// `JSON.stringify` of a string: well-formed escapes, a lone surrogate as `\udxxx`.
pub fn quote(s: &[u16], out: &mut String) {
    out.push('"');
    let mut k = 0;
    while let Some(&u) = s.get(k) {
        k += 1;
        match u {
            0x22 => out.push_str("\\\""),
            0x5c => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            u if u < 0x20 => out.push_str(&format!("\\u{u:04x}")),
            0xd800..=0xdbff => match s.get(k) {
                Some(&lo @ 0xdc00..=0xdfff) => {
                    k += 1;
                    let c = 0x10000 + ((u32::from(u) - 0xd800) << 10) + (u32::from(lo) - 0xdc00);
                    out.push(char::from_u32(c).unwrap_or('\u{fffd}'));
                }
                _ => out.push_str(&format!("\\u{u:04x}")),
            },
            0xdc00..=0xdfff => out.push_str(&format!("\\u{u:04x}")),
            u => out.push(char::from_u32(u32::from(u)).unwrap_or('\u{fffd}')),
        }
    }
    out.push('"');
}

/// An array index key (`"0"`, `"17"`, below 2^32 - 1): `JSON.stringify` lists those first, ascending.
fn index_key(k: &[u16]) -> Option<u32> {
    if k.is_empty() || k.len() > 10 || (k.len() > 1 && k.first() == Some(&0x30)) {
        return None;
    }
    let mut v: u64 = 0;
    for &u in k {
        if !(0x30..=0x39).contains(&u) {
            return None;
        }
        v = v * 10 + u64::from(u - 0x30);
    }
    u32::try_from(v).ok().filter(|&v| v != u32::MAX)
}

/// `JSON.stringify(value)` (`undefined` at the top as `null`, as the callers need).
pub fn stringify(w: &W, out: &mut String) {
    enum T<'a> {
        V(&'a W),
        S(&'static str),
        K(&'a [u16]),
    }
    let mut stack = vec![T::V(w)];
    while let Some(t) = stack.pop() {
        match t {
            T::S(s) => out.push_str(s),
            T::K(k) => {
                quote(k, out);
                out.push(':');
            }
            T::V(v) => match v {
                W::Undefined | W::Null => out.push_str("null"),
                W::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                W::Num(n) if n.is_finite() => out.push_str(&js_json::number_to_string(*n)),
                W::Num(_) => out.push_str("null"),
                W::Str(s) => quote(s, out),
                W::Arr(items) => {
                    out.push('[');
                    stack.push(T::S("]"));
                    for (n, x) in items.iter().enumerate().rev() {
                        stack.push(T::V(x));
                        if n > 0 {
                            stack.push(T::S(","));
                        }
                    }
                }
                W::Obj(items) => {
                    let mut indexed: Vec<(u32, &Vec<u16>, &W)> = Vec::new();
                    let mut named: Vec<(&Vec<u16>, &W)> = Vec::new();
                    for (k, x) in items {
                        if matches!(x, W::Undefined) {
                            continue;
                        }
                        match index_key(k) {
                            Some(n) => indexed.push((n, k, x)),
                            None => named.push((k, x)),
                        }
                    }
                    indexed.sort_by_key(|e| e.0);
                    let ordered: Vec<(&Vec<u16>, &W)> = indexed
                        .into_iter()
                        .map(|(_, k, x)| (k, x))
                        .chain(named)
                        .collect();
                    out.push('{');
                    stack.push(T::S("}"));
                    for (n, (k, x)) in ordered.into_iter().enumerate().rev() {
                        stack.push(T::V(x));
                        stack.push(T::K(k));
                        if n > 0 {
                            stack.push(T::S(","));
                        }
                    }
                }
            },
        }
    }
}

/// `String(value)` as `Array.prototype.join` and `${}` coerce it: `null`/`undefined` elements are
/// empty, nested arrays are joined, objects are `[object Object]`. `Err` when the nesting is deeper
/// than a JS engine's stack would allow (V8 throws a `RangeError` there; the depth is engine-bound).
pub fn to_js_string(w: &W) -> Result<Vec<u16>, String> {
    fn go(w: &W, depth: usize, out: &mut Vec<u16>) -> Result<(), String> {
        if depth > 4096 {
            return Err("Maximum call stack size exceeded".into());
        }
        match w {
            W::Undefined => out.extend(units("undefined")),
            W::Null => out.extend(units("null")),
            W::Bool(b) => out.extend(units(if *b { "true" } else { "false" })),
            W::Num(n) => out.extend(units(&js_json::number_to_string(*n))),
            W::Str(s) => out.extend_from_slice(s),
            W::Obj(_) => out.extend(units("[object Object]")),
            W::Arr(items) => {
                for (n, x) in items.iter().enumerate() {
                    if n > 0 {
                        out.push(0x2c);
                    }
                    if !x.is_nullish() {
                        go(x, depth + 1, out)?;
                    }
                }
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    go(w, 0, &mut out)?;
    Ok(out)
}

/// ECMAScript `ToNumber` of a string (StringToNumber): JS white space and line terminators trimmed,
/// empty is 0, `Infinity` with an optional sign, unsigned `0x`/`0o`/`0b` integers, the decimal
/// literal grammar (no `inf`/`nan`, no separators); anything else is NaN.
pub fn to_number(s: &str) -> f64 {
    let js_space = |c: char| {
        matches!(
            c,
            '\u{9}'..='\u{d}'
                | ' '
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    };
    let t = s.trim_matches(js_space);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = t.strip_prefix(prefix) {
            if digits.is_empty() {
                return f64::NAN;
            }
            let mut v = 0.0f64;
            for c in digits.chars() {
                match c.to_digit(radix) {
                    Some(d) => v = v * f64::from(radix) + f64::from(d),
                    None => return f64::NAN,
                }
            }
            return v;
        }
    }
    let b = t.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let int_start = i;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let mut digits = i - int_start;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let frac = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        digits += i - frac;
    }
    if digits == 0 {
        return f64::NAN;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == exp {
            return f64::NAN;
        }
    }
    if i != b.len() {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(w: &W) -> String {
        let mut s = String::new();
        stringify(w, &mut s);
        s
    }

    #[test]
    fn keeps_lone_surrogates_duplicates_and_index_key_order() {
        let src = units(r#"{"b":1,"7":2,"a":"\ud800x","b":3,"01":4,"0":5}"#);
        let w = parse(&src).ok().unwrap_or(W::Null);
        assert_eq!(text(&w), r#"{"0":5,"7":2,"b":3,"a":"\ud800x","01":4}"#);
    }

    #[test]
    fn deep_nesting_parses_and_drops_without_recursion() {
        let depth = 200_000;
        let mut s = "[".repeat(depth);
        s.push_str(&"]".repeat(depth));
        let w = parse(&units(&s)).ok();
        assert!(w.is_some());
        drop(w);
    }

    #[test]
    fn to_number_is_ecmascript() {
        for (s, n) in [
            ("0x2", 2.0),
            ("0b10", 2.0),
            ("0o7", 7.0),
            (" \u{a0}2\u{2028}", 2.0),
            ("", 0.0),
            ("  ", 0.0),
            ("-Infinity", f64::NEG_INFINITY),
            ("+2", 2.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("1e1", 10.0),
        ] {
            assert_eq!(to_number(s), n, "{s:?}");
        }
        for s in [
            "-0x2",
            "0x",
            "inf",
            "nan",
            "1_0",
            "2px",
            "Infinityx",
            "0b2",
            "e5",
            ".",
        ] {
            assert!(to_number(s).is_nan(), "{s:?}");
        }
    }
}
