//! Recheck the recording's file facts against a newer core tree without Node (council p10).
//!
//! The recorder (tools/gen-node-refusal-stubs.cjs) needs Node to probe the stubs, but most of what decides a delete
//! verdict is plain text in core: who requires a stub (`importers`), who names it in a string (`string_refs`) or in
//! prose (`mentions`), what index.cjs takes from it (`index_uses`), and whether index.cjs still calls its refusal at
//! all. Core main moves every hour while the deletion train waits; this module re-reads those facts from a core tree
//! with the recorder's own rules, reports every difference ([`Drift`]), and gives a recording with the new facts, so
//! [`crate::classify`] and [`crate::delete_list`] can be recomputed at the train's core SHA.
//!
//! `behind` (core lines outside the stubs that still name a refused route: the "delete with handler" facts) is
//! re-read too, with the recorder's piece matcher. `behind_methods` (the methods a hit line tests) is kept only for a
//! stub whose hits did not move; otherwise it is dropped, so every hit line counts for every method (the safe side,
//! as [`crate::handlers_behind`] documents).
//!
//! What it cannot recheck stays as recorded and is named in [`Rechecked::kept_as_recorded`]: the probed refusals
//! (they need Node), `pairing` (noevia's supervisor and assembler lines) and `owned_routes`. A changed refusal still needs a re-record; this check only
//! makes a stale *file* fact visible, the commonest way a delete list goes wrong (sbstndalton/noevia#2725 item 1).
//!
//! No I/O: the caller passes the tree as `path → text` (bin/route-pins-recheck.rs reads a directory).

use crate::{Recording, Stub};
use std::collections::{BTreeMap, BTreeSet};

/// A core tree: core-relative path (`server/x.cjs`) → file text. Callers leave out `node_modules/` and `.git/`, as
/// the recorder's `grep --exclude-dir` does.
pub type Tree = BTreeMap<String, String>;

/// One fact that differs between the recording and the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// The stub file is gone from core: nothing left to delete (another train removed it).
    Gone { file: String },
    /// index.cjs no longer calls the stub's refusal function: the stub is no longer in the request chain, so its
    /// recorded refusals say nothing about today's core (re-record before trusting its verdict).
    NotCalled { file: String, function: String },
    /// A list fact changed: `field` is `importers`, `string_refs`, `mentions`, `index_uses` or `behind` (as
    /// `pattern <- file`: a line number moving inside a file is not drift).
    Changed {
        file: String,
        field: &'static str,
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// The file's line count changed (the stub itself was edited: re-record before trusting its refusals).
    Lines {
        file: String,
        recorded: usize,
        now: usize,
    },
}

impl Drift {
    /// Whether this drift means the recorded refusals may be stale (re-record), rather than only a file fact moved.
    pub fn needs_rerecord(&self) -> bool {
        matches!(self, Drift::NotCalled { .. } | Drift::Lines { .. })
    }
}

/// The result of [`recheck`].
#[derive(Debug, Clone)]
pub struct Rechecked {
    /// The recording with the tree's file facts; stubs gone from the tree are dropped.
    pub recording: Recording,
    pub drift: Vec<Drift>,
}

impl Rechecked {
    /// The stub facts this check leaves as recorded.
    pub fn kept_as_recorded() -> &'static [&'static str] {
        &["refusals", "switched", "env", "pairing", "owned_routes"]
    }
}

/// The file's name after its last `/`.
fn basename(file: &str) -> &str {
    file.rsplit('/').next().unwrap_or(file)
}

/// The stub's path as index.cjs requires it (`./rust-x.cjs`, `./routes/x.cjs`).
fn require_spec(file: &str) -> String {
    format!("./{}", file.strip_prefix("server/").unwrap_or(file))
}

/// JavaScript's `\w` without the `u` flag, plus the recorder's `./-`.
fn path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-')
}

fn quote(c: char) -> bool {
    matches!(c, '`' | '\'' | '"')
}

