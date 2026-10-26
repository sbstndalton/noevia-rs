// Unicode decimal zero codepoints generated from CPython 3.12 / Unicode 15.0 decimal().
const DECIMAL_ZEROS: &[u32] = &[
    0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66,
    0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90,
    0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0, 0xff10,
    0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0, 0x11650,
    0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16a60, 0x16ac0,
    0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0, 0x1e950,
    0x1fbf0,
];
const INTEGER_WHITESPACE: &[u32] = &[
    0x9, 0xa, 0xb, 0xc, 0xd, 0x20, 0x85, 0xa0, 0x1680, 0x2000, 0x2001, 0x2002, 0x2003, 0x2004,
    0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200a, 0x2028, 0x2029, 0x202f, 0x205f, 0x3000,
];
// The shipped CPython runtime's decimal input/output conversion limit, not an
// integer-width limit. Incrementing 4300 nines also exceeds its output limit.
const MAX_DECIMAL_DIGITS: usize = 4300;
fn decimal(c: char) -> Option<u8> {
    let code = c as u32;
    DECIMAL_ZEROS
        .iter()
        .find_map(|zero| code.checked_sub(*zero).filter(|n| *n < 10).map(|n| n as u8))
}
pub(crate) fn increment(raw: &str) -> Result<String, crate::Error> {
    let raw = raw.trim_matches(|c: char| INTEGER_WHITESPACE.contains(&(c as u32)));
    let (negative, raw) = if let Some(rest) = raw.strip_prefix('-') {
        (true, rest)
    } else {
        (false, raw.strip_prefix('+').unwrap_or(raw))
    };
    let mut digits = Vec::new();
    let mut previous_digit = false;
    for c in raw.chars() {
        if c == '_' {
            if !previous_digit {
                return Err(crate::Error::Generation);
            }
            previous_digit = false;
        } else {
            if digits.len() >= MAX_DECIMAL_DIGITS {
                return Err(crate::Error::Generation);
            }
            digits.push(b'0' + decimal(c).ok_or(crate::Error::Generation)?);
            previous_digit = true;
        }
    }
    if !previous_digit {
        return Err(crate::Error::Generation);
    }
    let first = digits
        .iter()
        .position(|d| *d != b'0')
        .unwrap_or(digits.len());
    let mut digits = digits
        .get(first..)
        .ok_or(crate::Error::Generation)?
        .to_vec();
    if digits.is_empty() {
        return Ok("1".into());
    }
    if negative {
        for digit in digits.iter_mut().rev() {
            if *digit > b'0' {
                *digit -= 1;
                break;
            }
            *digit = b'9';
        }
        let first = digits
            .iter()
            .position(|d| *d != b'0')
            .unwrap_or(digits.len());
        let digits = digits.get(first..).ok_or(crate::Error::Generation)?;
        if digits.is_empty() {
            return Ok("0".into());
        }
        return Ok(format!(
            "-{}",
            String::from_utf8(digits.to_vec()).map_err(|_| crate::Error::Generation)?
        ));
    }
    for digit in digits.iter_mut().rev() {
        if *digit < b'9' {
            *digit += 1;
            return String::from_utf8(digits).map_err(|_| crate::Error::Generation);
        }
        *digit = b'0';
    }
    if digits.len() >= MAX_DECIMAL_DIGITS {
        return Err(crate::Error::Generation);
    }
    digits.insert(0, b'1');
    String::from_utf8(digits).map_err(|_| crate::Error::Generation)
}

#[cfg(test)]
mod tests {
    use super::{increment, DECIMAL_ZEROS, INTEGER_WHITESPACE, MAX_DECIMAL_DIGITS};

    #[derive(serde::Deserialize)]
    struct Fixture {
        python_version: String,
        unicode_version: String,
        decimal_zeros: Vec<u32>,
        integer_whitespace: Vec<u32>,
        integer_digits_limit: usize,
        cases: Vec<(String, Option<String>)>,
    }

    #[test]
    fn decimal_table_matches_shipped_python() -> Result<(), serde_json::Error> {
        let fixture: Fixture =
            serde_json::from_str(include_str!("../tests/generation-cases.json"))?;
        assert!(fixture.python_version.starts_with("3.12."));
        assert_eq!(fixture.unicode_version, "15.0.0");
        assert_eq!(fixture.decimal_zeros.len(), 68);
        assert_eq!(DECIMAL_ZEROS, fixture.decimal_zeros);
        assert_eq!(INTEGER_WHITESPACE, fixture.integer_whitespace);
        assert_eq!(MAX_DECIMAL_DIGITS, fixture.integer_digits_limit);
        Ok(())
    }

    #[test]
    fn generation_acceptance_matches_actual_python() -> Result<(), serde_json::Error> {
        let fixture: Fixture =
            serde_json::from_str(include_str!("../tests/generation-cases.json"))?;
        assert_eq!(fixture.cases.len(), 809);
        for (raw, expected) in fixture.cases {
            assert_eq!(
                increment(&raw),
                expected.ok_or(crate::Error::Generation),
                "{raw:?}"
            );
        }
        Ok(())
    }
}
