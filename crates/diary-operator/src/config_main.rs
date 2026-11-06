use std::{collections::BTreeMap, path::Path, process::ExitCode};
fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let env: BTreeMap<_, _> = std::env::vars().collect();
    let dir = env
        .get("DIARY_CONFIG_DIR")
        .map(String::as_str)
        .unwrap_or("/app/config");
    match diary_operator::config::load(args.first().map(Path::new), Path::new(dir), &env) {
        Ok(cfg) => {
            for warning in cfg.warnings {
                eprintln!("{warning}");
            }
            match cfg.data.pretty() {
                Ok(v) => {
                    println!("{v}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::from(1)
                }
            }
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}
