//! Capability probes do not start the server or confirm an incomplete probe.
#![allow(clippy::expect_used)]

use std::process::{Command, Stdio};

#[test]
fn features_cli_outputs_only_the_compiled_capabilities() {
    let output = Command::new(env!("CARGO_BIN_EXE_noevia-server"))
        .arg("--features")
        .env("UI_PORT", "invalid-port")
        .output()
        .expect("run capability CLI");
    assert!(output.status.success());
    let expected = noevia_server::FEATURES
        .iter()
        .map(|feature| format!("{feature}\n"))
        .collect::<String>();
    assert_eq!(output.stdout, expected.as_bytes());
    assert!(output.stderr.is_empty());
    // Always compiled in: the front strips and re-stamps X-Noevia-Client-Ip (#1578).
    assert!(expected.lines().any(|line| line == "client-address-header"));
    assert_eq!(
        expected.lines().any(|line| line == "rust-storage-reads"),
        cfg!(feature = "rust-storage-reads")
    );
    assert_eq!(
        expected
            .lines()
            .any(|line| line == "rust-reasoning-settings"),
        cfg!(feature = "rust-reasoning-settings")
    );
    assert_eq!(
        expected.lines().any(|line| line == "rust-project-chats"),
        cfg!(feature = "rust-project-chats")
    );
    assert_eq!(
        expected.lines().any(|line| line == "rust-free-chats"),
        cfg!(feature = "rust-free-chats")
    );
    assert_eq!(
        expected.lines().any(|line| line == "rust-project-writes"),
        cfg!(feature = "rust-project-writes")
    );
}

#[cfg(unix)]
#[test]
fn features_cli_closed_stdout_fails_without_panicking() {
    let (reader, writer) = std::os::unix::net::UnixStream::pair().expect("synthetic stdout pair");
    drop(reader);
    let output = Command::new(env!("CARGO_BIN_EXE_noevia-server"))
        .arg("--features")
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .output()
        .expect("run closed stdout CLI");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
fn replay_probe(front: &str) -> std::process::Output {
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/front/replay-front.sh");
    Command::new("sh")
        .arg("-x")
        .arg(script)
        .args(["/nonexistent-synthetic-core", front])
        .env("UI_PORT", "18021")
        .env("NOEVIA_RUST_AUTH", "1")
        .env("NOEVIA_RUST_USAGE", "1")
        .env("NOEVIA_RUST_SAMPLING_SETTINGS", "1")
        .env("NOEVIA_RUST_REASONING_SETTINGS", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run replay capability refusal")
}

#[cfg(unix)]
#[test]
fn replay_refuses_failed_capability_producer_before_launching() {
    let output = replay_probe("/usr/bin/false");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("ASCII refusal");
    assert!(stderr.contains("capability probe failed"));
    assert!(!stderr.contains("corpus-launcher"));
}

#[cfg(all(unix, not(feature = "rust-reasoning-settings")))]
#[test]
fn featureless_replay_checks_all_switches_from_one_complete_probe() {
    let output = replay_probe(env!("CARGO_BIN_EXE_noevia-server"));
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("ASCII refusal");
    assert!(stderr.contains("no rust-reasoning-settings"));
    assert_eq!(stderr.matches(" --features").count(), 1);
    for feature in [
        "rust-auth",
        "rust-projects",
        "rust-project-chats",
        "rust-free-chats",
        "rust-usage",
        "rust-sampling-settings",
    ] {
        assert!(stderr.contains(&format!("+ has_feature {feature}")));
    }
    assert!(!stderr.contains("corpus-launcher"));
    assert!(!stderr.contains("panicked"));
}
