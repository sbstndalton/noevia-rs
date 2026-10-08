//! The string operations role-context.cjs uses, over UTF-16 code units as V8 performs them:
//! `normalize('NFC'|'NFKC')`, `toLowerCase`, `/[\p{Cf}­]/gu`, `\s`, `Array.from` code points,
//! the JSON escape costs of `JSON.stringify`, `escapeNonAscii`, `decodeLiteralEscapes` and
//! `String.prototype.includes`/`split` as a linear (KMP) search charged to a [`Budget`].

use crate::Refusal;
use icu_properties::props::{GeneralCategory, GeneralCategoryGroup};
use icu_properties::CodePointMapData;
use prompt_framing::js::is_js_space;
pub use prompt_framing::js::{nfkc, to_lower, units};

/// Work left for one call, in code units scanned; past it the port refuses (`too_large`).
pub struct Budget(u64);

impl Budget {
    pub fn new(units: u64) -> Self {
        Budget(units)
    }
    pub fn spend(&mut self, units: usize) -> Result<(), Refusal> {
        let n = u64::try_from(units).unwrap_or(u64::MAX);
        if n > self.0 {
            self.0 = 0;
            return Err(Refusal::TooLarge);
        }
        self.0 -= n;
        Ok(())
    }
}

pub const fn is_high(c: u16) -> bool {
    matches!(c, 0xd800..=0xdbff)
}
pub const fn is_low(c: u16) -> bool {
    matches!(c, 0xdc00..=0xdfff)
}

/// Whether `s` holds a lone surrogate (JS strings may; the port refuses them, see the crate docs).
pub fn has_lone_surrogate(s: &[u16]) -> bool {
    let mut i = 0;
    while let Some(&c) = s.get(i) {
        if is_high(c) && s.get(i + 1).is_some_and(|&d| is_low(d)) {
            i += 2;
            continue;
        }
        if is_high(c) || is_low(c) {
            return true;
        }
        i += 1;
    }
    false
}

/// `Array.from(s)`: code points, each lone surrogate as itself.
pub fn code_points(s: &[u16]) -> Vec<u32> {
    char::decode_utf16(s.iter().copied())
        .map(|r| r.map_or_else(|e| u32::from(e.unpaired_surrogate()), u32::from))
        .collect()
}

/// `Array.from(s).length`.
pub fn cp_len(s: &[u16]) -> usize {
    char::decode_utf16(s.iter().copied()).count()
}

/// The units of the first `n` code points of `s`.
pub fn cp_prefix(s: &[u16], n: usize) -> &[u16] {
    let mut units = 0;
    for r in char::decode_utf16(s.iter().copied()).take(n) {
        units += r.map_or(1, char::len_utf16);
    }
    s.get(..units).unwrap_or(s)
}

/// Apply `f` to every well-formed run of `s`, keeping lone surrogates as they are.
fn per_run(s: &[u16], f: impl Fn(&str) -> String) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    let mut run = String::new();
    for r in char::decode_utf16(s.iter().copied()) {
        match r {
            Ok(c) => run.push(c),
            Err(e) => {
                out.extend(f(&run).encode_utf16());
                run.clear();
                out.push(e.unpaired_surrogate());
            }
        }
    }
    out.extend(f(&run).encode_utf16());
    out
}

/// `String.prototype.normalize('NFC')`.
pub fn nfc(s: &[u16]) -> Vec<u16> {
    if s.iter().all(|&c| c < 0x80) {
        return s.to_vec();
    }
    let n = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    per_run(s, |run| n.normalize(run).into_owned())
}

fn gc(cp: u32) -> GeneralCategory {
    CodePointMapData::<GeneralCategory>::new().get32(cp)
}

/// `/[\p{Cf}­]/u` for one code point.
pub fn is_format(cp: u32) -> bool {
    cp == 0xad || (cp >= 0x80 && GeneralCategoryGroup::Format.contains(gc(cp)))
}

/// `[\p{L}\p{N}_]` (u mode) for one code point.
pub fn is_word(cp: u32) -> bool {
    if cp < 0x80 {
        return u8::try_from(cp).is_ok_and(|b| b.is_ascii_alphanumeric() || b == b'_');
    }
    let g = gc(cp);
    GeneralCategoryGroup::Letter.contains(g) || GeneralCategoryGroup::Number.contains(g)
}

