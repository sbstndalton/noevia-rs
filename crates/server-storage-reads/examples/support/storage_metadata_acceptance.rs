//! Closed, digest-bound receipts. Supplied roots/ancestry and approval digest are trusted.
use super::capture;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs::{File, OpenOptions}, io::{Read, Write}, os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt}, path::{Component, Path}, process::Command};
pub const MANIFEST: usize = 4 * 1024 * 1024;
const FILE: usize = 128 * 1024 * 1024;
const FILE_COUNT: usize = 4096;
const FILE_TOTAL: u64 = 512 * 1024 * 1024;
pub const RS: &str = "a58e8fb4bfefa2a0cf5c3f6ccdfe385ac8dde20f";
pub const CORE: &str = "7960dd1d41c028e353b1176ee1e2da77c7c50c94";
pub const SUPERVISOR: &str = "039e485089b7122d5bc8b6d9da6e088567f39277";
fn hash(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn require(ok: bool, error: &str) -> Result<(), String> { if ok { Ok(()) } else { Err(error.into()) } }
pub fn keys(value: &Value, expected: &[&str]) -> Result<(), String> {
    let object = value.as_object().ok_or("object required")?;
    require(object.len() == expected.len() && expected.iter().all(|k| object.contains_key(*k)), "closed keys")
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> { value.get(key).and_then(Value::as_str).ok_or(format!("text {key}")) }
fn hex(value: &str) -> bool { value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }
pub fn absolute(value: &str) -> Result<&Path, String> {
    let path = Path::new(value);
    require(path.is_absolute() && value.len() <= 4096 && !value.bytes().any(|b| b < 32 || b == 127)
        && path.components().all(|c| !matches!(c, Component::ParentDir | Component::CurDir)), "absolute bounded path")?;
    Ok(path)
}
fn open(path: &Path) -> Result<File, String> {
    let flags = (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC).bits() as i32;
    OpenOptions::new().read(true).custom_flags(flags).open(path).map_err(|e| format!("receipt open: {e}"))
}
fn read(path: &Path, cap: usize) -> Result<Vec<u8>, String> {
    let mut file = open(path)?; let before = file.metadata().map_err(|e| e.to_string())?;
    require(before.is_file() && before.nlink() == 1 && before.uid() == rustix::process::geteuid().as_raw()
        && before.len() <= cap as u64 && before.mode() & 0o7000 == 0, "receipt file safety")?;
    let mut out = Vec::new(); let mut buffer = [0; 4096];
    loop { let n = file.read(&mut buffer).map_err(|e| e.to_string())?; if n == 0 { break; }
        require(out.len().checked_add(n).is_some_and(|v| v <= cap), "receipt cap")?;
        out.try_reserve(n).map_err(|_| "receipt allocation")?; out.extend_from_slice(buffer.get(..n).ok_or("read count")?);
    }
    let after = file.metadata().map_err(|e| e.to_string())?;
    require(before.dev() == after.dev() && before.ino() == after.ino() && before.len() == after.len()
        && before.mtime() == after.mtime() && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime() && before.ctime_nsec() == after.ctime_nsec() && out.len() as u64 == after.len(), "receipt changed")?;
    Ok(out)
}
pub fn canonical(bytes: &[u8]) -> Result<Value, String> {
    require(bytes.len() <= MANIFEST, "manifest cap")?;
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "manifest JSON")?;
    // Canonical compact serde JSON is the receipt format. Duplicate keys, alternate
    // spellings and whitespace cannot be silently normalized into an accepted receipt.
    require(serde_json::to_vec(&value).map_err(|_| "manifest encoding")? == bytes, "noncanonical receipt")?;
    Ok(value)
}
pub struct Artifact { pub path: String, pub bytes: u64, pub digest: String, pub mode: u32 }
pub struct Inputs { pub default: Artifact, pub storage: Artifact }
pub fn ordinary_profile(cargo: &Value, storage: bool) -> Result<(), String> {
    require(cargo.get("reason") == Some(&json!("compiler-artifact"))
        && cargo.pointer("/target/name") == Some(&json!("noevia-server"))
        && cargo.pointer("/target/kind") == Some(&json!(["bin"]))
        && cargo.pointer("/profile/test") == Some(&json!(false))
        && cargo.get("fresh").and_then(Value::as_bool).is_some(), "ordinary source profile")?;
    let features = cargo.get("features").and_then(Value::as_array).ok_or("artifact features")?;
    let names: BTreeSet<_> = features.iter().map(|v| v.as_str().ok_or("feature text")).collect::<Result<_, _>>()?;
    require(names.len() == features.len() && names == if storage { BTreeSet::from(["default", "rust-storage-reads"]) } else { BTreeSet::from(["default"]) }, "exact artifact features")
}
fn artifact(value: &Value, source_paths: &BTreeSet<String>, storage: bool) -> Result<Artifact, String> {
    keys(value, &["path", "sha256", "bytes", "mode", "cargo"])?;
    let path = text(value, "path")?; absolute(path)?;
    let digest = text(value, "sha256")?; require(hex(digest), "artifact digest")?;
    let bytes = value.get("bytes").and_then(Value::as_u64).ok_or("artifact bytes")?;
    let mode = value.get("mode").and_then(Value::as_u64).ok_or("artifact mode")?;
    require(bytes > 0 && bytes <= FILE as u64 && matches!(mode, 0o700 | 0o755), "artifact executable bounds")?;
    let cargo = value.get("cargo").ok_or("cargo artifact")?;
    ordinary_profile(cargo, storage)?;
    require(cargo.get("reason") == Some(&json!("compiler-artifact")) && cargo.get("executable") == Some(&json!(path))
        && cargo.pointer("/target/name") == Some(&json!("noevia-server"))
        && cargo.pointer("/target/kind") == Some(&json!(["bin"]))
        && cargo.pointer("/profile/test") == Some(&json!(false))
        && cargo.get("fresh").and_then(Value::as_bool).is_some(), "actual ordinary artifact profile")?;
    let src = cargo.pointer("/target/src_path").and_then(Value::as_str).ok_or("cargo source")?;
    require(source_paths.contains(src) && src.ends_with("/bins/noevia-server/src/main.rs"), "mapped artifact source")?;
    let package = cargo.get("package_id").and_then(Value::as_str).ok_or("cargo package")?;
    require(package.ends_with("#noevia-server@0.1.0") && package.starts_with("path+file://"), "artifact package")?;
    let raw = read(absolute(path)?, FILE)?;
    let stat = open(absolute(path)?)?.metadata().map_err(|e| e.to_string())?;
    require(raw.len() as u64 == bytes && hash(&raw) == digest && u64::from(stat.mode() & 0o7777) == mode, "artifact bytes/mode")?;
    Ok(Artifact { path: path.into(), bytes, digest: digest.into(), mode: mode as u32 })
}
pub fn admit(value: &Value) -> Result<Inputs, String> {
    keys(value, &["schema", "job", "rs", "core", "supervisor", "sources", "artifacts"])?;
    require(value.get("schema") == Some(&json!(1)) && text(value, "job")? == "storage-metadata-admission"
        && text(value, "rs")? == RS && text(value, "core")? == CORE && text(value, "supervisor")? == SUPERVISOR, "source/job identities")?;
    let sources = value.get("sources").and_then(Value::as_array).ok_or("sources")?;
    require(!sources.is_empty() && sources.len() <= FILE_COUNT, "source count")?;
    let mut paths = BTreeSet::new(); let mut total = 0u64;
    for source in sources {
        keys(source, &["path", "bytes", "sha256", "mode"])?;
        let path = text(source, "path")?; absolute(path)?;
        require(paths.insert(path.to_owned()), "duplicate source")?;
        let bytes = source.get("bytes").and_then(Value::as_u64).ok_or("source bytes")?;
        total = total.checked_add(bytes).filter(|n| *n <= FILE_TOTAL).ok_or("source aggregate cap")?;
        let mode = source.get("mode").and_then(Value::as_u64).ok_or("source mode")?;
        require(matches!(mode, 0o644 | 0o755 | 0o600 | 0o700), "source mode")?;
        let digest = text(source, "sha256")?; require(hex(digest), "source digest")?;
        let raw = read(absolute(path)?, FILE)?; let stat = open(absolute(path)?)?.metadata().map_err(|e| e.to_string())?;
        require(raw.len() as u64 == bytes && hash(&raw) == digest && u64::from(stat.mode() & 0o7777) == mode, "source bytes/mode")?;
    }
    for suffix in ["/Cargo.lock", "/rust-toolchain.toml", "/contracts/http/routes.toml", "/bins/noevia-server/src/main.rs", "/server/index.cjs", "/server/storage-owner-entry.test.cjs", "/build/web-supervisor.sh"] {
        require(paths.iter().any(|p| p.ends_with(suffix)), "required source binding")?;
    }
    let artifacts = value.get("artifacts").ok_or("artifacts")?; keys(artifacts, &["default", "storage"])?;
    let default = artifact(artifacts.get("default").ok_or("default")?, &paths, false)?;
    let storage = artifact(artifacts.get("storage").ok_or("storage")?, &paths, true)?;
    require(default.path != storage.path && default.digest != storage.digest, "disjoint artifacts")?;
    Ok(Inputs { default, storage })
}
pub fn accounting(probes: usize, serving: usize, events: &[&str]) -> Result<(), String> {
    require(events.len() <= 32 && events.iter().all(|e| matches!(*e, "probe" | "serving"))
        && events.iter().filter(|e| **e == "probe").count() == probes
        && events.iter().filter(|e| **e == "serving").count() == serving, "child count receipt")
}
fn save(root: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    require(bytes.len() <= capture::OUTPUT, "evidence cap")?;
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(root.join(name)).map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())
}
pub fn supported(stage: &str) -> Result<(), String> { require(matches!(stage, "validate-inputs" | "capabilities"), "stage unimplemented; public/entry/mutants/factory held") }
pub fn run(args: Vec<String>) -> Result<(), String> {
    require(args.len() == 4 && args.iter().all(|a| a.len() <= 4096), "usage: stage absoluteManifest approvedSHA256 newPrivateOutput")?;
    let stage = args.first().ok_or("stage")?; supported(stage)?;
    let manifest = args.get(1).ok_or("manifest")?; let digest = args.get(2).ok_or("approval digest")?; let output = absolute(args.get(3).ok_or("output")?)?;
    require(hex(digest), "approval digest format")?;
    let raw = read(absolute(manifest)?, MANIFEST)?; require(hash(&raw) == *digest, "approved manifest bytes")?;
    let inputs = admit(&canonical(&raw)?)?;
    std::fs::DirBuilder::new().mode(0o700).create(output).map_err(|e| e.to_string())?;
    save(output, "input.json", &raw)?;
    save(output, "scope.txt", b"receipt/capability evidence only; public/entry/mutants/factory unimplemented; no ancestry/RSS/kernel certificate\n")?;
    if stage == "capabilities" {
        for (name, artifact, on) in [("default", inputs.default, false), ("storage", inputs.storage, true)] {
            // Pre-spawn receipt, no inherited owner switches or credentials. Execute
            // only the digest-bound ordinary front's genuine --features mode.
            let receipt = json!({"argv":[artifact.path,"--features"],"cwd":output,"env":{"LC_ALL":"C"},"bytes":artifact.bytes,"sha256":artifact.digest,"mode":artifact.mode});
            save(output, &format!("{name}-argv.json"), &serde_json::to_vec(&receipt).map_err(|_| "receipt encoding")?)?;
            for tail in ["stdout", "stderr", "status.json"] { save(output, &format!("{name}-{tail}"), b"")?; }
            let mut command = Command::new(&artifact.path); command.arg("--features").env_clear().env("LC_ALL", "C").current_dir(output);
            let evidence = capture::bounded(command, b"");
            // These precreated files are private evidence, never application inputs.
            std::fs::write(output.join(format!("{name}-stdout")), &evidence.stdout).map_err(|e| e.to_string())?;
            std::fs::write(output.join(format!("{name}-stderr")), &evidence.stderr).map_err(|e| e.to_string())?;
            let status = json!({"rawStatus":evidence.raw_status,"code":evidence.code,"signal":evidence.signal,"groupSignal":evidence.group_signal,"failure":evidence.failure,"cleanup":evidence.cleanup,"stdoutEOF":evidence.stdout_eof,"stderrEOF":evidence.stderr_eof,"probeChildCount":usize::from(evidence.spawned),"servingChildCount":0});
            std::fs::write(output.join(format!("{name}-status.json")), serde_json::to_vec(&status).map_err(|_| "status encoding")?).map_err(|e| e.to_string())?;
            require(evidence.completed() && evidence.code == Some(0) && evidence.stderr.is_empty(), "capability capture incomplete")?;
            let text = std::str::from_utf8(&evidence.stdout).map_err(|_| "capability UTF8")?;
            require(text.ends_with('\n') && text.lines().any(|s| s == "rust-auth") && text.lines().any(|s| s == "rust-storage-reads") == on, "capability advertisement")?;
            accounting(1, 0, &["probe"])?;
        }
    }
    save(output, "completion.json", b"{\"admissionComplete\":true,\"publicQualified\":false,\"entryQualified\":false,\"mutantsQualified\":false,\"factoryQualified\":false}")
}
