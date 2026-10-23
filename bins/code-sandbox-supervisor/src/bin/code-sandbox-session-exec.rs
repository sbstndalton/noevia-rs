//! Enter a new session, then replace this process with the fixed sandbox harness.
//! This binary is installed beside code-sandbox-supervisor, never selected by a client.
#![forbid(unsafe_code)]

use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

const SESSION_READY: &[u8] = b"NOEVIA-SESSION-READY\n";
const SESSION_ACK: u8 = b'R';

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
    if std::io::stdout().write_all(SESSION_READY).is_err() || std::io::stdout().flush().is_err() {
        eprintln!("code-sandbox-session-exec: readiness write failed");
        return ExitCode::FAILURE;
    }
    // nix::read operates on fd 0 directly, so it cannot prefetch ACP bytes.
    let mut ack = [0_u8; 1];
    loop {
        match nix::unistd::read(std::io::stdin().as_fd(), &mut ack) {
            Ok(1) if ack[0] == SESSION_ACK => break,
            Err(nix::errno::Errno::EINTR) => continue,
            result => {
                eprintln!("code-sandbox-session-exec: invalid readiness ack: {result:?}");
                return ExitCode::FAILURE;
            }
        }
    }
    // exec inherits the supervisor-selected cwd, cleared environment and piped stdio.
    // It preserves this PID as the session and process-group leader.
    let error = Command::new(harness).args(args).exec();
    eprintln!("code-sandbox-session-exec: harness exec failed: {error}");
    ExitCode::FAILURE
}
