//! The JSON a FastAPI `-> dict` route answers: pydantic's JSON mode (a non-finite float becomes
//! `null`), then `json.dumps(ensure_ascii=False, separators=(",", ":"))`, byte for byte.
use model_files::json::Value;

/// `value` written as the route writes it. A repeated object key keeps its first position and
/// its last value, as a Python dict built from the pairs would.
pub(super) fn write(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) if f.is_finite() => out.push_str(&gguf::py_float_repr(*f)),
        Value::Float(_) => out.push_str("null"),
        Value::Str(s) => string(out, s),
        Value::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(out, item);
            }
            out.push(']');
        }
        Value::Obj(pairs) => {
            out.push('{');
            let mut first = true;
            for (i, (key, _)) in pairs.iter().enumerate() {
                if pairs.iter().take(i).any(|(k, _)| k == key) {
                    continue;
                }
                let Some((_, last)) = pairs.iter().rev().find(|(k, _)| k == key) else {
                    continue;
                };
                if !first {
                    out.push(',');
                }
                first = false;
                string(out, key);
                out.push(':');
                write(out, last);
            }
            out.push('}');
        }
    }
}

/// A string as `json.dumps(ensure_ascii=False)` writes it.
pub(super) fn string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if u32::from(c) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
