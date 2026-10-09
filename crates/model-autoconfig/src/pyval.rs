//! The Python semantics input prep and values assembly lean on, applied to JSON values exactly
//! as `json.loads` hands them to autoconfig_core.py: `int(x)`, `float(x)`, truthiness, `==`,
//! iteration (`for v in x`, `list(x)`), `str.strip()` and `str(x)`.
//!
//! Where reproducing Python exactly would need Unicode tables or unbounded integers (a
//! non-ASCII digit string, an integer past [`INT_LIMIT`], a NaN inside a compared value), the
//! port refuses with [`Error::Unsupported`] or [`Error::OutOfRange`] instead of guessing; the
//! service treats every refusal as "cannot confirm", never as a smaller answer.

use crate::pyfloat::{f, INT_LIMIT};
use crate::{Error, Work};
use model_files::json::Value;

/// The value at `key` of a dict, or `None` for a missing key (Python's `d.get(key)`).
pub fn get<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).unwrap_or(&Value::Null)
}

/// An integer literal as an `i128` inside [`INT_LIMIT`].
pub fn big(v: &Value, what: &'static str) -> Result<i128, Error> {
    match v {
        Value::Int(i) => {
            let n: i128 = i.to_string().parse().map_err(|_| Error::OutOfRange(what))?;
            if n.abs() >= INT_LIMIT {
                return Err(Error::OutOfRange(what));
            }
            Ok(n)
        }
        _ => Err(Error::Schema(what)),
    }
}

/// `isinstance(v, int)`: a JSON integer or a bool.
pub fn is_int(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::Bool(_))
}

/// The value of something `is_int` accepted.
pub fn int_value(v: &Value, what: &'static str) -> Result<i128, Error> {
    match v {
        Value::Bool(b) => Ok(i128::from(*b)),
        other => big(other, what),
    }
}

/// Python's `str.isspace()` for one character: exactly these code points (CPython 3.12; the
/// Unicode White_Space set plus U+001C..U+001F), spelled out so no table can drift.
pub fn is_py_space(c: char) -> bool {
    matches!(c, '\u{9}'..='\u{d}' | '\u{1c}'..='\u{20}' | '\u{85}' | '\u{a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

/// Python's `str.strip()`.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Python's `int(s)` for a string: surrounding ASCII whitespace (tab, LF, VT, FF, CR, space;
/// not U+001C..U+001F, which `int()` keeps although `str.strip()` drops them), an optional sign,
/// ASCII digits with single underscores between them. Any non-ASCII character is refused:
/// Python accepts other scripts' digits and Unicode spaces there.
pub fn int_str(s: &str, what: &'static str) -> Result<i128, Error> {
    if !s.is_ascii() {
        return Err(Error::Unsupported(what));
    }
    let t = s.trim_matches(|c: char| matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' '));
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, t.get(1..).unwrap_or("")),
        Some(b'+') => (false, t.get(1..).unwrap_or("")),
        _ => (false, t),
    };
    let bytes = body.as_bytes();
    if bytes.is_empty() || bytes.first() == Some(&b'_') || bytes.last() == Some(&b'_') {
        return Err(Error::Python("ValueError"));
    }
    let mut digits = String::with_capacity(bytes.len());
    let mut prev_underscore = false;
    for &b in bytes {
        match b {
            b'0'..=b'9' => {
                digits.push(char::from(b));
                prev_underscore = false;
            }
            b'_' if !prev_underscore => prev_underscore = true,
            _ => return Err(Error::Python("ValueError")),
        }
    }
    let significant = digits.trim_start_matches('0');
    if significant.len() > 31 {
        return Err(Error::OutOfRange(what));
    }
    let n: i128 = if significant.is_empty() {
        0
    } else {
        significant.parse().map_err(|_| Error::OutOfRange(what))?
    };
    if n >= INT_LIMIT {
        return Err(Error::OutOfRange(what));
    }
    Ok(if neg { -n } else { n })
}

/// Python's `int(v)`.
pub fn int_of(v: &Value, what: &'static str) -> Result<i128, Error> {
    match v {
        Value::Null | Value::Arr(_) | Value::Obj(_) => Err(Error::Python("TypeError")),
        Value::Bool(b) => Ok(i128::from(*b)),
        Value::Int(_) => big(v, what),
        Value::Float(x) => {
            if x.is_nan() {
                return Err(Error::Python("ValueError"));
            }
            if x.is_infinite() {
                return Err(Error::Python("OverflowError"));
            }
            let t = x.trunc();
            if t.abs() >= f(INT_LIMIT) {
                return Err(Error::OutOfRange(what));
            }
            // |t| < 2^100 and integral, so the conversion is exact.
            Ok(t as i128)
        }
        Value::Str(s) => int_str(s, what),
    }
}

/// Python's `int(v or default)`.
pub fn int_or(v: &Value, default: i128, what: &'static str) -> Result<i128, Error> {
    if v.truthy() {
        int_of(v, what)
    } else {
        Ok(default)
    }
}

