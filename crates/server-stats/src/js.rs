//! The JavaScript the stats handler evaluates, over `JSON.parse` values held as UTF-16 code units
//! (prompt-framing's ECMAScript-exact `JSON.parse`; lone surrogates kept): property reads with and
//! without `?.`, truthiness, `===`, `String(x)`, `Number(string)`, `encodeURIComponent` and
//! `JSON.stringify`.
//!
//! A property read here is only ever of a name no prototype in play defines (`data`, `status`,
//! `id`, `model_name`, `labels`, ...), so it is the object's own member or `undefined`. A value
//! nested deeper than [`DEPTH_CAP`] was not built by the parser ([`Value::Deep`]); where the JS
//! would look inside one, the port answers [`Fault::Unsupported`] instead of guessing.

use prompt_framing::json::{self, Value};

/// Containers at this depth or deeper are [`Value::Deep`]. Every field the handler reads is at
/// depth 4 or less.
pub const DEPTH_CAP: usize = 64;

/// What evaluating an expression did instead of giving a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The JS throws here (a TypeError, URIError or SyntaxError).
    Throws,
    /// The JS gives an answer this port does not model (a value nested past [`DEPTH_CAP`]).
    Unsupported,
}

/// http.cjs `fetchJson`'s body: `JSON.parse(text)`, or the text itself when that throws. The bytes
/// are decoded as `Buffer.toString('utf8')` does (each invalid sequence one U+FFFD; a BOM is kept,
/// so `JSON.parse` then throws).
pub fn body_of(bytes: &[u8]) -> Value {
    let units: Vec<u16> = String::from_utf8_lossy(bytes).encode_utf16().collect();
    match json::parse(&units, DEPTH_CAP) {
        Some(v) => v,
        None => Value::Str(units),
    }
}

/// `v.key`: `undefined` or `null` throws.
pub fn get<'a>(v: Option<&'a Value>, key: &str) -> Result<Option<&'a Value>, Fault> {
    match v {
        None | Some(Value::Null) => Err(Fault::Throws),
        Some(v) => opt(Some(v), key),
    }
}

/// `v?.key`.
pub fn opt<'a>(v: Option<&'a Value>, key: &str) -> Result<Option<&'a Value>, Fault> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Deep) => Err(Fault::Unsupported),
        Some(o @ Value::Obj(_)) => Ok(o.get(key)),
        Some(_) => Ok(None),
    }
}

/// `!!v`.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Num(n)) => *n != 0.0 && !n.is_nan(),
        Some(Value::Str(s)) => !s.is_empty(),
        Some(Value::Arr(_) | Value::Obj(_) | Value::Deep) => true,
    }
}

/// `a === b` for values from two different `JSON.parse` calls (two objects are never the same).
pub fn strict_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Value::Null), Some(Value::Null)) => true,
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x == y,
        (Some(Value::Num(x)), Some(Value::Num(y))) => x == y,
        (Some(Value::Str(x)), Some(Value::Str(y))) => x == y,
        _ => false,
    }
}

/// `typeof v === 'number' ? v : null`, as a number to write.
pub fn number(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Num(n)) => Some(*n),
        _ => None,
    }
}

/// `String(n)` for a number.
pub fn number_string(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n == f64::INFINITY {
        "Infinity".into()
    } else if n == f64::NEG_INFINITY {
        "-Infinity".into()
    } else {
        tool_exchange::js_number(n)
    }
}

/// `String(v)`. An object whose own `toString` is data (not callable) throws: `valueOf` then
/// gives the object back, so no primitive comes out.
pub fn to_string(v: Option<&Value>) -> Result<Vec<u16>, Fault> {
    let mut out = Vec::new();
    push_string(&mut out, v)?;
    Ok(out)
}

fn push_string(out: &mut Vec<u16>, v: Option<&Value>) -> Result<(), Fault> {
    match v {
        None => out.extend("undefined".encode_utf16()),
        Some(Value::Null) => out.extend("null".encode_utf16()),
        Some(Value::Bool(b)) => out.extend(if *b { "true" } else { "false" }.encode_utf16()),
        Some(Value::Num(n)) => out.extend(number_string(*n).encode_utf16()),
        Some(Value::Str(s)) => out.extend_from_slice(s),
        Some(Value::Arr(items)) => {
            // Array.prototype.join: null and undefined elements are empty.
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(u16::from(b','));
                }
                if !matches!(item, Value::Null) {
                    push_string(out, Some(item))?;
                }
            }
        }
        Some(Value::Obj(members)) => {
            if members
                .iter()
                .any(|(k, _)| k.iter().copied().eq("toString".encode_utf16()))
            {
                return Err(Fault::Throws);
            }
            out.extend("[object Object]".encode_utf16());
        }
        Some(Value::Deep) => return Err(Fault::Unsupported),
    }
    Ok(())
}

