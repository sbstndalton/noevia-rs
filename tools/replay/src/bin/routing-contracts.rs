//! Required complete actual-core replay, exact routing file bytes and valid-auth omission.
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};
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
fn replay(
    core: &Path,
    corpus: &Path,
    out: &Path,
    name: &str,
    front: bool,
    omission: bool,
) -> Result<bool, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let bin = exe.parent().ok_or("binary directory missing")?;
    let report = out.join(format!("{name}-report.json"));
    let state = out.join(format!("{name}-state.json"));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let command = if front {
        format!(
            "exec env NOEVIA_RUST_AUTH=1 NOEVIA_RUST_ROUTING_MODE=1 {} sh {} {} {}",
            if omission {
                "REPLAY_FRONT_CONTROL_NODE_ONLY_ROUTING_MODE=1"
            } else {
                ""
            },
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
    if front && !omission {
        c.arg("--expect-state").arg(out.join("node-state.json"));
    }
    let status = c.status().map_err(|e| e.to_string())?;
    Ok(status.success())
}
fn omission(out: &Path) -> Result<(), String> {
    let report = read(&out.join("omission-report.json"))?;
    let rows = report.as_array().ok_or("omission report rows missing")?;
    let routing = rows
        .iter()
        .filter(|r| {
            at(r, "path")
                .as_str()
                .is_some_and(|p| p == "/api/routing-mode" || p == "/api/routing-mode/allowed")
                && at(r, "expectedStatus") != 401
        })
        .collect::<Vec<_>>();
    let sessions = rows
        .iter()
        .filter(|r| {
            at(r, "method") == "GET"
                && at(r, "path") == "/api/auth/session"
                && at(r, "expectedStatus") == 200
        })
        .collect::<Vec<_>>();
    if routing.len() < 20
        || routing.iter().any(|r| at(r, "status") != 503)
        || sessions.len() != 3
        || sessions.iter().any(|r| at(r, "status") != 200)
    {
        return Err("valid-identity omission must refuse all routing methods503 and keep3 unrelated sessions200".into());
    }
    let state = read(&out.join("omission-state.json"))?;
    let db = at(&state, "db");
    if at(db, "settings")
        .as_array()
        .ok_or("omission settings missing")?
        .iter()
        .any(|r| at(r, "key") == "routing_modes_allowed")
        || at(db, "audit_events")
            .as_array()
            .ok_or("omission audit missing")?
            .iter()
            .any(|r| at(r, "action") == "routing.allowed")
        || at(&state, "routingModeFiles")
            .as_object()
            .is_none_or(|m| !m.is_empty())
    {
        return Err("omission unexpectedly mutated routing state".into());
    }
    println!(
        "valid omission routing503 count={} unrelated sessions200=3 no setting/file/audit writes",
        routing.len()
    );
    Ok(())
}
fn run() -> Result<(), String> {
    let mut a = std::env::args().skip(1);
    let core = PathBuf::from(a.next().ok_or("actual pinned core required")?)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let out = PathBuf::from(a.next().ok_or("new evidence directory required")?);
    if out.exists() {
        return Err("evidence directory must be new".into());
    }
    let expected = include_str!("../../../../contracts/http/core.ref").trim();
    let head = Command::new("git")
        .arg("-C")
        .arg(&core)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| e.to_string())?;
    if !head.status.success() || String::from_utf8_lossy(&head.stdout).trim() != expected {
        return Err("routing contracts require actual pinned core".into());
    }
    let clean = Command::new("git")
        .arg("-C")
        .arg(&core)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .map_err(|e| e.to_string())?;
    if !clean.status.success() || !clean.stdout.is_empty() {
        return Err("routing contracts require clean core".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let bin = exe.parent().ok_or("binary directory missing")?;
    let features = Command::new(bin.join("noevia-server"))
        .arg("--features")
        .output()
        .map_err(|e| e.to_string())?;
    if !features.status.success()
        || !String::from_utf8_lossy(&features.stdout)
            .lines()
            .any(|l| l == "rust-routing-mode")
    {
        return Err("front lacks compiled routing capability".into());
    }
    std::fs::create_dir_all(out.join("baseline/seed")).map_err(|e| e.to_string())?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let baseline = root.join("contracts/http/corpus");
    if !replay(
        &core,
        &baseline,
        &out.join("baseline"),
        "node",
        false,
        false,
    )? || !replay(
        &core,
        &baseline,
        &out.join("baseline"),
        "positive",
        true,
        false,
    )? {
        return Err("full existing193 Node/native/state replay failed".into());
    }
    let status = Command::new(bin.join("corpus-launcher"))
        .env("NOEVIA_ROUTING_CORPUS", "1")
        .arg("capture")
        .arg(&core)
        .arg(out.join("routing-corpus"))
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("actual routing corpus capture failed".into());
    }
    let manifest = read(&out.join("routing-corpus/manifest.json"))?;
    println!(
        "routing corpus manifest captured {}",
        at(&manifest, "exchanges")
    );
    std::fs::create_dir_all(out.join("routing/seed")).map_err(|e| e.to_string())?;
    if !replay(
        &core,
        &out.join("routing-corpus"),
        &out.join("routing"),
        "node",
        false,
        false,
    )? || !replay(
        &core,
        &out.join("routing-corpus"),
        &out.join("routing"),
        "positive",
        true,
        false,
    )? {
        return Err("new routing Node/native/state replay failed".into());
    }
    if replay(
        &core,
        &out.join("routing-corpus"),
        &out.join("routing"),
        "omission",
        true,
        true,
    )? {
        return Err("omission unexpectedly passed".into());
    }
    omission(&out.join("routing"))?;
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
