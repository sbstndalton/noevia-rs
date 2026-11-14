#![allow(clippy::indexing_slicing, clippy::expect_used, clippy::unwrap_used)]

use super::*;
use crate::{classify, Verdict};

fn tree(files: &[(&str, &str)]) -> Tree {
    files
        .iter()
        .map(|(p, t)| (p.to_string(), t.to_string()))
        .collect()
}

/// One recorded stub (JSON, as the recorder writes it) refusing `GET /api/<stem>`.
fn stub(file: &str, lines: usize, importers: &[&str], index_uses: &[&str]) -> serde_json::Value {
    let stem = file.trim_start_matches("server/").trim_end_matches(".cjs");
    serde_json::json!({
        "file": file,
        "fn": "refuseOwned",
        "lines": lines,
        "importers": importers,
        "index_uses": index_uses,
        "refusals": [{
            "method": "GET",
            "path": format!("/api/{stem}"),
            "pattern": format!("/api/{stem}"),
            "status": 503,
            "headers": {"content-type": "application/json"},
            "body_text": "{}",
            "body": {}
        }]
    })
}

fn recording(stubs: Vec<serde_json::Value>) -> Recording {
    serde_json::from_value(serde_json::json!({
        "generator": "test",
        "core": "0".repeat(40),
        "noevia": "1".repeat(40),
        "methods": ["GET"],
        "probes": 1,
        "stubs": stubs,
    }))
    .unwrap()
}

#[test]
fn a_quoted_path_string_is_a_reference_and_prose_is_a_mention() {
    let n = "rust-x.cjs";
    for yes in [
        "require('./rust-x.cjs')",
        r#"files: ["server/rust-x.cjs"]"#,
        "`rust-x.cjs`",
        "x = '../a/b-c_d/rust-x.cjs';",
        "see rust-x.cjs, then 'rust-x.cjs'",
    ] {
        assert!(quoted_ref(yes, n), "{yes}");
    }
    for no in [
        "see rust-x.cjs for the refusal",
        "'rust-x.cjs.bak'",
        "\"foo rust-x.cjs\"",
        "rust-x.cjs'",
        "'(rust-x.cjs)'",
        "",
    ] {
        assert!(!quoted_ref(no, n), "{no}");
    }
    assert!(!quoted_ref("''", ""));
}

#[test]
fn index_uses_follow_the_recorder_s_two_patterns() {
    let index = "\
const a = require('./rust-a.cjs');
const { owned, refuseOwned: r } = require('./rust-b.cjs');
require('./rust-c.cjs').refuseOwned(req, res, {});
const featuresB = require('./rust-d.cjs')
if (require('./rust-d.cjs').refuseOwned(req, res)) return;
const x = require('./rust-e.cjs').helper;
let\tq = require('./rust-f.cjs')  ;
constx=require('./rust-g.cjs');
const y = require('./rust-h.cjs') || null;
const k = require('./routes/account.cjs');
";
    let uses = |f: &str| index_uses(index, f);
    assert_eq!(uses("server/rust-a.cjs"), ["a"]);
    assert_eq!(uses("server/rust-b.cjs"), ["owned", "refuseOwned"]);
    assert_eq!(uses("server/rust-c.cjs"), ["refuseOwned"]);
    assert_eq!(uses("server/rust-d.cjs"), ["featuresB", "refuseOwned"]);
    assert_eq!(uses("server/rust-e.cjs"), ["helper"]);
    assert_eq!(uses("server/rust-f.cjs"), ["q"]);
    // No space after the keyword, or more after the call: only the bare module (the recorder's `<module>`).
    assert_eq!(uses("server/rust-g.cjs"), ["<module>"]);
    assert_eq!(uses("server/rust-h.cjs"), ["<module>"]);
    assert_eq!(uses("server/routes/account.cjs"), ["k"]);
    assert!(uses("server/rust-z.cjs").is_empty());
}

