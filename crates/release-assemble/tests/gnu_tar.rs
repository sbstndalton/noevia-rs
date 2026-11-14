//! The tar reader and writer against GNU tar and git archive (the tools the shell uses), and the
//! finish step against the shell's own excerpt. Each test skips with a note when its tool is missing.
#![cfg(unix)]

use release_assemble::plan::Resolved;
use release_assemble::tar::{self, Writer};
use release_assemble::tree;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn write(path: &Path, data: &[u8], exec: bool) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).ok();
    }
    fs::write(path, data).ok();
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }),
    )
    .ok();
}

/// A tree with the shapes a release carries: nested dirs, executables, empty files and dirs, long and
/// non-ASCII names, a relative symlink, a 1 MiB file.
fn sample(root: &Path) {
    write(
        &root.join("noevia/build/web.Dockerfile"),
        b"FROM scratch\n",
        false,
    );
    write(
        &root.join("noevia/deploy/tools/assemble-release.sh"),
        b"#!/bin/sh\n",
        true,
    );
    let deep = format!(
        "web/{}/{}/file-{}.txt",
        "d".repeat(60),
        "e".repeat(70),
        "f".repeat(90)
    );
    write(&root.join(deep), b"long", false);
    write(
        &root.join("web/src/caf\u{e9} na\u{ef}ve.ts"),
        "export {};\n".as_bytes(),
        false,
    );
    write(&root.join("web/src/empty"), b"", false);
    fs::create_dir_all(root.join("core/server/empty-dir")).ok();
    let big: Vec<u8> = (0..(1u32 << 20))
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
        .collect();
    write(&root.join("core/server/blob.bin"), &big, false);
    std::os::unix::fs::symlink("../server/blob.bin", root.join("core/server/link")).ok();
    let long_target = format!("../{}", "t".repeat(140));
    std::os::unix::fs::symlink(long_target, root.join("core/long-link")).ok();
    fs::create_dir_all(root.join("rs")).ok();
}

/// What an extraction leaves on disk, as manifest lines (same format as tree::manifest).
fn walk(root: &Path) -> Vec<String> {
    fn go(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p
                .strip_prefix(root)
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Ok(m) = fs::symlink_metadata(&p) else {
                continue;
            };
            if m.file_type().is_symlink() {
                let t = fs::read_link(&p)
                    .map(|t| t.to_string_lossy().into_owned())
                    .unwrap_or_default();
                out.push(format!("l 0 - {rel} -> {t}"));
            } else if m.is_dir() {
                out.push(format!("d 0 - {rel}"));
                go(root, &p, out);
            } else {
                let data = fs::read(&p).unwrap_or_default();
                let x = if m.permissions().mode() & 0o111 != 0 {
                    "x"
                } else {
                    "f"
                };
                out.push(format!(
                    "{x} {} {} {rel}",
                    data.len(),
                    release_assemble::digest::sha256_hex(&data)
                ));
            }
        }
    }
    let mut out = Vec::new();
    go(root, root, &mut out);
    out.sort_by(|a, b| name_of(a).cmp(name_of(b)));
    out
}

fn name_of(line: &str) -> &str {
    let rest = line.splitn(4, ' ').nth(3).unwrap_or_default();
    rest.split(" -> ").next().unwrap_or_default()
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort_by(|a, b| name_of(a).cmp(name_of(b)));
    v
}

fn manifest_of(tgz: &Path) -> Vec<String> {
    let mut f = fs::File::open(tgz).ok().unwrap_or_else(|| {
        fs::File::open("/dev/null")
            .ok()
            .unwrap_or_else(unreachable_file)
    });
    sorted(tree::manifest(&tar::read_gz(&mut f).unwrap_or_default()))
}

#[allow(clippy::panic)]
fn unreachable_file() -> fs::File {
    panic!("cannot open /dev/null")
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().ok().unwrap_or_else(unreachable_tmp)
}

#[allow(clippy::panic)]
fn unreachable_tmp() -> tempfile::TempDir {
    panic!("no temp dir")
}

