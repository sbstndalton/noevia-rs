//! Fixed synthetic child only; no sockets, resolver, keys or transport adapter.
use server_storage_reads::isolated::{self, BODY, INPUT, READY};
use std::{
    io::{Read, Write},
    process::ExitCode,
};
use zeroize::Zeroizing;
fn run(mode: &str) -> Result<(), ()> {
    // Fixture bypass is deliberately named and never a production capability.
    if mode == "linux-echo" {
        isolated::linux_setup().map_err(|_| ())?;
    } else if !matches!(
        mode,
        "fixture-echo"
            | "fixture-stall"
            | "fixture-bad-ready"
            | "fixture-noise"
            | "fixture-extra"
            | "fixture-exit"
            | "fixture-no-ready"
            | "fixture-gated"
            | "fixture-inherited"
            | "fixture-holder"
    ) {
        return Err(());
    }
    if mode == "fixture-holder" {
        std::thread::sleep(std::time::Duration::from_secs(5));
        return Ok(());
    }
    if mode == "fixture-gated" {
        // Observe the pipe before READY: any byte/EOF is a protocol failure.
        let stdin = std::io::stdin();
        let old = rustix::fs::fcntl_getfl(&stdin).map_err(|_| ())?;
        rustix::fs::fcntl_setfl(&stdin, old | rustix::fs::OFlags::NONBLOCK).map_err(|_| ())?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut byte = [0];
        match stdin.lock().read(&mut byte) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            _ => return Err(()),
        }
        rustix::fs::fcntl_setfl(&stdin, old).map_err(|_| ())?;
    }
    if mode == "fixture-no-ready" {
        std::thread::sleep(std::time::Duration::from_secs(5));
        return Err(());
    }
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(if mode == "fixture-bad-ready" {
            b"BADREADY"
        } else {
            READY
        })
        .map_err(|_| ())?;
    stdout.flush().map_err(|_| ())?;
    if mode == "fixture-inherited" {
        // Explicit synthetic descendant; stays in the owned group and inherits pipes.
        let mut command = std::process::Command::new(std::env::current_exe().map_err(|_| ())?);
        command
            .arg("fixture-holder")
            .env_clear()
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
        let _owned_group_descendant = command.spawn().map_err(|_| ())?;
    }
    if mode == "fixture-stall" {
        std::thread::sleep(std::time::Duration::from_secs(5));
        return Err(());
    }
    let mut stdin = std::io::stdin().lock();
    let mut length = [0; 4];
    stdin.read_exact(&mut length).map_err(|_| ())?;
    let size = usize::try_from(u32::from_be_bytes(length)).map_err(|_| ())?;
    if size > INPUT {
        return Err(());
    }
    let mut input = Zeroizing::new(Vec::new());
    input.try_reserve_exact(size).map_err(|_| ())?;
    input.resize(size, 0);
    stdin.read_exact(&mut input).map_err(|_| ())?;
    let mut extra = [0];
    if stdin.read(&mut extra).map_err(|_| ())? != 0 {
        return Err(());
    }
    if mode == "fixture-noise" {
        std::io::stderr().write_all(&[b'x'; 4097]).map_err(|_| ())?;
        std::thread::sleep(std::time::Duration::from_secs(5));
        return Err(());
    }
    stdout.write_all(BODY).map_err(|_| ())?;
    stdout.write_all(&length).map_err(|_| ())?;
    stdout.write_all(&input).map_err(|_| ())?;
    if mode == "fixture-extra" {
        stdout.write_all(b"x").map_err(|_| ())?;
    }
    stdout.flush().map_err(|_| ())?;
    if mode == "fixture-exit" {
        return Err(());
    }
    Ok(())
}
fn main() -> ExitCode {
    // The only argument is a fixed proof mode, never input or a private endpoint.
    let mode = std::env::args().nth(1).unwrap_or_default();
    if run(&mode).is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
