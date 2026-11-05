use std::io::Write;
use std::process::{Command, Stdio};
#[test]
fn malformed_control_refuses_without_output() -> Result<(), Box<dyn std::error::Error>> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_diary-backup-run"))
        .arg("run-v1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().ok_or("stdin")?.write_all(b"{}\n")?;
    let result = child.wait_with_output()?;
    assert_eq!(result.status.code(), Some(2));
    assert!(result.stdout.is_empty());
    assert_eq!(result.stderr, b"unavailable\n");
    Ok(())
}