#[test]
fn importers_are_server_cjs_files_naming_dot_slash_the_file() {
    let t = tree(&[
        (
            "server/index.cjs",
            "require('./rust-a.cjs').refuseOwned(req, res)",
        ),
        ("server/rust-a.cjs", "module.exports = {}; // rust-a.cjs"),
        ("server/other.cjs", "const a = require('./rust-a.cjs');"),
        ("server/rust-a.test.cjs", "require('./rust-a.cjs')"),
        ("server/lib.js", "load('./rust-a.cjs')"),
        ("tests/x.test.cjs", "require('../server/rust-a.cjs')"),
        ("docs/notes.md", "rust-a.cjs is gone after the train"),
        ("server/unrelated.cjs", "require('./rust-ab.cjs')"),
    ]);
    let imp = importers(&t, "server/rust-a.cjs");
    assert_eq!(
        imp,
        [
            "server/index.cjs",
            "server/other.cjs",
            "server/rust-a.test.cjs"
        ]
    );
    let (refs, mentions) = refs_and_mentions(&t, "server/rust-a.cjs", &imp);
    assert_eq!(refs, ["server/lib.js", "tests/x.test.cjs"]);
    assert_eq!(mentions, ["docs/notes.md"]);
    // A routes/ file is required by its own name only under the same directory spelling.
    let t = tree(&[("server/index.cjs", "require('./routes/account.cjs')")]);
    assert!(importers(&t, "server/routes/account.cjs").is_empty());
}

#[test]
fn a_new_importer_on_core_main_turns_a_file_delete_into_a_function_delete() {
    let rec = recording(vec![
        stub(
            "server/rust-a.cjs",
            2,
            &["server/index.cjs", "server/rust-a.test.cjs"],
            &["refuseOwned"],
        ),
        stub(
            "server/rust-gone.cjs",
            2,
            &["server/index.cjs"],
            &["refuseOwned"],
        ),
    ]);
    let front = |_: &str, _: &str| true;
    let before = classify(&rec, &front);
    assert_eq!(before[0].verdict, Verdict::DeleteFile);
    let t = tree(&[
        (
            "server/index.cjs",
            "async function handleRequestInner(req, res) {\nrequire('./rust-a.cjs').refuseOwned(req, res, p, json);\n",
        ),
        ("server/rust-a.cjs", "module.exports = {};\n"),
        ("server/rust-a.test.cjs", "require('./rust-a.cjs')"),
        (
            "server/other.cjs",
            "const { owned } = require('./rust-a.cjs');",
        ),
    ]);
    let r = recheck(&rec, &t).unwrap();
    assert_eq!(
        r.drift,
        [
            Drift::Changed {
                file: "server/rust-a.cjs".into(),
                field: "importers",
                added: vec!["server/other.cjs".into()],
                removed: vec![],
            },
            Drift::Gone {
                file: "server/rust-gone.cjs".into()
            },
        ]
    );
    assert!(r.drift.iter().all(|d| !d.needs_rerecord()));
    assert_eq!(r.recording.stubs.len(), 1);
    let after = classify(&r.recording, &front);
    assert_eq!(
        after[0].verdict,
        Verdict::DeleteFunction {
            other_importers: vec!["server/other.cjs".into()]
        }
    );
    let md = report(&r.drift);
    assert!(
        md.contains("`server/rust-gone.cjs`: gone from core"),
        "{md}"
    );
    assert!(md.contains("importers: +[\"server/other.cjs\"]"), "{md}");
}

#[test]
fn an_unchanged_tree_has_no_drift_and_keeps_the_verdicts() {
    let rec = recording(vec![stub(
        "server/rust-a.cjs",
        2,
        &["server/index.cjs"],
        &["refuseOwned"],
    )]);
    let t = tree(&[
        (
            "server/index.cjs",
            "async function handleRequestInner(req, res) {\nrequire('./rust-a.cjs').refuseOwned(req, res, p, json);",
        ),
        ("server/rust-a.cjs", "x\n"),
    ]);
    let r = recheck(&rec, &t).unwrap();
    assert!(r.drift.is_empty(), "{:?}", r.drift);
    assert_eq!(
        report(&r.drift),
        "No drift: the tree's file facts are the recording's.\n"
    );
    let front = |_: &str, _: &str| true;
    assert_eq!(
        classify(&r.recording, &front)[0].verdict,
        classify(&rec, &front)[0].verdict
    );
}

