//! The Python conversions the Diary applies to its configuration values and output: `int()`,
//! `float()`, `bool()`, `repr(float)`, and code-point string slicing.
//!
//! Configuration arrives as YAML (numbers, booleans, strings, null), read here as JSON values.

use serde_json::Value;

/// Why a conversion failed, named after the Python exception.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvError {
    TypeError,
    ValueError,
    OverflowError,
}

/// The whitespace `int()` and `float()` strip: ASCII ` \t\n\v\f\r` (not `\x1c`-`\x1f`) and the
/// non-ASCII Unicode spaces.
fn number_space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | ' ' | '\u{0085}' | '\u{00a0}'
        | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

/// Digits with single underscores between them (Python numeric literal rule), underscores removed.
fn digits_with_underscores(s: &str) -> Option<String> {
    if s.starts_with('_') || s.ends_with('_') || s.contains("__") {
        return None;
    }
    let out: String = s.chars().filter(|c| *c != '_').collect();
    out.bytes().all(|b| b.is_ascii_digit()).then_some(out)
}

/// Python `int(text)` (base 10). Known difference: non-ASCII decimal digits are refused.
pub fn int_str(text: &str) -> Result<i64, ConvError> {
    let t = text.trim_matches(number_space);
    let (negative, body) = match t.as_bytes().first() {
        Some(b'-') => (true, t.get(1..).unwrap_or("")),
        Some(b'+') => (false, t.get(1..).unwrap_or("")),
        _ => (false, t),
    };
    let digits = digits_with_underscores(body).filter(|d| !d.is_empty() && d.len() <= 4300);
    let digits = digits.ok_or(ConvError::ValueError)?;
    let magnitude: i128 = digits.parse().map_err(|_| ConvError::OverflowError)?;
    let value = if negative { -magnitude } else { magnitude };
    i64::try_from(value).map_err(|_| ConvError::OverflowError)
}

/// Python `float(text)`: decimal or exponent notation with underscores between digits, `inf`,
/// `infinity` and `nan` in any case with a sign. Known difference: non-ASCII digits are refused.
pub fn float_str(text: &str) -> Result<f64, ConvError> {
    let t = text.trim_matches(number_space);
    let (sign, body) = match t.as_bytes().first() {
        Some(b'-') => ("-", t.get(1..).unwrap_or("")),
        Some(b'+') => ("", t.get(1..).unwrap_or("")),
        _ => ("", t),
    };
    let lower = body.to_ascii_lowercase();
    if matches!(lower.as_str(), "inf" | "infinity" | "nan") {
        return format!("{sign}{lower}")
            .parse()
            .map_err(|_| ConvError::ValueError);
    }
    // mantissa [e exponent]; mantissa = digits [. [digits]] | . digits
    let (mantissa, exponent) = match lower.split_once('e') {
        Some((m, e)) => (m, Some(e)),
        None => (lower.as_str(), None),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let int_digits = if int_part.is_empty() {
        String::new()
    } else {
        digits_with_underscores(int_part).ok_or(ConvError::ValueError)?
    };
    let frac_digits = match frac_part {
        Some("") | None => String::new(),
        Some(f) => digits_with_underscores(f).ok_or(ConvError::ValueError)?,
    };
    if int_digits.is_empty() && frac_digits.is_empty() {
        return Err(ConvError::ValueError);
    }
    let mut literal = format!("{sign}{int_digits}.{frac_digits}");
    if literal.ends_with('.') {
        literal.push('0');
    }
    if let Some(e) = exponent {
        let (esign, edigits) = match e.as_bytes().first() {
            Some(b'-') => ("-", e.get(1..).unwrap_or("")),
            Some(b'+') => ("", e.get(1..).unwrap_or("")),
            _ => ("", e),
        };
        let edigits = digits_with_underscores(edigits)
            .filter(|d| !d.is_empty())
            .ok_or(ConvError::ValueError)?;
        literal = format!("{literal}e{esign}{edigits}");
    }
    literal.parse().map_err(|_| ConvError::ValueError)
}

/// Python `int(value)` for a configuration value.
pub fn int_value(value: &Value) -> Result<i64, ConvError> {
    match value {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else {
                float_to_int(n.as_f64().ok_or(ConvError::OverflowError)?)
            }
        }
        Value::String(s) => int_str(s),
        _ => Err(ConvError::TypeError),
    }
}

/// Python `float(value)` for a configuration value.
pub fn float_value(value: &Value) -> Result<f64, ConvError> {
    match value {
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64().ok_or(ConvError::ValueError),
        Value::String(s) => float_str(s),
        _ => Err(ConvError::TypeError),
    }
}

