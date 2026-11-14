//! The binary end to end with shell stubs (bash only, no node): `--check`, refusals before any
//! start, the first exit's code, and docker stop.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const FEATURES: &str = "code-net-guard rust-auth client-address-header rust-chatgpt-status rust-storage-reads rust-storage-browse rust-export rust-free-chats rust-usage rust-stats rust-routing-default rust-sampling-settings rust-reasoning-settings rust-routing-mode rust-projects rust-project-chats rust-project-writes project-store";

fn write_exec(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, body).unwrap_or_else(|e| unreachable!("{e}"));
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| unreachable!("{e}"));
}

/// A Core directory with stub `node` (on PATH) and front; each child logs its environment and
/// lives until its die file appears or it is signalled.
fn work() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let d = dir.path();
    fs::create_dir_all(d.join("bin")).unwrap_or_else(|e| unreachable!("{e}"));
    let stub = |name: &str| {
        format!(
            r#"#!/usr/bin/env bash
[ "${{1:-}}" = -e ] && exit 0
if [ "${{1:-}}" = --features ]; then printf '%s\n' ${{STUB_FEATURES-{FEATURES}}}; exit 0; fi
echo "{name} UI_HOST=${{UI_HOST:-}} UI_PORT=${{UI_PORT:-}} UPSTREAM=${{NOEVIA_LEGACY_UPSTREAM:-}} SPORT=${{NOEVIA_PROJECT_STORE_PORT:-}} TOK=${{#NOEVIA_PROJECT_STORE_TOKEN}} START=${{#NOEVIA_START_ID}} KCONF=${{NOEVIA_FRONT_CLIENT_IP-unset}} RETIRED=${{NOEVIA_RUST_AUTH-unset}}" >> "{log}"
trap 'echo "{name} TERM" >> "{log}"; exit 0' TERM
for _ in $(seq 1 400); do [ -f "{die}" ] && exit 3; sleep 0.05; done
"#,
            log = d.join("log").display(),
            die = d.join(format!("die-{name}")).display(),
        )
    };
    write_exec(&d.join("bin/node"), &stub("node"));
    write_exec(&d.join("noevia-server"), &stub("front"));
    dir
}

fn supervisor(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_noevia-web-supervisor"));
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    cmd.current_dir(dir)
        .env_clear()
        .env("PATH", path)
        .env("NOEVIA_SERVER_BIN", dir.join("noevia-server"))
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

#[test]
fn check_prints_the_plan_without_secrets_and_starts_nothing() {
    let w = work();
    let out = supervisor(w.path())
        .arg("--check")
        .env("UI_PORT", "8021")
        .output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("pair ok"));
    assert!(stdout.contains(
        "node node server/index.cjs UI_HOST=127.0.0.1 UI_PORT=9021 NOEVIA_FRONT_CLIENT_IP=1"
    ));
    assert!(stdout.contains("NOEVIA_LEGACY_UPSTREAM=http://127.0.0.1:9021"));
    assert!(stdout.contains("NOEVIA_PROJECT_STORE_PORT=10021"));
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
fn refusals_exit_2_before_anything_starts() {
    let w = work();
    for (key, value, want) in [
        (
            "NOEVIA_FRONT",
            "node",
            "NOEVIA_FRONT=node is no longer supported",
        ),
        (
            "NOEVIA_FRONT",
            "bogus",
            "NOEVIA_FRONT must be rust, got 'bogus'",
        ),
        (
            "STUB_FEATURES",
            "rust-auth",
            "lacks the front route feature(s): rust-chatgpt-status",
        ),
        ("UI_PORT", "64536", "leaves no room for the legacy port"),
    ] {
        let out = supervisor(w.path())
            .env(key, value)
            .output()
            .unwrap_or_else(|e| unreachable!("{e}"));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{key}={value}: {stderr}");
        assert!(stderr.contains(want), "{key}={value}: {stderr}");
        assert_eq!(log(w.path()), "", "{key}={value}: nothing started");
    }
    let out = supervisor(w.path())
        .env("NOEVIA_SERVER_BIN", w.path().join("missing"))
        .output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("has no noevia-server"));
}

#[test]
fn the_front_exiting_stops_node_and_the_container_takes_its_code() {
    let w = work();
    let child = supervisor(w.path())
        .env("NOEVIA_RUST_AUTH", "1")
        .env("NOEVIA_START_ID", "spoofed")
        .env("NOEVIA_FRONT_CLIENT_IP", "spoofed")
        .spawn()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert!(
        wait_for(w.path(), "front UI_HOST") && wait_for(w.path(), "node UI_HOST"),
        "{}",
        log(w.path())
    );
    fs::write(w.path().join("die-front"), "").unwrap_or_else(|e| unreachable!("{e}"));
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    let l = log(w.path());
    assert_eq!(out.status.code(), Some(3), "{l}");
    assert!(l.contains("node TERM"), "{l}");
    assert!(l.contains("node UI_HOST=127.0.0.1 UI_PORT=9021 UPSTREAM= SPORT=10021 TOK=64 START=32 KCONF=1 RETIRED=unset"), "{l}");
    assert!(l.contains("front UI_HOST= UI_PORT= UPSTREAM=http://127.0.0.1:9021 SPORT=10021 TOK=64 START=32 KCONF=unset RETIRED=unset"), "{l}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("noevia-server exited (3); stopping the other"),
        "{stderr}"
    );
    assert!(stderr.contains("NOEVIA_RUST_AUTH is retired"), "{stderr}");
}

#[test]
fn sigterm_stops_the_front_then_node_and_exits_0() {
    let w = work();
    let child = supervisor(w.path())
        .spawn()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert!(
        wait_for(w.path(), "front UI_HOST") && wait_for(w.path(), "node UI_HOST"),
        "{}",
        log(w.path())
    );
    let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap_or_default());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM)
        .unwrap_or_else(|e| unreachable!("{e}"));
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    let l = log(w.path());
    assert_eq!(out.status.code(), Some(0), "{l}");
    let front = l.find("front TERM");
    let node = l.find("node TERM");
    assert!(
        front.is_some() && node.is_some() && front < node,
        "front joined before Core: {l}"
    );
}

#[test]
fn a_second_pair_on_the_same_data_directory_is_refused_while_the_first_lives() {
    let w = work();
    let first = supervisor(w.path())
        .spawn()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert!(
        wait_for(w.path(), "front UI_HOST") && wait_for(w.path(), "node UI_HOST"),
        "{}",
        log(w.path())
    );
    let out = supervisor(w.path())
        .output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("previous Core/front pair is not quiescent")
    );
    fs::write(w.path().join("die-node"), "").unwrap_or_else(|e| unreachable!("{e}"));
    let out = first
        .wait_with_output()
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("node exited (3)"));
}
