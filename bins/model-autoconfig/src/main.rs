//! `model-autoconfig size`: read a size request (JSON) on stdin and print the size plan (JSON)
//! on stdout, as noevia model-manager's `autoconfig_core.size_plan` returns it.
//! `model-autoconfig check`: read `{"prep"?, "size"?, "values"?, "spec"?, "files"?, "present"?,
//! "baseline"?}` and print the answer to each part asked for, as
//! `autoconfig_core.check_reference` returns it (what the service runs).
//! Exit 0 on success; on error, `model-autoconfig: <code>: <message>` on stderr, nothing on
//! stdout, and exit 1 (2 for bad usage). Never touches the network or the filesystem.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::process::ExitCode;

const USAGE: &str = "usage: model-autoconfig size|check < request.json";

fn fail(msg: &str, code: u8) -> ExitCode {
    let _ = writeln!(std::io::stderr().lock(), "model-autoconfig: {msg}");
    ExitCode::from(code)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let op: fn(&[u8]) -> Result<String, model_autoconfig::Error> =
        match (args.len(), args.first().map(String::as_str)) {
            (1, Some("size")) => model_autoconfig::size_plan_json,
            (1, Some("check")) => model_autoconfig::check_json,
            _ => return fail(USAGE, 2),
        };
    let mut input = Vec::new();
    let cap = model_autoconfig::MAX_INPUT_BYTES as u64 + 1;
    if std::io::stdin()
        .lock()
        .take(cap)
        .read_to_end(&mut input)
        .is_err()
    {
        return fail("could not read stdin", 1);
    }
    match op(&input) {
        Ok(json) => {
            let mut out = std::io::stdout().lock();
            if writeln!(out, "{json}").and_then(|()| out.flush()).is_err() {
                return fail("could not write to stdout", 1);
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("{}: {e}", e.code()), 1),
    }
}