#[test]
fn rust_tarball_extracts_with_gnu_tar() {
    if !have("tar") {
        eprintln!("gnu_tar: tar not installed, skipped");
        return;
    }
    let d = tmp();
    let src = d.path().join("src");
    sample(&src);
    let tgz = d.path().join("out.tar.gz");
    assert!(tree::pack(&src, &["noevia", "web", "core", "rs"], &tgz, 0).is_ok());
    let dst = d.path().join("dst");
    fs::create_dir_all(&dst).ok();
    let st = Command::new("tar")
        .arg("-xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(&dst)
        .status();
    assert!(
        st.map(|s| s.success()).unwrap_or(false),
        "GNU tar could not extract the Rust tarball"
    );
    assert_eq!(walk(&dst), walk(&src));
    // And the manifest of the archive is what an extraction gives.
    assert_eq!(manifest_of(&tgz), walk(&src));
    // Deterministic: the same tree packs to the same bytes.
    let again = d.path().join("again.tar.gz");
    assert!(tree::pack(&src, &["noevia", "web", "core", "rs"], &again, 0).is_ok());
    assert_eq!(fs::read(&tgz).ok(), fs::read(&again).ok());
}

#[test]
fn gnu_tar_formats_read_like_an_extraction() {
    if !have("tar") {
        return;
    }
    let d = tmp();
    let src = d.path().join("src");
    sample(&src);
    for format in ["gnu", "pax", "posix", "oldgnu"] {
        let tgz = d.path().join(format!("{format}.tar.gz"));
        let ok = Command::new("tar")
            .arg(format!("--format={format}"))
            .arg("-czf")
            .arg(&tgz)
            .arg("-C")
            .arg(&src)
            .args(["noevia", "web", "core", "rs"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "tar --format={format} failed");
        assert_eq!(manifest_of(&tgz), walk(&src), "format {format}");
        let dst = d.path().join(format!("x-{format}"));
        fs::create_dir_all(&dst).ok();
        let mut f = fs::File::open(&tgz).ok().unwrap_or_else(unreachable_file);
        let entries = tar::read_gz(&mut f).unwrap_or_default();
        assert!(tar::extract(&entries, 0, &dst).is_ok());
        assert_eq!(walk(&dst), walk(&src), "extract of format {format}");
    }
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=QA Release",
            "-c",
            "user.email=qa@example.invalid",
            "-c",
            "core.symlinks=true",
        ])
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// codeload tarballs are `git archive --format=tar.gz --prefix=<repo>-<sha>/`; the shell extracts them
/// with `tar -xzf --strip-components=1`, and the self exports with `tar -x --strip-components=N`.
#[test]
fn git_archive_strip_matches_gnu_tar() {
    if !have("tar") || !have("git") {
        eprintln!("gnu_tar: git or tar not installed, skipped");
        return;
    }
    let d = tmp();
    let repo = d.path().join("repo");
    sample(&repo.join("apps/web"));
    fs::remove_dir_all(repo.join("apps/web/core/server/empty-dir")).ok();
    fs::remove_dir_all(repo.join("apps/web/rs")).ok();
    assert!(
        git(&repo, &["init", "-q"])
            && git(&repo, &["add", "-A"])
            && git(&repo, &["commit", "-qm", "qa"])
    );
    for (strip, prefix, gz) in [
        (1usize, "noevia-web-0123/", true),
        (2, "", false),
        (3, "", false),
    ] {
        let archive = d.path().join(format!("a{strip}"));
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&repo).arg("archive");
        cmd.arg(if gz {
            "--format=tar.gz"
        } else {
            "--format=tar"
        });
        if !prefix.is_empty() {
            cmd.arg(format!("--prefix={prefix}"));
        }
        cmd.arg("-o").arg(&archive).arg("HEAD");
        if !gz {
            cmd.args(["--", "apps/web/web", "apps/web/core"]);
        }
        assert!(cmd.status().map(|s| s.success()).unwrap_or(false));
        let g = d.path().join(format!("g{strip}"));
        let r = d.path().join(format!("r{strip}"));
        fs::create_dir_all(&g).ok();
        fs::create_dir_all(&r).ok();
        let flag = if gz { "-xzf" } else { "-xf" };
        let ok = Command::new("tar")
            .arg(flag)
            .arg(&archive)
            .arg(format!("--strip-components={strip}"))
            .arg("-C")
            .arg(&g)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok);
        let mut f = fs::File::open(&archive)
            .ok()
            .unwrap_or_else(unreachable_file);
        let entries = if gz {
            tar::read_gz(&mut f)
        } else {
            tar::read_all(&mut f)
        };
        let entries = entries.unwrap_or_default();
        assert!(!entries.is_empty());
        assert!(tar::extract(&entries, strip, &r).is_ok());
        assert_eq!(walk(&r), walk(&g), "strip {strip}");
    }
}

