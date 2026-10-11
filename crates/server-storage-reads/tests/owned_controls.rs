use server_storage_reads::{
    admission::Admission,
    isolated::{attempt, attempt_observed, Error, Observation, INPUT},
};
use std::{
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;
fn command(mode: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_private-child-proof"));
    command.arg(mode);
    command
}
#[test]
fn no_input_before_ready() {
    let admission = Admission::default();
    let permit = admission
        .try_acquire("owner")
        .unwrap_or_else(|_| unreachable!());
    assert_eq!(
        attempt(
            command("fixture-gated"),
            Zeroizing::new(b"synthetic sentinel".to_vec()),
            128,
            Duration::from_secs(2),
            &AtomicBool::new(false),
            permit
        ),
        Ok(b"synthetic sentinel".to_vec())
    );
}
#[test]
fn inflight_cancel_holds_admission_until_cleanup() {
    let admission = Admission::default();
    let permit = admission
        .try_acquire("owner")
        .unwrap_or_else(|_| unreachable!());
    let flag = AtomicBool::new(false);
    let mut progress = false;
    let mut cleanup = false;
    let result = attempt_observed(
        command("fixture-stall"),
        Zeroizing::new(vec![0; INPUT]),
        128,
        Duration::from_secs(2),
        &flag,
        permit,
        |stage, _| {
            assert!(admission.try_acquire("owner").is_err());
            match stage {
                Observation::InputProgress(_) => {
                    progress = true;
                    flag.store(true, Ordering::Release);
                }
                Observation::BeforeReap => cleanup = true,
            }
        },
    );
    assert_eq!(result, Err(Error::Cancelled));
    assert!(progress && cleanup);
    assert!(admission.try_acquire("owner").is_ok());
}
#[test]
fn inherited_pipes_retain_leader_before_group_signal() {
    let admission = Admission::default();
    let permit = admission
        .try_acquire("owner")
        .unwrap_or_else(|_| unreachable!());
    let mut seen = false;
    let start = Instant::now();
    let result = attempt_observed(
        command("fixture-inherited"),
        Zeroizing::new(b"synthetic".to_vec()),
        128,
        Duration::from_millis(200),
        &AtomicBool::new(false),
        permit,
        |stage, pid| {
            if stage == Observation::BeforeReap {
                assert!(admission.try_acquire("owner").is_err());
                let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap_or_default())
                    .unwrap_or_else(|| unreachable!());
                let state = rustix::process::waitid(
                    rustix::process::WaitId::Pid(pid),
                    rustix::process::WaitIdOptions::EXITED
                        | rustix::process::WaitIdOptions::NOHANG
                        | rustix::process::WaitIdOptions::NOWAIT,
                );
                assert!(matches!(state, Ok(Some(_))));
                seen = true;
            }
        },
    );
    assert_eq!(result, Err(Error::Deadline));
    assert!(seen);
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(admission.try_acquire("owner").is_ok());
}
#[test]
fn fixed_spawn_failure_releases_without_child() {
    let admission = Admission::default();
    let permit = admission
        .try_acquire("owner")
        .unwrap_or_else(|_| unreachable!());
    assert_eq!(
        attempt(
            Command::new("/nonexistent/m4-owned-synthetic-executable"),
            Zeroizing::new(Vec::new()),
            128,
            Duration::from_secs(1),
            &AtomicBool::new(false),
            permit
        ),
        Err(Error::Setup)
    );
    assert!(admission.try_acquire("owner").is_ok());
}
