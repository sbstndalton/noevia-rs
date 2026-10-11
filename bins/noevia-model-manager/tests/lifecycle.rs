#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use std::{os::unix::fs::PermissionsExt, process::Command};

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
        .env("PATH", path)
        .env("S1_TEST_MARKER", marker)
        .env_remove("MODEL_MANAGER_IMPL")
        .env_remove("MODEL_LOADER_TOKEN")
        .env_remove("LLAMA_CONTAINERS")
        .env_remove("DOCKER_HOST");
    command
}
#[test]
fn default_selector_execs_existing_python_without_confirmed_ownership() {
    let (dir, marker) = owned_uvicorn();
    let output = command(dir.path(), &marker)
        .env("NOEVIA_NATIVE_MODEL_MANAGER_CONFIRMED", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42));
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        "\napp.main:app\n--host\n0.0.0.0\n--port\n8090\n"
    );
}
#[test]
fn invalid_selector_and_invalid_token_never_spawn_child() {
    let (dir, marker) = owned_uvicorn();
    assert!(!command(dir.path(), &marker)
        .env("MODEL_MANAGER_IMPL", "invalid")
        .output()
        .unwrap()
        .status
        .success());
    assert!(!marker.exists());
    assert!(!command(dir.path(), &marker)
        .env("MODEL_MANAGER_IMPL", "rust")
        .env("MODEL_LOADER_TOKEN", " ")
        .output()
        .unwrap()
        .status
        .success());
    assert!(!marker.exists());
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
        assert_eq!(output.stdout, b"native-logs\n");
    }
}
#[cfg(not(feature = "native-logs"))]
#[test]
fn deliberately_omitted_feature_refuses_before_child_spawn() {
    let (dir, marker) = owned_uvicorn();
    let output = command(dir.path(), &marker)
        .env("MODEL_MANAGER_IMPL", "rust")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("feature is not enabled"));
    assert!(!marker.exists());
}
#[cfg(feature = "native-logs")]
#[test]
fn native_child_uses_loopback_confirmation_and_exit_is_fatal() {
    let (dir, marker) = owned_uvicorn();
    let output = command(dir.path(), &marker)
        .env("MODEL_MANAGER_IMPL", "rust")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Python-owned endpoint process exited")
    );
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        "1\napp.main:app\n--host\n127.0.0.1\n--port\n8091\n"
    );
}
