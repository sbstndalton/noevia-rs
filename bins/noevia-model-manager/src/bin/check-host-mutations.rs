#![forbid(unsafe_code)]
//! Run only in an isolated source checkout, after the unmodified tests passed.
use std::{fs, path::Path, process::Command};
fn mutation(
    path: &Path,
    before: &str,
    after: &str,
    test: &str,
    filter: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let original = fs::read_to_string(path)?;
    if original.matches(before).count() != 1 {
        return Err("host mutation target must occur exactly once".into());
    }
    fs::write(path, original.replace(before, after))?;
    let outcome = Command::new("cargo")
        .args([
            "test",
            "--locked",
            "-q",
            "-p",
            "noevia-model-manager",
            "--features",
            "native-host",
            "--test",
            test,
            filter,
        ])
        .output();
    // Restore before inspecting status, including a failed child spawn.
    fs::write(path, original)?;
    let outcome = outcome?;
    if outcome.status.success() {
        return Err(format!("host mutation survived: {filter}").into());
    }
    // Compilation errors are not evidence of behavioral detection.
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    if !stdout.contains("test result: FAILED") || !stdout.contains(filter) {
        return Err(format!("host mutation did not reach failing assertion: {filter}").into());
    }
    println!("detected {filter}");
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = Path::new("bins/noevia-model-manager/src/host.rs");
    mutation(
        host,
        "self.last_cpu = Some((idle, total));",
        "if mem.is_ok() { self.last_cpu = Some((idle, total)); }",
        "host",
        "actual_python_samples_failure_state_rounding_and_retention",
    )?;
    mutation(
        host,
        "pub const HISTORY_CAP: usize = 450;",
        "pub const HISTORY_CAP: usize = 451;",
        "host",
        "actual_python_samples_failure_state_rounding_and_retention",
    )?;
    mutation(
        host,
        "else {\n                    return;\n                };",
        "else {\n                    continue;\n                };",
        "host",
        "sampler_cadence_cancellation_and_timeout_bound_work",
    )?;
    mutation(
        Path::new("bins/noevia-model-manager/src/lib.rs"),
        "if path != \"/api/v1/health\" && !state.token.is_empty() {",
        "if path != \"/api/v1/health\" && path != \"/api/v1/host\" && !state.token.is_empty() {",
        "policy",
        "native_host_auth_method_empty_and_history_use_only_rust_state",
    )?;
    Ok(())
}
