//! `noevia-web-supervisor --image-entrypoint` with a stub script (bash only, no node): the default
//! is an exec of `bash <script>` (same PID, environment, no extra arguments, the script's exit code,
//! signals reach the script), `rust` is the native supervisor unchanged, anything else is refused.

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_noevia-web-supervisor");

/// A stub supervisor script that logs `$$ $0 $# MARK=$MARK` and exits 7, or 0 on TERM after
/// logging `TERM`; it waits for TERM only when `WAIT_TERM=1`.
fn stub(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("web-supervisor.sh");
    let log = dir.join("log");
    let body = format!(
        r#"echo "$$ $0 $# MARK=${{MARK-unset}}" >> "{log}"
if [ "${{WAIT_TERM:-}}" = 1 ]; then
  trap 'echo TERM >> "{log}"; exit 0' TERM
  for _ in $(seq 1 400); do sleep 0.05; done
  exit 9
fi
exit 7
"#,
        log = log.display()
    );
    fs::write(&path, body).unwrap_or_else(|e| unreachable!("{e}"));
    path
}

fn base(dir: &Path, vars: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.arg("--image-entrypoint").env_clear().current_dir(dir).stdin(Stdio::null());
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    for (k, v) in vars {
        cmd.env(k, v);
    }
    cmd
}

fn log(dir: &Path) -> String {
    fs::read_to_string(dir.join("log")).unwrap_or_default()
}

fn output(mut cmd: Command) -> Output {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.output().unwrap_or_else(|e| unreachable!("{e}"))
}

#[test]
fn default_execs_the_script_in_this_process() {
    for implementation in [None, Some(""), Some("sh")] {
        let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
        let script = stub(dir.path());
        let script_text = script.display().to_string();
        let mut vars = vec![("NOEVIA_SUPERVISOR_SCRIPT", script_text.as_str()), ("MARK", "kept")];
        if let Some(v) = implementation {
            vars.push(("NOEVIA_SUPERVISOR_IMPL", v));
        }
        let mut child = base(dir.path(), &vars)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| unreachable!("{e}"));
        let pid = child.id();
        let status = child.wait().unwrap_or_else(|e| unreachable!("{e}"));
        assert_eq!(status.code(), Some(7), "{implementation:?}");
        // Same PID (exec, not a child), $0 is the script, no arguments, environment passed on.
        assert_eq!(log(dir.path()), format!("{pid} {script_text} 0 MARK=kept\n"), "{implementation:?}");
    }
}

#[test]
fn a_term_reaches_the_script() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let script = stub(dir.path());
    let script_text = script.display().to_string();
    let mut child = base(
        dir.path(),
        &[("NOEVIA_SUPERVISOR_SCRIPT", script_text.as_str()), ("WAIT_TERM", "1")],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap_or_else(|e| unreachable!("{e}"));
    let until = Instant::now() + Duration::from_secs(10);
    while !log(dir.path()).contains(" MARK=") && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = i32::try_from(child.id()).unwrap_or_else(|e| unreachable!("{e}"));
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGTERM);
    let status = child.wait().unwrap_or_else(|e| unreachable!("{e}"));
    assert_eq!(status.code(), Some(0));
    assert!(log(dir.path()).ends_with("TERM\n"), "{}", log(dir.path()));
}

#[test]
fn a_missing_script_is_bash_s_own_127() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let missing = dir.path().join("absent.sh").display().to_string();
    let out = output(base(dir.path(), &[("NOEVIA_SUPERVISOR_SCRIPT", missing.as_str())]));
    assert_eq!(out.status.code(), Some(127));
}

#[test]
fn rust_is_the_native_supervisor_unchanged() {
    // NOEVIA_FRONT=node: the native supervisor refuses before it looks at any binary or Core;
    // --image-entrypoint with NOEVIA_SUPERVISOR_IMPL=rust must answer exactly as the binary
    // without arguments does.
    let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
    let script = stub(dir.path());
    let script_text = script.display().to_string();
    let vars = [
        ("NOEVIA_SUPERVISOR_IMPL", "rust"),
        ("NOEVIA_SUPERVISOR_SCRIPT", script_text.as_str()),
        ("NOEVIA_FRONT", "node"),
    ];
    let via_entry = output(base(dir.path(), &vars));
    let mut plain = Command::new(BIN);
    plain.env_clear().current_dir(dir.path()).stdin(Stdio::null());
    if let Some(path) = std::env::var_os("PATH") {
        plain.env("PATH", path);
    }
    for (k, v) in vars {
        plain.env(k, v);
    }
    let direct = output(plain);
    assert_eq!(via_entry.status.code(), direct.status.code());
    assert_eq!(via_entry.stderr, direct.stderr);
    assert_eq!(via_entry.stdout, direct.stdout);
    assert_eq!(direct.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&direct.stderr).contains("NOEVIA_FRONT=node is no longer supported"));
    assert_eq!(log(dir.path()), "", "the script must not run");
}

#[test]
fn any_other_value_is_refused_and_nothing_runs() {
    for value in ["RUST", "1", "node", "native", " rust"] {
        let dir = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
        let script = stub(dir.path());
        let script_text = script.display().to_string();
        let out = output(base(
            dir.path(),
            &[("NOEVIA_SUPERVISOR_IMPL", value), ("NOEVIA_SUPERVISOR_SCRIPT", script_text.as_str())],
        ));
        assert_eq!(out.status.code(), Some(2), "{value:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("NOEVIA_SUPERVISOR_IMPL must be unset, sh or rust"), "{value:?}: {stderr}");
        assert_eq!(log(dir.path()), "", "{value:?}: the script must not run");
    }
}
