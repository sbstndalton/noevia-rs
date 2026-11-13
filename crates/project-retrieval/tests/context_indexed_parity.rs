#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! F7 with an index (`generate_context_fixtures.cjs`): noevia-core's `filesContext` over an index
//! Node wrote, so its own search found the excerpts. Here the native search runs over the rebuilt
//! index with the query vector Node bound, and the native `files_context` over its hits: the prompt
//! text and the placed and injected sources must be Node's.

use project_retrieval::context::{files_context, FileIn, Input, DEFAULT_MAX_CHARS};
use project_retrieval::search::{open_index, search, Target, Versions, TOP_K};
use serde_json::Value;

mod support;

fn fixture() -> Value {
    let path = format!(
        "{}/tests/fixtures/F7-files-context-indexed.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn sources(v: &Value) -> Vec<(String, String, Option<f64>, String)> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["file"].as_str().unwrap().to_owned(),
                s["body"].as_str().unwrap().to_owned(),
                s["score"].as_f64(),
                s["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn files_context_over_the_native_search_matches_node() {
    let f = fixture();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("qa-proj-ctx.db");
    support::rebuild(&file, &f["index"]);
    let conn = open_index(&file).unwrap().unwrap();
    let target = Target {
        profile: f["target"]["profile"].as_str().unwrap(),
        model: f["target"]["model"].as_str().unwrap(),
    };
    let mut with_excerpts = 0;
    for case in f["cases"].as_array().unwrap() {
        let query = case["query"].as_str().unwrap();
        let files: Vec<FileIn> = f["files"][case["set"].as_str().unwrap()]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| FileIn {
                name: x["name"].as_str().unwrap().to_owned(),
                content: x["content"].as_str().unwrap().to_owned(),
                notice: None,
                attachment_reason: x["attachment"]["reason"].as_str().map(str::to_owned),
            })
            .collect();
        let vector: Vec<f32> = case["query_vector"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap() as f32)
            .collect();
        let hits = search(&conn, target, &vector, TOP_K, &Versions::new()).unwrap();
        let got = files_context(&Input {
            query,
            files: &files,
            excerpts: &hits,
            attached: case["attached"].as_bool().unwrap(),
            max_chars: DEFAULT_MAX_CHARS,
        })
        .unwrap();
        assert_eq!(got.text, case["text"].as_str().unwrap(), "{query}");
        let mine = |list: &[project_retrieval::context::Source]| {
            list.iter()
                .map(|s| (s.file.clone(), s.body.clone(), s.score, s.kind.to_owned()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            mine(&got.placed),
            sources(&case["placed"]),
            "{query}: placed"
        );
        assert_eq!(
            mine(&got.injected),
            sources(&case["injected"]),
            "{query}: injected"
        );
        with_excerpts += usize::from(got.injected.iter().any(|s| s.kind == "excerpt"));
    }
    assert!(
        with_excerpts >= 3,
        "the recording exercises the excerpt path"
    );
}
