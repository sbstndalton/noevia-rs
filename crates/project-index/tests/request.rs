#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! The store route's JSON contract ([`project_index::request`]): tail items read as Node reads them
//! (recorded), the three writes answer what the indexer answers, and malformed requests are refused.

use project_index::embed::Vector;
use project_index::indexer::{Host, Indexer};
use project_index::request::{answer, tail_item};
use project_index::slots::Target;
use serde_json::{json, Value};
use std::path::PathBuf;

#[test]
fn tail_items_are_read_as_node_reads_them() {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tail-items.json");
    let f: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    let input = f["input"].as_array().unwrap();
    let node = f["node"].as_array().unwrap();
    assert_eq!(input.len(), node.len());
    for (i, (item, want)) in input.iter().zip(node).enumerate() {
        let got = tail_item(item).map(|t| json!({ "body": t.body, "page": t.page }));
        assert_eq!(got.unwrap_or(Value::Null), *want, "item {i}: {item}");
    }
}

struct TestHost {
    dir: PathBuf,
}

impl Host for TestHost {
    fn index_path(&self, project: &str, user: Option<&str>) -> PathBuf {
        self.dir
            .join(user.unwrap_or("shared"))
            .join(format!("{project}.db"))
    }
    fn user_active(&self, _user: &str) -> bool {
        true
    }
    fn lifecycle_generation(&self, _user: Option<&str>) -> u64 {
        0
    }
    fn deleted_since(&self, _project: &str, _user: Option<&str>, _seen: u64) -> bool {
        false
    }
    fn active_target(&self) -> Target {
        Target {
            profile: "qa-profile".into(),
            model: "qa-model".into(),
        }
    }
    fn embedding_off(&self, _t: &Target) -> bool {
        false
    }
    fn foreground_busy(&self) -> bool {
        false
    }
}

fn vectors(texts: &[String]) -> Result<Vec<Vector>, String> {
    Ok(texts.iter().map(|_| Some(vec![1.0, 0.5, 0.25])).collect())
}

fn numbered(n: usize) -> String {
    (0..n)
        .map(|i| format!("Line {i} of the QA notes. "))
        .collect()
}

#[test]
fn the_three_writes_answer_what_the_indexer_answers() {
    sqlite_vec_loader::register().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ix = Indexer::new(TestHost {
        dir: tmp.path().to_path_buf(),
    });
    let mut embed = |_: &Target, t: &[String]| vectors(t);
    let head = numbered(300);
    let out = answer(
        &ix,
        "file",
        &json!({"projectId": "qa-proj", "fileName": "a.md", "text": head, "userId": "qa-user"}),
        &mut embed,
    );
    assert_eq!(out["ok"], true, "{out}");
    let stored = out["stored"].as_u64().unwrap();
    assert!(stored > 2);
    assert_eq!(out["embedded"].as_u64().unwrap(), stored);

    let out = answer(
        &ix,
        "tail",
        &json!({"projectId": "qa-proj", "fileName": "a.md", "headText": head, "userId": "qa-user",
            "tail": [{"body": "[Page 9]\nQA tail page nine."}, {"body": ""}, {"body": "QA tail ten.", "page": 10}]}),
        &mut embed,
    );
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["inserted"], 2, "{out}");

    // A tail for another head text: the finished run forgot the file's version (pruneFileState), so
    // Node answers from the index: the head on disk is not this text.
    let out = answer(
        &ix,
        "tail",
        &json!({"projectId": "qa-proj", "fileName": "a.md", "headText": numbered(301), "userId": "qa-user", "tail": []}),
        &mut embed,
    );
    assert_eq!(out["ok"], false);
    assert_eq!(out["reason"], "head-not-indexed", "{out}");

    let out = answer(
        &ix,
        "delete",
        &json!({"projectId": "qa-proj", "fileName": "a.md", "userId": "qa-user"}),
        &mut embed,
    );
    assert_eq!(out, json!({"ok": true}));
    let db = rusqlite::Connection::open(tmp.path().join("qa-user").join("qa-proj.db")).unwrap();
    let left: i64 = db
        .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
    assert_eq!(ix.tracked(), (0, 0, 0));
}

#[test]
fn malformed_requests_are_refused_without_writing() {
    sqlite_vec_loader::register().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ix = Indexer::new(TestHost {
        dir: tmp.path().to_path_buf(),
    });
    let mut embed = |_: &Target, t: &[String]| vectors(t);
    let bad = json!({"ok": false, "reason": "bad-request"});
    for (op, body) in [
        ("file", json!("not an object")),
        ("file", json!({"fileName": "a.md", "text": "x"})),
        (
            "file",
            json!({"projectId": 7, "fileName": "a.md", "text": "x"}),
        ),
        (
            "file",
            json!({"projectId": "qa-proj", "fileName": "a.md", "text": 5}),
        ),
        (
            "file",
            json!({"projectId": "qa-proj", "fileName": "a.md", "text": "x", "userId": 3}),
        ),
        ("tail", json!({"projectId": "qa-proj", "fileName": "a.md"})),
        (
            "tail",
            json!({"projectId": "qa-proj", "fileName": "a.md", "headText": "x", "limit": -1}),
        ),
        (
            "tail",
            json!({"projectId": "qa-proj", "fileName": "a.md", "headText": "x", "final": "yes"}),
        ),
        (
            "rename",
            json!({"projectId": "qa-proj", "fileName": "a.md"}),
        ),
    ] {
        assert_eq!(answer(&ix, op, &body, &mut embed), bad, "{op} {body}");
    }
    assert!(
        std::fs::read_dir(tmp.path()).unwrap().next().is_none(),
        "nothing was written"
    );
}
