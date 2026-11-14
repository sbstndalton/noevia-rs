use super::*;
use crate::{Assessment, Verdict};
use std::collections::BTreeSet;

fn tree(files: &[(&str, &str)]) -> Tree {
    files
        .iter()
        .map(|(p, t)| (p.to_string(), t.to_string()))
        .collect()
}

fn assessment(file: &str, verdict: Verdict) -> Assessment {
    Assessment {
        file: file.to_string(),
        function: "refuseOwned".to_string(),
        lines: 9,
        refusals: 1,
        pairing: Vec::new(),
        mentions: Vec::new(),
        verdict,
    }
}

/// The shape of core main's index.cjs around two plain stubs (index.cjs:1311 and :1434 at core 8f100d0).
const INDEX_TEXT: &str = "'use strict';\n\
const helper = require('./helper.cjs');\n\
async function handleRequestInner(req, res) {\n\
  if (require('./rust-tool-approvals.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;\n\
  // rust-workspace.cjs: the workspace list is the front's\n\
  if (require('./rust-workspace.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;\n\
  helper.run(req, res);\n\
}\n";

fn core() -> Tree {
    tree(&[
        (INDEX, INDEX_TEXT),
        ("server/helper.cjs", "module.exports = { run() {} };\n"),
        ("server/rust-tool-approvals.cjs", "module.exports = { refuseOwned() {} };\n"),
        (
            "server/rust-tool-approvals.test.cjs",
            "const t = require('./rust-tool-approvals.cjs');\n",
        ),
        ("server/rust-workspace.cjs", "module.exports = { refuseOwned() {} };\n"),
        (
            "server/rust-workspace.test.cjs",
            "const w = require('./rust-workspace.cjs');\n",
        ),
        (
            "server/rust-workspace.boot.test.cjs",
            "const w = require('./rust-workspace.cjs');\n",
        ),
        (
            "server/routes/chat-lists.cjs",
            "// GET /api/workspace is the Rust front's (rust-workspace.cjs)\nmodule.exports = {};\n",
        ),
        ("docs/notes.md", "See `server/rust-workspace.cjs`.\n"),
    ])
}

#[test]
fn guard_lines_are_exactly_the_one_line_refusal_call() {
    let f = "server/rust-workspace.cjs";
    for ok in [
        "  if (require('./rust-workspace.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;",
        "if (require('./rust-workspace.cjs').refuseOwned(req, res, {})) return;",
        "\tif (require('./rust-workspace.cjs').refuseOwned(req, res, p, json)) return;",
    ] {
        assert!(is_guard(ok, f, "refuseOwned"), "{ok}");
    }
    for no in [
        // another function of the file
        "if (require('./rust-workspace.cjs').owned(req, res, {})) return;",
        // another file whose name ends the same
        "if (require('./other-rust-workspace.cjs').refuseOwned(req, res, {})) return;",
        // more on the line than the guard
        "if (require('./rust-workspace.cjs').refuseOwned(req, res, {})) return; next();",
        "if (require('./rust-workspace.cjs').refuseOwned(req, res, {}) || x) return;",
        // arguments that call something
        "if (require('./rust-workspace.cjs').refuseOwned(req, res, { url: parse(req) })) return;",
        "if (require('./rust-workspace.cjs').refuseOwned(req, res, require('./x.cjs'))) return;",
        // a binding, not a guard
        "const { refuseOwned } = require('./rust-workspace.cjs');",
        // double quotes are not the recorder's call shape
        "if (require(\"./rust-workspace.cjs\").refuseOwned(req, res, {})) return;",
    ] {
        assert!(!is_guard(no, f, "refuseOwned"), "{no}");
    }
    assert!(is_guard(
        "if (require('./routes/x.cjs').refuseX(req, res, {})) return;",
        "server/routes/x.cjs",
        "refuseX"
    ));
}

#[test]
fn a_plain_plan_deletes_the_stub_and_its_own_tests_and_removes_its_guard() {
    let t = core();
    let list = [assessment("server/rust-workspace.cjs", Verdict::DeleteFile)];
    let plans = plan(&t, &list);
    assert_eq!(plans.len(), 1);
    let p = &plans[0];
    assert_eq!(p.stage, Stage::Plain);
    assert_eq!(
        p.delete,
        [
            "server/rust-workspace.cjs",
            "server/rust-workspace.boot.test.cjs",
            "server/rust-workspace.test.cjs"
        ]
    );
    assert_eq!(
        p.index_lines,
        [Line {
            line: 6,
            text: "  if (require('./rust-workspace.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;"
                .to_string()
        }]
    );
    // The comment lines in index.cjs and chat-lists.cjs and the doc are prose, not uses.
    assert!(p.manual.is_empty(), "{:?}", p.manual);
    assert!(p.mechanical());
}