/// The recorder's `` [`'"][\w./-]*NAME[`'"] ``: `name` inside a quoted path string.
pub fn quoted_ref(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    text.match_indices(name).any(|(i, _)| {
        let after = text.get(i + name.len()..).and_then(|t| t.chars().next());
        if !after.is_some_and(quote) {
            return false;
        }
        // The quote must sit just before the run of path characters that ends at `name` (a quote is not a path
        // character, so no other start is possible).
        let before = text.get(..i).unwrap_or("");
        before
            .chars()
            .rev()
            .find(|c| !path_char(*c))
            .is_some_and(quote)
    })
}

/// The recorder's `grep -rlF ./NAME server --include=*.cjs`: every `.cjs` file under `server/` containing
/// `./NAME`, sorted (the stub itself included when it names itself, as grep does).
pub fn importers(tree: &Tree, file: &str) -> Vec<String> {
    let needle = format!("./{}", basename(file));
    tree.iter()
        .filter(|(p, t)| p.starts_with("server/") && p.ends_with(".cjs") && t.contains(&needle))
        .map(|(p, _)| p.clone())
        .collect()
}

/// The recorder's mention scan: every file naming the stub by file name, minus the stub and its importers, split into
/// quoted string references and prose mentions.
pub fn refs_and_mentions(
    tree: &Tree,
    file: &str,
    importers: &[String],
) -> (Vec<String>, Vec<String>) {
    let name = basename(file);
    let mut refs = Vec::new();
    let mut mentions = Vec::new();
    for (p, t) in tree {
        if p == file || importers.contains(p) || !t.contains(name) {
            continue;
        }
        if quoted_ref(t, name) {
            refs.push(p.clone());
        } else {
            mentions.push(p.clone());
        }
    }
    (refs, mentions)
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The declaration in front of a `require(…)` call at byte `at` of `index`, as the recorder's
/// `(?:const|let|var)\s+(\{[^}]*\}|\w+)\s*=\s*require(…)` captures it: the destructuring braces or the bound name.
fn declared_before(index: &str, at: usize) -> Option<&str> {
    let before = index.get(..at)?.trim_end_matches(char::is_whitespace);
    let before = before
        .strip_suffix('=')?
        .trim_end_matches(char::is_whitespace);
    let (start, binding) = if before.ends_with('}') {
        let open = before.rfind('{')?;
        let inner = before.get(open..)?;
        // `\{[^}]*\}`: no closing brace inside.
        if inner.get(..inner.len().checked_sub(1)?)?.contains('}') {
            return None;
        }
        (open, inner)
    } else {
        let start = before
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_word(*c))
            .last()
            .map(|(i, _)| i)?;
        (start, before.get(start..)?)
    };
    let lead = before.get(..start)?;
    let keyword = lead.trim_end_matches(char::is_whitespace);
    if keyword.len() == lead.len() {
        return None; // `\s+` needs at least one space.
    }
    ["const", "let", "var"]
        .iter()
        .any(|k| keyword.ends_with(k))
        .then_some(binding)
}

/// The recorder's `indexUses(file)`: what index.cjs takes from the stub (`require('./x').name`, destructured or
/// bound requires; a bare `<module>` only when nothing else was named), sorted.
pub fn index_uses(index: &str, file: &str) -> Vec<String> {
    let call = format!("require('{}')", require_spec(file));
    let mut uses = BTreeSet::new();
    for (at, _) in index.match_indices(&call) {
        let rest = index.get(at + call.len()..).unwrap_or("");
        let named: String = rest
            .strip_prefix('.')
            .map(|r| r.chars().take_while(|c| is_word(*c)).collect())
            .unwrap_or_default();
        uses.insert(if named.is_empty() {
            "<module>".to_string()
        } else {
            named
        });
        // The declaration form ends the statement right after the call: `\s*[;\n]`.
        let tail = rest.trim_start_matches(|c: char| c.is_whitespace() && c != '\n');
        if !(tail.starts_with(';') || tail.starts_with('\n')) {
            continue;
        }
        if let Some(binding) = declared_before(index, at) {
            for part in binding.replace(['{', '}'], "").split(',') {
                let n = part.trim().split(':').next().unwrap_or("").trim();
                if !n.is_empty() {
                    uses.insert(n.to_string());
                }
            }
        }
    }
    if uses.len() > 1 {
        uses.remove("<module>");
    }
    uses.into_iter().collect()
}

