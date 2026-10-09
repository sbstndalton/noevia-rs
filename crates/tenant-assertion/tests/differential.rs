//! Replays tests/fixtures/tenant-assertion.v1.json (tools/gen-tenant-assertion.py, generated from
//! noevia-services' diary/agent/tenant_assertion.py) through the library.

use serde_json::Value;
use tenant_assertion::{secret_ref_matches, verify, Reject, Request};

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2).unwrap_or("zz"), 16).unwrap_or(0))
        .collect()
}

fn decide(i: &Value) -> String {
    let s = |k: &str| i.get(k).and_then(Value::as_str).unwrap_or("<missing>");
    let result = if s("op") == "verify" {
        let query = unhex(s("query_hex"));
        verify(&Request {
            key: s("key"),
            user_id: s("user_id"),
            assertion: s("assertion"),
            method: s("method"),
            path: s("path"),
            query: &query,
            body_hash: s("body_hash"),
            storage: s("storage"),
            legacy_owner: s("legacy_owner"),
            blocked: s("blocked"),
            now: s("now").parse().unwrap_or(f64::NAN),
        })
    } else if secret_ref_matches(s("key"), s("user_id"), s("secret"), s("ref")) {
        Ok(())
    } else {
        Err(Reject::BadSignature)
    };
    match result {
        Ok(()) => "accept".into(),
        Err(r) => format!("reject: {}", r.reason()),
    }
}

#[test]
fn fixtures_match_python_and_never_accept_what_python_rejects() {
    let text = include_str!("fixtures/tenant-assertion.v1.json");
    let doc: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let cases = doc
        .get("cases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(cases.len() > 400, "fixture table missing or truncated");
    let mut accepted = 0;
    for c in &cases {
        let name = c.get("name").and_then(Value::as_str).unwrap_or("?");
        let input = c.get("input").unwrap_or(&Value::Null);
        let python = c.get("python").and_then(Value::as_str).unwrap_or("?");
        let recorded = c.get("rust").and_then(Value::as_str).unwrap_or("?");
        let got = decide(input);
        assert_eq!(
            got, recorded,
            "{name}: library differs from the recorded binary decision"
        );
        if python != "accept" {
            assert_ne!(got, "accept", "{name}: accepts what Python rejects");
        }
        if got == "accept" {
            accepted += 1;
        }
        if c.get("stricter").is_none() && !name.starts_with("mutant") {
            assert_eq!(got, python, "{name}");
        }
    }
    assert!(accepted >= 15, "too few accepting cases ({accepted})");
}
