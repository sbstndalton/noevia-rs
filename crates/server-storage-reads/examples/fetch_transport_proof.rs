//! Finite official-registry dependency fetch; no adapter or remote storage access.
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use std::{
    process::{Command, ExitCode},
    time::Duration,
};
fn main() -> ExitCode {
    let cargo = std::env::var_os("M4_CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .args(["fetch", "--manifest-path"])
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("proofs/transport/Cargo.toml"));
    match process::bounded(command, Vec::new(), Duration::from_secs(120)) {
        Ok(output) if output.status.success() => {
            println!("official registry dependency fetch completed; diagnostics withheld");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("bounded dependency fetch failed; diagnostics withheld");
            ExitCode::FAILURE
        }
    }
}
