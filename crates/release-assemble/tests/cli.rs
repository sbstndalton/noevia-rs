//! The `release-assemble` binary's contract: outputs, the `assemble-release: ` refusal prefix, exit
//! codes 0 (ok), 1 (refused) and 2 (usage).
#![cfg(unix)]

use std::fs;
use std::process::{Command, Output};

fn bin(args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_release-assemble")).args(args).output();
    out.unwrap_or_else(|e| unreachable(&e.to_string()))
}

#[allow(clippy::panic)]
fn unreachable(e: &str) -> Output {
    panic!("binary did not run: {e}")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn plan_lock_value_and_refusals() {
    let Ok(d) = tempfile::tempdir() else { return };
    let h40 = "e0954036625168bde8cd0634f50c1fa81bfcb9e7";
    let h64 = "fb4d0ef572520f6700030c76a4b087dd7412f7c4ab2ee74ce896ea00c9472acb";
    let lock = d.path().join("versions.lock");
    let lock_s = lock.to_string_lossy().into_owned();
    fs::write(&lock, format!("# c\nNOEVIA_WEB_REF=self\nNOEVIA_CORE_REF={h40}\nNOEVIA_CORE_SHA256={h64}\nNOEVIA_RS_REF={h40}\n")).ok();
    let out = bin(&["plan", &lock_s, "11bf2da7"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), format!("web\tself\ncore\t{h40}\t{h64}\nservices\tself\nrs\tnone\n"));
    let out = bin(&["lock-value", &lock_s, "NOEVIA_CORE_REF"]);
    assert_eq!(text(&out.stdout), format!("{h40}\n"));
    // usage: bad sha, unknown subcommand
    assert_eq!(bin(&["plan", &lock_s, "11BF2DA7"]).status.code(), Some(2));
    assert_eq!(bin(&["frobnicate"]).status.code(), Some(2));
    // refusal
    fs::write(&lock, format!("NOEVIA_WEB_REF={h40}\n")).ok();
    let out = bin(&["plan", &lock_s, "11bf2da7"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "assemble-release: NOEVIA_WEB_REF is pinned but NOEVIA_WEB_SHA256 is missing or not 64 lowercase hex\n"
    );
}

#[test]
fn checksum_extract_and_manifest() {
    let Ok(d) = tempfile::tempdir() else { return };
    let src = d.path().join("src");
    fs::create_dir_all(src.join("top/a")).ok();
    fs::write(src.join("top/a/f.txt"), b"hello").ok();
    let tgz = d.path().join("t.tar.gz");
    assert!(release_assemble::tree::pack(&src, &["top"], &tgz, 0).is_ok());
    let tgz_s = tgz.to_string_lossy().into_owned();
    let sum = release_assemble::digest::sha256_file(&tgz).unwrap_or_default();
    assert_eq!(bin(&["check-sha256", &tgz_s, "noevia-web", "r", &sum]).status.code(), Some(0));
    let out = bin(&["check-sha256", &tgz_s, "noevia-web", "r", &"0".repeat(64)]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stderr), "assemble-release: noevia-web tarball checksum mismatch at r\n");
    let dest = d.path().join("dest");
    fs::create_dir_all(&dest).ok();
    let out = bin(&["extract", &tgz_s, &dest.to_string_lossy(), "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(fs::read(dest.join("a/f.txt")).ok(), Some(b"hello".to_vec()));
    let out = bin(&["manifest", &tgz_s]);
    assert_eq!(
        text(&out.stdout),
        "d 0 - top\nd 0 - top/a\nf 5 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824 top/a/f.txt\n"
    );
}
