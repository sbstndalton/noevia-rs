#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use proptest::prelude::*;
use serde_json::{json, Value};
use server_usage::summary::{summarize, Clock, Usage};
fn same(a: &Value, b: &Value, path: &str) {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.as_f64(), b.as_f64(), "{path}"),
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}");
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                same(a, b, &format!("{path}/{i}"));
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(a.len(), b.len(), "{path}");
            for (k, a) in a {
                same(a, b.get(k).expect(k), &format!("{path}/{k}"));
            }
        }
        _ => assert_eq!(a, b, "{path}"),
    }
}
#[test]
fn pinned_node_reporting() {
    let text = std::env::var("NOEVIA_USAGE_FIXTURE")
        .map(|p| std::fs::read_to_string(p).unwrap())
        .unwrap_or_else(|_| include_str!("fixtures/node-usage.v1.json").to_string());
    let f: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        f["core"],
        std::env::var("NOEVIA_USAGE_CORE_SHA")
            .unwrap_or_else(|_| "b068daa7454de0a2f113571d4e56ee464f6f7470".into())
    );
    let ordered: server_usage::input::Input = serde_json::from_str(&text).unwrap();
    let server_usage::input::Input::Array(cases) = ordered.get("cases").unwrap() else {
        panic!("cases")
    };
    for (c, input) in f["cases"].as_array().unwrap().iter().zip(cases) {
        let clock = Clock::new(c["tz"].as_str().unwrap(), c["locale"].as_str().unwrap()).unwrap();
        let mut usage = Usage::default();
        usage
            .merge_input(input.get("store").unwrap(), false)
            .unwrap();
        let got = summarize(
            &usage,
            &clock,
            c["nowMs"].as_i64().unwrap(),
            c["retentionDays"].as_u64().unwrap() as usize,
        )
        .unwrap();
        same(&got, &c["expected"], c["name"].as_str().unwrap());
    }
}
proptest! {
 #[test]
 fn counters_remain_bounded(a in -1e20f64..1e20,b in -1e20f64..1e20){let mut u=Usage::default();u.merge(&json!({"days":{"2026-10-10":{"input":a,"output":b,"replies":0.5}}}),false).unwrap();let c=Clock::new("UTC","en-US").unwrap();let v=summarize(&u,&c,1791633600000,365).unwrap();for k in ["input","output","replies"] {let n=v["allTime"][k].as_f64().unwrap();prop_assert!((0.0..=server_usage::SAFE_INTEGER).contains(&n));}prop_assert_eq!(v["currentStreak"].as_u64(),Some(1));}
}
#[test]
fn mutation_control_detects_clamp_omission() {
    let raw = json!(1e99);
    assert_ne!(server_usage::summary::count(&raw), raw.as_f64().unwrap());
    assert_eq!(
        server_usage::summary::count(&raw),
        server_usage::SAFE_INTEGER
    );
}

#[test]
fn timezone_aliases_use_node_canonical_display_names() {
    let clock = Clock::new("US/Eastern", "en-US").unwrap();
    assert_eq!(clock.timezone, "America/New_York");
    assert_eq!(Clock::new("Etc/UTC", "en-US").unwrap().timezone, "UTC");
    assert!(Clock::new("invalid-timezone", "en-US").is_err());
}
