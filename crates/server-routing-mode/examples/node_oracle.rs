#[path = "../tests/support/mod.rs"]
mod support;
use serde_json::json;
use std::{path::PathBuf, process::ExitCode};
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let core = PathBuf::from(args.next().ok_or("actual core required")?);
    let output = PathBuf::from(args.next().ok_or("output required")?);
    let expected = std::env::var("NOEVIA_ROUTING_CORE_SHA").unwrap_or_else(|_| {
        include_str!("../../../contracts/http/core.ref")
            .trim()
            .into()
    });
    let source = support::source(&core, &expected)?;
    let cases = support::cases()?;
    let expected = support::oracle(&core, &cases)?;
    if expected.as_array().map(Vec::len) != Some(cases.len()) {
        return Err("oracle case count mismatch".into());
    }
    let fixture = json!({"source":source,"now":support::NOW,"cases":cases,"expected":expected});
    if let Some(checked) = args.next() {
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(checked).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        if saved != fixture {
            return Err("actual merged-core routing fixture changed".into());
        }
    }
    std::fs::write(output, fixture.to_string()).map_err(|e| e.to_string())?;
    println!("actual Node routing cases={}", cases.len());
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("routing oracle refused: {e}");
            ExitCode::FAILURE
        }
    }
}
