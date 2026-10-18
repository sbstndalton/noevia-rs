//! Native lifecycle contracts replace retired child-selector/confirmation cases.
//! Historical source remains immutable at routing978660b; direct native process/DB/INI/lock
//! coverage lives in native_service.rs, roots/readiness ordering in routing_capability.rs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use std::{os::unix::fs::PermissionsExt, process::Command};
#[allow(dead_code)]
#[path = "../src/bin/support/prompts_process.rs"]
mod prompts_process;
#[test]
fn prompt_process_pump_caps_stalls_inherited_pipes_and_early_exit() {
    let python = std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into());
    for (code, expected, duration) in [
        (
            "import time;time.sleep(2)",
            "child deadline exceeded",
            std::time::Duration::from_millis(40),
        ),
        (
            "import sys;sys.stderr.write('x'*65537)",
            "child output limit exceeded",
            std::time::Duration::from_secs(2),
        ),
        (
            "import sys;sys.stdout.write('x'*16777217)",
            "child output limit exceeded",
            std::time::Duration::from_secs(5),
        ),
        (
            "import os,time;pid=os.fork();time.sleep(2) if pid==0 else os._exit(0)",
            "child deadline exceeded",
            std::time::Duration::from_millis(40),
        ),
    ] {
        let mut command = Command::new(&python);
        command.env_clear().args(["-c", code]);
        assert_eq!(
            prompts_process::bounded(command, Vec::new(), duration)
                .err()
                .unwrap(),
            expected
        );
    }
    let mut command = Command::new(&python);
    command
        .env_clear()
        .args(["-c", "import sys;print('synthetic');sys.exit(7)"]);
    let output =
        prompts_process::bounded(command, Vec::new(), std::time::Duration::from_secs(2)).unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"synthetic\n");
}
fn owned_uvicorn() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("spawned");
    let script = dir.path().join("uvicorn");
    // Test-only executable observes selector argv/env and immediately exits. It performs no
    // Python startup, DB work, models, network or Docker activity.
    std::fs::write(&script,"#!/bin/sh\nprintf '%s\\n' \"$NOEVIA_NATIVE_MODEL_MANAGER_CONFIRMED\" \"$@\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    (dir, marker)
}
fn command(path: &std::path::Path, marker: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"));
    command
        .env_clear()
        .env("PATH", path)
        .env("S1_TEST_MARKER", marker);
    command
}
#[test]
fn advertised_feature_matches_build_and_never_spawns_child() {
    let (dir, marker) = owned_uvicorn();
    let output = command(dir.path(), &marker)
        .arg("--features")
        .output()
        .unwrap();
    assert_eq!(output.status.success(), cfg!(feature = "native-logs"));
    assert!(!marker.exists());
    if cfg!(feature = "native-logs") {
        let mut expected = "native-logs\n".to_owned();
        if cfg!(feature = "native-host") {
            expected.push_str("native-host\n");
        }
        if cfg!(feature = "native-sections") {
            expected.push_str("native-sections\n");
        }
        if cfg!(feature = "native-db-init") {
            expected.push_str("native-db-init\n");
        }
        if cfg!(feature = "native-prompts") {
            expected.push_str("native-prompts\n");
        }
        if cfg!(feature = "native-settings") {
            expected.push_str("native-settings\n");
        }
        if cfg!(feature = "native-model-updates") {
            expected.push_str("native-model-updates\n");
        }
        if cfg!(feature = "native-model-detail") {
            expected.push_str("native-model-detail\n");
        }
        if cfg!(feature = "native-download-targets") {
            expected.push_str("native-download-targets\n");
        }
        if cfg!(feature = "native-badges") {
            expected.push_str("native-badges\n");
        }
        if cfg!(feature = "native-bench-runs") {
            expected.push_str("native-bench-runs\n");
        }
        if cfg!(feature = "native-section-writes") {
            expected.push_str("native-section-writes\n");
        }
        if cfg!(feature = "native-settings-write") {
            expected.push_str("native-settings-write\n");
        }
        if cfg!(feature = "native-token-test") {
            expected.push_str("native-token-test\n");
        }
        if cfg!(feature = "native-backend-control") {
            expected.push_str("native-backend-control\n");
        }
        if cfg!(feature = "native-benchmark-overview") {
            expected.push_str("native-benchmark-overview\n");
        }
        if cfg!(feature = "native-check-updates") {
            expected.push_str("native-check-updates\n");
        }
        if cfg!(feature = "native-benchmark-runner") {
            expected.push_str("native-benchmark-runner\n");
        }
        if cfg!(feature = "native-routing") {
            expected.push_str("native-routing\n");
        }
        if cfg!(feature = "native-download-plan") {
            expected.push_str("native-download-plan\n");
        }
        if cfg!(feature = "native-downloads") {
            expected.push_str("native-downloads\n");
        }
        if cfg!(feature = "native-model-delete") {
            expected.push_str("native-model-delete\n");
        }
        if cfg!(feature = "native-draft-heads") {
            expected.push_str("native-draft-heads\n");
        }
        if cfg!(feature = "native-bench-observe") {
            expected.push_str("native-bench-observe\n");
        }
        if cfg!(feature = "native-backends") {
            expected.push_str("native-backends\n");
        }
        if cfg!(feature = "native-search") {
            expected.push_str("native-search\n");
        }
        if cfg!(feature = "native-model-list") {
            expected.push_str("native-model-list\n");
        }
        if cfg!(feature = "native-safe-defaults") {
            expected.push_str("native-safe-defaults\n");
        }
        if cfg!(feature = "native-section-autoconfig") {
            expected.push_str("native-section-autoconfig\n");
        }
        if cfg!(feature = "native-telemetry") {
            expected.push_str("native-telemetry\n");
        }
        // noevia#1925: the production image also advertises native-diagnose (printed last).
        if cfg!(feature = "native-diagnose") {
            expected.push_str("native-diagnose\n");
        }
        assert_eq!(output.stdout, expected.as_bytes());
    }
}
#[cfg(feature = "native-logs")]
#[test]
fn capability_closed_stdout_returns_error_without_panic_or_child() {
    let (dir, marker) = owned_uvicorn();
    // noevia#1965: not a pipe whose read end is dropped. Under the parallel harness another
    // test's fork may briefly hold a copy of that read end (until its exec closes it), so the
    // child's first write could land in the pipe buffer and succeed. A socket shut down for
    // writing refuses every write (EPIPE) through any copy of any descriptor.
    let (stdout, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    stdout.shutdown(std::net::Shutdown::Write).unwrap();
    let output = command(dir.path(), &marker)
        .arg("--features")
        .stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
            stdout,
        )))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!marker.exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"));
    assert!(stderr.contains("BrokenPipe"));
}

#[test]
fn retired_implementation_keys_never_change_required_startup_profile() {
    let (dir, marker) = owned_uvicorn();
    for value in ["python", "rust", "invalid", ""] {
        let mut cmd = command(dir.path(), &marker);
        for key in noevia_model_manager::routing_cmd::REQUIRED {
            cmd.env(key, value);
        }
        cmd.env("MODEL_MANAGER_ROUTING_IMPL", value)
            .env("MODEL_MANAGER_IMPL", value)
            .env("MODEL_FILES_IMPL", value);
        let output =
            prompts_process::bounded(cmd, Vec::new(), std::time::Duration::from_secs(3)).unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        let expected = if cfg!(feature = "native-routing") {
            "mandatory native service requires exact TZ=UTC"
        } else {
            "mandatory native-routing capability is not compiled"
        };
        assert!(error.contains(expected), "{value:?}: {error}");
        assert!(
            !marker.exists(),
            "retired implementation selector spawned a child"
        );
    }
}
