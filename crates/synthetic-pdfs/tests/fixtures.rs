//! The reference's own fixture tests (services ocr/test_synthetic_pdfs.py), ported, plus the
//! edge cases the port decides.

mod common;

use std::cell::Cell;

use synthetic_pdfs::{
    build, encrypted_pdf, escape, fixture, glyph, malformed_pdf, missing_engines, pdf, rc4,
    render_lines, render_lines_at, serialise, skip_reason, skip_reason_from, write_all,
    write_ballast_pdf, Error, Page, Probe, CELL, DEFAULT_OWNER_PASSWORD, DEFAULT_USER_PASSWORD,
    FIXTURES, FONT, GLYPH_H, GLYPH_W, HEADER, IMAGE_H, IMAGE_W, OVERSIZE_PADDING, ROWS,
};

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn written() -> (tempfile::TempDir, Vec<(&'static str, std::path::PathBuf)>) {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let files = write_all(dir.path()).unwrap_or_else(|e| unreachable!("{e}"));
    (dir, files)
}

fn read(files: &[(&str, std::path::PathBuf)], name: &str) -> Vec<u8> {
    let path = files
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, p)| p.clone());
    std::fs::read(path.unwrap_or_default()).unwrap_or_else(|e| unreachable!("{name}: {e}"))
}

#[test]
fn every_fixture_is_written_and_small() {
    let (_dir, files) = written();
    let names: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, FIXTURES);
    for (name, _) in &files {
        let data = read(&files, name);
        assert!(data.starts_with(b"%PDF-"), "{name}");
        assert!(data.len() < 100_000, "{name} is {} bytes", data.len());
    }
}

#[test]
fn cross_reference_offsets_point_at_their_objects() {
    let (_dir, files) = written();
    for name in [
        "scanned.pdf",
        "mixed-page.pdf",
        "mixed-pages.pdf",
        "text.pdf",
        "encrypted.pdf",
    ] {
        assert!(
            common::xref_points_at_objects(&read(&files, name)),
            "{name}"
        );
    }
}

#[test]
fn the_scan_is_an_image_with_no_text_operators() {
    let (_dir, files) = written();
    let data = read(&files, "scanned.pdf");
    assert!(contains(&data, b"/Subtype /Image"));
    assert!(!contains(&data, b" Tj"));
    assert!(!contains(&data, b"/Encrypt"));
}

#[test]
fn the_mixed_pages_have_native_text_beside_the_scan() {
    let (_dir, files) = written();
    assert!(contains(
        &read(&files, "mixed-page.pdf"),
        format!("({HEADER}) Tj").as_bytes()
    ));
    let data = read(&files, "mixed-pages.pdf");
    assert!(contains(&data, b"/Count 2"));
    assert!(contains(&data, b"(NATIVE PAGE ONE) Tj"));
    assert!(contains(&data, b"/Subtype /Image"));
}

#[test]
fn the_scan_has_ink_where_the_text_is_and_none_elsewhere() {
    let bitmap = render_lines(&ROWS).unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(bitmap.pixels.len(), bitmap.width * bitmap.height);
    let dark: Vec<usize> = bitmap
        .pixels
        .iter()
        .enumerate()
        .filter(|(_, v)| **v < 128)
        .map(|(i, _)| i)
        .collect();
    assert!(dark.len() > 2000, "{}", dark.len());
    let rows: Vec<usize> = dark.iter().map(|i| i / bitmap.width).collect();
    assert!(rows.iter().min().copied().unwrap_or(0) >= 240 - 12);
    assert!(rows.iter().max().copied().unwrap_or(usize::MAX) < 240 + 4 * 10 * CELL);
}

#[test]
fn every_character_in_the_rows_has_a_glyph_of_the_right_shape() {
    for c in ROWS.concat().chars().chain(HEADER.chars()) {
        let shape = glyph(c).unwrap_or_else(|| unreachable!("no glyph for {c:?}"));
        assert_eq!(shape.len(), GLYPH_H, "{c}");
        assert!(shape.iter().all(|row| row.len() == GLYPH_W), "{c}");
    }
    // Only 0 and O may share a shape (see the comment on the glyph).
    let shapes: Vec<_> = FONT
        .iter()
        .filter(|(c, _)| *c != '0')
        .map(|(_, g)| g)
        .collect();
    for (i, a) in shapes.iter().enumerate() {
        assert!(
            shapes.iter().skip(i + 1).all(|b| a != b),
            "two characters share a glyph"
        );
    }
    assert_eq!(glyph('0'), glyph('O'));
}

