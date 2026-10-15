//! Separate portable synthetic artifact: no Linux bypass in private-request-child.
use server_storage_reads::{
    child_request,
    protocol::Method,
    response_ipc::{self, Reply},
};
use std::process::ExitCode;
fn run() -> Result<(), ()> {
    let request =
        child_request::receive(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())
            .map_err(|_| ())?;
    let payload: &[u8] = match request.method {
        Method::Get => b"synthetic GET",
        Method::Propfind => b"synthetic PROPFIND",
    };
    let frame = response_ipc::encode(
        Reply::Http {
            status: 207,
            capped: false,
            payload,
        },
        request.body_cap,
    )
    .map_err(|_| ())?;
    child_request::nested(&mut std::io::stdout().lock(), &frame).map_err(|_| ())
}
fn main() -> ExitCode {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if !matches!(mode.as_str(), "" | "nonzero" | "extra" | "stall") || run().is_err() {
        return ExitCode::FAILURE;
    }
    match mode.as_str() {
        "nonzero" => ExitCode::FAILURE,
        "extra" => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(b"x");
            ExitCode::SUCCESS
        }
        "stall" => {
            std::thread::sleep(std::time::Duration::from_secs(5));
            ExitCode::SUCCESS
        }
        _ => ExitCode::SUCCESS,
    }
}
