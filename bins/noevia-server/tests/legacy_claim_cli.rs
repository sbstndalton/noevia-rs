#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use std::fs;
use std::process::{Command, Output};
use tempfile::tempdir;

const USER: &str = "11111111-2222-4333-8444-555555555555";
fn helper(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_noevia-server"))
        .args(args)
        .env_remove("UI_DATA_DIR")
        .env_remove("UI_PORT")
        .env_remove("NOEVIA_LEGACY_UPSTREAM")
        .output()
        .unwrap()
}

#[test]
fn claim_subcommand_runs_without_loading_server_config_or_auth_database() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("projects.json"), r#"{"projects":[]}"#).unwrap();
    let root_arg = root.path().as_os_str();
    let output = helper(&[
        std::ffi::OsStr::new("--claim-legacy-root"),
        root_arg,
        std::ffi::OsStr::new(USER),
    ]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"CLAIMED\n");
    assert!(root.path().join("users").join(USER).join("migration.json").is_file());
    assert!(!root.path().join("cowork.db").exists(), "filesystem helper must not open auth DB");
}

#[test]
fn empty_root_skips_without_creating_the_coordination_sidecar() {
    let root = tempdir().unwrap();
    let output = helper(&[
        std::ffi::OsStr::new("--claim-legacy-root"),
        root.path().as_os_str(),
        std::ffi::OsStr::new(USER),
    ]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"SKIPPED\n");
    assert!(!root.path().join("legacy-owner.json.lock").exists());
}

#[test]
fn claim_subcommand_rejects_missing_extra_and_invalid_arguments() {
    for args in [
        vec![std::ffi::OsStr::new("--claim-legacy-root")],
        vec![
            std::ffi::OsStr::new("--claim-legacy-root"),
            std::ffi::OsStr::new("relative-root"),
            std::ffi::OsStr::new(USER),
        ],
        vec![
            std::ffi::OsStr::new("--claim-legacy-root"),
            std::ffi::OsStr::new("/tmp"),
        ],
        vec![
            std::ffi::OsStr::new("--claim-legacy-root"),
            std::ffi::OsStr::new("/tmp"),
            std::ffi::OsStr::new("not-a-uuid"),
        ],
        vec![
            std::ffi::OsStr::new("--claim-legacy-root"),
            std::ffi::OsStr::new("/tmp"),
            std::ffi::OsStr::new(USER),
            std::ffi::OsStr::new("extra"),
        ],
    ] {
        let output = helper(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("usage:"));
    }
}
