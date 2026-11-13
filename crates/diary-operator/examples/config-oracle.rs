//! Test-only raw loader/prompt projection. Never installed in the service image.
use std::{collections::BTreeMap, path::Path, process::ExitCode};
fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let env: BTreeMap<_, _> = std::env::vars().collect();
    let mode = args.first().map(String::as_str).unwrap_or("config");
    let path = args.get(1).or_else(|| {
        args.first()
            .filter(|s| s.as_str() != "config" && s.as_str() != "prompt")
    });
    let result = diary_operator::config::load(path.map(Path::new), Path::new("/app/config"), &env)
        .and_then(|cfg| {
            for warning in cfg.warnings {
                eprintln!("{warning}");
            }
            if mode == "prompt" {
                let dir = args.get(2).map(String::as_str).unwrap_or("/app/config");
                diary_operator::config::system_prompt(&cfg.data, Path::new(dir))
                    .map(|s| diary_operator::value::quote(&s))
            } else {
                cfg.data.pretty()
            }
        });
    match result {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}