/// ECMAScript `\s` for one code unit (WhiteSpace and LineTerminator).
pub fn is_space(u: u16) -> bool {
    prompt_framing::js::is_js_space(u)
}

/// `Number(s)`: ECMAScript StringToNumber (whitespace trimmed; `Infinity`; `0x`/`0o`/`0b`
/// literals of any length, correctly rounded; everything else that is not a decimal literal NaN).
pub fn string_to_number(s: &[u16]) -> f64 {
    let t = prompt_framing::js::trim(s);
    if t.is_empty() {
        return 0.0;
    }
    if t.iter().any(|&c| c >= 0x80) {
        return f64::NAN;
    }
    let t: Vec<u8> = t.iter().map(|&c| c as u8).collect();
    if let (Some(b'0'), Some(p)) = (t.first(), t.get(1)) {
        let radix = match p {
            b'x' | b'X' => Some(16),
            b'o' | b'O' => Some(8),
            b'b' | b'B' => Some(2),
            _ => None,
        };
        if let Some(radix) = radix {
            return radix_value(t.get(2..).unwrap_or(&[]), radix);
        }
    }
    let (neg, body) = match t.first() {
        Some(b'+') => (false, t.get(1..).unwrap_or(&[])),
        Some(b'-') => (true, t.get(1..).unwrap_or(&[])),
        _ => (false, t.as_slice()),
    };
    if body == b"Infinity" {
        return if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    if !decimal_literal(body) {
        return f64::NAN;
    }
    let text = std::str::from_utf8(body).unwrap_or("x");
    match text.parse::<f64>() {
        Ok(n) if neg => -n,
        Ok(n) => n,
        Err(_) => f64::NAN,
    }
}

/// StrUnsignedDecimalLiteral without `Infinity`: digits with an optional fraction (either side may
/// be empty, not both) and an optional exponent.
fn decimal_literal(b: &[u8]) -> bool {
    let digits = |s: &[u8], mut i: usize| {
        let start = i;
        while s.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        (i, i - start)
    };
    let (mut i, int) = digits(b, 0);
    let mut frac = 0;
    if b.get(i) == Some(&b'.') {
        let (j, n) = digits(b, i + 1);
        i = j;
        frac = n;
    }
    if int == 0 && frac == 0 {
        return false;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let (j, n) = digits(b, i);
        if n == 0 {
            return false;
        }
        i = j;
    }
    i == b.len()
}

/// The value of a `0x`/`0o`/`0b` literal's digits, rounded to nearest, ties to even.
fn radix_value(digits: &[u8], radix: u32) -> f64 {
    if digits.is_empty() {
        return f64::NAN;
    }
    let width = match radix {
        16 => 4,
        8 => 3,
        _ => 1,
    };
    let mut bits: Vec<bool> = Vec::with_capacity(digits.len() * width);
    for &d in digits {
        let Some(v) = char::from(d).to_digit(radix) else {
            return f64::NAN;
        };
        for k in (0..width).rev() {
            bits.push((v >> k) & 1 == 1);
        }
    }
    let first = bits.iter().position(|&b| b).unwrap_or(bits.len());
    let bits = bits.get(first..).unwrap_or(&[]);
    if bits.len() <= 64 {
        let n = bits.iter().fold(0u64, |n, &b| (n << 1) | u64::from(b));
        return n as f64;
    }
    // The top 64 bits, with every lower set bit folded into the last one (a sticky bit below the
    // rounding position), round to the same double as the whole number.
    let (top, rest) = bits.split_at(64);
    let mut n = top.iter().fold(0u64, |n, &b| (n << 1) | u64::from(b));
    if rest.iter().any(|&b| b) {
        n |= 1;
    }
    let shift = i32::try_from(rest.len()).unwrap_or(i32::MAX);
    (n as f64) * 2f64.powi(shift)
}

/// `encodeURIComponent(String(v))`; a lone surrogate is a URIError.
pub fn encode_uri_component(v: Option<&Value>) -> Result<String, Fault> {
    let units = to_string(v)?;
    let mut out = String::new();
    for r in char::decode_utf16(units.iter().copied()) {
        let c = r.map_err(|_| Fault::Throws)?;
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    Ok(out)
}

/// `JSON.stringify(v)` for a value that came out of `JSON.parse` (no `undefined` inside, nothing
/// callable). Non-finite numbers are `null`.
pub fn stringify(out: &mut Vec<u8>, v: &Value) -> Result<(), Fault> {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Num(n) => push_number(out, *n),
        Value::Str(s) => json::push_str(out, s),
        Value::Arr(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                stringify(out, item)?;
            }
            out.push(b']');
        }
        Value::Obj(members) => {
            out.push(b'{');
            for (i, (k, item)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                json::push_str(out, k);
                out.push(b':');
                stringify(out, item)?;
            }
            out.push(b'}');
        }
        Value::Deep => return Err(Fault::Unsupported),
    }
    Ok(())
}

