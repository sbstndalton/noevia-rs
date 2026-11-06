use diary_operator::value::repr;
use std::{path::Path, process::ExitCode};
const USAGE: &str = "usage: backup_restore.py [-h] backup_root manifest new_directory";
fn failure(error: &str) -> ExitCode {
    eprintln!("{USAGE}\nbackup_restore.py: error: {error}");
    ExitCode::from(2)
}
fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut unknown = Vec::new();
    let mut literal = false;
    let negative = regex::Regex::new(r"^-\d+$|^-\d*\.\d+$").ok();
    for (index, arg) in args.iter().enumerate() {
        if !literal && arg == "--" {
            literal = true;
            continue;
        }
        if !literal {
            let (option, explicit) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(a, b)| (a, Some(b)));
            if option == "-h" || (option.starts_with("--") && "--help".starts_with(option)) {
                if let Some(value) = explicit {
                    return failure(&format!(
                        "argument -h/--help: ignored explicit argument {}",
                        repr(value)
                    ));
                }
                print!("{}", include_str!("../tests/help.txt"));
                return ExitCode::SUCCESS;
            }
            if arg.strip_prefix("-h").is_some_and(|s| !s.is_empty()) {
                print!("{}", include_str!("../tests/help.txt"));
                return ExitCode::SUCCESS;
            }
        }
        if !literal
            && arg.starts_with('-')
            && arg != "-"
            && !negative.as_ref().is_some_and(|r| r.is_match(arg))
        {
            unknown.push((index, arg.as_str()));
        } else {
            positional.push((index, arg.as_str()));
        }
    }
    if positional.len() < 3 {
        let missing = ["backup_root", "manifest", "new_directory"]
            .get(positional.len()..)
            .unwrap_or(&[])
            .join(", ");
        return failure(&format!("the following arguments are required: {missing}"));
    }
    unknown.extend(positional.iter().skip(3).copied());
    unknown.sort_by_key(|(i, _)| *i);
    if !unknown.is_empty() {
        return failure(&format!(
            "unrecognized arguments: {}",
            unknown
                .iter()
                .map(|(_, s)| *s)
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    let mut args = positional.into_iter().map(|(_, s)| s);
    let Some(root) = args.next() else {
        return ExitCode::from(2);
    };
    let Some(manifest) = args.next() else {
        return ExitCode::from(2);
    };
    let Some(destination) = args.next() else {
        return ExitCode::from(2);
    };
    match diary_operator::restore::restore(
        Path::new(root),
        Path::new(manifest),
        Path::new(destination),
    )
    .and_then(|v| diary_operator::restore::pretty(&v))
    {
        Ok(result) => {
            println!("{result}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}
