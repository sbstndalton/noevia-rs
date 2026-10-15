//! Fixed synthetic DNS mutations of the actual transport source.
#[path = "support/capped_capture.rs"]
mod capped_capture;
#[allow(dead_code)]
#[path = "support/mutant_classifier.rs"]
mod classifier;
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use std::{
    fs,
    path::Path,
    process::{Command, ExitCode},
    time::Duration,
};
fn read(p: &Path) -> Result<String, String> {
    String::from_utf8(capped_capture::read(p).map_err(|_| "read refused")?)
        .map_err(|_| "encoding refused".into())
}
fn copy(source: &Path, dest: &Path) -> Result<(), String> {
    fs::write(
        dest,
        capped_capture::read(source).map_err(|_| "read refused")?,
    )
    .map_err(|_| "write refused".into())
}
fn run() -> Result<(), String> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let original = read(&source.join("src/child_http.rs"))?;
    let controls = read(&source.join("tests/dns_policies.rs"))?;
    let mut manifest = read(&source.join("Cargo.toml"))?;
    for name in [
        "dav-parse",
        "storage-path",
        "s3-list-parse",
        "s3-sign",
        "js-json",
    ] {
        manifest = manifest.replace(
            &format!("path = \"../{name}\""),
            &format!("path = {:?}", source.parent().ok_or("parent")?.join(name)),
        );
    }
    let root = std::env::temp_dir().join(format!("m4-dns-mutants-{}", std::process::id()));
    fs::create_dir(&root).map_err(|_| "directory")?;
    for (name, needle, replacement, test, id) in [
        ("IDENTITY", "ServerName::try_from(identity_host.to_owned())", "ServerName::try_from(\"127.0.0.1\".to_owned())", "original_dns_identity_policy", "M4_DNS_IDENTITY"),
        ("RESULT33", "let Some(slot) = addresses.slots.get_mut(addresses.count) else {\n            return Err(TransportFailure::Dns);", "let Some(slot) = addresses.slots.get_mut(addresses.count) else {\n            return Ok(addresses);", "result33_policy", "M4_DNS_RESULT33"),
        ("ORDER", ".take(addresses.count).flatten()", ".take(addresses.count).flatten().rev()", "os_order_policy", "M4_DNS_OS_ORDER"),
        ("DEADLINE", "let deadline = Instant::from_std(deadline);", "let deadline = Instant::now() + Duration::from_millis(500);", "remaining_deadline_policy", "M4_DNS_DEADLINE"),
    ] {
        if original.matches(needle).count() != 1 {
            return Err("anchor".into());
        }
        let policy =
            classifier::Policy::bind_at(&controls, test, id, "tests/network_policies.rs", 4)?;
        let dir = root.join(name);
        fs::create_dir_all(dir.join("src/bin/support")).map_err(|_| "directory")?;
        fs::create_dir_all(dir.join("tests/support")).map_err(|_| "directory")?;
        fs::write(dir.join("Cargo.toml"), &manifest).map_err(|_| "manifest")?;
        copy(&source.join("Cargo.lock"), &dir.join("Cargo.lock"))?;
        for file in [
            "lib.rs",
            "admission.rs",
            "json.rs",
            "protocol.rs",
            "request_ipc.rs",
            "response_ipc.rs",
            "child_request.rs",
            "isolated.rs",
        ] {
            copy(&source.join("src").join(file), &dir.join("src").join(file))?;
        }
        fs::write(
            dir.join("src/child_http.rs"),
            original.replacen(needle, replacement, 1),
        )
        .map_err(|_| "mutation")?;
        copy(
            &source.join("src/bin/private-dns-fixture.rs"),
            &dir.join("src/bin/private-dns-fixture.rs"),
        )?;
        copy(
            &source.join("src/bin/support/tls_file.rs"),
            &dir.join("src/bin/support/tls_file.rs"),
        )?;
        copy(
            &source.join("tests/support/dns_lab.rs"),
            &dir.join("tests/support/dns_lab.rs"),
        )?;
        let fixture = read(&source.join("tests/support/tls_fixture.rs"))?.replace(
            "\"../../../../tools/qualification/process.rs\"",
            &format!(
                "{:?}",
                source
                    .parent()
                    .and_then(Path::parent)
                    .ok_or("parent")?
                    .join("tools/qualification/process.rs")
            ),
        );
        fs::write(dir.join("tests/support/tls_fixture.rs"), fixture).map_err(|_| "fixture")?;
        fs::write(dir.join("tests/network_policies.rs"), &controls).map_err(|_| "control")?;
        let mut command =
            Command::new(std::env::var_os("M4_CARGO").unwrap_or_else(|| "cargo".into()));
        command
            .env("RUST_BACKTRACE", "0")
            .env("RUST_LIB_BACKTRACE", "0")
            .args(["test", "--offline", "--locked", "--manifest-path"])
            .arg(dir.join("Cargo.toml"))
            .args(["--test", "network_policies", test, "--", "--exact"]);
        let result = process::bounded(command, vec![], Duration::from_secs(60))?;
        fs::write(dir.join("actual.stdout"), &result.stdout).map_err(|_| "capture")?;
        fs::write(dir.join("actual.stderr"), &result.stderr).map_err(|_| "capture")?;
        fs::write(
            dir.join("actual.status"),
            format!("code={:?}\n", result.status.code()),
        )
        .map_err(|_| "status")?;
        if !policy.accepts(result.status.code(), &result.stdout, &result.stderr) {
            return Err(format!("{name} uncontrolled"));
        }
        let stdout = std::str::from_utf8(&result.stdout).map_err(|_| "encoding")?;
        for (old, new) in [
            (id, "SETUP"),
            (id, "Deadline"),
            (id, "OTHER_POLICY"),
            (test, "wrong_test"),
            (policy.location.as_str(), "tests/network_policies.rs:0:0"),
        ] {
            if policy.accepts(
                Some(101),
                stdout.replace(old, new).as_bytes(),
                &result.stderr,
            ) {
                return Err("forgery accepted".into());
            }
        }
        for tail in [
            b"process failure\n".as_slice(),
            b"SETUP\n",
            b"error: test failed, to rerun pass `--test network_policies`\n",
        ] {
            let mut out = result.stdout.clone();
            out.extend_from_slice(tail);
            let mut err = result.stderr.clone();
            err.extend_from_slice(tail);
            if policy.accepts(Some(101), &out, &result.stderr)
                || policy.accepts(Some(101), &result.stdout, &err)
            {
                return Err("tail accepted".into());
            }
        }
        println!(
            "{name}: exact semantic mutation killed; forged identities and terminal tails refused"
        );
    }
    println!("four DNS captures retained {}", root.display());
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
