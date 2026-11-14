//! `route-pins-train --core DIR [--manifest FILE] [--noevia DIR] [--stage plain|edit-tests]... [--apply]`: the deletion
//! train's core edits (route_pins::train) for the delete list rechecked at DIR, as Markdown on stdout.
//!
//! It rechecks the recording's file facts at DIR (route_pins::recheck), classifies with the `--manifest` front
//! (`noevia-server --owned-routes` of the binary the train ships; without it every refusal counts as answered), plans
//! every file verdict, makes the chosen stages' edits in memory (default: stage 1, the plain deletions) and checks the
//! result: no production line names a deleted file, no test still requires one, nothing newly unreached. With
//! `--noevia DIR` it lists each noevia line naming a planned file by kind (the pairing blocks the same train edits by
//! hand). Only with `--apply`, and only when the dry run is clean (no drift needing a re-record, every planned stub
//! mechanical, no problem), it deletes the files in DIR and rewrites DIR's server/index.cjs: point it at the train's
//! own worktree, never a shared checkout.
//!
//! Exit codes: 0 clean (and applied with `--apply`), 1 not clean (drift needing a re-record, a plan that is not
//! mechanical, or a post-condition problem; nothing written), 2 usage or I/O error. Inert: nothing calls it.

use route_pins::recheck::{self, Tree};
use route_pins::train::{self, Problem, SiteKind, Stage};
use route_pins::{classify, embedded, Manifest};
use std::path::Path;
use std::process::ExitCode;

/// As route-pins-recheck: the recorder's `--exclude-dir` set plus build output, and no file over 8 MiB.
const SKIP_DIRS: &[&str] = &["node_modules", ".git", "target"];
const MAX_FILE: u64 = 8 * 1024 * 1024;

fn walk(root: &Path, dir: &Path, tree: &mut Tree) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let kind = e.file_type()?;
        let name = e.file_name().to_string_lossy().into_owned();
        if kind.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                walk(root, &path, tree)?;
            }
            continue;
        }
        if !kind.is_file() || e.metadata()?.len() > MAX_FILE {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or(name);
        let bytes = std::fs::read(&path)?;
        tree.insert(rel, String::from_utf8_lossy(&bytes).into_owned());
    }
    Ok(())
}

fn read_tree(dir: &str) -> Result<Tree, String> {
    let root = Path::new(dir);
    let mut tree = Tree::new();
    walk(root, root, &mut tree).map_err(|e| format!("reading {dir}: {e}"))?;
    Ok(tree)
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: route-pins-train --core DIR [--manifest FILE] [--noevia DIR] [--stage plain|edit-tests]... [--apply]"
    );
    ExitCode::from(2)
}

fn kind_label(k: &SiteKind) -> &'static str {
    match k {
        SiteKind::OwnerProbe => "owner probe",
        SiteKind::FileRead => "file read",
        SiteKind::TableRow => "table row",
        SiteKind::Fixture => "fixture",
        SiteKind::Comment => "comment",
        SiteKind::Other => "other",
    }
}

fn problem_line(p: &Problem) -> String {
    match p {
        Problem::StillNamed { file, at } => format!("- `{file}` is still named at {at}"),
        Problem::TestStillNames { file, test } => {
            format!("- `{file}` is still required or named by the test {test} (edit it in the same step)")
        }
        Problem::Stranded { file } => {
            format!("- `{file}` is no longer reached from server/index.cjs")
        }
    }
}

