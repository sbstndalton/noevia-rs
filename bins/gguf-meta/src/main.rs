//! `gguf-meta <path>`: print a GGUF file's metadata summary as JSON on stdout.
//! Exit 0 on success; on error, a message on stderr and exit 1 (2 for bad usage).

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

fn fail(msg: &str, code: u8) -> ExitCode {
    let _ = writeln!(std::io::stderr().lock(), "gguf-meta: {msg}");
    ExitCode::from(code)
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let path = match args.as_slice() {
        [p] if p != "-h" && p != "--help" => Path::new(p),
        _ => return fail("usage: gguf-meta <path-to-gguf>", 2),
    };
    match gguf::summarize_file(path) {
        Ok(summary) => {
            let mut out = std::io::stdout().lock();
            if writeln!(out, "{summary}")
                .and_then(|()| out.flush())
                .is_err()
            {
                return fail("could not write to stdout", 1);
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("{}: {e}", path.display()), 1),
    }
}
