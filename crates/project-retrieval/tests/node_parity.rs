#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

//! Differential checks against outputs recorded from noevia-core `server/rag.cjs` (NODE_ENV=test,
//! synthetic inputs). A failure here is a finding about the port, not a fixture to regenerate.

use project_retrieval::context::{files_context, FileIn, Input, DEFAULT_MAX_CHARS};
use project_retrieval::names::question_names;
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(path).expect("fixture file");
    serde_json::from_str(&text).expect("fixture json")
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[test]
fn question_names_matches_node_for_every_recorded_pair() {
    let f8 = fixture("F8-question-names.json");
    let cases = f8["cases"].as_array().expect("cases");
    assert!(cases.len() >= 12);
    for case in cases {
        let (name, query) = (str_of(case, "name"), str_of(case, "query"));
        let want = case["named"].as_bool().expect("named");
        assert_eq!(
            question_names(query, name),
            want,
            "name={name:?} query={query:?}"
        );
    }
}

fn source_list(v: &Value) -> Vec<(String, String, String)> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|s| {
                    (
                        str_of(s, "file").to_owned(),
                        str_of(s, "body").to_owned(),
                        str_of(s, "kind").to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn check_files_context(fixture_name: &str, max_chars: usize) {
    let f7 = fixture(fixture_name);
    let cases = f7["cases"].as_array().expect("cases");
    assert!(!cases.is_empty());
    for case in cases {
        let scenario = str_of(case, "scenario");
        let input = &case["input"];
        let files: Vec<FileIn> = input["files"]
            .as_array()
            .expect("files")
            .iter()
            .map(|f| FileIn {
                name: str_of(f, "name").to_owned(),
                content: str_of(f, "content").to_owned(),
                notice: None,
                attachment_reason: f["attachment"]["reason"].as_str().map(str::to_owned),
            })
            .collect();
        let query = str_of(input, "query");
        let got = files_context(&Input {
            query,
            files: &files,
            excerpts: &[],
            attached: input["attached"].as_bool().unwrap_or(false),
            max_chars,
        })
        .expect("files present");
        assert_eq!(
            got.text,
            str_of(case, "output"),
            "{fixture_name} {scenario}: text"
        );
        let placed: Vec<_> = got
            .placed
            .iter()
            .map(|s| (s.file.clone(), s.body.clone(), s.kind.to_owned()))
            .collect();
        let injected: Vec<_> = got
            .injected
            .iter()
            .map(|s| (s.file.clone(), s.body.clone(), s.kind.to_owned()))
            .collect();
        assert_eq!(
            placed,
            source_list(&case["placed"]),
            "{fixture_name} {scenario}: placed"
        );
        assert_eq!(
            injected,
            source_list(&case["injected"]),
            "{fixture_name} {scenario}: injected"
        );
    }
}

#[test]
fn files_context_matches_node_with_default_budget() {
    check_files_context("F7-files-context.json", DEFAULT_MAX_CHARS);
}

#[test]
fn files_context_matches_node_when_the_budget_omits_files() {
    // The recorder ran with FILES_CONTEXT_MAX_CHARS=4000 (the clamp floor).
    check_files_context("F7-files-context-budget-4000.json", 4_000);
}

#[test]
fn no_files_gives_no_context() {
    let got = files_context(&Input {
        query: "q",
        files: &[],
        excerpts: &[],
        attached: false,
        max_chars: DEFAULT_MAX_CHARS,
    });
    assert!(got.is_none());
}
