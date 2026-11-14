//! Small synthetic PDFs for the extraction tests (sbstndalton/noevia#700), byte for byte the
//! documents services `docling/synthetic_pdfs.py` builds: hand-assembled PDF 1.4 files, generated at
//! test time so nothing binary is committed and no real document is ever needed.
//!
//! Page kinds (anything else is a page with an empty content stream, as in the reference):
//! - `picture-text`: a full-page image with real (selectable) text drawn on top of it, the shape a
//!   layout model can classify as one picture region whose text it then skips (#700);
//! - `text`: plain text, no image;
//! - `blank`: an empty content stream;
//! - `picture`: an image and no text at all (a scan with no text layer).
//!
//! The one compressed stream (the 8x8 grey image) is written by [`zlib_run`], which emits exactly
//! the bytes zlib's default level gives for a buffer of one repeated byte, so the documents are equal
//! to the reference's, not only equivalent.

/// Neutral, obviously synthetic prose. Long enough to clear the native-fallback threshold on a single
/// page several times over. `{n}` is the paragraph number.
pub const FILLER: &str = "Synthetic fixture paragraph {n}: the quarterly widget audit counted \
forty-two blue widgets and seventeen green ones in aisle {n}.";

/// The file names and pages the reference's `__main__` writes, in that order.
pub const DEFAULT_SET: [(&str, &str); 3] = [
    ("synthetic-picture-text.pdf", "picture-text"),
    ("synthetic-text.pdf", "text"),
    ("synthetic-blank.pdf", "blank"),
];

/// The grey value and pixel count of the page image.
const GREY: u8 = 180;
const PIXELS: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// A text line holds a character outside Latin-1 (the reference's `encode("latin-1")` raises).
    NotLatin1(char),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotLatin1(c) => write!(f, "character U+{:04X} is not Latin-1", *c as u32),
        }
    }
}

impl std::error::Error for Error {}

/// One page: its kind and its text lines (ignored by `blank` and `picture`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub kind: String,
    pub lines: Vec<String>,
}

impl Page {
    pub fn new(kind: &str, lines: Vec<String>) -> Page {
        Page {
            kind: kind.to_string(),
            lines,
        }
    }
}

/// `paragraphs` filler lines numbered from `start` (the reference's `page_lines`, default 12 from 1).
pub fn page_lines(paragraphs: usize, start: i64) -> Vec<String> {
    (0..paragraphs as i64)
        .map(|offset| FILLER.replace("{n}", &(start + offset).to_string()))
        .collect()
}

/// The reference's default page text: `page_lines()`.
pub fn default_lines() -> Vec<String> {
    page_lines(12, 1)
}

/// Backslash, `(` and `)` escaped for a PDF literal string.
pub fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

fn text_ops(lines: &[String]) -> String {
    let (top, step) = (760, 14);
    let mut ops = vec![
        "BT".to_string(),
        "/F1 10 Tf".to_string(),
        format!("72 {top} Td"),
        format!("{step} TL"),
    ];
    for line in lines {
        ops.push(format!("({}) Tj T*", escape(line)));
    }
    ops.push("ET".to_string());
    ops.join("\n")
}

/// Draw the 8x8 grey image across the whole page.
const IMAGE_OPS: &str = "q 612 0 0 792 0 0 cm /Im1 Do Q";

fn latin1(text: &str) -> Result<Vec<u8>, Error> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).map_err(|_| Error::NotLatin1(c)))
        .collect()
}

fn stream_object(dictionary_head: &[u8], data: &[u8]) -> Vec<u8> {
    let mut object = dictionary_head.to_vec();
    object.extend_from_slice(b"/Length ");
    object.extend_from_slice(data.len().to_string().as_bytes());
    object.extend_from_slice(b" >>\nstream\n");
    object.extend_from_slice(data);
    object.extend_from_slice(b"\nendstream");
    object
}

