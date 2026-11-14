//! Synthetic PDFs for the OCR worker's real-engine tests (noevia#856), ported from the services
//! repository's `ocr/synthetic_pdfs.py` (the oracle; `tests/oracle.rs` compares the two).
//!
//! Generated at test time, nothing binary committed, and nothing here is real data: the
//! "financial rows" are invented numbers.
//!
//! Fixtures ([`write_all`]):
//! - `scanned.pdf`: one page that is only an image of text, no text layer, so the words can only
//!   be found by OCR;
//! - `mixed-page.pdf`: the same scan with a native text line ([`HEADER`]) on it;
//! - `mixed-pages.pdf`: page 1 native text only, page 2 the scan;
//! - `text.pdf`: native text, no image ([`oversized_text_pdf`] pads it past 25 MB);
//! - `malformed.pdf`: a PDF header and then garbage;
//! - `encrypted.pdf`: RC4-40 standard security with a non-empty user password, so a reader
//!   without the password is refused (hand-built, no crypto library).
//!
//! The scan is drawn with a 5x7 dot-matrix capital font so an OCR engine reads it reliably
//! without any imaging dependency, which is why the rows are upper case.
//!
//! Parity with the reference: every byte is the same except the deflate data of the scan image
//! (the reference uses zlib level 9; this uses the locked pure-Rust deflate). The decoded pixels,
//! and so what an OCR engine sees, are identical; `tests/oracle.rs` proves both.

mod engines;
pub mod md5;

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

pub use engines::{missing_engines, skip_reason, skip_reason_from, Probe, SystemProbe};

/// What the scanned page says. Mirrors a bank-statement shape: an identifier, signed amounts and
/// a total.
pub const ROWS: [&str; 4] = [
    "REF-2042",
    "2042-01-02 UTILITIES 42.15",
    "2042-01-03 REFUND -7.20",
    "TOTAL 34.95",
];
pub const HEADER: &str = "DIGITAL HEADER";

/// Image pixels per font dot, and blur passes (chosen in CI over a grid of sizes and blurs: 3 px
/// dots with one pass read all four rows exactly at every blur level tried).
pub const CELL: usize = 3;
pub const BLUR_PASSES: usize = 1;
/// The whole US-Letter page at 150 dpi.
pub const IMAGE_W: usize = 1275;
pub const IMAGE_H: usize = 1650;
pub const GLYPH_W: usize = 5;
pub const GLYPH_H: usize = 7;

/// `pdf(..., padding)` this much to pass the 25 MB limit the PDF reduction must get under.
pub const OVERSIZE_PADDING: usize = 26 * 1024 * 1024;

