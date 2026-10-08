//! Differential fixtures: every case in `fixtures/s3-sign.v1.json` was evaluated by noevia-core's
//! JS references (server/s3-sign.cjs signS3Parts, server/s3-region.cjs normalizeS3Region) on Node.
//! The file is byte-identical to noevia-core's `tests/fixtures/s3-sign.v1.json` (noevia-core CI
//! compares the two; `tools/gen-s3-sign-fixtures.cjs` regenerates it). All credentials are
//! synthetic. Each case is checked through `sign` and again through the `sign_call` ABI.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use s3_sign::{normalize_region, region_call, sign, sign_call, Error, Request};
use serde_json::Value;

fn fixtures() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/s3-sign.v1.json"
    ))
    .expect("fixture file");
    let f: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(f["version"], 1);
    f
}

fn s(v: &Value) -> &str {
    v.as_str().expect("string")
}

fn unhex(h: &str) -> Vec<u8> {
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
        .collect()
}

fn field(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

/// The input the core loader builds (server/dav-parse-wasm.cjs s3Sign).
pub fn abi_input(r: &Request<'_>) -> Vec<u8> {
    let mut v = Vec::new();
    for f in [r.method, r.host, r.pathname, r.access_key] {
        field(&mut v, f.as_bytes());
    }
    field(&mut v, r.secret_key);
    for f in [r.region, r.session_token, r.amz_date] {
        field(&mut v, f.as_bytes());
    }
    v.extend_from_slice(&(r.query.len() as u32).to_le_bytes());
    for (k, val) in r.query {
        field(&mut v, k.as_bytes());
        field(&mut v, val.as_bytes());
    }
    v.extend_from_slice(r.payload);
    v
}

#[test]
fn signing_matches_the_js_reference() {
    let f = fixtures();
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 150, "{}", cases.len());
    let mut refused = 0;
    for c in cases {
        let name = s(&c["name"]);
        let query: Vec<(&str, &str)> = c["query"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (s(&p[0]), s(&p[1])))
            .collect();
        let payload = unhex(s(&c["payloadHex"]));
        let secret = unhex(s(&c["secretHex"]));
        let req = Request {
            method: s(&c["method"]),
            host: s(&c["host"]),
            pathname: s(&c["pathname"]),
            query: &query,
            payload: &payload,
            access_key: s(&c["accessKey"]),
            secret_key: &secret,
            region: s(&c["region"]),
            session_token: s(&c["sessionToken"]),
            amz_date: s(&c["amzDate"]),
        };
        let abi = sign_call(&abi_input(&req));
        if let Some(code) = c.get("refused") {
            assert_eq!(s(code), "input", "{name}");
            assert_eq!(sign(&req), Err(Error::Input), "{name}");
            assert_eq!(abi, Err(Error::Input), "{name}");
            refused += 1;
            continue;
        }
        let e = &c["expect"];
        let got = sign(&req).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(got.canonical_request, s(&e["canonicalRequest"]), "{name}");
        assert_eq!(got.string_to_sign, s(&e["stringToSign"]), "{name}");
        assert_eq!(got.signature, s(&e["signature"]), "{name}");
        let want: Vec<(String, String)> = e["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (s(&p[0]).to_owned(), s(&p[1]).to_owned()))
            .collect();
        let have: Vec<(String, String)> = got
            .headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        assert_eq!(have, want, "{name}");
        // The ABI reply is the same object, keys in the same order.
        let text = String::from_utf8(abi.unwrap()).unwrap();
        let reply: Value = serde_json::from_str(&text).unwrap();
        let obj = reply.as_object().unwrap();
        assert_eq!(obj.len(), want.len(), "{name}");
        let mut at = 0;
        for (k, v) in &want {
            assert_eq!(obj[k.as_str()], Value::String(v.clone()), "{name}: {k}");
            let key = format!("\"{k}\":");
            let pos = text[at..].find(&key).expect("key order") + at;
            at = pos + key.len();
        }
    }
    assert_eq!(refused, 1);
}

#[test]
fn regions_match_the_js_reference() {
    let f = fixtures();
    let regions = f["regions"].as_array().unwrap();
    assert!(regions.len() >= 20);
    for r in regions {
        let input = s(&r["input"]);
        assert_eq!(normalize_region(input), s(&r["expect"]), "{input:?}");
        assert_eq!(
            region_call(input.as_bytes()).unwrap(),
            s(&r["expect"]).as_bytes(),
            "{input:?}"
        );
    }
}
