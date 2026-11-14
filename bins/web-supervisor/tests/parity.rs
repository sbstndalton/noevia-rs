//! Parity with build/web-supervisor.sh: the script's own suite (tests/parity/test-web-supervisor.sh,
//! copied with the script at noevia 11bf2da7) drives this binary through stub `node` and front
//! processes, then the bash reference, in the same environment. Both must pass every check.
//! Needs bash >= 4.3 and a real `node` (the stubs hand `node -e` probes to it); without node the
//! test says so and skips, it never passes silently on a partial run.

use std::path::Path;
use std::process::Command;

fn have(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn suite(supervisor: Option<&str>) -> (bool, String) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/parity/test-web-supervisor.sh");
    let mut cmd = Command::new("bash");
    cmd.arg(&script);
    match supervisor {
        Some(bin) => cmd.env("WEB_SUPERVISOR_BIN", bin),
        None => cmd.env_remove("WEB_SUPERVISOR_BIN"),
    };
    let out = match cmd.output() {
        Ok(out) => out,
        Err(e) => return (false, format!("cannot run bash: {e}")),
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn summary(text: &str) -> Option<(u32, u32)> {
    let line = text.lines().rev().find(|l| l.starts_with("passed "))?;
    let mut words = line.split_whitespace();
    let passed = words.nth(1)?.trim_end_matches(',').parse().ok()?;
    let failed = words.nth(1)?.parse().ok()?;
    Some((passed, failed))
}

#[test]
fn the_script_suite_passes_against_the_native_supervisor_and_the_reference() {
    if !have("node") || !have("bash") {
        eprintln!("skip - the parity suite needs bash and node on PATH");
        return;
    }
    let (native_ok, native) = suite(Some(env!("CARGO_BIN_EXE_noevia-web-supervisor")));
    let failures: Vec<&str> = native.lines().filter(|l| l.starts_with("FAIL")).collect();
    let native_counts = summary(&native);
    let (reference_ok, reference) = suite(None);
    let reference_counts = summary(&reference);
    assert!(
        reference_ok,
        "the bash reference fails its own suite here:\n{reference}"
    );
    assert!(
        native_ok,
        "native supervisor failures:\n{}\n\nfull output:\n{native}",
        failures.join("\n")
    );
    assert_eq!(
        native_counts, reference_counts,
        "both runs must make the same number of checks"
    );
    let (passed, failed) = native_counts.unwrap_or_default();
    assert_eq!(failed, 0);
    assert!(passed > 150, "the suite ran only {passed} checks");
}
