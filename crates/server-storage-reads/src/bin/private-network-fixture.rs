//! Separate synthetic network-envelope artifact; no Linux bypass/network calls.
use server_storage_reads::{
    child_request, isolated,
    response_ipc::{self, Reply},
};
use std::{io::Write, process::ExitCode};
fn run(mode: &str) -> Result<(), ()> {
    if !matches!(
        mode,
        "full"
            | "lookahead"
            | "boundary"
            | "legacy"
            | "bad-phase"
            | "bad-tag"
            | "nonzero"
            | "extra"
            | "stall"
            | "bad-flags"
            | "lookahead-stall"
    ) {
        return Err(());
    }
    let request =
        child_request::receive(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())
            .map_err(|_| ())?;
    let size = match mode {
        "full" | "nonzero" | "extra" | "stall" => request.body_cap,
        "lookahead" | "lookahead-stall" => request.body_cap + 1,
        _ => 4097,
    };
    let payload = vec![b'x'; size];
    let mut frame = response_ipc::encode(
        Reply::Http {
            status: 200,
            capped: size > request.body_cap,
            payload: &payload,
        },
        request.body_cap,
    )
    .map_err(|_| ())?;
    if mode == "legacy" {
        return child_request::nested(&mut std::io::stdout().lock(), &frame).map_err(|_| ());
    }
    if mode == "bad-phase" {
        if let Some(byte) = frame.get_mut(9) {
            *byte = 9;
        }
    }
    if mode == "bad-flags" {
        if let Some(byte) = frame.get_mut(13) {
            *byte = 2;
        }
    }
    let mut output = std::io::stdout().lock();
    output
        .write_all(if mode == "bad-tag" {
            isolated::BODY
        } else {
            isolated::NET
        })
        .map_err(|_| ())?;
    output
        .write_all(&u32::try_from(frame.len()).map_err(|_| ())?.to_be_bytes())
        .map_err(|_| ())?;
    if mode == "bad-phase" {
        output
            .write_all(frame.get(..response_ipc::METADATA).ok_or(())?)
            .map_err(|_| ())?;
        output.flush().map_err(|_| ())?;
        std::thread::sleep(std::time::Duration::from_secs(5));
        return Err(());
    }
    output.write_all(&frame).map_err(|_| ())?;
    if mode == "extra" {
        output.write_all(b"x").map_err(|_| ())?;
    }
    output.flush().map_err(|_| ())?;
    if matches!(mode, "stall" | "lookahead-stall") {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    if mode == "nonzero" {
        return Err(());
    }
    Ok(())
}
fn main() -> ExitCode {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if run(&mode).is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
