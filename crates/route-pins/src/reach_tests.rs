#![allow(clippy::indexing_slicing, clippy::expect_used, clippy::unwrap_used)]

use super::*;

fn tree(files: &[(&str, &str)]) -> Tree {
    files
        .iter()
        .map(|(p, t)| (p.to_string(), t.to_string()))
        .collect()
}

fn none() -> BTreeSet<String> {
    BTreeSet::new()
}

#[test]
fn static_relative_requires_are_read_and_dynamic_ones_skipped() {
    let text = "const a = require('./a.cjs');\n\
                const { b } = require(\"../lib/b\");\n\
                const c = require( './c.cjs' );\n\
                const fs = require('node:fs');\n\
                const d = require(path.join(__dirname, 'd.cjs'));\n\
                const e = require(`./${name}.cjs`);\n\
                const f = require('./f.cjs' + suffix);\n\
                const g = require('./g.cjs\";\n\
                // require('./h.cjs') in a comment still counts (the safe side: more reached, fewer dead)";
    assert_eq!(
        requires(text),
        ["./a.cjs", "../lib/b", "./c.cjs", "./h.cjs"]
    );
}

#[test]
fn resolution_follows_node_s_order_and_ignores_files_taken_away() {
    let t = tree(&[
        ("server/a.cjs", ""),
        ("server/lib/b.js", ""),
        ("server/dir/index.cjs", ""),
        ("server/x", ""),
        ("server/x.cjs", ""),
    ]);
    let n = none();
    assert_eq!(
        resolve(&t, &n, "server/index.cjs", "./a.cjs").as_deref(),
        Some("server/a.cjs")
    );
    assert_eq!(
        resolve(&t, &n, "server/index.cjs", "./a").as_deref(),
        Some("server/a.cjs")
    );
    assert_eq!(
        resolve(&t, &n, "server/routes/r.cjs", "../lib/b").as_deref(),
        Some("server/lib/b.js")
    );
    assert_eq!(
        resolve(&t, &n, "server/index.cjs", "./dir").as_deref(),
        Some("server/dir/index.cjs")
    );
    // The exact path first, as Node.
    assert_eq!(
        resolve(&t, &n, "server/index.cjs", "./x").as_deref(),
        Some("server/x")
    );
    assert_eq!(resolve(&t, &n, "server/index.cjs", "./missing.cjs"), None);
    // Climbing above the root resolves nothing.
    assert_eq!(resolve(&t, &n, "server/index.cjs", "../../../a.cjs"), None);
    let gone: BTreeSet<String> = ["server/a.cjs".to_string()].into();
    assert_eq!(resolve(&t, &gone, "server/index.cjs", "./a"), None);
}

#[test]
fn the_graph_is_walked_from_the_entry_through_cycles() {
    let t = tree(&[
        (
            "server/index.cjs",
            "require('./a.cjs'); require('./routes/r.cjs');",
        ),
        ("server/a.cjs", "require('./b.cjs');"),
        ("server/b.cjs", "require('./a.cjs');"),
        ("server/routes/r.cjs", "require('../c');"),
        ("server/c.cjs", ""),
        ("server/lonely.cjs", "require('./a.cjs');"),
    ]);
    let seen = reached(&t, &none());
    assert_eq!(
        seen.into_iter().collect::<Vec<_>>(),
        [
            "server/a.cjs",
            "server/b.cjs",
            "server/c.cjs",
            "server/index.cjs",
            "server/routes/r.cjs"
        ]
    );
    // Without the entry nothing is reached (not a core tree): reached is empty, never "everything".
    assert!(reached(&tree(&[("server/a.cjs", "")]), &none()).is_empty());
}

#[test]
fn a_stem_is_named_only_as_a_whole_word() {
    assert!(names_stem("const x = 'rust-x';", "rust-x"));
    assert!(names_stem("rust-x", "rust-x"));
    assert!(names_stem("see rust-x.cjs", "rust-x"));
    assert!(names_stem("RUST=rust-x-y rust-x", "rust-x"));
    assert!(!names_stem("rust-x-y", "rust-x"));
    assert!(!names_stem("my-rust-x", "rust-x"));
    assert!(!names_stem("rust_x", "rust-x"));
    assert!(!names_stem("rust-xé", "rust-x"));
    assert!(!names_stem("anything", ""));
}

