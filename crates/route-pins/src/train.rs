//! The deletion train's edits (council p15, pool item 8): what exactly changes in core and in noevia when the
//! delete list's file deletions are made, computed from the trees instead of by hand.
//!
//! [`plan`] turns each file verdict of [`crate::classify`] (rechecked at the train's core with [`crate::recheck`])
//! into a [`CorePlan`]: the files to delete (the stub and its own tests), the one-line guards in `server/index.cjs`
//! to remove (`if (require('./rust-x.cjs').refuseOwned(req, res, {…})) return;`, the only shape removed
//! mechanically), and every other production line that still names the stub ([`CorePlan::manual`]: a plan with any
//! is not mechanical and [`apply`] refuses it). Stage 1 is the plain deletions, stage 2 the ones whose foreign tests
//! must be edited by hand in the same train, and Diary stubs are held while the storage move holds Diary work.
//! "Delete function" and "delete with handler" verdicts are not file deletions and are not planned here.
//!
//! [`apply`] makes the stage's core edits on a tree in memory (it checks each guard line's text first, so a plan
//! made at another SHA fails instead of removing the wrong line), and [`check`] is the post-condition: no
//! production line names a deleted file, and no server file is newly unreached ([`crate::reach`]).
//!
//! The noevia half is not mechanical: the supervisor's and assembler's pairing blocks decide each owner from a
//! `grep -qF "require('./rust-x.cjs')"` probe of core's index.cjs, so after the deletion core reads as the old owner
//! and the block refuses a front that still names the capability. [`pairing_sites`] lists each noevia line naming
//! the file with its [`SiteKind`], so the train edits every probe, file read and table row in the same step and the
//! fixtures in deploy/tools/test-*.sh with them.
//!
//! No I/O and no production caller (inert): `route-pins-train` (src/bin) reads the directories and prints the plan.

use crate::recheck::{quoted_ref, Tree};
use crate::{reach, Assessment, Verdict};
use std::collections::BTreeSet;

/// Core's request chain: the file whose guard lines a deletion removes.
pub const INDEX: &str = "server/index.cjs";

/// Which part of the train a deletion belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// "delete file": the stub, its own tests and its index.cjs guard; nothing else in core names it.
    Plain,
    /// "delete file, edit tests": as [`Stage::Plain`], plus the listed foreign tests edited by hand in the same step.
    EditTests,
    /// Not in this train, with the reason (Diary stubs while the storage move holds Diary work).
    Held(&'static str),
}

impl Stage {
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Plain => "1 (plain)",
            Stage::EditTests => "2 (edit tests)",
            Stage::Held(_) => "held",
        }
    }
}

/// One line of a file, 1-based, with its exact text (no line end).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub line: usize,
    pub text: String,
}

/// The core edits for one stub file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorePlan {
    pub file: String,
    pub function: String,
    pub stage: Stage,
    /// Files removed: the stub, then its own tests present in the tree, sorted.
    pub delete: Vec<String>,
    /// The index.cjs guard lines removed.
    pub index_lines: Vec<Line>,
    /// Production lines (`path:line: text`) that still name the stub in a quoted path and are not a removable guard.
    /// Non-empty = the plan is not mechanical (re-record: the verdict was made on other facts).
    pub manual: Vec<String>,
    /// Stage 2: the tests outside the stub's own that require or name it (edit by hand, same step).
    pub foreign_tests: Vec<String>,
}

impl CorePlan {
    /// Whether [`apply`] may make this plan's edits: in the train, a guard found, nothing manual.
    pub fn mechanical(&self) -> bool {
        !matches!(self.stage, Stage::Held(_))
            && !self.index_lines.is_empty()
            && self.manual.is_empty()
    }
}

fn basename(file: &str) -> &str {
    file.rsplit('/').next().unwrap_or(file)
}

/// The stub's path as index.cjs requires it (`./rust-x.cjs`).
fn spec(file: &str) -> String {
    format!("./{}", file.strip_prefix("server/").unwrap_or(file))
}