#[test]
fn an_edited_or_uncalled_stub_needs_a_re_record() {
    let rec = recording(vec![stub(
        "server/rust-a.cjs",
        2,
        &["server/index.cjs"],
        &["refuseOwned"],
    )]);
    let t = tree(&[
        // index.cjs still requires the file but calls something else now.
        (
            "server/index.cjs",
            "async function handleRequestInner(req, res) {\nrequire('./rust-a.cjs').owned(req);",
        ),
        ("server/rust-a.cjs", "a\nb\nc\n"),
    ]);
    let r = recheck(&rec, &t).unwrap();
    assert!(r.drift.contains(&Drift::NotCalled {
        file: "server/rust-a.cjs".into(),
        function: "refuseOwned".into()
    }));
    assert!(r.drift.contains(&Drift::Lines {
        file: "server/rust-a.cjs".into(),
        recorded: 2,
        now: 4
    }));
    assert!(r.drift.iter().any(Drift::needs_rerecord));
    assert_eq!(r.recording.stubs[0].lines, 4);
    assert_eq!(r.recording.stubs[0].index_uses, ["owned"]);
}

#[test]
fn a_tree_without_index_cjs_is_refused() {
    let rec = recording(vec![]);
    assert!(recheck(&rec, &tree(&[("server/x.cjs", "")])).is_err());
}

#[test]
fn line_count_is_split_on_newlines_like_the_recorder() {
    assert_eq!(line_count(""), 1);
    assert_eq!(line_count("a"), 1);
    assert_eq!(line_count("a\n"), 2);
    assert_eq!(line_count("a\r\nb"), 2);
}

#[test]
fn the_embedded_recording_s_facts_are_in_the_rules_shape() {
    // What recheck re-derives must be what the recorder recorded: sorted lists, index_uses never mixing `<module>`
    // with names, and every stub called by index.cjs under its recorded function.
    let rec = crate::embedded().unwrap();
    for s in &rec.stubs {
        for (field, v) in [
            ("importers", &s.importers),
            ("string_refs", &s.string_refs),
            ("mentions", &s.mentions),
            ("index_uses", &s.index_uses),
        ] {
            let mut sorted = v.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(&sorted, v, "{} {field}", s.file);
        }
        assert!(
            s.index_uses.len() <= 1 || !s.index_uses.iter().any(|u| u == "<module>"),
            "{}",
            s.file
        );
        assert!(
            s.importers
                .iter()
                .all(|f| f.starts_with("server/") && f.ends_with(".cjs")),
            "{}",
            s.file
        );
        assert!(
            !s.mentions.iter().any(|f| s.string_refs.contains(f)),
            "{}",
            s.file
        );
        assert!(
            !s.mentions.iter().any(|f| s.importers.contains(f)),
            "{}",
            s.file
        );
    }
    assert_eq!(
        Rechecked::kept_as_recorded(),
        ["refusals", "switched", "pairing", "owned_routes"],
        "the facts left as recorded are named in the module docs"
    );
}

#[test]
fn pattern_pieces_split_on_parameters_like_the_recorder() {
    assert_eq!(pattern_pieces("/api/x"), ["/api/x"]);
    assert_eq!(pattern_pieces("/api/x/{id}"), ["/api/x/"]);
    assert_eq!(pattern_pieces("/api/x/{id}/y"), ["/api/x/", "/y"]);
    assert_eq!(pattern_pieces("/api/{a}/{b}"), ["/api/"]);
    assert_eq!(pattern_pieces("/api/x/{*}"), ["/api/x/"]);
    assert_eq!(pattern_pieces("/{id}"), Vec::<&str>::new());
    // No closing brace: no parameter (`\{[^}]*\}` needs one).
    assert_eq!(pattern_pieces("/api/broken/{id"), ["/api/broken/{id"]);
}

