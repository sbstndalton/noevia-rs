//! Synthetic Rust children exercise the exact M6 process seam used by Git/Node.
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use std::{
    io::Write,
    os::unix::process::CommandExt,
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
        "early" => Ok(()),
        "stdout-noise" => std::io::stdout()
            .write_all(&vec![b'x'; process::INPUT + 1])
            .map_err(|e| e.to_string()),
        "stderr-noise" => {
            std::io::stdout()
                .write_all(b"fair-stdout\n")
                .map_err(|e| e.to_string())?;
            std::io::stderr()
                .write_all(&vec![b'x'; 65537])
                .map_err(|e| e.to_string())
        }
        "inherited" | "escaped" => {
            let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
            command.args(["--probe", "stall"]);
            if mode == "escaped" {
                command.process_group(0);
            }
            let child = command.spawn().map_err(|e| e.to_string())?;
            std::fs::write(
                std::env::args().nth(3).ok_or("missing owned pid file")?,
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
fn run() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    for (mode, expected) in [
        ("stall", "child deadline exceeded"),
        ("stdout-noise", "child output limit exceeded"),
        ("stderr-noise", "child output limit exceeded"),
    ] {
        let mut command = Command::new(&exe);
        command.args(["--probe", mode]);
        let start = Instant::now();
        let result = process::bounded(command, Vec::new(), Duration::from_millis(300));
        check(result.as_ref().err().is_some_and(|e| e == expected), mode)?;
        check(start.elapsed() < Duration::from_secs(2), "finite return")?;
    }
    let mut command = Command::new(&exe);
    command.args(["--probe", "early"]);
    let result = process::bounded(
        command,
        vec![b'x'; process::INPUT],
        Duration::from_millis(300),
    );
    check(
        result
            .as_ref()
            .err()
            .is_some_and(|e| e.starts_with("child stdin failed")),
        "early stdin closure",
    )?;
    let mut command = Command::new(&exe);
    command.args(["--probe", "early"]);
    let mut retained = false;
    let result = process::bounded_observed(
        &mut command,
        Vec::new(),
        Duration::from_millis(300),
        |_| {},
        |id| {
            retained = i32::try_from(id)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
                .is_some_and(|pid| {
                    rustix::process::waitid(
                        rustix::process::WaitId::Pid(pid),
                        rustix::process::WaitIdOptions::EXITED
                            | rustix::process::WaitIdOptions::NOHANG
                            | rustix::process::WaitIdOptions::NOWAIT,
                    )
                    .is_ok_and(|status| status.is_some())
                });
        },
    )?;
    check(
        retained && result.status.success(),
        "retained leader before group signal",
    )?;
    let dir = std::env::temp_dir().join(format!("m4-oracle-process-{}", std::process::id()));
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| e.to_string())?;
    for mode in ["inherited", "escaped"] {
        let pid_file = dir.join(mode);
        let mut command = Command::new(&exe);
        command.args(["--probe", mode]).arg(&pid_file);
        let start = Instant::now();
        let result = process::bounded(command, Vec::new(), Duration::from_millis(300));
        let pid = std::fs::read_to_string(&pid_file)
            .map_err(|e| e.to_string())?
            .parse::<i32>()
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .ok_or("invalid synthetic child pid")?;
        if mode == "escaped" {
            // Escaped descendants are outside the seam's cleanup authority.
            // This test owns and explicitly signals only the synthetic holder.
            rustix::process::kill_process_group(pid, rustix::process::Signal::KILL)
                .map_err(|e| e.to_string())?;
        }
        std::fs::remove_file(&pid_file).map_err(|e| e.to_string())?;
        check(
            result
                .as_ref()
                .err()
                .is_some_and(|e| e == "child deadline exceeded")
                && start.elapsed() < Duration::from_secs(2),
            mode,
        )?;
    }
    std::fs::remove_dir(dir).map_err(|e| e.to_string())?;
    check(
        process::json_input(&serde_json::json!("x".repeat(process::INPUT))).is_err(),
        "bounded JSON input serialization",
    )?;
    println!("stall/stdout+stderr noise/early stdin/retained leader/inherited+escaped pipes/input refusal passed");
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
        Err(e) => {
            eprintln!("private oracle process control: {e}");
            ExitCode::FAILURE
        }
    }
}
