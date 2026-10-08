//! ECMAScript string operations over UTF-16 code units, as V8 performs them: `trim`, `\s`,
//! `toLowerCase`, `normalize('NFKC')`, `indexOf`, `includes`, `decodeURIComponent`. Lone
//! surrogates pass through unchanged (a JS string may hold them); the Unicode-aware operations
//! work on each well-formed run between them, which is what V8/ICU do (a lone surrogate has no
//! case or decomposition, is a starter and ends any casing context).

pub use mcp_frame::is_js_space;

/// `s` as UTF-16 code units.
pub fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `String.prototype.trim`.
pub fn trim(s: &[u16]) -> &[u16] {
    let start = s.iter().position(|&c| !is_js_space(c)).unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|&c| !is_js_space(c))
        .map_or(start, |i| i + 1);
    s.get(start..end).unwrap_or(&[])
}

/// `hay.indexOf(needle, from)`.
pub fn index_of(hay: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return (from <= hay.len()).then_some(from);
    }
    let first = *needle.first()?;
    let last_start = hay.len().checked_sub(needle.len())?;
    let mut i = from;
    while i <= last_start {
        if hay.get(i) == Some(&first) && hay.get(i..i + needle.len()) == Some(needle) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `hay.includes(needle)`.
pub fn includes(hay: &[u16], needle: &[u16]) -> bool {
    index_of(hay, needle, 0).is_some()
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

/// `String.prototype.toLowerCase` (full mappings and Final_Sigma, like ICU's root locale).
pub fn to_lower(s: &[u16]) -> Vec<u16> {
    if s.iter().all(|&c| c < 0x80) {
        return s
            .iter()
            .map(|&c| c | u16::from((0x41..=0x5a).contains(&c)) << 5)
            .collect();
    }
    per_run(s, str::to_lowercase)
}

/// `String.prototype.normalize('NFKC')`.
pub fn nfkc(s: &[u16]) -> Vec<u16> {
    if s.iter().all(|&c| c < 0x80) {
        return s.to_vec();
    }
    let n = icu_normalizer::ComposingNormalizerBorrowed::new_nfkc();
    per_run(s, |run| n.normalize(run).into_owned())
}

/// A JS string as a Rust string, each lone surrogate replaced with U+FFFD (WebIDL USVString,
/// what `new URL()` and Node's bindings see).
pub fn lossy(s: &[u16]) -> String {
    String::from_utf16_lossy(s)
}

const fn hex(c: u16) -> Option<u8> {
    match c {
        0x30..=0x39 => Some((c - 0x30) as u8),
        0x41..=0x46 => Some((c - 0x41 + 10) as u8),
        0x61..=0x66 => Some((c - 0x61 + 10) as u8),
        _ => None,
    }
}

/// The byte of `%XX` at `s[k..k + 3]`.
fn pct(s: &[u16], k: usize) -> Option<u8> {
    if s.get(k) != Some(&0x25) {
        return None;
    }
    Some(hex(*s.get(k + 1)?)? << 4 | hex(*s.get(k + 2)?)?)
}

/// `decodeURIComponent` (ECMA-262 Decode with an empty reserved set); `None` where it throws a
/// URIError.
pub fn decode_uri_component(s: &[u16]) -> Option<Vec<u16>> {
    let mut out = Vec::with_capacity(s.len());
    let mut k = 0;
    while let Some(&c) = s.get(k) {
        if c != 0x25 {
            out.push(c);
            k += 1;
            continue;
        }
        let b = pct(s, k)?;
        k += 3;
        if b < 0x80 {
            out.push(u16::from(b));
            continue;
        }
        let n = b.leading_ones() as usize;
        if n == 1 || n > 4 {
            return None;
        }
        let mut octets = vec![b];
        for _ in 1..n {
            let b = pct(s, k)?;
            if b & 0xc0 != 0x80 {
                return None;
            }
            octets.push(b);
            k += 3;
        }
        let text = std::str::from_utf8(&octets).ok()?;
        out.extend(text.encode_utf16());
    }
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn s(u: &[u16]) -> String {
        String::from_utf16(u).unwrap()
    }

    #[test]
    fn string_ops() {
        assert_eq!(s(trim(&units("\u{feff} a b\u{3000}\n"))), "a b");
        assert_eq!(trim(&units(" \t ")), &[] as &[u16]);
        assert_eq!(s(&to_lower(&units("ABC ΑΣ İ K"))), "abc ας i\u{307} k");
        assert_eq!(to_lower(&[0x41, 0xd800, 0x3a3]), vec![0x61, 0xd800, 0x3c3]);
        assert_eq!(s(&nfkc(&units("ﬁ①Ａ"))), "fi1A");
        assert_eq!(index_of(&units("abcabc"), &units("ca"), 0), Some(2));
        assert_eq!(index_of(&units("ab"), &units("abc"), 0), None);
        assert_eq!(index_of(&units("ab"), &units("b"), 5), None);
    }

    #[test]
    fn decode_uri() {
        let d = |t: &str| decode_uri_component(&units(t)).map(|u| s(&u));
        assert_eq!(d("a%41%c3%a9%F0%9F%98%80"), Some("aAé😀".to_owned()));
        for bad in [
            "%",
            "%4",
            "%zz",
            "%c3",
            "%c3%41",
            "%80",
            "%c0%80",
            "%ed%a0%80",
            "%f8%80%80%80",
        ] {
            assert_eq!(d(bad), None, "{bad}");
        }
        assert_eq!(
            decode_uri_component(&[0xd800, 0x25, 0x34, 0x31]),
            Some(vec![0xd800, 0x41])
        );
    }
}