/// Python `int(x)` of a float: truncation; `inf` overflows and `nan` is a value error. A result
/// beyond i64 is reported as an overflow (Python would carry the big integer on).
pub fn float_to_int(x: f64) -> Result<i64, ConvError> {
    if x.is_nan() {
        return Err(ConvError::ValueError);
    }
    if x.is_infinite() {
        return Err(ConvError::OverflowError);
    }
    let t = x.trunc();
    if t < -9.223_372_036_854_776e18 || t >= 9.223_372_036_854_776e18 {
        return Err(ConvError::OverflowError);
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(t as i64)
}

/// Python `bool(value)` (truthiness) of a configuration value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `repr(x)` of a float: the shortest round-trip digits, in fixed notation for decimal
/// exponents -4..16 (always with a fractional part) and scientific notation otherwise.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    // `{:e}` gives the shortest round-trip digits: "d.ddde<exp>".
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if x < 0.0 { "-" } else { "" };
    if (-4..16).contains(&exp) {
        let body = if exp >= 0 {
            let point = usize::try_from(exp + 1).unwrap_or(0);
            if digits.len() <= point {
                format!("{digits}{}.0", "0".repeat(point - digits.len()))
            } else {
                format!(
                    "{}.{}",
                    digits.get(..point).unwrap_or(""),
                    digits.get(point..).unwrap_or("")
                )
            }
        } else {
            let zeros = usize::try_from(-exp - 1).unwrap_or(0);
            format!("0.{}{digits}", "0".repeat(zeros))
        };
        format!("{sign}{body}")
    } else {
        let (first, rest) = digits.split_at(1.min(digits.len()));
        let frac = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let esign = if exp < 0 { "-" } else { "+" };
        format!("{sign}{first}{frac}e{esign}{:02}", exp.abs())
    }
}

/// Python `s[start:end]` on code points, with negative indices counted from the end.
pub fn slice(s: &str, start: Option<i64>, end: Option<i64>) -> String {
    let n = i64::try_from(s.chars().count()).unwrap_or(i64::MAX);
    let clamp = |i: i64| if i < 0 { (n + i).max(0) } else { i.min(n) };
    let a = start.map_or(0, clamp);
    let b = end.map_or(n, clamp);
    if b <= a {
        return String::new();
    }
    let skip = usize::try_from(a).unwrap_or(0);
    let take = usize::try_from(b - a).unwrap_or(0);
    s.chars().skip(skip).take(take).collect()
}

/// The length of Python `seq[:end]` for a sequence of `n` items.
pub fn slice_len(n: usize, end: i64) -> usize {
    let n_i = i64::try_from(n).unwrap_or(i64::MAX);
    let end = if end < 0 {
        (n_i + end).max(0)
    } else {
        end.min(n_i)
    };
    usize::try_from(end).unwrap_or(0)
}

/// Python `len(s)`: code points.
pub fn len(s: &str) -> i64 {
    i64::try_from(s.chars().count()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_matches_python() {
        // Values from CPython 3.12 repr().
        for (x, want) in [
            (0.91, "0.91"),
            (1.0, "1.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (123.456, "123.456"),
            (1e16, "1e+16"),
            (1234567890123456.0, "1234567890123456.0"),
            (-0.5, "-0.5"),
            (0.7226, "0.7226"),
            (1.5e-7, "1.5e-07"),
            (2.5e300, "2.5e+300"),
        ] {
            assert_eq!(float_repr(x), want, "{x}");
        }
    }

    #[test]
    fn conversions_follow_python() {
        assert_eq!(int_str(" 8 "), Ok(8));
        assert_eq!(int_str("8.0"), Err(ConvError::ValueError));
        assert_eq!(int_str("1_000"), Ok(1000));
        assert_eq!(float_str(" 0.3\n"), Ok(0.3));
        assert_eq!(float_str("1_0.5"), Ok(10.5));
        assert_eq!(float_str(".5"), Ok(0.5));
        assert_eq!(float_str("5."), Ok(5.0));
        assert_eq!(float_str("1e3"), Ok(1000.0));
        assert!(float_str("NaN").is_ok_and(f64::is_nan));
        assert_eq!(float_str("-Infinity"), Ok(f64::NEG_INFINITY));
        assert_eq!(float_str("1__0"), Err(ConvError::ValueError));
        assert_eq!(float_str("e5"), Err(ConvError::ValueError));
        assert_eq!(float_str("."), Err(ConvError::ValueError));
        assert_eq!(float_str("\u{1c}1"), Err(ConvError::ValueError));
        assert_eq!(float_to_int(-2.7), Ok(-2));
        assert_eq!(slice("abcdef", Some(-2), None), "ef");
        assert_eq!(slice("abcdef", Some(0), Some(-10)), "");
        assert_eq!(slice("abcdef", Some(-0), None), "abcdef");
    }
}