fn release_tree(root: &Path) {
    sample(root);
    for need in tree::REQUIRED {
        let p = root.join(need);
        if need.ends_with("src") || need.ends_with("contracts") {
            fs::create_dir_all(&p).ok();
            write(&p.join("x"), b"x", false);
        } else {
            write(&p, need.as_bytes(), false);
        }
    }
    write(
        &root.join("core/server/ui-data/projects.json"),
        b"{}",
        false,
    );
    write(
        &root.join("noevia/apps/web/server/ui-data/state"),
        b"s",
        false,
    );
    write(&root.join("web/server/ui-data-not/keep"), b"k", false);
    // A symlink named ui-data is not a directory to find -type d: it stays.
    fs::create_dir_all(root.join("services/x/server")).ok();
    std::os::unix::fs::symlink("../../diary", root.join("services/x/server/ui-data")).ok();
}

fn copy_tree(from: &Path, to: &Path) -> bool {
    Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn shell_finish(tree_dir: &Path, out_dir: &Path, sha: &str, r: &Resolved) -> Result<(), String> {
    let excerpt = include_str!("../fixtures/assemble-release-excerpt.sh");
    let finish = excerpt.split("#@finish\n").nth(1).unwrap_or_default();
    let script = format!(
        "set -euo pipefail\ndie() {{ echo \"assemble-release: $*\" >&2; exit 1; }}\n{finish}"
    );
    let out = Command::new("bash")
        .arg("-c")
        .arg(script)
        .env("tree", tree_dir)
        .env("out_dir", out_dir)
        .env("sha", sha)
        .env("web_sha", &r.web)
        .env("core_sha", &r.core)
        .env("services_sha", &r.services)
        .env("rs_sha", &r.rs)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim_end().to_string())
    }
}

#[test]
fn finish_matches_the_shell() {
    if !have("tar") || !have("bash") || !have("cp") {
        eprintln!("gnu_tar: bash/tar/cp missing, skipped");
        return;
    }
    let d = tmp();
    let base = d.path().join("base");
    release_tree(&base);
    let r = Resolved {
        web: "a".repeat(40),
        core: "b".repeat(40),
        services: "c".repeat(40),
        rs: String::new(),
    };
    let (sh_tree, rs_tree) = (d.path().join("sh"), d.path().join("rs"));
    assert!(copy_tree(&base, &sh_tree) && copy_tree(&base, &rs_tree));
    let (sh_out, rs_out) = (d.path().join("sh-out"), d.path().join("rs-out"));
    fs::create_dir_all(&sh_out).ok();
    fs::create_dir_all(&rs_out).ok();
    assert_eq!(shell_finish(&sh_tree, &sh_out, "11bf2da7", &r), Ok(()));
    let got = tree::finish(&rs_tree, "11bf2da7", &r, &rs_out, 0);
    assert_eq!(
        got.as_ref().ok(),
        Some(&rs_out.join("noevia-release-11bf2da7.tar.gz"))
    );
    let name = "noevia-release-11bf2da7.tar.gz";
    let sh_manifest = manifest_of(&sh_out.join(name));
    assert!(sh_manifest.iter().any(|l| l.ends_with(" release-refs")));
    assert!(!sh_manifest.iter().any(|l| l.contains("server/ui-data/")));
    assert_eq!(manifest_of(&rs_out.join(name)), sh_manifest);
    assert_eq!(
        walk(&rs_tree),
        walk(&sh_tree),
        "the trees after the finish step"
    );
    assert!(!rs_out.join(format!("{name}.partial")).exists());
}