/// Whether index.cjs still calls the stub's refusal: `require('./x').fn(`.
pub fn still_called(index: &str, stub: &Stub) -> bool {
    index.contains(&format!(
        "require('{}').{}(",
        require_spec(&stub.file),
        stub.function
    ))
}

/// The recorder's `lines(f)`: `split('\n').length`.
pub fn line_count(text: &str) -> usize {
    text.split('\n').count()
}

/// The recorder's handler files: `server/**/*.cjs` outside `node_modules/` and `fixtures/` directories, not a
/// `.test.cjs`, not one of the recorded stubs. Each as its lines with a regex literal's `\/` read as `/`.
fn handler_lines<'a>(tree: &'a Tree, stubs: &BTreeSet<&str>) -> Vec<(&'a str, Vec<String>)> {
    tree.iter()
        .filter(|(p, _)| {
            p.starts_with("server/")
                && p.ends_with(".cjs")
                && !p.ends_with(".test.cjs")
                && !stubs.contains(p.as_str())
                && !p
                    .split('/')
                    .rev()
                    .skip(1)
                    .any(|d| d == "node_modules" || d == "fixtures")
        })
        .map(|(p, t)| {
            (
                p.as_str(),
                t.split('\n').map(|l| l.replace("\\/", "/")).collect(),
            )
        })
        .collect()
}

/// The literal pieces of a routes.toml pattern between its `{…}` parameters, without empty pieces and bare `/`
/// (the recorder's `pattern.split(/\{[^}]*\}/).filter((x) => x && x !== '/')`).
pub fn pattern_pieces(pattern: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = pattern;
    loop {
        let split = rest
            .find('{')
            .and_then(|open| Some((open, open + rest.get(open..)?.find('}')?)));
        let Some((open, close)) = split else {
            out.push(rest);
            break;
        };
        out.push(rest.get(..open).unwrap_or(""));
        rest = rest.get(close + 1..).unwrap_or("");
    }
    out.retain(|p| !p.is_empty() && *p != "/");
    out
}

/// Whether one line names `pieces` in order (the recorder's `behind` line test). An `exact` pattern (one piece, no
/// parameter) must not continue as a longer literal: `/api/x` is not `/api/xy`.
fn names_route(line: &str, pieces: &[&str], exact: bool) -> bool {
    let t = line.trim();
    if t.starts_with("//") || t.starts_with('*') || line.contains("require('./rust-") {
        return false;
    }
    let mut from = 0;
    for piece in pieces {
        let Some(at) = line
            .get(from..)
            .and_then(|l| l.find(piece))
            .map(|i| i + from)
        else {
            return false;
        };
        let end = at + piece.len();
        let next = line.get(end..).and_then(|l| l.chars().next());
        if exact && next.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return false;
        }
        from = end;
    }
    true
}

/// The recorder's `behind(patterns)`: per refused pattern, the `file:line` hits in core's handler files (by file,
/// then line; a pattern without hits is left out).
pub fn behind(
    tree: &Tree,
    stubs: &BTreeSet<&str>,
    patterns: &[String],
) -> BTreeMap<String, Vec<String>> {
    let files = handler_lines(tree, stubs);
    let mut out = BTreeMap::new();
    for pattern in patterns {
        let pieces = pattern_pieces(pattern);
        if pieces.is_empty() {
            continue;
        }
        let exact = pieces.len() == 1 && !pattern.contains('{');
        let mut hits = Vec::new();
        for (f, lines) in &files {
            for (i, l) in lines.iter().enumerate() {
                if names_route(l, &pieces, exact) {
                    hits.push(format!("{f}:{}", i + 1));
                }
            }
        }
        if !hits.is_empty() {
            out.insert(pattern.clone(), hits);
        }
    }
    out
}

/// `behind` as sets of hits per pattern (the recorder lists them in directory order; the verdicts read them as a set).
fn hit_sets(b: &BTreeMap<String, Vec<String>>) -> BTreeMap<&str, BTreeSet<&str>> {
    b.iter()
        .map(|(p, h)| (p.as_str(), h.iter().map(String::as_str).collect()))
        .collect()
}

