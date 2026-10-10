//! The JavaScript built-ins the ported handlers call, with Node's results.

use rand_core::{OsRng, RngCore};

/// `decodeURIComponent(s)`; `None` where JS throws URIError (a `%` not followed by two hex digits,
/// or bytes that are not UTF-8: overlong forms, surrogates, past U+10FFFF). Nothing is reserved.
pub fn decode_uri_component(s: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(s.len());
    let mut it = s.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hi = hex(it.next()?)?;
            let lo = hex(it.next()?)?;
            bytes.push(hi << 4 | lo);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `Buffer.from(s, 'base64')`: Node's lenient decoder. Each UTF-16 code unit is taken by its low
/// byte (Node narrows a two-byte string that way: U+0144 reads as `D`, U+D83D as `=`); then both
/// alphabets (`+/` and `-_`) decode, any other byte (whitespace, punctuation) is skipped, decoding
/// stops at the first `=`, and a trailing group of two or three characters gives one or two
/// bytes, a single one none. Never fails.
pub fn base64_loose(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 2);
    let (mut acc, mut n) = (0u32, 0u8);
    for unit in s.encode_utf16() {
        let v = match (unit & 0xff) as u8 {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'a'..=b'z' => c - b'a' + 26,
            c @ b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        n += 1;
        if n == 4 {
            out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]);
            (acc, n) = (0, 0);
        }
    }
    match n {
        2 => out.push((acc >> 4) as u8),
        3 => out.extend_from_slice(&[(acc >> 10) as u8, (acc >> 2) as u8]),
        _ => {}
    }
    out
}

const DIGITS36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// `n.toString(36)` for a non-negative integer (`Date.now().toString(36)`).
pub fn base36(mut n: u64) -> String {
    let mut out = Vec::new();
    loop {
        out.push(
            DIGITS36
                .get(usize::try_from(n % 36).unwrap_or(0))
                .copied()
                .unwrap_or(b'0'),
        );
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Six random `[0-9a-z]` characters, the shape of `Math.random().toString(36).slice(2, 8)`
/// (from the OS generator, unbiased).
pub fn random36(len: usize) -> String {
    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 32];
    while out.len() < len {
        OsRng.fill_bytes(&mut buf);
        for b in buf {
            // 252 = 7 * 36: no modulo bias.
            if b < 252 && out.len() < len {
                if let Some(&d) = DIGITS36.get(usize::from(b % 36)) {
                    out.push(char::from(d));
                }
            }
        }
    }
    out
}

/// `String(s).replace(/[^a-zA-Z0-9_-]/g, '')`: what core workspace.cjs `assetDir` and the image
/// route keep of an id before it names a directory or a file.
pub fn id_chars(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn hexs(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Node 22/26 `Buffer.from(input, 'base64').toString('hex')`.
    #[test]
    fn base64_matches_nodes_lenient_decoder() {
        for (input, want) in [
            ("", ""),
            ("QQ", "41"),
            ("QQ=", "41"),
            ("QQ==", "41"),
            ("QUJD", "414243"),
            ("QU JD", "414243"),
            ("QU\nJD", "414243"),
            ("Q!U@J#D", "414243"),
            ("QQ==QUJD", "41"),
            ("QUJD=QUJD", "414243"),
            ("-_-_", "fbffbf"),
            ("+/+/", "fbffbf"),
            ("QUJ", "4142"),
            ("Q", ""),
            ("====", ""),
            ("QQ=x", "41"),
            ("A", ""),
            ("AB", "00"),
            ("ABC", "0010"),
            ("QUJDRA", "41424344"),
            ("QUJDRA=", "41424344"),
            ("\u{e9}QUJD", "414243"),
            ("QU=JD", "41"),
            ("Q=Q=", ""),
            ("*", ""),
            ("R0lG OD lh\nAQAB!AAAA=ignored", "474946383961010001000000"),
            // Two-byte strings: each code unit by its low byte.
            ("\u{1F600}QUJD", ""),
            ("QUJD\u{1F600}", "414243"),
            ("QU\u{1F600}JD", "41"),
            ("\u{0100}QUJD", "414243"),
            ("QU\u{20ac}JD", "414243"),
            ("QUJ\u{0144}", "414243"),
            ("\u{0151}\u{0155}JD", "414243"),
            ("QUJD\u{013d}\u{013d}", "414243"),
            ("\u{10000}QUJD", "414243"),
            ("QUJD\u{1F600}QUJD", "414243"),
        ] {
            assert_eq!(hexs(&base64_loose(input)), want, "{input:?}");
        }
    }

    #[test]
    fn decode_uri_component_as_js() {
        assert_eq!(
            decode_uri_component("proj-1-abc").as_deref(),
            Some("proj-1-abc")
        );
        assert_eq!(
            decode_uri_component("a%2Fb%20c%25").as_deref(),
            Some("a/b c%")
        );
        assert_eq!(
            decode_uri_component("%C3%A9%e2%82%ac").as_deref(),
            Some("é€")
        );
        for bad in [
            "%",
            "%4",
            "%zz",
            "%80",
            "%C3",
            "%C3A",
            "%C0%AF",
            "%ED%A0%80",
            "%F4%90%80%80",
            "%FF",
        ] {
            assert_eq!(decode_uri_component(bad), None, "{bad}");
        }
    }

    #[test]
    fn base36_random36_and_id_chars() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        // (1791600933849).toString(36) in Node.
        assert_eq!(base36(1_791_600_933_849), "mv1sxryx");
        let r = random36(6);
        assert_eq!(r.len(), 6);
        assert!(r
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()));
        assert_ne!(random36(12), random36(12));
        assert_eq!(id_chars("img-ab_C9/../é x"), "img-ab_C9x");
        assert_eq!(id_chars("../.."), "");
    }
}
