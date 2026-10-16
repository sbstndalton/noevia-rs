#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use std::{os::unix::fs::PermissionsExt, process::Command};
#[allow(dead_code)]
#[path = "../src/bin/support/prompts_process.rs"]
mod prompts_process;
// New prompt cases use the same bounded pump as the immutable Python oracle.
// Retained predecessor cases below keep their previous launch implementation.
trait PromptBounded {
    fn prompt_output(&mut self) -> Result<prompts_process::Output, String>;
}
impl PromptBounded for Command {
    fn prompt_output(&mut self) -> Result<prompts_process::Output, String> {
        prompts_process::bounded_with(self, Vec::new(), std::time::Duration::from_secs(12), |_| {})
    }
}
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
        .env_remove("MODEL_MANAGER_PROMPTS_IMPL")
        .env_remove("MODEL_MANAGER_SETTINGS_IMPL")
        .env_remove("MODEL_MANAGER_MODEL_UPDATES_IMPL")
        .env_remove("MODEL_MANAGER_MODEL_DETAIL_IMPL")
        .env_remove("MODEL_LOADER_TOKEN")
        .env_remove("LLAMA_CONTAINERS")
        .env_remove("DOCKER_HOST")
        .env_remove("CHAT_MODEL_IDLE_UNLOAD_SECONDS");
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
fn prompts_selector_omission_and_python_confirmation_are_closed_before_db_or_child() {
    let (dir, marker) = owned_uvicorn();
    let database = dir.path().join("data");
    for (parent, prompts) in [("python", "rust"), ("rust", "invalid"), ("invalid", "rust")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_PROMPTS_IMPL", prompts)
            .env("DATA_DIR", &database)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!database.exists());
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-prompts") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_PROMPTS_IMPL", "rust")
            .env("DATA_DIR", &database)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-prompts"));
        assert!(!database.exists());
        assert!(!marker.exists());
    }
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    let output = command(dir.path(), &marker)
        .env("NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED", "1")
        .prompt_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42));
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "unset");
    assert!(!database.exists());
}

#[test]
fn settings_selector_omission_and_python_confirmation_are_closed_before_db_or_child() {
    let (dir, marker) = owned_uvicorn();
    let database = dir.path().join("data");
    for (parent, settings) in [
        ("python", "rust"),
        ("rust", "invalid"),
        ("rust", "Rust"),
        ("invalid", "rust"),
    ] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_SETTINGS_IMPL", settings)
            .env("DATA_DIR", &database)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{settings}");
        assert!(!database.exists());
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-settings") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_SETTINGS_IMPL", "rust")
            .env("DATA_DIR", &database)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-settings"));
        assert!(!marker.exists());
    }
    // Default python: an inherited confirmation never reaches uvicorn, so Python keeps GET.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_SETTINGS_IMPL", value);
        }
        let output = cmd.prompt_output().unwrap();
        assert_eq!(output.status.code(), Some(42));
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "unset");
        std::fs::remove_file(&marker).unwrap();
    }
    assert!(!database.exists());
}

#[test]
fn model_updates_selector_omission_and_python_confirmation_are_closed_before_db_or_child() {
    let (dir, marker) = owned_uvicorn();
    let database = dir.path().join("data");
    for (parent, updates) in [
        ("python", "rust"),
        ("rust", "invalid"),
        ("rust", "Rust"),
        ("invalid", "rust"),
    ] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_MODEL_UPDATES_IMPL", updates)
            .env("DATA_DIR", &database)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{updates}");
        assert!(!database.exists());
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-model-updates") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_MODEL_UPDATES_IMPL", "rust")
            .env("DATA_DIR", &database)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-model-updates"));
        assert!(!marker.exists());
    }
    // Default python: an inherited confirmation never reaches uvicorn, so Python keeps GET.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_MODEL_UPDATES_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_MODEL_UPDATES_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_MODEL_UPDATES_IMPL", value);
        }
        let output = cmd.prompt_output().unwrap();
        assert_eq!(output.status.code(), Some(42));
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "unset");
        std::fs::remove_file(&marker).unwrap();
    }
    assert!(!database.exists());
}

