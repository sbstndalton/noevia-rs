//! Replays `fixtures/tool-rate-limit.v1.json`, recorded from core's own `server/tool-rate-limit.cjs` by
//! `gen-fixtures.cjs` (see its header).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
use serde_json::Value;
use tool_rate_limit::*;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/tool-rate-limit.v1.json")).unwrap()
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

#[test]
fn the_constants_are_cores() {
    let f = fixture();
    assert_eq!(f["version"], 1);
    assert_eq!(f["source"]["path"], "server/tool-rate-limit.cjs");
    assert_eq!(f["source"]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(num(&f["constants"]["baseMs"]), Some(BASE_MS));
    assert_eq!(num(&f["constants"]["maxMs"]), Some(MAX_MS));
}

#[test]
fn rate_limited_text_matches_core_on_every_recorded_text() {
    let f = fixture();
    let cases = f["texts"].as_array().unwrap();
    assert!(cases.len() > 600);
    for c in cases {
        let text = c["text"].as_str().unwrap();
        assert_eq!(
            is_rate_limited_text(text),
            c["limited"].as_bool().unwrap(),
            "{text:?}"
        );
    }
}

/// What `hit` does with a parsed value: only a positive one is used (core `Number.isFinite(r) && r > 0`).
fn effective(ms: Option<f64>) -> Option<f64> {
    ms.filter(|m| m.is_finite() && *m > 0.0)
}

#[test]
fn retry_after_matches_core_wherever_hit_can_tell() {
    let f = fixture();
    let now = num(&f["now"]).unwrap();
    for c in f["retryAfter"].as_array().unwrap() {
        let value = c["value"].as_str();
        let ours = parse_retry_after(value, now);
        let core = num(&c["ms"]);
        // Exact for numbers and for the two date forms; V8's other spellings that land in the past give
        // core 0 and us `None`, which `hit` treats alike (the named difference in the crate docs).
        assert_eq!(effective(ours), effective(core), "{value:?}");
        if ours.is_some() {
            assert_eq!(ours, core, "{value:?}");
        }
    }
}

#[test]
fn a_spelling_only_v8_reads_is_no_retry_after_here() {
    // The named difference: V8's Date.parse reads this as a future date; we do not, so `hit` backs off.
    assert_eq!(
        parse_retry_after(Some("Friday, 13-Nov-26 04:31:00 GMT"), 0.0),
        None
    );
    assert_eq!(parse_retry_after(Some("Nov 13 2026"), 0.0), None);
}

#[test]
fn the_limiter_replays_every_recorded_sequence() {
    let f = fixture();
    for (n, steps) in f["sequences"].as_array().unwrap().iter().enumerate() {
        let mut clock = 1_000_000.0;
        let mut limit = RateLimit::default();
        for (k, s) in steps.as_array().unwrap().iter().enumerate() {
            let at = format!("sequence {n} step {k}: {s}");
            let key = s["key"].as_str().unwrap_or("");
            match s["op"].as_str().unwrap() {
                "advance" => clock += num(&s["by"]).unwrap(),
                "remaining" => {
                    assert_eq!(Some(limit.remaining(key, clock)), num(&s["out"]), "{at}");
                }
                "hit" => {
                    let r = num(&s["retryAfterMs"]);
                    assert_eq!(Some(limit.hit(key, r, clock)), num(&s["out"]), "{at}");
                }
                "clear" => limit.clear(key),
                other => panic!("{other}"),
            }
            if let Some(size) = s["size"].as_u64() {
                assert_eq!(limit.len() as u64, size, "{at}");
            }
        }
    }
}

#[test]
fn the_models_texts_are_cores() {
    let f = fixture();
    for c in f["messages"].as_array().unwrap() {
        let (name, ms) = (c["name"].as_str().unwrap(), num(&c["ms"]).unwrap());
        assert_eq!(rate_limited_text(name, ms), c["limited"].as_str().unwrap());
        assert_eq!(cooling_down_text(name, ms), c["cooling"].as_str().unwrap());
    }
}
