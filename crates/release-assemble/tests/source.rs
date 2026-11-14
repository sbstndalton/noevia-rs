//! `ProcessSource` against the shell's own `export_self` and `fetch_remote` (verbatim excerpts), over
//! a local QA git repo and a stub `curl` that serves a local tarball (no network). Skipped without git.
#![cfg(unix)]

use release_assemble::plan::{assemble_components, Source};
use release_assemble::source::ProcessSource;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const EXCERPT: &str = include_str!("../fixtures/assemble-release-excerpt.sh");

fn section(name: &str) -> String {
    let start = format!("#@{name}\n");
    let from = EXCERPT
        .find(&start)
        .map(|i| i + start.len())
        .unwrap_or(EXCERPT.len());
    EXCERPT
        .get(from..)
        .unwrap_or_default()
        .split("\n#@")
        .next()
        .unwrap_or_default()
        .to_string()
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[allow(clippy::panic)]
fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap_or_else(|e| panic!("no temp dir: {e}"))
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

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=QA Release",
            "-c",
            "user.email=qa@example.invalid",
        ])
        .args(args)
        .output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// A pre-cutover noevia layout: apps/web (web + core paths) and services/.
fn qa_repo(root: &Path) -> String {
    let web = [
        "src/main.ts",
        "public/icon.svg",
        "index.html",
        "vite.config.ts",
        "tsconfig.json",
        "package.json",
        "package-lock.json",
        "scripts/build.sh",
        "contracts/a.json",
        "tests/client/a.test.ts",
        "qa/run.sh",
        "tests/hermetic-network.cjs",
        ".gitignore",
        ".dockerignore",
        "server/index.cjs",
        "server/package.json",
        "tests/server/a.test.cjs",
        "tests/fixtures/f.json",
    ];
    for f in web {
        let body = if f == ".gitignore" {
            "node_modules/\n"
        } else {
            f
        };
        write(
            &root.join("apps/web").join(f),
            body.as_bytes(),
            f.ends_with(".sh"),
        );
    }
    write(
        &root.join("apps/web/server/ui-data/state.json"),
        b"{}",
        false,
    );
    for s in [
        "diary",
        "docling",
        "laya",
        "model-manager",
        "ocr",
        "code-sandbox",
    ] {
        write(
            &root.join("services").join(s).join("Dockerfile"),
            s.as_bytes(),
            false,
        );
    }
    std::os::unix::fs::symlink("Dockerfile", root.join("services/ocr/Dockerfile.link")).ok();
    write(&root.join("README.md"), b"QA", false);
    git(root, &["init", "-q"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "qa"]);
    git(root, &["rev-parse", "HEAD"])
}

fn walk(root: &Path) -> Vec<String> {
    let out = Command::new("find")
        .arg(root)
        .arg("-printf")
        .arg("%P %y %m %s\n")
        .output();
    let mut lines: Vec<String> = out
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // Modes beyond the executable bit come from the extracting process's umask (GNU tar applies it,
    // the Rust reader writes 0644/0755): compare the type, the executable bit and the size.
    for l in &mut lines {
        let parts: Vec<&str> = l.rsplitn(4, ' ').collect();
        if let [size, mode, kind, name] = parts.as_slice() {
            let x = mode.bytes().last().is_some_and(|b| (b - b'0') & 1 == 1);
            *l = match *kind {
                "d" => format!("{name} d"),
                "f" => format!("{name} {} {size}", if x { "x" } else { "f" }),
                other => format!("{name} {other} {size}"),
            };
        }
    }
    lines.sort();
    lines
}

fn shell(
    script_tail: &str,
    envs: &[(&str, &Path)],
    path_prefix: Option<&Path>,
) -> Result<(), String> {
    let script = format!(
        "set -euo pipefail\ndie() {{ echo \"assemble-release: $*\" >&2; exit 1; }}\n{}\n{script_tail}\n",
        section("sources")
    );
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(script);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    if let Some(p) = path_prefix {
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{path}", p.display()));
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim_end().to_string())
    }
}

#[test]
fn export_self_matches_the_shell() {
    if !have("git") || !have("bash") || !have("tar") {
        eprintln!("source: git/bash/tar missing, skipped");
        return;
    }
    let d = tmp();
    let repo = d.path().join("repo");
    let full = qa_repo(&repo);
    let work = d.path().join("work");
    fs::create_dir_all(&work).ok();
    let cases: [(&str, &[&str]); 4] = [
        (
            "apps/web",
            &[
                "server",
                "contracts",
                "tests/server",
                "tests/fixtures",
                "tests/hermetic-network.cjs",
            ],
        ),
        (
            "services",
            &["diary", "docling", "laya", "model-manager", "ocr"],
        ),
        (
            "apps/web",
            &[
                "src",
                "public",
                "index.html",
                ".gitignore",
                ".dockerignore",
                "qa",
            ],
        ),
        ("services", &["diary", "missing-one", "ocr"]),
    ];
    for (i, (prefix, paths)) in cases.iter().enumerate() {
        let sh_dest = d.path().join(format!("sh{i}"));
        let rs_dest = d.path().join(format!("rs{i}"));
        fs::create_dir_all(&sh_dest).ok();
        fs::create_dir_all(&rs_dest).ok();
        let tail = format!("export_self \"$dest\" {prefix} {}", paths.join(" "));
        let sh = shell(
            &tail,
            &[
                ("repo", &repo),
                ("work", &work),
                ("dest", &sh_dest),
                ("full_sha", Path::new(&full)),
                ("sha", Path::new(&full[..8])),
            ],
            None,
        );
        let mut src = ProcessSource::new(
            repo.clone(),
            full[..8].to_string(),
            full.clone(),
            work.clone(),
        );
        let rs = src
            .export_self(&rs_dest, prefix, paths)
            .map_err(|e| format!("assemble-release: {e}"));
        assert_eq!(rs, sh, "case {i}");
        assert_eq!(walk(&rs_dest), walk(&sh_dest), "case {i}");
    }
}

