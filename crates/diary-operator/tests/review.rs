#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use diary_backup_format::sha256;
use std::{fs, path::Path, process::Command, time::Instant};
#[test]
fn config_diagnostic_has_no_values() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.yaml");
    let secret = "synthetic-credential-canary-731";
    fs::write(&file,format!("llm:\n  api_key: {secret}\n  chat_model: {secret}\ncorpus:\n  webdav:\n    password: {secret}\nunknown:\n  nested: [{secret}]\n")).unwrap();
    for body in [None, Some(format!("llm: !invalid {secret}\n"))] {
        if let Some(body) = body {
            fs::write(&file, body).unwrap();
        }
        let out = Command::new(env!("CARGO_BIN_EXE_diary-config"))
            .env_clear()
            .env("S3_SECRET_ACCESS_KEY", secret)
            .env("S3_SESSION_TOKEN", secret)
            .env("DIARY_AUTH_TOKEN", secret)
            .arg(&file)
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains(secret),
            "diagnostic stdout exposed credential"
        );
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains(secret),
            "diagnostic stderr exposed credential"
        );
    }
}
#[test]
fn config_diagnostic_does_not_echo_invalid_unicode() {
    use std::os::unix::ffi::OsStringExt;
    let secret = "synthetic-invalid-unicode-canary-925";
    let value = std::ffi::OsString::from_vec([secret.as_bytes(), &[0xff]].concat());
    let out = Command::new(env!("CARGO_BIN_EXE_diary-config"))
        .env_clear()
        .env("LLM_API_KEY", value)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(secret));
    assert!(out.stdout.is_empty());
}

#[test]
fn restore_large_namespace_is_bounded() {
    // noevia#2497: the bound is on the restore's own work (no file x directory cross-product),
    // not on the disk. Every restored file is fsynced, and 5000 fsyncs alone take 25 s on a
    // slow disk and more than 5 s on a busy shared runner, so the namespace lives on tmpfs when
    // there is one (fsync is nearly free there).
    // About 21 MiB there (a 4 KiB page per file); a /dev/shm that is missing, not writable or
    // without room for it falls back to the ordinary temp directory (noevia#2684 item 2: a nearly
    // full /dev/shm failed the restore with ENOSPC, which read like the old timing flake).
    let shm = Path::new("/dev/shm");
    let dir = if free_bytes(shm).is_some_and(|free| free >= 64 * 1024 * 1024) {
        tempfile::tempdir_in(shm).or_else(|_| tempfile::tempdir())
    } else {
        tempfile::tempdir()
    }
    .unwrap();
    let tenant = "00000000-0000-4000-8000-000000000123";
    let object = format!("noevia-backups/{tenant}/objects/synthetic/payload");
    let source = dir.path().join(&object);
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(source, b"x").unwrap();
    let files=(0..5000).map(|i|serde_json::json!({"path":format!("files/f{i:05}"),"object":object,"sha256":sha256(b"x")})).collect::<Vec<_>>();
    let dirs = (0..5000)
        .map(|i| format!("dirs/d{i:05}"))
        .collect::<Vec<_>>();
    let body=serde_json::to_vec(&serde_json::json!({"format":"noevia-diary-backup-v1","tenant":tenant,"files":files,"directories":dirs})).unwrap();
    let manifest = dir.path().join(format!("{}.json", sha256(&body)));
    fs::write(&manifest, body).unwrap();
    let destination = dir.path().join("new");
    let now = Instant::now();
    let out = Command::new("/usr/bin/timeout")
        .args(["-k", "1", "5"])
        .arg(env!("CARGO_BIN_EXE_diary-backup-restore"))
        .arg(dir.path())
        .arg(manifest)
        .arg(&destination)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "restore exceeded five seconds or failed: {:?} in {}: {}",
        out.status.code(),
        dir.path().display(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(now.elapsed().as_secs() < 6);
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["files"], 5000);
    assert_eq!(result["bytes"], 5000);
    assert_eq!(
        fs::read_dir(destination.join("files")).unwrap().count(),
        5000
    );
    assert_eq!(
        fs::read_dir(destination.join("dirs")).unwrap().count(),
        5000
    );
}

/// Free bytes for an unprivileged writer on the file system holding `path` (`df`'s available
/// column); `None` when it cannot be read.
fn free_bytes(path: &Path) -> Option<u64> {
    let out = Command::new("df")
        .args(["--output=avail", "-B1"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)?
        .trim()
        .parse()
        .ok()
}

#[test]
fn free_bytes_reads_the_temp_directory_and_refuses_a_missing_path() {
    assert!(free_bytes(&std::env::temp_dir()).is_some_and(|free| free > 0));
    assert_eq!(free_bytes(Path::new("/nonexistent-qa-path-2684")), None);
}
