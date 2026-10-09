//! The few pieces of Python float semantics the size core leans on, reproduced exactly.

use crate::Error;

/// Largest magnitude accepted for any integer that the size core does arithmetic with. Real
/// values are tiny (contexts below 2^40, head counts below 2^16); the cap keeps every product
/// the core forms well inside `i128` and every float finite.
pub const INT_LIMIT: i128 = 1 << 100;

/// `float(i)` for a Python int: round to nearest, ties to even (what `as` does for `i128`).
pub fn f(i: i128) -> f64 {
    i as f64
}

/// Python `round(x, nd)` for a float: the exact binary value rounded to `nd` decimals, ties to
/// even, read back as the nearest float. Rust's fixed-precision formatting is exact and rounds
/// ties to even too. Infinities and NaN come back unchanged, as in Python.
pub fn round_nd(x: f64, nd: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.nd$}").parse::<f64>().unwrap_or(x)
}

/// Python `round(x)` for a float (an int, ties to even). Python raises for NaN and infinity.
pub fn round_int(x: f64) -> Result<i128, Error> {
    to_int(x.round_ties_even())
}

/// Python `int(x)` for a float: truncation. Python raises for NaN and infinity; values past
/// [`INT_LIMIT`] are refused here (Python would carry on with a huge int).
pub fn trunc_int(x: f64) -> Result<i128, Error> {
    to_int(x.trunc())
}

fn to_int(t: f64) -> Result<i128, Error> {
    if t.is_nan() {
        return Err(Error::Python("ValueError"));
    }
    if t.is_infinite() {
        return Err(Error::Python("OverflowError"));
    }
    if t.abs() >= f(INT_LIMIT) {
        return Err(Error::OutOfRange("an integer result"));
    }
    // |t| < 2^100 and integral, so the conversion is exact.
    Ok(t as i128)
}

/// Python `int(x)` of a float that is only compared afterwards: `None` stands for "at least
/// [`INT_LIMIT`]" (it can only be positive where this is used).
pub fn trunc_cmp(x: f64) -> Result<Option<i128>, Error> {
    let t = x.trunc();
    if t.is_nan() {
        return Err(Error::Python("ValueError"));
    }
    if t.is_infinite() {
        return Err(Error::Python("OverflowError"));
    }
    if t >= f(INT_LIMIT) {
        return Ok(None);
    }
    if t <= -f(INT_LIMIT) {
        return Err(Error::OutOfRange("an integer result"));
    }
    Ok(Some(t as i128))
}

/// Python `max(a, b)`: `a` unless `b > a` (so NaN handling follows Python's comparisons).
pub fn max(a: f64, b: f64) -> f64 {
    if b > a {
        b
    } else {
        a
    }
}

/// Python `min(a, b)`: `a` unless `b < a`.
pub fn min(a: f64, b: f64) -> f64 {
    if b < a {
        b
    } else {
        a
    }
}

/// A float as Python's `json.dumps` writes it, closely enough that `json.loads` gives the same
/// float back: shortest round-trip digits, always marked as a float, and Python's spellings of
/// the non-finite values.
pub fn json(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    // Debug prints the shortest representation that round-trips, with ".0" or an exponent.
    format!("{x:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_matches_python() {
        // Values checked against CPython 3.12's round().
        assert_eq!(round_nd(0.125, 2), 0.12);
        assert_eq!(round_nd(0.375, 2), 0.38);
        assert_eq!(round_nd(2.675, 2), 2.67);
        assert_eq!(round_nd(0.0625, 3), 0.062);
        assert_eq!(round_nd(-0.001, 2).to_bits(), (-0.0f64).to_bits());
        assert_eq!(round_int(2.5), Ok(2));
        assert_eq!(round_int(3.5), Ok(4));
        assert_eq!(round_int(f64::NAN), Err(Error::Python("ValueError")));
        assert_eq!(trunc_int(-2.7), Ok(-2));
    }

    #[test]
    fn json_floats_stay_floats() {
        assert_eq!(json(2.0), "2.0");
        assert_eq!(json(1e16), "1e16");
        assert_eq!(json(-0.0), "-0.0");
        assert_eq!(json(f64::NEG_INFINITY), "-Infinity");
    }
}
