//! `model-files tree`: read a Hugging Face tree listing (a JSON array) on stdin and print
//! `{"files": [...]}` on stdout, as noevia model-manager's `files_from_tree_py` returns it.
//! `model-files unicode-version`: print the Unicode version the character tables came from.
//! `model-files backups`: read a models.ini write's backup request (JSON) on stdin and print which
//! recovery copies to make and which to remove (noevia#1021; see `model_files::backups`). A
//! refused request prints `model-files: refused: <code>` and exits 1.
//! Exit 0 on success; on error, a message on stderr, nothing on stdout, and exit 1 (2 for bad
//! usage). Never touches the network or the filesystem.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::process::ExitCode;

const USAGE: &str =
    "usage: model-files tree < listing.json | model-files backups < request.json | model-files unicode-version";

fn fail(msg: &str, code: u8) -> ExitCode {
    let _ = writeln!(std::io::stderr().lock(), "model-files: {msg}");
    ExitCode::from(code)
}

fn print(text: &str) -> ExitCode {
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{text}").and_then(|()| out.flush()).is_err() {
        return fail("could not write to stdout", 1);
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["tree"] => {}
        ["backups"] => return backups(),
        ["unicode-version"] => return print(model_files::pytables::UNIDATA_VERSION),
        _ => return fail(USAGE, 2),
    }
    let mut input = Vec::new();
    let cap = model_files::MAX_INPUT_BYTES as u64 + 1;
    if std::io::stdin()
        .lock()
        .take(cap)
        .read_to_end(&mut input)
        .is_err()
    {
        return fail("could not read stdin", 1);
    }
    match model_files::files_from_tree_json(&input) {
        Ok(json) => print(&json),
        Err(e) => fail(&e.to_string(), 1),
    }
}

fn backups() -> ExitCode {
    let mut input = Vec::new();
    let cap = model_files::backups::MAX_INPUT_BYTES as u64 + 1;
    if std::io::stdin()
        .lock()
        .take(cap)
        .read_to_end(&mut input)
        .is_err()
    {
        return fail("could not read stdin", 1);
    }
    match model_files::backups::plan_json(&input) {
        Ok(json) => print(&json),
        Err(e) => fail(&format!("refused: {}", e.code()), 1),
    }
}
