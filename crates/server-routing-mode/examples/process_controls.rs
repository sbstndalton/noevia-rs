//! Focused bounded-process and classifier infrastructure controls; no Node policy.
#[allow(dead_code)]
#[path = "../../../tools/qualification/mutation.rs"]
mod mutation;
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use std::{
    io::Write,
    os::unix::{fs::DirBuilderExt, process::CommandExt},
    process::{Command, ExitCode},
    time::{Duration, Instant},
};
fn check(ok: bool, name: &str) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(format!("control failed: {name}"))
    }
}
fn probe(mode: &str) -> Result<(), String> {
    match mode {
        "exit" => Ok(()),
        "oversized" => std::io::stdout()
            .write_all(&vec![b'x'; process::INPUT + 1])
            .map_err(|e| e.to_string()),
        "detached" => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let child = Command::new(exe)
                .args(["--probe", "holder"])
                .process_group(0)
                .spawn()
                .map_err(|e| e.to_string())?;
            std::fs::write(
                std::env::args().nth(3).ok_or("missing pid file")?,
                child.id().to_string(),
            )
            .map_err(|e| e.to_string())
        }
        "holder" => {
            std::thread::sleep(Duration::from_secs(5));
            Ok(())
        }
        _ => {
            std::thread::sleep(Duration::from_secs(5));
            Ok(())
        }
    }
}
fn run() -> Result<(), String> {
    use rustix::process::{waitid, WaitId, WaitIdOptions};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(&exe);
    command.args(["--probe", "exit"]);
    let mut retained = false;
    let output = process::bounded_observed(
        &mut command,
        Vec::new(),
        Duration::from_millis(300),
        |_| {},
        |id| {
            retained = rustix::process::Pid::from_raw(id as i32).is_some_and(|pid| {
                waitid(
                    WaitId::Pid(pid),
                    WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
                )
                .is_ok_and(|v| v.is_some())
            });
        },
    )?;
    check(retained && output.status.success(), "signal-before-reap")?;
    for (mode, expected) in [
        ("stall", "child deadline exceeded"),
        ("oversized", "child output limit exceeded"),
    ] {
        let mut command = Command::new(&exe);
        command.args(["--probe", mode]);
        let result = process::bounded(command, Vec::new(), Duration::from_millis(300));
        check(
            result.as_ref().err().is_some_and(|error| error == expected),
            mode,
        )?;
    }
    let directory =
        std::env::temp_dir().join(format!("routing-process-control-{}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .map_err(|e| e.to_string())?;
    let pid_file = directory.join("pid");
    let mut command = Command::new(&exe);
    command.args(["--probe", "detached"]).arg(&pid_file);
    let start = Instant::now();
    let result = process::bounded(
        command,
        vec![b'x'; process::INPUT],
        Duration::from_millis(300),
    );
    let elapsed = start.elapsed();
    let holder = std::fs::read_to_string(&pid_file)
        .map_err(|e| e.to_string())?
        .parse::<i32>()
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or("invalid owned synthetic pid")?;
    let alive = rustix::process::test_kill_process(holder).is_ok();
    rustix::process::kill_process_group(holder, rustix::process::Signal::KILL)
        .map_err(|e| e.to_string())?;
    std::fs::remove_file(pid_file).map_err(|e| e.to_string())?;
    std::fs::remove_dir(directory).map_err(|e| e.to_string())?;
    check(
        alive
            && elapsed < Duration::from_millis(1500)
            && result
                .as_ref()
                .err()
                .is_some_and(|e| e == "child deadline exceeded"),
        "detached pipe holder deadline",
    )?;
    check(
        process::json_input(&serde_json::json!("x".repeat(process::INPUT))).is_err(),
        "input serialization limit",
    )?;
    println!("stalled/oversized/input limit/retained leader/detached inherited stdin+stdout+stderr controls passed; deadline returned{}ms; escaped synthetic holder explicitly signalled",elapsed.as_millis());
    Ok(())
}
fn main() -> ExitCode {
    let result = if std::env::args().nth(1).as_deref() == Some("--probe") {
        probe(&std::env::args().nth(2).unwrap_or_default())
    } else {
        run()
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("process control: {error}");
            ExitCode::FAILURE
        }
    }
}
