use std::{path::Path, process::ExitCode};
const USAGE: &str = "usage: backup_restore.py [-h] backup_root manifest new_directory";
fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut unknown = Vec::new();
    let mut literal = false;
    for arg in &args {
        if !literal && arg == "--" {
            literal = true;
            continue;
        }
        if !literal && (arg == "-h" || (arg.starts_with("--") && "--help".starts_with(arg))) {
            print!("{}", include_str!("../tests/help.txt"));
            return ExitCode::SUCCESS;
        }
        if !literal && arg.starts_with('-') && arg != "-" && arg.parse::<f64>().is_err() {
            unknown.push(arg.as_str());
        } else {
            positional.push(arg.as_str());
        }
    }
    let error = if positional.len() < 3 {
        let missing = ["backup_root", "manifest", "new_directory"]
            .get(positional.len()..)
            .unwrap_or(&[])
            .join(", ");
        Some(format!("the following arguments are required: {missing}"))
    } else {
        unknown.extend(positional.iter().skip(3).copied());
        (!unknown.is_empty()).then(|| format!("unrecognized arguments: {}", unknown.join(" ")))
    };
    if let Some(error) = error {
        eprintln!("{USAGE}\nbackup_restore.py: error: {error}");
        return ExitCode::from(2);
    }
    let mut args = positional.into_iter();
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
