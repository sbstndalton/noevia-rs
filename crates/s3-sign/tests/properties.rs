//! Properties: arbitrary ABI bytes never panic and refuse only with the fixed public errors;
//! the secret key never reaches a reply, an error or `Debug`; errors never echo input; signing is
//! deterministic and keyed by the secret. All values are synthetic.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use proptest::prelude::*;
use s3_sign::{
    canonical_uri, cmp_utf16, normalize_region, region_call, sign, sign_call, uri_encode, Error,
    Request, MAX_FIELD_BYTES,
};

const ERRORS: [&[u8]; 2] = [b"{\"error\":\"input\"}", b"{\"error\":\"too_large\"}"];

fn field(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn abi(r: &Request<'_>) -> Vec<u8> {
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

/// Public text: no `#`, so a secret built around `#` cannot appear by coincidence.
fn public() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 ./%~é😀+=_-]{0,24}"
}

fn secret() -> impl Strategy<Value = String> {
    "[a-z0-9]{4,24}".prop_map(|s| format!("#k3y#{s}#"))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_abi_input_never_panics(input in proptest::collection::vec(any::<u8>(), 0..512)) {
        match sign_call(&input) {
            Ok(reply) => prop_assert!(serde_json::from_slice::<serde_json::Value>(&reply).is_ok()),
            Err(e) => prop_assert!(ERRORS.contains(&e.json().as_slice())),
        }
        let _ = region_call(&input);
    }

    #[test]
    fn the_secret_never_leaves(
        method in public(), host in public(), path in public(), ak in public(), token in public(),
        region in "[a-z0-9-]{0,12}", sk in secret(), k in public(), v in public(),
        payload in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let query = [(k.as_str(), v.as_str())];
        let req = Request {
            method: &method, host: &host, pathname: &path, query: &query, payload: &payload,
            access_key: &ak, secret_key: sk.as_bytes(), region: &region, session_token: &token,
            amz_date: "20260101T000000Z",
        };
        let signed = sign(&req).unwrap();
        let dump = format!("{signed:?}{req:?}");
        prop_assert!(!dump.contains("#k3y#"));
        let reply = sign_call(&abi(&req)).unwrap();
        prop_assert!(!String::from_utf8(reply).unwrap().contains("#k3y#"));
        // Deterministic, and a different secret gives a different signature.
        prop_assert_eq!(&sign(&req).unwrap(), &signed);
        let other = format!("{sk}x");
        let req2 = Request { secret_key: other.as_bytes(), ..req };
        prop_assert_ne!(sign(&req2).unwrap().signature, signed.signature);
    }

    #[test]
    fn refusals_never_echo_input(sk in secret(), astral in "[\\u{10000}-\\u{10FFFF}]{1,3}", head in "[0-9]{7}") {
        let date = format!("{head}{astral}");
        // An astral date stamp is cut inside a surrogate pair: refused, with only the code.
        let req = Request {
            method: "GET", host: "h", pathname: "/", query: &[], payload: b"", access_key: "a",
            secret_key: sk.as_bytes(), region: "", session_token: "", amz_date: &date,
        };
        let e = sign_call(&abi(&req)).unwrap_err();
        prop_assert_eq!(e, Error::Input);
        prop_assert_eq!(e.json(), b"{\"error\":\"input\"}".to_vec());
        prop_assert_eq!(format!("{e}{e:?}").contains("#k3y#"), false);
    }

    #[test]
    fn encoding_is_unreserved_only(s in any::<String>()) {
        let e = uri_encode(&s);
        prop_assert!(e.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.~%".contains(&b)));
        let c = canonical_uri(&format!("/{s}"));
        prop_assert!(c.starts_with('/'));
        prop_assert!(c.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.~%/".contains(&b)));
    }

    #[test]
    fn utf16_order_is_a_total_order(a in any::<String>(), b in any::<String>()) {
        prop_assert_eq!(cmp_utf16(&a, &b), cmp_utf16(&b, &a).reverse());
    }

    #[test]
    fn region_is_valid_or_default(s in any::<String>()) {
        let r = normalize_region(&s);
        prop_assert!((1..=32).contains(&r.len()));
        prop_assert!(r.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
    }
}

#[test]
fn caps_refuse_with_too_large() {
    let big = "a".repeat(MAX_FIELD_BYTES + 1);
    let req = Request {
        method: "GET",
        host: &big,
        pathname: "/",
        query: &[],
        payload: b"",
        access_key: "a",
        secret_key: b"s",
        region: "",
        session_token: "",
        amz_date: "20260101T000000Z",
    };
    assert_eq!(sign(&req), Err(Error::TooLarge));
    assert_eq!(sign_call(&abi(&req)), Err(Error::TooLarge));
    let empty_date = Request {
        host: "h",
        amz_date: "",
        ..req
    };
    assert_eq!(sign(&empty_date), Err(Error::Input));
    // A length prefix beyond the buffer, or invalid UTF-8 in a text field.
    assert_eq!(sign_call(&[3, 0, 0, 0, b'G']), Err(Error::Input));
    assert_eq!(sign_call(&[1, 0, 0, 0, 0xff]), Err(Error::Input));
    assert_eq!(region_call(&[0xff]), Err(Error::Input));
}
