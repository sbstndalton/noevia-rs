//! The two Python built-ins the slot records depend on: `json.dumps` of a string (ASCII-only
//! escapes, default separators) and `int()` of the stored legacy width.

/// `json.dumps(s)` with `ensure_ascii=True`: the exact bytes Python writes into `vec_meta`.
pub fn dumps_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
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
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The slot record exactly as `json.dumps({"model", "profile", "dim", "table", "map"})` writes it.
pub fn slot_record(
    model: &str,
    profile: &str,
    dim: &str,
    table: &str,
    map: Option<&str>,
) -> String {
    format!(
        "{{\"model\": {}, \"profile\": {}, \"dim\": {}, \"table\": {}, \"map\": {}}}",
        dumps_str(model),
        dumps_str(profile),
        dim,
        dumps_str(table),
        map.map_or_else(|| "null".to_owned(), dumps_str)
    )
}

/// The whitespace `int()` strips: ASCII ` \t\n\v\f\r` (not `\x1c`-`\x1f`, which `str.isspace`
/// counts) and the non-ASCII Unicode spaces.
fn int_space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | ' ' | '\u{0085}' | '\u{00a0}'
        | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

/// Python `int(text)` for base 10: surrounding whitespace, an optional sign, ASCII digits with
/// single underscores between them. Returns the canonical decimal text (no leading zeros) so a
/// huge width is written back the way Python would write it.
///
/// Python 3.12 refuses more than 4300 digits (`sys.int_info.str_digits_check_threshold`), and so
/// does this.
///
/// Known difference: Python also accepts non-ASCII decimal digits (`int("١٦") == 16`); they are
/// refused here, so such a legacy width is not adopted.
pub fn py_int(text: &str) -> Option<(bool, String)> {
    let t = text.trim_matches(int_space);
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, t.get(1..)?),
        Some(b'+') => (false, t.get(1..)?),
        _ => (false, t),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    if digits.bytes().filter(u8::is_ascii_digit).count() > 4300 {
        return None;
    }
    let mut canonical = String::with_capacity(digits.len());
    for c in digits.chars() {
        match c {
            '0'..='9' => {
                if !(canonical.is_empty() && c == '0') {
                    canonical.push(c);
                }
            }
            '_' => {}
            _ => return None,
        }
    }
    if canonical.is_empty() {
        return Some((false, "0".to_owned()));
    }
    Some((negative, canonical))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_matches_python_escapes() {
        assert_eq!(dumps_str("qa-embed-c-modèl"), "\"qa-embed-c-mod\\u00e8l\"");
        assert_eq!(
            dumps_str("a\"b\\c\n\u{1}\u{7f}"),
            "\"a\\\"b\\\\c\\n\\u0001\\u007f\""
        );
        assert_eq!(dumps_str("🌿"), "\"\\ud83c\\udf3f\"");
    }

    #[test]
    fn py_int_accepts_what_python_accepts() {
        assert_eq!(py_int(" 16\n"), Some((false, "16".to_owned())));
        assert_eq!(py_int("+0_16"), Some((false, "16".to_owned())));
        assert_eq!(py_int("-3"), Some((true, "3".to_owned())));
        assert_eq!(py_int("000"), Some((false, "0".to_owned())));
        assert_eq!(py_int("16.0"), None);
        assert_eq!(py_int("\u{1c}16"), None, "int() does not strip \\x1c");
        assert_eq!(py_int("\u{3000}16\u{85}"), Some((false, "16".to_owned())));
        assert_eq!(py_int("1__6"), None);
        assert_eq!(py_int("_16"), None);
        assert_eq!(py_int(""), None);
        assert_eq!(py_int("-"), None);
    }
}
