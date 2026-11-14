//! The front-only start (NOEVIA_SUPERVISOR_FRONT_ONLY=1) end to end with a bash stub front: no
//! `node` on PATH and no Core files in the working directory.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn write_exec(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, body).unwrap_or_else(|e| unreachable!("{e}"));
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| unreachable!("{e}"));
}

/// An empty directory (no Core) with a stub front that logs its preparation steps and its serving
/// environment, then lives until a die file appears or it is signalled.
fn work() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let d = dir.path();
    let body = format!(
        r#"#!/usr/bin/env bash
case "${{1:-}}" in --ensure-*) echo "front ${{1#--}} UI_DATA_DIR=${{UI_DATA_DIR:-}} ONLY=${{NOEVIA_FRONT_ONLY-unset}}" >> "{log}"; [ "${{1:-}}" = "${{STUB_ENSURE_FAIL:-}}" ] && exit 4; exit 0;; esac
if [ "${{1:-}}" = --features ]; then printf '%s\n' ${{STUB_FEATURES-rust-auth front-only-v1 boot-ensure-v1}}; exit 0; fi
echo "front serve DATA=${{UI_DATA_DIR:-}} ONLY=${{NOEVIA_FRONT_ONLY-unset}} UPSTREAM=${{NOEVIA_LEGACY_UPSTREAM:-}} SPORT=${{NOEVIA_PROJECT_STORE_PORT:-}} TOK=${{#NOEVIA_PROJECT_STORE_TOKEN}} START=${{#NOEVIA_START_ID}} KCONF=${{NOEVIA_FRONT_CLIENT_IP-unset}} CLAIM=${{NOEVIA_LEGACY_CLAIM_BIN-unset}}" >> "{log}"
trap 'echo "front TERM" >> "{log}"; exit 0' TERM
for _ in $(seq 1 400); do [ -f "{die}" ] && exit 3; [ -f "{die0}" ] && exit 0; sleep 0.05; done
"#,
        log = d.join("log").display(),
        die = d.join("die").display(),
        die0 = d.join("die0").display(),
    );
    write_exec(&d.join("noevia-server"), &body);
    dir
}

/// PATH without `node`: only the directories the stub's own commands need.
const PATH: &str = "/usr/bin:/bin";

fn supervisor(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_noevia-web-supervisor"));
    cmd.current_dir(dir)
        .env_clear()
        .env("PATH", PATH)
        .env("NOEVIA_SERVER_BIN", dir.join("noevia-server"))
        .env("NOEVIA_SUPERVISOR_FRONT_ONLY", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn log(dir: &Path) -> String {
    fs::read_to_string(dir.join("log")).unwrap_or_default()
}

fn wait_for(dir: &Path, needle: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if log(dir).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Waits for the child with a deadline (every wait has one); kills it when the deadline passes.
fn finish(mut child: Child) -> (Option<i32>, String) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                break;
            }
        }
    }
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn term(child: &Child) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status();
}

#[test]
fn check_prints_the_front_alone_without_secrets_and_starts_nothing() {
    let w = work();
    let out = supervisor(w.path())
        .arg("--check")
        .output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.starts_with("web-supervisor: front only (no Core)\n"), "{stdout}");
    assert!(!stdout.contains("\nnode "), "no Core child: {stdout}");
    assert!(stdout.contains("NOEVIA_FRONT_ONLY=1"), "{stdout}");
    assert!(stdout.contains("NOEVIA_LEGACY_UPSTREAM=http://127.0.0.1:9021"));
    assert!(stdout.contains("NOEVIA_PROJECT_STORE_PORT=10021"));
    assert!(stdout.contains("ensure ") && stdout.contains(" --ensure-secrets-key\n"));
    assert!(stdout.contains(" --ensure-schema\n"));
    let key = stdout.find("--ensure-secrets-key").unwrap_or(usize::MAX);
    let schema = stdout.find("--ensure-schema").unwrap_or(0);
    assert!(key < schema, "key first: {stdout}");
    assert!(
        !stdout.contains("TOKEN") && !stdout.contains("START_ID"),
        "{stdout}"
    );
    assert_eq!(log(w.path()), "", "nothing started");
    assert!(
        !w.path().join("server/ui-data/embedding-pair.lock").exists(),
        "no lock taken"
    );
}