#[test]
fn the_encrypted_pdf_declares_standard_security_with_a_user_password() {
    let (_dir, files) = written();
    let data = read(&files, "encrypted.pdf");
    assert!(contains(&data, b"/Encrypt"));
    assert!(contains(&data, b"/Filter /Standard /V 1 /R 2"));
    let text = String::from_utf8_lossy(&data);
    let o = text
        .split("/O <")
        .nth(1)
        .and_then(|s| s.split('>').next())
        .unwrap_or_default();
    let u = text
        .split("/U <")
        .nth(1)
        .and_then(|s| s.split('>').next())
        .unwrap_or_default();
    assert_eq!((o.len(), u.len()), (64, 64)); // 32 bytes each
    assert!(o
        .bytes()
        .chain(u.bytes())
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    // The page text is not readable without the key.
    assert!(!contains(&data, b"ENCRYPTED SYNTHETIC PAGE"));
}

#[test]
fn the_encrypted_content_decrypts_with_the_object_key() {
    // Independent check of the handler: decrypting object 5 with the documented key derivation
    // gives the plain content stream back (RC4 is its own inverse).
    let data = encrypted_pdf(DEFAULT_USER_PASSWORD, DEFAULT_OWNER_PASSWORD)
        .unwrap_or_else(|e| unreachable!("{e}"));
    let parsed = common::parse(&data).unwrap_or_else(|| unreachable!("parse"));
    let content = parsed
        .objects
        .iter()
        .find(|o| o.number == 5)
        .and_then(|o| o.data.clone());
    let plain = pdf(&[Page::text(&["ENCRYPTED SYNTHETIC PAGE"])], 0)
        .unwrap_or_else(|e| unreachable!("{e}"));
    let plain =
        common::parse(&plain).and_then(|p| p.objects.into_iter().find(|o| o.number == 5)?.data);
    let content = content.unwrap_or_default();
    assert_ne!(Some(content.clone()), plain);
    // The same derivation as the handler, spelled out from the spec.
    let pad = synthetic_pdfs::PAD;
    let padded = |p: &[u8]| -> Vec<u8> { p.iter().chain(pad.iter()).take(32).copied().collect() };
    let id = synthetic_pdfs::md5::digest(b"noevia synthetic fixture");
    let owner = rc4(
        synthetic_pdfs::md5::digest(&padded(DEFAULT_OWNER_PASSWORD))
            .get(..5)
            .unwrap_or_default(),
        &padded(DEFAULT_USER_PASSWORD),
    );
    let mut m = padded(DEFAULT_USER_PASSWORD);
    m.extend_from_slice(&owner);
    m.extend_from_slice(&0xFFFF_FFFCu32.to_le_bytes());
    m.extend_from_slice(&id);
    let key = synthetic_pdfs::md5::digest(&m);
    let mut ok = key.get(..5).unwrap_or_default().to_vec();
    ok.extend_from_slice(&[5, 0, 0, 0, 0]);
    let object_key = synthetic_pdfs::md5::digest(&ok);
    assert_eq!(
        Some(rc4(object_key.get(..10).unwrap_or_default(), &content)),
        plain
    );
    // Revision 2's /U is RC4(key, PAD).
    let u = rc4(key.get(..5).unwrap_or_default(), &pad);
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    assert!(contains(&data, format!("/U <{hex}>").as_bytes()));
}

#[test]
fn rc4_matches_known_vectors() {
    // RFC 6229 test vector: key 0x0102030405, first 16 bytes of keystream.
    let stream = rc4(&[1, 2, 3, 4, 5], &[0; 16]);
    let hex: String = stream.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, "b2396305f03dc027ccc3524a0a1118a8");
    // Classic "Key"/"Plaintext" vector.
    let out = rc4(b"Key", b"Plaintext");
    let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, "bbf316e8d940af0ad3");
}

#[test]
fn the_malformed_pdf_has_no_cross_reference_table() {
    let (_dir, files) = written();
    let data = read(&files, "malformed.pdf");
    assert!(data.starts_with(b"%PDF-"));
    assert!(!contains(&data, b"startxref"));
    assert_eq!(data, malformed_pdf());
}

