//! UTF16 JSON adapter. Parse recursion is capped at 64 frames; output uses an explicit stack.
//! Duplicate lookup uses bounded probing, with a whole-parse work budget.
use crate::{Control, Error, OUTPUT_BYTES, PLAIN_BYTES};
use zeroize::{Zeroize, Zeroizing};
const DEPTH: usize = 64;
const NODES: usize = 8192;
const UNITS: usize = 65536;
const WORK: usize = 4 * 1024 * 1024;
#[derive(Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(Vec<u16>),
    Array(Vec<Value>),
    Object(Vec<(Vec<u16>, Value)>),
}
impl Drop for Value {
    fn drop(&mut self) {
        match self {
            Self::String(s) => s.zeroize(),
            Self::Object(o) => {
                for (k, _) in o {
                    k.zeroize()
                }
            }
            Self::Number(n) => n.zeroize(),
            Self::Bool(b) => b.zeroize(),
            _ => {}
        }
    }
}
impl Value {
    pub(crate) fn truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(b) => *b,
            Self::Number(n) => *n != 0.0 && !n.is_nan(),
            Self::String(s) => !s.is_empty(),
            Self::Array(_) | Self::Object(_) => true,
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Failure {
    Syntax,
    Limit,
    Cancelled,
}
struct Parser<'a> {
    s: &'a str,
    p: usize,
    nodes: usize,
    units: usize,
    work: usize,
    control: &'a Control,
}
impl Parser<'_> {
    fn charge(&mut self, n: usize) -> Result<(), Failure> {
        self.control.check().map_err(|_| Failure::Cancelled)?;
        self.work = self.work.checked_add(n).ok_or(Failure::Limit)?;
        if self.work > WORK {
            Err(Failure::Limit)
        } else {
            Ok(())
        }
    }
    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.p).copied()
    }
    fn white(&mut self) {
        while self
            .peek()
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.p += 1
        }
    }
    fn take(&mut self, b: u8) -> Result<(), Failure> {
        if self.peek() == Some(b) {
            self.p += 1;
            Ok(())
        } else {
            Err(Failure::Syntax)
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value, Failure> {
        self.control.check().map_err(|_| Failure::Cancelled)?;
        if depth > DEPTH {
            return Err(Failure::Limit);
        }
        self.nodes += 1;
        if self.nodes > NODES {
            return Err(Failure::Limit);
        }
        self.white();
        match self.peek() {
            Some(b'"') => self.string().map(Value::String),
            Some(b'n') => {
                self.literal("null")?;
                Ok(Value::Null)
            }
            Some(b't') => {
                self.literal("true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Value::Bool(false))
            }
            Some(b'[') => {
                self.p += 1;
                let mut a = Vec::new();
                self.white();
                if self.peek() == Some(b']') {
                    self.p += 1;
                    return Ok(Value::Array(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    self.white();
                    match self.peek() {
                        Some(b',') => self.p += 1,
                        Some(b']') => {
                            self.p += 1;
                            break;
                        }
                        _ => return Err(Failure::Syntax),
                    }
                }
                Ok(Value::Array(a))
            }
            Some(b'{') => {
                self.p += 1;
                let mut object = Value::Object(Vec::new());
                let Value::Object(a) = &mut object else {
                    return Err(Failure::Syntax);
                };
                let mut slots = vec![None; 16];
                self.white();
                if self.peek() == Some(b'}') {
                    self.p += 1;
                    return Ok(object);
                }
                loop {
                    self.white();
                    let mut k = Zeroizing::new(self.string()?);
                    self.white();
                    self.take(b':')?;
                    let v = self.value(depth + 1)?;
                    if (a.len() + 1) * 2 > slots.len() {
                        let len = slots.len() * 2;
                        slots = vec![None; len];
                        for (i, (key, _)) in a.iter().enumerate() {
                            let pos = self.slot(&slots, a, key)?;
                            if let Some(s) = slots.get_mut(pos) {
                                *s = Some(i)
                            } else {
                                return Err(Failure::Limit);
                            }
                        }
                    }
                    let pos = self.slot(&slots, a, &k)?;
                    if let Some(Some(index)) = slots.get(pos) {
                        let entry = a.get_mut(*index).ok_or(Failure::Limit)?;
                        entry.1 = v;
                    } else {
                        let index = a.len();
                        a.push((std::mem::take(&mut *k), v));
                        *slots.get_mut(pos).ok_or(Failure::Limit)? = Some(index)
                    }
                    self.white();
                    match self.peek() {
                        Some(b',') => self.p += 1,
                        Some(b'}') => {
                            self.p += 1;
                            break;
                        }
                        _ => return Err(Failure::Syntax),
                    }
                }
                Ok(object)
            }
            Some(b'-' | b'0'..=b'9') => self.number().map(Value::Number),
            _ => Err(Failure::Syntax),
        }
    }
    fn slot(
        &mut self,
        slots: &[Option<usize>],
        a: &[(Vec<u16>, Value)],
        k: &[u16],
    ) -> Result<usize, Failure> {
        self.charge(k.len())?;
        let hash = k.iter().fold(0xcbf29ce484222325u64, |h, u| {
            (h ^ u64::from(*u)).wrapping_mul(0x100000001b3)
        });
        let mut pos = (hash as usize) & (slots.len() - 1);
        for _ in 0..slots.len() {
            self.charge(1)?;
            match slots.get(pos).copied().flatten() {
                None => return Ok(pos),
                Some(i) => {
                    let key = &a.get(i).ok_or(Failure::Limit)?.0;
                    self.charge(key.len().min(k.len()))?;
                    if key == k {
                        return Ok(pos);
                    }
                }
            }
            pos = (pos + 1) & (slots.len() - 1)
        }
        Err(Failure::Limit)
    }
    fn literal(&mut self, s: &str) -> Result<(), Failure> {
        let end = self.p + s.len();
        if self.s.get(self.p..end) == Some(s) {
            self.p = end;
            Ok(())
        } else {
            Err(Failure::Syntax)
        }
    }
    fn string(&mut self) -> Result<Vec<u16>, Failure> {
        self.take(b'"')?;
        let mut out = Zeroizing::new(Vec::new());
        loop {
            self.control.check().map_err(|_| Failure::Cancelled)?;
            match self.peek() {
                None => return Err(Failure::Syntax),
                Some(b'"') => {
                    self.p += 1;
                    return Ok(std::mem::take(&mut *out));
                }
                Some(0..=31) => return Err(Failure::Syntax),
                Some(b'\\') => {
                    self.p += 1;
                    let b = self.peek().ok_or(Failure::Syntax)?;
                    self.p += 1;
                    let u = match b {
                        b'"' => 34,
                        b'\\' => 92,
                        b'/' => 47,
                        b'b' => 8,
                        b'f' => 12,
                        b'n' => 10,
                        b'r' => 13,
                        b't' => 9,
                        b'u' => {
                            let mut u = 0u16;
                            for _ in 0..4 {
                                let d = self
                                    .peek()
                                    .and_then(|c| char::from(c).to_digit(16))
                                    .ok_or(Failure::Syntax)?;
                                self.p += 1;
                                u = (u << 4) | d as u16
                            }
                            u
                        }
                        _ => return Err(Failure::Syntax),
                    };
                    self.push_unit(&mut out, u)?
                }
                Some(_) => {
                    let ch = self
                        .s
                        .get(self.p..)
                        .and_then(|s| s.chars().next())
                        .ok_or(Failure::Syntax)?;
                    self.p += ch.len_utf8();
                    let mut buf = [0; 2];
                    for u in ch.encode_utf16(&mut buf) {
                        self.push_unit(&mut out, *u)?
                    }
                }
            }
        }
    }
    fn push_unit(&mut self, out: &mut Vec<u16>, u: u16) -> Result<(), Failure> {
        self.units += 1;
        if self.units > UNITS {
            return Err(Failure::Limit);
        }
        out.push(u);
        Ok(())
    }
    fn digits(&mut self) -> usize {
        let start = self.p;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.p += 1
        }
        self.p - start
    }
    fn number(&mut self) -> Result<f64, Failure> {
        let start = self.p;
        if self.peek() == Some(b'-') {
            self.p += 1
        }
        match self.peek() {
            Some(b'0') => self.p += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(Failure::Syntax),
        }
        if self.peek() == Some(b'.') {
            self.p += 1;
            if self.digits() == 0 {
                return Err(Failure::Syntax);
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.p += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.p += 1
            }
            if self.digits() == 0 {
                return Err(Failure::Syntax);
            }
        }
        self.s
            .get(start..self.p)
            .ok_or(Failure::Syntax)?
            .parse()
            .map_err(|_| Failure::Syntax)
    }
}
pub(crate) fn parse(s: &str, control: &Control) -> Result<Value, Failure> {
    if s.len() > PLAIN_BYTES {
        return Err(Failure::Limit);
    }
    let mut p = Parser {
        s,
        p: 0,
        nodes: 0,
        units: 0,
        work: 0,
        control,
    };
    let v = p.value(0)?;
    p.white();
    if p.p != s.len() {
        return Err(Failure::Syntax);
    }
    Ok(v)
}
fn append(out: &mut String, s: &str) -> Result<(), Error> {
    if out.len() + s.len() > OUTPUT_BYTES {
        return Err(Error::Limit);
    }
    out.push_str(s);
    Ok(())
}
pub(crate) fn quote(s: &[u16], out: &mut String, control: &Control) -> Result<(), Error> {
    append(out, "\"")?;
    let mut i = 0;
    while let Some(&u) = s.get(i) {
        control.check()?;
        i += 1;
        let escaped = match u {
            34 => Some("\\\""),
            92 => Some("\\\\"),
            8 => Some("\\b"),
            12 => Some("\\f"),
            10 => Some("\\n"),
            13 => Some("\\r"),
            9 => Some("\\t"),
            _ => None,
        };
        if let Some(e) = escaped {
            append(out, e)?;
            continue;
        }
        if u < 32 {
            append(out, &format!("\\u{u:04x}"))?;
            continue;
        }
        let ch = if (0xd800..=0xdbff).contains(&u) {
            if let Some(&low) = s.get(i).filter(|x| (0xdc00..=0xdfff).contains(*x)) {
                i += 1;
                char::from_u32(
                    0x10000 + ((u32::from(u) - 0xd800) << 10) + (u32::from(low) - 0xdc00),
                )
            } else {
                None
            }
        } else {
            char::from_u32(u32::from(u))
        };
        match ch {
            Some(c) => append(out, c.encode_utf8(&mut [0; 4]))?,
            None => append(out, &format!("\\u{u:04x}"))?,
        }
    }
    append(out, "\"")
}
fn index(k: &[u16]) -> Option<u32> {
    if k.is_empty() || k.len() > 10 || (k.len() > 1 && k.first() == Some(&48)) {
        return None;
    }
    let mut n = 0u32;
    for u in k {
        let d = u.checked_sub(48).filter(|d| *d <= 9)?;
        n = n.checked_mul(10)?.checked_add(u32::from(d))?
    }
    if n == u32::MAX {
        None
    } else {
        Some(n)
    }
}
fn object_order(a: &[(Vec<u16>, Value)]) -> Result<Vec<usize>, Error> {
    let mut numbers = Vec::new();
    let mut other = Vec::new();
    for (i, (k, _)) in a.iter().enumerate() {
        match index(k) {
            Some(n) => numbers.push((n, i)),
            None => other.push(i),
        }
    }
    // Four fixed radix passes avoid comparison-sort work on persisted input.
    for shift in [0, 8, 16, 24] {
        let mut buckets: Vec<Vec<(u32, usize)>> = (0..256).map(|_| Vec::new()).collect();
        for pair in numbers {
            let bucket = ((pair.0 >> shift) & 255) as usize;
            buckets.get_mut(bucket).ok_or(Error::Limit)?.push(pair)
        }
        numbers = buckets.into_iter().flatten().collect();
    }
    let mut order: Vec<usize> = numbers.into_iter().map(|(_, i)| i).collect();
    order.extend(other);
    Ok(order)
}
enum Event<'a> {
    Value(&'a Value),
    Key(&'a [u16]),
    Token(&'static str),
}
pub(crate) fn emit(value: &Value, out: &mut String, control: &Control) -> Result<(), Error> {
    let mut stack = vec![Event::Value(value)];
    while let Some(event) = stack.pop() {
        control.check()?;
        match event {
            Event::Token(t) => append(out, t)?,
            Event::Key(k) => {
                quote(k, out, control)?;
                append(out, ":")?
            }
            Event::Value(v) => match v {
                Value::Null => append(out, "null")?,
                Value::Bool(b) => append(out, if *b { "true" } else { "false" })?,
                Value::Number(n) => {
                    let text = if n.is_finite() {
                        js_json::number_to_string(*n)
                    } else {
                        "null".into()
                    };
                    append(out, &text)?
                }
                Value::String(s) => quote(s, out, control)?,
                Value::Array(a) => {
                    append(out, "[")?;
                    stack.push(Event::Token("]"));
                    for (i, v) in a.iter().enumerate().rev() {
                        stack.push(Event::Value(v));
                        if i > 0 {
                            stack.push(Event::Token(","))
                        }
                    }
                }
                Value::Object(a) => {
                    append(out, "{")?;
                    stack.push(Event::Token("}"));
                    let ordered = object_order(a)?;
                    for (i, pos) in ordered.iter().enumerate().rev() {
                        let (k, v) = a.get(*pos).ok_or(Error::Limit)?;
                        stack.push(Event::Value(v));
                        stack.push(Event::Key(k));
                        if i > 0 {
                            stack.push(Event::Token(","))
                        }
                    }
                }
            },
        }
    }
    Ok(())
}
