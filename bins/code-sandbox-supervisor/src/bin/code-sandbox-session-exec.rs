//! Enter a new session, then replace this process with the fixed sandbox harness.
//! This binary is installed beside code-sandbox-supervisor, never selected by a client.
#![forbid(unsafe_code)]

use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let _self_path = args.next();
    let Some(harness) = args.next() else {
        eprintln!("code-sandbox-session-exec: harness path is required");
        return ExitCode::FAILURE;
    };
    if let Err(error) = nix::unistd::setsid() {
        eprintln!("code-sandbox-session-exec: setsid failed: {error}");
        return ExitCode::FAILURE;
    }
    // exec inherits the supervisor-selected cwd, cleared environment and piped stdio.
    // It preserves this PID as the session and process-group leader.
    let error = Command::new(harness).args(args).exec();
    eprintln!("code-sandbox-session-exec: harness exec failed: {error}");
    ExitCode::FAILURE
}
