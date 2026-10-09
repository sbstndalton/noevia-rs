//! `model-autoconfig size`: read a size request (JSON) on stdin and print the size plan (JSON)
//! on stdout, as noevia model-manager's `autoconfig_core.size_plan` returns it.
//! Exit 0 on success; on error, `model-autoconfig: <code>: <message>` on stderr, nothing on
//! stdout, and exit 1 (2 for bad usage). Never touches the network or the filesystem.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::process::ExitCode;

const USAGE: &str = "usage: model-autoconfig size < request.json";

fn fail(msg: &str, code: u8) -> ExitCode {
    let _ = writeln!(std::io::stderr().lock(), "model-autoconfig: {msg}");
    ExitCode::from(code)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 1 || args.first().map(String::as_str) != Some("size") {
        return fail(USAGE, 2);
    }
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
    match model_autoconfig::size_plan_json(&input) {
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
