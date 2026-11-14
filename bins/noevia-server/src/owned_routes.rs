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
//!
//! The list is true only for the binary that prints it (sbstndalton/noevia#2725 item 2): the route table is
//! `routes::compiled()`, which depends on the build's cargo features, so a build with fewer `rust-*` features lists
//! fewer pairs. Consumers (the assembler, the supervisor) must run `--owned-routes` on the binary they ship, never
//! read a pinned copy such as crates/route-pins/fixtures/front-owned-routes.txt (that one is the default-feature
//! test build's, and only the route-pins golden reads it).
//!
//! It is route-table ownership, not "Node never sees it" (#2725 item 4): about 30 native handlers still hand some
//! requests on to Node (`app.proxy.forward`) for a path spelling or method they do not serve themselves (a trailing
//! slash, a second segment, a method outside the row's `methods`). [`answers`] sees the route table's method list
//! and the exact-path owners in `routes::dispatch_under`, not a guard inside a handler, so a handler that forwards a
//! method its row lists would be over-claimed here. tests::handlers_that_forward_by_method pins the handlers that
//! guard on a method, with the methods their rows list, so a new guard of that kind fails a test until it is checked.

use crate::{routes, static_files};
use std::collections::BTreeSet;
use std::io::{self, Write};

/// The methods asked of every row (core's recorder asks the same seven).
pub const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// The two parameter fillings: a plain id and an id of the UUID shape some handlers insist on. A row is owned for a
/// method only when both fillings dispatch natively (see [`disagreements`] for the rows where they differ).
pub const FILLS: [&str; 2] = ["qa-1", "00000000-0000-4000-8000-000000000001"];

/// Pairs the route table hands to a native handler that, with every switch off, passes the request on to Node
/// itself (sbstndalton/noevia#2725 item 4): `(METHOD, pattern, why)`. They are left out of the manifest.
pub const HANDLER_FORWARDS: &[(&str, &str, &str)] = &[(
    "POST",
    "/api/models/autotune",
    "autotune_read_routes.rs: the POST is native only with the front's runner (AUTOTUNE_RUNNER_IMPL=rust)",
)];

/// What a `{*}` becomes. The matcher (`routes::pattern_matches`) takes `{*}` as exactly one segment, so the filling
/// is one segment; the project original's `{*}` is `uploads` (project_routes.rs sends any other segment to Node).
pub fn wildcard_fill(pattern: &str) -> &'static str {
    if pattern == routes::PROJECT_ORIGINAL {
        "uploads"
    } else {
        "qa-seg"
    }
}

/// `pattern` with each `{name}` replaced by `id`, `{*}` by one segment ([`wildcard_fill`]) and `{*?}` by nothing
/// (the recorder's filling, tools/gen-node-refusal-stubs.cjs `fill`, plus the id choice). Until council p7 a `{*}`
/// was filled with two segments, which no `{*}` row matches, so no wildcard row was ever listed.
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
            "*" => wildcard_fill(pattern),
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

/// Whether `route` answers `method` natively on its path filled with `id`: the filled path resolves back to the
/// same row (a more specific sibling pattern that captures it owns that request, and is listed under its own
/// pattern), the route table dispatches it natively, and the pair is not one of [`HANDLER_FORWARDS`].
fn owned_with(route: &'static routes::Route, method: &str, id: &str) -> bool {
    let path = fill(route.path, id);
    routes::route_for(&path).is_some_and(|r| std::ptr::eq(r, route))
        && answers(method, &path)
        && !HANDLER_FORWARDS
            .iter()
            .any(|&(m, p, _)| m == method && p == route.path)
}

/// The owned pairs, sorted: `(METHOD, pattern)` for every row and method both fillings own ([`owned_with`]).
pub fn manifest() -> BTreeSet<(&'static str, &'static str)> {
    let mut out = BTreeSet::new();
    for route in rows() {
        for &method in METHODS {
            let owned = FILLS.iter().all(|id| owned_with(route, method, id));
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
            let [a, b] = FILLS.map(|id| owned_with(route, method, id));
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
