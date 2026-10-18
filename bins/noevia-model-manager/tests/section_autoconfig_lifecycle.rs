//! noevia#1318: the `MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL` selector at process level: what
//! refuses startup before any child, what falls back to Python with one log line, and the
//! confirmation and settings the private Python child is handed. The child is a shell stand-in
//! that records its environment and exits; nothing reads models, Docker or a network.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use std::{os::unix::fs::PermissionsExt, process::Command};

/// The parent binds fixed ports (8090, and probes 8091): one spawning test at a time.
#[cfg(feature = "native-logs")]
static PORTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

const RECORD: &str = "#!/bin/sh\nprintf '%s\\n' \"${NOEVIA_NATIVE_MODEL_MANAGER_SECTION_AUTOCONFIG_CONFIRMED-unset}\" \"${MODELS_INI_PATH-unset}\" \"${TZ-unset}\" \"${MODELS_DIR-unset}\" \"${DATA_DIR-unset}\" \"${MODELS_HOST_PATH-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n";

fn uvicorn() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("uvicorn");
    std::fs::write(&script, RECORD).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let marker = dir.path().join("spawned");
    (dir, marker)
}

fn command(dir: &std::path::Path, marker: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"));
    command.env_clear();
    command
        .env("PATH", dir)
        .env("S1_TEST_MARKER", marker)
        // Everything the parent might touch stays inside the test folder.
        .env("MODELS_INI_PATH", dir.join("models.ini"))
        .env("DATA_DIR", dir.join("data"))
        .env(
            "DOCKER_HOST",
            format!("unix://{}", dir.join("docker.sock").display()),
        );
    command
}

fn run(cmd: &mut Command) -> (bool, String) {
    let output = cmd.output().unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

const KEY: &str = "MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL";
#[cfg(feature = "native-section-autoconfig")]
const FALLBACK: &str = "GET /api/v1/sections/{name}/autoconfig stays with python";

#[test]
fn values_other_than_python_or_rust_refuse_before_any_child() {
    let (dir, marker) = uvicorn();
    for value in ["invalid", "Rust", "", " rust"] {
        let (ok, stderr) = run(command(dir.path(), &marker).env(KEY, value));
        assert!(!ok);
        assert!(
            stderr.contains(&format!("{KEY} must be python or rust")),
            "{stderr}"
        );
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-section-autoconfig") {
        let (ok, stderr) = run(command(dir.path(), &marker).env(KEY, "rust"));
        assert!(!ok);
        assert!(
            stderr.contains("compiled native-section-autoconfig feature"),
            "{stderr}"
        );
        assert!(!marker.exists());
    }
}

/// Startup stops at the invalid token, a later guard, so no listener or child is started.
#[cfg(feature = "native-section-autoconfig")]
#[test]
fn settings_it_cannot_read_exactly_fall_back_with_one_line_and_never_refuse() {
    let (dir, marker) = uvicorn();
    let check = |extra: &[(&str, &str)]| {
        let mut cmd = command(dir.path(), &marker);
        cmd.env(KEY, "rust").env("MODEL_LOADER_TOKEN", " ");
        for (key, value) in extra {
            cmd.env(key, value);
        }
        let (ok, stderr) = run(&mut cmd);
        assert!(!ok);
        assert!(stderr.contains("MODEL_LOADER_TOKEN"), "{stderr}");
        assert!(!marker.exists());
        stderr.matches(FALLBACK).count()
    };
    assert_eq!(check(&[]), 0);
    assert_eq!(check(&[("MODELS_DIR", "/synthetic/models")]), 0);
    assert_eq!(
        check(&[("LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB", "512")]),
        0
    );
    assert_eq!(check(&[("MODELS_DIR", "/models/")]), 1);
    assert_eq!(check(&[("MODELS_DIR", "models")]), 1);
    assert_eq!(check(&[("models_dir", "/x")]), 1);
    assert_eq!(check(&[("MODELS_INI_PATH", "models.ini")]), 1);
    assert_eq!(
        check(&[("LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB", "1_0")]),
        1
    );
    assert_eq!(check(&[("llamacpp_cache_ram_hard_max_mib", "1")]), 1);
}

#[cfg(feature = "native-logs")]
#[test]
fn the_child_is_told_exactly_whether_the_route_is_native() {
    let _ports = PORTS.lock().unwrap();
    let (dir, marker) = uvicorn();
    let data = dir.path().join("data");
    let ini = dir.path().join("models.ini");
    let record = |extra: &[(&str, &str)]| -> Vec<String> {
        let mut cmd = command(dir.path(), &marker);
        // An inherited confirmation never reaches the child as it was.
        cmd.env(
            "NOEVIA_NATIVE_MODEL_MANAGER_SECTION_AUTOCONFIG_CONFIRMED",
            "1",
        );
        for (key, value) in extra {
            cmd.env(key, value);
        }
        let (ok, stderr) = run(&mut cmd);
        assert!(!ok);
        assert!(
            stderr.contains("Python-owned endpoint process exited"),
            "{stderr}"
        );
        let lines = std::fs::read_to_string(&marker).unwrap();
        std::fs::remove_file(&marker).unwrap();
        lines.lines().map(str::to_owned).collect()
    };
    let (data, ini) = (
        data.to_str().unwrap().to_owned(),
        ini.to_str().unwrap().to_owned(),
    );
    assert_eq!(
        record(&[]),
        ["0", ini.as_str(), "unset", "unset", data.as_str(), "unset"]
    );
    if !cfg!(feature = "native-section-autoconfig") {
        return;
    }
    assert_eq!(
        record(&[(KEY, "rust"), ("MODELS_DIR", "/synthetic/models")]),
        [
            "1",
            ini.as_str(),
            "unset",
            "/synthetic/models",
            data.as_str(),
            "unset"
        ]
    );
    assert_eq!(
        record(&[(KEY, "rust")]),
        [
            "1",
            ini.as_str(),
            "unset",
            "/models",
            data.as_str(),
            "unset"
        ]
    );
    // A fallback is a plain python selector for the child.
    assert_eq!(
        record(&[(KEY, "rust"), ("MODELS_DIR", "/models/")]),
        [
            "0",
            ini.as_str(),
            "unset",
            "/models/",
            data.as_str(),
            "unset"
        ]
    );
}