#[test]
fn other_verdicts_are_not_planned_and_diary_files_are_held() {
    let mut t = core();
    t.insert(
        "server/rust-diary-x.cjs".into(),
        "module.exports = {};\n".into(),
    );
    let list = [
        assessment("server/rust-tool-approvals.cjs", Verdict::SwitchGated),
        assessment(
            "server/rust-workspace.cjs",
            Verdict::DeleteFunction {
                other_importers: vec!["server/x.cjs".into()],
            },
        ),
        assessment(
            "server/rust-workspace.cjs",
            Verdict::GuardsHandler {
                handlers: vec!["server/routes/x.cjs:1".into()],
            },
        ),
        assessment("server/rust-diary-x.cjs", Verdict::DeleteFile),
    ];
    let plans = plan(&t, &list);
    assert_eq!(plans.len(), 1);
    assert!(matches!(plans[0].stage, Stage::Held(_)));
    assert!(!plans[0].mechanical());
    // A held plan is never applied, even when asked for.
    assert!(apply(&t, &plans, &[plans[0].stage.clone()]).is_err());
}

#[test]
fn a_production_line_that_still_names_the_stub_makes_the_plan_manual() {
    let mut t = core();
    t.insert(
        "server/routes/code.cjs".into(),
        "const { owned } = require('../rust-workspace.cjs');\nmodule.exports = {};\n".into(),
    );
    // A second, bound use inside index.cjs is manual too.
    let idx = t.get_mut(INDEX).unwrap();
    idx.push_str("const ws = require('./rust-workspace.cjs');\n");
    let plans = plan(
        &t,
        &[assessment("server/rust-workspace.cjs", Verdict::DeleteFile)],
    );
    let p = &plans[0];
    assert_eq!(
        p.manual,
        [
            "server/index.cjs:9: const ws = require('./rust-workspace.cjs');",
            "server/routes/code.cjs:1: const { owned } = require('../rust-workspace.cjs');"
        ]
    );
    assert_eq!(p.index_lines.len(), 1);
    assert!(!p.mechanical());
    let err = apply(&t, &plans, &[Stage::Plain]).unwrap_err();
    assert!(err.contains("not mechanical"), "{err}");
}

#[test]
fn a_stub_without_a_guard_is_not_mechanical() {
    let mut t = core();
    let idx = t.get_mut(INDEX).unwrap();
    *idx = idx.replace(
        "  if (require('./rust-workspace.cjs').refuseOwned(req, res, { parseRequestUrl, json })) return;\n",
        "",
    );
    let plans = plan(
        &t,
        &[assessment("server/rust-workspace.cjs", Verdict::DeleteFile)],
    );
    assert!(plans[0].index_lines.is_empty());
    assert!(!plans[0].mechanical());
}

#[test]
fn apply_removes_exactly_the_planned_files_and_lines_and_keeps_the_rest_byte_for_byte() {
    let t = core();
    let list = [
        assessment("server/rust-tool-approvals.cjs", Verdict::DeleteFile),
        assessment("server/rust-workspace.cjs", Verdict::DeleteFile),
    ];
    let plans = plan(&t, &list);
    let after = apply(&t, &plans, &[Stage::Plain]).unwrap();
    for gone in [
        "server/rust-tool-approvals.cjs",
        "server/rust-tool-approvals.test.cjs",
        "server/rust-workspace.cjs",
        "server/rust-workspace.test.cjs",
        "server/rust-workspace.boot.test.cjs",
    ] {
        assert!(!after.contains_key(gone), "{gone}");
    }
    assert_eq!(
        after[INDEX],
        "'use strict';\n\
const helper = require('./helper.cjs');\n\
async function handleRequestInner(req, res) {\n\
  // rust-workspace.cjs: the workspace list is the front's\n\
  helper.run(req, res);\n\
}\n"
    );
    for kept in [
        "server/helper.cjs",
        "server/routes/chat-lists.cjs",
        "docs/notes.md",
    ] {
        assert_eq!(after[kept], t[kept], "{kept}");
    }
    assert_eq!(after.len(), t.len() - 5);
    let deleted: Vec<String> = plans.iter().map(|p| p.file.clone()).collect();
    assert!(check(&t, &after, &deleted).is_empty());
}

