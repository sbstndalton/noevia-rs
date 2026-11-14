//! `synthetic-pdfs`: write the OCR real-engine test fixtures (the services `ocr/synthetic_pdfs.py`
//! CLI and the one-liners the noevia-ocr tests run through `python3 -c`).
//!
//!   synthetic-pdfs [write-all [DIR]]          every fixture into DIR (default .), "path size" lines
//!   synthetic-pdfs oversized PATH             the padded text PDF past 25 MB
//!   synthetic-pdfs ballast PATH BYTES [CHUNK] the #1867 three-page ballast PDF; prints its size
//!   synthetic-pdfs skip-reason                the missing-engine skip reason ("" = none missing)

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use synthetic_pdfs::{oversized_text_pdf, skip_reason, write_all, write_ballast_pdf, SystemProbe, BALLAST_CHUNK};

const USAGE: &str = "usage: synthetic-pdfs [write-all [DIR] | oversized PATH | ballast PATH BYTES [CHUNK] | skip-reason]";

/// How the reference prints `Path(directory) / name`: "." is dropped, as pathlib does.
fn shown(directory: &Path, name: &str) -> PathBuf {
    if directory == Path::new(".") || directory.as_os_str().is_empty() {
        PathBuf::from(name)
    } else {
        directory.join(name)
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let text = |e: synthetic_pdfs::Error| e.to_string();
    let number = |s: &String| s.parse::<usize>().map_err(|_| format!("not a byte count: {s}"));
    match args {
        [] => run(&["write-all".to_owned()]),
        [cmd] | [cmd, _] if cmd == "write-all" => {
            let directory = Path::new(args.get(1).map_or(".", String::as_str));
            for (name, path) in write_all(directory).map_err(text)? {
                let size = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
                println!("{} {size}", shown(directory, name).display());
            }
            Ok(())
        }
        [cmd, path] if cmd == "oversized" => {
            std::fs::write(path, oversized_text_pdf().map_err(text)?).map_err(|e| e.to_string())
        }
        [cmd, path, bytes, rest @ ..] if cmd == "ballast" && rest.len() <= 1 => {
            let chunk = rest.first().map(number).transpose()?.unwrap_or(BALLAST_CHUNK);
            let size = write_ballast_pdf(Path::new(path), number(bytes)?, chunk).map_err(text)?;
            println!("{size}");
            Ok(())
        }
        [cmd] if cmd == "skip-reason" => {
            println!("{}", skip_reason(&SystemProbe));
            Ok(())
        }
        _ => Err(USAGE.to_owned()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("synthetic-pdfs: {e}");
            ExitCode::FAILURE
        }
    }
}
