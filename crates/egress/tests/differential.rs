//! Differential corpus: every case in tests/fixtures/egress-diff.json was evaluated by noevia's
//! own hostAllowed / parseTarget (code-egress.cjs), isPrivateIp (ssrf.cjs) and Node's
//! net.isIP (tools/egress-diff.cjs). The Rust port must agree on every one.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use egress::{host_allowed, is_ip, is_private_ip, parse_target};
use serde_json::Value as J;

fn corpus() -> J {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/egress-diff.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn rust_agrees_with_js_on_every_case() {
    let corpus = corpus();
    let cases = corpus["cases"].as_array().unwrap();
    assert_eq!(cases.len() as u64, corpus["count"].as_u64().unwrap());
    assert!(cases.len() >= 150);
    let mut failures = Vec::new();
    let mut per_fn = std::collections::BTreeMap::<String, usize>::new();
    for case in cases {
        let f = case["fn"].as_str().unwrap();
        *per_fn.entry(f.to_owned()).or_default() += 1;
        let expect = &case["expect"];
        let got: J = match f {
            "isIP" => J::from(is_ip(case["input"].as_str().unwrap())),
            "isPrivateIp" => J::from(is_private_ip(case["input"].as_str().unwrap())),
            "hostAllowed" => {
                let domains: Vec<&str> = case["domains"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|d| d.as_str().unwrap())
                    .collect();
                J::from(host_allowed(case["host"].as_str().unwrap(), &domains))
            }
            "parseTarget" => {
                let port = u16::try_from(case["defaultPort"].as_u64().unwrap()).unwrap();
                match parse_target(case["raw"].as_str().unwrap(), port) {
                    Some(t) => serde_json::json!({ "host": t.host, "port": t.port }),
                    None => J::Null,
                }
            }
            other => panic!("unknown fn {other}"),
        };
        if &got != expect {
            failures.push(format!("{case} -> rust {got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases disagree with JS:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
    eprintln!("agreed on {} cases: {per_fn:?}", cases.len());
}

/// The proxy connects to an IP literal by parsing it with std, so std must accept every
/// zone-less literal Node accepts (and nothing Node rejects).
#[test]
fn std_parses_exactly_the_zone_less_literals_node_accepts() {
    let corpus = corpus();
    for case in corpus["cases"].as_array().unwrap() {
        if case["fn"] != "isIP" {
            continue;
        }
        let s = case["input"].as_str().unwrap();
        if s.contains('%') {
            continue;
        }
        let node = case["expect"].as_u64().unwrap();
        let std_v = match s.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(_)) => 4,
            Ok(std::net::IpAddr::V6(_)) => 6,
            Err(_) => 0,
        };
        assert_eq!(std_v, node, "std vs node isIP on {s:?}");
    }
}