/// Python's `float(v)`. Strings (whose float grammar Python spells out at length) are refused.
pub fn float_of(v: &Value, what: &'static str) -> Result<f64, Error> {
    match v {
        Value::Null | Value::Arr(_) | Value::Obj(_) => Err(Error::Python("TypeError")),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        // Python rounds an int to the nearest float, ties to even, as `as` does.
        Value::Int(_) => big(v, what).map(f),
        Value::Float(x) => Ok(*x),
        Value::Str(_) => Err(Error::Unsupported(what)),
    }
}

/// Python's `float(v or 0)`.
pub fn float_or_zero(v: &Value, what: &'static str) -> Result<f64, Error> {
    if v.truthy() {
        float_of(v, what)
    } else {
        Ok(0.0)
    }
}

fn unique_keys(pairs: &[(String, Value)], work: &mut Work) -> Result<(), Error> {
    work.charge(pairs.len().saturating_mul(pairs.len()))?;
    for (i, (k, _)) in pairs.iter().enumerate() {
        if pairs.iter().skip(i + 1).any(|(o, _)| o == k) {
            // json.loads keeps the last; iterating such an object is not worth reproducing.
            return Err(Error::Unsupported("a duplicated object key"));
        }
    }
    Ok(())
}

/// Python's iteration of `v` (`for x in v`, `list(v)`): a list's items, a string's characters,
/// a dict's keys. Anything else is not iterable (TypeError).
pub fn iterate(v: &Value, work: &mut Work) -> Result<Vec<Value>, Error> {
    match v {
        Value::Arr(items) => {
            work.charge(items.len())?;
            Ok(items.clone())
        }
        Value::Str(s) => {
            work.charge(s.len())?;
            Ok(s.chars().map(|c| Value::Str(c.to_string())).collect())
        }
        Value::Obj(pairs) => {
            unique_keys(pairs, work)?;
            Ok(pairs.iter().map(|(k, _)| Value::Str(k.clone())).collect())
        }
        _ => Err(Error::Python("TypeError")),
    }
}

enum Num {
    Int(i128),
    Float(f64),
}

fn num(v: &Value) -> Result<Option<Num>, Error> {
    Ok(match v {
        Value::Bool(b) => Some(Num::Int(i128::from(*b))),
        Value::Int(_) => {
            Some(Num::Int(big(v, "a compared integer").map_err(|_| {
                Error::Unsupported("a huge compared integer")
            })?))
        }
        Value::Float(x) => {
            if x.is_nan() {
                // NaN != NaN, but Python's containers compare an object to itself as equal.
                return Err(Error::Unsupported("a NaN in a compared value"));
            }
            Some(Num::Float(*x))
        }
        _ => None,
    })
}

/// Python's `a == b` for JSON values: ints, floats and bools compare by value (exactly), lists
/// item by item, dicts as maps; different kinds are unequal.
pub fn py_eq(a: &Value, b: &Value, work: &mut Work) -> Result<bool, Error> {
    work.charge(1)?;
    if let (Some(x), Some(y)) = (num(a)?, num(b)?) {
        return Ok(match (x, y) {
            (Num::Int(i), Num::Int(j)) => i == j,
            (Num::Float(p), Num::Float(q)) => p == q,
            (Num::Int(i), Num::Float(p)) | (Num::Float(p), Num::Int(i)) => {
                // Exact: the float must be integral and equal to the int. |i| < 2^100.
                p.is_finite() && p.trunc() == p && p.abs() < f(INT_LIMIT) && (p as i128) == i
            }
        });
    }
    Ok(match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Arr(x), Value::Arr(y)) => {
            if x.len() != y.len() {
                return Ok(false);
            }
            for (p, q) in x.iter().zip(y) {
                if !py_eq(p, q, work)? {
                    return Ok(false);
                }
            }
            true
        }
        (Value::Obj(x), Value::Obj(y)) => {
            unique_keys(x, work)?;
            unique_keys(y, work)?;
            if x.len() != y.len() {
                return Ok(false);
            }
            for (k, p) in x {
                match b.get(k) {
                    Some(q) if py_eq(p, q, work)? => {}
                    _ => return Ok(false),
                }
            }
            true
        }
        _ => false,
    })
}

/// Python's `str(x)` / `f"{x}"` (`repr`) of a float whose magnitude is in [1e-4, 1e16): the
/// shortest round-trip digits with a ".0" when integral, which is what Rust's `{:?}` prints in
/// that range too (outside it Python writes "1e+16" where Rust writes "1e16"; refused).
pub fn float_repr(x: f64, what: &'static str) -> Result<String, Error> {
    if !(x.is_finite() && (1e-4..1e16).contains(&x.abs())) {
        return Err(Error::Unsupported(what));
    }
    Ok(format!("{x:?}"))
}

/// Python's `str(v)` for the values it is used on here (names): strings, ints, floats, bools.
pub fn py_str(v: &Value, what: &'static str) -> Result<String, Error> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        Value::Bool(true) => Ok("True".to_owned()),
        Value::Bool(false) => Ok("False".to_owned()),
        Value::Null => Ok("None".to_owned()),
        Value::Int(i) => Ok(i.to_string()),
        Value::Float(x) => float_repr(*x, what),
        _ => Err(Error::Unsupported(what)),
    }
}

