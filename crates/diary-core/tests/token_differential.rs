//! Replays `token-estimate-fixtures.json`, recorded from the frozen Python `estimate_tokens`
//! (`diary/agent/util.py`, sha256 in the file) by `generate_token_fixtures.py`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use diary_core::tokens::estimate_tokens;
use serde_json::Value;

#[test]
fn estimate_tokens_matches_python_for_every_recorded_text() {
    let fixture: Value =
        serde_json::from_str(include_str!("token-estimate-fixtures.json")).expect("fixture");
    assert_eq!(
        fixture["source_sha256"],
        "f1cfedf68937724331989b9083e2fff661b0c8b09db86659f32e41803ea4dfe9"
    );
    let cases = fixture["cases"].as_array().expect("cases");
    assert!(cases.len() >= 90);
    for case in cases {
        let text = case["text"].as_str().expect("text");
        assert_eq!(
            Some(estimate_tokens(text)),
            case["tokens"].as_u64(),
            "len {}",
            text.chars().count()
        );
    }
}
