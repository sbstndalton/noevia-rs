//! Private source mutants, bounded Rust tooling; no network/real input.
#[path = "support/capped_capture.rs"]
mod capped_capture;
#[path = "support/mutant_classifier.rs"]
mod mutant_classifier;
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
/// Source inputs are read through the descriptor-capped reader, never unbounded (#1371).
fn text(path: impl AsRef<Path>) -> Result<String, String> {
    String::from_utf8(capped_capture::read(path.as_ref()).map_err(|_| "source read refused")?)
        .map_err(|_| "source encoding refused".into())
}
use std::{
    fs,
    path::Path,
    process::{Command, ExitCode},
    time::Duration,
};
fn run() -> Result<(), String> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cargo = std::env::var_os("M4_CARGO").unwrap_or_else(|| "cargo".into());
    let root = std::env::temp_dir().join(format!("m4-owned-mutants-{}", std::process::id()));
    fs::create_dir(&root).map_err(|_| "owned fixture directory unavailable")?;
    let original = text(source.join("src/isolated.rs")).map_err(|_| "source read failed")?;
    let controls =
        text(source.join("tests/owned_controls.rs")).map_err(|_| "controls read failed")?;
    let mut manifest = text(source.join("Cargo.toml")).map_err(|_| "manifest read failed")?;
    for name in [
        "dav-parse",
        "storage-path",
        "s3-list-parse",
        "s3-sign",
        "js-json",
    ] {
        let absolute = source.parent().ok_or("source parent missing")?.join(name);
        manifest = manifest.replace(
            &format!("path = \"../{name}\""),
            &format!("path = {:?}", absolute),
        );
    }
    let mutants = [
        (
            "READY_GATE",
            "if ready {",
            "if true {",
            "no_input_before_ready",
            "M4_POLICY_READY_GATE",
        ),
        (
            "CANCEL_IGNORED",
            "if cancelled.load(Ordering::Acquire) {",
            "if false && cancelled.load(Ordering::Acquire) {",
            "inflight_cancel_holds_admission_until_cleanup",
            "M4_POLICY_CANCEL",
        ),
        (
            "ADMISSION_EARLY_RELEASE",
            "permit: Some(permit),",
            "permit: { drop(permit); None },",
            "inflight_cancel_holds_admission_until_cleanup",
            "M4_POLICY_ADMISSION",
        ),
    ];
    for (name, needle, replacement, test, id) in mutants {
        let policy = mutant_classifier::Policy::bind(&controls, test, id)?;
        if original.matches(needle).count() != 1 {
            return Err("mutant source anchor changed".into());
        }
        let directory = root.join(name);
        fs::create_dir_all(directory.join("src/bin")).map_err(|_| "fixture directory failed")?;
        fs::create_dir(directory.join("tests")).map_err(|_| "fixture tests directory failed")?;
        fs::write(directory.join("Cargo.toml"), &manifest)
            .map_err(|_| "fixture manifest failed")?;
        fs::copy(source.join("Cargo.lock"), directory.join("Cargo.lock"))
            .map_err(|_| "fixture lock failed")?;
        for file in [
            "lib.rs",
            "admission.rs",
            "json.rs",
            "protocol.rs",
            "request_ipc.rs",
            "response_ipc.rs",
            "child_request.rs",
        ] {
            fs::copy(
                source.join("src").join(file),
                directory.join("src").join(file),
            )
            .map_err(|_| "fixture source failed")?;
        }
        fs::write(
            directory.join("src/isolated.rs"),
            original.replacen(needle, replacement, 1),
        )
        .map_err(|_| "mutant source failed")?;
        fs::copy(
            source.join("src/bin/private-child-proof.rs"),
            directory.join("src/bin/private-child-proof.rs"),
        )
        .map_err(|_| "fixture child failed")?;
        fs::copy(
            source.join("tests/owned_controls.rs"),
            directory.join("tests/owned_controls.rs"),
        )
        .map_err(|_| "fixture controls failed")?;
        let mut command = Command::new(&cargo);
        command
            .env("RUST_BACKTRACE", "0")
            .env("RUST_LIB_BACKTRACE", "0");
        command
            .args(["test", "--offline", "--locked", "--manifest-path"])
            .arg(directory.join("Cargo.toml"))
            .args(["--test", "owned_controls", test, "--", "--exact"]);
        let result = process::bounded(command, Vec::new(), Duration::from_secs(60))?;
        fs::write(directory.join("actual.stdout"), &result.stdout)
            .map_err(|_| "evidence write failed")?;
        fs::write(directory.join("actual.stderr"), &result.stderr)
            .map_err(|_| "evidence write failed")?;
        if !policy.accepts(result.status.code(), &result.stdout, &result.stderr) {
            return Err(format!("{name} did not produce a controlled test failure"));
        }
        let actual =
            std::str::from_utf8(&result.stdout).map_err(|_| "failure output encoding changed")?;
        for (old, new) in [
            (id, "M4_FORGED_SETUP"),
            (id, "M4_FORGED_DEADLINE"),
            (id, "M4_FORGED_CAP"),
            (id, "M4_OTHER_POLICY"),
            (policy.location.as_str(), "tests/owned_controls.rs:0:0"),
            (test, "wrong_selected_test"),
        ] {
            if policy.accepts(
                Some(101),
                actual.replace(old, new).as_bytes(),
                &result.stderr,
            ) {
                return Err("actual-format negative classifier control accepted".into());
            }
        }
        if policy.accepts(Some(1), &result.stdout, &result.stderr)
            || policy.accepts(None, &result.stdout, &result.stderr)
            || policy.accepts(
                Some(101),
                &result.stdout,
                b"error[E0000]: could not compile",
            )
            || policy.accepts(
                Some(101),
                format!("{actual}thread 'extra' (1) panicked at extra:1:1:\nextra\n").as_bytes(),
                &result.stderr,
            )
        {
            return Err("actual-format process/compiler/extra panic control accepted".into());
        }
        println!("{name}: killed by {test}");
    }
    println!(
        "three actual source mutants killed; isolated fixture sources retained at {}",
        root.display()
    );
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
