//! Synthetic misbehaving child for the bounded oracle/qualification process seams (noevia#1371).
//! Test-only: it never reads credentials, opens sockets or touches anything but its own pipes
//! and an explicitly named pid file.
use std::{
    io::Write,
    os::unix::process::CommandExt,
    process::{Command, ExitCode},
    time::Duration,
};

/// Larger than every capture cap the seams enforce (8 MiB stdout, 64 KiB stderr).
const NOISE: usize = 8 * 1024 * 1024 + 1;

fn probe(mode: &str, pid_file: Option<&str>) -> Result<(), String> {
    match mode {
        // Exit at once without reading stdin, so a parent still writing input sees closure.
        "early" => Ok(()),
        "stdout-noise" => std::io::stdout()
            .write_all(&vec![b'x'; NOISE])
            .map_err(|e| e.to_string()),
        "stderr-noise" => {
            std::io::stdout()
                .write_all(b"fair-stdout\n")
                .map_err(|e| e.to_string())?;
            std::io::stderr()
                .write_all(&vec![b'x'; 65537])
                .map_err(|e| e.to_string())
        }
        // First-attempt evidence: partial output that must survive a deadline refusal.
        "partial" => {
            std::io::stdout()
                .write_all(b"partial-stdout")
                .and_then(|()| std::io::stdout().flush())
                .map_err(|e| e.to_string())?;
            std::io::stderr()
                .write_all(b"partial-stderr")
                .and_then(|()| std::io::stderr().flush())
                .map_err(|e| e.to_string())?;
            std::thread::sleep(Duration::from_secs(5));
            Ok(())
        }
        // A descendant keeps the inherited pipes open after the leader exits; "escaped" also
        // leaves the owned process group, putting it outside the seam's cleanup authority.
        "inherited" | "escaped" => {
            let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
            command.arg("stall");
            if mode == "escaped" {
                command.process_group(0);
            }
            let child = command.spawn().map_err(|e| e.to_string())?;
            std::fs::write(
                pid_file.ok_or("missing owned pid file")?,
                child.id().to_string(),
            )
            .map_err(|e| e.to_string())
        }
        "stall" => {
            std::thread::sleep(Duration::from_secs(5));
            Ok(())
        }
        _ => Err("unknown synthetic probe".into()),
    }
}

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let mode = args.first().map(String::as_str).unwrap_or_default();
    match probe(mode, args.get(1).map(String::as_str)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("private oracle probe: {e}");
            ExitCode::FAILURE
        }
    }
}
