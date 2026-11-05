#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use embedding_worker::{
    migration::{parity_vectors, served_result},
    Target,
};
use serde_json::json;
fn target() -> Target {
    Target {
        model: "fixture".into(),
        profile: "nomic-v1".into(),
        base_url: None,
    }
}
#[test]
fn served_refusals_match_node_and_never_reflect_transport_errors() {
    assert!(served_result(&target(), Ok(vec!["FIXTURE:latest".into()])).is_ok());
    assert_eq!(
        served_result(&target(), Err("synthetic-reflected-token".into()))
            .unwrap_err()
            .body,
        json!({"code":"endpointUnverified","error":"Could not confirm that the embeddings server serves fixture."})
    );
    assert_eq!(
        served_result(&target(), Ok(vec![])).unwrap_err().body,
        json!({"code":"modelNotServed","served":[],"error":"The embeddings server does not serve fixture."})
    );
    assert_eq!(
        served_result(&target(), Ok(vec!["/model/a.gguf".into()]))
            .unwrap_err()
            .body,
        json!({"code":"modelNotServed","served":["/model/a.gguf"],"error":"The embeddings server lists only a model file, not model names, so it cannot be confirmed that it serves fixture. Start it with --alias fixture (or load fixture there)."})
    );
    let error = served_result(&target(), Ok(vec!["x".repeat(200); 40])).unwrap_err();
    assert_eq!(error.body["served"].as_array().unwrap().len(), 20);
    assert_eq!(error.body["served"][0].as_str().unwrap().len(), 120);
}
#[test]
fn parity_requires_four_finite_same_dimension_nonzero_vectors() {
    let a = vec![vec![1., 0.]; 4];
    assert_eq!(parity_vectors(&a, &a), json!({"ok":true,"minCosine":1.}));
    for b in [
        vec![],
        vec![vec![0., 0.]; 4],
        vec![vec![0., 1.]; 4],
        vec![vec![1.]; 4],
        vec![vec![f32::NAN, 0.]; 4],
    ] {
        assert_eq!(parity_vectors(&a, &b)["ok"], false);
    }
    assert_eq!(
        parity_vectors(&a, &vec![vec![0., 0.]; 4])["minCosine"],
        serde_json::Value::Null
    );
}
