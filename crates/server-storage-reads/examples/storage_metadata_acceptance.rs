//! Private receipt admission and capability capture only. No public qualification.
#![forbid(unsafe_code)]
#[path = "support/storage_metadata_acceptance.rs"]
mod acceptance;
#[path = "support/storage_metadata_capture.rs"]
mod capture;

fn main() -> std::process::ExitCode {
    match acceptance::run(std::env::args().skip(1).collect()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("storage acceptance refused: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
