#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! F14: `searchProjectWithImages` (#1726) recorded from noevia-core `rag.cjs` over an index Node wrote
//! (`generate_image_fixtures.cjs`: text through `indexProjectFile`, images through `syncProjectMedia`),
//! under six `RAG_IMAGE_TOP_K` / `RAG_IMAGE_MIN_SCORE` settings. Each index is rebuilt from its dump;
//! every search must give Node's `{text, images, indexed}`.

use project_retrieval::search::{
    open_index, search_with_images, ImageSettings, Target, Versions, TOP_K,
};
use serde_json::{json, Value};

mod support;

fn fixture() -> Value {
    let path = format!(
        "{}/tests/fixtures/F14-image-search.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A JSON value with every number as f64, so `1` and `1.0` compare equal.
fn norm(v: &Value) -> Value {
    match v {
        Value::Number(n) => json!(n.as_f64().unwrap()),
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), norm(v))).collect()),
        other => other.clone(),
    }
}

#[test]
fn every_recorded_image_search_matches_node() {
    let f = fixture();
    let mut checked = 0;
    for setting in f["settings"].as_array().unwrap() {
        let env = &setting["env"];
        let settings = ImageSettings::from_env(
            env["RAG_IMAGE_TOP_K"].as_str(),
            env["RAG_IMAGE_MIN_SCORE"].as_str(),
        );
        for q in setting["searches"].as_array().unwrap() {
            let ctx = format!("{} / {}", setting["name"], q["query"]);
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("qa-proj-img.db");
            support::rebuild(&file, &f["snapshots"][q["index"].as_str().unwrap()]);
            let conn = open_index(&file).unwrap().unwrap();
            let query: Vec<f32> = q["query_vector"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_f64().unwrap() as f32)
                .collect();
            let target = Target {
                profile: q["target"]["profile"].as_str().unwrap(),
                model: q["target"]["model"].as_str().unwrap(),
            };
            let found =
                search_with_images(&conn, target, &query, TOP_K, &Versions::new(), settings)
                    .unwrap();
            let got_rows: Vec<String> = found
                .images
                .iter()
                .map(|i| i.row.meta.clone())
                .chain(found.indexed.iter().map(|i| i.meta.clone()))
                .collect();
            let got = found;
            let got = json!({
                "text": got.text.iter().map(|h| json!({"file": h.file, "body": h.body, "score": h.score})).collect::<Vec<_>>(),
                "images": got.images.iter().map(|i| json!({"file": i.row.file, "kind": "image", "meta": serde_json::from_str::<Value>(&i.row.meta).unwrap(), "label": i.label, "hash": i.hash, "score": i.score})).collect::<Vec<_>>(),
                "indexed": got.indexed.iter().map(|i| json!({"file": i.file, "kind": "image", "meta": serde_json::from_str::<Value>(&i.meta).unwrap()})).collect::<Vec<_>>(),
            });
            assert_eq!(norm(&got), norm(&q["out"]), "{ctx}");
            // Node's JSON.stringify(JSON.parse(meta)) of a meta Node wrote is the stored text.
            let stored: Vec<&str> = f["snapshots"][q["index"].as_str().unwrap()]["chunks"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|c| c["meta"].as_str())
                .collect();
            let emitted = got_rows.iter().map(String::as_str);
            for meta in emitted {
                assert!(stored.contains(&meta), "{ctx}: {meta}");
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 30);
    // The recording covers what the module promises: an unreadable meta row is never listed.
    let all = f.to_string();
    assert!(
        all.contains("not json")
            && !f["settings"][0]["searches"][0]["out"]
                .to_string()
                .contains("qa-broken.png")
    );
}
