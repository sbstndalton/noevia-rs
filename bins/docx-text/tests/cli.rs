//! The CLI contract the OCR service relies on: JSON on stdout and exit 0; or nothing on
//! stdout, `docx-text: refused: <class>` on stderr and exit 1; exit 2 for bad usage.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run(args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_docx-text"))
        .args(args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn docx-text");
    let mut pipe = child.stdin.take().unwrap();
    // A refused oversized input may close stdin early; that is fine.
    let _ = pipe.write_all(stdin);
    drop(pipe);
    child.wait_with_output().unwrap()
}

/// The fixture file's "paragraphs" case, decoded by the differential test's rules.
fn fixture(name: &str) -> Vec<u8> {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../crates/docx-text/tests/fixtures/docx-text.v1.json"
    ))
    .unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    let case = doc["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap();
    let s = case["docx"].as_str().unwrap();
    let mut out = Vec::new();
    let bytes: Vec<u32> = s
        .bytes()
        .filter(|&c| c != b'=')
        .map(|c| match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            _ => 63,
        })
        .collect();
    for chunk in bytes.chunks(4) {
        let n = chunk.iter().fold(0u32, |a, &c| (a << 6) | c) << (6 * (4 - chunk.len()));
        out.extend_from_slice(&n.to_be_bytes()[1..chunk.len()]);
    }
    out
}

#[test]
fn extract_prints_the_python_shaped_object() {
    let out = run(&["extract"], &fixture("paragraphs"));
    assert!(out.status.success(), "{:?}", out);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["text"],
        "Synthetic first paragraph.\nSecond, invented.\n\nThird"
    );
    assert_eq!(v["truncated"], false);
    assert_eq!(v["scope"], docx_text_scope());
    assert_eq!(v.as_object().unwrap().len(), 3);
}

fn docx_text_scope() -> &'static str {
    "DOCX body text and tables only; page layout, images, headers, footers, comments and footnotes are not interpreted."
}

#[test]
fn refusal_leaves_stdout_empty_and_names_the_class() {
    let out = run(&["extract"], &fixture("xxe-doctype-entity"));
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "docx-text: refused: xml_declarations\n"
    );
    let out = run(&["extract"], b"not a zip");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn empty_zip_is_the_documented_smoke_test() {
    let mut eocd = b"PK\x05\x06".to_vec();
    eocd.extend_from_slice(&[0; 18]);
    let out = run(&["extract"], &eocd);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "docx-text: refused: missing\n"
    );
}

#[test]
fn oversized_stdin_is_refused() {
    let out = run(&["extract"], &vec![b'P'; 25 * 1024 * 1024 + 1]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stderr)
        .unwrap()
        .contains("refused: input"));
}

#[test]
fn bad_usage_exits_2() {
    for args in [
        &[][..],
        &["--help"][..],
        &["extract", "x"][..],
        &["tree"][..],
    ] {
        let out = run(args, b"");
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8(out.stderr).unwrap().contains("usage"));
        assert!(out.stdout.is_empty());
    }
}