#[test]
fn missing_entry_refusal_matches_the_shell() {
    if !have("bash") || !have("cp") {
        return;
    }
    let d = tmp();
    let base = d.path().join("base");
    release_tree(&base);
    let r = Resolved {
        web: "a".into(),
        core: "b".into(),
        services: "c".into(),
        rs: "d".into(),
    };
    for drop in [
        "core/contracts",
        "services/ocr/Dockerfile",
        "web/package.json",
    ] {
        let (sh_tree, rs_tree) = (
            d.path().join(format!("sh-{}", drop.len())),
            d.path().join(format!("rs-{}", drop.len())),
        );
        assert!(copy_tree(&base, &sh_tree) && copy_tree(&base, &rs_tree));
        let _ = fs::remove_dir_all(sh_tree.join(drop));
        let _ = fs::remove_file(sh_tree.join(drop));
        let _ = fs::remove_dir_all(rs_tree.join(drop));
        let _ = fs::remove_file(rs_tree.join(drop));
        let sh = shell_finish(&sh_tree, d.path(), "11bf2da7", &r);
        let rs = tree::finish(&rs_tree, "11bf2da7", &r, d.path(), 0)
            .map(|_| ())
            .map_err(|e| format!("assemble-release: {e}"));
        assert_eq!(rs, sh, "dropping {drop}");
    }
}

fn header_tar(build: impl FnOnce(&mut Writer<Vec<u8>>)) -> Vec<u8> {
    let mut w = Writer::new(Vec::new(), 0);
    build(&mut w);
    w.finish().unwrap_or_default()
}

#[test]
fn hostile_archives_are_refused() {
    let d = tmp();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "dotdot",
            header_tar(|w| drop(w.file(b"top/../../escape", false, b"x"))),
        ),
        (
            "absolute",
            header_tar(|w| drop(w.file(b"/top/abs", false, b"x"))),
        ),
        (
            "through a symlink",
            header_tar(|w| {
                drop(w.symlink(b"top/a", b"/tmp"));
                drop(w.file(b"top/a/x", false, b"x"));
            }),
        ),
        (
            "file over a dir",
            header_tar(|w| {
                drop(w.dir(b"top/a"));
                drop(w.file(b"top/a", false, b"x"));
            }),
        ),
    ];
    for (what, bytes) in cases {
        let dst: PathBuf = d.path().join(what.replace(' ', "-"));
        fs::create_dir_all(&dst).ok();
        let entries = tar::read_all(&mut bytes.as_slice()).unwrap_or_default();
        assert!(!entries.is_empty(), "{what}: archive did not parse");
        assert!(
            tar::extract(&entries, 1, &dst).is_err(),
            "{what}: extracted"
        );
    }
    // A corrupted header checksum and a truncated member are read errors.
    let good = header_tar(|w| drop(w.file(b"top/f", false, &[7u8; 1000])));
    let mut bad_sum = good.clone();
    if let Some(b) = bad_sum.get_mut(0) {
        *b ^= 1;
    }
    assert!(tar::read_all(&mut bad_sum.as_slice()).is_err());
    let truncated = good.get(..900).unwrap_or_default().to_vec();
    assert!(tar::read_all(&mut truncated.as_slice()).is_err());
    let no_marker = good.get(..1536).unwrap_or_default().to_vec();
    assert!(tar::read_all(&mut no_marker.as_slice()).is_err());
}

#[test]
fn gnu_hardlinks_are_refused() {
    if !have("tar") {
        return;
    }
    let d = tmp();
    let src = d.path().join("src");
    write(&src.join("top/a"), b"a", false);
    fs::hard_link(src.join("top/a"), src.join("top/b")).ok();
    let tgz = d.path().join("h.tar.gz");
    let ok = Command::new("tar")
        .arg("-czf")
        .arg(&tgz)
        .arg("-C")
        .arg(&src)
        .arg("top")
        .status();
    assert!(ok.map(|s| s.success()).unwrap_or(false));
    let mut f = fs::File::open(&tgz).ok().unwrap_or_else(unreachable_file);
    assert!(tar::read_gz(&mut f).is_err());
}
