#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use std::{os::unix::fs::PermissionsExt, process::Command};
#[cfg(feature = "native-logs")]
static CHILD_PORTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(feature = "native-logs")]
fn child_ports() -> std::sync::MutexGuard<'static, ()> {
    CHILD_PORTS.lock().unwrap()
}
#[cfg(feature = "native-logs")]
#[test]
fn child_port_guard_excludes_second_fixed_port_owner() {
    let _ports = child_ports();
    assert!(matches!(
        CHILD_PORTS.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
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
        .env("PATH", path)
        .env("S1_TEST_MARKER", marker)
        .env_remove("MODEL_MANAGER_IMPL")
        .env_remove("MODEL_MANAGER_HOST_IMPL")
        .env_remove("MODEL_MANAGER_SECTIONS_IMPL")
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
        let mut expected = "native-logs\n".to_owned();
        if cfg!(feature = "native-host") {
            expected.push_str("native-host\n");
        }
        if cfg!(feature = "native-sections") {
            expected.push_str("native-sections\n");
        }
        assert_eq!(output.stdout, expected.as_bytes());
    }
}
#[cfg(feature = "native-logs")]
#[test]
fn capability_closed_stdout_returns_error_without_panic_or_child() {
    let (dir, marker) = owned_uvicorn();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = command(dir.path(), &marker)
        .arg("--features")
        .stdout(std::process::Stdio::from(writer))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!marker.exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"));
    assert!(stderr.contains("BrokenPipe"));
}

#[test]
fn sections_selector_and_stale_default_confirmation() {
    let (dir, marker) = owned_uvicorn();
    for (mode, sections) in [("python", "rust"), ("rust", "invalid")] {
        assert!(!command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", mode)
            .env("MODEL_MANAGER_SECTIONS_IMPL", sections)
            .output()
            .unwrap()
            .status
            .success());
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-sections") {
        let out = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_SECTIONS_IMPL", "rust")
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(!marker.exists());
        assert!(String::from_utf8_lossy(&out.stderr).contains("native-sections"));
    }
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"$NOEVIA_NATIVE_MODEL_MANAGER_SECTIONS_CONFIRMED\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    let out = command(dir.path(), &marker)
        .env("NOEVIA_NATIVE_MODEL_MANAGER_SECTIONS_CONFIRMED", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(42));
    assert_eq!(std::fs::read(&marker).unwrap(), b"");
}
#[cfg(all(target_os = "linux", feature = "native-sections"))]
#[test]
fn selected_sections_confirm_child_without_python_startup() {
    let _ports = child_ports();
    let (dir, marker) = owned_uvicorn();
    let models = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("uvicorn"), "#!/bin/sh\nprintf '%s' \"$NOEVIA_NATIVE_MODEL_MANAGER_SECTIONS_CONFIRMED\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    let output = command(dir.path(), &marker)
        .env("MODEL_MANAGER_IMPL", "rust")
        .env("MODEL_MANAGER_SECTIONS_IMPL", "rust")
        .env("MODELS_DIR", models.path())
        .env("MODELS_INI_PATH", models.path().join("models.ini"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Python-owned endpoint process exited")
    );
    assert_eq!(std::fs::read(marker).unwrap(), b"1");
    assert_eq!(std::fs::read_dir(models.path()).unwrap().count(), 0);
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
    let _ports = child_ports();
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

#[test]
fn host_selector_requires_compiled_capability_and_native_parent() {
    let (dir, marker) = owned_uvicorn();
    for (mode, host) in [("python", "rust"), ("rust", "invalid")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", mode)
            .env("MODEL_MANAGER_HOST_IMPL", host)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-host") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_HOST_IMPL", "rust")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!marker.exists());
        assert!(String::from_utf8_lossy(&output.stderr).contains("native-host"));
    }
}
