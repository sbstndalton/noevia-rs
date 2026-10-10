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

/// Every statement shape that writes an owned table must be recognised by Node's guard, not just
/// the plain `INSERT INTO t`: WITH ... writes, schema-qualified and quoted names, REPLACE/UPSERT.
#[test]
fn node_detects_every_write_shape_on_every_owned_table() {
    let Ok(core) = std::env::var("NOEVIA_CORE_CHECKOUT") else {
        eprintln!("NOEVIA_CORE_CHECKOUT not set: skipped");
        return;
    };
    let module = std::path::Path::new(&core).join("server/rust-auth.cjs");
    let script = r#"
      const { writeTargets } = require(process.argv[1]);
      const tables = JSON.parse(process.argv[2]);
      const shapes = [
        (t) => `INSERT INTO ${t}(a) VALUES(?)`,
        (t) => `INSERT OR REPLACE INTO ${t}(a) VALUES(?)`,
        (t) => `REPLACE INTO ${t}(a) VALUES(?)`,
        (t) => `UPSERT INTO ${t}(a) VALUES(?)`,
        (t) => `INSERT INTO ${t}(a) VALUES(?) ON CONFLICT(a) DO UPDATE SET a=1`,
        (t) => `UPDATE ${t} SET a=1`,
        (t) => `DELETE FROM ${t}`,
        (t) => `WITH x AS (SELECT 1) INSERT INTO ${t}(a) SELECT * FROM x`,
        (t) => `WITH x AS (SELECT 1) UPDATE ${t} SET a=1`,
        (t) => `WITH x AS (SELECT 1) DELETE FROM ${t}`,
        (t) => `INSERT INTO main.${t}(a) VALUES(?)`,
        (t) => `UPDATE "main"."${t}" SET a=1`,
        (t) => `DELETE FROM [${t}]`,
        (t) => `DELETE FROM \`${t}\``,
      ];
      const missed = [];
      for (const t of tables) for (const s of shapes) {
        const sql = s(t);
        if (!writeTargets(sql).some((x) => x.table === t)) missed.push(sql);
      }
      process.stdout.write(JSON.stringify(missed));
    "#;
    let tables: Vec<&str> = server_store::OWNED_TABLES
        .iter()
        .map(|(t, _)| *t)
        .filter(|t| *t != "audit_events" && *t != "diary_connectors")
        .collect();
    let out = std::process::Command::new("node")
        .arg("-e")
        .arg(script)
        .arg(&module)
        .arg(serde_json::to_string(&tables).unwrap())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("node runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let missed = String::from_utf8(out.stdout).unwrap();
    assert_eq!(missed, "[]", "Node's guard misses these write shapes");
}
