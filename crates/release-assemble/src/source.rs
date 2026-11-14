//! The real [`Source`]: `git archive` for `self` components and an anonymous https download for pinned
//! ones, the way assemble-release.sh does them, with the archives read and written by [`crate::tar`]
//! instead of `tar -x`. Every child process runs under a deadline.

use crate::digest;
use crate::plan::Source;
use crate::tar;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `git` and `curl` against one noevia checkout.
pub struct ProcessSource {
    /// The noevia checkout (`$repo`).
    pub repo: PathBuf,
    /// The noevia sha as given (`$sha`, used in refusals).
    pub sha: String,
    /// What it resolved to (`$full_sha`).
    pub full_sha: String,
    /// Scratch directory for archives (`$work`).
    pub work: PathBuf,
    /// The download program (default `curl`).
    pub curl: OsString,
    /// The base download URL (default `https://codeload.github.com/sbstndalton`).
    pub base_url: String,
    /// Deadline per child process.
    pub deadline: Duration,
}

impl ProcessSource {
    pub fn new(repo: PathBuf, sha: String, full_sha: String, work: PathBuf) -> Self {
        ProcessSource {
            repo,
            sha,
            full_sha,
            work,
            curl: OsString::from("curl"),
            base_url: "https://codeload.github.com/sbstndalton".into(),
            deadline: Duration::from_secs(600),
        }
    }
}

/// Run to completion within `deadline` (killed after it); stdout and stderr are discarded.
pub fn run_with_deadline(cmd: &mut Command, deadline: Duration) -> bool {
    let Ok(mut child) = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(unix)]
impl Source for ProcessSource {
    fn export_self(&mut self, dest: &Path, prefix: &str, paths: &[&str]) -> Result<(), String> {
        let mut specs = Vec::with_capacity(paths.len());
        for p in paths {
            let spec = format!("{prefix}/{p}");
            let ok = run_with_deadline(
                Command::new("git")
                    .arg("-C")
                    .arg(&self.repo)
                    .arg("cat-file")
                    .arg("-e")
                    .arg(format!("{}:{spec}", self.full_sha)),
                self.deadline,
            );
            if !ok {
                return Err(format!("{spec} is missing at {}", self.sha));
            }
            specs.push(spec);
        }
        let strip = prefix.bytes().filter(|b| *b == b'/').count() + 1;
        let archive = self
            .work
            .join(format!("export-{}.tar", prefix.replace('/', "_")));
        let ok = run_with_deadline(
            Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .arg("archive")
                .arg("--format=tar")
                .arg("-o")
                .arg(&archive)
                .arg(&self.full_sha)
                .arg("--")
                .args(&specs),
            self.deadline,
        );
        if !ok {
            return Err(format!("git archive of {prefix} failed at {}", self.sha));
        }
        let entries = std::fs::File::open(&archive)
            .and_then(|mut f| tar::read_all(&mut f))
            .map_err(|e| format!("export of {prefix}: {e}"))?;
        let _ = std::fs::remove_file(&archive);
        std::fs::create_dir_all(dest).map_err(|e| format!("export of {prefix}: {e}"))?;
        tar::extract(&entries, strip, dest).map_err(|e| format!("export of {prefix}: {e}"))?;
        Ok(())
    }

    fn fetch(
        &mut self,
        repo: &str,
        reference: &str,
        sha256: &str,
        dest: &Path,
    ) -> Result<(), String> {
        let tgz = self.work.join(format!("{repo}.tar.gz"));
        let url = format!("{}/{repo}/tar.gz/{reference}", self.base_url);
        let ok = run_with_deadline(
            Command::new(&self.curl)
                .args([
                    "-fsSL",
                    "--proto",
                    "=https",
                    "--tlsv1.2",
                    "--retry",
                    "3",
                    "-o",
                ])
                .arg(&tgz)
                .arg(&url),
            self.deadline,
        );
        if !ok {
            return Err(format!("could not download {repo} at {reference}"));
        }
        digest::check_tarball(&tgz, repo, reference, sha256)?;
        let entries = std::fs::File::open(&tgz)
            .and_then(|mut f| tar::read_gz(&mut f))
            .map_err(|e| format!("{repo} tarball at {reference}: {e}"))?;
        std::fs::create_dir_all(dest).map_err(|e| format!("{repo}: {e}"))?;
        tar::extract(&entries, 1, dest)
            .map_err(|e| format!("{repo} tarball at {reference}: {e}"))?;
        Ok(())
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
}
