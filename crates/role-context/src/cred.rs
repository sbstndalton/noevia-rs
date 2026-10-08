//! role-context.cjs `CREDENTIAL_PATTERNS`, matched by hand in linear time over UTF-16 code units
//! with the semantics V8 gives them (no u flag; `i` folds ASCII letters only, since ECMAScript's
//! non-unicode Canonicalize never maps a non-ASCII unit to an ASCII one):
//!
//! 1. `/(?<![A-Za-z0-9])sk-(?=[A-Za-z0-9_-]*\d)[A-Za-z0-9_-]{16,}/`
//! 2. `/(?<![A-Za-z])bearer\s+(?=[A-Za-z0-9._~+/-]*\d)[A-Za-z0-9._~+/-]{16,}/i`
//! 3. `/gh[pousr]_[A-Za-z0-9]{20,}/`
//! 4. `/AKIA[0-9A-Z]{16}/i`
//! 5. `/-----BEGIN [A-Z ]*PRIVATE KEY-----/i`
//! 6. `/xox[abpr]-[A-Za-z0-9-]{10,}/`
//!
//! Each greedy run is maximal (its class never contains what follows it in the pattern), so a
//! match at a start index is decided from precomputed run ends.

use crate::text::Budget;
use crate::Refusal;
use prompt_framing::js::is_js_space;

fn ascii(c: u16) -> Option<u8> {
    u8::try_from(c).ok().filter(u8::is_ascii)
}
fn alnum(c: u16) -> bool {
    ascii(c).is_some_and(|b| b.is_ascii_alphanumeric())
}
fn letter(c: u16) -> bool {
    ascii(c).is_some_and(|b| b.is_ascii_alphabetic())
}
fn digit(c: u16) -> bool {
    ascii(c).is_some_and(|b| b.is_ascii_digit())
}
fn class_sk(c: u16) -> bool {
    alnum(c) || c == u16::from(b'_') || c == u16::from(b'-')
}
fn class_bearer(c: u16) -> bool {
    alnum(c) || ascii(c).is_some_and(|b| b".-_~+/".contains(&b))
}
fn class_pem(c: u16) -> bool {
    letter(c) || c == 0x20
}
fn class_xox(c: u16) -> bool {
    alnum(c) || c == u16::from(b'-')
}

/// `run[i]`: the end of the run of `class` starting at `i` (`i` itself when `s[i]` is outside).
fn runs(s: &[u16], class: impl Fn(u16) -> bool) -> Vec<usize> {
    let mut out = vec![s.len(); s.len() + 1];
    for i in (0..s.len()).rev() {
        let end = if s.get(i).is_some_and(|&c| class(c)) {
            out.get(i + 1).copied().unwrap_or(s.len())
        } else {
            i
        };
        if let Some(slot) = out.get_mut(i) {
            *slot = end;
        }
    }
    out
}

/// `next[i]`: the first digit at or after `i`.
fn next_digit(s: &[u16]) -> Vec<usize> {
    let mut out = vec![s.len(); s.len() + 1];
    for i in (0..s.len()).rev() {
        let v = if s.get(i).is_some_and(|&c| digit(c)) {
            i
        } else {
            out.get(i + 1).copied().unwrap_or(s.len())
        };
        if let Some(slot) = out.get_mut(i) {
            *slot = v;
        }
    }
    out
}

fn lit(s: &[u16], at: usize, word: &[u8], fold: bool) -> bool {
    word.iter().enumerate().all(|(k, &w)| {
        s.get(at + k).and_then(|&c| ascii(c)).is_some_and(|b| {
            if fold {
                b.eq_ignore_ascii_case(&w)
            } else {
                b == w
            }
        })
    })
}

/// The six patterns over one string.
struct Scanner<'a> {
    s: &'a [u16],
    digits: Vec<usize>,
    run: Vec<usize>,
    spaces: Vec<usize>,
}

impl<'a> Scanner<'a> {
    fn new(s: &'a [u16], pattern: usize) -> Self {
        let run = match pattern {
            0 => runs(s, class_sk),
            1 => runs(s, class_bearer),
            2 => runs(s, alnum),
            4 => runs(s, class_pem),
            5 => runs(s, class_xox),
            _ => Vec::new(),
        };
        let digits = if pattern <= 1 {
            next_digit(s)
        } else {
            Vec::new()
        };
        let spaces = if pattern == 1 {
            runs(s, is_js_space)
        } else {
            Vec::new()
        };
        Scanner {
            s,
            digits,
            run,
            spaces,
        }
    }

    fn run_end(&self, i: usize) -> usize {
        self.run.get(i).copied().unwrap_or(i)
    }
    fn digit_in(&self, a: usize, b: usize) -> bool {
        self.digits.get(a).is_some_and(|&d| d < b)
    }

