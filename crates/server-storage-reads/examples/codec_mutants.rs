//! Four fixed codec mutations of the actual frozen production source.
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
    let codec_original = read(&source.join("src/response_codecs.rs"))?;
    let controls = read(&source.join("tests/codec_policies.rs"))?;
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
    let root = std::env::temp_dir().join(format!("m4-codec-mutants-{}", std::process::id()));
    fs::create_dir(&root).map_err(|_| "directory")?;
    let snapshot = root.join("source");
    fs::create_dir(&snapshot).map_err(|_| "snapshot")?;
    for file in [
        "Cargo.toml",
        "Cargo.lock",
        "CODECS_PRIVATE.md",
        "src/child_http.rs",
        "src/response_codecs.rs",
        "tests/support/codec_lab.rs",
        "tests/codec_controls.rs",
        "tests/codec_node22.rs",
        "tests/codec_policies.rs",
        "examples/codec_mutants.rs",
        "examples/support/capped_capture.rs",
        "examples/support/mutant_classifier.rs",
    ] {
        let destination = snapshot.join(file);
        fs::create_dir_all(destination.parent().ok_or("parent")?)
            .map_err(|_| "snapshot directory")?;
        copy(&source.join(file), &destination)?;
    }
    copy(
        &source
            .parent()
            .and_then(Path::parent)
            .ok_or("parent")?
            .join("tools/qualification/process.rs"),
        &snapshot.join("process.rs"),
    )?;

    for (name, file, needle, replacement, test, id) in [
        ("ORDER", "response_codecs.rs", "kinds.get_mut(..count).ok_or(Fault::Body)?.reverse();", "let _ = kinds.get_mut(..count).ok_or(Fault::Body)?;", "reverse_stack_policy", "M4_CODEC_ORDER"),
        ("DEFLATE", "response_codecs.rs", "let zlib_header = (*first & 15) == 8;", "let zlib_header = { let _ = first; true };", "raw_deflate_policy", "M4_CODEC_DEFLATE"),
        ("OVERFLOW_REFUSAL", "child_http.rs", "if codecs.compressed() {", "if false && codecs.compressed() {", "decoded_capped_policy", "M4_CODEC_OVERFLOW_REFUSAL"),
        ("UNKNOWN", "response_codecs.rs", "return Ok(Self {\n                kinds: [None; 5],\n                count: 0,\n            });", "return Ok(Self { kinds, count });", "whole_unknown_policy", "M4_CODEC_UNKNOWN"),
    ] {
        let selected = if file == "child_http.rs" { &original } else { &codec_original };
        if selected.matches(needle).count() != 1 { return Err("anchor".into()); }
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
        fs::write(dir.join("src/child_http.rs"), if file == "child_http.rs" { original.replacen(needle,replacement,1) } else { original.clone() }).map_err(|_| "mutation")?;
        fs::write(dir.join("src/response_codecs.rs"), if file == "response_codecs.rs" { codec_original.replacen(needle,replacement,1) } else { codec_original.clone() }).map_err(|_| "mutation")?;
        copy(&source.join("src/bin/private-http-fixture.rs"), &dir.join("src/bin/private-http-fixture.rs"))?;
        copy(&source.join("tests/support/codec_lab.rs"), &dir.join("tests/support/codec_lab.rs"))?;
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
        if policy.accepts(Some(0), &result.stdout, &result.stderr) || policy.accepts(None, &result.stdout, &result.stderr) { return Err("status forgery accepted".into()); }
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
    println!("four codec captures retained {}", root.display());
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
