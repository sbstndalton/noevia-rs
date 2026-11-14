//! `route-pins-recheck --core DIR [--manifest FILE] [--also LABEL=DIR]...`: re-reads the recording's file facts from
//! a core checkout (no Node; see route_pins::recheck) and prints the drift and the delete list recomputed with them,
//! as Markdown on stdout. With `--manifest` (the output of `noevia-server --owned-routes` from the binary the train
//! ships) the verdicts use that front; without it, every recorded refusal counts as answered, so only the file facts
//! decide. Then (council p12) the core server files the static require graph no longer reaches, today and once the
//! delete list's file deletions are made (route_pins::reach); `--also noevia-rs=DIR --also noevia=DIR` adds the other
//! repos to the name search, so a recorder input or a pairing marker there is kept. Reach does not change the exit code.
//!
//! Exit codes: 0 no drift, 1 drift found (the list is printed either way), 2 usage or read error. Read-only: it
//! never writes into DIR. Inert: nothing in a release or in CI calls it yet (the deletion train will).

use route_pins::reach;
use route_pins::recheck::{self, Tree};
use route_pins::{classify, delete_list, embedded, Manifest, Verdict};
use std::path::Path;
use std::process::ExitCode;

/// The recorder's `--exclude-dir` set, plus build output a checkout may hold.
const SKIP_DIRS: &[&str] = &["node_modules", ".git", "target"];
/// Files above this are not core sources (a stray archive); the recorder's grep would read them, so they are named.
const MAX_FILE: u64 = 8 * 1024 * 1024;

fn walk(
    root: &Path,
    dir: &Path,
    tree: &mut Tree,
    skipped: &mut Vec<String>,
) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let kind = e.file_type()?;
        let name = e.file_name().to_string_lossy().into_owned();
        if kind.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                walk(root, &path, tree, skipped)?;
            }
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or(name);
        if e.metadata()?.len() > MAX_FILE {
            skipped.push(rel);
            continue;
        }
        let bytes = std::fs::read(&path)?;
        tree.insert(rel, String::from_utf8_lossy(&bytes).into_owned());
    }
    Ok(())
}

fn usage() -> ExitCode {
    eprintln!("usage: route-pins-recheck --core DIR [--manifest FILE] [--also LABEL=DIR]...");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut core = None;
    let mut manifest = None;
    let mut also = Vec::new();
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        match a.to_str() {
            Some("--core") => core = args.next(),
            Some("--manifest") => manifest = args.next(),
            Some("--also") => match args.next().as_ref().and_then(|v| v.to_str()) {
                Some(v) => match v.split_once('=') {
                    Some((label, dir)) if !label.is_empty() && !dir.is_empty() => {
                        also.push((label.to_string(), dir.to_string()));
                    }
                    _ => return usage(),
                },
                None => return usage(),
            },
            _ => return usage(),
        }
    }
    let Some(core) = core else {
        return usage();
    };
    let root = Path::new(&core);
    let mut tree = Tree::new();
    let mut skipped = Vec::new();
    if let Err(e) = walk(root, root, &mut tree, &mut skipped) {
        eprintln!("route-pins-recheck: reading {}: {e}", root.display());
        return ExitCode::from(2);
    }
    let rec = match embedded() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("route-pins-recheck: the compiled-in recording: {e}");
            return ExitCode::from(2);
        }
    };
    let checked = match recheck::recheck(&rec, &tree) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("route-pins-recheck: {e}");
            return ExitCode::from(2);
        }
    };
    let list = match manifest {
        Some(file) => {
            let parsed = std::fs::read_to_string(&file)
                .map_err(|e| e.to_string())
                .and_then(|t| Manifest::parse(&t));
            let m = match parsed {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "route-pins-recheck: manifest {}: {e}",
                        Path::new(&file).display()
                    );
                    return ExitCode::from(2);
                }
            };
            classify(&checked.recording, &m.front(&checked.recording))
        }
        None => classify(&checked.recording, &|_: &str, _: &str| true),
    };
    println!("## Recheck against {}", root.display());
    println!();
    println!(
        "Recording: core `{}`. Left as recorded: {}.",
        rec.core,
        recheck::Rechecked::kept_as_recorded().join(", ")
    );
    if !skipped.is_empty() {
        println!("Not read (over {MAX_FILE} bytes): {}.", skipped.join(", "));
    }
    println!();
    print!("{}", recheck::report(&checked.drift));
    println!();
    print!("{}", delete_list(&checked.recording, &list));
    let mut others = Vec::new();
    for (label, dir) in &also {
        let (mut t, mut skipped_there) = (Tree::new(), Vec::new());
        if let Err(e) = walk(Path::new(dir), Path::new(dir), &mut t, &mut skipped_there) {
            eprintln!("route-pins-recheck: reading {dir}: {e}");
            return ExitCode::from(2);
        }
        others.push((label.as_str(), t));
    }
    let others: Vec<(&str, &Tree)> = others.iter().map(|(l, t)| (*l, t)).collect();
    let deleted: std::collections::BTreeSet<String> = list
        .iter()
        .filter(|a| {
            matches!(
                a.verdict,
                Verdict::DeleteFile | Verdict::DeleteFileEditTests { .. }
            )
        })
        .map(|a| a.file.clone())
        .collect();
    println!();
    println!("## Core server files no production entry reaches");
    println!();
    println!(
        "Static require graph from `{}`; a file is dead only when no non-test file in core{} names its stem.",
        reach::ENTRY,
        others
            .iter()
            .map(|(l, _)| format!(", {l}"))
            .collect::<String>()
    );
    println!();
    let now = reach::unreached(&tree, &Default::default(), &others);
    println!("Today:");
    println!();
    print!("{}", reach::report(&now));
    println!();
    println!("Stranded by the {} file deletions above:", deleted.len());
    println!();
    let today: std::collections::BTreeSet<&str> = now.iter().map(|u| u.file.as_str()).collect();
    let stranded: Vec<reach::Unreached> = reach::unreached(&tree, &deleted, &others)
        .into_iter()
        .filter(|u| !today.contains(u.file.as_str()))
        .collect();
    print!("{}", reach::report(&stranded));
    if checked.drift.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
