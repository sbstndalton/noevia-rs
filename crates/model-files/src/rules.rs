//! The Python reference's string rules, character for character: `shard_key` (utils.py
//! `_SHARD_RE`), `infer_quant` (`_QUANT_RE`, IGNORECASE, then `.upper()`), the `.lower()`
//! suffix tests, and `int()` of a string. Character classes come from [`crate::pytables`].

use crate::bigint::BigInt;
use crate::pytables as t;

fn in_ranges(table: &[(u32, u32)], cp: u32) -> Option<(u32, u32)> {
    let i = table.partition_point(|&(_, hi)| hi < cp);
    match table.get(i) {
        Some(&(lo, hi)) if lo <= cp && cp <= hi => Some((lo, hi)),
        _ => None,
    }
}

/// `re` `\w`.
pub fn is_word(c: char) -> bool {
    in_ranges(t::WORD, c as u32).is_some()
}

/// The value of a `re` `\d` character (Unicode Nd), or None.
pub fn decimal_value(c: char) -> Option<u32> {
    let cp = c as u32;
    in_ranges(t::DECIMAL, cp).map(|(lo, _)| (cp - lo) % 10)
}

fn is_space(c: char) -> bool {
    t::SPACE.binary_search(&(c as u32)).is_ok()
}

fn folds(table: &[u32], c: char) -> bool {
    table.binary_search(&(c as u32)).is_ok()
}

fn literal_table(lit: char) -> &'static [u32] {
    match lit {
        'I' => t::FOLD_I,
        'Q' => t::FOLD_Q,
        'F' => t::FOLD_F,
        'B' => t::FOLD_B,
        'P' => t::FOLD_P,
        '1' => t::FOLD_1,
        '2' => t::FOLD_2,
        '3' => t::FOLD_3,
        '4' => t::FOLD_4,
        '6' => t::FOLD_6,
        '8' => t::FOLD_8,
        _ => t::FOLD_UNDERSCORE,
    }
}

/// `-NNNNN-of-NNNNN`.
const SHARD_SUFFIX_LEN: usize = 15;

/// `utils.shard_key`: (base name without `-NNNNN-of-NNNNN`, part index, part total).
///
/// The pattern's lookahead `(?=\.[^.]+$)` can only hold right before the last '.', when at
/// least one character follows it, so there is at most one match and it ends there.
pub fn shard_key(name: &str) -> (String, Option<u32>, Option<u32>) {
    let chars: Vec<char> = name.chars().collect();
    let parsed = chars.iter().rposition(|&c| c == '.').and_then(|dot| {
        if dot + 1 >= chars.len() || dot < SHARD_SUFFIX_LEN {
            return None;
        }
        let span = chars.get(dot - SHARD_SUFFIX_LEN..dot)?;
        let (head, tail) = span.split_at(6);
        let (mid, last) = tail.split_at(4);
        let (first, second) = (head.get(1..)?, last);
        if head.first() != Some(&'-') || mid != ['-', 'o', 'f', '-'] {
            return None;
        }
        let number = |ds: &[char]| -> Option<u32> {
            ds.iter()
                .try_fold(0u32, |acc, &c| Some(acc * 10 + decimal_value(c)?))
        };
        Some((dot, number(first)?, number(second)?))
    });
    match parsed {
        Some((dot, index, total)) => {
            let mut base: String = chars.iter().take(dot - SHARD_SUFFIX_LEN).collect();
            base.extend(chars.iter().skip(dot));
            (base, Some(index), Some(total))
        }
        None => (name.to_owned(), None, None),
    }
}

struct Quant<'a> {
    c: &'a [char],
}

