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

/// Run a command whose fake `uvicorn` script a test has just (re)written. On Linux a parallel
/// test's fork can briefly hold the new script open for writing, so exec fails with ETXTBSY and
/// the manager exits 1 before the script runs; that is retried, never a real refusal.
fn busy_retry(cmd: &mut Command) -> prompts_process::Output {
    for _ in 0..20 {
        let output = cmd.prompt_output().unwrap();
        let busy = output.status.code() == Some(1)
            && String::from_utf8_lossy(&output.stderr).contains("ExecutableFileBusy");
        if !busy {
            return output;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    cmd.prompt_output().unwrap()
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
        .env_remove("MODEL_MANAGER_DOWNLOAD_TARGETS_IMPL")
        .env_remove("MODEL_MANAGER_BADGES_IMPL")
        .env_remove("MODEL_MANAGER_BENCH_RUNS_IMPL")
        .env_remove("MODEL_MANAGER_SECTION_WRITES_IMPL")
        .env_remove("MODEL_MANAGER_SETTINGS_WRITE_IMPL")
        .env_remove("MODEL_MANAGER_BACKEND_CONTROL_IMPL")
        .env_remove("MODEL_MANAGER_BENCHMARK_OVERVIEW_IMPL")
        .env_remove("MODEL_MANAGER_CHECK_UPDATES_IMPL")
        .env_remove("MODEL_LOADER_TOKEN")
        .env_remove("LLAMA_CONTAINERS")
        .env_remove("DOCKER_HOST")
        .env_remove("CHAT_MODEL_IDLE_UNLOAD_SECONDS");
    command
}
/// noevia#1071: the native parent always runs and the selector cannot reach the child. With the
/// build lacking native-logs, startup refuses before any child; otherwise the shim child records
/// what the native parent handed it (the parent then reports the child's exit).
fn native_child_record(cmd: &mut Command, marker: &std::path::Path) -> (Option<String>, String) {
    #[cfg(feature = "native-logs")]
    let _ports = child_ports();
    let output = busy_retry(cmd);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!output.status.success(), "{stderr}");
    if cfg!(feature = "native-logs") {
        assert!(
            stderr.contains("Python-owned endpoint process exited"),
            "{stderr}"
        );
        let record = std::fs::read_to_string(marker).unwrap();
        std::fs::remove_file(marker).unwrap();
        (Some(record), stderr)
    } else {
        assert!(stderr.contains("feature is not enabled"), "{stderr}");
        assert!(!marker.exists());
        (None, stderr)
    }
}
/// noevia#2011: the retired MODEL_MANAGER_IMPL and MODEL_FILES_IMPL keys are not read at all: a
/// leftover value of either changes nothing and is not reported, and the parent is always native.
#[test]
fn retired_keys_are_not_read_and_native_parent_is_unconditional() {
    let (dir, marker) = owned_uvicorn();
    for key in [None, Some("MODEL_MANAGER_IMPL"), Some("MODEL_FILES_IMPL")] {
        for value in ["rust", "python", "invalid", ""] {
            let mut cmd = command(dir.path(), &marker);
            cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_CONFIRMED", "hostile")
                .env_remove("MODEL_FILES_IMPL");
            if let Some(key) = key {
                cmd.env(key, value);
            }
            let (record, stderr) = native_child_record(&mut cmd, &marker);
            // Never the old public uvicorn exec on 8090: only the private loopback child. A build
            // without native-logs refuses before any child (native_child_record checks that).
            if cfg!(feature = "native-logs") {
                assert_eq!(
                    record.expect("the native parent started the child"),
                    "1\napp.main:app\n--host\n127.0.0.1\n--port\n8091\n",
                    "{key:?}={value:?}"
                );
            } else {
                assert_eq!(record, None, "{key:?}={value:?}");
            }
            for retired in ["MODEL_MANAGER_IMPL", "MODEL_FILES_IMPL", "retired"] {
                assert!(!stderr.contains(retired), "{key:?}={value:?}: {stderr}");
            }
        }
    }
}
#[test]
fn invalid_token_never_spawns_child() {
    let (dir, marker) = owned_uvicorn();
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
    for (parent, prompts) in [("rust", "invalid"), ("rust", "Rust")] {
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
    // Selector off: the always-native parent hands the child "0" over an inherited value.
    let mut cmd = command(dir.path(), &marker);
    cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED", "1")
        .env("DATA_DIR", &database);
    if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
        assert_eq!(record, "0");
    }
    assert!(!database.exists());
}

#[test]
fn settings_selector_omission_and_python_confirmation_are_closed_before_db_or_child() {
    let (dir, marker) = owned_uvicorn();
    let database = dir.path().join("data");
    for (parent, settings) in [("rust", "invalid"), ("rust", "Rust")] {
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
    // Selector off: the always-native parent hands the child "0" over an inherited value, so
    // Python keeps GET.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_SETTINGS_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
    assert!(!database.exists());
}

#[test]
fn model_updates_selector_omission_and_python_confirmation_are_closed_before_db_or_child() {
    let (dir, marker) = owned_uvicorn();
    let database = dir.path().join("data");
    for (parent, updates) in [("rust", "invalid"), ("rust", "Rust")] {
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
    // Selector off: the always-native parent hands the child "0" over an inherited value, so
    // Python keeps GET.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_MODEL_UPDATES_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_MODEL_UPDATES_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_MODEL_UPDATES_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
    assert!(!database.exists());
}

#[test]
fn model_detail_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, detail) in [("rust", "invalid"), ("rust", "Rust")] {
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
    // Selector off: the always-native parent hands the child "0", so Python keeps the GET and
    // never serves the private runtime call.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DETAIL_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DETAIL_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_MODEL_DETAIL_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
}

#[test]
fn download_targets_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, targets) in [("rust", "invalid"), ("rust", "Rust")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_DOWNLOAD_TARGETS_IMPL", targets)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{targets}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-download-targets") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_DOWNLOAD_TARGETS_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("compiled native-download-targets")
        );
        assert!(!marker.exists());
    }
    // Selector off: the always-native parent hands the child "0" over an inherited value.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOAD_TARGETS_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env(
            "NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOAD_TARGETS_CONFIRMED",
            "1",
        );
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_DOWNLOAD_TARGETS_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
}

