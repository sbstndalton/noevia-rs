#![allow(clippy::indexing_slicing, clippy::expect_used, clippy::unwrap_used)]

use super::*;
use crate::routes::{Owner, ROUTES};

/// The default-feature build's manifest, as route-pins pins it. Rewrite it with
/// `cargo run -p noevia-server -- --owned-routes > crates/route-pins/fixtures/front-owned-routes.txt` when a route
/// moves (and re-run the route-pins golden).
const PINNED: &str = include_str!("../../../crates/route-pins/fixtures/front-owned-routes.txt");

#[test]
fn fill_replaces_parameters_wildcards_and_keeps_literals() {
    assert_eq!(
        fill("/api/admin/users/{id}", "qa-1"),
        "/api/admin/users/qa-1"
    );
    assert_eq!(fill("/api/models/{*}", "qa-1"), "/api/models/qa-seg");
    assert_eq!(
        fill("/api/projects/{id}/{*}/original", "qa-1"),
        "/api/projects/qa-1/uploads/original"
    );
    assert_eq!(
        fill("/api/x/{a}/y/{*?}", FILLS[1]),
        format!("/api/x/{}/y/", FILLS[1])
    );
    assert_eq!(fill("/api/plain", "qa-1"), "/api/plain");
    assert_eq!(fill("/api/broken/{id", "qa-1"), "/api/broken/{id");
}

