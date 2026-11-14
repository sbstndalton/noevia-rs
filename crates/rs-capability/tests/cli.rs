//! The CLI contract the assembler's shell adapter (deploy/tools/lib/rs-capability.sh) relies on, after
//! deploy/tools/test-rs-capability.sh: one verdict line, exit 0, invalid (never absence) on read failures.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn run(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_rs-capability")).args(args).env_remove("RS_CAPABILITY_TOP").output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

fn work() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rs-capability-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, body: &str) -> String {
    let p = dir.join("source.rs");
    std::fs::write(&p, format!("{body}\n")).unwrap();
    p.to_string_lossy().into_owned()
}

#[test]
fn cli_matches_the_shell_adapter_suite() {
    let dir = work();
    let fixtures = format!("{}/fixtures", env!("CARGO_MANIFEST_DIR"));
    let build = format!("{fixtures}/production.Dockerfile");
    let cargo = format!("{fixtures}/production-Cargo.toml");
    let src = write(&dir, "mod unrelated { #![cfg(test)] fn helper() {} }\npub const FEATURES: &[&str] = &[\"cap\"];");
    assert_eq!(run(&[&src, "FEATURES", "cap"]), "v1\n");
    assert_eq!(run(&[&src, "FEATURES", "absent"]), "legacy\n");
    let src = write(&dir, "const ALIAS: &str = \"cap\"; pub const FEATURES: &[&str] = &[ALIAS];");
    assert_eq!(run(&[&src, "FEATURES", "cap"]), "legacy\n");
    let src = write(&dir, "pub const FEATURES: &[&str] = &[#[cfg(feature = \"rust-storage-reads\")] \"cap\"];");
    assert_eq!(run(&[&src, "FEATURES", "cap", &build, &cargo]), "v1\n");
    let src = write(&dir, "pub const FEATURES: &[&str] = &[#[cfg(feature = \"disabled-feature\")] \"cap\"];");
    assert_eq!(run(&[&src, "FEATURES", "cap", &build, &cargo]), "legacy\n");
    let missing_build = dir.join("missing.Dockerfile").to_string_lossy().into_owned();
    assert_eq!(run(&[&src, "FEATURES", "cap", &missing_build, &cargo]), "invalid(production-features-underivable)\n");
    // No build given and no RS_CAPABILITY_TOP: nothing is derived, so a queried cfg refuses.
    assert_eq!(run(&[&src, "FEATURES", "cap"]), "invalid(production-features-underivable)\n");
    // The Node CLI reports an unreadable source as invalid(read); the adapter maps a missing path to legacy itself.
    assert_eq!(run(&[&dir.join("absent.rs").to_string_lossy(), "FEATURES", "cap"]), "invalid(read)\n");
    assert_eq!(run(&[]), "invalid(read)\n");
    assert_eq!(run(&[&src]), "invalid(input)\n");
    // Default manifest: FILE's ../Cargo.toml, as in Node.
    let crate_dir = dir.join("crate");
    std::fs::create_dir_all(crate_dir.join("src")).unwrap();
    std::fs::copy(&cargo, crate_dir.join("Cargo.toml")).unwrap();
    let nested = crate_dir.join("src/lib.rs");
    std::fs::write(&nested, "pub const FEATURES: &[&str] = &[#[cfg(feature = \"rust-storage-reads\")] \"cap\"];\n").unwrap();
    assert_eq!(run(&[&nested.to_string_lossy(), "FEATURES", "cap", &build]), "v1\n");
    let _ = std::fs::remove_dir_all(&dir);
}