#[test]
fn badges_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, targets) in [("rust", "invalid"), ("rust", "Rust")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_BADGES_IMPL", targets)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{targets}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-badges") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_BADGES_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-badges"));
        assert!(!marker.exists());
    }
    // Selector off: the always-native parent hands the child "0" over an inherited value.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_BADGES_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_BADGES_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_BADGES_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
}

#[test]
fn bench_runs_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, targets) in [("rust", "invalid"), ("rust", "Rust")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_BENCH_RUNS_IMPL", targets)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{targets}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-bench-runs") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_BENCH_RUNS_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-bench-runs"));
        assert!(!marker.exists());
    }
    // Selector off: the always-native parent hands the child "0" over an inherited value.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_BENCH_RUNS_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_BENCH_RUNS_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_BENCH_RUNS_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
}

#[test]
fn section_writes_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, targets) in [("rust", "invalid"), ("rust", "Rust")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_SECTION_WRITES_IMPL", targets)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{targets}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-section-writes") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_SECTION_WRITES_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-section-writes"));
        assert!(!marker.exists());
    }
    // Selector off: the always-native parent hands the child "0" over an inherited value.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_SECTION_WRITES_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_SECTION_WRITES_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_SECTION_WRITES_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
}

/// noevia#2010: with the native writer selected the parent migrates models.ini after the
/// idle-unload reconcile (#1747) and before the child starts, the old order (the child that used to
/// migrate starts after the reconcile). The child sees exactly reconcile-then-migration bytes,
/// odd `[*]` headers included, and the confirmation that makes it skip its own run;
/// MIGRATE_CACHE_RAM_ON_START=false leaves only the reconcile.
#[cfg(all(feature = "native-section-writes", feature = "native-logs"))]
#[test]
fn native_writer_migrates_after_the_reconcile_and_before_the_child_starts() {
    use noevia_model_manager::{idle_unload, section_writes::SectionWrites};
    let (dir, marker) = owned_uvicorn();
    let script = dir.path().join("uvicorn");
    std::fs::write(&script, "#!/bin/sh\nprintf '%s|' \"$NOEVIA_NATIVE_MODEL_MANAGER_SECTION_WRITES_CONFIRMED\" > \"$S1_TEST_MARKER\"\n/bin/cat \"$MODELS_INI_PATH\" >> \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    let models = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let ini = models.path().join("models.ini");
    let old = tempfile::tempdir().unwrap();
    let old_ini = old.path().join("models.ini");
    for (original, refused) in [
        ("[chat]\nmodel = /models/synthetic.gguf\n", false),
        // noevia#2019: the reconcile recognises a `[*]` header with an inline comment, so it edits
        // that one section and the migration then runs on it (no repeated `[*]` to refuse).
        (
            "version = 1\n[*] ; shared\ncache-ram = -1\n\n[chat]\nmodel = m\n\n[my-embed]\n",
            false,
        ),
        (
            "[*]trailing\nsleep-idle-seconds = 60\ncache-ram = 99999\n[chat]\n",
            false,
        ),
        ("[*]\n[chat]\ncache-ram = 512\n", false),
        ("[a] trailing\nmodel = m\n[b];c\nk : v\n", false),
    ] {
        for setting in [None, Some("false")] {
            // The old order: the reconcile, then the (byte-identical) migration.
            std::fs::write(&old_ini, original).unwrap();
            idle_unload::reconcile_file(&old_ini, Some(idle_unload::DEFAULT_SECONDS))
                .expect("the reference reconcile");
            let reconciled = std::fs::read_to_string(&old_ini).unwrap();
            if setting.is_none() {
                let migrated = SectionWrites::new(&old_ini, (1024, 2048)).migrate_cache_ram();
                if refused {
                    migrated.expect_err("the reference migration refuses the repeated [*]");
                } else {
                    migrated.expect("the reference migration");
                }
            }
            let want = format!("1|{}", std::fs::read_to_string(&old_ini).unwrap());
            std::fs::write(&ini, original).unwrap();
            let mut cmd = command(dir.path(), &marker);
            cmd.env("MODEL_MANAGER_SECTION_WRITES_IMPL", "rust")
                .env("TZ", "UTC")
                .env("MODELS_INI_PATH", &ini)
                .env("DATA_DIR", data.path())
                .env_remove("MIGRATE_CACHE_RAM_ON_START");
            if let Some(value) = setting {
                cmd.env("MIGRATE_CACHE_RAM_ON_START", value);
            }
            let (record, stderr) = native_child_record(&mut cmd, &marker);
            let record = record.expect("the native parent started the child");
            assert_eq!(record, want, "{original:?} {setting:?}: {stderr}");
            if original.starts_with("[chat]") {
                // Not a vacuous comparison: the reconcile wrote, and the migration did too.
                assert!(
                    reconciled.contains("sleep-idle-seconds = 1800"),
                    "{reconciled}"
                );
                assert_eq!(
                    record.contains("cache-ram = 1024"),
                    setting.is_none(),
                    "{record}"
                );
                assert_eq!(
                    stderr.contains("set an explicit bounded cache-ram in 1 section(s): chat"),
                    setting.is_none(),
                    "{stderr}"
                );
            }
        }
    }
}

#[test]
fn settings_write_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, targets) in [("rust", "invalid"), ("rust", "Rust")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_SETTINGS_WRITE_IMPL", targets)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{targets}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-settings-write") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_SETTINGS_WRITE_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-settings-write"));
        assert!(!marker.exists());
    }
    // Selector off: the always-native parent hands the child "0" over an inherited value.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_WRITE_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_WRITE_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_SETTINGS_WRITE_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
}

