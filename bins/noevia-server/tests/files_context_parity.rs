//! Council p9 (retrieval): `native_files_context::answer_with` replayed against core `rag.cjs`
//! `filesContext` as recorded by tests/record_files_context.cjs (fixtures/files-context.v1.json):
//! the same files, question, options and search answer give the same block, the same
//! `onSources(placed, injected)` report, the same number of searches, and a throw where Node throws.
//!
//! Until the recording is committed this test prints SKIP; with FILES_CONTEXT_ORACLE_REQUIRED=1 a
//! missing recording fails. Record it on the N5:
//! `n5run --with <core checkout> --pull -- 'node bins/noevia-server/tests/record_files_context.cjs <core checkout>'`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::cell::Cell;

use chat_prepare::retrieval::{Answer, Request, Source};
use js_json::JValue;
use noevia_server::native_files_context::{answer_with, Native};
use noevia_server::native_retrieval::Outcome;
use project_retrieval::hits::Hit;
use serde_json::Value;

fn recording() -> Option<Value> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("files-context.v1.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(serde_json::from_str(&text).unwrap()),
        Err(_) if std::env::var("FILES_CONTEXT_ORACLE_REQUIRED").as_deref() == Ok("1") => {
            panic!(
                "FILES_CONTEXT_ORACLE_REQUIRED=1 but {} is missing",
                path.display()
            )
        }
        Err(_) => None,
    }
}

fn jvalue(v: &Value) -> JValue {
    js_json::parse(&v.to_string()).unwrap()
}

fn back(v: &JValue) -> Value {
    serde_json::from_str(&js_json::stringify(v).unwrap()).unwrap()
}

#[tokio::test]
async fn native_files_context_matches_nodes_recording() {
    let Some(rec) = recording() else {
        eprintln!("SKIP files_context_parity: no recording (tests/record_files_context.cjs)");
        return;
    };
    let cases = rec["cases"].as_array().unwrap();
    assert!(cases.len() >= 10, "recorded {} cases", cases.len());
    let project = JValue::Str("qa-proj".into());
    for c in cases {
        let note = c["note"].as_str().unwrap();
        let files: Vec<Source> = c["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| Source {
                file: jvalue(&f["file"]),
                external: f["external"].as_bool().unwrap_or(false),
            })
            .collect();
        let query = c["query"].as_str().unwrap();
        let req = Request {
            project_id: &project,
            readable: (0..files.len()).collect(),
            files,
            query,
            user: "qa-user",
            attached: c["attached"].as_bool().unwrap_or(false),
            images: false,
        };
        let hits: Vec<Hit> = c["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| Hit {
                file: h["file"].as_str().unwrap().to_owned(),
                body: h["body"].as_str().unwrap().to_owned(),
                score: h["score"].as_f64().unwrap(),
            })
            .collect();
        let searched = Cell::new(0_u64);
        let ours = answer_with(
            &req,
            c["rag"].as_bool().unwrap(),
            120_000,
            |_project: String| {
                searched.set(searched.get() + 1);
                async move { Outcome::Hits(hits) }
            },
        )
        .await;
        let want = &c["recorded"];
        assert_eq!(
            searched.get(),
            want["searched"].as_u64().unwrap(),
            "{note}: searches"
        );
        if want["threw"].as_bool().unwrap() {
            assert_eq!(ours, Native::Answer(Answer::Threw), "{note}");
            continue;
        }
        let Native::Answer(Answer::Answered { block, reported }) = ours else {
            panic!("{note}: not answered natively");
        };
        assert_eq!(block.as_deref(), want["block"].as_str(), "{note}: block");
        match (reported, &want["reported"]) {
            (None, Value::Null) => {}
            (Some((placed, injected)), w) => {
                assert_eq!(back(&placed), w["placed"], "{note}: placed");
                assert_eq!(back(&injected), w["injected"], "{note}: injected");
            }
            (None, w) => panic!("{note}: Node reported {w}, native did not"),
        }
    }
}