/// The 5x7 font, in the reference's order.
pub const FONT: [(char, [&str; GLYPH_H]); 39] = [
    (
        'A',
        [
            ".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#",
        ],
    ),
    (
        'B',
        [
            "####.", "#...#", "#...#", "####.", "#...#", "#...#", "####.",
        ],
    ),
    (
        'C',
        [
            ".###.", "#...#", "#....", "#....", "#....", "#...#", ".###.",
        ],
    ),
    (
        'D',
        [
            "####.", "#...#", "#...#", "#...#", "#...#", "#...#", "####.",
        ],
    ),
    (
        'E',
        [
            "#####", "#....", "#....", "####.", "#....", "#....", "#####",
        ],
    ),
    (
        'F',
        [
            "#####", "#....", "#....", "####.", "#....", "#....", "#....",
        ],
    ),
    (
        'G',
        [
            ".###.", "#...#", "#....", "#.###", "#...#", "#...#", ".####",
        ],
    ),
    (
        'H',
        [
            "#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#",
        ],
    ),
    (
        'I',
        [
            ".###.", "..#..", "..#..", "..#..", "..#..", "..#..", ".###.",
        ],
    ),
    (
        'J',
        [
            "..###", "...#.", "...#.", "...#.", "...#.", "#..#.", ".##..",
        ],
    ),
    (
        'K',
        [
            "#...#", "#..#.", "#.#..", "##...", "#.#..", "#..#.", "#...#",
        ],
    ),
    (
        'L',
        [
            "#....", "#....", "#....", "#....", "#....", "#....", "#####",
        ],
    ),
    (
        'M',
        [
            "#...#", "##.##", "#.#.#", "#.#.#", "#...#", "#...#", "#...#",
        ],
    ),
    (
        'N',
        [
            "#...#", "##..#", "#.#.#", "#..##", "#...#", "#...#", "#...#",
        ],
    ),
    (
        'O',
        [
            ".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###.",
        ],
    ),
    (
        'P',
        [
            "####.", "#...#", "#...#", "####.", "#....", "#....", "#....",
        ],
    ),
    (
        'Q',
        [
            ".###.", "#...#", "#...#", "#...#", "#.#.#", "#..#.", ".##.#",
        ],
    ),
    (
        'R',
        [
            "####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#",
        ],
    ),
    (
        'S',
        [
            ".####", "#....", "#....", ".###.", "....#", "....#", "####.",
        ],
    ),
    (
        'T',
        [
            "#####", "..#..", "..#..", "..#..", "..#..", "..#..", "..#..",
        ],
    ),
    (
        'U',
        [
            "#...#", "#...#", "#...#", "#...#", "#...#", "#...#", ".###.",
        ],
    ),
    (
        'V',
        [
            "#...#", "#...#", "#...#", ".#.#.", ".#.#.", ".#.#.", "..#..",
        ],
    ),
    (
        'W',
        [
            "#...#", "#...#", "#...#", "#.#.#", "#.#.#", "##.##", "#...#",
        ],
    ),
    (
        'X',
        [
            "#...#", "#...#", ".#.#.", "..#..", ".#.#.", "#...#", "#...#",
        ],
    ),
    (
        'Y',
        [
            "#...#", "#...#", ".#.#.", "..#..", "..#..", "..#..", "..#..",
        ],
    ),
    (
        'Z',
        [
            "#####", "....#", "...#.", "..#..", ".#...", "#....", "#####",
        ],
    ),
    // Same shape as 'O' on purpose: a zero with a diagonal or a dot inside was misread as 6, 4,
    // H, B or b (measured in CI, every size and blur); the plain oval reads as 0 among digits.
    (
        '0',
        [
            ".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###.",
        ],
    ),
    (
        '1',
        [
            "..#..", ".##..", "..#..", "..#..", "..#..", "..#..", ".###.",
        ],
    ),
    (
        '2',
        [
            ".###.", "#...#", "....#", "...#.", "..#..", ".#...", "#####",
        ],
    ),
    (
        '3',
        [
            "####.", "....#", "....#", ".###.", "....#", "....#", "####.",
        ],
    ),
    (
        '4',
        [
            "...#.", "..##.", ".#.#.", "#..#.", "#####", "...#.", "...#.",
        ],
    ),
    (
        '5',
        [
            "#####", "#....", "####.", "....#", "....#", "#...#", ".###.",
        ],
    ),
    (
        '6',
        [
            ".###.", "#....", "#....", "####.", "#...#", "#...#", ".###.",
        ],
    ),
    (
        '7',
        [
            "#####", "....#", "...#.", "..#..", ".#...", ".#...", ".#...",
        ],
    ),
    (
        '8',
        [
            ".###.", "#...#", "#...#", ".###.", "#...#", "#...#", ".###.",
        ],
    ),
    (
        '9',
        [
            ".###.", "#...#", "#...#", ".####", "....#", "....#", ".###.",
        ],
    ),
    (
        '-',
        [
            ".....", ".....", ".....", "#####", ".....", ".....", ".....",
        ],
    ),
    (
        '.',
        [
            ".....", ".....", ".....", ".....", ".....", ".##..", ".##..",
        ],
    ),
    (' ', ["....."; GLYPH_H]),
];

/// The glyph of `c`, if the font has one.
pub fn glyph(c: char) -> Option<&'static [&'static str; GLYPH_H]> {
    FONT.iter().find(|(k, _)| *k == c).map(|(_, g)| g)
}

