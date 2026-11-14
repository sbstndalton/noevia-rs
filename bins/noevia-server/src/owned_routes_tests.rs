#![allow(clippy::indexing_slicing, clippy::expect_used, clippy::unwrap_used)]

use super::*;
use crate::routes::{Owner, ROUTES};

#[test]
fn fill_replaces_parameters_wildcards_and_keeps_literals() {
    assert_eq!(fill("/api/admin/users/{id}", "qa-1"), "/api/admin/users/qa-1");
    assert_eq!(fill("/api/models/{*}", "qa-1"), "/api/models/qa-seg/qa-rest");
    assert_eq!(fill("/api/x/{a}/y/{*?}", FILLS[1]), format!("/api/x/{}/y/", FILLS[1]));
    assert_eq!(fill("/api/plain", "qa-1"), "/api/plain");
    assert_eq!(fill("/api/broken/{id", "qa-1"), "/api/broken/{id");
}

#[test]
fn normalised_matches_the_assemblers_sed() {
    // deploy/tools/assemble-release.sh project_routes_core: sed 's/{[^}]*}/{}/g'
    assert_eq!(normalised("/api/chats/{chatId}/export"), "/api/chats/{}/export");
    assert_eq!(normalised("/api/models/{*}"), "/api/models/{}");
    assert_eq!(normalised("/api/a/{x}/{y}"), "/api/a/{}/{}");
    assert_eq!(normalised("/api/none"), "/api/none");
    assert_eq!(normalised("/api/open/{x"), "/api/open/{x");
}

#[test]
fn the_uuid_filling_has_the_uuid_shape_core_checks() {
    // core rust-research.cjs / rust-task-reads.cjs only refuse ids matching [0-9a-f-]{36}.
    let id = FILLS[1];
    assert_eq!(id.len(), 36);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
}

#[test]
fn every_listed_pair_is_a_rust_row_method_the_front_dispatches() {
    let m = manifest();
    assert!(m.len() > 100, "only {} owned pairs", m.len());
    for &(method, pattern) in &m {
        let row = ROUTES.iter().find(|r| r.path == pattern).expect("listed pattern is a routes.toml row");
        assert_eq!(row.owner, Owner::Rust, "{method} {pattern}");
        assert!(!row.catch_all);
        assert!(row.methods.is_empty() || row.methods.contains(&method), "{method} {pattern}");
        for id in FILLS {
            let path = fill(pattern, id);
            assert!(routes::dispatch(method, &path).is_some(), "{method} {path}");
        }
    }
}

#[test]
fn node_rows_and_unlisted_methods_are_never_listed() {
    let m = manifest();
    for row in ROUTES.iter().filter(|r| r.owner == Owner::Node) {
        assert!(m.iter().all(|&(_, p)| p != row.path), "node row {} listed", row.path);
    }
    for row in ROUTES.iter().filter(|r| r.owner == Owner::Rust && !r.methods.is_empty()) {
        for &method in METHODS.iter().filter(|m| !row.methods.contains(m)) {
            assert!(!m.contains(&(method, row.path)), "{method} {} is Node's", row.path);
        }
    }
}

#[test]
fn known_pairs_are_owned_and_their_siblings_are_not() {
    let m = manifest();
    // routes.toml: /api/admin/users/{id} is Rust's for DELETE only (core rust-admin-users.cjs OWNED_ROUTES).
    assert!(m.contains(&("DELETE", "/api/admin/users/{id}")));
    assert!(!m.contains(&("GET", "/api/admin/users/{id}")));
    // The Node-owned rows of routes.toml stay Node's.
    assert!(!m.iter().any(|&(_, p)| p == "/api/embedding"));
}

#[test]
fn a_pair_is_listed_under_the_row_that_owns_its_filled_path() {
    for &(method, pattern) in &manifest() {
        for id in FILLS {
            let path = fill(pattern, id);
            let row = routes::route_for(&path).expect("routed");
            assert_eq!(row.path, pattern, "{method} {path} belongs to {}", row.path);
        }
    }
}

#[test]
fn fillings_agree_or_the_disagreement_is_reported() {
    // The manifest lists only pairs both fillings answer. Whatever disagrees is under-claimed; the list must be
    // exactly the pairs one filling answers and the other does not, and none of them may be listed.
    let m = manifest();
    for (method, pattern, a, b) in disagreements() {
        assert_ne!(a, b);
        assert!(!m.contains(&(method, pattern)), "{method} {pattern}");
    }
}

#[test]
fn written_manifest_is_sorted_unique_and_one_pair_per_line() {
    let mut out = Vec::new();
    write(&mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.ends_with('\n'));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), manifest().len());
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, lines);
    for line in &lines {
        let (method, pattern) = line.split_once(' ').expect("METHOD pattern");
        assert!(METHODS.contains(&method), "{line}");
        assert!(pattern.starts_with('/') && !pattern.contains(' '), "{line}");
    }
}

#[test]
fn normalised_manifest_covers_every_rust_row_core_lists_for_admin_users() {
    // The assembler compares `sort -u` of normalised pairs; normalising may merge two rows' pairs into one line
    // but never loses a pair.
    let m = manifest();
    let norm: BTreeSet<String> = m.iter().map(|(meth, p)| format!("{meth} {}", normalised(p))).collect();
    assert!(norm.contains("DELETE /api/admin/users/{}"));
    assert!(norm.len() <= m.len());
}