/// The library's own-test rule (`rust-x.test.cjs`, `rust-x.boot.test.cjs` for `rust-x.cjs`): the test's file name is
/// the stub's stem followed by a dot.
fn own_test(file: &str, test: &str) -> bool {
    let stem = basename(file);
    let stem = stem.strip_suffix(".cjs").unwrap_or(stem);
    basename(test)
        .strip_prefix(stem)
        .is_some_and(|rest| rest.starts_with('.'))
}

/// A line that is only a comment (`//`, `/*`, ` * `), as the recorder's prose rule reads it.
fn comment_line(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("//") || t.starts_with("/*") || t.starts_with('*')
}

/// Whether `text` is exactly one refusal guard of `file`'s `function`: `if (require('SPEC').FUNCTION(req, res, ARGS))
/// return;` with nothing else on the line, ARGS being `{ … }` or `p, json`, and no second `require(`.
pub fn is_guard(text: &str, file: &str, function: &str) -> bool {
    let t = text.trim();
    let head = format!("if (require('{}').{}(req, res, ", spec(file), function);
    let Some(rest) = t.strip_prefix(&head) else {
        return false;
    };
    let Some(args) = rest.strip_suffix(")) return;") else {
        return false;
    };
    let args_ok = (args.starts_with('{') && args.ends_with('}') && !args.contains(['(', ')']))
        || args == "p, json";
    args_ok && !rest.contains("require(")
}

fn is_test_or_doc(path: &str) -> bool {
    reach::is_test(path)
        || path.ends_with(".md")
        || path.starts_with("docs/")
        || path.contains("/fixtures/")
}

/// Every production line of `tree` (not the stub, not tests, fixtures or docs, not a comment-only line) that names
/// `file` in a quoted path (the recorder's string-reference rule), as `(path, line, text)`.
fn naming_lines<'a>(
    tree: &'a Tree,
    file: &'a str,
) -> impl Iterator<Item = (&'a str, usize, &'a str)> + 'a {
    let name = basename(file);
    tree.iter()
        .filter(move |(p, t)| p.as_str() != file && !is_test_or_doc(p) && t.contains(name))
        .flat_map(move |(p, t)| {
            t.lines()
                .enumerate()
                .filter(move |(_, l)| !comment_line(l) && quoted_ref(l, name))
                .map(move |(i, l)| (p.as_str(), i + 1, l))
        })
}

/// The train's core plans for the file verdicts in `list`, in list order. Other verdicts are left out.
pub fn plan(tree: &Tree, list: &[Assessment]) -> Vec<CorePlan> {
    let mut out = Vec::new();
    for a in list {
        let (stage, foreign_tests) = match &a.verdict {
            Verdict::DeleteFile => (Stage::Plain, Vec::new()),
            Verdict::DeleteFileEditTests { tests } => (Stage::EditTests, tests.clone()),
            _ => continue,
        };
        let stage = if a.file.contains("diary") {
            Stage::Held("Diary: the storage move holds Diary work")
        } else {
            stage
        };
        let mut delete: Vec<String> = Vec::new();
        if tree.contains_key(&a.file) {
            delete.push(a.file.clone());
        }
        let dir = a.file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let mut own: Vec<String> = tree
            .keys()
            .filter(|p| {
                p.rsplit_once('/').map(|(d, _)| d).unwrap_or("") == dir
                    && **p != a.file
                    && reach::is_test(p)
                    && own_test(&a.file, p)
            })
            .cloned()
            .collect();
        own.sort();
        delete.extend(own);
        let mut index_lines = Vec::new();
        let mut manual = Vec::new();
        for (path, line, text) in naming_lines(tree, &a.file) {
            if path == INDEX && is_guard(text, &a.file, &a.function) {
                index_lines.push(Line {
                    line,
                    text: text.to_string(),
                });
            } else if !delete.iter().any(|d| d == path) {
                manual.push(format!("{path}:{line}: {}", text.trim()));
            }
        }
        out.push(CorePlan {
            file: a.file.clone(),
            function: a.function.clone(),
            stage,
            delete,
            index_lines,
            manual,
            foreign_tests,
        });
    }
    out
}

