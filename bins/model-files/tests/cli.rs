//! The CLI contract the model manager relies on: JSON on stdout and exit 0, or nothing on
//! stdout, a message on stderr and a nonzero exit.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run(args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_model-files"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn model-files");
    let mut pipe = child.stdin.take().unwrap();
    // A refused oversized input may close stdin early; that is fine.
    let _ = pipe.write_all(stdin);
    drop(pipe);
    child.wait_with_output().unwrap()
}

#[test]
fn tree_prints_files() {
    let out = run(
        &["tree"],
        br#"[{"type":"file","path":"m-Q4_K_M-00001-of-00002.gguf","lfs":{"size":5}},{"type":"directory","path":"x"}]"#,
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"files": [{"path": "m-Q4_K_M-00001-of-00002.gguf", "size": 5,
            "quant": "Q4_K_M", "shard_base": "m-Q4_K_M.gguf", "shard_index": 1, "shard_total": 2}]})
    );
}

#[test]
fn errors_go_to_stderr_with_exit_1() {
    let out = run(
        &["tree"],
        br#"[{"type":"file","path":"a.gguf","size":"nope"}]"#,
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("size is not an integer"));
}

#[test]
fn oversized_stdin_is_refused() {
    let big = vec![b' '; model_files::MAX_INPUT_BYTES + 10];
    let out = run(&["tree"], &big);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn usage_errors_exit_2() {
    for args in [
        &[][..],
        &["--help"][..],
        &["tree", "extra"][..],
        &["nope"][..],
    ] {
        let out = run(args, b"");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("usage"));
    }
}

#[test]
fn unicode_version_is_printed() {
    let out = run(&["unicode-version"], b"");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        model_files::pytables::UNIDATA_VERSION
    );
}
