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
    ) {
        return Err(());
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
