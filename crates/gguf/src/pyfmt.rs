//! Python `str()`/`repr()` formatting for the few places gguf_meta.py turns a value into text
//! (the arch key prefix, an unknown quant number, the magic in an error message).

use crate::raw::Value;

/// `repr(float)`: shortest round-trip digits, scientific when the exponent is < -4 or >= 16.
pub fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sci = format!("{:e}", f.abs());
    let (mant, exp) = match sci.split_once('e') {
        Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
        None => (sci.as_str(), 0),
    };
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let sign = if f.is_sign_negative() { "-" } else { "" };
    let ndig = digits.len() as i32;
    if !(-4..16).contains(&exp) {
        let (head, tail) = digits.split_at(1.min(digits.len()));
        let mant = if tail.is_empty() {
            head.to_string()
        } else {
            format!("{head}.{tail}")
        };
        let esign = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{mant}e{esign}{:02}", exp.unsigned_abs());
    }
    // Fixed notation: the decimal point sits after digit number exp + 1.
    let point = exp + 1;
    let body = if point <= 0 {
        format!("0.{}{}", "0".repeat(point.unsigned_abs() as usize), digits)
    } else if point >= ndig {
        format!("{}{}.0", digits, "0".repeat((point - ndig) as usize))
    } else {
        let (int_part, frac) = digits.split_at(point as usize);
        format!("{int_part}.{frac}")
    };
    format!("{sign}{body}")
}

fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            // ASCII and Latin-1 control / non-printing characters. Python also escapes other
            // non-printable code points (format chars, unassigned); those are kept as-is here.
            c if (c as u32) < 0x20 || (0x7f..=0xa0).contains(&(c as u32)) || c == '\u{ad}' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `repr(bytes)`.
pub fn py_bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') {
        b'"'
    } else {
        b'\''
    };
    let mut out = String::from("b");
    out.push(char::from(quote));
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(char::from(c));
            }
            0x20..=0x7e => out.push(char::from(c)),
            c => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out.push(char::from(quote));
    out
}

/// `repr(value)` of a parsed metadata value.
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => py_float_repr(*f),
        Value::Str(s) => py_str_repr(s),
        Value::List(items) => {
            let parts: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::ArraySummary { count, sample } => {
            let parts: Vec<String> = sample.iter().map(py_repr).collect();
            format!(
                "{{'_array': True, 'count': {count}, 'sample': [{}]}}",
                parts.join(", ")
            )
        }
    }
}

/// `str(value)`.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => py_repr(other),
    }
}

/// Python truthiness.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::Str(s) => !s.is_empty(),
        Value::List(l) => !l.is_empty(),
        Value::ArraySummary { .. } => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (1.0, "1.0"),
            (-0.0, "-0.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (123.456, "123.456"),
            (1e300, "1e+300"),
            (f64::from(0.1f32), "0.10000000149011612"),
            (1.8446744073709552e19, "1.8446744073709552e+19"),
            (5e-324, "5e-324"),
        ];
        for (f, want) in cases {
            assert_eq!(py_float_repr(f), want, "{f}");
        }
    }

    #[test]
    fn bytes_repr_matches_python() {
        assert_eq!(py_bytes_repr(b""), "b''");
        assert_eq!(py_bytes_repr(b"NOPE"), "b'NOPE'");
        assert_eq!(py_bytes_repr(b"G\x00'\\"), "b\"G\\x00'\\\\\"");
        assert_eq!(py_bytes_repr(b"'\""), "b'\\'\"'");
    }
}