#[derive(Debug)]
pub enum Error {
    /// The reference's `assert ... "text does not fit the page"`.
    DoesNotFit,
    /// The reference raises `KeyError` for a character outside the font.
    NoGlyph(char),
    /// Page text must be Latin-1 (the reference raises `UnicodeEncodeError`).
    NotLatin1(char),
    /// The text block reaches past the page edge so the blurred band has fewer than two rows
    /// (the reference raises `IndexError` or grows the page there).
    BandOffPage,
    /// Object numbers must run 1..=max without a gap (the reference raises `KeyError`).
    MissingObject(u32),
    /// `write_ballast_pdf` with a zero chunk (the reference loops forever).
    ZeroChunk,
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::DoesNotFit => f.write_str("text does not fit the page"),
            Error::NoGlyph(c) => write!(f, "no glyph for {c:?}"),
            Error::NotLatin1(c) => write!(f, "{c:?} is not Latin-1"),
            Error::BandOffPage => f.write_str("text band runs off the page"),
            Error::MissingObject(n) => write!(f, "object {n} is missing"),
            Error::ZeroChunk => f.write_str("chunk must be at least one byte"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A gray bitmap, one byte per pixel, rows top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

fn avg3(a: u8, b: u8, c: u8) -> u8 {
    // At most 765 / 3 = 255, so the narrowing cannot lose anything.
    ((u16::from(a) + u16::from(b) + u16::from(c)) / 3) as u8
}

/// 3x3 box blur of equal-length rows; the outermost rows and columns keep their value (after the
/// horizontal pass, as in the reference).
fn box_blur(rows: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let horizontal: Vec<Vec<u8>> = rows
        .iter()
        .map(|row| {
            let mut out = Vec::with_capacity(row.len());
            out.extend(row.first().copied());
            out.extend(row.windows(3).map(|w| match w {
                [a, b, c] => avg3(*a, *b, *c),
                _ => 0,
            }));
            if row.len() > 1 {
                out.extend(row.last().copied());
            }
            out
        })
        .collect();
    let mut out = Vec::with_capacity(horizontal.len());
    out.extend(horizontal.first().cloned());
    out.extend(horizontal.windows(3).map(|w| {
        match w {
            [above, row, below] => above
                .iter()
                .zip(row)
                .zip(below)
                .map(|((a, b), c)| avg3(*a, *b, *c))
                .collect(),
            _ => Vec::new(),
        }
    }));
    if horizontal.len() > 1 {
        out.extend(horizontal.last().cloned());
    }
    out
}

/// Python slice bounds `[start:end]` of a sequence of length `len`.
fn py_slice(start: isize, end: isize, len: usize) -> (usize, usize) {
    let len_i = isize::try_from(len).unwrap_or(isize::MAX);
    let norm = |i: isize| -> usize {
        let j = if i < 0 {
            (i + len_i).max(0)
        } else {
            i.min(len_i)
        };
        usize::try_from(j).unwrap_or(0)
    };
    let (s, e) = (norm(start), norm(end));
    (s, e.max(s))
}

/// The reference's `render_lines(lines)` with its defaults (left 96, top 240, line gap 3).
pub fn render_lines(lines: &[&str]) -> Result<Bitmap> {
    render_lines_at(lines, 96, 240, 3)
}

/// Gray bitmap (white page, text in dots) of `lines`. The dots are softened with a 3x3 blur: a
/// raw 5x7 dot matrix is read as junk by some engines, the softened strokes read cleanly, and a
/// real scan is never pixel-perfect anyway.
pub fn render_lines_at(lines: &[&str], left: usize, top: usize, line_gap: usize) -> Result<Bitmap> {
    let mut page = vec![vec![0xffu8; IMAGE_W]; IMAGE_H];
    let advance = (GLYPH_W + 1) * CELL;
    for (row, text) in lines.iter().enumerate() {
        let y0 = top + row * (GLYPH_H + line_gap) * CELL;
        for (column, c) in text.chars().enumerate() {
            let x0 = left + column * advance;
            if x0 + advance > IMAGE_W || y0 + GLYPH_H * CELL > IMAGE_H {
                return Err(Error::DoesNotFit);
            }
            let shape = glyph(c).ok_or(Error::NoGlyph(c))?;
            for (gy, glyph_row) in shape.iter().enumerate() {
                for (gx, dot) in glyph_row.bytes().enumerate() {
                    if dot != b'#' {
                        continue;
                    }
                    for dy in 0..CELL {
                        let start = x0 + gx * CELL;
                        if let Some(cells) = page
                            .get_mut(y0 + gy * CELL + dy)
                            .and_then(|r| r.get_mut(start..start + CELL))
                        {
                            cells.fill(0);
                        }
                    }
                }
            }
        }
    }
    let first = top as isize - (2 * CELL) as isize;
    let last = (top + lines.len() * (GLYPH_H + line_gap) * CELL + 2 * CELL) as isize;
    let (s, e) = py_slice(first, last, IMAGE_H);
    let band = page.get(s..e).unwrap_or_default();
    // The reference's blur indexes the band's first row and keeps its first and last row; with
    // fewer than two rows it fails or changes the page height.
    if band.len() < 2 {
        return Err(Error::BandOffPage);
    }
    let mut band = band.to_vec();
    for _ in 0..BLUR_PASSES {
        band = box_blur(&band);
    }
    page.splice(s..e, band);
    Ok(Bitmap {
        width: IMAGE_W,
        height: IMAGE_H,
        pixels: page.concat(),
    })
}

/// One page of [`build`]: a scan of `scan` (an image, no text layer) and/or native `text` lines.
/// Empty means absent, as in the reference (`if page.get("scan")`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub scan: Vec<String>,
    pub text: Vec<String>,
}

impl Page {
    pub fn scan<S: AsRef<str>>(lines: &[S]) -> Self {
        Page {
            scan: owned(lines),
            text: Vec::new(),
        }
    }

