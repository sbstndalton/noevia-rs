//! `limit.rs` replayed against core's own `resolveRuntimeLimit` (tests/record_hosted_limit.cjs; 72 cases: every
//! kind of `contextTokens` a row may hold, with the manager absent and off, and the local fallback).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_upstream::limit;
use js_json::JValue;

fn fixture() -> JValue {
    let text = include_str!("fixtures/node-hosted-limit.v1.json");
    js_json::parse(text).unwrap()
}

#[test]
fn every_recorded_row_matches() {
    let f = fixture();
    assert_eq!(
        f.get("hostedDefault").as_num(),
        Some(limit::HOSTED_DEFAULT_LIMIT as f64)
    );
    let local = limit::without_manager(None);
    assert_eq!(f.get("local").get("limit").as_num(), Some(local.0 as f64));
    assert_eq!(f.get("local").get("limitSource").as_str(), Some(local.1));
    let JValue::Arr(cases) = f.get("cases") else {
        panic!("no cases")
    };
    assert_eq!(cases.len(), 72);
    let mut stated = 0;
    for c in cases {
        let row = js_json::parse(c.get("row").as_str().unwrap()).unwrap();
        let (n, source) = limit::without_manager(Some(&row));
        assert_eq!(c.get("limit").as_num(), Some(n as f64), "{row:?}");
        assert_eq!(c.get("limitSource").as_str(), Some(source), "{row:?}");
        if source == limit::PROVIDER_SETTING {
            stated += 1;
        }
    }
    // The recording exercises both answers (whole numbers inside the bounds are the stated ones).
    assert_eq!(stated, 2 * 9);
}

#[test]
fn bounds() {
    let n = |x: f64| limit::valid_context_tokens(&JValue::Num(x));
    assert_eq!(n(2048.0), Some(2048));
    assert_eq!(n(2_000_000.0), Some(2_000_000));
    assert_eq!(n(2047.0), None);
    assert_eq!(n(2_000_001.0), None);
    assert_eq!(n(f64::NAN), None);
    assert_eq!(n(f64::INFINITY), None);
    assert_eq!(n(4096.5), None);
    assert_eq!(limit::valid_context_tokens(&JValue::Undefined), None);
}
