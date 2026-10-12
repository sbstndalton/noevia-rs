//! Required negative control: an actual 12 GiB input must fail the fixed 8 GiB corpus.
use std::{
    collections::BTreeSet,
    path::PathBuf,
    process::{Command, ExitCode},
};
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let core = args.next().ok_or("core required")?;
    let corpus = args.next().ok_or("corpus required")?;
    let out = PathBuf::from(args.next().ok_or("output required")?);
    if args.next().is_some() {
        return Err("too many arguments".into());
    }
    std::fs::create_dir_all(out.join("seed")).map_err(|e| e.to_string())?;
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let binaries = executable.parent().ok_or("binary directory missing")?;
    let launcher = binaries.join("corpus-launcher");
    // Shell quoting is required because replay intentionally accepts a shell command.
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let command = format!(
        "exec {} wrong-host {}",
        quote(&launcher.to_string_lossy()),
        quote(&core)
    );
    let report = out.join("wrong-host-report.json");
    let status = Command::new(binaries.join("replay"))
        .args([
            "run",
            "--corpus",
            &corpus,
            "--base",
            "http://127.0.0.1:18021",
            "--ignore-body",
            "hours",
            "--ignore-body",
            "peakHour/hour",
        ])
        .arg("--seed")
        .arg(out.join("seed"))
        .arg("--work")
        .arg(out.join("work"))
        .arg("--server-cmd")
        .arg(command)
        .arg("--report")
        .arg(&report)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        return Err("wrong host unexpectedly passed".into());
    }
    let rows: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let rows = rows.as_array().ok_or("report must be an array")?;
    if rows.len() != 207 {
        return Err("wrong-host replay lost exchanges".into());
    }
    let mut fields = BTreeSet::new();
    for row in rows {
        if row.get("error").is_some_and(|e| !e.is_null())
            || row.get("status") != row.get("expectedStatus")
        {
            return Err("wrong-host control had transport/status failure".into());
        }
        let diffs = row
            .get("diffs")
            .and_then(serde_json::Value::as_array)
            .ok_or("diff list missing")?;
        if !diffs.is_empty()
            && row.get("path").and_then(serde_json::Value::as_str)
                != Some("/api/models/inference-budget")
        {
            return Err("unexpected non-budget difference".into());
        }
        for diff in diffs {
            fields.insert(
                diff.get("at")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("diff path missing")?
                    .to_string(),
            );
        }
    }
    let expected = BTreeSet::from([
        "body/budgetGib".to_string(),
        "body/hostRamGib".to_string(),
        "body/maxGib".to_string(),
    ]);
    if fields != expected {
        return Err(format!(
            "wrong host must change all strict budget fields: {fields:?}"
        ));
    }
    println!("207 actual exchanges: wrong host rejected at budgetGib, hostRamGib, maxGib; all other fields/statuses preserved");
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("host-control: {e}");
            ExitCode::from(2)
        }
    }
}
