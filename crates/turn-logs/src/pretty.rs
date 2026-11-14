//! `JSON.stringify(value, null, indent)` for any indent (core's two CLIs print with 1 and 2).
use js_json::{wtf16, JValue};

/// `JSON.stringify(value, null, indent)`; `None` for `undefined`, as JS answers `undefined`.
pub fn stringify(v: &JValue, indent: usize) -> Option<String> {
    let mut out = String::new();
    emit(v, indent, 0, &mut out).then_some(out)
}

/// Writes `v`; false when `v` has no JSON text (`undefined`).
fn emit(v: &JValue, indent: usize, depth: usize, out: &mut String) -> bool {
    let pad = |out: &mut String, d: usize| {
        out.push('\n');
        out.push_str(&" ".repeat(indent * d));
    };
    match v {
        JValue::Undefined => false,
        JValue::Arr(items) if !items.is_empty() => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                pad(out, depth + 1);
                if !emit(item, indent, depth + 1, out) {
                    out.push_str("null");
                }
            }
            pad(out, depth);
            out.push(']');
            true
        }
        JValue::Obj(items) if items.iter().any(|(_, v)| !matches!(v, JValue::Undefined)) => {
            out.push('{');
            let mut first = true;
            for (k, val) in items {
                if matches!(val, JValue::Undefined) {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                pad(out, depth + 1);
                out.push_str(&wtf16::stringify(&JValue::Str(k.clone())).unwrap_or_default());
                out.push_str(": ");
                emit(val, indent, depth + 1, out);
            }
            pad(out, depth);
            out.push('}');
            true
        }
        leaf => match wtf16::stringify(leaf) {
            Some(text) => {
                out.push_str(&text);
                true
            }
            None => false,
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn matches_json_stringify_with_indent() {
        let v = wtf16::parse(r#"{"a":[1,{"b":{}},[]],"c":{"d":null},"e":"x"}"#).unwrap();
        // JSON.stringify(v, null, 1)
        assert_eq!(
            stringify(&v, 1).unwrap(),
            "{\n \"a\": [\n  1,\n  {\n   \"b\": {}\n  },\n  []\n ],\n \"c\": {\n  \"d\": null\n },\n \"e\": \"x\"\n}"
        );
        assert_eq!(stringify(&v, 2).unwrap(), wtf16::stringify_pretty(&v).unwrap());
        assert_eq!(stringify(&JValue::Undefined, 2), None);
        let holes = JValue::Arr(vec![JValue::Undefined]);
        assert_eq!(stringify(&holes, 1).unwrap(), "[\n null\n]");
        let skipped = JValue::obj([("u", JValue::Undefined)]);
        assert_eq!(stringify(&skipped, 1).unwrap(), "{}");
    }
}