    pub fn text<S: AsRef<str>>(lines: &[S]) -> Self {
        Page {
            scan: Vec::new(),
            text: owned(lines),
        }
    }

    pub fn mixed<S: AsRef<str>, T: AsRef<str>>(scan: &[S], text: &[T]) -> Self {
        Page {
            scan: owned(scan),
            text: owned(text),
        }
    }
}

fn owned<S: AsRef<str>>(lines: &[S]) -> Vec<String> {
    lines.iter().map(|s| s.as_ref().to_owned()).collect()
}

/// PDF literal-string escaping of the reference (backslash, then both parentheses).
pub fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

fn latin1(text: &str) -> Result<Vec<u8>> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).map_err(|_| Error::NotLatin1(c)))
        .collect()
}

/// `<< {dictionary} /Length n >>\nstream\n{data}\nendstream`.
pub fn stream(dictionary: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = b"<< ".to_vec();
    out.extend_from_slice(dictionary);
    out.extend_from_slice(format!(" /Length {} >>\nstream\n", data.len()).as_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream");
    out
}

fn deflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// Object number to object body (without `n 0 obj` / `endobj`).
pub type Objects = BTreeMap<u32, Vec<u8>>;

/// Content-stream encryption: (object number, bytes) to bytes.
pub type Encrypt<'a> = &'a dyn Fn(u32, &[u8]) -> Vec<u8>;

/// The objects of a PDF 1.4 file: 1 catalog, 2 page tree, 3 font, then per page its page object,
/// its content stream and (with a scan) its image.
pub fn build(pages: &[Page], encrypt: Option<Encrypt<'_>>) -> Result<Objects> {
    let mut objects = Objects::new();
    objects.insert(
        3,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    );
    let mut kids = Vec::new();
    let mut number = 4u32;
    for page in pages {
        let (page_obj, content_obj) = (number, number + 1);
        number += 2;
        kids.push(page_obj);
        let mut ops: Vec<String> = Vec::new();
        let mut resources = b"/Font << /F1 3 0 R >>".to_vec();
        if !page.scan.is_empty() {
            let image_obj = number;
            number += 1;
            let lines: Vec<&str> = page.scan.iter().map(String::as_str).collect();
            let bitmap = render_lines(&lines)?;
            let dictionary = format!(
                "/Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceGray \
                 /BitsPerComponent 8 /Filter /FlateDecode",
                bitmap.width, bitmap.height
            );
            objects.insert(
                image_obj,
                stream(dictionary.as_bytes(), &deflate(&bitmap.pixels)?),
            );
            resources.extend_from_slice(format!(" /XObject << /Im1 {image_obj} 0 R >>").as_bytes());
            ops.push("q 612 0 0 792 0 0 cm /Im1 Do Q".to_owned());
        }
        if !page.text.is_empty() {
            ops.push("BT /F1 18 Tf 72 740 Td 24 TL".to_owned());
            ops.extend(
                page.text
                    .iter()
                    .map(|line| format!("({}) Tj T*", escape(line))),
            );
            ops.push("ET".to_owned());
        }
        let mut content = latin1(&ops.join("\n"))?;
        if let Some(encrypt) = encrypt {
            content = encrypt(content_obj, &content);
        }
        objects.insert(content_obj, stream(b"", &content));
        let mut body =
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << ".to_vec();
        body.extend_from_slice(&resources);
        body.extend_from_slice(format!(" >> /Contents {content_obj} 0 R >>").as_bytes());
        objects.insert(page_obj, body);
    }
    let kids: Vec<String> = kids.iter().map(|k| format!("{k} 0 R")).collect();
    objects.insert(
        2,
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            pages.len()
        )
        .into_bytes(),
    );
    objects.insert(1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    Ok(objects)
}

/// The header both writers start with: version line, then a binary comment.
pub const PDF_HEADER: &[u8] = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n";

/// The xref table and trailer. `trailer` is the extra trailer entries: `serialise` always puts
/// them between two spaces (so an empty one leaves `R  >>`), the ballast writer has none at all
/// (`R >>`), exactly as the reference's two writers differ.
fn xref_and_trailer(
    offsets: &BTreeMap<u32, usize>,
    size: u32,
    trailer: Option<&[u8]>,
    xref: usize,
) -> Result<Vec<u8>> {
    let mut out = format!("xref\n0 {size}\n0000000000 65535 f \n").into_bytes();
    for key in 1..size {
        let offset = offsets.get(&key).ok_or(Error::MissingObject(key))?;
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R ").as_bytes());
    if let Some(trailer) = trailer {
        out.extend_from_slice(trailer);
        out.push(b' ');
    }
    out.extend_from_slice(format!(">>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    Ok(out)
}

fn object_head(key: u32) -> Vec<u8> {
    format!("{key} 0 obj\n").into_bytes()
}

/// The file bytes. `padding` > 0 adds a comment line of that many bytes after the header: a valid
/// but oversized file whose xref offsets still match (the trailer stays at the end).
pub fn serialise(objects: &Objects, trailer: &[u8], padding: usize) -> Result<Vec<u8>> {
    let mut out = PDF_HEADER.to_vec();
    if padding > 0 {
        out.push(b'%');
        out.resize(out.len() + padding.saturating_sub(2), b' ');
        out.push(b'\n');
    }
    let mut offsets = BTreeMap::new();
    for (key, body) in objects {
        offsets.insert(*key, out.len());
        out.extend_from_slice(&object_head(*key));
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    let size = objects.keys().next_back().map_or(1, |m| m + 1);
    out.extend_from_slice(&xref_and_trailer(&offsets, size, Some(trailer), xref)?);
    Ok(out)
}

/// The reference's `pdf(pages, padding)`.
pub fn pdf(pages: &[Page], padding: usize) -> Result<Vec<u8>> {
    serialise(&build(pages, None)?, b"", padding)
}

// -- encrypted ---------------------------------------------------------------------------------

/// The standard security handler's password padding (PDF 32000-1, 7.6.3.3).
pub const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

/// RC4 of `data` under `key` (an empty key is treated as one zero byte).
pub fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let key: &[u8] = if key.is_empty() { &[0] } else { key };
    let mut state: [u8; 256] = core::array::from_fn(|i| i as u8);
    let at = |state: &[u8; 256], i: u8| state.get(usize::from(i)).copied().unwrap_or(0);
    let mut j: u8 = 0;
    for (i, k) in (0..=255u8).zip(key.iter().cycle()) {
        j = j.wrapping_add(at(&state, i)).wrapping_add(*k);
        state.swap(usize::from(i), usize::from(j));
    }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter()
        .map(|byte| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(at(&state, i));
            state.swap(usize::from(i), usize::from(j));
            byte ^ at(&state, at(&state, i).wrapping_add(at(&state, j)))
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn padded(password: &[u8]) -> Vec<u8> {
    password
        .iter()
        .chain(PAD.iter())
        .take(32)
        .copied()
        .collect()
}

pub const DEFAULT_USER_PASSWORD: &[u8] = b"synthetic-user-pw";
pub const DEFAULT_OWNER_PASSWORD: &[u8] = b"synthetic-owner-pw";

/// Standard security handler, revision 2 (RC4, 40-bit), PDF 32000-1 section 7.6.3. The user
/// password is NOT empty: a reader that is not given one must be refused, which is what makes
/// this a real "encrypted" input.
pub fn encrypted_pdf(user_password: &[u8], owner_password: &[u8]) -> Result<Vec<u8>> {
    let document_id = md5::digest(b"noevia synthetic fixture");
    let permissions: u32 = (-4i32) as u32;
    let owner_key = md5::digest(&padded(owner_password));
    let owner = rc4(
        owner_key.get(..5).unwrap_or_default(),
        &padded(user_password),
    );
    let mut material = padded(user_password);
    material.extend_from_slice(&owner);
    material.extend_from_slice(&permissions.to_le_bytes());
    material.extend_from_slice(&document_id);
    let digest = md5::digest(&material);
    let key = digest.get(..5).unwrap_or_default().to_vec();
    let user = rc4(&key, &PAD);
    let encrypt = |object_number: u32, data: &[u8]| -> Vec<u8> {
        let mut material = key.clone();
        material.extend_from_slice(object_number.to_le_bytes().get(..3).unwrap_or_default());
        material.extend_from_slice(&[0, 0]);
        let object_key = md5::digest(&material);
        rc4(object_key.get(..10).unwrap_or_default(), data)
    };
    let mut objects = build(&[Page::text(&["ENCRYPTED SYNTHETIC PAGE"])], Some(&encrypt))?;
    let encrypt_obj = objects.keys().next_back().map_or(1, |m| m + 1);
    objects.insert(
        encrypt_obj,
        format!(
            "<< /Filter /Standard /V 1 /R 2 /P -4 /O <{}> /U <{}> >>",
            hex(&owner),
            hex(&user)
        )
        .into_bytes(),
    );
    let id = hex(&document_id);
    let trailer = format!("/Encrypt {encrypt_obj} 0 R /ID [<{id}> <{id}>]");
    serialise(&objects, trailer.as_bytes(), 0)
}

/// A PDF header and then a catalog that never ends: no cross-reference table.
pub fn malformed_pdf() -> Vec<u8> {
    b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 99 0 R\nthis file stops mid-object".to_vec()
}

// -- fixture set -------------------------------------------------------------------------------

/// A valid PDF whose only content is one native text line, padded past 25 MB.
pub fn oversized_text_pdf() -> Result<Vec<u8>> {
    pdf(
        &[Page::text(&["SYNTHETIC NATIVE TEXT FOR REDUCTION"])],
        OVERSIZE_PADDING,
    )
}

/// The reference's default ballast chunk.
pub const BALLAST_CHUNK: usize = 1024 * 1024;

/// noevia#1867: a large synthetic PDF, written to `path` a chunk at a time (never held whole).
///
/// Page 1 is native text, page 2 carries `ballast_bytes` (rounded down to whole 4096-pixel rows,
/// at least one row) of an uncompressed flat mid-gray image, and page 3 is the scan of [`ROWS`]
/// with no text layer. Rendering page 3 must not need page 2's bytes: that is what a ranged OCR
/// relies on. Returns the file size.
pub fn write_ballast_pdf(path: &Path, ballast_bytes: usize, chunk: usize) -> Result<u64> {
    if chunk == 0 {
        return Err(Error::ZeroChunk);
    }
    let width = 4096usize;
    let height = (ballast_bytes / width).max(1);
    let mut objects = build(
        &[
            Page::text(&["NATIVE PAGE ONE"]),
            Page::text(&["BALLAST PAGE"]),
            Page::scan(&ROWS),
        ],
        None,
    )?;
    let ballast = objects.keys().next_back().map_or(1, |m| m + 1);
    // Page 2 is objects 6 (page) and 7 (content): draw the ballast image on it.
    if let Some(page) = objects.get_mut(&6) {
        *page = replace_all(
            page,
            b"/Font << /F1 3 0 R >>",
            format!("/Font << /F1 3 0 R >> /XObject << /Im2 {ballast} 0 R >>").as_bytes(),
        );
    }
    objects.insert(7, stream(b"", b"q 612 0 0 792 0 0 cm /Im2 Do Q"));
    let head = format!(
        "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceGray \
         /BitsPerComponent 8 /Length {} >>\nstream\n",
        width * height
    );
    let file = std::fs::File::create(path)?;
    let mut out = std::io::BufWriter::new(file);
    let mut written = 0usize;
    let put = |out: &mut std::io::BufWriter<std::fs::File>, bytes: &[u8]| -> Result<usize> {
        out.write_all(bytes)?;
        Ok(bytes.len())
    };
    written += put(&mut out, PDF_HEADER)?;
    let mut offsets = BTreeMap::new();
    for (key, body) in &objects {
        offsets.insert(*key, written);
        written += put(&mut out, &object_head(*key))?;
        written += put(&mut out, body)?;
        written += put(&mut out, b"\nendobj\n")?;
    }
    offsets.insert(ballast, written);
    written += put(&mut out, &object_head(ballast))?;
    written += put(&mut out, head.as_bytes())?;
    let row = vec![0x80u8; chunk];
    let mut left = width * height;
    while left > 0 {
        let n = chunk.min(left);
        written += put(&mut out, row.get(..n).unwrap_or_default())?;
        left -= n;
    }
    written += put(&mut out, b"\nendstream\nendobj\n")?;
    let xref = written;
    written += put(
        &mut out,
        &xref_and_trailer(&offsets, ballast + 1, None, xref)?,
    )?;
    out.flush()?;
    Ok(written as u64)
}

fn replace_all(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len() + with.len());
    let mut rest = haystack;
    while !needle.is_empty() {
        match rest.windows(needle.len()).position(|w| w == needle) {
            Some(at) => {
                out.extend_from_slice(rest.get(..at).unwrap_or_default());
                out.extend_from_slice(with);
                rest = rest.get(at + needle.len()..).unwrap_or_default();
            }
            None => break,
        }
    }
    out.extend_from_slice(rest);
    out
}

/// The fixture names [`write_all`] writes, in the reference's order.
pub const FIXTURES: [&str; 6] = [
    "scanned.pdf",
    "mixed-page.pdf",
    "mixed-pages.pdf",
    "text.pdf",
    "malformed.pdf",
    "encrypted.pdf",
];

/// The bytes of the fixture `name` (one of [`FIXTURES`]).
pub fn fixture(name: &str) -> Option<Result<Vec<u8>>> {
    Some(match name {
        "scanned.pdf" => pdf(&[Page::scan(&ROWS)], 0),
        "mixed-page.pdf" => pdf(&[Page::mixed(&ROWS, &[HEADER])], 0),
        "mixed-pages.pdf" => pdf(&[Page::text(&["NATIVE PAGE ONE"]), Page::scan(&ROWS)], 0),
        "text.pdf" => pdf(&[Page::text(&["SYNTHETIC NATIVE TEXT FOR REDUCTION"])], 0),
        "malformed.pdf" => Ok(malformed_pdf()),
        "encrypted.pdf" => encrypted_pdf(DEFAULT_USER_PASSWORD, DEFAULT_OWNER_PASSWORD),
        _ => return None,
    })
}

/// Write every fixture into `directory` (created if needed); returns (name, path) in order.
pub fn write_all(directory: &Path) -> Result<Vec<(&'static str, PathBuf)>> {
    std::fs::create_dir_all(directory)?;
    let mut paths = Vec::new();
    for name in FIXTURES {
        let data = fixture(name).ok_or(Error::MissingObject(0))??;
        let path = directory.join(name);
        std::fs::write(&path, data)?;
        paths.push((name, path));
    }
    Ok(paths)
}
