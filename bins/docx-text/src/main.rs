//! `docx-text extract`: read a DOCX on stdin and print `{"text", "truncated", "scope"}` on
//! stdout, as noevia-services' `ocr/docx_text.py` `extract_docx` returns it (noevia#981).
//! Exit 0 on success. A refused document: nothing on stdout, `docx-text: refused: <class>` on
//! stderr, exit 1. Bad usage: exit 2. I/O failure: exit 3. Never touches the network or the
//! filesystem.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::process::ExitCode;

const USAGE: &str = "usage: docx-text extract < document.docx";

fn fail(msg: &str, code: u8) -> ExitCode {
    let _ = writeln!(std::io::stderr().lock(), "docx-text: {msg}");
    ExitCode::from(code)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 1 || args.first().map(String::as_str) != Some("extract") {
        return fail(USAGE, 2);
    }
    let mut input = Vec::new();
    let cap = docx_text::MAX_INPUT_BYTES as u64 + 1;
    if std::io::stdin()
        .lock()
        .take(cap)
        .read_to_end(&mut input)
        .is_err()
    {
        return fail("could not read stdin", 3);
    }
    match docx_text::extract_docx(&input) {
        Ok(e) => {
            let mut out = std::io::stdout().lock();
            if writeln!(out, "{}", docx_text::to_json(&e))
                .and_then(|()| out.flush())
                .is_err()
            {
                return fail("could not write to stdout", 3);
            }
            ExitCode::SUCCESS
        }
        Err(r) => fail(&format!("refused: {}", r.class()), 1),
    }
}