/// `behind` as `pattern <- file` facts, for drift that ignores line numbers.
fn behind_files(b: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let set: BTreeSet<String> = b
        .iter()
        .flat_map(|(p, hits)| {
            hits.iter().map(move |h| {
                let file = h.rsplit_once(':').map_or(h.as_str(), |(f, _)| f);
                format!("{p} <- {file}")
            })
        })
        .collect();
    set.into_iter().collect()
}

fn changed(file: &str, field: &'static str, was: &[String], now: &[String]) -> Option<Drift> {
    let added: Vec<String> = now.iter().filter(|x| !was.contains(x)).cloned().collect();
    let removed: Vec<String> = was.iter().filter(|x| !now.contains(x)).cloned().collect();
    (!added.is_empty() || !removed.is_empty()).then(|| Drift::Changed {
        file: file.to_string(),
        field,
        added,
        removed,
    })
}

/// Re-reads every stub's file facts from `tree` (see the module docs). `tree` must hold `server/index.cjs`.
pub fn recheck(rec: &Recording, tree: &Tree) -> Result<Rechecked, String> {
    let index = tree
        .get("server/index.cjs")
        .ok_or("the tree has no server/index.cjs (not a core checkout?)")?;
    let mut out = rec.clone();
    let mut drift = Vec::new();
    out.stubs.clear();
    let stub_files: BTreeSet<&str> = rec.stubs.iter().map(|s| s.file.as_str()).collect();
    for s in &rec.stubs {
        let Some(text) = tree.get(&s.file) else {
            drift.push(Drift::Gone {
                file: s.file.clone(),
            });
            continue;
        };
        let mut n = s.clone();
        if !still_called(index, s) {
            drift.push(Drift::NotCalled {
                file: s.file.clone(),
                function: s.function.clone(),
            });
        }
        let lines = line_count(text);
        if lines != s.lines {
            drift.push(Drift::Lines {
                file: s.file.clone(),
                recorded: s.lines,
                now: lines,
            });
        }
        n.lines = lines;
        n.importers = importers(tree, &s.file);
        let (refs, mentions) = refs_and_mentions(tree, &s.file, &n.importers);
        n.string_refs = refs;
        n.mentions = mentions;
        n.index_uses = index_uses(index, &s.file);
        let patterns: BTreeSet<String> = s
            .refusals
            .iter()
            .filter_map(|r| r.pattern.clone())
            .collect();
        let patterns: Vec<String> = patterns.into_iter().collect();
        n.behind = behind(tree, &stub_files, &patterns);
        if hit_sets(&n.behind) != hit_sets(&s.behind) {
            // A hit line's method facts belong to its old line number: without them every hit counts (safe side).
            n.behind_methods.clear();
        }
        let (was_behind, now_behind) = (behind_files(&s.behind), behind_files(&n.behind));
        for (field, was, now) in [
            ("importers", &s.importers, &n.importers),
            ("string_refs", &s.string_refs, &n.string_refs),
            ("mentions", &s.mentions, &n.mentions),
            ("index_uses", &s.index_uses, &n.index_uses),
            ("behind", &was_behind, &now_behind),
        ] {
            drift.extend(changed(&s.file, field, was, now));
        }
        out.stubs.push(n);
    }
    Ok(Rechecked {
        recording: out,
        drift,
    })
}

/// The drift as Markdown lines for the train's PR body (`none` when the tree agrees with the recording).
pub fn report(drift: &[Drift]) -> String {
    if drift.is_empty() {
        return "No drift: the tree's file facts are the recording's.\n".to_string();
    }
    let mut out = String::new();
    for d in drift {
        let line = match d {
            Drift::Gone { file } => format!("- `{file}`: gone from core"),
            Drift::NotCalled { file, function } => {
                format!("- `{file}`: index.cjs no longer calls `{function}` (re-record)")
            }
            Drift::Lines {
                file,
                recorded,
                now,
            } => format!("- `{file}`: {recorded} → {now} lines (edited; re-record)"),
            Drift::Changed {
                file,
                field,
                added,
                removed,
            } => format!("- `{file}` {field}: +{added:?} −{removed:?}"),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
#[path = "recheck_tests.rs"]
mod tests;
