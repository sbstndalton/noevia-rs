//! The CLI contract the Diary relies on: exit codes, the exact output shapes, the stdin cap.

// Test code: a panic is the failure report; fixture offsets come from archives built here.
#![allow(clippy::panic, clippy::indexing_slicing)]

use std::io::Write;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_workspace-restore-verify");

fn run(args: &[&str], stdin: &[u8]) -> (Option<i32>, String) {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn: {e}"));
    if let Some(mut input) = child.stdin.take() {
        let _ = input.write_all(stdin);
    }
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn self_test_prints_ok() {
    assert_eq!(run(&["self-test"], b""), (Some(0), "ok\n".into()));
}

#[test]
fn garbage_is_a_refusal_with_exit_1() {
    assert_eq!(
        run(&["verify"], b"not a zip"),
        (Some(1), "reject: invalid\n".into())
    );
}

#[test]
fn bad_usage_prints_nothing_and_exits_2() {
    for args in [&[][..], &["check"][..], &["verify", "extra"][..]] {
        assert_eq!(run(args, b""), (Some(2), String::new()), "{args:?}");
    }
}

#[test]
fn input_past_the_cap_is_archive_too_large() {
    let body = vec![0u8; 272 * 1024 * 1024 + 1];
    assert_eq!(
        run(&["verify"], &body),
        (Some(1), "reject: archive_too_large\n".into())
    );
}

#[test]
fn an_accepted_archive_prints_only_the_listing_digest() {
    let text = include_str!(
        "../../../crates/workspace-restore-verify/tests/fixtures/workspace-restore.v1.json"
    );
    let doc: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
    let case = doc
        .get("cases")
        .and_then(serde_json::Value::as_array)
        .and_then(|c| {
            c.iter().find(|c| {
                c.get("name").and_then(serde_json::Value::as_str) == Some("accept/export")
            })
        })
        .cloned()
        .unwrap_or_default();
    use base64::Engine;
    let body = base64::engine::general_purpose::STANDARD
        .decode(
            case.get("archive_b64")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
        )
        .unwrap_or_default();
    let (code, out) = run(&["verify"], &body);
    assert_eq!(code, Some(0));
    assert_eq!(
        out,
        format!(
            "{}\n",
            case.get("rust")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<missing>")
        )
    );
    assert!(out.starts_with("accept ") && out.len() == "accept ".len() + 64 + 1);
}