#[test]
fn backend_control_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, targets) in [("rust", "invalid"), ("rust", "Rust")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_BACKEND_CONTROL_IMPL", targets)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{targets}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-backend-control") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_BACKEND_CONTROL_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-backend-control"));
        assert!(!marker.exists());
    }
    // Selector off: the always-native parent hands the child "0" over an inherited value.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_BACKEND_CONTROL_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_BACKEND_CONTROL_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_BACKEND_CONTROL_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
}

#[test]
fn benchmark_overview_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, targets) in [("rust", "invalid"), ("rust", "Rust")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_BENCHMARK_OVERVIEW_IMPL", targets)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{targets}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-benchmark-overview") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_BENCHMARK_OVERVIEW_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("compiled native-benchmark-overview")
        );
        assert!(!marker.exists());
    }
    // Selector off: the always-native parent hands the child "0" over an inherited value.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_OVERVIEW_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env(
            "NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_OVERVIEW_CONFIRMED",
            "1",
        );
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_BENCHMARK_OVERVIEW_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
}

#[test]
fn check_updates_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, targets) in [("rust", "invalid"), ("rust", "Rust")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_CHECK_UPDATES_IMPL", targets)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{targets}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-check-updates") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_CHECK_UPDATES_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-check-updates"));
        assert!(!marker.exists());
    }
    // Selector off: the always-native parent hands the child "0" over an inherited value.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_CHECK_UPDATES_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_CHECK_UPDATES_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_CHECK_UPDATES_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
}

