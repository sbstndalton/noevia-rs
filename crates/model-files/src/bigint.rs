//! Just enough of Python's `int` to carry a size through unchanged: a sign and decimal digits.

use std::fmt;

/// An integer of any length, kept as normalised decimal digits (no leading zeros, no "-0").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BigInt {
    negative: bool,
    digits: String,
}

impl BigInt {
    pub fn zero() -> Self {
        Self::from_parts(false, "0")
    }

    pub fn from_u64(v: u64) -> Self {
        Self::from_parts(false, &v.to_string())
    }

    /// `negative` + ASCII decimal digits (leading zeros allowed). Digits must be non-empty.
    fn from_parts(negative: bool, digits: &str) -> Self {
        let trimmed = digits.trim_start_matches('0');
        if trimmed.is_empty() {
            return BigInt {
                negative: false,
                digits: "0".to_owned(),
            };
        }
        BigInt {
            negative,
            digits: trimmed.to_owned(),
        }
    }

    /// A JSON integer literal: `-?[0-9]+`.
    pub fn from_ascii_decimal(lit: &str) -> Option<Self> {
        let (negative, digits) = match lit.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, lit),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Self::from_parts(negative, digits))
    }

    /// Sign plus digits as collected by the `int(str)` parser.
    pub(crate) fn from_sign_digits(negative: bool, digits: &str) -> Self {
        Self::from_parts(negative, digits)
    }

    pub fn is_zero(&self) -> bool {
        self.digits == "0"
    }

    /// `int(x)` of a finite float: truncation toward zero, exactly. None for NaN or infinity.
    pub fn from_f64_trunc(x: f64) -> Option<Self> {
        if !x.is_finite() {
            return None;
        }
        let t = x.trunc();
        if t == 0.0 {
            return Some(Self::zero());
        }
        let bits = t.to_bits();
        let exp_bits = ((bits >> 52) & 0x7ff) as i32;
        let frac = bits & ((1u64 << 52) - 1);
        // t is a nonzero integer, so it is normal (subnormals are < 1).
        let mantissa = frac | (1u64 << 52);
        let mut exp = exp_bits - 1075; // t = mantissa * 2^exp
        let mut m = mantissa;
        while exp < 0 {
            // t is an integer, so the low bits being dropped are zero.
            m >>= 1;
            exp += 1;
        }
        // Little-endian base-1e9 limbs.
        const BASE: u64 = 1_000_000_000;
        let mut limbs: Vec<u64> = vec![m % BASE, (m / BASE) % BASE, m / (BASE * BASE)];
        let mut remaining = exp;
        while remaining > 0 {
            let step = remaining.min(29);
            remaining -= step;
            let mut carry = 0u64;
            for limb in limbs.iter_mut() {
                let v = (*limb << step) + carry;
                *limb = v % BASE;
                carry = v / BASE;
            }
            while carry > 0 {
                limbs.push(carry % BASE);
                carry /= BASE;
            }
        }
        while limbs.len() > 1 && limbs.last() == Some(&0) {
            limbs.pop();
        }
        let mut s = String::new();
        for (i, limb) in limbs.iter().rev().enumerate() {
            if i == 0 {
                s.push_str(&limb.to_string());
            } else {
                s.push_str(&format!("{limb:09}"));
            }
        }
        Some(Self::from_parts(t < 0.0, &s))
    }
}

impl fmt::Display for BigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.negative {
            f.write_str("-")?;
        }
        f.write_str(&self.digits)
    }
}