#[test]
fn apply_only_touches_the_stages_asked_for() {
    let t = core();
    let list = [
        assessment("server/rust-tool-approvals.cjs", Verdict::DeleteFile),
        assessment(
            "server/rust-workspace.cjs",
            Verdict::DeleteFileEditTests {
                tests: vec!["server/other.test.cjs".into()],
            },
        ),
    ];
    let plans = plan(&t, &list);
    assert_eq!(plans[1].stage, Stage::EditTests);
    assert_eq!(plans[1].foreign_tests, ["server/other.test.cjs"]);
    let after = apply(&t, &plans, &[Stage::Plain]).unwrap();
    assert!(!after.contains_key("server/rust-tool-approvals.cjs"));
    assert!(after.contains_key("server/rust-workspace.cjs"));
    assert!(after[INDEX].contains("require('./rust-workspace.cjs')"));
    assert_eq!(apply(&t, &plans, &[]).unwrap(), t);
}

#[test]
fn a_plan_made_at_another_index_is_refused_not_applied_to_the_wrong_line() {
    let t = core();
    let plans = plan(
        &t,
        &[assessment("server/rust-workspace.cjs", Verdict::DeleteFile)],
    );
    let mut moved = t.clone();
    let idx = moved.get_mut(INDEX).unwrap();
    *idx = idx.replacen("'use strict';\n", "'use strict';\n// a new first line\n", 1);
    let err = apply(&moved, &plans, &[Stage::Plain]).unwrap_err();
    assert!(err.contains("server/index.cjs:6"), "{err}");
    // A file already gone is refused too.
    let mut gone = t.clone();
    gone.remove("server/rust-workspace.test.cjs");
    let err = apply(&gone, &plans, &[Stage::Plain]).unwrap_err();
    assert!(
        err.contains("rust-workspace.test.cjs is not in the tree"),
        "{err}"
    );
}

#[test]
fn apply_keeps_crlf_lines_and_a_missing_final_newline() {
    let mut t = core();
    let crlf = INDEX_TEXT.trim_end_matches('\n').replace('\n', "\r\n");
    t.insert(INDEX.into(), crlf);
    let plans = plan(
        &t,
        &[assessment("server/rust-workspace.cjs", Verdict::DeleteFile)],
    );
    assert_eq!(plans[0].index_lines[0].line, 6);
    let after = apply(&t, &plans, &[Stage::Plain]).unwrap();
    assert!(!after[INDEX].contains("rust-workspace.cjs')"));
    assert!(after[INDEX].ends_with("}"));
    assert_eq!(after[INDEX].matches("\r\n").count(), 6);
}

#[test]
fn check_names_leftover_uses_foreign_tests_and_stranded_files() {
    let mut t = core();
    t.insert(
        "server/rust-workspace-helper.cjs".into(),
        "module.exports = {};\n".into(),
    );
    let idx = t.get_mut(INDEX).unwrap();
    idx.push_str("require('./rust-workspace-helper.cjs');\n");
    t.insert(
        "server/other.test.cjs".into(),
        "require('./rust-workspace.cjs');\n".into(),
    );
    let mut after = t.clone();
    after.remove("server/rust-workspace.cjs");
    // The helper's only require goes away with an index line.
    let idx = after.get_mut(INDEX).unwrap();
    *idx = idx.replace("require('./rust-workspace-helper.cjs');\n", "");
    let problems = check(&t, &after, &["server/rust-workspace.cjs".to_string()]);
    assert_eq!(
        problems,
        [
            Problem::StillNamed {
                file: "server/rust-workspace.cjs".into(),
                at: "server/index.cjs:6".into()
            },
            Problem::TestStillNames {
                file: "server/rust-workspace.cjs".into(),
                test: "server/other.test.cjs".into()
            },
            Problem::TestStillNames {
                file: "server/rust-workspace.cjs".into(),
                test: "server/rust-workspace.boot.test.cjs".into()
            },
            Problem::TestStillNames {
                file: "server/rust-workspace.cjs".into(),
                test: "server/rust-workspace.test.cjs".into()
            },
            Problem::Stranded {
                file: "server/rust-workspace-helper.cjs".into()
            },
        ]
    );
}