#[test]
fn unreached_files_are_dead_test_support_or_named() {
    let core = tree(&[
        ("server/index.cjs", "require('./live.cjs');"),
        ("server/live.cjs", "// nothing here names the others"),
        ("server/dead-one.cjs", "module.exports = {};"),
        ("server/helper-oracle.cjs", "module.exports = {};"),
        (
            "server/helper-oracle.test.cjs",
            "require('./helper-oracle.cjs');",
        ),
        (
            "tests/server/more.test.cjs",
            "require('../../server/helper-oracle.cjs');",
        ),
        (
            "server/marker.cjs",
            "module.exports = { M: 'rust-marker-v1' };",
        ),
        ("server/worker-x.cjs", ""),
        (
            "server/spawner.cjs",
            "new Worker(path.join(__dirname, 'worker-x.cjs'));",
        ),
        // Tests and fixtures are never listed themselves.
        ("server/fixtures/data.cjs", ""),
        // A recorder run by hand, named by nothing.
        (
            "server/x-oracle.cjs",
            "async function capture() {}\nif (require.main === module) capture();",
        ),
        ("server/a.test.cjs", ""),
        // Not a server source.
        ("tools/gen.cjs", ""),
    ]);
    let rs = tree(&[(
        "crates/x/tests/record.cjs",
        "require(core + '/server/marker.cjs')",
    )]);
    let noevia = tree(&[(
        "deploy/tools/assemble-release.sh",
        "grep -q marker \"$core/server/\"",
    )]);
    let list = unreached(&core, &none(), &[("noevia-rs", &rs), ("noevia", &noevia)]);
    let by_file: Vec<(&str, &Kind)> = list.iter().map(|u| (u.file.as_str(), &u.kind)).collect();
    assert_eq!(
        by_file,
        [
            ("server/dead-one.cjs", &Kind::Dead),
            (
                "server/helper-oracle.cjs",
                &Kind::TestSupport {
                    tests: vec![
                        "server/helper-oracle.test.cjs".into(),
                        "tests/server/more.test.cjs".into()
                    ]
                }
            ),
            (
                "server/marker.cjs",
                &Kind::Named {
                    by: vec!["noevia:deploy/tools/assemble-release.sh".into()]
                }
            ),
            // The spawner is itself unreached and dead, but it still names the worker: kept until it goes.
            ("server/spawner.cjs", &Kind::Dead),
            (
                "server/worker-x.cjs",
                &Kind::Named {
                    by: vec!["server/spawner.cjs".into()]
                }
            ),
            ("server/x-oracle.cjs", &Kind::Script),
        ]
    );
    assert_eq!(list[0].lines, 1);
}

#[test]
fn taking_the_delete_list_away_shows_what_it_would_strand() {
    let core = tree(&[
        (
            "server/index.cjs",
            "require('./rust-stub.cjs'); require('./shared.cjs');",
        ),
        (
            "server/rust-stub.cjs",
            "require('./stub-only-helper.cjs'); require('./shared.cjs');",
        ),
        ("server/stub-only-helper.cjs", ""),
        ("server/shared.cjs", ""),
    ]);
    assert!(unreached(&core, &none(), &[]).is_empty());
    let gone: BTreeSet<String> = ["server/rust-stub.cjs".to_string()].into();
    let list = unreached(&core, &gone, &[]);
    // The gone file is neither listed nor counted as naming anything.
    assert_eq!(
        list,
        [Unreached {
            file: "server/stub-only-helper.cjs".into(),
            lines: 1,
            kind: Kind::Dead
        }]
    );
}

#[test]
fn the_report_names_every_kind() {
    let r = report(&[
        Unreached {
            file: "server/a.cjs".into(),
            lines: 3,
            kind: Kind::Dead,
        },
        Unreached {
            file: "server/b.cjs".into(),
            lines: 4,
            kind: Kind::TestSupport {
                tests: vec!["server/b.test.cjs".into()],
            },
        },
        Unreached {
            file: "server/c.cjs".into(),
            lines: 5,
            kind: Kind::Named {
                by: vec!["noevia:build/x.sh".into()],
            },
        },
    ]);
    assert_eq!(
        r,
        "| File | Lines | Kind | Named by / tests |\n|---|---:|---|---|\n\
         | `server/a.cjs` | 3 | dead: delete |  |\n\
         | `server/b.cjs` | 4 | test support | server/b.test.cjs |\n\
         | `server/c.cjs` | 5 | keep (named) | noevia:build/x.sh |\n"
    );
    assert!(report(&[]).contains("| (none) | | | |"));
}

#[test]
fn a_script_runs_as_its_own_entry() {
    assert!(is_script("#!/usr/bin/env node\nmain();"));
    assert!(is_script("if (require.main === module) main();"));
    assert!(is_script("if (module === require.main) main();"));
    assert!(is_script("if (require.main == module) main();"));
    assert!(!is_script("module.exports = { main };"));
    assert!(!is_script(" #!/usr/bin/env node"));
}
