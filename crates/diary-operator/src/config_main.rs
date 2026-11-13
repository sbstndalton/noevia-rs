use std::{collections::BTreeMap, path::Path, process::ExitCode};
fn invalid() -> ExitCode {
    eprintln!("diary-config: configuration could not be loaded");
    ExitCode::from(1)
}
fn main() -> ExitCode {
    // OS-string conversion failures must not panic and echo an environment value.
    let args = std::env::args_os()
        .skip(1)
        .map(|s| s.into_string().map_err(|_| ()))
        .collect::<Result<Vec<_>, _>>();
    let env = std::env::vars_os()
        .map(|(k, v)| {
            Ok((
                k.into_string().map_err(|_| ())?,
                v.into_string().map_err(|_| ())?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, ()>>();
    let (Ok(args), Ok(env)) = (args, env) else {
        return invalid();
    };
    let dir = env
        .get("DIARY_CONFIG_DIR")
        .map(String::as_str)
        .unwrap_or("/app/config");
    // This command is a load-status diagnostic, never a config export. Even model
    // names, arbitrary keys, parser source snippets and warnings can contain secrets.
    match diary_operator::config::load(args.first().map(Path::new), Path::new(dir), &env) {
        Ok(_) => {
            println!("{{\"configured\":true}}");
            ExitCode::SUCCESS
        }
        Err(_) => invalid(),
    }
}
