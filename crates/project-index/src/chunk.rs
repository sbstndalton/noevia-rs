//! `chunkText` and `hash` of rag.cjs.
//!
//! JavaScript measures and slices strings in UTF-16 code units, so the chunker works on `&[u16]`
//! and cuts where Node cuts, even between the halves of a surrogate pair. A text crosses into
//! SQLite and into sha256 as UTF-8, where Node writes an unpaired surrogate as U+FFFD; [`to_text`]
//! does the same.

use sha2::{Digest, Sha256};

/// Code units per chunk (`CHUNK_SIZE`).
pub const CHUNK_SIZE: usize = 1200;
/// Overlap between consecutive chunks (`CHUNK_STRIDE`).
pub const CHUNK_STRIDE: usize = 150;
/// A text of at most this many code units is stored whole and never embedded (`DIRECT_INJECT_MAX`).
pub const DIRECT_INJECT_MAX: usize = 2400;

/// UTF-16 code units of a string.
pub fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// A code-unit text as Node hands it to SQLite or sha256 (an unpaired surrogate becomes U+FFFD).
pub fn to_text(text: &[u16]) -> String {
    String::from_utf16_lossy(text)
}

/// sha256 hex of a text's UTF-8 bytes (`hash`).
pub fn hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `hash` of a code-unit text.
pub fn hash_units(text: &[u16]) -> String {
    hash(&to_text(text))
}

/// What `String.prototype.trim` removes: WhiteSpace and LineTerminator code points (all in the BMP).
fn js_space(u: u16) -> bool {
    matches!(
        u,
        0x0009..=0x000D
            | 0x0020
            | 0x00A0
            | 0x1680
            | 0x2000..=0x200A
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
            | 0xFEFF
    )
}

/// [`js_space`] for a `char` (every such code point is in the BMP).
pub(crate) fn is_js_space(c: char) -> bool {
    u16::try_from(u32::from(c)).is_ok_and(js_space)
}

/// `String.prototype.trim` on code units.
pub fn js_trim(text: &[u16]) -> &[u16] {
    let start = text
        .iter()
        .position(|u| !js_space(*u))
        .unwrap_or(text.len());
    let end = text
        .iter()
        .rposition(|u| !js_space(*u))
        .map_or(start, |i| i + 1);
    text.get(start..end).unwrap_or_default()
}

fn digit(u: u16) -> bool {
    (u16::from(b'0')..=u16::from(b'9')).contains(&u)
}

/// Length of a `[Page \d+]` marker at the start of `text`, or None (`/^\[Page \d+\]/`).
pub fn page_marker(text: &[u16]) -> Option<usize> {
    let lead = units("[Page ");
    if !text.starts_with(&lead) {
        return None;
    }
    let digits = text
        .iter()
        .skip(lead.len())
        .take_while(|u| digit(**u))
        .count();
    if digits == 0 {
        return None;
    }
    let close = lead.len() + digits;
    (text.get(close) == Some(&u16::from(b']'))).then_some(close + 1)
}

/// Where `text.split(/(?=\[Page \d+\]\n)/)` cuts: every index after 0 that starts a marker and a newline.
fn page_starts(text: &[u16]) -> Vec<usize> {
    (1..text.len())
        .filter(|&i| {
            let rest = text.get(i..).unwrap_or_default();
            page_marker(rest).is_some_and(|m| rest.get(m) == Some(&u16::from(b'\n')))
        })
        .collect()
}

/// `text.replace(/\r\n/g, '\n')`.
fn crlf(text: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while let Some(&u) = text.get(i) {
        if u == 0x0D && text.get(i + 1) == Some(&0x0A) {
            out.push(0x0A);
            i += 2;
        } else {
            out.push(u);
            i += 1;
        }
    }
    out
}

fn last_double_newline(window: &[u16]) -> Option<usize> {
    window.windows(2).rposition(|w| w == [0x0A, 0x0A])
}

/// `chunkText(text)`: a document's text cut into overlapping chunks. A text that starts with a
/// `[Page N]` marker is cut page by page, and every chunk of a page starts with its marker.
pub fn chunk_text(text: &[u16]) -> Vec<Vec<u16>> {
    chunk_part(text, false)
}

/// `chunkText` of a `&str` (a convenience: the chunks come back as Node would store them).
pub fn chunk_str(text: &str) -> Vec<String> {
    chunk_text(&units(text))
        .iter()
        .map(|c| to_text(c))
        .collect()
}

fn chunk_part(text: &[u16], page_part: bool) -> Vec<Vec<u16>> {
    if !page_part && page_marker(text).is_some() {
        let mut bounds = vec![0];
        bounds.extend(page_starts(text));
        bounds.push(text.len());
        let mut out = Vec::new();
        for pair in bounds.windows(2) {
            let (Some(&a), Some(&b)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            let part = text.get(a..b).unwrap_or_default();
            if part.is_empty() {
                continue;
            }
            let marker = page_marker(part).unwrap_or(0);
            let (head, body) = part.split_at(marker);
            for chunk in chunk_part(body, true) {
                let mut c = head.to_vec();
                c.push(0x0A);
                c.extend(chunk);
                out.push(c);
            }
        }
        return out;
    }
    let joined = crlf(text);
    let clean = js_trim(&joined);
    if clean.is_empty() {
        return Vec::new();
    }
    if clean.len() <= CHUNK_SIZE {
        return vec![clean.to_vec()];
    }
    let half = CHUNK_SIZE / 2;
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < clean.len() {
        let mut end = (start + CHUNK_SIZE).min(clean.len());
        if end < clean.len() {
            // Prefer a paragraph break, then a sentence/line end, in the back half.
            let window = clean.get(start..end).unwrap_or_default();
            match last_double_newline(window) {
                Some(para) if para > half => end = start + para + 1,
                _ => {
                    let best = (half..window.len()).rev().find(|&i| {
                        window
                            .get(i)
                            .is_some_and(|u| b".!?\n".iter().any(|c| u16::from(*c) == *u))
                    });
                    if let Some(best) = best.filter(|b| *b > 0) {
                        end = start + best + 1;
                    }
                }
            }
        }
        let piece = js_trim(clean.get(start..end).unwrap_or_default());
        if !piece.is_empty() {
            chunks.push(piece.to_vec());
        }
        if end >= clean.len() {
            break;
        }
        start = end.saturating_sub(CHUNK_STRIDE).max(start + 1);
    }
    chunks
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn short_and_empty() {
        assert!(chunk_str("  \r\n ").is_empty());
        assert_eq!(chunk_str(" a\r\nb "), vec!["a\nb"]);
    }

    #[test]
    fn page_marker_shapes() {
        assert_eq!(page_marker(&units("[Page 12]\nx")), Some(9));
        assert_eq!(page_marker(&units("[Page ]")), None);
        assert_eq!(page_marker(&units("[Page 1")), None);
    }

    #[test]
    fn pages_keep_their_marker_and_empty_pages_vanish() {
        assert_eq!(
            chunk_str("[Page 1]\none\n\n[Page 2]\n  \n[Page 3]\nthree"),
            vec!["[Page 1]\none", "[Page 3]\nthree"]
        );
    }

    #[test]
    fn hash_is_sha256_hex() {
        assert_eq!(
            hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn windows_overlap_by_the_stride() {
        let text = "x".repeat(3000);
        let chunks = chunk_str(&text);
        assert_eq!(
            chunks.iter().map(String::len).collect::<Vec<_>>(),
            vec![1200, 1200, 900]
        );
    }
}
