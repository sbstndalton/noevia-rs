//! Core server files no production entry reaches, found without Node (council p12, pool item 8).
//!
//! The delete list ([`crate::delete_list`]) covers the refusal stubs. This is the other half of "which Node files can
//! go": `server/**/*.cjs` files (not tests) that the static `require('./…')` graph from `server/index.cjs` never
//! reaches, after an optional set of files is taken away (the delete list's files, to see what a train would strand).
//!
//! Static requires miss dynamic loads (`require(path.join(…))`, `new Worker(file)`, a Dockerfile `CMD`, a recorder in
//! another repo, a pairing marker grepped by noevia's assembler). So an unreached file is only listed as dead when its
//! stem (file name without `.cjs`) appears as a whole word in **no** non-test file anywhere: not in core outside
//! itself, and not in the other trees passed in (noevia-rs, noevia). A stem that is a common word (`http`) is
//! therefore always kept: the safe direction. Files named only by tests are test support, and a file named by nothing
//! that runs as its own entry (`require.main === module`) is a hand-run script; neither is reported dead.
//!
//! No I/O: trees are `path → text` (see [`crate::recheck::Tree`]).

use crate::recheck::Tree;
use std::collections::BTreeSet;

/// The production entry: what `node server/index.cjs` loads first.
pub const ENTRY: &str = "server/index.cjs";

/// Whether a core-relative path is test code or test data (never a production reacher, never "dead").
pub fn is_test(path: &str) -> bool {
    path.ends_with(".test.cjs")
        || path.starts_with("tests/")
        || path.contains("/tests/")
        || path.contains("/fixtures/")
}

fn server_source(path: &str) -> bool {
    path.starts_with("server/") && path.ends_with(".cjs")
}

/// Every static relative require spec in `text`: `require(` + optional spaces + a quote + `./` or `../` + the spec
/// up to the same quote + optional spaces + `)`. Template literals and computed arguments are dynamic and skipped.
pub fn requires(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("require(") {
        rest = rest.get(at + "require(".len()..).unwrap_or("");
        let arg = rest.trim_start_matches([' ', '\t']);
        let Some(q) = arg.chars().next().filter(|c| *c == '\'' || *c == '"') else {
            continue;
        };
        let body = arg.get(1..).unwrap_or("");
        let Some(end) = body.find(q) else {
            continue;
        };
        let spec = body.get(..end).unwrap_or("");
        let after = body
            .get(end + 1..)
            .unwrap_or("")
            .trim_start_matches([' ', '\t']);
        if after.starts_with(')')
            && (spec.starts_with("./") || spec.starts_with("../"))
            && !spec.contains('\n')
        {
            out.push(spec);
        }
    }
    out
}

/// `a/b/../c/./d` → `a/c/d`; `None` when `..` climbs above the root.
fn normalise(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// Node's resolution of a relative spec from `from`, over the files present: the path itself, then `.cjs`, `.js`,
/// then a directory's `index.cjs`. Files in `gone` count as absent.
pub fn resolve(tree: &Tree, gone: &BTreeSet<String>, from: &str, spec: &str) -> Option<String> {
    let dir = from.rsplit_once('/').map_or("", |(d, _)| d);
    let base = normalise(&format!("{dir}/{spec}"))?;
    [
        base.clone(),
        format!("{base}.cjs"),
        format!("{base}.js"),
        format!("{base}/index.cjs"),
    ]
    .into_iter()
    .find(|c| tree.contains_key(c) && !gone.contains(c))
}

/// Every file the static require graph reaches from [`ENTRY`] (the entry included), with `gone` taken away.
pub fn reached(tree: &Tree, gone: &BTreeSet<String>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    if !tree.contains_key(ENTRY) || gone.contains(ENTRY) {
        return seen;
    }
    let mut stack = vec![ENTRY.to_string()];
    while let Some(file) = stack.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        let Some(text) = tree.get(&file) else {
            continue;
        };
        for spec in requires(text) {
            if let Some(next) = resolve(tree, gone, &file, spec) {
                if !seen.contains(&next) {
                    stack.push(next);
                }
            }
        }
    }
    seen
}

