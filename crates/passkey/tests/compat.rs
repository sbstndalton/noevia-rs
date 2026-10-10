//! Passkey compatibility with noevia-core's @simplewebauthn/server v14 (full-Rust migration M3).
//! Every case in `fixtures/simplewebauthn-v14.json` was verified by @simplewebauthn itself
//! (tools/gen-passkey-fixtures.cjs; CI regenerates it against core's node_modules and runs this
//! test on the fresh output). Rust must reach Node's verdict on every registration and assertion,
//! store exactly what auth.cjs stores for a registration, return Node's new counter, and give
//! Node's message wherever it says the message is exact. A Rust accept of what Node refuses is
//! reported separately.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;
use passkey::{b64, verify_authentication, verify_registration, Expected, Stored};
use serde_json::Value;

fn fixture() -> Value {
    let path = std::env::var("NOEVIA_PASSKEY_FIXTURE").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/simplewebauthn-v14.json"
        )
        .to_string()
    });
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("{path}: generate it with tools/gen-passkey-fixtures.cjs"));
    let f: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(f["version"], 1);
    assert!(
        f["simplewebauthn"].as_str().unwrap().starts_with("14."),
        "fixture from simplewebauthn {}",
        f["simplewebauthn"]
    );
    f
}

fn jv(v: &Value) -> JValue {
    js_json::parse(&v.to_string()).unwrap()
}

fn expected(v: &Value) -> (String, Vec<String>, String) {
    (
        v["challenge"].as_str().unwrap().to_string(),
        v["origins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o.as_str().unwrap().to_string())
            .collect(),
        v["rpId"].as_str().unwrap().to_string(),
    )
}

/// Registrations Rust refuses on purpose although @simplewebauthn v14 may accept them: packed
/// self attestation whose attStmt.alg is not the credential key's alg (Node lets the statement's
/// alg choose the hash; WebAuthn 8.2 says it must be the key's own). The fixture records Node's
/// verdict either way; where Node also refuses, the normal comparison applies.
const STRICTER_THAN_NODE: &str = "packed self attestation alg mismatch";

#[test]
fn registrations_agree_with_simplewebauthn() {
    let f = fixture();
    let (mut ok, mut exact, mut stricter, mut bad, mut unsafe_accepts) =
        (0, 0, 0, Vec::new(), Vec::new());
    let cases = f["registrations"].as_array().unwrap();
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let response = if c["responseUndefined"] == true {
            JValue::Undefined
        } else {
            jv(&c["response"])
        };
        let (challenge, origins, rp_id) = expected(&c["expected"]);
        let got = verify_registration(
            &response,
            Expected {
                challenge: &challenge,
                origins: &origins,
                rp_id: &rp_id,
            },
        );
        let want = &c["result"];
        match (&got, want["ok"].as_bool().unwrap()) {
            (Ok(cred), true) => {
                ok += 1;
                let s = &want["stored"];
                let transports = match &cred.transports {
                    t if t.truthy() => js_json::stringify(t).unwrap(),
                    _ => "[]".to_string(),
                };
                let mine = serde_json::json!({
                    "id": cred.id, "publicKey": b64::from_buffer(&cred.public_key), "counter": cred.counter,
                    "transports": transports, "deviceType": cred.device_type,
                    "backedUp": i32::from(cred.backed_up), "fmt": cred.fmt,
                });
                if &mine != s {
                    bad.push(format!("{name}: stored Rust {mine} Node {s}"));
                }
            }
            (Err(e), false) => {
                if e.exact {
                    exact += 1;
                    if e.message != want["message"].as_str().unwrap() {
                        bad.push(format!(
                            "{name}: message Rust {:?} Node {:?}",
                            e.message, want["message"]
                        ));
                    }
                }
            }
            (Ok(_), false) => {
                unsafe_accepts.push(format!("{name}: Node refused: {}", want["message"]))
            }
            (Err(e), true) if name.starts_with(STRICTER_THAN_NODE) => {
                // Deliberately stricter: the refusal must name the mismatch, not be a lookalike.
                if !e.message.contains("alg does not match") {
                    bad.push(format!("{name}: refused for another reason: {e}"));
                }
                stricter += 1;
            }
            (Err(e), true) => bad.push(format!("{name}: Rust refused ({e}), Node accepted")),
        }
    }
    let mismatches = cases
        .iter()
        .filter(|c| c["name"].as_str().unwrap().starts_with(STRICTER_THAN_NODE))
        .count();
    assert!(mismatches >= 3, "only {mismatches} alg-mismatch cases");
    // Rust refuses every mismatch: where Node accepted it counts as stricter, where Node refused too
    // it was compared above (and could not have been accepted: that is an unsafe accept).
    assert!(
        stricter <= mismatches,
        "{stricter} stricter refusals of {mismatches} cases"
    );
    assert!(cases.len() >= 40, "only {} registrations", cases.len());
    assert!(ok >= 18, "only {ok} accepted registrations");
    assert!(exact >= 15, "only {exact} exact refusals");
    assert!(
        unsafe_accepts.is_empty(),
        "Rust accepts what Node refuses:\n{}",
        unsafe_accepts.join("\n")
    );
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn authentications_agree_with_simplewebauthn() {
    let f = fixture();
    let (mut ok, mut bad, mut unsafe_accepts) = (0, Vec::new(), Vec::new());
    let cases = f["authentications"].as_array().unwrap();
    let kinds = ["es256", "eddsa", "rs256", "rs256-3072", "p384-alg-7"];
    let mut signed_in = std::collections::BTreeSet::new();
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let (challenge, origins, rp_id) = expected(&c["expected"]);
        let stored = &c["stored"];
        let key = b64::to_buffer(stored["publicKey"].as_str().unwrap());
        let got = verify_authentication(
            &jv(&c["response"]),
            Expected {
                challenge: &challenge,
                origins: &origins,
                rp_id: &rp_id,
            },
            Stored {
                public_key: &key,
                counter: stored["counter"].as_f64().unwrap(),
            },
        );
        let want = &c["result"];
        match (&got, want["ok"].as_bool().unwrap()) {
            (Ok(n), true) => {
                ok += 1;
                if let Some(k) = kinds.iter().find(|k| name.starts_with(&format!("{k} #"))) {
                    signed_in.insert(*k);
                }
                if u64::from(*n) != want["newCounter"].as_u64().unwrap() {
                    bad.push(format!(
                        "{name}: counter Rust {n} Node {}",
                        want["newCounter"]
                    ));
                }
            }
            (Err(e), false) => {
                if e.exact && e.message != want["message"].as_str().unwrap() {
                    bad.push(format!(
                        "{name}: message Rust {:?} Node {:?}",
                        e.message, want["message"]
                    ));
                }
            }
            (Ok(_), false) => {
                unsafe_accepts.push(format!("{name}: Node refused: {}", want["message"]))
            }
            (Err(e), true) => bad.push(format!("{name}: Rust refused ({e}), Node accepted")),
        }
    }
    assert!(cases.len() >= 300, "only {} assertions", cases.len());
    assert!(ok >= 60, "only {ok} accepted assertions");
    assert_eq!(
        signed_in.len(),
        kinds.len(),
        "kinds that signed in: {signed_in:?}"
    );
    assert!(
        unsafe_accepts.is_empty(),
        "Rust accepts what Node refuses:\n{}",
        unsafe_accepts.join("\n")
    );
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