/// Makes the core edits of every plan in `stages` on a copy of `tree`. Refuses (naming the plan) when a plan in those
/// stages is not [`CorePlan::mechanical`], a file to delete is missing, or a guard line's text differs from the tree.
/// Stage 2 plans need their foreign tests edited too: [`apply`] does not touch them, [`check`] names them.
pub fn apply(tree: &Tree, plans: &[CorePlan], stages: &[Stage]) -> Result<Tree, String> {
    let mut out = tree.clone();
    let mut drop_lines: Vec<(usize, &str, &str)> = Vec::new();
    for p in plans.iter().filter(|p| stages.contains(&p.stage)) {
        if !p.mechanical() {
            return Err(format!(
                "{}: not mechanical (guards found: {}, manual lines: {})",
                p.file,
                p.index_lines.len(),
                p.manual.len()
            ));
        }
        for f in &p.delete {
            if out.remove(f).is_none() {
                return Err(format!("{}: {f} is not in the tree", p.file));
            }
        }
        for l in &p.index_lines {
            drop_lines.push((l.line, l.text.as_str(), p.file.as_str()));
        }
    }
    if drop_lines.is_empty() {
        return Ok(out);
    }
    let Some(index) = out.get(INDEX) else {
        return Err(format!("{INDEX} is not in the tree"));
    };
    let lines: Vec<&str> = index.split('\n').collect();
    let mut gone = BTreeSet::new();
    for (n, text, file) in drop_lines {
        let at = n.checked_sub(1).and_then(|i| lines.get(i).map(|l| (i, *l)));
        match at {
            Some((i, l)) if l.trim_end_matches('\r') == text.trim_end_matches('\r') => {
                gone.insert(i);
            }
            _ => {
                return Err(format!(
                    "{file}: {INDEX}:{n} is not `{}` in this tree",
                    text.trim()
                ))
            }
        }
    }
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| !gone.contains(i))
        .map(|(_, l)| *l)
        .collect();
    out.insert(INDEX.to_string(), kept.join("\n"));
    Ok(out)
}

/// What is wrong with a tree after [`apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// A production line still names a deleted file in a quoted path.
    StillNamed { file: String, at: String },
    /// A test outside the deleted file's own still requires or names it (stage 2: edit it in the same step).
    TestStillNames { file: String, test: String },
    /// A server source the request graph reached before is unreached now.
    Stranded { file: String },
}

/// The post-condition of a train step: `after` names none of `deleted` (production lines, then tests), and every
/// server file [`reach`] reached in `before` is still reached (or deleted).
pub fn check(before: &Tree, after: &Tree, deleted: &[String]) -> Vec<Problem> {
    let mut out = Vec::new();
    for file in deleted {
        for (path, line, _) in naming_lines(after, file) {
            out.push(Problem::StillNamed {
                file: file.clone(),
                at: format!("{path}:{line}"),
            });
        }
        let name = basename(file);
        for (path, text) in after {
            if reach::is_test(path) && text.lines().any(|l| quoted_ref(l, name)) {
                out.push(Problem::TestStillNames {
                    file: file.clone(),
                    test: path.clone(),
                });
            }
        }
    }
    let none = BTreeSet::new();
    let was = reach::reached(before, &none);
    let now = reach::reached(after, &none);
    for f in was.difference(&now) {
        if after.contains_key(f) {
            out.push(Problem::Stranded { file: f.clone() });
        }
    }
    out
}

/// How a noevia line names a stub file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteKind {
    /// `grep -qF "require('./rust-x.cjs')" …index.cjs`: the pairing block's core-owner probe. After the deletion it
    /// reads the old owner; the block must change in the same step.
    OwnerProbe,
    /// A path into core's server directory (`$tree/core/server/rust-x.cjs`, `[ -f … ]`): the block reads the file.
    FileRead,
    /// A row of the supervisor's table (`rust-x.cjs rust-x words`).
    TableRow,
    /// A line of deploy/tools/test-*.sh: a fixture of the block above; changes with it.
    Fixture,
    /// A shell comment.
    Comment,
    /// Anything else (read by hand).
    Other,
}

/// One noevia line naming a stub file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub path: String,
    pub line: usize,
    pub kind: SiteKind,
    pub text: String,
}