fn main() -> ExitCode {
    let (mut core, mut manifest, mut noevia) = (None, None, None);
    let (mut stages, mut write) = (Vec::new(), false);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--core" => core = args.next(),
            "--manifest" => manifest = args.next(),
            "--noevia" => noevia = args.next(),
            "--stage" => match args.next().as_deref() {
                Some("plain") => stages.push(Stage::Plain),
                Some("edit-tests") => stages.push(Stage::EditTests),
                _ => return usage(),
            },
            "--apply" => write = true,
            _ => return usage(),
        }
    }
    let Some(core) = core else {
        return usage();
    };
    if stages.is_empty() {
        stages.push(Stage::Plain);
    }
    let fail = |e: String| {
        eprintln!("route-pins-train: {e}");
        ExitCode::from(2)
    };
    let tree = match read_tree(&core) {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    let noevia_tree = match noevia.as_deref().map(read_tree).transpose() {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    let rec = match embedded() {
        Ok(r) => r,
        Err(e) => return fail(format!("the compiled-in recording: {e}")),
    };
    let checked = match recheck::recheck(&rec, &tree) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let list = match &manifest {
        Some(file) => {
            let m = match std::fs::read_to_string(file)
                .map_err(|e| e.to_string())
                .and_then(|t| Manifest::parse(&t))
            {
                Ok(m) => m,
                Err(e) => return fail(format!("manifest {file}: {e}")),
            };
            classify(&checked.recording, &m.front(&checked.recording))
        }
        None => classify(&checked.recording, &|_: &str, _: &str| true),
    };
    let plans = train::plan(&tree, &list);
    let mut clean = true;

    println!("## Deletion train against {core}");
    println!();
    println!(
        "Recording: core `{}`. Front: {}. Stages: {}.",
        rec.core,
        manifest
            .as_deref()
            .map(|m| format!("`{m}`"))
            .unwrap_or_else(|| "every refusal answered (no --manifest)".to_string()),
        stages
            .iter()
            .map(Stage::label)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();
    let stale: Vec<_> = checked
        .drift
        .iter()
        .filter(|d| d.needs_rerecord())
        .collect();
    if !stale.is_empty() {
        clean = false;
        println!(
            "**{} drift entries need a re-record first** (route-pins-recheck lists them).",
            stale.len()
        );
        println!();
    }
    print!("{}", train::report(&plans, noevia_tree.as_ref()));
    if let Some(n) = &noevia_tree {
        println!();
        println!("### noevia lines naming the planned files (edit in the same step)");
        for p in plans.iter().filter(|p| stages.contains(&p.stage)) {
            let sites = train::pairing_sites(n, &p.file);
            if sites.is_empty() {
                continue;
            }
            println!();
            println!("`{}`:", p.file);
            for s in sites {
                println!(
                    "- {}:{} ({}): `{}`",
                    s.path,
                    s.line,
                    kind_label(&s.kind),
                    s.text
                );
            }
        }
    }
    println!();
    println!("### Dry run");
    println!();
    let after = match train::apply(&tree, &plans, &stages) {
        Ok(a) => a,
        Err(e) => {
            println!("Refused: {e}");
            return ExitCode::from(1);
        }
    };
    let chosen: Vec<&train::CorePlan> =
        plans.iter().filter(|p| stages.contains(&p.stage)).collect();
    let deleted: Vec<String> = chosen.iter().map(|p| p.file.clone()).collect();
    let problems = train::check(&tree, &after, &deleted);
    println!(
        "{} stubs, {} files deleted, {} index.cjs lines removed; {} problems.",
        chosen.len(),
        chosen.iter().map(|p| p.delete.len()).sum::<usize>(),
        chosen.iter().map(|p| p.index_lines.len()).sum::<usize>(),
        problems.len()
    );
    for p in &problems {
        println!("{}", problem_line(p));
    }
    if !problems.is_empty() {
        clean = false;
    }
    if !clean {
        if write {
            println!();
            println!("Not applied: the dry run is not clean.");
        }
        return ExitCode::from(1);
    }
    if write {
        let root = Path::new(&core);
        for p in &chosen {
            for f in &p.delete {
                if let Err(e) = std::fs::remove_file(root.join(f)) {
                    return fail(format!("removing {f}: {e}"));
                }
            }
        }
        if let Some(index) = after.get(train::INDEX) {
            if let Err(e) = std::fs::write(root.join(train::INDEX), index) {
                return fail(format!("writing {}: {e}", train::INDEX));
            }
        }
        println!();
        println!("Applied to {core}.");
    }
    ExitCode::SUCCESS
}