#[test]
fn noevia_sites_are_classified_by_shape() {
    let name = "rust-tool-approvals.cjs";
    let cases = [
        (
            "deploy/tools/assemble-release.sh",
            "grep -qF \"require('./rust-tool-approvals.cjs')\" \"$tree/core/server/index.cjs\" 2>/dev/null && ta_core_owner=rust",
            SiteKind::OwnerProbe,
        ),
        (
            "build/web-supervisor.sh",
            "grep -qF \"require('./rust-tool-approvals.cjs')\" \"$PWD/server/index.cjs\" 2>/dev/null && ta=rust",
            SiteKind::OwnerProbe,
        ),
        (
            "deploy/tools/assemble-release.sh",
            "    ta_refused=\"$( { project_routes_core \"$tree/core/server/rust-tool-approvals.cjs\" || true; } | sort)\"",
            SiteKind::FileRead,
        ),
        (
            "build/web-supervisor.sh",
            "rust-tool-approvals.cjs rust-tool-approvals tool approval decision",
            SiteKind::TableRow,
        ),
        (
            "deploy/tools/test-assemble-release.sh",
            "cat > \"$w/server/rust-tool-approvals.cjs\" <<'JS'",
            SiteKind::Fixture,
        ),
        (
            "deploy/tools/test-web-supervisor.sh",
            "for pair in \"rust-tool-approvals.cjs rust-tool-approvals tool approval decision\"; do",
            SiteKind::Fixture,
        ),
        (
            "build/web-supervisor.sh",
            "# list (server/rust-tool-approvals.cjs beside rust-tool-approvals)",
            SiteKind::Comment,
        ),
        (
            "deploy/tools/assemble-release.sh",
            "get_only_pair rust-tool-approvals.cjs rust-tool-approvals \"tool approvals\" /api/x",
            SiteKind::PairCall,
        ),
        (
            "deploy/tools/assemble-release.sh",
            "marker=rust-tool-approvals.cjs",
            SiteKind::Other,
        ),
        (
            "deploy/tools/assemble-release.sh",
            "  die \"approval pairing: the front needs Core's rust-tool-approvals.cjs refusal\"",
            SiteKind::Other,
        ),
    ];
    for (path, text, kind) in cases {
        assert_eq!(site_kind(path, text, name), kind, "{path}: {text}");
    }
}

#[test]
fn pairing_sites_read_build_and_deploy_only_and_whole_file_names() {
    let noevia = tree(&[
        (
            "build/web-supervisor.sh",
            "#!/bin/sh\nrust-workspace.cjs rust-workspace workspace list\nrust-workspace.cjs.bak x\n",
        ),
        (
            "deploy/tools/assemble-release.sh",
            "get_only_pair rust-workspace.cjs rust-workspace \"workspace list\" /api/workspace\nother-rust-workspace.cjs\n",
        ),
        ("docs/x.md", "rust-workspace.cjs\n"),
        ("web/rust-workspace.cjs.txt", "rust-workspace.cjs\n"),
    ]);
    let sites = pairing_sites(&noevia, "server/rust-workspace.cjs");
    assert_eq!(
        sites,
        [
            Site {
                path: "build/web-supervisor.sh".into(),
                line: 2,
                kind: SiteKind::TableRow,
                text: "rust-workspace.cjs rust-workspace workspace list".into()
            },
            Site {
                path: "deploy/tools/assemble-release.sh".into(),
                line: 1,
                kind: SiteKind::PairCall,
                text: "get_only_pair rust-workspace.cjs rust-workspace \"workspace list\" /api/workspace"
                    .into()
            },
        ]
    );
}

#[test]
fn the_report_has_one_row_per_plan_with_the_site_counts() {
    let t = core();
    let list = [
        assessment("server/rust-tool-approvals.cjs", Verdict::DeleteFile),
        assessment(
            "server/rust-workspace.cjs",
            Verdict::DeleteFileEditTests {
                tests: vec!["server/other.test.cjs".into()],
            },
        ),
    ];
    let plans = plan(&t, &list);
    let noevia = tree(&[(
        "build/web-supervisor.sh",
        "rust-tool-approvals.cjs rust-tool-approvals tool approval decision\n# rust-tool-approvals.cjs\n",
    )]);
    let md = report(&plans, Some(&noevia));
    assert!(
        md.starts_with("2 file deletions planned, 2 mechanical in core.\n"),
        "{md}"
    );
    assert!(md.contains(
        "| `server/rust-tool-approvals.cjs` | 1 (plain) | server/rust-tool-approvals.cjs, server/rust-tool-approvals.test.cjs | 4 | none | none | 1 table row, 1 comment |"
    ), "{md}");
    assert!(
        md.contains("| `server/rust-workspace.cjs` | 2 (edit tests) |"),
        "{md}"
    );
    assert!(md.contains("| server/other.test.cjs | none |"), "{md}");
    assert!(report(&plans, None).contains("| not read |"));
}

