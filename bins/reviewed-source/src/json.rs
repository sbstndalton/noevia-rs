//! The one JSON shape the digest needs: a string as Python's `json.dumps(..., ensure_ascii=True)` writes it.

/// Append `text` as a JSON string: `"` `\` and the short control escapes as Python writes them, other
/// control characters and every non-ASCII character as lowercase `\uXXXX` (UTF-16 surrogate pairs above
/// U+FFFF). DEL (0x7f) stays literal, as in Python.
pub(crate) fn push_str(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='\u{7f}' => out.push(ch),
            _ => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::push_str;

    fn enc(text: &str) -> String {
        let mut out = String::new();
        push_str(&mut out, text);
        out
    }

    #[test]
    fn matches_python_ensure_ascii() {
        // Expected values are json.dumps(x, ensure_ascii=True) from Python 3.12.
        assert_eq!(enc("src/main.rs"), r#""src/main.rs""#);
        assert_eq!(enc("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(
            enc("\n\r\t\u{8}\u{c}\u{1}\u{1f}\u{7f}"),
            "\"\\n\\r\\t\\b\\f\\u0001\\u001f\u{7f}\""
        );
        assert_eq!(enc("\u{e9}"), r#""\u00e9""#);
        assert_eq!(enc("\u{2028}"), r#""\u2028""#);
        assert_eq!(enc("\u{1f600}"), r#""\ud83d\ude00""#);
    }
}