/// A stub curl: copies $CURL_FIXTURE to the -o argument, or fails when CURL_FAIL=1.
fn stub_curl(bin: &Path) -> PathBuf {
    fs::create_dir_all(bin).ok();
    let path = bin.join("curl");
    write(
        &path,
        b"#!/bin/sh\n[ \"${CURL_FAIL:-0}\" = 1 ] && exit 22\nout=\nwhile [ $# -gt 0 ]; do [ \"$1\" = -o ] && { out=$2; shift; }; shift; done\nexec cp \"$CURL_FIXTURE\" \"$out\"\n",
        true,
    );
    // Without it the shell side would find the real curl on PATH and download (noevia#2726 review).
    let meta = fs::metadata(&path);
    let executable = meta.is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    assert!(
        executable,
        "the stub curl was not written as an executable: {}",
        path.display()
    );
    path
}

#[test]
fn fetch_remote_matches_the_shell() {
    if !have("git") || !have("bash") || !have("tar") {
        return;
    }
    let d = tmp();
    let repo = d.path().join("repo");
    let full = qa_repo(&repo);
    let tgz = d.path().join("codeload.tar.gz");
    git(
        &repo,
        &[
            "archive",
            "--format=tar.gz",
            "--prefix=noevia-web-qa/",
            "-o",
            &tgz.to_string_lossy(),
            "HEAD",
        ],
    );
    let good = release_assemble::digest::sha256_file(&tgz).unwrap_or_default();
    let bin = d.path().join("bin");
    let curl = stub_curl(&bin);
    let reference = "a".repeat(40);
    for (i, (sum, fail)) in [
        (good.clone(), "0"),
        ("0".repeat(64), "0"),
        (good.clone(), "1"),
    ]
    .iter()
    .enumerate()
    {
        let sh_dest = d.path().join(format!("fsh{i}"));
        let rs_dest = d.path().join(format!("frs{i}"));
        let sh_work = d.path().join(format!("wsh{i}"));
        let rs_work = d.path().join(format!("wrs{i}"));
        for p in [&sh_dest, &rs_dest, &sh_work, &rs_work] {
            fs::create_dir_all(p).ok();
        }
        std::env::set_var("CURL_FIXTURE", &tgz);
        std::env::set_var("CURL_FAIL", fail);
        let tail = format!("fetch_remote noevia-web {reference} {sum} \"$dest\"");
        let sh = shell(&tail, &[("work", &sh_work), ("dest", &sh_dest)], Some(&bin));
        let mut src = ProcessSource::new(repo.clone(), "qa".into(), full.clone(), rs_work);
        src.curl = curl.clone().into_os_string();
        let rs = src
            .fetch("noevia-web", &reference, sum, &rs_dest)
            .map_err(|e| format!("assemble-release: {e}"));
        assert_eq!(rs, sh, "case {i}");
        assert_eq!(walk(&rs_dest), walk(&sh_dest), "case {i}");
    }
    std::env::remove_var("CURL_FAIL");
}

/// The whole component step over a pre-cutover lock (everything `self`) against the real git repo.
#[test]
fn self_lock_assembles_components() {
    if !have("git") {
        return;
    }
    let d = tmp();
    let repo = d.path().join("repo");
    let full = qa_repo(&repo);
    let tree = d.path().join("tree");
    let work = d.path().join("work");
    fs::create_dir_all(&work).ok();
    let lock =
        release_assemble::lock::Lock::from_bytes(b"NOEVIA_WEB_REF=self\nNOEVIA_CORE_REF=self\n");
    let mut src = ProcessSource::new(repo, full[..8].to_string(), full.clone(), work);
    let got = assemble_components(&lock, &full[..8], &full, &tree, &mut src);
    let got = got.map(|r| (r.web, r.core, r.services, r.rs));
    assert_eq!(
        got,
        Ok((full.clone(), full.clone(), full.clone(), String::new()))
    );
    for need in [
        "web/src/main.ts",
        "core/server/index.cjs",
        "core/code-sandbox/Dockerfile",
        "services/ocr/Dockerfile",
    ] {
        assert!(tree.join(need).exists(), "{need}");
    }
    assert!(!tree.join("services/code-sandbox").exists());
}
