//! `turn-logs decisions <dir>`: core `node server/decision-log.cjs <dir>` (a summary of
//! `system-one-decisions.jsonl.1` + `system-one-decisions.jsonl`, no individual rows printed).
//! `turn-logs context <file>...`: core `node server/context-log.cjs <file>...` (the report).
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;
use turn_logs::{context, decision, pretty};

const USAGE: &str = "usage: turn-logs decisions <data dir> | turn-logs context <context-log.jsonl> [...]";

fn print(text: &str) -> ExitCode {
    match writeln!(std::io::stdout().lock(), "{text}") {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.split_first() {
        // Core defaults the directory to server/ui-data beside the script; a binary has no such
        // place, so the directory is required.
        Some((cmd, [dir])) if cmd == "decisions" => {
            let lines = decision::read_dir_lines(Path::new(dir));
            let summary = decision::summarize(lines.iter().map(String::as_str));
            print(&pretty::stringify(&summary, 1).unwrap_or_default())
        }
        Some((cmd, files)) if cmd == "context" && !files.is_empty() => {
            let mut lines = Vec::new();
            for f in files {
                // core: readFileSync throws (exit 1) on a missing file.
                match std::fs::read(f) {
                    Ok(bytes) => lines.extend(
                        String::from_utf8_lossy(&bytes)
                            .split('\n')
                            .filter(|l| !l.is_empty())
                            .map(str::to_owned),
                    ),
                    Err(e) => {
                        let _ = writeln!(std::io::stderr().lock(), "turn-logs: {f}: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            let r = context::report(lines.iter().map(String::as_str));
            print(&pretty::stringify(&r, 2).unwrap_or_default())
        }
        _ => {
            let _ = writeln!(std::io::stderr().lock(), "{USAGE}");
            ExitCode::from(2)
        }
    }
}
