use server_storage_reads::{
    admission::Admission,
    isolated::{attempt, Error, INPUT},
};
use std::{
    process::Command,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;
fn command(mode: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_private-child-proof"));
    command.arg(mode);
    command
}
fn invoke(mode: &str, bytes: Vec<u8>, cap: usize, cancelled: bool) -> Result<Vec<u8>, Error> {
    let admission = Admission::default();
    let permit = admission
        .try_acquire("synthetic-owner")
        .map_err(|_| Error::Setup)?;
    let result = attempt(
        command(mode),
        Zeroizing::new(bytes),
        cap,
        Duration::from_millis(300),
        &AtomicBool::new(cancelled),
        permit,
    );
    // A completed call must have reaped its owned child before admission releases.
    assert!(admission.try_acquire("synthetic-owner").is_ok());
    result
}
#[test]
fn finite_ipc_and_byte_properties() {
    for size in [0, 1, 4095, 4096, 4097, 32768] {
        let bytes: Vec<u8> = (0..size).map(|n| (n % 256) as u8).collect();
        assert_eq!(
            invoke("fixture-echo", bytes.clone(), 32768, false),
            Ok(bytes)
        );
    }
    assert_eq!(
        invoke("fixture-echo", vec![0; INPUT + 1], 1, false),
        Err(Error::Input)
    );
    assert_eq!(
        invoke("fixture-echo", vec![0; 21], 20, false),
        Err(Error::Output)
    );
}
#[test]
fn readiness_noise_deadline_cancel_and_exit_controls() {
    assert_eq!(
        invoke("fixture-bad-ready", vec![0; 10], 10, false),
        Err(Error::Protocol)
    );
    assert_eq!(
        invoke("fixture-noise", vec![0; 10], 10, false),
        Err(Error::Output)
    );
    assert_eq!(
        invoke("fixture-extra", vec![0; 10], 10, false),
        Err(Error::Output)
    );
    assert_eq!(
        invoke("fixture-exit", vec![0; 10], 10, false),
        Err(Error::Exit)
    );
    assert_eq!(
        invoke("fixture-echo", vec![0; 10], 10, true),
        Err(Error::Cancelled)
    );
    let start = Instant::now();
    assert_eq!(
        invoke("fixture-stall", vec![0; INPUT], 10, false),
        Err(Error::Deadline)
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(
        invoke("fixture-no-ready", vec![0; 10], 10, false),
        Err(Error::Deadline)
    );
}
#[test]
fn platform_limit_boundary() {
    let result = invoke("linux-echo", b"fixed synthetic bytes".to_vec(), 128, false);
    #[cfg(not(target_os = "linux"))]
    assert_eq!(result, Err(Error::Protocol));
    // Actual Linux limit/readback proof must run on Linux CI; never infer from Mac.
    #[cfg(target_os = "linux")]
    assert_eq!(result, Ok(b"fixed synthetic bytes".to_vec()));
}