#[test]
fn benchmark_runner_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, targets) in [("rust", "invalid"), ("rust", "Rust")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_BENCHMARK_RUNNER_IMPL", targets)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{targets}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-benchmark-runner") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_BENCHMARK_RUNNER_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("compiled native-benchmark-runner")
        );
        assert!(!marker.exists());
    }
    // Selector off: the always-native parent hands the child "0" over an inherited value.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_RUNNER_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env(
            "NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_RUNNER_CONFIRMED",
            "1",
        );
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_BENCHMARK_RUNNER_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
}

#[test]
fn downloads_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, targets) in [("rust", "invalid"), ("rust", "Rust")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_DOWNLOADS_IMPL", targets)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{targets}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-downloads") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_DOWNLOADS_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-downloads"));
        assert!(!marker.exists());
    }
    // Selector off: the always-native parent hands the child "0" over an inherited value.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOADS_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOADS_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_DOWNLOADS_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
    }
}

#[test]
fn model_delete_selector_omission_and_python_confirmation_are_closed_before_child() {
    let (dir, marker) = owned_uvicorn();
    for (parent, targets) in [("rust", "invalid"), ("rust", "Rust")] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", parent)
            .env("MODEL_MANAGER_MODEL_DELETE_IMPL", targets)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{parent}/{targets}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-model-delete") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_MODEL_DELETE_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-model-delete"));
        assert!(!marker.exists());
    }
    // Selector off: the always-native parent hands the child "0" over an inherited value.
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DELETE_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    for selected in [None, Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DELETE_CONFIRMED", "1");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_MODEL_DELETE_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, "0");
        }
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
        let output = busy_retry(&mut cmd);
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
            let output = busy_retry(&mut cmd);
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("canonical DATA_DIR"));
            assert!(!database.exists());
            assert!(!dir.path().join("canonical").exists());
            assert!(!marker.exists());
        }
    }
    // Prompts off: the alias policy is not applied and the native parent opens no prompt store.
    let mut cmd = command(dir.path(), &marker);
    cmd.env("data_dir", &database);
    native_child_record(&mut cmd, &marker);
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
    // Linux ETXTBSY: the script was just rewritten, so the spawn is retried like the others.
    let mut cmd = command(dir.path(), &marker);
    cmd.env("MODEL_MANAGER_IMPL", "rust")
        .env("DATA_DIR", &database)
        .env("NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED", "1");
    let output = busy_retry(&mut cmd);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Python-owned endpoint process exited"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
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
        if cfg!(feature = "native-downloads") {
            expected.push_str("native-downloads\n");
        }
        if cfg!(feature = "native-model-delete") {
            expected.push_str("native-model-delete\n");
        }
        if cfg!(feature = "native-draft-heads") {
            expected.push_str("native-draft-heads\n");
        }
        if cfg!(feature = "native-backends") {
            expected.push_str("native-backends\n");
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
fn sections_selector_and_stale_default_confirmation() {
    let (dir, marker) = owned_uvicorn();
    for (mode, sections) in [("rust", "invalid"), ("rust", "Rust")] {
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
    let mut cmd = command(dir.path(), &marker);
    cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_SECTIONS_CONFIRMED", "1");
    if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
        assert_eq!(record, "0");
    }
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
    // noevia#1071: no selector value brings back a Python-only start without native-logs.
    for selected in [None, Some("rust"), Some("python")] {
        let mut cmd = command(dir.path(), &marker);
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_IMPL", value);
        }
        let output = cmd.output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("feature is not enabled"));
        assert!(!marker.exists());
    }
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
    for (mode, host) in [("rust", "invalid"), ("rust", "Rust")] {
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

/// noevia#1966: a native delete must see every download, so it is native only together with
/// native downloads; alone it stays with Python (whose delete checks its own job table).
#[cfg(feature = "native-model-delete")]
#[test]
fn native_model_delete_needs_native_downloads() {
    let (dir, marker) = owned_uvicorn();
    let script = dir.path().join("uvicorn");
    std::fs::write(&script,"#!/bin/sh\nprintf '%s %s' \"${NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DELETE_CONFIRMED-unset}\" \"${NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOADS_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    let fallback = "native-model-delete: needs MODEL_MANAGER_DOWNLOADS_IMPL=rust";
    for (downloads, expected, falls_back) in [
        (None, "0 0", true),
        (Some("python"), "0 0", true),
        (Some("rust"), "1 1", false),
    ] {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("MODEL_MANAGER_MODEL_DELETE_IMPL", "rust")
            .env_remove("MODEL_MANAGER_DOWNLOADS_IMPL");
        if let Some(value) = downloads {
            cmd.env("MODEL_MANAGER_DOWNLOADS_IMPL", value);
        }
        let (record, stderr) = native_child_record(&mut cmd, &marker);
        assert_eq!(record.as_deref(), Some(expected), "{downloads:?}: {stderr}");
        assert_eq!(
            stderr.contains(fallback),
            falls_back,
            "{downloads:?}: {stderr}"
        );
    }
}

/// noevia#1318 slice 12: the draft-head selector is closed before any child, and the native parent
/// hands the child the exact confirmation (over an inherited value).
#[test]
fn draft_heads_selector_and_confirmation() {
    let (dir, marker) = owned_uvicorn();
    for value in ["invalid", "Rust", ""] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_DRAFT_HEADS_IMPL", value)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{value:?}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-draft-heads") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_DRAFT_HEADS_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-draft-heads"));
        assert!(!marker.exists());
    }
    let script = dir.path().join("uvicorn");
    std::fs::write(&script, "#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_DRAFT_HEADS_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    let mut cases = vec![(None, "0"), (Some("python"), "0")];
    if cfg!(feature = "native-draft-heads") {
        cases.push((Some("rust"), "1"));
    }
    for (selected, expected) in cases {
        let mut cmd = command(dir.path(), &marker);
        cmd.env(
            "NOEVIA_NATIVE_MODEL_MANAGER_DRAFT_HEADS_CONFIRMED",
            "hostile",
        )
        .env_remove("MODEL_MANAGER_DRAFT_HEADS_IMPL");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_DRAFT_HEADS_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, expected, "{selected:?}");
        }
    }
}

