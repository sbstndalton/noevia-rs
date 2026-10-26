//! Capability probes do not start the server or confirm an incomplete probe.
#![allow(clippy::expect_used)]

use std::process::{Command, Stdio};

// Keep executable-fixture writes and every process launch in this test binary
// disjoint. A concurrent child can inherit a writer before its CLOEXEC boundary.
static CLI_PROBE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn features_cli_outputs_only_the_compiled_capabilities() {
    let _probe_guard = CLI_PROBE_LOCK.lock().expect("serialize CLI fixtures");
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
    assert!(expected.lines().any(|line| line == "legacy-root-claim"));
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
    let _probe_guard = CLI_PROBE_LOCK.lock().expect("serialize CLI fixtures");
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
        .env("NOEVIA_RUST_PROJECT_WRITES", "0")
        .env("NOEVIA_RUST_PROJECT_WRITES_CONFIRMED", "hostile")
        .env("NOEVIA_RUST_USAGE", "1")
        .env("NOEVIA_RUST_SAMPLING_SETTINGS", "1")
        .env("NOEVIA_RUST_REASONING_SETTINGS", "1")
        // Direct wrapper probes stand in for replay, which normally provides this
        // fresh proof-bound start ID before the wrapper checks route capabilities.
        .env(
            "NOEVIA_REPLAY_NATIVE_MOVE_START",
            "00000000000000000000000000000001",
        )
        .stdin(Stdio::null())
        .output()
        .expect("run replay capability refusal")
}

#[cfg(unix)]
#[test]
fn replay_refuses_failed_capability_producer_before_launching() {
    let _probe_guard = CLI_PROBE_LOCK.lock().expect("serialize CLI fixtures");
    let output = replay_probe("/usr/bin/false");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("ASCII refusal");
    assert!(stderr.contains("capability probe failed"));
    assert!(!stderr.contains("corpus-launcher"));
}

#[cfg(all(unix, not(feature = "rust-reasoning-settings")))]
#[test]
fn featureless_replay_refuses_first_missing_permanent_capability() {
    let _probe_guard = CLI_PROBE_LOCK.lock().expect("serialize CLI fixtures");
    let front = env!("CARGO_BIN_EXE_noevia-server");
    let output = replay_probe(front);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("ASCII refusal");
    // Permanent M4 checks precede reasoning. A real featureless front must
    // refuse at the first absent capability, without reaching later guards.
    let order = [
        "rust-auth",
        "rust-projects",
        "rust-project-chats",
        "rust-free-chats",
        "rust-export",
        "project-store",
        "rust-usage",
        "rust-sampling-settings",
        "rust-reasoning-settings",
    ];
    let first = order
        .iter()
        .position(|feature| !noevia_server::FEATURES.contains(feature))
        .expect("this profile omits reasoning or an earlier permanent route");
    let missing = *order
        .get(first)
        .expect("first missing permanent capability");
    let suffix = if [
        "rust-projects",
        "rust-project-chats",
        "rust-free-chats",
        "rust-export",
        "project-store",
    ]
    .contains(&missing)
    {
        " (the project routes are the front's, #1071)"
    } else {
        ""
    };
    let refusal = format!("replay-front: {front} has no {missing}{suffix}");
    assert!(stderr.lines().any(|line| line == refusal), "{stderr}");
    assert_eq!(stderr.matches(" --features").count(), 1);
    let reached: Vec<&str> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("+ has_feature "))
        .collect();
    assert_eq!(
        reached,
        order
            .get(..=first)
            .expect("all guards through the first refusal")
    );
    assert!(!stderr.contains("corpus-launcher"));
    assert!(!stderr.contains("panicked"));
}

#[cfg(unix)]
#[test]
fn replay_checks_reasoning_omission_from_one_complete_probe() {
    use std::os::unix::fs::PermissionsExt;
    let _probe_guard = CLI_PROBE_LOCK.lock().expect("serialize CLI fixtures");
    let dir = tempfile::tempdir().expect("private reasoning probe directory");
    let probe = dir.path().join("front");
    // Keep the original reasoning-specific proof in every build profile: all
    // other capabilities are advertised, independently of this binary's flags.
    let features = noevia_server::FEATURES
        .iter()
        .copied()
        .chain(
            noevia_server::config::FRONT_ROUTE_FEATURES
                .iter()
                .map(|(name, _)| *name),
        )
        .filter(|feature| *feature != "rust-reasoning-settings")
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|feature| format!("{feature}\n"))
        .collect::<String>();
    std::fs::write(&probe, format!("#!/bin/sh\nprintf '%s' '{}'", features))
        .expect("write reasoning probe");
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700))
        .expect("reasoning probe mode");
    let output = replay_probe(probe.to_str().expect("UTF-8 reasoning probe"));
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
    for feature in ["rust-export", "project-store"] {
        assert!(stderr.contains(&format!("+ has_feature {feature}")));
    }
    assert!(!stderr.contains("corpus-launcher"));
    assert!(!stderr.contains("panicked"));
}

/// Run this target with the other ten default features and rust-project-writes omitted.
/// The production binary must refuse before touching the synthetic data path or binding.
#[cfg(not(feature = "rust-project-writes"))]
#[test]
fn missing_context_capability_refuses_before_startup_for_every_retired_value() {
    let _probe_guard = CLI_PROBE_LOCK.lock().expect("serialize CLI fixtures");
    let root = tempfile::tempdir().expect("private synthetic root");
    let data = root.path().join("must-not-exist");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve startup port");
    let port = listener.local_addr().expect("local address").port();
    for value in [None, Some(""), Some("0"), Some("1"), Some("hostile")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_noevia-server"));
        command
            .env_clear()
            .env("UI_HOST", "127.0.0.1")
            .env("UI_PORT", port.to_string())
            .env("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9")
            .env("UI_DATA_DIR", &data);
        if let Some(value) = value {
            command
                .env("NOEVIA_RUST_PROJECT_WRITES", value)
                .env("NOEVIA_RUST_PROJECT_WRITES_CONFIRMED", value);
        }
        let output = command.output().expect("run omission control");
        assert_eq!(output.status.code(), Some(2), "{value:?}");
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).expect("UTF-8 refusal");
        assert!(
            error.contains("needs compiled rust-project-writes"),
            "{error}"
        );
        assert!(!error.contains("Address already in use"), "{error}");
        assert!(!data.exists(), "startup mutated the data path");
    }
}

#[cfg(unix)]
#[test]
fn replay_requires_context_capability_even_with_retired_switch_off() {
    use std::os::unix::fs::PermissionsExt;
    let _probe_guard = CLI_PROBE_LOCK.lock().expect("serialize CLI fixtures");
    let dir = tempfile::tempdir().expect("private probe directory");
    let probe = dir.path().join("front");
    let features = noevia_server::FEATURES
        .iter()
        .filter(|f| **f != "rust-project-writes")
        .map(|f| format!("{f}\n"))
        .collect::<String>();
    // Use the default/full build for this specific omission, with every earlier capability present.
    if noevia_server::config::FRONT_ROUTE_FEATURES
        .iter()
        .any(|(f, on)| *f != "rust-project-writes" && !on)
    {
        return;
    }
    std::fs::write(&probe, format!("#!/bin/sh\nprintf '%s' '{}'", features)).expect("write probe");
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700)).expect("probe mode");
    let output = replay_probe(probe.to_str().expect("UTF-8 probe"));
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr).expect("UTF-8 refusal");
    assert!(error.contains("no rust-project-writes"), "{error}");
    assert!(!error.contains("corpus-launcher"), "{error}");
}
