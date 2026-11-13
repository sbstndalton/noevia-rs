//! The Python string behaviours the embedding module depends on: `str.isspace` / `re` `\s`,
//! `str.strip`, truthiness and `str()` of a decoded JSON value, `float.__repr__`, and
//! `pathlib.PurePosixPath` normalisation.

use serde_json::Value;

/// `str.isspace` (and the `re` class `\s` for `str` patterns): the Unicode White_Space
/// characters plus `\x1c`-`\x1f`. Recorded over every code point in the fixtures.
pub fn is_space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{001c}'..='\u{001f}' | ' ' | '\u{0085}' | '\u{00a0}'
        | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

/// `str.strip()` with no argument.
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// Python truthiness of a decoded JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i != 0
            } else if let Some(u) = n.as_u64() {
                u != 0
            } else {
                n.as_f64().is_some_and(|f| f != 0.0)
            }
        }
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `str(value or "")` for an optional (missing key) decoded JSON value.
pub fn str_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(v) if truthy(v) => to_str(v),
        _ => String::new(),
    }
}

/// `str(value)`.
pub fn to_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => repr(other),
    }
}

/// `repr(value)` of a decoded JSON value (`None`, `True`, ints, floats, quoted strings, lists,
/// dicts).
pub fn repr(v: &Value) -> String {
    match v {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                n.as_f64().map_or_else(String::new, float_repr)
            }
        }
        Value::String(s) => str_repr(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", str_repr(k), repr(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// Whether `str.isprintable` is true for `c`, as far as it can be decided without the Unicode
/// category tables: ASCII exactly, C1 controls and the Zs / Zl / Zp spaces exactly. Format
/// (Cf), private-use (Co) and unassigned (Cn) characters are treated as printable, which
/// Python does not (known difference 3).
fn printable(c: char) -> bool {
    if c.is_ascii() {
        return (' '..='~').contains(&c);
    }
    !(c.is_control() || (is_space(c) && c != '\u{0085}'))
}

/// `repr(str)`.
pub fn str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            c if c == quote || c == '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if printable(c) => out.push(c),
            c => {
                let n = u32::from(c);
                if n <= 0xff {
                    out.push_str(&format!("\\x{n:02x}"));
                } else if n <= 0xffff {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
        }
    }
    out.push(quote);
    out
}

/// `repr(float)`: the shortest string that reads back, in exponent form below 1e-4 and from 1e16.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_owned();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-inf" } else { "inf" }.to_owned();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.to_owned();
    }
    let sign = if x < 0.0 { "-" } else { "" };
    // `{:e}` writes the shortest round-trip digits as `d.ddde<exp>`.
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let n = i32::try_from(digits.len()).unwrap_or(i32::MAX);
    let body = if !(-4..16).contains(&exp) {
        let mut m = String::new();
        let mut chars = digits.chars();
        if let Some(first) = chars.next() {
            m.push(first);
        }
        let rest: String = chars.collect();
        if !rest.is_empty() {
            m.push('.');
            m.push_str(&rest);
        }
        format!("{m}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    } else if exp < 0 {
        let zeros = usize::try_from(-exp - 1).unwrap_or(0);
        format!("0.{}{digits}", "0".repeat(zeros))
    } else if exp + 1 >= n {
        let zeros = usize::try_from(exp + 1 - n).unwrap_or(0);
        format!("{digits}{}.0", "0".repeat(zeros))
    } else {
        let at = usize::try_from(exp + 1).unwrap_or(0);
        let (a, b) = digits.split_at_checked(at).unwrap_or((digits.as_str(), ""));
        format!("{a}.{b}")
    };
    format!("{sign}{body}")
}

/// `str(pathlib.PurePosixPath(s))`: repeated slashes and `.` components removed, exactly two
/// leading slashes kept, three or more collapsed, an empty result `.`.
pub fn path_normalize(s: &str) -> String {
    let root = if s.starts_with("//") && !s.starts_with("///") {
        "//"
    } else if s.starts_with('/') {
        "/"
    } else {
        ""
    };
    let rest = s.trim_start_matches('/');
    let rest = if root == "//" {
        s.get(2..).unwrap_or("")
    } else {
        rest
    };
    let parts: Vec<&str> = rest
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let joined = parts.join("/");
    if joined.is_empty() && root.is_empty() {
        ".".to_owned()
    } else {
        format!("{root}{joined}")
    }
}

/// `str(PurePosixPath(s).parent)`.
pub fn path_parent(s: &str) -> String {
    let normal = path_normalize(s);
    let root = if normal.starts_with("//") {
        "//"
    } else if normal.starts_with('/') {
        "/"
    } else {
        ""
    };
    let rest = normal.get(root.len()..).unwrap_or("");
    if normal == "." || rest.is_empty() {
        return normal;
    }
    let tail = match rest.rsplit_once('/') {
        Some((head, _)) => head,
        None => "",
    };
    if tail.is_empty() && root.is_empty() {
        ".".to_owned()
    } else {
        format!("{root}{tail}")
    }
}

/// `str(PurePosixPath(base) / name)` for a plain `name`.
pub fn path_join(base: &str, name: &str) -> String {
    let b = path_normalize(base);
    if b == "." {
        path_normalize(name)
    } else if b.ends_with('/') {
        format!("{b}{name}")
    } else {
        format!("{b}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_matches_python_rules() {
        for (x, want) in [
            (1.5, "1.5"),
            (100_000.0, "100000.0"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.2345e-7, "1.2345e-07"),
            (-2.5, "-2.5"),
            (123_456_789.125, "123456789.125"),
        ] {
            assert_eq!(float_repr(x), want);
        }
    }

    #[test]
    fn paths_normalise_like_purepath() {
        assert_eq!(path_normalize("./a//b/./c/"), "a/b/c");
        assert_eq!(path_normalize("//x/y"), "//x/y");
        assert_eq!(path_normalize("///x"), "/x");
        assert_eq!(path_normalize(""), ".");
        assert_eq!(path_parent("b.db"), ".");
        assert_eq!(path_parent("/b.db"), "/");
        assert_eq!(path_parent("/"), "/");
        assert_eq!(path_parent("a/b/c.db"), "a/b");
        assert_eq!(path_join(".", "x.json"), "x.json");
        assert_eq!(path_join("/", "x.json"), "/x.json");
        assert_eq!(path_join("a/b", "x.json"), "a/b/x.json");
    }
}
