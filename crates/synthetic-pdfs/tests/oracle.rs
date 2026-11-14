//! Parity with the reference generator (services `ocr/synthetic_pdfs.py`), run live.
//!
//! `SYNTHETIC_PDFS_ORACLE` names the directory holding the reference (CI: the services checkout's
//! `ocr/`); without it this test only says SKIP, and with `SYNTHETIC_PDFS_ORACLE_REQUIRED=1` a
//! missing oracle fails. Every output is byte-identical except deflate data: the image streams
//! are compared after inflating, with their `/Length` and the offsets it moves left out, and the
//! raw scan pixels are compared byte for byte.

mod common;

use std::path::Path;
use std::process::Command;

use synthetic_pdfs::{
    encrypted_pdf, fixture, malformed_pdf, oversized_text_pdf, pdf, render_lines, render_lines_at,
    write_ballast_pdf, Error, Page, Result, FIXTURES, HEADER, IMAGE_H, ROWS, CELL,
};

enum Kind {
    /// Byte-identical.
    Exact,
    /// A PDF with FlateDecode streams: compared through `common::semantic`.
    Semantic,
}

fn compare(name: &str, kind: &Kind, ours: &Result<Vec<u8>>, out: &Path) -> std::result::Result<(), String> {
    let error = std::fs::read_to_string(out.join(format!("{name}.error"))).ok();
    let theirs = std::fs::read(out.join(name)).ok();
    match (ours, error, theirs) {
        (Err(e), Some(t), None) => {
            eprintln!("{name}: both refuse (reference {t}, native {e})");
            Ok(())
        }
        (Ok(ours), None, Some(theirs)) => {
            let same = match kind {
                Kind::Exact => *ours == theirs,
                Kind::Semantic => {
                    let (a, b) = (common::semantic(ours), common::semantic(&theirs));
                    a.is_some() && a == b && common::xref_points_at_objects(ours)
                }
            };
            if same {
                Ok(())
            } else {
                Err(format!(
                    "{name}: output differs ({} native vs {} reference bytes){}",
                    ours.len(),
                    theirs.len(),
                    explain(ours, &theirs)
                ))
            }
        }
        (ours, error, theirs) => Err(format!(
            "{name}: verdicts differ (native {}, reference error {error:?}, reference output {})",
            ours.as_ref().map_or_else(ToString::to_string, |d| format!("{} bytes", d.len())),
            theirs.map_or(0, |t| t.len())
        )),
    }
}

/// Where two PDFs first part, for the failure message.
fn explain(ours: &[u8], theirs: &[u8]) -> String {
    match (common::semantic(ours), common::semantic(theirs)) {
        (Some(a), Some(b)) => {
            if a.0 != b.0 {
                return ": preambles differ".to_owned();
            }
            if a.2 != b.2 || a.3 != b.3 {
                return format!(": trailers differ ({:?} vs {:?})", String::from_utf8_lossy(&a.2), String::from_utf8_lossy(&b.2));
            }
            let first = a.1.iter().zip(&b.1).find(|(x, y)| x != y);
            first.map_or_else(
                || format!(": {} vs {} objects", a.1.len(), b.1.len()),
                |(x, y)| {
                    format!(
                        ": object {} differs: {:?} vs {:?} (data {:?} vs {:?} bytes)",
                        x.0,
                        String::from_utf8_lossy(&x.1),
                        String::from_utf8_lossy(&y.1),
                        x.2.as_ref().map(Vec::len),
                        y.2.as_ref().map(Vec::len)
                    )
                },
            )
        }
        (a, b) => format!(": unparsable (native {}, reference {})", a.is_some(), b.is_some()),
    }
}

fn pixels(r: Result<synthetic_pdfs::Bitmap>) -> Result<Vec<u8>> {
    r.map(|b| b.pixels)
}

fn ballast(dir: &Path, name: &str, size: usize, chunk: usize) -> Result<Vec<u8>> {
    let path = dir.join(name);
    let written = write_ballast_pdf(&path, size, chunk)?;
    let data = std::fs::read(&path)?;
    if written != data.len() as u64 {
        return Err(Error::MissingObject(0));
    }
    Ok(data)
}