/// The whole document for `pages`. Objects: 1 catalog, 2 pages tree, 3 font, 4 image, then a page
/// and its content stream per page, written in number order with a classic cross-reference table.
pub fn build(pages: &[Page]) -> Result<Vec<u8>, Error> {
    // Page i is object 5 + 2i, its content stream 6 + 2i.
    let kids: Vec<String> = (0..pages.len())
        .map(|index| format!("{} 0 R", 5 + 2 * index))
        .collect();
    // In object-number order from 1.
    let mut objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            kids.len()
        )
        .into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        stream_object(
            b"<< /Type /XObject /Subtype /Image /Width 8 /Height 8 \
/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode ",
            &zlib_run(GREY, PIXELS),
        ),
    ];
    for (index, page) in pages.iter().enumerate() {
        let content_obj = 6 + 2 * index;
        let mut parts: Vec<String> = Vec::new();
        if page.kind == "picture-text" || page.kind == "picture" {
            parts.push(IMAGE_OPS.to_string());
        }
        if page.kind == "picture-text" || page.kind == "text" {
            parts.push(text_ops(&page.lines));
        }
        let content = latin1(&parts.join("\n"))?;
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
/Resources << /Font << /F1 3 0 R >> /XObject << /Im1 4 0 R >> >> /Contents {content_obj} 0 R >>"
            )
            .into_bytes(),
        );
        objects.push(stream_object(b"<< ", &content));
    }

    let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (number, body) in (1..).zip(&objects) {
        offsets.push(out.len());
        out.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    let size = objects.len() + 1;
    out.extend_from_slice(format!("xref\n0 {size}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    Ok(out)
}

/// Write `build(pages)` to `path`; returns the path.
pub fn write(path: &std::path::Path, pages: &[Page]) -> std::io::Result<std::path::PathBuf> {
    let bytes =
        build(pages).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    std::fs::write(path, bytes)?;
    Ok(path.to_path_buf())
}

/// The reference's `__main__`: one page of each default kind, written into `directory`.
pub fn write_default_set(directory: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    DEFAULT_SET
        .iter()
        .map(|(name, kind)| {
            let lines = if *kind == "blank" {
                Vec::new()
            } else {
                default_lines()
            };
            write(&directory.join(name), &[Page::new(kind, lines)])
        })
        .collect()
}

/// Parse one page spec of the command line: `KIND`, `KIND:START` or `KIND:START:COUNT` (filler
/// paragraphs numbered from START, COUNT of them; defaults 1 and 12; `blank` and `picture` take no
/// lines), or `KIND=TEXT` for exactly one line of literal text.
pub fn parse_page(spec: &str) -> Option<Page> {
    if let Some((kind, text)) = spec.split_once('=') {
        return (!kind.is_empty()).then(|| Page::new(kind, vec![text.to_string()]));
    }
    let mut parts = spec.split(':');
    let kind = parts.next().filter(|k| !k.is_empty())?;
    let start = match parts.next() {
        Some(s) => s.parse::<i64>().ok()?,
        None => 1,
    };
    let count = match parts.next() {
        Some(c) => c.parse::<usize>().ok().filter(|c| *c <= 10_000)?,
        None => 12,
    };
    if parts.next().is_some() {
        return None;
    }
    let lines = if kind == "blank" || kind == "picture" {
        Vec::new()
    } else {
        page_lines(count, start)
    };
    Some(Page::new(kind, lines))
}

/// A zlib stream (default compression level) of `count` copies of `byte`, equal to what zlib's
/// level 6 writes: one fixed-Huffman block holding two literals (zlib never matches against window
/// position 0) and then distance-1 matches of at most 258 bytes, a tail under 3 bytes as literals.
pub fn zlib_run(byte: u8, count: usize) -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.put(1, 1); // BFINAL
    bits.put(1, 2); // BTYPE 01: fixed Huffman codes
    let mut left = count;
    for _ in 0..count.min(2) {
        fixed_literal(&mut bits, byte);
        left -= 1;
    }
    while left >= 3 {
        let length = left.min(258);
        fixed_length(&mut bits, length);
        bits.put_code(0, 5); // distance code 0 = distance 1, no extra bits
        left -= length;
    }
    for _ in 0..left {
        fixed_literal(&mut bits, byte);
    }
    bits.put_code(0, 7); // end of block (256)
    let mut out = vec![0x78, 0x9c];
    out.extend_from_slice(&bits.finish());
    let (mut a, mut b) = (1u32, 0u32);
    for _ in 0..count {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

const LENGTH_BASE: [usize; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

fn fixed_literal(bits: &mut BitWriter, byte: u8) {
    let byte = u32::from(byte);
    if byte < 144 {
        bits.put_code(0x30 + byte, 8);
    } else {
        bits.put_code(0x190 + byte - 144, 9);
    }
}

fn fixed_length(bits: &mut BitWriter, length: usize) {
    // The last base not above the length; 258 has its own code (285).
    let (index, base, extra) = LENGTH_BASE
        .iter()
        .zip(LENGTH_EXTRA.iter())
        .enumerate()
        .rev()
        .find(|(_, (base, _))| **base <= length)
        .map_or((0, 3, 0), |(index, (base, extra))| (index, *base, *extra));
    let symbol = 257 + index as u32;
    if symbol < 280 {
        bits.put_code(symbol - 256, 7);
    } else {
        bits.put_code(0xc0 + symbol - 280, 8);
    }
    bits.put((length - base) as u32, extra);
}

/// Deflate's bit order: values least significant bit first, Huffman codes most significant first.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    used: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, width: u32) {
        for i in 0..width {
            self.acc |= ((value >> i) & 1) << self.used;
            self.used += 1;
            if self.used == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.used = 0;
            }
        }
    }

    fn put_code(&mut self, code: u32, width: u32) {
        for i in (0..width).rev() {
            self.put((code >> i) & 1, 1);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.used > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    /// `zlib.compress(bytes([180]) * 64)` on CPython 3.12 (zlib 1.3).
    const REFERENCE_IMAGE: [u8; 12] = [
        0x78, 0x9c, 0xdb, 0xb2, 0x85, 0x32, 0x00, 0x00, 0xb7, 0x0b, 0x2d, 0x01,
    ];

    #[test]
    fn image_stream_is_zlibs_bytes() {
        assert_eq!(zlib_run(GREY, PIXELS), REFERENCE_IMAGE);
    }

    #[test]
    fn filler_lines_are_numbered_from_start() {
        let lines = page_lines(2, 20);
        assert_eq!(
            lines,
            [
                "Synthetic fixture paragraph 20: the quarterly widget audit counted forty-two blue \
widgets and seventeen green ones in aisle 20.",
                "Synthetic fixture paragraph 21: the quarterly widget audit counted forty-two blue \
widgets and seventeen green ones in aisle 21."
            ]
        );
        assert_eq!(default_lines().len(), 12);
        assert!(default_lines()[11].contains("paragraph 12:"));
        assert!(page_lines(0, 1).is_empty());
    }

    #[test]
    fn escape_doubles_backslash_first() {
        assert_eq!(escape(r"a\(b)"), r"a\\\(b\)");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn blank_page_document_layout() {
        let pdf = build(&[Page::new("blank", Vec::new())]).unwrap();
        let text = String::from_utf8_lossy(&pdf);
        assert!(pdf.starts_with(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n1 0 obj\n"));
        assert!(text.contains("6 0 obj\n<< /Length 0 >>\nstream\n\nendstream\nendobj\n"));
        assert!(text.contains("<< /Type /Pages /Kids [5 0 R] /Count 1 >>"));
        assert!(text.contains("xref\n0 7\n0000000000 65535 f \n0000000015 00000 n \n"));
        assert!(text.ends_with("%%EOF\n"));
        // Every offset in the table points at its object (byte offsets: the header is binary).
        let find = |needle: &[u8]| pdf.windows(needle.len()).position(|w| w == needle).unwrap();
        let xref = find(b"xref\n");
        let table = String::from_utf8(pdf[xref..].to_vec()).unwrap();
        for (number, line) in table.lines().skip(3).take(6).enumerate() {
            let offset: usize = line[..10].parse().unwrap();
            assert!(pdf[offset..].starts_with(format!("{} 0 obj\n", number + 1).as_bytes()));
        }
        let tail = table.rsplit("startxref\n").next().unwrap();
        let startxref: usize = tail.trim_end_matches("\n%%EOF\n").parse().unwrap();
        assert_eq!(startxref, xref);
    }

    #[test]
    fn page_kinds_choose_their_operators() {
        let lines = vec!["one (1)".to_string()];
        let content = |kind: &str| {
            let pdf = build(&[Page::new(kind, lines.clone())]).unwrap();
            let text = String::from_utf8_lossy(&pdf).into_owned();
            let start = text.find("6 0 obj\n").unwrap();
            let end = text[start..].find("\nendobj").unwrap();
            text[start..start + end].to_string()
        };
        let both = content("picture-text");
        assert!(both.contains("q 612 0 0 792 0 0 cm /Im1 Do Q\nBT\n/F1 10 Tf\n72 760 Td\n14 TL\n(one \\(1\\)) Tj T*\nET"));
        assert!(content("picture").contains("stream\nq 612 0 0 792 0 0 cm /Im1 Do Q\nendstream"));
        assert!(content("text").contains("stream\nBT\n"));
        assert!(!content("text").contains("/Im1 Do"));
        assert!(content("blank").contains("<< /Length 0 >>"));
        // An unknown kind is an empty page, as in the reference.
        assert_eq!(content("unknown"), content("blank"));
    }

    #[test]
    fn length_counts_bytes_and_latin1_is_one_byte() {
        let pdf = build(&[Page::new("text", vec!["caf\u{e9}".to_string()])]).unwrap();
        let needle = b"(caf\xe9) Tj T*";
        assert!(pdf.windows(needle.len()).any(|w| w == needle));
        assert_eq!(
            build(&[Page::new("text", vec!["\u{2014}".to_string()])]),
            Err(Error::NotLatin1('\u{2014}'))
        );
        // A non-Latin-1 line on a page that draws no text does not matter (never encoded).
        assert!(build(&[Page::new("picture", vec!["\u{2014}".to_string()])]).is_ok());
    }

    #[test]
    fn page_and_content_numbers_follow_the_page_order() {
        let pdf = build(&[
            Page::new("text", default_lines()),
            Page::new("picture-text", page_lines(12, 20)),
            Page::new("blank", Vec::new()),
        ])
        .unwrap();
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("/Kids [5 0 R 7 0 R 9 0 R] /Count 3"));
        assert!(text.contains("/Contents 10 0 R >>"));
        assert!(text.contains("trailer\n<< /Size 11 /Root 1 0 R >>"));
    }

    #[test]
    fn page_specs() {
        assert_eq!(parse_page("text"), Some(Page::new("text", default_lines())));
        assert_eq!(
            parse_page("picture-text:20"),
            Some(Page::new("picture-text", page_lines(12, 20)))
        );
        assert_eq!(
            parse_page("text:3:2"),
            Some(Page::new("text", page_lines(2, 3)))
        );
        assert_eq!(parse_page("blank:5"), Some(Page::new("blank", Vec::new())));
        assert_eq!(
            parse_page("picture-text=Page 3 of 9"),
            Some(Page::new("picture-text", vec!["Page 3 of 9".to_string()]))
        );
        for bad in [
            "",
            ":1",
            "text:x",
            "text:1:y",
            "text:1:2:3",
            "=x",
            "text:1:10001",
        ] {
            assert_eq!(parse_page(bad), None, "{bad}");
        }
    }

    /// A fixed-Huffman-only inflater, enough to read back what `zlib_run` writes.
    fn inflate_fixed(data: &[u8]) -> Vec<u8> {
        assert_eq!(&data[..2], &[0x78, 0x9c]);
        let body = &data[2..data.len() - 4];
        let mut position = 0usize;
        let mut bit = |n: u32| -> u32 {
            let mut v = 0;
            for i in 0..n {
                let b = (body[position / 8] >> (position % 8)) & 1;
                v |= u32::from(b) << i;
                position += 1;
            }
            v
        };
        assert_eq!(bit(1), 1);
        assert_eq!(bit(2), 1);
        let mut out: Vec<u8> = Vec::new();
        loop {
            // Read a fixed code MSB-first, 7 bits then up to 9.
            let mut code = 0u32;
            for _ in 0..7 {
                code = (code << 1) | bit(1);
            }
            let symbol = if code <= 0b0010111 {
                256 + code
            } else {
                code = (code << 1) | bit(1);
                if (0x30..=0xbf).contains(&code) {
                    code - 0x30
                } else if (0xc0..=0xc7).contains(&code) {
                    280 + code - 0xc0
                } else {
                    code = (code << 1) | bit(1);
                    144 + code - 0x190
                }
            };
            match symbol {
                0..=255 => out.push(symbol as u8),
                256 => break,
                _ => {
                    let index = (symbol - 257) as usize;
                    let length = LENGTH_BASE[index] + bit(LENGTH_EXTRA[index]) as usize;
                    let mut d = 0;
                    for _ in 0..5 {
                        d = (d << 1) | bit(1);
                    }
                    assert_eq!(d, 0, "only distance 1 is written");
                    for _ in 0..length {
                        out.push(*out.last().unwrap());
                    }
                }
            }
        }
        out
    }

    #[test]
    fn runs_of_any_length_read_back() {
        for byte in [0u8, 143, 144, 180, 255] {
            for count in (0..20).chain([63, 64, 258, 259, 260, 261, 262, 517, 1000]) {
                let stream = zlib_run(byte, count);
                assert_eq!(
                    inflate_fixed(&stream),
                    vec![byte; count],
                    "{byte} x {count}"
                );
            }
        }
    }
}