impl Quant<'_> {
    fn word_at(&self, i: usize) -> bool {
        self.c.get(i).is_some_and(|&c| is_word(c))
    }

    /// `\b` at position `i` (between c[i-1] and c[i]).
    fn boundary(&self, i: usize) -> bool {
        let before = i > 0 && self.word_at(i - 1);
        before != self.word_at(i)
    }

    fn at(&self, i: usize, table: &[u32]) -> bool {
        self.c.get(i).is_some_and(|&c| folds(table, c))
    }

    /// `(?:_[A-Z0-9]+)*` from `q`, greedy, then `\b`: the first end Python's backtracking
    /// accepts, or None.
    fn tail(&self, q: usize) -> Option<usize> {
        if self.at(q, t::FOLD_UNDERSCORE) {
            let run = (q + 1..).take_while(|&i| self.at(i, t::CLASS_AZ09)).count();
            for len in (1..=run).rev() {
                if let Some(end) = self.tail(q + 1 + len) {
                    return Some(end);
                }
            }
        }
        self.boundary(q).then_some(q)
    }

    /// `I?Q\d+(?:_[A-Z0-9]+)*` then `\b`, from `start`.
    fn iq(&self, start: usize) -> Option<usize> {
        let with_i = [true, false];
        for has_i in with_i {
            if has_i && !self.at(start, t::FOLD_I) {
                continue;
            }
            let p = start + usize::from(has_i);
            if !self.at(p, t::FOLD_Q) {
                continue;
            }
            let p = p + 1;
            let run = self.c.get(p..).map_or(0, |r| {
                r.iter()
                    .take_while(|&&c| decimal_value(c).is_some())
                    .count()
            });
            for len in (1..=run).rev() {
                if let Some(end) = self.tail(p + len) {
                    return Some(end);
                }
            }
        }
        None
    }

    fn literal(&self, start: usize, lit: &str) -> Option<usize> {
        let mut i = start;
        for ch in lit.chars() {
            if !self.at(i, literal_table(ch)) {
                return None;
            }
            i += 1;
        }
        self.boundary(i).then_some(i)
    }

    fn search(&self) -> Option<(usize, usize)> {
        (0..=self.c.len()).find_map(|start| {
            if !self.boundary(start) {
                return None;
            }
            let end = self.iq(start).or_else(|| {
                ["F16", "F32", "BF16", "FP8", "FP4"]
                    .iter()
                    .find_map(|lit| self.literal(start, lit))
            })?;
            Some((start, end))
        })
    }
}

fn py_upper(c: char) -> String {
    if c.is_ascii() {
        return c.to_ascii_uppercase().to_string();
    }
    match t::UPPER.binary_search_by_key(&(c as u32), |&(cp, _)| cp) {
        Ok(i) => t::UPPER
            .get(i)
            .map_or_else(|| c.to_string(), |&(_, u)| u.to_owned()),
        // Nd digits (asserted by the generator to be their own upper case).
        Err(_) => c.to_string(),
    }
}

/// `hf.infer_quant`: the first quant label in `name`, upper-cased, or None.
pub fn infer_quant(name: &str) -> Option<String> {
    let chars: Vec<char> = name.chars().collect();
    let (start, end) = Quant { c: &chars }.search()?;
    Some(
        chars
            .get(start..end)?
            .iter()
            .map(|&c| py_upper(c))
            .collect(),
    )
}

/// `name.lower().endswith(suffix)` for an ASCII `suffix`. A character whose lower case holds
/// any non-ASCII character can never be part of an ASCII suffix (the generator checks that
/// such a lower case always ends in a non-ASCII character), so it stands in as None.
pub fn lower_ends_with(name: &str, suffix: &str) -> bool {
    let mut lowered: Vec<Option<char>> = Vec::new();
    for c in name.chars() {
        if c.is_ascii() {
            lowered.push(Some(c.to_ascii_lowercase()));
        } else if let Ok(i) = t::LOWER_ASCII.binary_search_by_key(&(c as u32), |&(cp, _)| cp) {
            if let Some(&(_, low)) = t::LOWER_ASCII.get(i) {
                lowered.extend(low.chars().map(Some));
            }
        } else {
            lowered.push(None);
        }
    }
    let want: Vec<Option<char>> = suffix.chars().map(Some).collect();
    lowered.ends_with(&want)
}

/// `model_files.is_support_file`.
pub fn is_support_file(path: &str) -> bool {
    [
        ".mmproj",
        "mmproj.gguf",
        "chat_template.jinja",
        "tokenizer.model",
    ]
    .iter()
    .any(|s| lower_ends_with(path, s))
}

/// `int(s)` for a Python str, or None where Python raises ValueError.
///
/// CPython first maps the string to ASCII (other chars below U+007F kept, Unicode spaces to ' ',
/// Nd digits to '0'..'9', anything else to an invalid byte), then parses base 10: optional
/// ASCII whitespace, an optional sign, digits with single underscores between them, optional
/// ASCII whitespace.
pub fn py_int_str(s: &str) -> Option<BigInt> {
    let mapped: Vec<u8> = s
        .chars()
        .map(|c| {
            if (c as u32) < 127 {
                c as u8
            } else if is_space(c) {
                b' '
            } else if let Some(d) = decimal_value(c) {
                b'0' + d as u8
            } else {
                b'?'
            }
        })
        .collect();
    let is_ws = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let start = mapped.iter().position(|b| !is_ws(b))?;
    let end = mapped.iter().rposition(|b| !is_ws(b))? + 1;
    let body = mapped.get(start..end)?;
    let (negative, body) = match body.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, body),
    };
    let mut digits = String::new();
    let mut prev_digit = false;
    for &b in body {
        match b {
            b'0'..=b'9' => {
                digits.push(b as char);
                prev_digit = true;
            }
            b'_' if prev_digit => prev_digit = false,
            _ => return None,
        }
    }
    if digits.is_empty() || !prev_digit {
        return None;
    }
    Some(BigInt::from_sign_digits(negative, &digits))
}