#[test]
fn live_reference_agrees() {
    let Some(reference) = std::env::var_os("SYNTHETIC_PDFS_ORACLE") else {
        assert_ne!(
            std::env::var("SYNTHETIC_PDFS_ORACLE_REQUIRED").as_deref(),
            Ok("1"),
            "SYNTHETIC_PDFS_ORACLE is required here"
        );
        eprintln!("SKIP live oracle: set SYNTHETIC_PDFS_ORACLE to the directory holding synthetic_pdfs.py");
        return;
    };
    let reference = Path::new(&reference);
    assert!(reference.join("synthetic_pdfs.py").is_file(), "no synthetic_pdfs.py in {}", reference.display());
    let out = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let ours_dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let driver = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/oracle.py");
    let status = Command::new("python3")
        .arg("-I")
        .arg(&driver)
        .arg(reference)
        .arg(out.path())
        .status()
        .unwrap_or_else(|e| unreachable!("python3: {e}"));
    assert!(status.success(), "reference driver failed");
    let fixtures = out.path().join("fixtures");
    let mut failures = Vec::new();
    let mut checked = 0;
    for name in FIXTURES {
        let kind = if name.contains("scan") || name.contains("mixed") { Kind::Semantic } else { Kind::Exact };
        let ours = fixture(name).unwrap_or_else(|| unreachable!("{name}"));
        failures.extend(compare(name, &kind, &ours, &fixtures).err());
        checked += 1;
    }
    let p = |lines: &[&str]| -> Vec<Page> { vec![Page::text(lines)] };
    let cases: Vec<(&str, Kind, Result<Vec<u8>>)> = vec![
        ("pixels-rows", Kind::Exact, pixels(render_lines(&ROWS))),
        ("pixels-header", Kind::Exact, pixels(render_lines(&[HEADER]))),
        (
            "pixels-font",
            Kind::Exact,
            pixels(render_lines_at(&["ABCDEFGHIJKLM", "NOPQRSTUVWXYZ", "0123456789 -."], 0, 6, 0)),
        ),
        ("pixels-empty", Kind::Exact, pixels(render_lines(&[]))),
        ("pixels-bottom", Kind::Exact, pixels(render_lines_at(&["TOTAL"], 96, IMAGE_H - 7 * CELL, 3))),
        ("pixels-lowercase", Kind::Exact, pixels(render_lines(&["a"]))),
        ("pixels-too-wide", Kind::Exact, pixels(render_lines(&["X".repeat(66).as_str()]))),
        ("pixels-widest", Kind::Exact, pixels(render_lines(&["X".repeat(65).as_str()]))),
        ("pixels-off-top", Kind::Exact, pixels(render_lines_at(&["A"], 96, 0, 3))),
        ("pdf-none", Kind::Exact, pdf(&[], 0)),
        ("pdf-blank-page", Kind::Exact, pdf(&[Page::default()], 0)),
        ("pdf-empty-lists", Kind::Exact, pdf(&[Page::default()], 0)),
        ("pdf-escapes", Kind::Exact, pdf(&p(&["(x)\\", "a(b)c"]), 0)),
        ("pdf-latin1", Kind::Exact, pdf(&p(&["caf\u{e9}"]), 0)),
        ("pdf-not-latin1", Kind::Exact, pdf(&p(&["\u{20ac}"]), 0)),
        (
            "pdf-three-pages",
            Kind::Semantic,
            pdf(&[Page::mixed(&["A"], &["B"]), Page::text(&["C"]), Page::scan(&["D 1"])], 0),
        ),
        ("pdf-padding-1", Kind::Exact, pdf(&p(&["X"]), 1)),
        ("pdf-padding-2", Kind::Exact, pdf(&p(&["X"]), 2)),
        ("pdf-padding-3", Kind::Exact, pdf(&p(&["X"]), 3)),
        ("pdf-padding-100000", Kind::Exact, pdf(&p(&["X"]), 100_000)),
        ("encrypted-empty", Kind::Exact, encrypted_pdf(b"", b"")),
        ("encrypted-long", Kind::Exact, encrypted_pdf(&[b'u'; 40], b"o")),
        ("oversized", Kind::Exact, oversized_text_pdf()),
        ("malformed", Kind::Exact, Ok(malformed_pdf())),
        ("ballast-300k", Kind::Semantic, ballast(ours_dir.path(), "a", 300_000, 1024 * 1024)),
        ("ballast-tiny", Kind::Semantic, ballast(ours_dir.path(), "b", 10, 1024 * 1024)),
        ("ballast-chunked", Kind::Semantic, ballast(ours_dir.path(), "c", 300_000, 7_777)),
    ];
    for (name, kind, ours) in &cases {
        failures.extend(compare(name, kind, ours, out.path()).err());
        checked += 1;
    }
    // Negative control: the comparison must notice a one-pixel change in a scan.
    let mut mutant = pixels(render_lines(&ROWS)).unwrap_or_default();
    if let Some(px) = mutant.iter_mut().find(|p| **p == 0) {
        *px = 1;
    }
    assert!(compare("pixels-rows", &Kind::Exact, &Ok(mutant), out.path()).is_err(), "negative control");
    let mutant_pdf = pdf(&[Page::scan(&["REF-2O42"])], 0);
    assert!(compare("scanned.pdf", &Kind::Semantic, &mutant_pdf, &fixtures).is_err(), "negative control");
    assert!(failures.is_empty(), "{failures:#?}");
    eprintln!("live oracle: {checked} cases agree");
}
