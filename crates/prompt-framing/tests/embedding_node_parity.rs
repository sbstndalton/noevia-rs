//! Differential check of `embedding_input::frame` against outputs recorded from noevia-core
//! `server/embedding-profile.cjs` (`frame`, NODE_ENV=test, synthetic texts and titles).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use prompt_framing::embedding_input::{frame, Kind, Profile};
use prompt_framing::js::units;
use serde_json::Value;

fn kind_of(name: &str) -> Kind {
    Kind::try_from(name).expect("known kind")
}

fn profile_of(name: &str) -> Profile {
    Profile::try_from(name).expect("known profile")
}

#[test]
fn frame_matches_node_for_every_recorded_case() {
    let text = include_str!("fixtures/embedding-profile-F12.json");
    let fixture: Value = serde_json::from_str(text).expect("fixture json");
    let cases = fixture["cases"].as_array().expect("cases");
    assert_eq!(cases.len(), 12);
    for case in cases {
        let profile = profile_of(case["profile"].as_str().expect("profile"));
        let kind = kind_of(case["kind"].as_str().expect("kind"));
        let texts: Vec<&str> = case["texts"]
            .as_array()
            .expect("texts")
            .iter()
            .map(|v| v.as_str().unwrap_or_default())
            .collect();
        let titles: Option<Vec<String>> = case["titles"].as_array().map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or_default().to_owned())
                .collect()
        });
        let expected: Vec<String> = case["out"]
            .as_array()
            .expect("out")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_owned())
            .collect();
        let mut got: Vec<String> = Vec::new();
        for (i, t) in texts.iter().enumerate() {
            let title = titles.as_ref().and_then(|ts| ts.get(i)).map(|s| units(s));
            let framed = frame(profile, kind, &units(t), title.as_deref());
            got.push(String::from_utf16_lossy(&framed));
        }
        assert_eq!(
            got, expected,
            "profile {:?} kind {:?} titles {:?}",
            case["profile"], case["kind"], titles
        );
    }
}
