//! The CLI contract: JSON summary on stdout + exit 0, or a message on stderr + nonzero exit.
//! Mirrors the never-500 test in test_gguf_hostile_input.py (a hostile header still yields a
//! summary with header_error; a non-GGUF file is refused).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gguf-meta"))
        .args(args)
        .output()
        .unwrap()
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/gguf/tests/fixtures")
        .join(name)
}

fn tmp(name: &str, bytes: &[u8]) -> PathBuf {
    let p = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn prints_the_reference_summary_for_a_fixture() {
    let out = run(&[fixture("llama_full.gguf").as_os_str()]);
    assert!(out.status.success());
    let actual: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture("llama_full.json")).unwrap())
            .unwrap();
    assert_eq!(actual, expected);
    assert!(out.stderr.is_empty());
}

#[test]
fn hostile_header_still_exits_zero_with_header_error() {
    let mut bytes = b"GGUF".to_vec();
    bytes.extend_from_slice(&3u32.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&1u64.to_le_bytes());
    let key = b"general.architecture";
    bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
    bytes.extend_from_slice(key);
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&u64::MAX.to_le_bytes());
    bytes.extend_from_slice(&[b'x'; 64]);
    let p = tmp("cli-evil-Q4_K_M.gguf", &bytes);
    let out = run(&[p.as_os_str()]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["general"]["header_error"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
}

#[test]
fn non_gguf_file_fails_with_a_message() {
    let mut bytes = b"NOPE".to_vec();
    bytes.extend_from_slice(&[0u8; 64]);
    let p = tmp("cli-notgguf-Q4_K_M.gguf", &bytes);
    let out = run(&[p.as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not a GGUF file (magic=b'NOPE')"), "{err}");
}

#[test]
fn summarize_failure_exits_nonzero() {
    let out = run(&[fixture("quant_list_raises.gguf").as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unhashable"));
}

#[test]
fn non_finite_int_field_exits_zero_with_null() {
    // noevia#913: NaN/Inf in an integer-coerced field is null, as in gguf_meta.py since #901.
    let out = run(&[fixture("nonfinite_context_length_nan.gguf").as_os_str()]);
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["model"]["context_length"].is_null());
    assert_eq!(v["arch"], "llama");
}

#[test]
fn missing_file_and_directory_fail() {
    let missing = Path::new(env!("CARGO_TARGET_TMPDIR")).join("does-not-exist.gguf");
    assert_eq!(run(&[missing.as_os_str()]).status.code(), Some(1));
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let out = run(&[dir.as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!out.stderr.is_empty());
}

#[test]
fn bad_usage_exits_two() {
    assert_eq!(run(&[]).status.code(), Some(2));
    let a = fixture("llama_full.gguf");
    assert_eq!(run(&[a.as_os_str(), a.as_os_str()]).status.code(), Some(2));
    assert_eq!(run(&["--help".as_ref()]).status.code(), Some(2));
}
