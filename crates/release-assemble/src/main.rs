//! `release-assemble`: the Rust slices of assemble-release.sh as subcommands (inert: nothing calls it).
//!
//! ```text
//! release-assemble lock-value LOCK KEY            value of the last KEY= line (lock_value)
//! release-assemble plan LOCK SHA                  web/core/services/rs resolution, or the die text
//! release-assemble check-sha256 FILE REPO REF SUM the fetch_remote checksum check
//! release-assemble extract TGZ DEST [STRIP]       tar -xzf TGZ --strip-components=STRIP -C DEST (fail closed)
//! release-assemble finish TREE SHA OUT_DIR WEB CORE SERVICES [RS]
//!                                                 required entries, release-refs, ui-data prune,
//!                                                 .dockerignore, deterministic tarball
//! release-assemble manifest TGZ                   content manifest (compare two release tarballs)
//! ```
//! Refusals print `assemble-release: <reason>` on stderr and exit 1; bad usage exits 2.

use release_assemble::lock::Lock;
use release_assemble::plan::{self, Component, Resolution, Resolved};
use release_assemble::{digest, tar, tree};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: release-assemble lock-value LOCK KEY | plan LOCK SHA | check-sha256 FILE REPO REF SUM | extract TGZ DEST [STRIP] | finish TREE SHA OUT_DIR WEB CORE SERVICES [RS] | manifest TGZ";

fn die(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("assemble-release: {msg}");
    ExitCode::from(1)
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn read_lock(path: &str) -> Result<Lock, String> {
    std::fs::read(path).map(|b| Lock::from_bytes(&b)).map_err(|_| format!("--lock file not found: {path}"))
}

fn open_tgz(path: &str) -> Result<Vec<tar::Entry>, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    tar::read_gz(&mut f).map_err(|e| format!("{path}: {e}"))
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["lock-value", lock, key] => {
            let v = read_lock(lock)?.value(key);
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            out.write_all(&v).and_then(|()| out.write_all(b"\n")).map_err(|e| e.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        ["plan", lock, sha] => {
            if !release_assemble::lock::release_sha(sha) {
                return Ok(usage());
            }
            let lock = read_lock(lock)?;
            for c in Component::ALL {
                match plan::resolve(&lock, c, sha)? {
                    Resolution::SelfExport => println!("{}\tself", c.dir()),
                    Resolution::Pinned { reference, sha256 } => println!("{}\t{reference}\t{sha256}", c.dir()),
                }
            }
            match plan::resolve_rs(&lock, sha)? {
                None => println!("rs\tnone"),
                Some((reference, sum)) => println!("rs\t{reference}\t{sum}"),
            }
            Ok(ExitCode::SUCCESS)
        }
        ["check-sha256", file, repo, reference, sum] => {
            digest::check_tarball(Path::new(file), repo, reference, sum)?;
            Ok(ExitCode::SUCCESS)
        }
        ["extract", tgz, dest, rest @ ..] => {
            let strip = match rest {
                [] => 0,
                [s] => match s.parse() {
                    Ok(n) => n,
                    Err(_) => return Ok(usage()),
                },
                _ => return Ok(usage()),
            };
            let entries = open_tgz(tgz)?;
            tar::extract(&entries, strip, Path::new(dest)).map_err(|e| e.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        ["finish", tree_dir, sha, out_dir, web, core, services, rest @ ..] => {
            let rs = match rest {
                [] => String::new(),
                [r] => (*r).to_string(),
                _ => return Ok(usage()),
            };
            let resolved = Resolved { web: (*web).into(), core: (*core).into(), services: (*services).into(), rs };
            let mtime = std::env::var("SOURCE_DATE_EPOCH").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
            let out = tree::finish(Path::new(tree_dir), sha, &resolved, Path::new(out_dir), mtime)?;
            println!("ok: {}", out.display());
            Ok(ExitCode::SUCCESS)
        }
        ["manifest", tgz] => {
            for line in tree::manifest(&open_tgz(tgz)?) {
                println!("{line}");
            }
            Ok(ExitCode::SUCCESS)
        }
        _ => Ok(usage()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(msg) => die(msg),
    }
}
