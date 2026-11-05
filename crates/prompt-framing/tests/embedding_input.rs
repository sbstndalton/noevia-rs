//! Generated from unmodified Core b91b3f03 server/embedding-profile.cjs.
//! UTF-16 arrays preserve JS strings including lone surrogates and slice splits.
#![allow(clippy::unwrap_used)]
use prompt_framing::embedding_input::{frame, Kind, Profile};
use serde_json::Value;
#[test]
fn core_embedding_profile_oracle() {
    let cases: Value = serde_json::from_str(include_str!("fixtures/embedding-input.json")).unwrap();
    let mut count = 0;
    for case in cases.as_array().unwrap() {
        let units = |value: &Value| {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|n| u16::try_from(n.as_u64().unwrap()).unwrap())
                .collect::<Vec<_>>()
        };
        let profile = Profile::try_from(case.get("profile").unwrap().as_str().unwrap()).unwrap();
        let kind = Kind::try_from(case.get("kind").unwrap().as_str().unwrap()).unwrap();
        let text = units(case.get("text").unwrap());
        let title = case.get("title").filter(|t| !t.is_null()).map(units);
        assert_eq!(
            frame(profile, kind, &text, title.as_deref()),
            units(case.get("expected").unwrap())
        );
        count += 1;
    }
    assert_eq!(count, 198);
}
