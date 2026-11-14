//! The recorder's `methodsNear` (tools/gen-node-refusal-stubs.cjs, council p7) in plain Rust (council p12).
//!
//! For one `behind` hit line, the request methods that line itself compares the method with
//! (`req.method === 'PUT'`, `method == "GET"`, `'POST' === req.method`, `['GET', 'HEAD'].includes(req.method)`).
//! A negation on the line (`method !== 'X'`, `'X' != req.method`) or no comparison gives `None`, and
//! [`crate::handlers_behind`] then counts the line for every method (the safe side).
//!
//! The recorder's five regexes are hand-ported as byte scanners with JavaScript `matchAll` semantics (each match
//! starts at the leftmost position at or after the previous match's end), so the crate needs no regex dependency and
//! Cargo.lock stays unchanged:
//! - negation 1: `/method\s*!==?\s*['"]/`
//! - negation 2: `/['"][A-Z]+['"]\s*!==?\s*(?:req\.)?method/`
//! - named 1: `/method\s*===?\s*['"]([A-Z]+)['"]/g`
//! - named 2: `/['"]([A-Z]+)['"]\s*===?\s*(?:req\.)?method\b/g`
//! - list: `/\[([^\]]*)\]\.includes\((?:req\.)?method\)/g`, then `/['"]([A-Z]+)['"]/g` inside the brackets.
//!
//! One known difference: `\s` here is ASCII whitespace (space, tab, CR, VT, FF; lines never hold LF). JavaScript's
//! `\s` also takes Unicode spaces (NBSP, U+FEFF, …), which core's sources do not put next to a method comparison.

use std::collections::BTreeSet;

/// The methods the recorder probes (`METHODS`); a named method outside this list is ignored, as there.
pub const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

fn space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | 0x0b | 0x0c)
}

fn quote(b: u8) -> bool {
    b == b'\'' || b == b'"'
}

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn at(s: &[u8], i: usize) -> Option<u8> {
    s.get(i).copied()
}

fn skip_space(s: &[u8], mut i: usize) -> usize {
    while at(s, i).is_some_and(space) {
        i += 1;
    }
    i
}

fn lit(s: &[u8], i: usize, l: &[u8]) -> Option<usize> {
    s.get(i..i + l.len())
        .filter(|w| *w == l)
        .map(|_| i + l.len())
}

/// `[A-Z]+` from `i` (greedy); the end and the text.
fn caps(s: &[u8], i: usize) -> Option<(usize, String)> {
    let mut j = i;
    while at(s, j).is_some_and(|b| b.is_ascii_uppercase()) {
        j += 1;
    }
    let text = s.get(i..j)?;
    (j > i).then(|| (j, String::from_utf8_lossy(text).into_owned()))
}

/// `!==?` (when `negated`) or `===?` from `i`.
fn compare(s: &[u8], i: usize, negated: bool) -> Option<usize> {
    let i = lit(s, i, if negated { b"!=" } else { b"==" })?;
    Some(lit(s, i, b"=").unwrap_or(i))
}

/// `(?:req\.)?method` from `i`. JavaScript tries `req.` first and falls back to the bare word, which for a fixed
/// literal is the same as taking `req.` when it is there.
fn req_method(s: &[u8], i: usize) -> Option<usize> {
    let i = lit(s, i, b"req.").unwrap_or(i);
    lit(s, i, b"method")
}

/// `method\s*(!==?|===?)\s*['"]`; the index after the quote.
fn method_compare_quote(s: &[u8], i: usize, negated: bool) -> Option<usize> {
    let i = lit(s, i, b"method")?;
    let i = compare(s, skip_space(s, i), negated)?;
    let i = skip_space(s, i);
    at(s, i).filter(|b| quote(*b)).map(|_| i + 1)
}

/// `['"]([A-Z]+)['"]\s*(!==?|===?)\s*(?:req\.)?method`; the end and the method.
fn quoted_compare_method(s: &[u8], i: usize, negated: bool) -> Option<(usize, String)> {
    at(s, i).filter(|b| quote(*b))?;
    let (j, name) = caps(s, i + 1)?;
    at(s, j).filter(|b| quote(*b))?;
    let j = compare(s, skip_space(s, j + 1), negated)?;
    let j = req_method(s, skip_space(s, j))?;
    Some((j, name))
}

/// Every match of `try_at` in `s`, JavaScript `matchAll` style: leftmost first, the next search from the match's end
/// (one past the start for an empty match, which none of these patterns can give).
fn match_all<T>(s: &[u8], try_at: impl Fn(&[u8], usize) -> Option<(usize, T)>) -> Vec<T> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        match try_at(s, i) {
            Some((end, v)) => {
                out.push(v);
                i = end.max(i + 1);
            }
            None => i += 1,
        }
    }
    out
}

/// Whether the line holds a negated method comparison (either negation regex).
pub fn negated(line: &str) -> bool {
    let s = line.as_bytes();
    (0..s.len()).any(|i| {
        method_compare_quote(s, i, true).is_some() || quoted_compare_method(s, i, true).is_some()
    })
}

/// Every method the line names in a comparison, before the [`METHODS`] filter.
pub fn named(line: &str) -> BTreeSet<String> {
    let s = line.as_bytes();
    let mut out = BTreeSet::new();
    // named 1: the capture follows the quote.
    out.extend(match_all(s, |s, i| {
        let j = method_compare_quote(s, i, false)?;
        let (k, name) = caps(s, j)?;
        at(s, k).filter(|b| quote(*b))?;
        Some((k + 1, name))
    }));
    // named 2: `\b` after `method`.
    out.extend(match_all(s, |s, i| {
        let (j, name) = quoted_compare_method(s, i, false)?;
        (!at(s, j).is_some_and(word)).then_some((j, name))
    }));
    // list: `[...]` (no `]` inside) directly followed by `.includes(` method `)`.
    for inner in match_all(s, |s, i| {
        if at(s, i) != Some(b'[') {
            return None;
        }
        let close = i + 1 + s.get(i + 1..)?.iter().position(|b| *b == b']')?;
        let j = lit(s, close + 1, b".includes(")?;
        let j = lit(s, req_method(s, j)?, b")")?;
        Some((j, s.get(i + 1..close)?.to_vec()))
    }) {
        out.extend(match_all(&inner, |s, i| {
            at(s, i).filter(|b| quote(*b))?;
            let (j, name) = caps(s, i + 1)?;
            at(s, j).filter(|b| quote(*b))?;
            Some((j + 1, name))
        }));
    }
    out
}

/// The recorder's `methodsNear` for one line: the known methods it compares with, sorted, or `None` when it negates
/// one or names none.
pub fn methods_near(line: &str) -> Option<Vec<String>> {
    if negated(line) {
        return None;
    }
    let known: Vec<String> = named(line)
        .into_iter()
        .filter(|m| METHODS.contains(&m.as_str()))
        .collect();
    (!known.is_empty()).then_some(known)
}

#[cfg(test)]
#[path = "methods_near_tests.rs"]
mod tests;