#[test]
fn model_detail_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, detail) in [
        ("python", "rust"),
        ("rust", "invalid"),
        ("rust", "Rust"),
        ("invalid", "rust"),
    ] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_MODEL_DETAIL_IMPL", detail)
            .env("TZ", "UTC")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{detail}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-model-detail") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_MODEL_DETAIL_IMPL", "rust")
            .env("TZ", "UTC")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-model-detail"));
        assert!(!marker.exists());
    }
    // Default python: an inherited confirmation never reaches uvicorn, so Python keeps the GET
    // and never serves the private runtime call.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DETAIL_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DETAIL_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_MODEL_DETAIL_IMPL", value);
        }
        let output = cmd.prompt_output().unwrap();
        assert_eq!(output.status.code(), Some(42));
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "unset");
        std::fs::remove_file(&marker).unwrap();
    }
}

#[cfg(feature = "native-model-detail")]
#[test]
fn native_model_detail_never_refuses_startup_over_zone_or_roots() {
    // noevia#1889: a zone other than UTC, or roots the native read cannot use, keep Python serving
    // the detail read with one log line; startup goes on (here it stops at the invalid token, a
    // later guard, so no listener or child is started).
    let (dir, marker) = owned_uvicorn();
    let fallback = "GET /api/v1/models/detail stays with python";
    let run = |tz: Option<&str>, extra: &[(&str, &str)], remove: &[&str]| {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_MODEL_DETAIL_IMPL", "rust")
            .env("MODEL_LOADER_TOKEN", " ");
        match tz {
            Some(value) => cmd.env("TZ", value),
            None => cmd.env_remove("TZ"),
        };
        for key in remove {
            cmd.env_remove(key);
        }
        for (key, value) in extra {
            cmd.env(key, value);
        }
        let output = cmd.prompt_output().unwrap();
        assert!(!marker.exists());
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(stderr.contains("MODEL_LOADER_TOKEN"), "{stderr}");
        assert!(!stderr.contains("requires exact TZ"), "{stderr}");
        stderr
    };
    for tz in [None, Some(""), Some("UTC")] {
        assert!(!run(tz, &[], &[]).contains(fallback), "{tz:?}");
    }
    for tz in ["Europe/Berlin", "utc", "UTC0"] {
        let stderr = run(Some(tz), &[], &[]);
        assert_eq!(stderr.matches(fallback).count(), 1, "{tz}: {stderr}");
    }
    for models in [
        "models",
        "/models/",
        "/models//x",
        "/models/./x",
        "/models/../x",
        "",
    ] {
        assert!(
            run(Some("UTC"), &[("MODELS_DIR", models)], &[]).contains(fallback),
            "{models}"
        );
    }
    for alias in ["models_dir", "Models_Ini_Path", "data_dir"] {
        let path = dir.path().join("alias");
        let stderr = run(
            Some("UTC"),
            &[(alias, path.to_str().unwrap())],
            &["DATA_DIR", "MODELS_DIR", "MODELS_INI_PATH"],
        );
        assert!(stderr.contains(fallback), "{alias}");
    }
}

#[cfg(feature = "native-model-updates")]
#[test]
fn native_model_updates_reject_case_folded_roots_before_child() {
    let (dir, marker) = owned_uvicorn();
    for alias in ["data_dir", "models_dir", "Models_Ini_Path"] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_MODEL_UPDATES_IMPL", "rust")
            .env_remove("DATA_DIR")
            .env_remove("MODELS_DIR")
            .env_remove("MODELS_INI_PATH")
            .env(alias, dir.path().join("alias"))
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{alias}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("canonical"),
            "{alias}"
        );
        assert!(!marker.exists());
    }
}