fn stem_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Whether `text` holds `stem` as a whole word, where a word is made of letters, digits, `_` and `-`
/// (`rust-x` does not name `rust-x-y` or `my-rust-x`).
pub fn names_stem(text: &str, stem: &str) -> bool {
    if stem.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(i) = text.get(from..).and_then(|t| t.find(stem)) {
        let at = from + i;
        let end = at + stem.len();
        let before = text.get(..at).and_then(|t| t.chars().next_back());
        let after = text.get(end..).and_then(|t| t.chars().next());
        if !before.is_some_and(stem_char) && !after.is_some_and(stem_char) {
            return true;
        }
        from = at + stem.chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// What an unreached file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Named by no non-test file anywhere and by no test: delete it.
    Dead,
    /// Named only by tests (a helper, oracle or fixture module): delete it together with `tests`, or keep it as test
    /// support; never a production reacher.
    TestSupport { tests: Vec<String> },
    /// Named by a non-test file (`tree:path`): possibly loaded dynamically, a pairing marker or a recorder's input.
    /// Keep until that name goes.
    Named { by: Vec<String> },
    /// Named by nothing, but it runs as its own entry ([`is_script`]): a recorder or tool run by hand
    /// (`node server/x.cjs …`). Not production code; whether its output is still wanted is a person's call.
    Script,
}

/// Whether a file runs as its own entry: a `#!` line, or Node's `require.main === module` test (either spelling or
/// order).
pub fn is_script(text: &str) -> bool {
    text.starts_with("#!")
        || [
            "require.main === module",
            "require.main == module",
            "module === require.main",
            "module == require.main",
        ]
        .iter()
        .any(|t| text.contains(t))
}

/// One `server/**/*.cjs` file the require graph does not reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreached {
    pub file: String,
    pub lines: usize,
    pub kind: Kind,
}

/// The unreached non-test server sources of `core` once `gone` is taken away, each with its [`Kind`]. `others` are
/// the other repos' trees by label (`noevia-rs`, `noevia`), searched for the stem too; tests there count as tests.
pub fn unreached(core: &Tree, gone: &BTreeSet<String>, others: &[(&str, &Tree)]) -> Vec<Unreached> {
    let seen = reached(core, gone);
    let mut out = Vec::new();
    for (file, text) in core {
        if !server_source(file) || is_test(file) || gone.contains(file) || seen.contains(file) {
            continue;
        }
        let stem = file
            .rsplit('/')
            .next()
            .and_then(|n| n.strip_suffix(".cjs"))
            .unwrap_or(file);
        let mut by = Vec::new();
        let mut tests = BTreeSet::new();
        let trees = std::iter::once(("core", core)).chain(others.iter().map(|(l, t)| (*l, *t)));
        for (label, tree) in trees {
            for (p, t) in tree {
                if (label == "core" && (p == file || gone.contains(p))) || !names_stem(t, stem) {
                    continue;
                }
                let at = if label == "core" {
                    p.clone()
                } else {
                    format!("{label}:{p}")
                };
                if is_test(p) {
                    tests.insert(at);
                } else {
                    by.push(at);
                }
            }
        }
        let kind = if !by.is_empty() {
            Kind::Named { by }
        } else if !tests.is_empty() {
            Kind::TestSupport {
                tests: tests.into_iter().collect(),
            }
        } else if is_script(text) {
            Kind::Script
        } else {
            Kind::Dead
        };
        out.push(Unreached {
            file: file.clone(),
            lines: crate::recheck::line_count(text),
            kind,
        });
    }
    out
}

/// The list as Markdown for the train's report.
pub fn report(list: &[Unreached]) -> String {
    let mut s = String::from("| File | Lines | Kind | Named by / tests |\n|---|---:|---|---|\n");
    if list.is_empty() {
        s.push_str("| (none) | | | |\n");
    }
    for u in list {
        let (kind, names) = match &u.kind {
            Kind::Dead => ("dead: delete", String::new()),
            Kind::TestSupport { tests } => ("test support", tests.join(", ")),
            Kind::Named { by } => ("keep (named)", by.join(", ")),
            Kind::Script => ("script (run by hand)", String::new()),
        };
        s.push_str(&format!(
            "| `{}` | {} | {kind} | {names} |\n",
            u.file, u.lines
        ));
    }
    s
}

#[cfg(test)]
#[path = "reach_tests.rs"]
mod tests;
