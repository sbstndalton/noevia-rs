#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

//! The reranker and embedding-service decisions against outputs recorded from noevia-core
//! (`generate_policy_fixtures.cjs`): `rerank-target.cjs`, `rag.cjs` `embeddingOff` and
//! `rust-embedding-background.cjs` `snapshot()`.

use project_retrieval::policy::{current_snapshot, embedding_off, observe, rerank_target, Backend};
use serde_json::Value;
use std::collections::HashMap;

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn opt(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

#[test]
fn rerank_target_matches_node() {
    let f = fixture("policy-rerank-target.json");
    let cases = f["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 30);
    for case in cases {
        let env: HashMap<String, String> = case["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        let got = rerank_target(&env);
        let out = &case["out"];
        let ctx = format!("{env:?}");
        assert_eq!(got.enabled, out["enabled"].as_bool().unwrap(), "{ctx}");
        assert_eq!(
            got.backend.map(Backend::name),
            out["backend"].as_str(),
            "{ctx}"
        );
        assert_eq!(got.base_url, opt(&out["baseUrl"]), "{ctx}");
        assert_eq!(got.model, opt(&out["model"]), "{ctx}");
        assert_eq!(got.shared, out["shared"].as_bool().unwrap(), "{ctx}");
        assert_eq!(got.reason, out["reason"].as_str(), "{ctx}");
    }
}

#[test]
fn embedding_off_matches_node() {
    let f = fixture("policy-embedding-off.json");
    let now = f["now"].as_f64().unwrap();
    let cases = f["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 449);
    let mut off = 0;
    for case in cases {
        let snapshot = Some(&case["snapshot"]).filter(|s| !s.is_null());
        let last = observe(None, snapshot, now);
        let got = embedding_off(
            last.as_ref(),
            now,
            case["env"]["EMBEDDING_SERVICE_URL"].as_str(),
            case["target"]["baseUrl"].as_str(),
        );
        let want = case["off"].as_bool().unwrap();
        assert_eq!(got, want, "{}", serde_json::to_string(case).unwrap());
        off += usize::from(want);
    }
    assert!(off > 50 && off < 400, "the cases cover both answers");
}

/// One service across snapshots and clock steps: Node's last observation is kept, goes stale when
/// the snapshot stops coming, and an older snapshot does not replace it.
#[test]
fn embedding_off_sequences_match_node() {
    let f = fixture("policy-embedding-off.json");
    let sequences = f["sequences"].as_array().unwrap();
    assert_eq!(sequences.len(), 15);
    for seq in sequences {
        let mut last = None;
        for (i, step) in seq["steps"].as_array().unwrap().iter().enumerate() {
            let now = step["now"].as_f64().unwrap();
            last = observe(last, Some(&step["snapshot"]).filter(|s| !s.is_null()), now);
            let got = embedding_off(
                last.as_ref(),
                now,
                seq["env"]["EMBEDDING_SERVICE_URL"].as_str(),
                seq["target"]["baseUrl"].as_str(),
            );
            assert_eq!(
                got,
                step["off"].as_bool().unwrap(),
                "{} step {i} {}",
                seq["name"],
                seq["target"]
            );
        }
    }
}

#[test]
fn snapshot_acceptance_matches_node() {
    let f = fixture("policy-snapshot.json");
    let now = f["now"].as_f64().unwrap();
    for case in f["cases"].as_array().unwrap() {
        let file = case["text"]
            .as_str()
            .and_then(|t| serde_json::from_str::<Value>(t).ok());
        let got = file
            .as_ref()
            .and_then(|v| current_snapshot(v, case["start"].as_str().unwrap(), now))
            .is_some();
        assert_eq!(got, case["accepted"].as_bool().unwrap(), "{}", case["name"]);
    }
}