#[cfg(feature = "native-settings")]
#[test]
fn native_settings_reject_case_folded_data_dir_aliases_before_child() {
    let (dir, marker) = owned_uvicorn();
    for alias in ["data_dir", "Data_Dir"] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_SETTINGS_IMPL", "rust")
            .env_remove("DATA_DIR")
            .env(alias, dir.path().join("alias"))
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("canonical DATA_DIR"));
        assert!(!marker.exists());
    }
}

#[cfg(feature = "native-prompts")]
#[test]
fn native_prompts_reject_case_folded_data_dir_aliases_before_side_effects() {
    let (dir, marker) = owned_uvicorn();
    let database = dir.path().join("data");
    for alias in ["data_dir", "Data_Dir", "dAtA_dIr"] {
        for canonical in [false, true] {
            let mut cmd = command(dir.path(), &marker);
            cmd.env("MODEL_MANAGER_IMPL", "rust")
                .env("MODEL_MANAGER_PROMPTS_IMPL", "rust")
                .env_remove("DATA_DIR")
                .env(alias, &database);
            if canonical {
                cmd.env("DATA_DIR", dir.path().join("canonical"));
            }
            let output = cmd.prompt_output().unwrap();
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("canonical DATA_DIR"));
            assert!(!database.exists());
            assert!(!dir.path().join("canonical").exists());
            assert!(!marker.exists());
        }
    }
    // Python keeps its existing case-insensitive configuration policy.
    let output = command(dir.path(), &marker)
        .env("data_dir", &database)
        .prompt_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42));
    assert!(!database.exists());
}

