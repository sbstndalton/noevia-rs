#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use chatgpt_status::{Root, StatusKeys};
use std::os::unix::fs::symlink;
fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("secrets.key"), [7; 32]).unwrap();
    d
}
fn open(d: &tempfile::TempDir) -> Root {
    Root::open(&d.path().canonicalize().unwrap()).unwrap()
}
#[test]
fn key_sizes_and_missing() {
    for n in [0, 31, 33, 1024 * 1024] {
        let d = fixture();
        std::fs::write(d.path().join("secrets.key"), vec![7; n]).unwrap();
        assert!(StatusKeys::load(&open(&d), None).is_err());
    }
    let d = tempfile::tempdir().unwrap();
    assert!(StatusKeys::load(&open(&d), None).is_err());
    assert!(!d.path().join("secrets.key").exists());
}
#[test]
fn nofollow_key() {
    let d = fixture();
    std::fs::rename(d.path().join("secrets.key"), d.path().join("real")).unwrap();
    symlink("real", d.path().join("secrets.key")).unwrap();
    assert!(
        StatusKeys::load(&open(&d), None).is_err(),
        "ROUTING_ASSERT/status/nofollow_key"
    );
}
#[test]
fn nofollow_root_components() {
    let d = fixture();
    let holder = tempfile::tempdir().unwrap();
    let base = holder.path().canonicalize().unwrap();
    symlink(d.path().canonicalize().unwrap(), base.join("link")).unwrap();
    assert!(Root::open(&base.join("link")).is_err());
    assert!(Root::open(&base.join("link/child")).is_err());
    assert!(Root::open(std::path::Path::new("relative")).is_err());
}
#[test]
fn previous_precedence() {
    let d = fixture();
    std::fs::write(d.path().join("secrets.key.previous"), [1; 31]).unwrap();
    assert!(StatusKeys::load(&open(&d), Some(&"07".repeat(32))).is_err());
    std::fs::remove_file(d.path().join("secrets.key.previous")).unwrap();
    assert!(StatusKeys::load(&open(&d), Some(&format!(" {} ", "07".repeat(32)))).is_ok());
    assert!(StatusKeys::load(
        &open(&d),
        Some(&secret_envelope::encode_base64url(&[1; 32]))
    )
    .is_ok());
    assert!(StatusKeys::load(&open(&d), Some(&"x".repeat(4097))).is_err());
    assert!(StatusKeys::load(&open(&d), Some("bad")).is_err());
}
#[test]
fn special_files_refused() {
    let d = fixture();
    std::fs::remove_file(d.path().join("secrets.key")).unwrap();
    std::fs::create_dir(d.path().join("secrets.key")).unwrap();
    assert!(StatusKeys::load(&open(&d), None).is_err());
    std::fs::remove_dir(d.path().join("secrets.key")).unwrap();
    let mut command = std::process::Command::new("/usr/bin/mkfifo");
    command.arg(d.path().join("secrets.key"));
    assert!(
        process::bounded(command, Vec::new(), std::time::Duration::from_secs(5))
            .unwrap()
            .status
            .success()
    );
    assert!(StatusKeys::load(&open(&d), None).is_err());
}
