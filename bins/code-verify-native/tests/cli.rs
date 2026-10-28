//! Ordinary-user startup refusals only. No privileged execution or host cleanup test.
use std::io::Write;
use std::process::{Command, Stdio};
#[test]
fn host_invocation_and_forged_helper_input_cannot_run_repository_code(
) -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    for args in [
        vec![],
        vec!["--owned-helper"],
        vec!["--private-canary-unsupported"],
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_code-verify-native"))
            .args(args)
            .current_dir(root.path())
            .env_clear()
            .env("VERIFY_SOURCE_ROOT", root.path())
            .env("VERIFY_SOCKET", root.path().join("must-not-bind.sock"))
            .env("VERIFY_GIT_USER", "1002:1005")
            .env("VERIFY_TEST_USER", "1003:1003")
            .env("CODE_VERIFY", "app|touch must-not-execute")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // A guessed stdin plan is not an authenticated namespace-init bootstrap socket.
        let _ = child
            .stdin
            .take()
            .ok_or("stdin")?
            .write_all(b"{\"operation\":\"Exec\",\"command\":\"private-canary\"}\n");
        let output = child.wait_with_output()?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let text = String::from_utf8(output.stderr)?;
        assert!(text.contains("isolated verifier unavailable"));
        assert!(!text.contains("private-canary"));
        assert!(!root.path().join("must-not-execute").exists());
        assert!(!root.path().join("must-not-bind.sock").exists());
    }
    Ok(())
}

#[test]
fn source_probe_is_not_runtime_readiness() -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_code-verify-native"))
        .arg("--capabilities")
        .env_clear()
        .output()?;
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        value.get("probe").and_then(serde_json::Value::as_str),
        Some("source-capability")
    );
    assert_eq!(
        value.get("readiness").and_then(serde_json::Value::as_str),
        Some("requires-isolated-namespace-init")
    );
    assert_eq!(
        value
            .get("nativeVerifier")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        value.get("oneShot").and_then(serde_json::Value::as_bool),
        Some(true)
    );
    Ok(())
}
