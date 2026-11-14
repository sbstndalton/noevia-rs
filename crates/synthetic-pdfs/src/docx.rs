//! A minimal DOCX for the OCR front gate (the body fixture of noevia-core `tools/ocr-front/fixtures.py`):
//! a ZIP of two DEFLATE entries, written by hand so the crate needs no archive dependency.
//!
//! The reference writes it with Python's `zipfile` (`ZIP_DEFLATED`, the current local time). The
//! bytes here differ (zlib vs the locked pure-Rust deflate, and a fixed 1980-01-01 timestamp, so
//! the file is the same on every run); the entries, their names, order and contents are the same,
//! which is what the OCR worker's DOCX reader sees.

use std::io::Write;

/// `[Content_Types].xml` of the reference fixture, verbatim.
pub const CONTENT_TYPES: &str = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;

/// `word/document.xml` of the reference fixture, verbatim.
pub const DOCUMENT: &str = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>SYNTHETIC DOCX REF 1271</w:t></w:r></w:p></w:body></w:document>"#;

/// The text the body fixture carries.
pub const BODY_TEXT: &str = "SYNTHETIC DOCX REF 1271";

/// MS-DOS date of 1980-01-01 (the earliest a ZIP can say), time 00:00:00.
const DOS_DATE: u16 = 0x0021;
const DOS_TIME: u16 = 0;
/// "Version needed to extract": 2.0, the first with DEFLATE (what `zipfile` writes too).
const VERSION: u16 = 20;
const DEFLATE: u16 = 8;

fn u16le(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn u32le(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn small(n: usize) -> std::io::Result<u32> {
    u32::try_from(n).map_err(|_| std::io::Error::other("ZIP64 is not written here"))
}

/// A ZIP archive of `entries` (name, contents), each DEFLATE-compressed, in the given order.
pub fn zip(entries: &[(&str, &[u8])]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data)?;
        let compressed = encoder.finish()?;
        let mut crc = flate2::Crc::new();
        crc.update(data);
        let (crc, packed, size) = (crc.sum(), small(compressed.len())?, small(data.len())?);
        let name_len = u16::try_from(name.len()).map_err(std::io::Error::other)?;
        let offset = small(out.len())?;
        // Local file header.
        u32le(&mut out, 0x0403_4b50);
        for v in [VERSION, 0, DEFLATE, DOS_TIME, DOS_DATE] {
            u16le(&mut out, v);
        }
        for v in [crc, packed, size] {
            u32le(&mut out, v);
        }
        u16le(&mut out, name_len);
        u16le(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&compressed);
        // Its central directory record.
        u32le(&mut central, 0x0201_4b50);
        for v in [VERSION, VERSION, 0, DEFLATE, DOS_TIME, DOS_DATE] {
            u16le(&mut central, v);
        }
        for v in [crc, packed, size] {
            u32le(&mut central, v);
        }
        for v in [name_len, 0, 0, 0, 0] {
            u16le(&mut central, v);
        }
        u32le(&mut central, 0);
        u32le(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let count = u16::try_from(entries.len()).map_err(std::io::Error::other)?;
    let (directory_size, directory_offset) = (small(central.len())?, small(out.len())?);
    out.extend_from_slice(&central);
    u32le(&mut out, 0x0605_4b50);
    for v in [0, 0, count, count] {
        u16le(&mut out, v);
    }
    u32le(&mut out, directory_size);
    u32le(&mut out, directory_offset);
    u16le(&mut out, 0);
    Ok(out)
}

/// The OCR front gate's `body.docx`.
pub fn body_docx() -> std::io::Result<Vec<u8>> {
    zip(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("word/document.xml", DOCUMENT.as_bytes()),
    ])
}
