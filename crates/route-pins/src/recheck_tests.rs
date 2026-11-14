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
            "require('./rust-a.cjs').refuseOwned(req, res);\n",
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
            "require('./rust-a.cjs').refuseOwned(req, res);",
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
        ("server/index.cjs", "require('./rust-a.cjs').owned(req);"),
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
        Rechecked::kept_as_recorded().len(),
        7,
        "the facts left as recorded are named in the module docs"
    );
}