/// A number as `JSON.stringify` writes it.
pub fn push_number(out: &mut Vec<u8>, n: f64) {
    if n.is_finite() {
        out.extend_from_slice(tool_exchange::js_number(n).as_bytes());
    } else {
        out.extend_from_slice(b"null");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn string_to_number_follows_the_spec() {
        for (s, want) in [
            ("12", 12.0),
            ("+12", 12.0),
            ("-0", -0.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("1e3", 1000.0),
            ("1E+3", 1000.0),
            ("1e-3", 0.001),
            ("0x1A", 26.0),
            ("0X1a", 26.0),
            ("0o17", 15.0),
            ("0b101", 5.0),
            ("Infinity", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
            ("1e400", f64::INFINITY),
            ("", 0.0),
            ("  7  ", 7.0),
        ] {
            let got = string_to_number(&u(s));
            assert!(
                got == want && got.is_sign_negative() == want.is_sign_negative(),
                "{s}: {got}"
            );
        }
        for s in [
            "1e", "e1", ".", "+", "-0x1", "0x", "0xg", "1_000", "inf", "nan", "NaN", "infinity",
            "1.2.3", "١", "+-1", "--1",
        ] {
            assert!(string_to_number(&u(s)).is_nan(), "{s}");
        }
        // A long hex literal rounds to nearest, ties to even, like V8.
        assert_eq!(string_to_number(&u("0x20000000000001")), 9007199254740992.0);
        assert_eq!(string_to_number(&u("0x20000000000003")), 9007199254740996.0);
        // Values V8 gives (Node 26): past 64 bits, ties to even and the sticky bit.
        for (s, want) in [
            ("0x200000000000010000000000000000001", 6.805647338418771e38),
            ("0x20000000000001000000000000000000", 4.253529586511731e37),
            ("0x2000000000000180000000000000000", 2.6584559915698323e36),
            ("0x20000000000001800000000000000001", 4.253529586511732e37),
        ] {
            assert_eq!(string_to_number(&u(s)), want, "{s}");
        }
        assert_eq!(
            string_to_number(&u(&format!("0x1{}", "0".repeat(300)))),
            f64::INFINITY
        );
    }

    #[test]
    fn to_string_and_encoding() {
        let v = json::parse(&u(r#"[1,null,"a",[2,3],{"x":1}]"#), DEPTH_CAP).unwrap();
        assert_eq!(
            String::from_utf16(&to_string(Some(&v)).unwrap()).unwrap(),
            "1,,a,2,3,[object Object]"
        );
        let own = json::parse(&u(r#"{"toString":"x"}"#), DEPTH_CAP).unwrap();
        assert_eq!(to_string(Some(&own)), Err(Fault::Throws));
        assert_eq!(
            encode_uri_component(Some(&Value::Str(u("a b/ü€😀-_.!~*'()")))).unwrap(),
            "a%20b%2F%C3%BC%E2%82%AC%F0%9F%98%80-_.!~*'()"
        );
        assert_eq!(
            encode_uri_component(Some(&Value::Str(vec![0x61, 0xd800]))),
            Err(Fault::Throws)
        );
        assert_eq!(encode_uri_component(None).unwrap(), "undefined");
        assert_eq!(
            encode_uri_component(Some(&Value::Num(1e21))).unwrap(),
            "1e%2B21"
        );
    }
}