#[test]
fn the_front_is_prepared_then_started_alone_and_docker_stop_exits_0() {
    let w = work();
    let data = w.path().join("data");
    let child = supervisor(w.path())
        .env("UI_DATA_DIR", &data)
        .env("UI_PORT", "8021")
        // Inherited: never reaches the front as given (the supervisor sets its own).
        .env("NOEVIA_FRONT_ONLY", "spoof")
        .env("NOEVIA_FRONT_CLIENT_IP", "1")
        .spawn()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert!(wait_for(w.path(), "front serve"), "{}", log(w.path()));
    term(&child);
    let (code, stderr) = finish(child);
    assert_eq!(code, Some(0), "{stderr}");
    let log = log(w.path());
    let lines: Vec<&str> = log.lines().collect();
    let d = data.display();
    assert_eq!(
        lines,
        [
            format!("front ensure-secrets-key UI_DATA_DIR={d} ONLY=unset"),
            format!("front ensure-schema UI_DATA_DIR={d} ONLY=unset"),
            format!("front serve DATA={d} ONLY=1 UPSTREAM=http://127.0.0.1:9021 SPORT=10021 TOK=64 START=32 KCONF=unset CLAIM=unset"),
            "front TERM".to_owned(),
        ],
        "{stderr}"
    );
    assert!(data.join("embedding-pair.lock").exists());
}

#[test]
fn without_ui_data_dir_the_front_and_its_preparation_share_the_fallback() {
    let w = work();
    let child = supervisor(w.path())
        .spawn()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert!(wait_for(w.path(), "front serve"), "{}", log(w.path()));
    term(&child);
    let (code, stderr) = finish(child);
    assert_eq!(code, Some(0), "{stderr}");
    let fallback = w.path().join("server/ui-data");
    let want = format!("UI_DATA_DIR={}", fallback.display());
    let log = log(w.path());
    assert_eq!(log.matches(&want).count(), 2, "both steps: {log}");
    assert!(
        log.contains(&format!("front serve DATA={} ", fallback.display())),
        "{log}"
    );
    assert!(fallback.join("embedding-pair.lock").exists());
}

#[test]
fn the_fronts_exit_ends_the_container_non_zero() {
    for (file, want) in [("die", 3), ("die0", 1)] {
        let w = work();
        let child = supervisor(w.path())
            .spawn()
            .unwrap_or_else(|e| unreachable!("{e}"));
        assert!(wait_for(w.path(), "front serve"), "{}", log(w.path()));
        fs::write(w.path().join(file), "").unwrap_or_else(|e| unreachable!("{e}"));
        let (code, stderr) = finish(child);
        assert_eq!(code, Some(want), "{file}: {stderr}");
        assert!(
            stderr.contains(&format!("noevia-server exited ({})", if want == 1 { 0 } else { want })),
            "{file}: {stderr}"
        );
        assert!(!log(w.path()).contains("front TERM"), "{file}: already gone");
    }
}

#[test]
fn a_front_without_the_capabilities_is_refused_before_anything_starts() {
    for (features, missing) in [
        ("rust-auth boot-ensure-v1", "front-only-v1"),
        ("rust-auth front-only-v1", "boot-ensure-v1"),
    ] {
        let w = work();
        let out = supervisor(w.path())
            .env("STUB_FEATURES", features)
            .output()
            .unwrap_or_else(|e| unreachable!("{e}"));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains(&format!("does not advertise {missing}")), "{stderr}");
        assert_eq!(log(w.path()), "", "{features}: nothing started");
    }
}

#[test]
fn a_failed_preparation_step_starts_nothing() {
    let w = work();
    let out = supervisor(w.path())
        .env("STUB_ENSURE_FAIL", "--ensure-schema")
        .output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("--ensure-schema failed"), "{stderr}");
    let log = log(w.path());
    assert!(log.contains("ensure-secrets-key") && log.contains("ensure-schema"), "{log}");
    assert!(!log.contains("front serve"), "{log}");
}

#[test]
fn the_pair_refusals_before_the_mode_still_apply() {
    let w = work();
    let out = supervisor(w.path())
        .env("NOEVIA_FRONT", "node")
        .output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("NOEVIA_FRONT=node is no longer supported"));
    let out = supervisor(w.path())
        .env("UI_PORT", "64000")
        .output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no room for the project store port"));
    assert_eq!(log(w.path()), "", "nothing started");
}

#[test]
fn a_second_start_on_the_same_data_directory_is_refused_while_the_first_runs() {
    let w = work();
    let first = supervisor(w.path())
        .spawn()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert!(wait_for(w.path(), "front serve"), "{}", log(w.path()));
    let out = supervisor(w.path())
        .output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("not quiescent"), "{stderr}");
    assert_eq!(
        log(w.path()).matches("front serve").count(),
        1,
        "the second start never served"
    );
    term(&first);
    let (code, stderr) = finish(first);
    assert_eq!(code, Some(0), "{stderr}");
}

#[test]
fn without_the_switch_the_pair_checks_run_as_before() {
    // No Core here and no node on PATH: the pair start refuses, so the switch is what skips them.
    let w = work();
    let out = supervisor(w.path())
        .env("NOEVIA_SUPERVISOR_FRONT_ONLY", "0")
        .output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(log(w.path()), "", "nothing started");
}
