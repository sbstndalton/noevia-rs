//! M3: noevia-core's single-writer guard (server/rust-auth.cjs) and this crate must name the
//! same tables and settings keys, or a table would have two writers (or none) under
//! NOEVIA_RUST_AUTH. Runs when NOEVIA_CORE_CHECKOUT points at a core checkout (CI); skipped
//! otherwise.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;

/// The quoted names inside `NAME = Object.freeze(new Set([ ... ]))`.
fn js_set(src: &str, name: &str) -> BTreeSet<String> {
    let start = src
        .find(&format!("const {name} = Object.freeze(new Set(["))
        .unwrap_or_else(|| panic!("{name} not found in rust-auth.cjs"));
    let rest = &src[start..];
    let end = rest.find("]))").unwrap();
    rest[..end]
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

#[test]
fn node_refuses_exactly_what_rust_owns() {
    let Ok(core) = std::env::var("NOEVIA_CORE_CHECKOUT") else {
        eprintln!("NOEVIA_CORE_CHECKOUT not set: skipped");
        return;
    };
    let src =
        std::fs::read_to_string(std::path::Path::new(&core).join("server/rust-auth.cjs")).unwrap();
    let node_tables = js_set(&src, "OWNED_TABLES");
    let node_keys = js_set(&src, "OWNED_SETTING_KEYS");
    // Shared on purpose: audit_events (both append) and diary_connectors (Node's; Rust only deletes
    // a recovered account's rows). Every other owned table is Node-refused.
    let rust_tables: BTreeSet<String> = server_store::OWNED_TABLES
        .iter()
        .filter(|(t, _)| *t != "audit_events" && *t != "diary_connectors")
        .map(|(t, _)| (*t).to_string())
        .collect();
    let rust_keys: BTreeSet<String> = server_store::OWNED_SETTING_KEYS
        .iter()
        .map(|k| (*k).to_string())
        .collect();
    assert_eq!(node_tables, rust_tables, "tables");
    assert_eq!(node_keys, rust_keys, "settings keys");
}
