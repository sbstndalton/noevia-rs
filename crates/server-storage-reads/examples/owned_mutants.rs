//! Private source mutants, bounded Rust tooling; no network/real input.
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
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
    let original =
        fs::read_to_string(source.join("src/isolated.rs")).map_err(|_| "source read failed")?;
    let mut manifest =
        fs::read_to_string(source.join("Cargo.toml")).map_err(|_| "manifest read failed")?;
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
        ),
        (
            "CANCEL_IGNORED",
            "if cancelled.load(Ordering::Acquire) {",
            "if false && cancelled.load(Ordering::Acquire) {",
            "inflight_cancel_holds_admission_until_cleanup",
        ),
        (
            "ADMISSION_EARLY_RELEASE",
            "permit: Some(permit),",
            "permit: { drop(permit); None },",
            "inflight_cancel_holds_admission_until_cleanup",
        ),
    ];
    for (name, needle, replacement, test) in mutants {
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
            .args(["test", "--offline", "--locked", "--manifest-path"])
            .arg(directory.join("Cargo.toml"))
            .args(["--test", "owned_controls", test, "--", "--exact"]);
        let result = process::bounded(command, Vec::new(), Duration::from_secs(60))?;
        if result.status.success()
            || !result
                .stdout
                .windows(b"test result: FAILED".len())
                .any(|w| w == b"test result: FAILED")
        {
            return Err(format!("{name} did not produce a controlled test failure"));
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