#[test]
fn behind_finds_handler_lines_outside_stubs_tests_and_fixtures() {
    let t = tree(&[
        ("server/index.cjs", "if (p === '/api/a') return a(req, res);\nrequire('./rust-a.cjs').refuseOwned(req, res, '/api/a');"),
        ("server/rust-a.cjs", "if (p === '/api/a') refuse();"),
        ("server/routes/a.cjs", "// '/api/a' handled here\n  * '/api/a'\nrouter.get('/api/ab', x);\nrouter.get('/api/a', h);\nconst re = /^\\/api\\/a$/;"),
        ("server/routes/a.test.cjs", "get('/api/a')"),
        ("server/fixtures/a.cjs", "get('/api/a')"),
        ("server/notes.js", "get('/api/a')"),
        ("tests/a.cjs", "get('/api/a')"),
        ("server/routes/items.cjs", "if (m[1] && p.startsWith('/api/items/') && p.endsWith('/open')) open();"),
    ]);
    let stubs: BTreeSet<&str> = ["server/rust-a.cjs"].into_iter().collect();
    let b = behind(
        &t,
        &stubs,
        &[
            "/api/a".to_string(),
            "/api/items/{id}/open".to_string(),
            "/api/none".to_string(),
        ],
    );
    assert_eq!(
        b["/api/a"],
        [
            "server/index.cjs:1",
            "server/routes/a.cjs:4",
            "server/routes/a.cjs:5"
        ]
    );
    assert_eq!(b["/api/items/{id}/open"], ["server/routes/items.cjs:1"]);
    assert!(!b.contains_key("/api/none"));
}

#[test]
fn a_new_handler_on_core_main_turns_a_file_delete_into_delete_with_handler() {
    let rec = recording(vec![stub(
        "server/rust-a.cjs",
        2,
        &["server/index.cjs"],
        &["refuseOwned"],
    )]);
    let t = tree(&[
        (
            "server/index.cjs",
            "async function handleRequestInner(req, res) {\nrequire('./rust-a.cjs').refuseOwned(req, res, p, json);",
        ),
        ("server/rust-a.cjs", "x\n"),
        ("server/routes/a.cjs", "router.get('/api/rust-a', h);"),
    ]);
    let r = recheck(&rec, &t).unwrap();
    assert_eq!(
        r.drift,
        [Drift::Changed {
            file: "server/rust-a.cjs".into(),
            field: "behind",
            added: vec!["/api/rust-a <- server/routes/a.cjs".into()],
            removed: vec![],
        }]
    );
    let front = |_: &str, _: &str| true;
    assert_eq!(classify(&rec, &front)[0].verdict, Verdict::DeleteFile);
    assert_eq!(
        classify(&r.recording, &front)[0].verdict,
        Verdict::GuardsHandler {
            handlers: vec!["server/routes/a.cjs:1".into()]
        }
    );
}

