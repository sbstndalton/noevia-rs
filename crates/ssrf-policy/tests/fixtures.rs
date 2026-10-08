//! The shared fixture table (`tests/fixtures/ssrf.v1.json`, byte-identical to noevia-core's,
//! printed by its `tools/gen-ssrf-fixtures.cjs` from the JS itself: `isPrivateIp`, `isPublicUrl`
//! up to its DNS step and `createPublicFetch` up to the socket).
//!
//! Addresses must agree exactly. For URLs, an acceptance must be one the JS makes too, with the
//! same host and kind; a refusal where the JS accepts must be one of the documented extra
//! strictness reasons; under fetch, a refusal the JS also makes must carry the JS's reason.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use serde_json::Value;
use ssrf_policy::{addresses_public, check_url, is_private_ip, run_json, Mode, Refusal};
use std::collections::BTreeMap;

fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/ssrf.v1.json")).unwrap()
}

#[test]
fn limits_match() {
    let f = fixtures();
    assert_eq!(f["version"], 1);
    assert_eq!(f["limits"]["maxUrlBytes"], ssrf_policy::MAX_URL_BYTES);
    assert_eq!(f["limits"]["maxAddresses"], ssrf_policy::MAX_ADDRESSES);
    assert_eq!(f["limits"]["maxInputBytes"], ssrf_policy::MAX_INPUT_BYTES);
}

#[test]
fn addresses_agree_exactly() {
    let f = fixtures();
    let rows = f["addresses"].as_array().unwrap();
    assert!(rows.len() >= 900);
    let (mut private, mut public) = (0, 0);
    for r in rows {
        let a = r["address"].as_str().unwrap();
        let js = r["private"].as_bool().unwrap();
        assert_eq!(is_private_ip(a), js, "{a:?}");
        assert_eq!(addresses_public(&[a]), !js, "{a:?}");
        let (s, reply) =
            run_json(&serde_json::json!({"op":"addresses","addresses":[a]}).to_string());
        assert_eq!((s, reply), (0, format!("{{\"public\":{}}}", !js)), "{a:?}");
        if js {
            private += 1
        } else {
            public += 1
        }
    }
    assert!(private >= 600 && public >= 150, "{private} {public}");
}

#[test]
fn urls_never_allow_more_than_the_js() {
    let f = fixtures();
    let rows = f["urls"].as_array().unwrap();
    assert!(rows.len() >= 4000);
    let mut extra: BTreeMap<String, usize> = BTreeMap::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut mismatches: Vec<String> = Vec::new();
    for r in rows {
        let url = r["url"].as_str().unwrap();
        let loopback = r["loopback"].as_bool().unwrap();
        for (mode, key) in [(Mode::Check, "check"), (Mode::Fetch, "fetch")] {
            let js = &r[key];
            let js_ok = js["ok"].as_bool().unwrap();
            let got = check_url(url, mode, loopback);
            // The JSON face says the same thing.
            let req = serde_json::json!({"op":"url","url":url,"mode":key,"loopback":loopback})
                .to_string();
            let (status, reply) = run_json(&req);
            assert_eq!(status, 0, "{url:?}");
            let reply: Value = serde_json::from_str(&reply).unwrap();
            match &got {
                Ok(a) => {
                    assert!(js_ok, "{key}: Rust allows {url:?} which the JS refuses");
                    assert_eq!(js["kind"], a.kind.code(), "{key} {url:?}");
                    assert_eq!(js["host"], a.host.as_str(), "{key} {url:?}");
                    assert_eq!(reply["host"], a.host.as_str());
                    *seen.entry(format!("{key}:{}", a.kind.code())).or_default() += 1;
                }
                Err(e) => {
                    assert_eq!(reply, serde_json::json!({"ok":false,"reason":e.code()}));
                    if js_ok {
                        let allowed_extra = e.is_extra_strict()
                            || (mode == Mode::Fetch && *e == Refusal::BlockedName);
                        assert!(
                            allowed_extra,
                            "{key}: Rust refuses {url:?} ({}) which the JS accepts",
                            e.code()
                        );
                        *extra.entry(format!("{key}:{}", e.code())).or_default() += 1;
                    } else if mode == Mode::Fetch {
                        // ada rejects some non-special-scheme URLs (`data://\\:p@…`) that the url
                        // crate parses; both refuse them, Rust on the scheme.
                        let both_not_http = js["reason"] == "unparseable" && *e == Refusal::Scheme;
                        if js["reason"] != e.code() && !both_not_http {
                            mismatches.push(format!(
                                "{url:?} js={} rust={}",
                                js["reason"],
                                e.code()
                            ));
                        }
                    }
                    *seen.entry(format!("{key}:{}", e.code())).or_default() += 1;
                }
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "fetch reasons differ: {mismatches:#?}"
    );
    eprintln!("verdicts {seen:?}\nextra strictness {extra:?}");
    for k in [
        "check:ip",
        "check:name",
        "fetch:ip",
        "fetch:name",
        "check:private_address",
        "check:scheme",
        "check:unparseable",
        "check:blocked_name",
        "fetch:credentials",
        "fetch:private_address",
    ] {
        assert!(
            seen.get(k).copied().unwrap_or(0) >= 1,
            "{k} never exercised"
        );
    }
}