/// Whether `name` occurs in `text` with no file-name character just before or after it (so `rust-x.cjs` does not
/// match inside `rust-x.cjs.bak` or `other-rust-x.cjs`).
fn names_file(text: &str, name: &str) -> bool {
    let part = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
    text.match_indices(name).any(|(i, _)| {
        let before = text.get(..i).and_then(|t| t.chars().next_back());
        let after = text.get(i + name.len()..).and_then(|t| t.chars().next());
        !before.is_some_and(part) && !after.is_some_and(part)
    })
}

/// The kind of a noevia line `text` of `path` that names `name` (the stub's file name).
pub fn site_kind(path: &str, text: &str, name: &str) -> SiteKind {
    let t = text.trim_start();
    if t.starts_with('#') {
        SiteKind::Comment
    } else if path.starts_with("deploy/tools/test-") || path.starts_with("build/test-") {
        SiteKind::Fixture
    } else if t.contains("grep -qF") && t.contains(&format!("require('./{name}')")) {
        SiteKind::OwnerProbe
    } else if t.contains(&format!("/server/{name}")) {
        SiteKind::FileRead
    } else if t.starts_with(name)
        && t.as_bytes().get(name.len()) == Some(&b' ')
        && !t.contains(['$', '(', '"', '\''])
    {
        SiteKind::TableRow
    } else {
        SiteKind::Other
    }
}

/// Every line under noevia's `build/` and `deploy/` naming `file`'s name, by path then line.
pub fn pairing_sites(noevia: &Tree, file: &str) -> Vec<Site> {
    let name = basename(file);
    let mut out = Vec::new();
    for (path, text) in noevia {
        if !(path.starts_with("build/") || path.starts_with("deploy/")) || !text.contains(name) {
            continue;
        }
        for (i, l) in text.lines().enumerate() {
            if names_file(l, name) {
                out.push(Site {
                    path: path.clone(),
                    line: i + 1,
                    kind: site_kind(path, l, name),
                    text: l.trim().to_string(),
                });
            }
        }
    }
    out
}

fn site_counts(sites: &[Site]) -> String {
    let kinds = [
        (SiteKind::OwnerProbe, "probe"),
        (SiteKind::FileRead, "file read"),
        (SiteKind::TableRow, "table row"),
        (SiteKind::Fixture, "fixture"),
        (SiteKind::Comment, "comment"),
        (SiteKind::Other, "other"),
    ];
    let parts: Vec<String> = kinds
        .iter()
        .filter_map(|(k, label)| {
            let n = sites.iter().filter(|s| s.kind == *k).count();
            (n > 0).then(|| format!("{n} {label}"))
        })
        .collect();
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(", ")
    }
}

/// The train plan as Markdown: one row per file deletion, with the noevia site counts when `noevia` is given.
pub fn report(plans: &[CorePlan], noevia: Option<&Tree>) -> String {
    let mut s = String::new();
    let mech = plans.iter().filter(|p| p.mechanical()).count();
    s.push_str(&format!(
        "{} file deletions planned, {mech} mechanical in core.\n\n",
        plans.len()
    ));
    s.push_str(
        "| Stub | Stage | Deletes | index.cjs lines | Manual | Foreign tests | noevia sites |\n",
    );
    s.push_str("|---|---|---|---|---|---|---|\n");
    for p in plans {
        let sites = noevia
            .map(|t| site_counts(&pairing_sites(t, &p.file)))
            .unwrap_or_else(|| "not read".to_string());
        let held = match &p.stage {
            Stage::Held(why) => format!(" ({why})"),
            _ => String::new(),
        };
        s.push_str(&format!(
            "| `{}` | {}{held} | {} | {} | {} | {} | {sites} |\n",
            p.file,
            p.stage.label(),
            p.delete.join(", "),
            p.index_lines
                .iter()
                .map(|l| l.line.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            if p.manual.is_empty() {
                "none".to_string()
            } else {
                p.manual.join("; ")
            },
            if p.foreign_tests.is_empty() {
                "none".to_string()
            } else {
                p.foreign_tests.join(", ")
            },
        ));
    }
    s
}

#[cfg(test)]
#[path = "train_tests.rs"]
mod tests;