/// `text.replace(/[\p{Cf}­]/gu, '')`.
pub fn strip_format(s: &[u16]) -> Vec<u16> {
    if s.iter().all(|&c| c < 0x80) {
        return s.to_vec();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while let Some(&c) = s.get(i) {
        let pair = is_high(c) && s.get(i + 1).is_some_and(|&d| is_low(d));
        let (cp, n) = if pair {
            let d = s.get(i + 1).copied().unwrap_or(0);
            (
                0x10000 + ((u32::from(c) - 0xd800) << 10) + (u32::from(d) - 0xdc00),
                2,
            )
        } else {
            (u32::from(c), 1)
        };
        // A lone surrogate is a code point of category Cs in u mode: kept.
        if !(is_format(cp) && !(n == 1 && (is_high(c) || is_low(c)))) {
            out.extend(s.get(i..i + n).unwrap_or(&[]));
        }
        i += n;
    }
    out
}

/// role-context.cjs `fold`: strip, NFKC, strip, lower case, each run of `\s` as one space.
pub fn fold(s: &[u16]) -> Vec<u16> {
    let lowered = to_lower(&strip_format(&nfkc(&strip_format(s))));
    let mut out = Vec::with_capacity(lowered.len());
    let mut in_space = false;
    for c in lowered {
        if is_js_space(c) {
            if !in_space {
                out.push(0x20);
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `String.prototype.trim`.
pub fn trim(s: &[u16]) -> &[u16] {
    prompt_framing::js::trim(s)
}

/// `/^\S{1,64}$/.test(s)` (no u flag: code units).
pub fn is_structured_token(s: &[u16]) -> bool {
    (1..=64).contains(&s.len()) && !s.iter().any(|&c| is_js_space(c))
}

/// `Array.from(JSON.stringify(s)).length - 2`.
pub fn serialised_cost(s: &[u16]) -> usize {
    let mut cost = 0;
    let mut i = 0;
    while let Some(&c) = s.get(i) {
        if is_high(c) && s.get(i + 1).is_some_and(|&d| is_low(d)) {
            cost += 1;
            i += 2;
            continue;
        }
        cost += match c {
            0x22 | 0x5c | 0x08 | 0x09 | 0x0a | 0x0c | 0x0d => 2,
            0..=0x1f => 6,
            _ if is_high(c) || is_low(c) => 6,
            _ => 1,
        };
        i += 1;
    }
    cost
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn push_u_escape(out: &mut Vec<u16>, c: u16) {
    out.extend([0x5c, 0x75]);
    for shift in [12u16, 8, 4, 0] {
        let nibble = usize::from((c >> shift) & 0xf);
        out.push(u16::from(HEX.get(nibble).copied().unwrap_or(b'0')));
    }
}

/// `text.replace(/[\u007f-￿]/g, (c) => '\\u' + hex4(c))`.
pub fn escape_non_ascii(s: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    for &c in s {
        if c >= 0x7f {
            push_u_escape(&mut out, c);
        } else {
            out.push(c);
        }
    }
    out
}

/// `JSON.stringify(s).slice(1, -1)`.
pub fn json_body(s: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len() + 2);
    for &c in s {
        match c {
            0x22 => out.extend([0x5c, 0x22]),
            0x5c => out.extend([0x5c, 0x5c]),
            0x08 => out.extend([0x5c, 0x62]),
            0x09 => out.extend([0x5c, 0x74]),
            0x0a => out.extend([0x5c, 0x6e]),
            0x0c => out.extend([0x5c, 0x66]),
            0x0d => out.extend([0x5c, 0x72]),
            0..=0x1f => push_u_escape(&mut out, c),
            _ => out.push(c),
        }
    }
    // Lone surrogates: JSON.stringify writes them as \udxxx.
    let mut fixed = Vec::with_capacity(out.len());
    let mut i = 0;
    while let Some(&c) = out.get(i) {
        if is_high(c) && out.get(i + 1).is_some_and(|&d| is_low(d)) {
            fixed.extend(out.get(i..i + 2).unwrap_or(&[]));
            i += 2;
        } else if is_high(c) || is_low(c) {
            push_u_escape(&mut fixed, c);
            i += 1;
        } else {
            fixed.push(c);
            i += 1;
        }
    }
    fixed
}

const fn hex_value(c: u16) -> Option<u16> {
    match c {
        0x30..=0x39 => Some(c - 0x30),
        0x41..=0x46 => Some(c - 0x41 + 10),
        0x61..=0x66 => Some(c - 0x61 + 10),
        _ => None,
    }
}

/// `text.replace(/\\u([0-9a-fA-F]{4})/g, (_m, hex) => String.fromCharCode(parseInt(hex, 16)))`.
pub fn decode_literal_escapes(s: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while let Some(&c) = s.get(i) {
        if c == 0x5c && s.get(i + 1) == Some(&0x75) {
            let mut v: u16 = 0;
            let mut ok = true;
            for k in 0..4 {
                match s.get(i + 2 + k).copied().and_then(hex_value) {
                    Some(h) => v = (v << 4) | h,
                    None => ok = false,
                }
            }
            if ok {
                out.push(v);
                i += 6;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// A needle prepared for linear search (Knuth-Morris-Pratt).
pub struct Needle<'a> {
    pub units: &'a [u16],
    fail: Vec<usize>,
}

impl<'a> Needle<'a> {
    pub fn new(units: &'a [u16]) -> Self {
        let mut fail = vec![0usize; units.len()];
        let mut k = 0;
        for i in 1..units.len() {
            while k > 0 && units.get(i) != units.get(k) {
                k = fail.get(k - 1).copied().unwrap_or(0);
            }
            if units.get(i) == units.get(k) {
                k += 1;
            }
            if let Some(slot) = fail.get_mut(i) {
                *slot = k;
            }
        }
        Needle { units, fail }
    }

    /// Start indices of matches in `hay`, overlapping (`overlap`) or as `split` finds them, each
    /// passed to `f`; stops early when `f` returns true (and returns true).
    pub fn scan(
        &self,
        hay: &[u16],
        overlap: bool,
        budget: &mut Budget,
        mut f: impl FnMut(usize) -> bool,
    ) -> Result<bool, Refusal> {
        let m = self.units.len();
        if m == 0 || m > hay.len() {
            return Ok(false);
        }
        budget.spend(hay.len() + m)?;
        let mut k = 0;
        for (i, c) in hay.iter().enumerate() {
            while k > 0 && self.units.get(k) != Some(c) {
                k = self.fail.get(k - 1).copied().unwrap_or(0);
            }
            if self.units.get(k) == Some(c) {
                k += 1;
            }
            if k == m {
                if f(i + 1 - m) {
                    return Ok(true);
                }
                k = if overlap {
                    self.fail.get(m - 1).copied().unwrap_or(0)
                } else {
                    0
                };
            }
        }
        Ok(false)
    }

    /// `hay.includes(needle)` (an empty needle is never searched for; callers handle it).
    pub fn found_in(&self, hay: &[u16], budget: &mut Budget) -> Result<bool, Refusal> {
        self.scan(hay, false, budget, |_| true)
    }
}

/// `hay.includes(needle)`, with `''` included everywhere.
pub fn includes(hay: &[u16], needle: &[u16], budget: &mut Budget) -> Result<bool, Refusal> {
    if needle.is_empty() {
        return Ok(true);
    }
    Needle::new(needle).found_in(hay, budget)
}

/// `hay.split(needle).join(with)` and the number of pieces minus one.
pub fn replace_all(
    hay: &[u16],
    needle: &[u16],
    with: &[u16],
    budget: &mut Budget,
) -> Result<(Vec<u16>, usize), Refusal> {
    let n = Needle::new(needle);
    let mut starts = Vec::new();
    n.scan(hay, false, budget, |i| {
        starts.push(i);
        false
    })?;
    if starts.is_empty() {
        return Ok((hay.to_vec(), 0));
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut at = 0;
    for &s in &starts {
        out.extend(hay.get(at..s).unwrap_or(&[]));
        out.extend(with);
        at = s + needle.len();
    }
    out.extend(hay.get(at..).unwrap_or(&[]));
    Ok((out, starts.len()))
}

/// The code point ending just before `i` in `s` (a pair read whole), if any.
pub fn cp_before(s: &[u16], i: usize) -> Option<u32> {
    let c = *s.get(i.checked_sub(1)?)?;
    if is_low(c) && i >= 2 {
        if let Some(&h) = s.get(i - 2) {
            if is_high(h) {
                return Some(0x10000 + ((u32::from(h) - 0xd800) << 10) + (u32::from(c) - 0xdc00));
            }
        }
    }
    Some(u32::from(c))
}

/// The code point starting at `i` in `s` (a pair read whole), if any.
pub fn cp_at(s: &[u16], i: usize) -> Option<u32> {
    let c = *s.get(i)?;
    if is_high(c) {
        if let Some(&d) = s.get(i + 1) {
            if is_low(d) {
                return Some(0x10000 + ((u32::from(c) - 0xd800) << 10) + (u32::from(d) - 0xdc00));
            }
        }
    }
    Some(u32::from(c))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(
            serialised_cost(&units("{\"path\":\"\",\"patch\":\"\"},")),
            31
        );
        assert_eq!(serialised_cost(&units("…[truncated]")), 12);
        assert_eq!(serialised_cost(&units("\t\u{1}a😀")), 2 + 6 + 1 + 1);
        assert_eq!(
            escape_non_ascii(&units("a\u{7f}é")),
            units("a\\u007f\\u00e9")
        );
        assert_eq!(
            decode_literal_escapes(&units("\\\\u0041x\\u00zz")),
            units("\\Ax\\u00zz")
        );
        assert_eq!(fold(&units(" A\u{200b}B \t\n C")), units(" ab c"));
        assert_eq!(
            json_body(&units("a\"\\\n\u{1}")),
            units("a\\\"\\\\\\n\\u0001")
        );
        assert!(is_word(u32::from('é')) && is_word(u32::from('٣')) && !is_word(u32::from('-')));
        assert!(is_format(0x200b) && is_format(0xad) && is_format(0xe0001) && !is_format(0x41));
        assert!(has_lone_surrogate(&[0x41, 0xd800]) && !has_lone_surrogate(&units("😀")));
        let mut b = Budget::new(1000);
        assert_eq!(
            replace_all(&units("aaaa"), &units("aa"), &units("X"), &mut b).unwrap(),
            (units("XX"), 2)
        );
        let mut hits = Vec::new();
        Needle::new(&units("aa"))
            .scan(&units("aaaa"), true, &mut b, |i| {
                hits.push(i);
                false
            })
            .unwrap();
        assert_eq!(hits, [0, 1, 2]);
        assert_eq!(cp_prefix(&units("a😀b"), 2), units("a😀").as_slice());
    }
}