/// Whether `str(v).lower().startswith(prefix)`, for the ASCII lower-case prefixes used here
/// ("gemma", "gemma3": no "k" or "i"). The only characters outside ASCII that lower-case into
/// ASCII are U+212A KELVIN SIGN ("k") and U+0130 ("i" + a combining dot), so a non-ASCII
/// character among the first `prefix.len()` never matches. `str()` of a non-string starts with
/// a digit, a sign, a bracket or "True"/"False"/"None", none of which match either.
pub fn lower_starts_with(v: &Value, prefix: &str) -> bool {
    let s = match v {
        Value::Str(s) => s.as_str(),
        _ => return false,
    };
    let n = prefix.len();
    let head: String = s.chars().take(n).collect();
    head.len() == n && head.is_ascii() && head.eq_ignore_ascii_case(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::Str(x.to_owned())
    }

    #[test]
    fn int_of_strings_matches_python() {
        // Checked against CPython 3.12's int().
        assert_eq!(int_str(" 12 ", "t"), Ok(12));
        assert_eq!(int_str("+4", "t"), Ok(4));
        assert_eq!(int_str("-0_7", "t"), Ok(-7));
        assert_eq!(int_str("1_000", "t"), Ok(1000));
        assert_eq!(int_str("\u{b}8\u{c}", "t"), Ok(8));
        assert_eq!(int_str("\u{1c}8", "t"), Err(Error::Python("ValueError")));
        assert_eq!(int_str("8\u{a0}", "t"), Err(Error::Unsupported("t")));
        assert_eq!(int_str("007", "t"), Ok(7));
        for bad in [
            "", " ", "_1", "1_", "1__0", "x", "1.5", "+", "- 1", "0x10", "1e3",
        ] {
            assert_eq!(
                int_str(bad, "t"),
                Err(Error::Python("ValueError")),
                "{bad:?}"
            );
        }
        assert_eq!(int_str("\u{663}", "t"), Err(Error::Unsupported("t")));
        assert_eq!(int_str(&"9".repeat(40), "t"), Err(Error::OutOfRange("t")));
    }

    #[test]
    fn int_of_values() {
        assert_eq!(int_of(&Value::Float(-2.7), "t"), Ok(-2));
        assert_eq!(int_of(&Value::Bool(true), "t"), Ok(1));
        assert_eq!(int_of(&Value::Null, "t"), Err(Error::Python("TypeError")));
        assert_eq!(
            int_of(&Value::Float(f64::NAN), "t"),
            Err(Error::Python("ValueError"))
        );
        assert_eq!(
            int_of(&Value::Float(f64::INFINITY), "t"),
            Err(Error::Python("OverflowError"))
        );
        assert_eq!(
            int_of(&Value::Arr(vec![]), "t"),
            Err(Error::Python("TypeError"))
        );
    }

    #[test]
    fn equality_matches_python() {
        let mut w = Work::new(1000);
        let one = model_files::json::parse("1").unwrap_or(Value::Null);
        assert!(py_eq(&one, &Value::Float(1.0), &mut w).unwrap_or(false));
        assert!(py_eq(&one, &Value::Bool(true), &mut w).unwrap_or(false));
        assert!(!py_eq(&one, &s("1"), &mut w).unwrap_or(true));
        assert!(!py_eq(&Value::Float(1.5), &one, &mut w).unwrap_or(true));
        assert!(py_eq(&Value::Null, &Value::Null, &mut w).unwrap_or(false));
        assert!(py_eq(
            &Value::Arr(vec![one.clone(), s("a")]),
            &Value::Arr(vec![Value::Float(1.0), s("a")]),
            &mut w
        )
        .unwrap_or(false));
        assert!(py_eq(&Value::Float(f64::NAN), &Value::Null, &mut w).is_err());
    }

    #[test]
    fn float_repr_matches_python() {
        // Checked against CPython 3.12's repr().
        for (x, want) in [
            (1.0, "1.0"),
            (2.5, "2.5"),
            (-1.0, "-1.0"),
            (0.0001, "0.0001"),
            (1099511627776.0, "1099511627776.0"),
            (0.1 + 0.2, "0.30000000000000004"),
            (9999999999999998.0, "9999999999999998.0"),
        ] {
            assert_eq!(float_repr(x, "t").as_deref(), Ok(want));
        }
        assert!(float_repr(1e16, "t").is_err() && float_repr(1e-5, "t").is_err());
    }

    #[test]
    fn strip_and_prefix() {
        assert_eq!(py_strip("\u{1f} none \u{3000}"), "none");
        assert!(lower_starts_with(&s("GEMMA3n"), "gemma3"));
        assert!(!lower_starts_with(&s("gemm\u{e1}3"), "gemma3"));
        assert!(!lower_starts_with(&Value::Float(1.0), "gemma"));
        assert!(!lower_starts_with(&s("gem"), "gemma"));
    }
}