/// noevia#1318 slice 13: the backend selector is closed before any child; the native parent hands
/// the child the exact confirmation (over an inherited value), and keeps Python owning the
/// backends while the restart is Python's (its errors are what GET /backends shows).
#[test]
fn backends_selector_confirmation_and_fallback() {
    let (dir, marker) = owned_uvicorn();
    for value in ["invalid", "Rust", ""] {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_BACKENDS_IMPL", value)
            .prompt_output()
            .unwrap();
        assert!(!output.status.success(), "{value:?}");
        assert!(!marker.exists());
    }
    if !cfg!(feature = "native-backends") {
        let output = command(dir.path(), &marker)
            .env("MODEL_MANAGER_BACKENDS_IMPL", "rust")
            .prompt_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("compiled native-backends"));
        assert!(!marker.exists());
    }
    let script = dir.path().join("uvicorn");
    std::fs::write(&script, "#!/bin/sh\nprintf '%s' \"${NOEVIA_NATIVE_MODEL_MANAGER_BACKENDS_CONFIRMED-unset}\" > \"$S1_TEST_MARKER\"\nexit 42\n").unwrap();
    let mut cases = vec![(None, None, "0"), (Some("python"), None, "0")];
    if cfg!(feature = "native-backends") {
        cases.push((Some("python"), Some("rust"), "0"));
        // Without the native restart the sampler stays with Python.
        cases.push((Some("rust"), Some("python"), "0"));
        cases.push((Some("rust"), Some("rust"), "1"));
    }
    for (selected, control, expected) in cases {
        let mut cmd = command(dir.path(), &marker);
        cmd.env("NOEVIA_NATIVE_MODEL_MANAGER_BACKENDS_CONFIRMED", "hostile")
            .env_remove("MODEL_MANAGER_BACKENDS_IMPL")
            .env_remove("MODEL_MANAGER_BACKEND_CONTROL_IMPL");
        if let Some(value) = selected {
            cmd.env("MODEL_MANAGER_BACKENDS_IMPL", value);
        }
        if let Some(value) = control {
            cmd.env("MODEL_MANAGER_BACKEND_CONTROL_IMPL", value);
        }
        if let (Some(record), _) = native_child_record(&mut cmd, &marker) {
            assert_eq!(record, expected, "{selected:?} {control:?}");
        }
    }
}
