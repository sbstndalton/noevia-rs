//! `rs-capability FILE NAME LITERAL [BUILD] [CARGO]`: the Node CLI of rs-capability.cjs, same arguments and output
//! (`v1`, `legacy` or `invalid(reason)` on one line, exit 0). The features of a queried `cfg(feature)` element come
//! from BUILD (the Dockerfile that compiles the release source) and CARGO (default: FILE's `../Cargo.toml`). The Node
//! CLI defaults BUILD to its own Top checkout's `build/web.Dockerfile`; this binary has no checkout, so an empty BUILD
//! reads `$RS_CAPABILITY_TOP/build/web.Dockerfile` when that is set and otherwise derives nothing (a queried cfg
//! element then refuses, as it does in Node when the build cannot be read).

use std::io::Write;
use std::path::Path;

fn read(path: &str) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn dirname(file: &str) -> String {
    match Path::new(file)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
    {
        Some(p) if !p.is_empty() => p,
        _ if file.starts_with('/') => "/".to_string(),
        _ => ".".to_string(),
    }
}

fn verdict(args: &[String]) -> String {
    let arg = |i: usize| args.get(i).map(String::as_str).filter(|s| !s.is_empty());
    let Some(source) = arg(0).and_then(read) else {
        return "invalid(read)".to_string();
    };
    let file = arg(0).unwrap_or_default();
    let build_path = arg(3).map(str::to_string).or_else(|| {
        std::env::var("RS_CAPABILITY_TOP")
            .ok()
            .filter(|t| !t.is_empty())
            .map(|t| format!("{t}/build/web.Dockerfile"))
    });
    let cargo_path = arg(4)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}/../Cargo.toml", dirname(file)));
    let features = match (build_path.as_deref().and_then(read), read(&cargo_path)) {
        (Some(build), Some(cargo)) => rs_capability::derive_features(&build, &cargo),
        _ => None,
    };
    let name = args.get(1).map(String::as_str).unwrap_or_default();
    let capability = args.get(2).map(String::as_str).unwrap_or_default();
    rs_capability::detect(&source, name, capability, features.as_ref()).to_string()
}

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let line = verdict(&args);
    let _ = writeln!(std::io::stdout(), "{line}");
}