#[cfg(feature = "native-prompts")]
#[cfg_attr(target_os = "linux", test)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn native_prompt_readiness_precedes_exact_private_child_confirmation() {
    let _ports = child_ports();
    let (dir, marker) = owned_uvicorn();
    // A root-owned sticky temporary parent requires a private owned next directory.
    let database = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    std::fs::write(dir.path().join("uvicorn"),"#!/bin/sh\n[ -f \"$DATA_DIR/model_loader.db\" ] || exit 43\nprintf '%s\\n%s\\n%s\\n' \"$NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED\" \"$DATA_DIR\" \"$*\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    let output = command(dir.path(), &marker)
        .env("MODEL_MANAGER_IMPL", "rust")
        .env("MODEL_MANAGER_PROMPTS_IMPL", "rust")
        .env("DATA_DIR", database.path())
        .env("NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED", "untrusted")
        .prompt_output()
        .unwrap();
    if !String::from_utf8_lossy(&output.stderr).contains("Python-owned endpoint process exited") {
        use std::os::unix::fs::MetadataExt;
        let root = std::fs::symlink_metadata(database.path())
            .ok()
            .map(|m| (m.uid(), m.mode()));
        let parent = database
            .path()
            .parent()
            .and_then(|p| std::fs::symlink_metadata(p).ok())
            .map(|m| (m.uid(), m.mode()));
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("prompt readiness fixture: root_uid_mode={root:?} parent_uid_mode={parent:?} euid={} exit_code={:?} marker_exists={} database_exists={} store_error={} address_in_use={}",
            rustix::process::geteuid().as_raw(), output.status.code(), marker.exists(),
            database.path().join("model_loader.db").exists(), stderr.contains("Error: Store"),
            stderr.contains("AddrInUse"));
    }
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Python-owned endpoint process exited")
    );
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        format!(
            "1\n{}\napp.main:app --host 127.0.0.1 --port 8091\n",
            database.path().display()
        )
    );
    let db = rusqlite::Connection::open(database.path().join("model_loader.db")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM prompts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert_eq!(
        db.query_row(
            "SELECT value FROM kv WHERE key='bench_prompts_seeded'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "1"
    );
    // The reaped parent released its writer lock; a second owner can now acquire it.
    let lock = std::fs::File::options()
        .read(true)
        .write(true)
        .open(database.path().join("model_loader.db.prompts.lock"))
        .unwrap();
    lock.try_lock().unwrap();
}

#[cfg(feature = "native-logs")]
#[test]
fn native_parent_with_prompts_off_replaces_inherited_confirmation_without_database() {
    let _ports = child_ports();
    let (dir, marker) = owned_uvicorn();
    let database = dir.path().join("absent");
    std::fs::write(dir.path().join("uvicorn"),"#!/bin/sh\nprintf '%s' \"$NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    let output = command(dir.path(), &marker)
        .env("MODEL_MANAGER_IMPL", "rust")
        .env("DATA_DIR", &database)
        .env("NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED", "1")
        .prompt_output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Python-owned endpoint process exited")
    );
    assert_eq!(std::fs::read(marker).unwrap(), b"0");
    assert!(!database.exists());
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
/// #1747: a bad idle-unload setting refuses before the child; an existing chat preset gets the
/// key at startup; an unreadable one is reported and startup continues to the child.
#[cfg(feature = "native-logs")]
#[test]
fn idle_unload_startup_paths() {
    let _ports = child_ports();
    let (dir, marker) = owned_uvicorn();
    let models = tempfile::tempdir().unwrap();
    let ini = models.path().join("models.ini");
    std::fs::write(&ini, "[chat]\nmodel = synthetic.gguf\n").unwrap();
    let output = command(dir.path(), &marker)
        .env("MODEL_MANAGER_IMPL", "rust")
        .env("MODELS_INI_PATH", &ini)
        .env("CHAT_MODEL_IDLE_UNLOAD_SECONDS", "-5")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("CHAT_MODEL_IDLE_UNLOAD_SECONDS"));
    assert!(!marker.exists());
    assert_eq!(
        std::fs::read_to_string(&ini).unwrap(),
        "[chat]\nmodel = synthetic.gguf\n"
    );

    let output = command(dir.path(), &marker)
        .env("MODEL_MANAGER_IMPL", "rust")
        .env("MODELS_INI_PATH", &ini)
        .env_remove("CHAT_MODEL_IDLE_UNLOAD_SECONDS")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Python-owned endpoint process exited")
    );
    assert_eq!(
        std::fs::read_to_string(&ini).unwrap(),
        "[*]\nsleep-idle-seconds = 1800\n\n[chat]\nmodel = synthetic.gguf\n"
    );

    // noevia#1863: an admin's stored choice in DATA_DIR wins over the env seed at startup.
    std::fs::remove_file(&marker).unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(
        data.path().join("chat-idle-unload.json"),
        r#"{"seconds":7200}"#,
    )
    .unwrap();
    let output = command(dir.path(), &marker)
        .env("MODEL_MANAGER_IMPL", "rust")
        .env("MODELS_INI_PATH", &ini)
        .env("DATA_DIR", data.path())
        .env("CHAT_MODEL_IDLE_UNLOAD_SECONDS", "900")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Python-owned endpoint process exited")
    );
    assert_eq!(
        std::fs::read_to_string(&ini).unwrap(),
        "[*]\nsleep-idle-seconds = 7200\n\n[chat]\nmodel = synthetic.gguf\n"
    );

    std::fs::remove_file(&marker).unwrap();
    std::fs::write(&ini, [0xff, 0xfe]).unwrap();
    let output = command(dir.path(), &marker)
        .env("MODEL_MANAGER_IMPL", "rust")
        .env("MODELS_INI_PATH", &ini)
        .env("CHAT_MODEL_IDLE_UNLOAD_SECONDS", "60")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("idle unload: chat preset is not UTF-8"));
    assert!(stderr.contains("Python-owned endpoint process exited"));
    assert!(marker.exists());
    assert_eq!(std::fs::read(&ini).unwrap(), vec![0xff, 0xfe]);
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
