//! `noevia-diary --version`: one provenance line, exit 0, without reading stdin.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn version_prints_one_line_and_never_reads_stdin() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_noevia-diary"))
        .arg("--version")
        // Held open and never written: a binary that read its start-up config would block here.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("--version waited for stdin");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (mut out, mut err) = (String::new(), String::new());
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(status.success(), "{status:?} {err}");
    let rs_ref = option_env!("NOEVIA_RS_REF")
        .filter(|r| !r.is_empty())
        .unwrap_or("unknown");
    assert_eq!(
        out,
        format!(
            "noevia-diary {} noevia-rs {rs_ref}\n",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert_eq!(err, "", "nothing else is printed");
}
