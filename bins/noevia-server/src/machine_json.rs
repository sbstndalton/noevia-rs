//! Machine-local JSON codec. Literal Unicode and lone UTF-16 surrogates have disjoint
//! representations: the escape character escapes itself, and introduces a surrogate unit.
//! RawValue validates the entire JSON grammar (including ignored/deep fields) without
//! serde's Value recursion bound. Conversion uses an explicit work stack.
use serde_json::{value::RawValue, Map, Value};
const ESC: char = '\u{e000}';

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

fn string(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(c) = rest.chars().next() {
        rest = rest.get(c.len_utf8()..)?;
        if c == ESC {
            out.push(ESC);
        }
        if c == '\\' && rest.starts_with('u') {
            let unit = u16::from_str_radix(rest.get(1..5)?, 16).ok()?;
            if (0xd800..=0xdfff).contains(&unit) {
                let low = rest
                    .get(5..7)
                    .filter(|s| *s == "\\u")
                    .and_then(|_| u16::from_str_radix(rest.get(7..11)?, 16).ok());
                if (0xd800..=0xdbff).contains(&unit)
                    && low.is_some_and(|u| (0xdc00..=0xdfff).contains(&u))
                {
                    out.push_str("\\u");
                    out.push_str(rest.get(1..11)?);
                    rest = rest.get(11..)?;
                } else {
                    out.push(ESC);
                    out.push(js_json::wtf16::lone_char(unit)?);
                    rest = rest.get(5..)?;
                }
                continue;
            }
            if unit == ESC as u16 {
                out.push(ESC);
                out.push(ESC);
                rest = rest.get(5..)?;
                continue;
            }
        }
        out.push(c);
        if c == '\\' {
            let next = rest.chars().next()?;
            out.push(next);
            rest = rest.get(next.len_utf8()..)?;
        }
    }
    serde_json::from_str(&out).ok()
}

enum Task {
    Read(Box<RawValue>),
    Array(usize),
    Object(Vec<String>),
}
pub(super) fn parse(raw: &str) -> Option<Value> {
    let root = serde_json::from_str::<Box<RawValue>>(raw).ok()?;
    let mut tasks = vec![Task::Read(root)];
    let mut values = Vec::new();
    while let Some(task) = tasks.pop() {
        match task {
            Task::Read(raw) => {
                let text = raw.get();
                match text.as_bytes().first()? {
                    b'[' => {
                        let rows: Vec<Box<RawValue>> = serde_json::from_str(text).ok()?;
                        tasks.push(Task::Array(rows.len()));
                        tasks.extend(rows.into_iter().rev().map(Task::Read));
                    }
                    b'{' => {
                        // Decode keys through the same lossless string path as values.
                        // RawValue keys need their original escapes, so wrap each decoded
                        // key's raw spelling using a tiny object visitor.
                        let rows = object(text)?;
                        let keys = rows.iter().map(|(k, _)| k.clone()).collect();
                        tasks.push(Task::Object(keys));
                        tasks.extend(rows.into_iter().rev().map(|(_, v)| Task::Read(v)));
                    }
                    b'"' => values.push(Value::String(string(text)?)),
                    _ => {
                        let value = if text.starts_with('-')
                            || text.starts_with(|c: char| c.is_ascii_digit())
                        {
                            let number = text.parse::<f64>().ok()?;
                            match serde_json::Number::from_f64(number) {
                                Some(_) => {
                                    serde_json::from_str(&js_json::number_to_string(number)).ok()?
                                }
                                // This key cannot come from an external object: every literal
                                // escape character in a key is doubled by string().
                                None => serde_json::json!({"\u{e000}N": number.is_sign_negative()}),
                            }
                        } else {
                            serde_json::from_str(text).ok()?
                        };
                        values.push(value);
                    }
                }
            }
            Task::Array(n) => {
                let at = values.len().checked_sub(n)?;
                let rows = values.split_off(at);
                values.push(Value::Array(rows));
            }
            Task::Object(keys) => {
                let at = values.len().checked_sub(keys.len())?;
                let rows = values.split_off(at);
                let mut map = Map::new();
                for (key, value) in keys.into_iter().zip(rows) {
                    map.insert(key, value);
                }
                values.push(Value::Object(map));
            }
        }
    }
    values.pop()
}

fn object(text: &str) -> Option<Vec<(String, Box<RawValue>)>> {
    // Escape lone surrogates in keys before serde reads them. Values stay raw.
    use serde::de::{MapAccess, Visitor};
    struct Fields;
    impl<'de> Visitor<'de> for Fields {
        type Value = Vec<(String, Box<RawValue>)>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut rows = Vec::new();
            while let Some(key) = map.next_key::<String>()? {
                rows.push((key, map.next_value()?));
            }
            Ok(rows)
        }
    }
    // Transform only key strings; raw values are validated and copied untouched.
    let mut out = String::from("{");
    let mut rest = text.get(1..)?.trim_start();
    while !rest.starts_with('}') {
        let mut end = 1;
        let mut escaped = false;
        for c in rest.get(1..)?.chars() {
            end += c.len_utf8();
            if c == '"' && !escaped {
                break;
            }
            escaped = c == '\\' && !escaped;
        }
        out.push_str(&serde_json::to_string(&string(rest.get(..end)?)?).ok()?);
        rest = rest
            .get(end..)?
            .trim_start()
            .strip_prefix(':')?
            .trim_start();
        out.push(':');
        let mut de = serde_json::Deserializer::from_str(rest).into_iter::<Box<RawValue>>();
        let raw = de.next()?.ok()?;
        out.push_str(raw.get());
        rest = rest.get(de.byte_offset()..)?.trim_start();
        if let Some(tail) = rest.strip_prefix(',') {
            out.push(',');
            rest = tail.trim_start();
        } else {
            break;
        }
    }
    out.push('}');
    let mut de = serde_json::Deserializer::from_str(&out);
    serde::de::Deserializer::deserialize_map(&mut de, Fields).ok()
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