#[test]
fn normalised_matches_the_assemblers_sed() {
    // deploy/tools/assemble-release.sh project_routes_core: sed 's/{[^}]*}/{}/g'
    assert_eq!(
        normalised("/api/chats/{chatId}/export"),
        "/api/chats/{}/export"
    );
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
    // sbstndalton/noevia#2725 item 3: the exact count is the pinned manifest's (see
    // the_manifest_is_the_pinned_fixture for the lines).
    assert_eq!(m.len(), PINNED.lines().count());
    for &(method, pattern) in &m {
        let row = ROUTES
            .iter()
            .find(|r| r.path == pattern)
            .expect("listed pattern is a routes.toml row");
        assert_eq!(row.owner, Owner::Rust, "{method} {pattern}");
        assert!(!row.catch_all);
        assert!(
            row.methods.is_empty() || row.methods.contains(&method),
            "{method} {pattern}"
        );
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
        assert!(
            m.iter().all(|&(_, p)| p != row.path),
            "node row {} listed",
            row.path
        );
    }
    for row in ROUTES
        .iter()
        .filter(|r| r.owner == Owner::Rust && !r.methods.is_empty())
    {
        for &method in METHODS.iter().filter(|m| !row.methods.contains(m)) {
            assert!(
                !m.contains(&(method, row.path)),
                "{method} {} is Node's",
                row.path
            );
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
    let norm: BTreeSet<String> = m
        .iter()
        .map(|(meth, p)| format!("{meth} {}", normalised(p)))
        .collect();
    assert!(norm.contains("DELETE /api/admin/users/{}"));
    assert!(norm.len() <= m.len());
}

#[test]
fn the_manifest_is_the_pinned_fixture() {
    let mut out = Vec::new();
    write(&mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    let want: BTreeSet<&str> = PINNED.lines().collect();
    let got: BTreeSet<&str> = text.lines().collect();
    let extra: Vec<&&str> = got.difference(&want).collect();
    let gone: Vec<&&str> = want.difference(&got).collect();
    assert!(
        extra.is_empty() && gone.is_empty(),
        "manifest moved; new {extra:?}, gone {gone:?} (rewrite the fixture, see PINNED)"
    );
    assert_eq!(text, PINNED);
}

#[test]
fn wildcard_rows_are_filled_with_one_segment_and_listed() {
    // council p7: `{*}` is one segment in routes::pattern_matches, so the old two-segment filling listed none.
    let m = manifest();
    for pair in [
        ("GET", "/api/projects/{id}/{*}/original"),
        ("DELETE", "/api/mcp-keys/{*}"),
        ("PUT", "/api/mcp-keys/{*}"),
        ("GET", "/api/admin/mcp-directory/{*}"),
        ("GET", "/api/admin/mcp-directory/{*}/keys"),
    ] {
        assert!(m.contains(&pair), "{pair:?}");
    }
    // The table's methods still bound a wildcard row.
    assert!(!m.contains(&("GET", "/api/mcp-keys/{*}")));
    // Node's wildcard rows stay out.
    assert!(!m.iter().any(|&(_, p)| p == "/api/models/{*}"));
    for route in ROUTES
        .iter()
        .filter(|r| r.path.contains("{*}") && !r.catch_all)
    {
        let path = fill(route.path, FILLS[0]);
        assert_eq!(
            path.split('/').count(),
            route.path.split('/').count(),
            "{}",
            route.path
        );
    }
}

#[test]
fn handler_forwards_are_left_out() {
    let m = manifest();
    for &(method, pattern, why) in HANDLER_FORWARDS {
        assert!(!why.is_empty());
        assert!(!m.contains(&(method, pattern)), "{method} {pattern}");
        // It is a pair the route table does give a native handler (else the entry is stale).
        assert!(
            answers(method, &fill(pattern, FILLS[0])),
            "{method} {pattern}"
        );
    }
    assert!(m.contains(&("GET", "/api/models/autotune")));
}

/// The handlers that pass a request on to Node unless it has a given method, with the methods the manifest may list
/// for their rows (a method the row's table entry gives to a different handler, such as the provider registry's,
/// counts as listed). sbstndalton/noevia#2725 item 4.
const METHOD_GUARDS: &[(&str, &str, &[&str])] = &[
    ("autotune_read_routes.rs", "/api/models/autotune", &["GET"]),
    (
        "chatgpt_status_routes.rs",
        "/api/providers/chatgpt",
        &["DELETE", "GET"],
    ),
    ("connector_routes.rs", "/api/connectors", &["GET"]),
    ("export_routes.rs", "/api/export/conversations", &["GET"]),
    ("health_routes.rs", "/api/health", &["GET"]),
    ("idle_unload_read.rs", "/api/models/idle-unload", &["GET"]),
    ("model_assignments.rs", "/api/models/assignments", &["GET"]),
    (
        "model_manager_scan.rs",
        "/api/model-manager/models",
        &["GET"],
    ),
    ("model_routes.rs", "/api/models/capabilities", &["GET"]),
    ("model_routes.rs", "/api/models/hardware", &["GET"]),
    (
        "provider_test_routes.rs",
        "/api/providers/test",
        &["DELETE", "POST", "PUT"],
    ),
    ("public_pages.rs", "/about", &["GET", "HEAD"]),
    (
        "routing_default_routes.rs",
        "/api/routing-default",
        &["GET"],
    ),
    ("stats_routes.rs", "/api/stats", &["GET"]),
    (
        "storage_integration_routes.rs",
        "/api/integrations/storage",
        &["GET", "PUT"],
    ),
    (
        "tool_approval_routes.rs",
        "/api/tool-approvals/{approvalId}",
        &["POST"],
    ),
];

#[test]
fn handlers_that_forward_by_method() {
    // Every source file that hands requests to Node and tests a method is in METHOD_GUARDS: a new guard of that kind
    // fails here until its row is checked against the manifest.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = BTreeSet::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".rs") || name.ends_with("_tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        if text.contains("app.proxy.forward")
            && (text.contains("method() != Method") || text.contains("req.method() == Method::"))
        {
            found.insert(name);
        }
    }
    let pinned: BTreeSet<String> = METHOD_GUARDS.iter().map(|g| g.0.to_string()).collect();
    assert_eq!(found, pinned);
    let m = manifest();
    for &(file, pattern, methods) in METHOD_GUARDS {
        let listed: Vec<&str> = METHODS
            .iter()
            .copied()
            .filter(|meth| m.contains(&(*meth, pattern)))
            .collect();
        let mut want = methods.to_vec();
        want.sort_unstable();
        let mut got = listed;
        got.sort_unstable();
        assert_eq!(got, want, "{file}: {pattern}");
    }
}
