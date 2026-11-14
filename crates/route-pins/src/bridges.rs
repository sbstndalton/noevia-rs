//! Core's loopback bridges no Rust source calls any more (council p12, pool item 8: "the bridge code that becomes
//! dead once routes are pinned native").
//!
//! The front asks Node for facts and operations over `/internal/<family>/…` loopback paths that core's
//! `rust-*-bridge.cjs` (and similar) handlers answer. When a feature moves native, the front stops dialling that
//! family but core keeps mounting the handler. This module lists, per bridge family, the core server files that
//! serve it and whether any non-test source of the Rust tree names the family at all.
//!
//! The rule is deliberately loose on the "called" side (the safe direction): a family counts as called when any
//! non-test Rust source holds `/internal/<family>` as a whole segment, anywhere, comments included, so a path built
//! with `format!("{base}/internal/<family>/{op}")` or kept in a constant counts. Only a family nothing names is
//! uncalled. A core literal counts as served when it starts right after a quote or backtick on a non-comment line.
//!
//! An uncalled family's handler is dead in production, but deleting it is still a train step: index.cjs mounts it,
//! and noevia's assembler may require the file (pairing checks): the report names the files to read.
//!
//! No I/O: trees are `path → text` (see [`crate::recheck::Tree`]).

use crate::reach::is_test;
use crate::recheck::Tree;
use std::collections::{BTreeMap, BTreeSet};

const PREFIX: &str = "/internal/";

fn path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')
}

fn segment_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// The `/internal/…` literals of one core source: each starts right after `'`, `"` or a backtick and runs over
/// path characters (letters, digits, `_ - . /`), so a template's `${…}` ends it. Lines starting with `//` or `*`
/// are comments and skipped.
pub fn internal_literals(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        let mut from = 0;
        while let Some(i) = line.get(from..).and_then(|l| l.find(PREFIX)) {
            let at = from + i;
            let quoted = line
                .get(..at)
                .and_then(|b| b.chars().next_back())
                .is_some_and(|c| matches!(c, '\'' | '"' | '`'));
            if quoted {
                let rest = line.get(at..).unwrap_or("");
                let len = rest.find(|c: char| !path_char(c)).unwrap_or(rest.len());
                out.extend(rest.get(..len));
            }
            from = at + PREFIX.len();
        }
    }
    out
}

/// The family of an internal path: `/internal/<first segment>` (`/internal/calibration-cancel/facts` →
/// `/internal/calibration-cancel`); `None` for a bare `/internal/`.
pub fn family(path: &str) -> Option<String> {
    let rest = path.strip_prefix(PREFIX)?;
    let seg = rest.split('/').next().unwrap_or("");
    (!seg.is_empty()).then(|| format!("{PREFIX}{seg}"))
}

/// Whether `text` names `family` as a whole segment (`/internal/x` but not `/internal/xy`).
pub fn names_family(text: &str, family: &str) -> bool {
    let mut from = 0;
    while let Some(i) = text.get(from..).and_then(|t| t.find(family)) {
        let end = from + i + family.len();
        if !text
            .get(end..)
            .and_then(|t| t.chars().next())
            .is_some_and(segment_char)
        {
            return true;
        }
        from = end;
    }
    false
}

fn rust_source(path: &str) -> bool {
    path.ends_with(".rs")
        && !is_test(path)
        && !path.ends_with("_tests.rs")
        && !path.ends_with("/tests.rs")
}

/// One bridge family core serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bridge {
    /// `/internal/<family>`.
    pub family: String,
    /// The core server sources (not tests) holding one of its literals.
    pub served_by: Vec<String>,
    /// The Rust sources (not tests) naming the family; empty = uncalled.
    pub callers: Vec<String>,
}

/// Every bridge family in core's non-test `server/**/*.cjs`, with its Rust callers from `rust` (a noevia-rs tree).
pub fn bridges(core: &Tree, rust: &Tree) -> Vec<Bridge> {
    let mut served: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (p, text) in core {
        if !(p.starts_with("server/") && p.ends_with(".cjs")) || is_test(p) {
            continue;
        }
        for lit in internal_literals(text) {
            if let Some(f) = family(lit) {
                served.entry(f).or_default().insert(p.clone());
            }
        }
    }
    served
        .into_iter()
        .map(|(family, files)| Bridge {
            callers: rust
                .iter()
                .filter(|(p, t)| rust_source(p) && names_family(t, &family))
                .map(|(p, _)| p.clone())
                .collect(),
            family,
            served_by: files.into_iter().collect(),
        })
        .collect()
}

/// The uncalled families as Markdown, with `pairing` (`noevia:path` lines naming a serving file, from a noevia
/// tree) so the train sees the assembler checks it must edit.
pub fn report(list: &[Bridge], noevia: Option<&Tree>) -> String {
    let mut s = String::from("| Bridge family | Served by | Named in noevia |\n|---|---|---|\n");
    let uncalled: Vec<&Bridge> = list.iter().filter(|b| b.callers.is_empty()).collect();
    if uncalled.is_empty() {
        s.push_str("| (none) | | |\n");
    }
    for b in uncalled {
        let mut pairing = Vec::new();
        for file in &b.served_by {
            let name = file.rsplit('/').next().unwrap_or(file);
            for (p, t) in noevia.into_iter().flatten() {
                for (i, l) in t.split('\n').enumerate() {
                    if l.contains(name) {
                        pairing.push(format!("{p}:{}", i + 1));
                    }
                }
            }
        }
        let served: Vec<String> = b.served_by.iter().map(|f| format!("`{f}`")).collect();
        s.push_str(&format!(
            "| `{}` | {} | {} |\n",
            b.family,
            served.join(", "),
            pairing.join(", ")
        ));
    }
    s
}

#[cfg(test)]
#[path = "bridges_tests.rs"]
mod tests;