#[test]
fn moved_hit_lines_get_their_method_facts_re_read_and_are_not_drift() {
    let mut v = stub(
        "server/rust-a.cjs",
        2,
        &["server/index.cjs"],
        &["refuseOwned"],
    );
    v["behind"] = serde_json::json!({"/api/rust-a": ["server/routes/a.cjs:1"]});
    v["behind_methods"] = serde_json::json!({"server/routes/a.cjs:1": ["PUT"]});
    let rec = recording(vec![v]);
    let index = "async function handleRequestInner(req, res) {\nrequire('./rust-a.cjs').refuseOwned(req, res, p, json);";
    // Same line: the facts stay.
    let same = tree(&[
        ("server/index.cjs", index),
        ("server/rust-a.cjs", "x\n"),
        (
            "server/routes/a.cjs",
            "if (req.method === 'PUT' && p === '/api/rust-a') put();",
        ),
    ]);
    let r = recheck(&rec, &same).unwrap();
    assert!(r.drift.is_empty(), "{:?}", r.drift);
    assert_eq!(
        r.recording.stubs[0].behind_methods,
        BTreeMap::from([("server/routes/a.cjs:1".to_string(), vec!["PUT".to_string()])])
    );
    // One line lower: same file, so no drift, and the method fact follows the line (council p12: re-read with
    // methods_near, no longer dropped).
    let moved = tree(&[
        ("server/index.cjs", index),
        ("server/rust-a.cjs", "x\n"),
        (
            "server/routes/a.cjs",
            "\nif (req.method === 'PUT' && p === '/api/rust-a') put();",
        ),
    ]);
    let r = recheck(&rec, &moved).unwrap();
    assert!(r.drift.is_empty(), "{:?}", r.drift);
    assert_eq!(
        r.recording.stubs[0].behind_methods,
        BTreeMap::from([("server/routes/a.cjs:2".to_string(), vec!["PUT".to_string()])])
    );
    assert_eq!(
        r.recording.stubs[0].behind["/api/rust-a"],
        ["server/routes/a.cjs:2"]
    );
}

#[test]
fn a_refusal_counts_as_called_only_in_the_request_chain_in_the_recorder_s_shapes() {
    let mut st: Stub = serde_json::from_value(stub("server/rust-a.cjs", 1, &[], &[])).unwrap();
    let chain = "async function handleRequestInner(req, res) {\n";
    assert!(still_called(
        &format!("{chain}require('./rust-a.cjs').refuseOwned(req, res, p, json)"),
        &st
    ));
    assert!(still_called(
        &format!("{chain}require('./rust-a.cjs').refuseOwned(req, res, {{ json }})"),
        &st
    ));
    // Before the request chain, another argument shape, or no chain at all: not the recorder's call.
    assert!(!still_called(
        &format!("require('./rust-a.cjs').refuseOwned(req, res, p, json)\n{chain}"),
        &st
    ));
    assert!(!still_called(
        &format!("{chain}require('./rust-a.cjs').refuseOwned(req, res)"),
        &st
    ));
    assert!(!still_called(
        "require('./rust-a.cjs').refuseOwned(req, res, p, json)",
        &st
    ));
    st.function = "refuseOther".into();
    assert!(!still_called(
        &format!("{chain}require('./rust-a.cjs').refuseOwned(req, res, p, json)"),
        &st
    ));
}

#[test]
fn env_names_follow_the_recorder_s_pattern() {
    let src = "if (process.env.NOEVIA_RUST_X === '1') x(); const v = env.COWORK_Y; read('NOEVIA_Z_2'); \
               process.env.HOME; env.NOEVIA_; env.N; 'NOEVIA_open; env.noevia_q; 'COWORK_W' + env.NOEVIA_RUST_X";
    assert_eq!(
        env_names(src),
        [
            "COWORK_W",
            "COWORK_Y",
            "NOEVIA_",
            "NOEVIA_RUST_X",
            "NOEVIA_Z_2"
        ]
    );
    assert!(env_names("module.exports = {};").is_empty());
}

#[test]
fn a_stub_that_gains_a_switch_on_core_main_is_kept() {
    let rec = recording(vec![stub(
        "server/rust-a.cjs",
        2,
        &["server/index.cjs"],
        &["refuseOwned"],
    )]);
    let t = tree(&[
        (
            "server/index.cjs",
            "async function handleRequestInner(req, res) {\nrequire('./rust-a.cjs').refuseOwned(req, res, p, json);",
        ),
        ("server/rust-a.cjs", "process.env.NOEVIA_RUST_A\n"),
    ]);
    let r = recheck(&rec, &t).unwrap();
    assert_eq!(
        r.drift,
        [Drift::Changed {
            file: "server/rust-a.cjs".into(),
            field: "env",
            added: vec!["NOEVIA_RUST_A".into()],
            removed: vec![],
        }]
    );
    let front = |_: &str, _: &str| true;
    assert_eq!(
        classify(&r.recording, &front)[0].verdict,
        Verdict::SwitchGated
    );
}