#[test]
fn padding_makes_a_valid_oversized_file() {
    let data = pdf(&[Page::text(&["X"])], 100_000).unwrap_or_else(|e| unreachable!("{e}"));
    assert!(data.len() > 100_000);
    assert!(common::xref_points_at_objects(&data));
    let unpadded = pdf(&[Page::text(&["X"])], 0).unwrap_or_else(|e| unreachable!("{e}"));
    let start = |d: &[u8]| common::parse(d).map(|p| p.startxref).unwrap_or_default();
    // Everything before the xref table moves by exactly the padding (the startxref number grows).
    assert_eq!(start(&data), start(&unpadded) + 100_000);
    const { assert!(OVERSIZE_PADDING > 25 * 1024 * 1024) };
    // padding 1 and 2 are both the shortest comment line, as in the reference.
    for p in [1, 2] {
        let data = pdf(&[Page::text(&["X"])], p).unwrap_or_else(|e| unreachable!("{e}"));
        assert_eq!(start(&data), start(&unpadded) + 2, "padding {p}");
        assert!(common::xref_points_at_objects(&data));
    }
}

#[test]
fn the_ballast_pdf_streams_and_keeps_page_three_independent() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let path = dir.path().join("ballast.pdf");
    // A small chunk makes the writer loop many times; the bytes must not depend on it.
    let size = write_ballast_pdf(&path, 300_000, 7_777).unwrap_or_else(|e| unreachable!("{e}"));
    let data = std::fs::read(&path).unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(size, data.len() as u64);
    assert!(common::xref_points_at_objects(&data));
    let parsed = common::parse(&data).unwrap_or_else(|| unreachable!("parse"));
    let numbers: Vec<u32> = parsed.objects.iter().map(|o| o.number).collect();
    assert_eq!(numbers, (1..=11).collect::<Vec<_>>());
    let image = parsed
        .objects
        .iter()
        .find(|o| o.number == 11)
        .unwrap_or_else(|| unreachable!("11"));
    let rows = 300_000 / 4096;
    assert_eq!(image.data.as_deref().map(<[u8]>::len), Some(4096 * rows));
    assert!(image
        .data
        .as_deref()
        .unwrap_or_default()
        .iter()
        .all(|b| *b == 0x80));
    let page2 = parsed
        .objects
        .iter()
        .find(|o| o.number == 6)
        .unwrap_or_else(|| unreachable!("6"));
    assert!(contains(&page2.head, b"/XObject << /Im2 11 0 R >>"));
    let page3 = parsed
        .objects
        .iter()
        .find(|o| o.number == 8)
        .unwrap_or_else(|| unreachable!("8"));
    assert!(contains(&page3.head, b"/XObject << /Im1 10 0 R >>"));
    assert!(!contains(&page3.head, b"Im2"));
    let other = dir.path().join("other.pdf");
    write_ballast_pdf(&other, 300_000, 1024 * 1024).unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(std::fs::read(&other).ok(), Some(data));
    // Less than one row still writes one row; a zero chunk is refused instead of looping.
    let tiny = dir.path().join("tiny.pdf");
    write_ballast_pdf(&tiny, 10, 1).unwrap_or_else(|e| unreachable!("{e}"));
    let tiny = std::fs::read(&tiny).unwrap_or_default();
    assert!(contains(&tiny, b"/Width 4096 /Height 1 "));
    assert!(matches!(
        write_ballast_pdf(&dir.path().join("z.pdf"), 10, 0),
        Err(Error::ZeroChunk)
    ));
}

