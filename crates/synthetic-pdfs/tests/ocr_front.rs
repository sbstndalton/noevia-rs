//! The OCR front gate's inputs (`synthetic-pdfs ocr-front DIR`, port of noevia-core
//! `tools/ocr-front/fixtures.py`): the files, the hand-written DOCX read by the OCR worker's own
//! reader, and, with `OCR_FRONT_FIXTURES_ORACLE` (that script) and `SYNTHETIC_PDFS_ORACLE` (the
//! services `ocr/` it loads), a live comparison with what the script writes.
//! `OCR_FRONT_FIXTURES_ORACLE_REQUIRED=1` turns a missing oracle into a failure.

mod common;

use std::io::Read;
use std::path::Path;
use std::process::Command;

use synthetic_pdfs::docx::{body_docx, zip, BODY_TEXT, CONTENT_TYPES, DOCUMENT};
use synthetic_pdfs::{fixture, oversized_text_pdf, write_ocr_front, OCR_FRONT_FIXTURES};

fn le16(data: &[u8], at: usize) -> usize {
    data.get(at..at + 2)
        .and_then(|b| <[u8; 2]>::try_from(b).ok())
        .map_or(0, |b| usize::from(u16::from_le_bytes(b)))
}

fn le32(data: &[u8], at: usize) -> u32 {
    data.get(at..at + 4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map_or(0, u32::from_le_bytes)
}

/// (name, method, crc, contents) of every entry, read through the central directory and each
/// entry's local header, as an unzip tool would. Only stored and DEFLATE entries are expected.
fn entries(archive: &[u8]) -> Vec<(String, usize, u32, Vec<u8>)> {
    let end = archive
        .windows(4)
        .rposition(|w| w == b"PK\x05\x06")
        .unwrap_or_else(|| unreachable!("no end of central directory"));
    let count = le16(archive, end + 10);
    let mut at = le32(archive, end + 16) as usize;
    let mut out = Vec::new();
    for _ in 0..count {
        assert_eq!(le32(archive, at), 0x0201_4b50, "central directory record");
        let method = le16(archive, at + 10);
        let crc = le32(archive, at + 16);
        let packed = le32(archive, at + 20) as usize;
        let size = le32(archive, at + 24) as usize;
        let (name_len, extra, comment) = (
            le16(archive, at + 28),
            le16(archive, at + 30),
            le16(archive, at + 32),
        );
        let local = le32(archive, at + 42) as usize;
        let name =
            String::from_utf8_lossy(archive.get(at + 46..at + 46 + name_len).unwrap_or_default())
                .into_owned();
        assert_eq!(le32(archive, local), 0x0403_4b50, "local header of {name}");
        let data_at = local + 30 + le16(archive, local + 26) + le16(archive, local + 28);
        let raw = archive.get(data_at..data_at + packed).unwrap_or_default();
        let contents = match method {
            0 => raw.to_vec(),
            8 => {
                let mut inflated = Vec::new();
                flate2::read::DeflateDecoder::new(raw)
                    .read_to_end(&mut inflated)
                    .unwrap_or_else(|e| unreachable!("{e}"));
                inflated
            }
            other => unreachable!("method {other}"),
        };
        assert_eq!(contents.len(), size, "size of {name}");
        out.push((name, method, crc, contents));
        at += 46 + name_len + extra + comment;
    }
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    crc.sum()
}

#[test]
fn writes_the_three_gate_inputs_in_the_scripts_order() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let nested = dir.path().join("a/b");
    let written = write_ocr_front(&nested).unwrap_or_else(|e| unreachable!("{e}"));
    let names: Vec<&str> = written.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, OCR_FRONT_FIXTURES);
    let read =
        |name: &str| std::fs::read(nested.join(name)).unwrap_or_else(|e| unreachable!("{e}"));
    let scanned = fixture("scanned.pdf")
        .unwrap_or_else(|| unreachable!())
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(
        read("scan.pdf"),
        scanned,
        "scan.pdf is pdf([{{'scan': ROWS}}])"
    );
    assert_eq!(
        read("large.pdf"),
        oversized_text_pdf().unwrap_or_else(|e| unreachable!("{e}"))
    );
    assert!(read("large.pdf").len() > 25 * 1024 * 1024);
    let docx = read("body.docx");
    assert_eq!(docx, body_docx().unwrap_or_else(|e| unreachable!("{e}")));
    // The same bytes on every run: no timestamp of now.
    assert_eq!(docx, body_docx().unwrap_or_else(|e| unreachable!("{e}")));
}

#[test]
fn the_docx_is_a_valid_zip_with_the_scripts_two_entries() {
    let docx = body_docx().unwrap_or_else(|e| unreachable!("{e}"));
    let got = entries(&docx);
    let names: Vec<&str> = got.iter().map(|(n, ..)| n.as_str()).collect();
    assert_eq!(names, ["[Content_Types].xml", "word/document.xml"]);
    for ((name, method, crc, contents), want) in got.iter().zip([CONTENT_TYPES, DOCUMENT]) {
        assert_eq!(*method, 8, "{name} is DEFLATE like ZIP_DEFLATED");
        assert_eq!(contents.as_slice(), want.as_bytes(), "{name}");
        assert_eq!(*crc, crc32(contents), "CRC-32 of {name}");
    }
    // Known CRC-32 check value ("123456789" -> cbf43926), so the CRC itself is the standard one.
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
}

