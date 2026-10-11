//! Private source mutants, bounded Rust tooling; no network/real input.
#[path = "support/mutant_classifier.rs"]
#[allow(dead_code)]
mod mutant_classifier;
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
    let root = std::env::temp_dir().join(format!("m4-network-mutants-{}", std::process::id()));
    fs::create_dir(&root).map_err(|_| "owned fixture directory unavailable")?;
    let original =
        fs::read_to_string(source.join("src/isolated.rs")).map_err(|_| "source read failed")?;
    let metadata = fs::read_to_string(source.join("src/response_ipc.rs"))
        .map_err(|_| "metadata source failed")?;
    let controls = fs::read_to_string(source.join("tests/network_policies.rs"))
        .map_err(|_| "controls read failed")?;
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
            "TAG",
            "Self::Http { .. } => NET,",
            "Self::Http { .. } => BODY,",
            "tag_policy",
            "M4_NET_TAG",
        ),
        (
            "METADATA",
            "let size = metadata(bytes, cap)?;",
            "let size = metadata(bytes, cap)?; if bytes.len() >= METADATA { return Ok(size); }",
            "metadata_policy",
            "M4_NET_METADATA",
        ),
        (
            "EOF",
            "if owned.exited()? && out_done && err_done {",
            "if out.len() == envelope.maximum() + 20 {",
            "eof_policy",
            "M4_NET_EOF",
        ),
        (
            "EXIT",
            "Ok(bytes) if success => Ok(bytes),",
            "Ok(bytes) => { let _ = success; Ok(bytes) },",
            "exit_policy",
            "M4_NET_EXIT",
        ),
        (
            "CANCEL_IGNORED",
            "if cancelled.load(Ordering::Acquire) {",
            "if false && cancelled.load(Ordering::Acquire) {",
            "cancel_policy",
            "M4_NET_CANCEL",
        ),
        (
            "ADMISSION_EARLY_RELEASE",
            "permit: Some(permit),",
            "permit: { drop(permit); None },",
            "admission_policy",
            "M4_NET_ADMISSION",
        ),
    ];
    for (name, needle, replacement, test, id) in mutants {
        let policy = mutant_classifier::Policy::bind_at(
            &controls,
            test,
            id,
            "tests/network_policies.rs",
            5,
        )?;
        let selected = if name == "METADATA" {
            &metadata
        } else {
            &original
        };
        if selected.matches(needle).count() != 1 {
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
            if name == "METADATA" {
                original.clone()
            } else {
                original.replacen(needle, replacement, 1)
            },
        )
        .map_err(|_| "mutant source failed")?;
        fs::copy(
            source.join("src/bin/private-network-fixture.rs"),
            directory.join("src/bin/private-network-fixture.rs"),
        )
        .map_err(|_| "fixture child failed")?;
        fs::copy(
            source.join("tests/network_policies.rs"),
            directory.join("tests/network_policies.rs"),
        )
        .map_err(|_| "fixture controls failed")?;
        if name == "METADATA" {
            fs::write(
                directory.join("src/response_ipc.rs"),
                metadata.replacen(needle, replacement, 1),
            )
            .map_err(|_| "metadata mutation failed")?;
        }
        let mut command = Command::new(&cargo);
        command
            .env("RUST_BACKTRACE", "0")
            .env("RUST_LIB_BACKTRACE", "0");
        command
            .args(["test", "--offline", "--locked", "--manifest-path"])
            .arg(directory.join("Cargo.toml"))
            .args(["--test", "network_policies", test, "--", "--exact"]);
        let result = process::bounded(command, Vec::new(), Duration::from_secs(60))?;
        fs::write(directory.join("actual.stdout"), &result.stdout)
            .map_err(|_| "evidence write failed")?;
        fs::write(directory.join("actual.stderr"), &result.stderr)
            .map_err(|_| "evidence write failed")?;
        fs::write(
            directory.join("actual.status"),
            format!("code={:?}\n", result.status.code()),
        )
        .map_err(|_| "status evidence failed")?;
        if !policy.accepts(result.status.code(), &result.stdout, &result.stderr) {
            return Err(format!("{name} did not produce a controlled test failure"));
        }
        let actual =
            std::str::from_utf8(&result.stdout).map_err(|_| "failure output encoding changed")?;
        let footer = b"error: test failed, to rerun pass `--test network_policies`\n";
        for tail in [
            b"SETUP failed\n".as_slice(),
            b"process deadline\n",
            b"unrelated output\n",
            footer,
        ] {
            let mut out = result.stdout.clone();
            out.extend_from_slice(tail);
            let mut err = result.stderr.clone();
            err.extend_from_slice(tail);
            if policy.accepts(Some(101), &out, &result.stderr)
                || policy.accepts(Some(101), &result.stdout, &err)
            {
                return Err("terminal capture contamination accepted".into());
            }
        }
        let mut misplaced = footer.to_vec();
        misplaced.extend_from_slice(&result.stderr);
        if policy.accepts(Some(101), &result.stdout, &misplaced) {
            return Err("misplaced footer accepted".into());
        }
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
        "six actual source mutants killed; isolated fixture sources retained at {}",
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
