//! Differential fixtures (noevia#979): every case in `fixtures/secret-envelope.v1.json` was
//! produced and evaluated by noevia-core's current JS implementation (`server/secret-envelope.cjs`
//! openJs/encryptJs). The port must open every value JS opens, to the same text and with the same
//! key, refuse every value JS refuses with the same class, and seal byte-identically for the same
//! nonce. The file is byte-identical to noevia-core's `tests/fixtures/secret-envelope.v1.json`
//! (noevia-core CI compares the two). Synthetic keys only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use secret_envelope::{open, seal, Error, KeyUsed};
use serde_json::Value;

fn fixtures() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/secret-envelope.v1.json"
    ))
    .expect("fixture file");
    let f: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(f["version"], 1);
    f
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

fn units(hex: &str) -> Vec<u16> {
    unhex(hex)
        .chunks(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// `String(userId)` crosses as UTF-8 (lone surrogates as U+FFFD, as TextEncoder does).
fn user(c: &Value) -> Option<Vec<u8>> {
    c["user16"]
        .as_str()
        .map(|h| String::from_utf16_lossy(&units(h)).into_bytes())
}

fn key(f: &Value, name: &str) -> Vec<u8> {
    unhex(f["keys"][name].as_str().expect("key"))
}

#[test]
fn open_agrees_with_js() {
    let f = fixtures();
    let cases = f["open"].as_array().expect("open");
    assert!(cases.len() >= 1000, "open table shrank to {}", cases.len());
    let mut bad = Vec::new();
    for c in cases {
        let names: Vec<&str> = c["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();
        let current = key(&f, names[0]);
        let previous = names.get(1).map(|n| key(&f, n));
        let value = units(c["value16"].as_str().unwrap());
        let u = user(c);
        let got = match open(&current, previous.as_deref(), &value, u.as_deref()) {
            Ok((KeyUsed::None, _)) => format!("none {}", c["value16"].as_str().unwrap()),
            Ok((used, plain)) => {
                let text: Vec<u8> = String::from_utf8_lossy(&plain)
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect();
                let hex: String = text.iter().map(|b| format!("{b:02x}")).collect();
                let used = if used == KeyUsed::Current {
                    "current"
                } else {
                    "previous"
                };
                format!("{used} {hex}")
            }
            Err(Error::Bound) => "bound".into(),
            Err(Error::Unopenable) => "unopenable".into(),
            Err(e) => format!("other {e}"),
        };
        let e = &c["expect"];
        let want = match e["error"].as_str() {
            Some(err) => err.to_owned(),
            None => format!(
                "{} {}",
                e["keyUsed"].as_str().unwrap(),
                e["plain16"].as_str().unwrap()
            ),
        };
        if got != want {
            bad.push(format!("{}: expected {want:.80} got {got:.80}", c["name"]));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} open fixtures disagree:\n{}",
        bad.len(),
        cases.len(),
        bad.join("\n")
    );
}

#[test]
fn seal_is_byte_identical_for_the_same_nonce() {
    let f = fixtures();
    let cases = f["seal"].as_array().expect("seal");
    assert!(cases.len() >= 250, "seal table shrank to {}", cases.len());
    let mut bad = Vec::new();
    for c in cases {
        let got = seal(
            &key(&f, c["key"].as_str().unwrap()),
            &unhex(c["nonce8"].as_str().unwrap()),
            &unhex(c["plain8"].as_str().unwrap()),
            user(c).as_deref(),
        )
        .unwrap();
        if got != c["expect"].as_str().unwrap() {
            bad.push(c["name"].to_string());
        }
    }
    assert!(bad.is_empty(), "seal fixtures disagree: {bad:?}");
}
