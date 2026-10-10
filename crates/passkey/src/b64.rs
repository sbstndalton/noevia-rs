//! @hexagon/base64 1.1.28 as @simplewebauthn's isoBase64URL calls it.
//!
//! `toArrayBuffer(data, true)` is lenient: it reads JS string code units, maps each through a
//! 256-entry table (a unit outside it, or a character not in the alphabet, counts as 0), sizes
//! the output as `length * 0.75` minus one per trailing `=` (truncated), and drops what does not
//! fit. Ported exactly, so a decoded clientDataJSON hashes to what Node hashed.

const URL_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn lookup(unit: Option<u16>) -> u32 {
    let Some(u) = unit else { return 0 };
    let Ok(b) = u8::try_from(u) else { return 0 };
    URL_ALPHABET
        .iter()
        .position(|c| *c == b)
        .and_then(|i| u32::try_from(i).ok())
        .unwrap_or(0)
}

/// `isoBase64URL.toBuffer(s)`.
pub fn to_buffer(s: &str) -> Vec<u8> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let len = units.len();
    let mut size = len as f64 * 0.75;
    if units.last() == Some(&u16::from(b'=')) {
        size -= 1.0;
        if len >= 2 && units.get(len - 2) == Some(&u16::from(b'=')) {
            size -= 1.0;
        }
    }
    // new ArrayBuffer(size): ToIndex truncates toward zero.
    let size = if size > 0.0 { size.trunc() as usize } else { 0 };
    let mut out = Vec::with_capacity(size);
    let mut i = 0;
    while i < len {
        let e1 = lookup(units.get(i).copied());
        let e2 = lookup(units.get(i + 1).copied());
        let e3 = lookup(units.get(i + 2).copied());
        let e4 = lookup(units.get(i + 3).copied());
        for b in [
            (e1 << 2) | (e2 >> 4),
            ((e2 & 15) << 4) | (e3 >> 2),
            ((e3 & 3) << 6) | (e4 & 63),
        ] {
            if out.len() < size {
                out.push((b & 0xff) as u8);
            }
        }
        i += 4;
    }
    out
}

/// `isoBase64URL.isBase64URL(s)`: padding removed, then `/^[-A-Za-z0-9\-_]*$/`.
pub fn is_base64url(s: &str) -> bool {
    s.chars()
        .filter(|c| *c != '=')
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `isoBase64URL.fromBuffer(bytes)`: base64url without padding.
pub fn from_buffer(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or(0));
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        let chars = match chunk.len() {
            1 => 2,
            2 => 3,
            _ => 4,
        };
        for k in 0..chars {
            let idx = ((n >> (18 - 6 * k)) & 63) as usize;
            if let Some(c) = URL_ALPHABET.get(idx) {
                out.push(char::from(*c));
            }
        }
    }
    out
}

/// `new TextDecoder().decode(bytes)`: UTF-8 with replacement, a leading BOM dropped.
pub fn utf8_decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// `isoBase64URL.toUTF8String(s)`.
pub fn to_utf8_string(s: &str) -> String {
    utf8_decode(&to_buffer(s))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_leniency() {
        for n in 0..40u8 {
            let bytes: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37).wrapping_add(n)).collect();
            assert_eq!(to_buffer(&from_buffer(&bytes)), bytes, "{n}");
        }
        assert_eq!(from_buffer(b"\xfb\xff"), "-_8");
        // Padded input decodes the same; foreign characters count as 'A'.
        assert_eq!(to_buffer("YWI="), b"ab");
        assert_eq!(to_buffer("YW=="), b"a");
        assert_eq!(to_buffer("Y*I"), to_buffer("YAI"));
        assert_eq!(to_buffer("Y\u{142}I"), to_buffer("YAI"));
        assert!(to_buffer("").is_empty());
        assert!(to_buffer("=").is_empty());
        assert!(is_base64url("ab-_09=="));
        assert!(!is_base64url("ab+/"));
        assert!(!is_base64url("a b"));
        assert_eq!(utf8_decode(b"\xef\xbb\xbf{}"), "{}");
    }
}
