//! `reviewed-source digest <archive> <root>` prints the archive's reviewed-export digest.
//! `reviewed-source extract <archive> <root> <digest> <destination>` extracts it only when the digest matches.
//! `reviewed-source extract-ref <archive> <ref> <digest> <destination>` is `extract` for a codeload export of
//! the Rust repository at a full commit id (root `noevia-rs-<ref>`); it replaces the reference's `fetch`,
//! whose download step moves to the image recipe.
//! Every failure prints the reference's one line and exits 1.

#![forbid(unsafe_code)]

use reviewed_source::{extract, inspect, is_ref, read_bounded, Error, Limits, Refused};
use std::path::Path;

const REFUSED: &str = "native verifier source does not match the reviewed export";

fn read(path: &str, limits: &Limits) -> Result<Vec<u8>, Error> {
    read_bounded(std::fs::File::open(path)?, limits.max_expanded)
}

fn run(args: &[String]) -> Result<String, Error> {
    let limits = Limits::DEFAULT;
    match args {
        [command, archive, root] if command == "digest" => Ok(inspect(&read(archive, &limits)?, root, &limits)?.digest),
        [command, archive, root, digest, destination] if command == "extract" => {
            extract(&read(archive, &limits)?, root, digest, Path::new(destination), &limits)
        }
        [command, archive, reference, digest, destination] if command == "extract-ref" => {
            if !is_ref(reference) {
                return Err(Refused("invalid reviewed ref".into()).into());
            }
            let data = read_bounded(std::fs::File::open(archive)?, limits.max_archive)?;
            extract(&data, &format!("noevia-rs-{reference}"), digest, Path::new(destination), &limits)
        }
        _ => Err(Refused("invalid source verifier arguments".into()).into()),
    }
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = match std::env::args_os().skip(1).map(|arg| arg.into_string()).collect() {
        Ok(args) => args,
        Err(_) => {
            eprintln!("{REFUSED}");
            return std::process::ExitCode::from(1);
        }
    };
    match run(&args) {
        Ok(digest) => {
            println!("{digest}");
            std::process::ExitCode::SUCCESS
        }
        Err(_) => {
            eprintln!("{REFUSED}");
            std::process::ExitCode::from(1)
        }
    }
}
