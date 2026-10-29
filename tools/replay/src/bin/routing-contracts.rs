//! Required complete actual-core replay and exact routing file bytes. (The valid-auth omission
//! control left with the routing switch, sbstndalton/noevia#1071: the front can no longer omit the
//! routes, and core's own tests prove Node refuses them.)
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};
#[allow(dead_code)]
#[path = "../../../qualification/process.rs"]
mod process;
fn bounded(command: Command) -> Result<process::Output, String> {
    process::bounded(command, Vec::new(), std::time::Duration::from_secs(600))
}
fn at<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn read(path: &Path) -> Result<Value, String> {
    serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}
fn replay(core: &Path, corpus: &Path, out: &Path, name: &str, front: bool) -> Result<bool, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let bin = exe.parent().ok_or("binary directory missing")?;
    let report = out.join(format!("{name}-report.json"));
    let state = out.join(format!("{name}-state.json"));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let command = if front {
        format!(
            "exec env NOEVIA_RUST_AUTH=1 sh {} {} {}",
            quote(&root.join("tools/front/replay-front.sh").to_string_lossy()),
            quote(&core.to_string_lossy()),
            quote(&bin.join("noevia-server").to_string_lossy())
        )
    } else {
        format!(
            "exec {} serve {}",
            quote(&bin.join("corpus-launcher").to_string_lossy()),
            quote(&core.to_string_lossy())
        )
    };
    let mut c = Command::new(bin.join("replay"));
    c.arg("run")
        .arg("--corpus")
        .arg(corpus)
        .args([
            "--base",
            "http://127.0.0.1:18021",
            "--ignore-body",
            "hours",
            "--ignore-body",
            "peakHour/hour",
            "--state-ignore",
            "*routing-mode.lock",
            // sbstndalton/noevia#1071: the front always serves the project lists
            // through its store, which takes these locks; Node's oracle does not.
            "--state-ignore",
            "*projects.json.lock",
            "--state-ignore",
            "*free-chats.json.lock",
            // sbstndalton/noevia#1071 slice 6: a chat's delete runs in the front's store, under
            // the transcript lock, and journals the change; Node's oracle writes neither.
            "--state-ignore",
            "*chat-transcripts.lock",
            "--state-ignore",
            "*chat-changes.jsonl",
        ])
        .arg("--seed")
        .arg(out.join("seed"))
        .arg("--work")
        .arg(out.join(name))
        .arg("--server-cmd")
        .arg(command)
        .arg("--report")
        .arg(&report)
        .arg("--state-out")
        .arg(&state);
    if front {
        c.arg("--native-move-state")
            .arg("--expect-state")
            .arg(out.join("node-state.json"));
    }
    let output = bounded(c)?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    Ok(output.status.success())
}
fn run() -> Result<(), String> {
    let mut a = std::env::args().skip(1);
    let core = PathBuf::from(a.next().ok_or("actual pinned core required")?)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    // The front runs before the Core generation paired with it (front-core.ref); the oracle at
    // core.ref records the routing corpus and answers the Node-direct references.
    let paired = PathBuf::from(a.next().ok_or("actual paired front core required")?)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let out = PathBuf::from(a.next().ok_or("new evidence directory required")?);
    let (resume, resume_node) = match a.next().as_deref() {
        None => (false, false),
        Some("--resume-routing") => (true, false),
        Some("--resume-native") => (true, true),
        _ => return Err("unknown routing contracts argument".into()),
    };
    if a.next().is_some() {
        return Err("unexpected routing contracts argument".into());
    }
    if out.exists() && !resume {
        return Err("evidence directory must be new".into());
    }
    let expected = include_str!("../../../../contracts/http/core.ref").trim();
    for (checkout, pin) in [
        (&core, expected),
        (
            &paired,
            include_str!("../../../../contracts/http/front-core.ref").trim(),
        ),
    ] {
        let mut command = Command::new("git");
        command.arg("-C").arg(checkout).args(["rev-parse", "HEAD"]);
        let head = bounded(command)?;
        if !head.status.success() || String::from_utf8_lossy(&head.stdout).trim() != pin {
            return Err("routing contracts require actual pinned core".into());
        }
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(checkout)
            .args(["status", "--porcelain", "--untracked-files=no"]);
        let clean = bounded(command)?;
        if !clean.status.success() || !clean.stdout.is_empty() {
            return Err("routing contracts require clean core".into());
        }
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let bin = exe.parent().ok_or("binary directory missing")?;
    let mut command = Command::new(bin.join("noevia-server"));
    command.arg("--features");
    let features = bounded(command)?;
    if !features.status.success()
        || !String::from_utf8_lossy(&features.stdout)
            .lines()
            .any(|l| l == "rust-routing-mode")
    {
        return Err("front lacks compiled routing capability".into());
    }
    if resume {
        for name in ["node", "positive"] {
            let report = read(&out.join(format!("baseline/{name}-report.json")))?;
            let rows = report.as_array().ok_or("prior baseline report missing")?;
            if rows.len() != 228
                || rows.iter().any(|row| {
                    at(row, "status") != at(row, "expectedStatus")
                        || !at(row, "error").is_null()
                        || at(row, "diffs")
                            .as_array()
                            .is_none_or(|diffs| !diffs.is_empty())
                })
            {
                return Err("resume requires complete prior228 zero-diff baseline".into());
            }
        }
        if read(&out.join("baseline/node-state.json"))?
            != read(&out.join("baseline/positive-state.json"))?
        {
            return Err("resume baseline states differ".into());
        }
        let source = read(&out.join("routing-corpus/SOURCE_ORACLE.json"))?;
        if at(&source, "core") != expected
            || at(&source, "captureDomain") != "routing-mode"
            || at(&source, "exchanges") != 255
            || at(&source, "dropped") != 0
        {
            return Err("resume routing provenance differs".into());
        }
        println!("explicit routing resume: retained prior228 Node/native zero-diff and equal-state reports; baseline not rerun");
        if resume_node {
            let report = read(&out.join("routing/node-report.json"))?;
            let rows = report
                .as_array()
                .ok_or("prior Node routing report missing")?;
            if rows.len() != 255
                || rows.iter().any(|row| {
                    at(row, "status") != at(row, "expectedStatus")
                        || !at(row, "error").is_null()
                        || at(row, "diffs")
                            .as_array()
                            .is_none_or(|diffs| !diffs.is_empty())
                })
            {
                return Err(
                    "native resume requires complete prior255 Node zero-diff report".into(),
                );
            }
            read(&out.join("routing/node-state.json"))?;
            println!(
                "explicit native resume: prior255 Node zero-diff report retained; Node not rerun"
            );
        }
    } else {
        std::fs::create_dir_all(out.join("baseline/seed")).map_err(|e| e.to_string())?;
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let baseline = root.join("contracts/http/corpus");
        if !replay(&core, &baseline, &out.join("baseline"), "node", false)?
            || !replay(&paired, &baseline, &out.join("baseline"), "positive", true)?
        {
            return Err("full existing228 Node/native/state replay failed".into());
        }
        let mut command = Command::new(bin.join("corpus-launcher"));
        command
            .env("NOEVIA_ROUTING_CORPUS", "1")
            .arg("capture")
            .arg(&core)
            .arg(out.join("routing-corpus"));
        let capture = bounded(command)?;
        print!("{}", String::from_utf8_lossy(&capture.stdout));
        eprint!("{}", String::from_utf8_lossy(&capture.stderr));
        if !capture.status.success() {
            return Err("actual routing corpus capture failed".into());
        }
    }
    let manifest = read(&out.join("routing-corpus/manifest.json"))?;
    println!(
        "routing corpus manifest captured {}",
        at(&manifest, "exchanges")
    );
    std::fs::create_dir_all(out.join("routing/seed")).map_err(|e| e.to_string())?;
    if (!resume_node
        && !replay(
            &core,
            &out.join("routing-corpus"),
            &out.join("routing"),
            "node",
            false,
        )?)
        || !replay(
            &paired,
            &out.join("routing-corpus"),
            &out.join("routing"),
            "positive",
            true,
        )?
    {
        return Err("new routing Node/native/state replay failed".into());
    }
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("routing contracts failed: {e}");
            ExitCode::FAILURE
        }
    }
}
