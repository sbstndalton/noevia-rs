//! The `synthetic-pdfs` binary: the reference's `python synthetic_pdfs.py` and the one-liners the
//! noevia-ocr tests run.

use std::process::{Command, Output};

fn bin(args: &[&str], dir: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_synthetic-pdfs"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| unreachable!("{e}"))
}

#[test]
fn no_arguments_writes_every_fixture_here_like_the_reference() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let out = bin(&[], dir.path());
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let names: Vec<&str> = stdout.lines().filter_map(|l| l.split(' ').next()).collect();
    assert_eq!(names, synthetic_pdfs::FIXTURES);
    for line in stdout.lines() {
        let mut parts = line.split(' ');
        let (name, size) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        let actual = std::fs::metadata(dir.path().join(name)).map(|m| m.len().to_string());
        assert_eq!(actual.ok().as_deref(), Some(size), "{line}");
    }
}

#[test]
fn write_all_into_a_directory_names_the_paths() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let out = bin(&["write-all", "sub/dir"], dir.path());
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("sub/dir/scanned.pdf "), "{stdout}");
    assert!(dir.path().join("sub/dir/encrypted.pdf").is_file());
}

#[test]
fn oversized_and_ballast_write_what_the_library_writes() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    assert!(bin(&["oversized", "big.pdf"], dir.path()).status.success());
    let big = std::fs::read(dir.path().join("big.pdf")).unwrap_or_default();
    assert!(big.len() > 25 * 1024 * 1024);
    assert_eq!(Some(big), synthetic_pdfs::oversized_text_pdf().ok());
    let out = bin(&["ballast", "b.pdf", "50000", "999"], dir.path());
    assert!(out.status.success());
    let size = std::fs::metadata(dir.path().join("b.pdf"))
        .map(|m| m.len())
        .unwrap_or(0);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        size.to_string()
    );
}

#[test]
fn bad_arguments_fail_with_the_usage() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    for args in [
        &["nope"][..],
        &["ballast", "x.pdf"],
        &["ballast", "x.pdf", "ten"],
        &["ballast", "x.pdf", "10", "0"],
    ] {
        let out = bin(args, dir.path());
        assert!(!out.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).starts_with("synthetic-pdfs: "),
            "{args:?}"
        );
    }
    assert!(
        !dir.path().join("x.pdf").exists() || std::fs::metadata(dir.path().join("x.pdf")).is_ok()
    );
}

#[test]
fn skip_reason_prints_one_line() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let out = bin(&["skip-reason"], dir.path());
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.lines().count(), 1);
    let line = stdout.lines().next().unwrap_or_default();
    assert!(
        line.is_empty()
            || line.starts_with("real-engine OCR/PDF tests need what is not installed here: ")
    );
}