/// The stub `server/rust-a.cjs` (refusing `GET /api/rust-a`) rechecked against a tree whose only other core line
/// naming the route is `line` in server/routes/a.cjs.
fn recheck_with_handler_line(line: &str) -> Rechecked {
    let rec = recording(vec![stub(
        "server/rust-a.cjs",
        2,
        &["server/index.cjs"],
        &["refuseOwned"],
    )]);
    let t = tree(&[
        (
            "server/index.cjs",
            "async function handleRequestInner(req, res) {\nrequire('./rust-a.cjs').refuseOwned(req, res, p, json);",
        ),
        ("server/rust-a.cjs", "x\n"),
        ("server/routes/a.cjs", line),
    ]);
    recheck(&rec, &t).unwrap()
}

#[test]
fn a_new_handler_for_another_method_is_read_as_such_and_does_not_guard_the_stub() {
    // A PUT handler behind a GET refusal (idle-unload's shape): the recorder's methodsNear names PUT, so the stub
    // is still a plain file delete at the new core.
    let r = recheck_with_handler_line(
        "if (req.method === 'PUT' && p === '/api/rust-a') return put(req, res);",
    );
    assert_eq!(
        r.recording.stubs[0].behind_methods,
        BTreeMap::from([("server/routes/a.cjs:1".to_string(), vec!["PUT".to_string()])])
    );
    let front = |_: &str, _: &str| true;
    assert_eq!(
        classify(&r.recording, &front)[0].verdict,
        Verdict::DeleteFile
    );
}

#[test]
fn a_new_handler_for_the_refused_method_or_a_negated_test_still_guards_the_stub() {
    let front = |_: &str, _: &str| true;
    for line in [
        // Same method.
        "if (req.method === 'GET' && p === '/api/rust-a') return get(req, res);",
        // GET in a list with another method.
        "if (['GET', 'HEAD'].includes(req.method) && p === '/api/rust-a') return get(req, res);",
        // A negation names nothing: the line counts for every method (the safe side).
        "if (req.method !== 'PUT' && p === '/api/rust-a') return other(req, res);",
        // No method test at all.
        "if (p === '/api/rust-a') return any(req, res);",
    ] {
        let r = recheck_with_handler_line(line);
        assert_eq!(
            classify(&r.recording, &front)[0].verdict,
            Verdict::GuardsHandler {
                handlers: vec!["server/routes/a.cjs:1".into()]
            },
            "{line}"
        );
    }
}

#[test]
fn behind_methods_reads_only_the_hit_lines_of_the_unescaped_handler_text() {
    let handlers: BTreeMap<&str, Vec<String>> = BTreeMap::from([(
        "server/routes/a.cjs",
        vec![
            "if (req.method === 'POST') x();".to_string(),
            "if (/^\\/api\\/a$/.test(p) && req.method === 'DELETE') y();".replace("\\/", "/"),
            "z();".to_string(),
        ],
    )]);
    let behind = BTreeMap::from([(
        "/api/a".to_string(),
        vec![
            "server/routes/a.cjs:2".to_string(),
            "server/routes/a.cjs:3".to_string(),
            // A hit outside the file, a line past its end and a malformed hit name nothing.
            "server/routes/b.cjs:1".to_string(),
            "server/routes/a.cjs:9".to_string(),
            "server/routes/a.cjs:0".to_string(),
            "server/routes/a.cjs".to_string(),
        ],
    )]);
    assert_eq!(
        behind_methods(&handlers, &behind),
        BTreeMap::from([(
            "server/routes/a.cjs:2".to_string(),
            vec!["DELETE".to_string()]
        )])
    );
}