#[test]
fn text_is_escaped_and_must_be_latin1() {
    assert_eq!(escape(r"a\b(c)"), r"a\\b\(c\)");
    let data = pdf(&[Page::text(&[r"(x)\"])], 0).unwrap_or_else(|e| unreachable!("{e}"));
    assert!(contains(&data, br"(\(x\)\\) Tj T*"));
    let latin = pdf(&[Page::text(&["caf\u{e9}"])], 0).unwrap_or_else(|e| unreachable!("{e}"));
    assert!(contains(&latin, b"(caf\xe9) Tj"));
    assert!(matches!(
        pdf(&[Page::text(&["\u{20ac}"])], 0),
        Err(Error::NotLatin1('\u{20ac}'))
    ));
}

#[test]
fn rendering_refuses_what_the_reference_cannot_draw() {
    assert!(matches!(render_lines(&["a"]), Err(Error::NoGlyph('a'))));
    let long = "X".repeat(80);
    assert!(matches!(
        render_lines(&[long.as_str()]),
        Err(Error::DoesNotFit)
    ));
    // From x=96 with an 18 px advance, 65 characters fit (96 + 65*18 = 1266) and 66 do not.
    let fits = "X".repeat((IMAGE_W - 96) / 18);
    assert!(render_lines(&[fits.as_str()]).is_ok());
    let over = "X".repeat((IMAGE_W - 96) / 18 + 1);
    assert!(matches!(
        render_lines(&[over.as_str()]),
        Err(Error::DoesNotFit)
    ));
    assert!(matches!(
        render_lines_at(&[], 0, IMAGE_H + 10, 3),
        Err(Error::BandOffPage)
    ));
    let blank = render_lines(&[]).unwrap_or_else(|e| unreachable!("{e}"));
    assert!(blank.pixels.iter().all(|p| *p == 0xff));
    assert_eq!(blank.pixels.len(), IMAGE_W * IMAGE_H);
}

#[test]
fn pages_with_nothing_on_them_and_no_pages_at_all() {
    let empty = pdf(&[Page::default()], 0).unwrap_or_else(|e| unreachable!("{e}"));
    assert!(common::xref_points_at_objects(&empty));
    assert!(contains(&empty, b"<<  /Length 0 >>\nstream\n\nendstream"));
    let none = pdf(&[], 0).unwrap_or_else(|e| unreachable!("{e}"));
    assert!(contains(&none, b"/Kids [] /Count 0"));
    assert!(common::xref_points_at_objects(&none));
    let mut gap = build(&[], None).unwrap_or_else(|e| unreachable!("{e}"));
    gap.insert(9, b"null".to_vec());
    assert!(matches!(
        serialise(&gap, b"", 0),
        Err(Error::MissingObject(4))
    ));
    // The reference's `max(objects)` raises ValueError for no objects at all (noevia#2727 item 4).
    assert!(matches!(
        serialise(&Default::default(), b"", 0),
        Err(Error::NoObjects)
    ));
}

#[test]
fn named_differences_from_the_reference_are_pinned() {
    // `_rc4(b"", data)` raises ZeroDivisionError; the port keys an empty key as one zero byte.
    assert_eq!(rc4(&[], b"abc"), rc4(&[0], b"abc"));
    // Text on the page's last row: the reference's band would be one row and its pixels one row
    // longer than the declared height; the port refuses instead.
    assert!(matches!(
        render_lines_at(&[], 0, IMAGE_H + 2 * CELL - 1, 0),
        Err(Error::BandOffPage)
    ));
}

#[test]
fn every_fixture_name_resolves_and_nothing_else_does() {
    for name in FIXTURES {
        assert!(fixture(name).is_some_and(|r| r.is_ok()), "{name}");
    }
    assert!(fixture("other.pdf").is_none());
}

struct Fake<'a> {
    absent: &'a [&'a str],
    langs: Option<&'a [u8]>,
    asked: Cell<bool>,
}

impl Probe for Fake<'_> {
    fn which(&self, tool: &str) -> bool {
        !self.absent.contains(&tool)
    }
    fn list_langs(&self) -> Option<Vec<u8>> {
        self.asked.set(true);
        self.langs.map(<[u8]>::to_vec)
    }
}

#[test]
fn a_missing_engine_is_named_in_the_skip_reason() {
    let probe = Fake {
        absent: &["gs"],
        langs: Some(b"List of available languages (3):\neng\ndeu\nosd\n"),
        asked: Cell::new(false),
    };
    assert_eq!(missing_engines(&probe), ["gs"]);
    assert!(skip_reason(&probe).contains("gs"));
}

#[test]
fn missing_language_data_is_named_too() {
    let probe = Fake {
        absent: &[],
        langs: Some(b"List of available languages (2):\neng\nosd\n"),
        asked: Cell::new(false),
    };
    assert_eq!(missing_engines(&probe), ["tesseract language data 'deu'"]);
}

#[test]
fn nothing_missing_means_no_skip() {
    let probe = Fake {
        absent: &[],
        langs: Some(b"eng\ndeu\n"),
        asked: Cell::new(false),
    };
    assert_eq!(skip_reason(&probe), "");
}

#[test]
fn a_failing_language_listing_counts_as_no_languages() {
    let probe = Fake {
        absent: &["pdfinfo"],
        langs: None,
        asked: Cell::new(false),
    };
    assert_eq!(
        missing_engines(&probe),
        [
            "pdfinfo",
            "tesseract language data 'eng'",
            "tesseract language data 'deu'"
        ]
    );
    assert_eq!(
        skip_reason(&probe),
        "real-engine OCR/PDF tests need what is not installed here: pdfinfo, \
         tesseract language data 'eng', tesseract language data 'deu'"
    );
}

#[test]
fn without_tesseract_the_languages_are_not_asked_for() {
    let probe = Fake {
        absent: &["tesseract", "gs"],
        langs: None,
        asked: Cell::new(false),
    };
    assert_eq!(missing_engines(&probe), ["tesseract", "gs"]);
    assert!(!probe.asked.get());
    assert_eq!(skip_reason_from(&[]), "");
}