#[test]
fn the_embedded_recordings_file_verdicts_plan_on_a_tree_built_from_their_index_uses() {
    // Every file verdict of the pinned recording, under a front that answers everything, on a minimal tree that has
    // each stub and its guard: every non-Diary plan is mechanical, which is the shape the train relies on.
    let rec = crate::embedded().unwrap();
    let list = crate::classify(&rec, &|_: &str, _: &str| true);
    let files: Vec<&Assessment> = list
        .iter()
        .filter(|a| {
            matches!(
                a.verdict,
                Verdict::DeleteFile | Verdict::DeleteFileEditTests { .. }
            )
        })
        .collect();
    assert!(!files.is_empty());
    let mut index = String::from("async function handleRequestInner(req, res) {\n");
    let mut t = Tree::new();
    for a in &files {
        index.push_str(&format!(
            "  if (require('./{}').{}(req, res, {{ parseRequestUrl, json }})) return;\n",
            a.file.strip_prefix("server/").unwrap(),
            a.function
        ));
        t.insert(a.file.clone(), "module.exports = {};\n".into());
    }
    index.push_str("}\n");
    t.insert(INDEX.into(), index);
    let owned: Vec<Assessment> = files.into_iter().cloned().collect();
    let plans = plan(&t, &owned);
    assert_eq!(plans.len(), owned.len());
    for p in &plans {
        match p.stage {
            Stage::Held(_) => assert!(p.file.contains("diary"), "{}", p.file),
            _ => assert!(p.mechanical(), "{p:?}"),
        }
    }
    let stages = [Stage::Plain, Stage::EditTests];
    let after = apply(&t, &plans, &stages).unwrap();
    assert_eq!(
        after[INDEX],
        "async function handleRequestInner(req, res) {\n}\n"
    );
    let deleted: Vec<String> = plans.iter().map(|p| p.file.clone()).collect();
    assert!(check(&t, &after, &deleted).is_empty());
}

/// Lines 1308-1436 of core main 8f100d0's server/index.cjs: the refusal guard block (66 guards among bridges and
/// comments), copied verbatim so the guard rule runs on the real shapes.
const INDEX_GUARDS: &str = include_str!("../fixtures/index-guards-core-8f100d0.txt");

#[test]
fn core_mains_guard_block_plans_every_pinned_file_verdict_mechanically() {
    let rec = crate::embedded().unwrap();
    let m = crate::Manifest::parse(include_str!("../fixtures/front-owned-routes.txt")).unwrap();
    let list = crate::classify(&rec, &m.front(&rec));
    let files = list
        .iter()
        .filter(|a| {
            matches!(
                a.verdict,
                Verdict::DeleteFile | Verdict::DeleteFileEditTests { .. }
            )
        })
        .count();
    // The pinned delete list: 24 plain + 8 with test edits.
    assert_eq!(files, 32);
    let mut t = Tree::new();
    t.insert(
        INDEX.into(),
        format!("async function handleRequestInner(req, res) {{\n{INDEX_GUARDS}}}\n"),
    );
    for a in &list {
        t.insert(a.file.clone(), "module.exports = {};\n".into());
    }
    let plans = plan(&t, &list);
    assert_eq!(plans.len(), 32);
    for p in &plans {
        assert!(p.mechanical(), "{p:?}");
        assert_eq!(p.index_lines.len(), 1, "{}", p.file);
        assert_eq!(p.delete, [p.file.clone()]);
    }
    assert_eq!(plans.iter().filter(|p| p.stage == Stage::Plain).count(), 24);
    assert_eq!(
        plans.iter().filter(|p| p.stage == Stage::EditTests).count(),
        8
    );
    let after = apply(&t, &plans, &[Stage::Plain, Stage::EditTests]).unwrap();
    let removed: BTreeSet<usize> = plans.iter().map(|p| p.index_lines[0].line).collect();
    assert_eq!(removed.len(), 32);
    let expect: Vec<&str> = t[INDEX]
        .lines()
        .enumerate()
        .filter(|(i, _)| !removed.contains(&(i + 1)))
        .map(|(_, l)| l)
        .collect();
    assert_eq!(after[INDEX].lines().collect::<Vec<_>>(), expect);
    // 34 guards of kept stubs stay (66 in the block).
    let guards = |text: &str| {
        text.lines()
            .filter(|l| l.starts_with("  if (require('./"))
            .count()
    };
    assert_eq!(guards(&t[INDEX]), 66);
    assert_eq!(guards(&after[INDEX]), 34);
    let deleted: Vec<String> = plans.iter().map(|p| p.file.clone()).collect();
    assert!(check(&t, &after, &deleted).is_empty());
}
