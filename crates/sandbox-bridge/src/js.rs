//! The few ECMAScript conversions the bridge relies on: truthiness, `String(value)` and
//! `Number.prototype.toString()`, for values that came out of `JSON.parse`.

use crate::json::{parse, Value};

/// `String(value)` of nested arrays recurses (`Array.prototype.join`). V8 throws a RangeError at a
/// few thousand levels; past this many this port gives up (the caller fails closed).
pub const MAX_COERCE_DEPTH: usize = 1000;

/// Why `String(value)` did not produce text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoerceError {
    /// JS throws `TypeError: Cannot convert object to primitive value` (an object with an own,
    /// non-callable `toString`).
    TypeError,
    /// Nested deeper than [`MAX_COERCE_DEPTH`].
    Depth,
}

/// JS truthiness of a JSON value.
pub fn truthy(v: &Value<'_>) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Num(n) => {
            let x = n.parse::<f64>().unwrap_or(f64::NAN);
            x != 0.0 && !x.is_nan()
        }
        Value::Str(s) => !s.is_empty(),
        Value::Arr(_) | Value::Obj(_) | Value::Raw(_) => true,
    }
}

/// `Number.prototype.toString()` (radix 10), ECMAScript Number::toString.
pub fn number_to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if x == 0.0 {
        return "0".to_owned();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-Infinity" } else { "Infinity" }.to_owned();
    }
    let sign = if x < 0.0 { "-" } else { "" };
    // Shortest round-trip digits, as `d.ddde±N`.
    let sci = format!("{:e}", x.abs());
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let e: i64 = exp.parse().unwrap_or(0);
    let k = digits.len() as i64;
    let n = e + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        let (a, b) = digits.split_at(n as usize);
        format!("{a}.{b}")
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let es = if n > 0 { '+' } else { '-' };
        let ea = (n - 1).abs();
        if k == 1 {
            format!("{digits}e{es}{ea}")
        } else {
            let (a, b) = digits.split_at(1);
            format!("{a}.{b}e{es}{ea}")
        }
    };
    format!("{sign}{body}")
}

/// `String(value)` for a JSON value, as UTF-16 code units.
pub fn to_string16(v: &Value<'_>) -> Result<Vec<u16>, CoerceError> {
    coerce(v, 0)
}

fn coerce(v: &Value<'_>, depth: usize) -> Result<Vec<u16>, CoerceError> {
    if depth > MAX_COERCE_DEPTH {
        return Err(CoerceError::Depth);
    }
    Ok(match v {
        Value::Null => "null".encode_utf16().collect(),
        Value::Bool(b) => if *b { "true" } else { "false" }.encode_utf16().collect(),
        Value::Num(n) => number_to_string(n.parse::<f64>().unwrap_or(f64::NAN))
            .encode_utf16()
            .collect(),
        Value::Str(s) => s.clone(),
        Value::Obj(_) => {
            // ToPrimitive(hint string): an own `toString` shadows Object.prototype.toString and is
            // not callable (JSON has no functions); Object.prototype.valueOf returns the object
            // itself, so nothing primitive comes out: TypeError.
            if v.get("toString").is_some() {
                return Err(CoerceError::TypeError);
            }
            "[object Object]".encode_utf16().collect()
        }
        Value::Arr(items) => {
            // Array.prototype.join(","): null/undefined elements are empty.
            let mut out = Vec::new();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(u16::from(b','));
                }
                if *item != Value::Null {
                    out.extend(coerce(item, depth + 1)?);
                }
            }
            out
        }
        Value::Raw(text) => {
            let inner = parse(text).ok_or(CoerceError::Depth)?;
            coerce(&inner, depth)?
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_like_js() {
        for (x, s) in [
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (123e-20, "1.23e-18"),
            (0.000001, "0.000001"),
            (0.0000001, "1e-7"),
            (-1.5, "-1.5"),
            (5e-324, "5e-324"),
            (0.1 + 0.2, "0.30000000000000004"),
            (-0.0, "0"),
        ] {
            assert_eq!(number_to_string(x), s);
        }
    }
}
