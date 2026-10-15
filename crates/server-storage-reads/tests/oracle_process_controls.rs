//! Failure-path controls for the bounded process seam every M4 oracle and source check uses
//! (`support::run`: the git source check and both actual-source Node seams), noevia#1371.
//! Synthetic Rust children only; no Node, Git checkout, network or credentials.
#[allow(dead_code)]
mod support;
use std::{
    process::Command,
    time::{Duration, Instant},
};
use support::process;

const PROBE: &str = env!("CARGO_BIN_EXE_private-oracle-probe");
const DEADLINE: Duration = Duration::from_millis(300);
/// Generous against scheduler noise, far below the 5 s the stalled probes would otherwise take.
const FINITE: Duration = Duration::from_secs(3);

fn probe(mode: &str) -> Command {
    let mut command = Command::new(PROBE);
    command.arg(mode);
    command
}

#[test]
fn stalled_and_noisy_children_are_refused_within_the_deadline() -> Result<(), String> {
    for (mode, expected) in [
        ("stall", "child deadline exceeded"),
        ("stdout-noise", "child output limit exceeded"),
        ("stderr-noise", "child output limit exceeded"),
    ] {
        let start = Instant::now();
        let result = support::run_within(probe(mode), Vec::new(), DEADLINE);
        assert_eq!(result.err().as_deref(), Some(expected), "{mode}");
        assert!(start.elapsed() < FINITE, "{mode} returned late");
    }
    Ok(())
}

#[test]
fn early_exit_while_parent_writes_input_is_a_refusal_not_a_hang() -> Result<(), String> {
    let start = Instant::now();
    // Larger than a pipe buffer, so the parent is still writing when the child exits.
    let result = support::run_within(probe("early"), vec![b'x'; process::INPUT], DEADLINE);
    assert!(
        result
            .as_ref()
            .err()
            .is_some_and(|e| e.starts_with("child stdin failed")),
        "{:?}",
        result.err()
    );
    assert!(start.elapsed() < FINITE);
    Ok(())
}

#[test]
fn input_and_serialization_caps_refuse_before_spawn() -> Result<(), String> {
    let result = support::run_within(probe("stall"), vec![b'x'; process::INPUT + 1], DEADLINE);
    assert!(result.is_err());
    assert!(process::json_input(&serde_json::json!("x".repeat(process::INPUT))).is_err());
    Ok(())
}

#[test]
fn leader_identity_is_retained_until_the_group_signal() -> Result<(), String> {
    let mut retained = false;
    let result = process::bounded_observed(
        &mut probe("early"),
        Vec::new(),
        DEADLINE,
        |_| {},
        |id| {
            // The leader has exited but is not reaped: its pid is still ours to signal.
            retained = i32::try_from(id)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
                .is_some_and(|pid| {
                    rustix::process::waitid(
                        rustix::process::WaitId::Pid(pid),
                        rustix::process::WaitIdOptions::EXITED
                            | rustix::process::WaitIdOptions::NOHANG
                            | rustix::process::WaitIdOptions::NOWAIT,
                    )
                    .is_ok_and(|status| status.is_some())
                });
        },
    )?;
    assert!(retained && result.status.success());
    Ok(())
}

#[test]
fn inherited_pipe_holders_cannot_delay_return_and_owned_ones_are_killed() -> Result<(), String> {
    // TempDir removes the pid files on every path, including a failed assertion.
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    for mode in ["inherited", "escaped"] {
        let pid_file = dir.path().join(mode);
        let mut command = probe(mode);
        command.arg(&pid_file);
        let start = Instant::now();
        let result = support::run_within(command, Vec::new(), DEADLINE);
        let elapsed = start.elapsed();
        let pid = std::fs::read_to_string(&pid_file)
            .map_err(|e| e.to_string())?
            .parse::<i32>()
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .ok_or("invalid synthetic child pid")?;
        let alive = rustix::process::test_kill_process(pid).is_ok();
        if mode == "escaped" {
            // Escaped descendants are outside the seam's cleanup authority; this test owns the
            // synthetic holder and signals it itself.
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        } else {
            // The owned group signal reached the descendant still in the leader's group. It may
            // linger briefly as a zombie of init, which the signal probe still reports, so only
            // require that it is gone or going within a bounded wait.
            let start = Instant::now();
            let mut gone = !alive;
            while !gone && start.elapsed() < FINITE {
                std::thread::sleep(Duration::from_millis(20));
                gone = rustix::process::test_kill_process(pid).is_err() || zombie(pid);
            }
            assert!(gone, "owned descendant survived the group signal");
        }
        assert_eq!(
            result.err().as_deref(),
            Some("child deadline exceeded"),
            "{mode}"
        );
        assert!(elapsed < FINITE, "{mode} returned late");
    }
    Ok(())
}

/// Best effort: a killed, unreaped orphan shows as a zombie (`Z`) in `ps`. Bounded output.
fn zombie(pid: rustix::process::Pid) -> bool {
    let mut command = Command::new("ps");
    command.args(["-o", "stat=", "-p", &pid.as_raw_nonzero().to_string()]);
    support::run_within(command, Vec::new(), Duration::from_secs(2))
        .is_ok_and(|o| o.stdout.starts_with(b"Z"))
}

/// The oracle source check opens with no-follow and requires one link (noevia#1827, #1371).
#[test]
fn oracle_sources_refuse_symlinks_hardlinks_and_fifos() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = |name: &str| dir.path().join(name).display().to_string();
    std::fs::write(path("source.cjs"), b"synthetic").map_err(|e| e.to_string())?;
    assert!(support::open_regular(&path("source.cjs")).is_ok());
    std::os::unix::fs::symlink(path("source.cjs"), path("link.cjs")).map_err(|e| e.to_string())?;
    assert!(support::open_regular(&path("link.cjs")).is_err());
    std::fs::hard_link(path("source.cjs"), path("second.cjs")).map_err(|e| e.to_string())?;
    assert!(support::open_regular(&path("source.cjs")).is_err());
    std::fs::create_dir(path("directory")).map_err(|e| e.to_string())?;
    assert!(support::open_regular(&path("directory")).is_err());
    let mut mkfifo = Command::new("mkfifo");
    mkfifo.arg(path("fifo"));
    let made = support::run_within(mkfifo, Vec::new(), Duration::from_secs(5))?;
    assert!(made.status.success(), "mkfifo failed");
    let start = Instant::now();
    assert!(support::open_regular(&path("fifo")).is_err());
    assert!(start.elapsed() < FINITE, "FIFO open blocked");
    Ok(())
}