#[test]
fn the_ocr_workers_docx_reader_reads_the_body() {
    let docx = body_docx().unwrap_or_else(|e| unreachable!("{e}"));
    let extracted = docx_text::extract_docx(&docx).unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(extracted.text, BODY_TEXT);
    assert!(!extracted.truncated);
}

#[test]
fn zip_edge_cases() {
    // No entries: just the end record, which says 0 entries at offset 0.
    let empty = zip(&[]).unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(empty.len(), 22);
    assert!(entries(&empty).is_empty());
    // An empty entry and a large one round-trip.
    let big = vec![b'x'; 300_000];
    let archive = zip(&[("e", b""), ("big.txt", &big)]).unwrap_or_else(|e| unreachable!("{e}"));
    let got = entries(&archive);
    assert_eq!(got.len(), 2);
    assert!(got
        .first()
        .is_some_and(|(n, _, crc, c)| n == "e" && c.is_empty() && *crc == 0));
    assert!(got.get(1).is_some_and(|(_, _, _, c)| *c == big));
    // Compressed: 300 kB of one byte is far smaller packed.
    assert!(archive.len() < 10_000);
}

#[test]
fn cli_writes_the_gate_inputs_and_prints_path_size_lines() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let target = dir.path().join("fixtures");
    let out = Command::new(env!("CARGO_BIN_EXE_synthetic-pdfs"))
        .arg("ocr-front")
        .arg(&target)
        .output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3);
    for (line, name) in lines.iter().zip(OCR_FRONT_FIXTURES) {
        let size = std::fs::metadata(target.join(name))
            .map(|m| m.len())
            .unwrap_or_else(|e| unreachable!("{e}"));
        assert_eq!(*line, format!("{} {size}", target.join(name).display()));
    }
    let usage = Command::new(env!("CARGO_BIN_EXE_synthetic-pdfs"))
        .arg("ocr-front")
        .output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert!(!usage.status.success());
    assert!(String::from_utf8_lossy(&usage.stderr).contains("ocr-front DIR"));
}

/// The gate's three files written by the script itself.
fn script_output(script: &Path, services_ocr: &Path, out: &Path) {
    let status = Command::new("python3")
        .arg("-I")
        .arg(script)
        .arg(services_ocr)
        .arg(out)
        .status()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert!(status.success(), "fixtures.py failed");
}

#[test]
fn live_script_agrees() {
    let (Some(script), Some(services)) = (
        std::env::var_os("OCR_FRONT_FIXTURES_ORACLE"),
        std::env::var_os("SYNTHETIC_PDFS_ORACLE"),
    ) else {
        assert_ne!(
            std::env::var("OCR_FRONT_FIXTURES_ORACLE_REQUIRED").as_deref(),
            Ok("1"),
            "OCR_FRONT_FIXTURES_ORACLE and SYNTHETIC_PDFS_ORACLE are required here"
        );
        eprintln!(
            "SKIP ocr-front oracle: set OCR_FRONT_FIXTURES_ORACLE to core tools/ocr-front/fixtures.py \
             and SYNTHETIC_PDFS_ORACLE to the services ocr/ directory"
        );
        return;
    };
    let theirs = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    script_output(Path::new(&script), Path::new(&services), theirs.path());
    let ours = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    write_ocr_front(ours.path()).unwrap_or_else(|e| unreachable!("{e}"));
    let read = |dir: &Path, name: &str| {
        std::fs::read(dir.join(name)).unwrap_or_else(|e| unreachable!("{e}"))
    };
    let mut listed: Vec<String> = std::fs::read_dir(theirs.path())
        .unwrap_or_else(|e| unreachable!("{e}"))
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    listed.sort();
    let mut want: Vec<String> = OCR_FRONT_FIXTURES.iter().map(|s| (*s).to_owned()).collect();
    want.sort();
    assert_eq!(listed, want, "the script writes exactly these files");
    // The scan: the same up to the image's deflate bytes (zlib-9 vs the locked deflate).
    let (a, b) = (
        read(ours.path(), "scan.pdf"),
        read(theirs.path(), "scan.pdf"),
    );
    assert!(common::semantic(&a).is_some());
    assert_eq!(common::semantic(&a), common::semantic(&b), "scan.pdf");
    // The oversized text PDF has no deflate stream: byte for byte.
    assert_eq!(
        read(ours.path(), "large.pdf"),
        read(theirs.path(), "large.pdf")
    );
    // The DOCX: the same entries, in order, with the same method and contents, and the same text
    // through the OCR worker's reader (zipfile's timestamps and zlib bytes differ by design).
    let (a, b) = (
        read(ours.path(), "body.docx"),
        read(theirs.path(), "body.docx"),
    );
    assert_eq!(entries(&a), entries(&b), "body.docx entries");
    assert_eq!(docx_text::extract_docx(&a), docx_text::extract_docx(&b));
    // Negative control: a one-character change in the document is seen.
    let mutant = zip(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        (
            "word/document.xml",
            DOCUMENT.replace("1271", "1272").as_bytes(),
        ),
    ])
    .unwrap_or_else(|e| unreachable!("{e}"));
    assert_ne!(entries(&mutant), entries(&b), "negative control");
    eprintln!("ocr-front oracle: scan.pdf, large.pdf and body.docx agree");
}
