//! The Python string semantics slices 3-6 lean on (CPython 3.12, Unicode 15.0): `str.lower()`
//! compared against ASCII text, `repr()` of a string, `format(n, ",")`, `format(x, ".Nf")`.
//!
//! Where reproducing Python would need Unicode tables (the lower case of most non-ASCII
//! characters, `str.isprintable()` past U+024F), the port refuses with [`Error::Unsupported`]
//! instead of guessing.

use crate::Error;
use std::fmt::Write as _;

/// `s.lower()` as far as comparisons with ASCII text can tell: ASCII letters lower-cased, and the
/// only two characters outside ASCII whose lower case contains ASCII mapped as CPython maps them
/// (U+0130 to "i" U+0307, U+212A KELVIN SIGN to "k"; checked over every code point). Any other
/// non-ASCII character is kept as it is: its real lower case is non-ASCII too, so an ASCII
/// substring, prefix, suffix or equality test reads the same either way. Two results that both
/// still hold non-ASCII characters can NOT be compared with each other (see [`lower_eq`]).
pub fn lower_tok(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{130}' => out.push_str("i\u{307}"),
            '\u{212a}' => out.push('k'),
            c => out.push(c.to_ascii_lowercase()),
        }
    }
    out
}

/// `a.lower() == b.lower()`.
pub fn lower_eq(a: &str, b: &str) -> Result<bool, Error> {
    if a == b {
        return Ok(true);
    }
    let (la, lb) = (lower_tok(a), lower_tok(b));
    match (la.is_ascii(), lb.is_ascii()) {
        (true, true) => Ok(la == lb),
        // One side's lower case still holds a non-ASCII character, the other's does not.
        (true, false) | (false, true) => Ok(false),
        (false, false) => Err(Error::Unsupported(
            "a case-insensitive comparison of non-ASCII text",
        )),
    }
}

/// `repr(s)` for a string of characters below U+0250 (ASCII, Latin-1 and Latin Extended-A/B,
/// where CPython 3.12 escapes exactly the C0/C1 controls, DEL, U+00A0 and U+00AD); anything
/// above is refused, since its printability is a Unicode-table question.
pub fn repr(s: &str) -> Result<String, Error> {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        let u = c as u32;
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            _ if u < 0x20 || (0x7f..=0xa0).contains(&u) || u == 0xad => {
                let _ = write!(out, "\\x{u:02x}");
            }
            _ if u < 0x250 => out.push(c),
            _ => return Err(Error::Unsupported("repr() of a character past U+024F")),
        }
    }
    out.push(quote);
    Ok(out)
}

/// `f"{n:,}"`: decimal digits in groups of three, the sign in front.
pub fn thousands(n: i128) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(d);
    }
    out
}

/// `f"{x:.{nd}f}"` for a finite float: the exact binary value rounded half-to-even, which is
/// what Rust's fixed-precision formatting does too (`-0.00` included). Non-finite values are
/// refused (Python writes "inf"/"nan", which no recommendation should contain).
pub fn fixed(x: f64, nd: usize) -> Result<String, Error> {
    if !x.is_finite() {
        return Err(Error::Unsupported("a non-finite number in a message"));
    }
    Ok(format!("{x:.nd$}"))
}

/// `_fmt_ctx(n)`: "32K" for whole multiples of 1024 from 1024 up, else `f"{n:,}"`.
pub fn fmt_ctx(n: i128) -> String {
    if n >= 1024 && n % 1024 == 0 {
        format!("{}K", n / 1024)
    } else {
        thousands(n)
    }
}

/// `s.strip()` for the characters `str.strip()` drops (see pyval).
pub fn strip(s: &str) -> &str {
    crate::pyval::py_strip(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_matches_python() {
        // Checked against CPython 3.12's repr().
        for (s, want) in [
            ("abc", "'abc'"),
            ("it's", "\"it's\""),
            ("say \"hi\"", "'say \"hi\"'"),
            ("both'\"", "'both\\'\"'"),
            ("back\\slash", "'back\\\\slash'"),
            ("tab\there\nx\r", "'tab\\there\\nx\\r'"),
            (
                "\u{7f}\u{1}\u{85}\u{a0}\u{ad}",
                "'\\x7f\\x01\\x85\\xa0\\xad'",
            ),
            ("\u{fc}\u{130}\u{24f}", "'\u{fc}\u{130}\u{24f}'"),
            ("", "''"),
        ] {
            assert_eq!(repr(s).as_deref(), Ok(want), "{s:?}");
        }
        assert!(repr("\u{2192}").is_err());
    }

    #[test]
    fn numbers_match_python() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(-5), "-5");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(-1000), "-1,000");
        assert_eq!(thousands(100), "100");
        assert_eq!(fmt_ctx(32768), "32K");
        assert_eq!(fmt_ctx(1023), "1,023");
        assert_eq!(fmt_ctx(-2048), "-2,048");
        assert_eq!(fixed(0.125, 2).as_deref(), Ok("0.12"));
        assert_eq!(fixed(0.375, 2).as_deref(), Ok("0.38"));
        assert_eq!(fixed(2.5, 0).as_deref(), Ok("2"));
        assert_eq!(fixed(-0.0, 2).as_deref(), Ok("-0.00"));
        assert!(fixed(f64::NAN, 2).is_err());
    }

    #[test]
    fn lower_against_ascii() {
        assert_eq!(lower_tok("Ab-MTP\u{212a}"), "ab-mtpk");
        assert!(lower_eq("Q8_0", "q8_0").unwrap_or(false));
        assert!(lower_eq("K\u{212a}", "kk").unwrap_or(false));
        assert!(!lower_eq("\u{e9}", "e").unwrap_or(true));
        assert!(lower_eq("\u{e9}", "\u{e9}").unwrap_or(false));
        assert!(lower_eq("\u{c9}", "\u{e9}").is_err());
    }
}
