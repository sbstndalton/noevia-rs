//! Council p2 (Node shims and route tables): core's recorded ownership refusal stubs (crates/route-pins) checked
//! against this front's own dispatch. Test-only: nothing here runs in the shipped server.
//!
//! The verdict list and the pin candidates are a golden file (crates/route-pins/fixtures/front-verdicts.md): a change
//! to routes.toml, the dispatch or the residual answers that makes a stub reachable (or dead) changes it, and the test
//! says so. `ROUTE_PINS_WRITE=1 cargo test -p noevia-server --lib route_pins_check` rewrites it.

#![allow(clippy::indexing_slicing, clippy::expect_used)]

use crate::{residual_fallback, routes, static_files};
use route_pins::{classify, delete_list, PinTable, Recording, Verdict};
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// What this front answers itself for `method path` with every deployment switch off (lib.rs `handle`): the
/// residual answers first, then the route table's native handler (the bundle only for its canonical paths);
/// everything else is forwarded to Node.
fn front_answers(method: &str, path: &str) -> bool {
    if residual_fallback::classify(method, path).is_some() {
        return true;
    }
    match routes::dispatch(method, path) {
        None => false,
        Some(routes::Native::Static) => static_files::canonical(path),
        Some(_) => true,
    }
}

fn recording() -> Recording {
    route_pins::embedded().expect("crates/route-pins recording parses")
}

const GOLDEN: &str = "../../crates/route-pins/fixtures/front-verdicts.md";

/// The Rust-owned (method, route) pairs no Node stub refuses: a request that bypassed the front would run Node's own
/// handler (if it still has one). Only the methods a Rust row lists, on each probe path of that row.
fn unguarded(rec: &Recording) -> Vec<(String, String)> {
    let refused: BTreeSet<(String, String)> = rec
        .stubs
        .iter()
        .flat_map(|s| s.refusals.iter().chain(s.switched.iter().flat_map(|w| w.refusals.iter())))
        .map(|r| (r.method.clone(), r.path.clone()))
        .collect();
    let mut out = Vec::new();
    for route in routes::ROUTES.iter().filter(|r| r.owner == routes::Owner::Rust && !r.catch_all) {
        let path = fill(route.path);
        let methods: Vec<&str> =
            if route.methods.is_empty() { route_pins::METHODS.to_vec() } else { route.methods.to_vec() };
        for m in methods {
            if front_answers(m, &path) && !refused.contains(&(m.to_string(), path.clone())) {
                out.push((m.to_string(), route.path.to_string()));
            }
        }
    }
    out
}

/// The recorder's parameter filling (tools/gen-node-refusal-stubs.cjs `fill`).
fn fill(pattern: &str) -> String {
    let mut out = String::new();
    let mut rest = pattern;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else { break };
        let name = &rest[start + 1..start + end];
        out.push_str(match name {
            "*?" => "",
            "*" => "qa-seg/qa-rest",
            _ => "qa-1",
        });
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

fn report() -> String {
    let rec = recording();
    let list = classify(&rec, &front_answers);
    let mut out = delete_list(&rec, &list);
    let pins = PinTable::leaked(&rec, &front_answers);
    let _ = writeln!(out);
    let _ = writeln!(out, "## Pin candidates: refusals the front still forwards to Node ({})", pins.len());
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Each is a request the front proxies only for Node to refuse it. A front step that answers these with the \
         recorded bytes (`route_pins::PinTable`) makes the listed Live stubs dead. Method, routes.toml pattern, \
         trailing slash:"
    );
    let _ = writeln!(out);
    for (m, p, slash) in pins.keys() {
        let _ = writeln!(out, "- `{m} {p}{}`", if *slash { " (trailing /)" } else { "" });
    }
    if !pins.conflicts().is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Not pinnable (probes of one pattern answered differently):");
        for (m, p, slash) in pins.conflicts() {
            let _ = writeln!(out, "- `{m} {p}{}`", if *slash { " (trailing /)" } else { "" });
        }
    }
    let unguarded = unguarded(&rec);
    let _ = writeln!(out);
    let _ = writeln!(out, "## Rust-owned methods no Node stub refuses ({})", unguarded.len());
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "The front answers these, and nothing in core's refusal chain refuses them for a request that bypassed the \
         front (the row's methods on the recorder's probe path). Not a leak through the front; a check for the \
         removal: Node's own handler, if it still has one, is the code that would run."
    );
    let _ = writeln!(out);
    for (m, p) in &unguarded {
        let _ = writeln!(out, "- `{m} {p}`");
    }
    out
}

#[test]
fn the_front_verdicts_match_the_golden_list() {
    let got = report();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
    if std::env::var_os("ROUTE_PINS_WRITE").is_some() {
        assert!(std::fs::write(&path, &got).is_ok(), "write {}", path.display());
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        want == got,
        "{} is stale: rerun with ROUTE_PINS_WRITE=1 and review the diff.\n--- got ---\n{got}",
        path.display()
    );
}

#[test]
fn every_pin_key_uses_the_front_s_own_route_match() {
    let rec = recording();
    let pins = PinTable::leaked(&rec, &front_answers);
    for s in &rec.stubs {
        for r in &s.refusals {
            let Some(pattern) = &r.pattern else { continue };
            let slash = r.path.len() > 1 && r.path.ends_with('/');
            if pins.lookup(&r.method, pattern, slash).is_none() {
                continue;
            }
            // The key's pattern is the routes.toml row the front itself matches for that path, or the request is an
            // unlisted /api path (no row: Node's).
            if let Some(route) = routes::route_for(&r.path) {
                assert!(
                    route.path == pattern.as_str() || route.catch_all,
                    "{} {}: recorder says {pattern}, the front matches {}",
                    r.method,
                    r.path,
                    route.path
                );
            }
            assert!(!front_answers(&r.method, &r.path), "{} {} is pinned but answered natively", r.method, r.path);
        }
    }
}

#[test]
fn a_deletable_stub_refuses_only_requests_the_front_answers() {
    let rec = recording();
    for a in classify(&rec, &front_answers) {
        if !matches!(a.verdict, Verdict::DeleteFile | Verdict::DeleteFunction { .. }) {
            continue;
        }
        let stub = rec.stubs.iter().find(|s| s.file == a.file && s.function == a.function).expect("assessed stub");
        assert!(stub.env.is_empty() && stub.switched.is_empty(), "{}", a.file);
        for r in &stub.refusals {
            assert!(front_answers(&r.method, &r.path), "{}: {} {}", a.file, r.method, r.path);
        }
    }
}

#[test]
fn fill_matches_the_recorder() {
    assert_eq!(fill("/api/projects/{id}/files"), "/api/projects/qa-1/files");
    assert_eq!(fill("/api/mcp-keys/{*}"), "/api/mcp-keys/qa-seg/qa-rest");
    assert_eq!(fill("/api/integrations/storage/files{*?}"), "/api/integrations/storage/files");
    assert_eq!(fill("/api/health"), "/api/health");
}