    /// The end of a match of `pattern` starting at `p`.
    fn at(&self, pattern: usize, p: usize) -> Option<usize> {
        let s = self.s;
        let prev = p.checked_sub(1).and_then(|i| s.get(i)).copied();
        match pattern {
            0 => {
                if !lit(s, p, b"sk-", false) || prev.is_some_and(alnum) {
                    return None;
                }
                let q = p + 3;
                let e = self.run_end(q);
                (e - q >= 16 && self.digit_in(q, e)).then_some(e)
            }
            1 => {
                if !lit(s, p, b"bearer", true) || prev.is_some_and(letter) {
                    return None;
                }
                let q = p + 6;
                let sp = self.spaces.get(q).copied().unwrap_or(q);
                if sp == q {
                    return None;
                }
                let e = self.run_end(sp);
                (e - sp >= 16 && self.digit_in(sp, e)).then_some(e)
            }
            2 => {
                if !lit(s, p, b"gh", false)
                    || !s
                        .get(p + 2)
                        .and_then(|&c| ascii(c))
                        .is_some_and(|b| b"pousr".contains(&b))
                    || s.get(p + 3) != Some(&u16::from(b'_'))
                {
                    return None;
                }
                let q = p + 4;
                let e = self.run_end(q);
                (e - q >= 20).then_some(e)
            }
            3 => {
                if !lit(s, p, b"akia", true) {
                    return None;
                }
                (4..20)
                    .all(|k| s.get(p + k).is_some_and(|&c| alnum(c)))
                    .then_some(p + 20)
            }
            4 => {
                if !lit(s, p, b"-----begin ", true) {
                    return None;
                }
                let q = p + 11;
                let e = self.run_end(q);
                (e - q >= 11 && lit(s, e - 11, b"private key", true) && lit(s, e, b"-----", false))
                    .then_some(e + 5)
            }
            5 => {
                if !lit(s, p, b"xox", false)
                    || !s
                        .get(p + 3)
                        .and_then(|&c| ascii(c))
                        .is_some_and(|b| b"abpr".contains(&b))
                    || s.get(p + 4) != Some(&u16::from(b'-'))
                {
                    return None;
                }
                let q = p + 5;
                let e = self.run_end(q);
                (e - q >= 10).then_some(e)
            }
            _ => None,
        }
    }
}

pub const PATTERNS: usize = 6;

/// `CREDENTIAL_PATTERNS[pattern].test(s)`.
pub fn test(pattern: usize, s: &[u16], budget: &mut Budget) -> Result<bool, Refusal> {
    budget.spend(s.len() * 4 + 1)?;
    let sc = Scanner::new(s, pattern);
    Ok((0..s.len()).any(|p| sc.at(pattern, p).is_some()))
}

/// `s.replace(new RegExp(pattern, 'g…'), () => { n += 1; return with; })`.
pub fn replace(
    pattern: usize,
    s: &[u16],
    with: &[u16],
    budget: &mut Budget,
) -> Result<(Vec<u16>, usize), Refusal> {
    budget.spend(s.len() * 4 + 1)?;
    let sc = Scanner::new(s, pattern);
    let mut out = Vec::with_capacity(s.len());
    let mut n = 0;
    let mut p = 0;
    let mut copied = 0;
    while p < s.len() {
        if let Some(e) = sc.at(pattern, p) {
            out.extend(s.get(copied..p).unwrap_or(&[]));
            out.extend(with);
            n += 1;
            p = e;
            copied = e;
        } else {
            p += 1;
        }
    }
    out.extend(s.get(copied..).unwrap_or(&[]));
    Ok((out, n))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::text::units;

    fn t(p: usize, s: &str) -> bool {
        test(p, &units(s), &mut Budget::new(1 << 20)).unwrap()
    }

    #[test]
    fn patterns() {
        assert!(t(0, "key sk-test0000CANARY1111key2222"));
        assert!(!t(0, "ask-test0000CANARY1111key2222"));
        assert!(!t(0, "sk-abcdefghijklmnopqrstu"));
        assert!(t(1, "Authorization: BEARER  abcDEF0123456789CANARYtoken"));
        assert!(!t(1, "xbearer abcDEF0123456789CANARYtoken"));
        assert!(!t(1, "bearer abcdefghijklmnopqrstuvwxyz"));
        assert!(t(2, "ghp_abcdefghijklmnopqrst"));
        assert!(!t(2, "ghp_abcdefghijklmnopqrs"));
        assert!(t(3, "akiaabcdefghijklmnop"));
        assert!(!t(3, "AKIAABCDEFGHIJKLMNO"));
        assert!(t(4, "-----BEGIN RSA PRIVATE KEY-----"));
        assert!(t(4, "-----begin private key-----"));
        assert!(!t(4, "-----BEGIN PRIVATE KEY----"));
        assert!(!t(4, "-----BEGIN RSA-PRIVATE KEY-----"));
        assert!(t(5, "xoxb-1234567890"));
        assert!(!t(5, "xoxc-1234567890"));
        let (out, n) = replace(
            0,
            &units("a sk-aaaaaaaaaaaaaaa1 b sk-bbbbbbbbbbbbbbb2"),
            &units("[R]"),
            &mut Budget::new(1 << 20),
        )
        .unwrap();
        assert_eq!(
            (String::from_utf16(&out).unwrap().as_str(), n),
            ("a [R] b [R]", 2)
        );
    }
}
