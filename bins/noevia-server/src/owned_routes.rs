//! The front's owned-route manifest (`noevia-server --owned-routes`, council p5 for sbstndalton/noevia#2700): every
//! `METHOD pattern` this binary answers natively from the route table (contracts/http/routes.toml) with every
//! deployment switch off. The release assembler and the web supervisor read core's Node refusal stubs
//! (`OWNED_ROUTES` sets in core's server/rust-*.cjs) to learn which routes the front owns; once those stubs are
//! deleted they need this list instead, taken from the binary that does the answering.
//!
//! One line per pair, sorted, no duplicates: `METHOD /api/x/{name}`. The patterns are routes.toml's own; the
//! assembler's `project_routes_core` normalises parameter names with `sed 's/{[^}]*}/{}/g'`, and
//! [`normalised`] gives the same form. Inert: nothing in the server calls this; it runs before the runtime, binds
//! nothing and reads no configuration.

use crate::{routes, static_files};
use std::collections::BTreeSet;
use std::io::{self, Write};

/// The methods asked of every row (core's recorder asks the same seven).
pub const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// The two parameter fillings: a plain id and an id of the UUID shape some handlers insist on. A row is owned for a
/// method only when both fillings dispatch natively (see [`disagreements`] for the rows where they differ).
pub const FILLS: [&str; 2] = ["qa-1", "00000000-0000-4000-8000-000000000001"];

/// `pattern` with each `{name}` replaced by `id`, `{*}` by two segments and `{*?}` by nothing (the recorder's
/// filling, tools/gen-node-refusal-stubs.cjs `fill`, plus the id choice).
pub fn fill(pattern: &str, id: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut rest = pattern;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(len) = rest[start..].find('}') else {
            // An unterminated brace is literal text; routes.toml's schema refuses it anyway.
            out.push_str(&rest[start..]);
            return out;
        };
        out.push_str(match &rest[start + 1..start + len] {
            "*?" => "",
            "*" => "qa-seg/qa-rest",
            _ => id,
        });
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

/// The assembler's normal form of a pattern: every `{…}` becomes `{}`.
pub fn normalised(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut rest = pattern;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        match rest[start..].find('}') {
            Some(len) => {
                out.push_str("{}");
                rest = &rest[start + len + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether the route table answers `method path` natively with every switch off. The bundle (`Static`) counts only
/// for its canonical paths, exactly as lib.rs `handle` serves it; anything else `dispatch` names is answered here.
pub fn answers(method: &str, path: &str) -> bool {
    match routes::dispatch(method, path) {
        None => false,
        Some(routes::Native::Static) => static_files::canonical(path),
        Some(_) => true,
    }
}

/// The rows the manifest asks: every listed routes.toml row (the non-/api catch-all is the bundle, not a route).
fn rows() -> impl Iterator<Item = &'static routes::Route> {
    routes::ROUTES.iter().filter(|r| !r.catch_all)
}

/// The owned pairs, sorted: `(METHOD, pattern)` for every row and method both fillings dispatch natively. The
/// filled path must also resolve back to the same row (a more specific sibling pattern that captures the filled
/// path owns that request, and is listed under its own pattern).
pub fn manifest() -> BTreeSet<(&'static str, &'static str)> {
    let mut out = BTreeSet::new();
    for route in rows() {
        for &method in METHODS {
            let owned = FILLS.iter().all(|id| {
                let path = fill(route.path, id);
                routes::route_for(&path).is_some_and(|r| std::ptr::eq(r, route)) && answers(method, &path)
            });
            if owned {
                out.insert((method, route.path));
            }
        }
    }
    out
}

/// Rows where the two fillings disagree for a method: `(METHOD, pattern, answered with qa-1, answered with a
/// UUID)`. A non-empty list means the manifest under-claims those pairs (it lists only what both answer).
pub fn disagreements() -> Vec<(&'static str, &'static str, bool, bool)> {
    let mut out = Vec::new();
    for route in rows() {
        for &method in METHODS {
            let [a, b] = FILLS.map(|id| {
                let path = fill(route.path, id);
                routes::route_for(&path).is_some_and(|r| std::ptr::eq(r, route)) && answers(method, &path)
            });
            if a != b {
                out.push((method, route.path, a, b));
            }
        }
    }
    out
}

/// Writes the manifest, one `METHOD pattern` per line.
pub fn write(writer: &mut impl Write) -> io::Result<()> {
    for (method, pattern) in manifest() {
        writeln!(writer, "{method} {pattern}")?;
    }
    writer.flush()
}

#[cfg(test)]
#[path = "owned_routes_tests.rs"]
mod tests;
