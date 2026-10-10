//! Local ceremonies with software keys (no Node needed): a registration verifies, the stored key
//! signs in, and each tampering is refused. The cross-check against @simplewebauthn itself is
//! tests/compat.rs. Synthetic keys only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use js_json::JValue;
use passkey::cbor::{encode, Cbor};
use passkey::{b64, verify_authentication, verify_registration, Expected, Stored};
use sha2::{Digest, Sha256};

const RP: &str = "noevia.example.test";
const ORIGIN: &str = "https://noevia.example.test";

fn auth_data(flags: u8, counter: u32, cred: Option<(&[u8], &[u8])>) -> Vec<u8> {
    let mut out = Sha256::digest(RP.as_bytes()).to_vec();
    out.push(flags);
    out.extend(counter.to_be_bytes());
    if let Some((id, key)) = cred {
        out.extend([0u8; 16]);
        out.extend((id.len() as u16).to_be_bytes());
        out.extend_from_slice(id);
        out.extend_from_slice(key);
    }
    out
}

fn client(kind: &str, challenge: &str) -> String {
    b64::from_buffer(
        format!(r#"{{"type":"{kind}","challenge":"{challenge}","origin":"{ORIGIN}","crossOrigin":false}}"#)
            .as_bytes(),
    )
}

fn p256_key() -> (p256::ecdsa::SigningKey, Vec<u8>) {
    let sk = p256::ecdsa::SigningKey::random(&mut rand_core::OsRng);
    let point = sk.verifying_key().to_encoded_point(false);
    let cose = Cbor::Map(vec![
        (Cbor::Num(1.0), Cbor::Num(2.0)),
        (Cbor::Num(3.0), Cbor::Num(-7.0)),
        (Cbor::Num(-1.0), Cbor::Num(1.0)),
        (Cbor::Num(-2.0), Cbor::Bytes(point.x().unwrap().to_vec())),
        (Cbor::Num(-3.0), Cbor::Bytes(point.y().unwrap().to_vec())),
    ]);
    (sk, encode(&cose))
}

fn registration(key: &[u8], challenge: &str, flags: u8) -> JValue {
    let id = [7u8; 16];
    let ad = auth_data(flags, 0, Some((&id, key)));
    let ao = encode(&Cbor::Map(vec![
        (Cbor::Text("fmt".into()), Cbor::Text("none".into())),
        (Cbor::Text("attStmt".into()), Cbor::Map(vec![])),
        (Cbor::Text("authData".into()), Cbor::Bytes(ad)),
    ]));
    let idb = b64::from_buffer(&id);
    JValue::obj([
        ("id", JValue::from(idb.as_str())),
        ("rawId", JValue::from(idb.as_str())),
        ("type", JValue::from("public-key")),
        (
            "response",
            JValue::obj([
                ("clientDataJSON", JValue::from(client("webauthn.create", challenge))),
                ("attestationObject", JValue::from(b64::from_buffer(&ao))),
                ("transports", JValue::Arr(vec![JValue::from("internal")])),
            ]),
        ),
    ])
}

fn assertion(sign: impl Fn(&[u8]) -> Vec<u8>, challenge: &str, flags: u8, counter: u32) -> JValue {
    let ad = auth_data(flags, counter, None);
    let cdj = client("webauthn.get", challenge);
    let mut base = ad.clone();
    base.extend(Sha256::digest(b64::to_buffer(&cdj)));
    let sig = sign(&base);
    JValue::obj([
        ("id", JValue::from("BwcHBwcHBwcHBwcHBwcHBw")),
        ("rawId", JValue::from("BwcHBwcHBwcHBwcHBwcHBw")),
        ("type", JValue::from("public-key")),
        (
            "response",
            JValue::obj([
                ("clientDataJSON", JValue::from(cdj)),
                ("authenticatorData", JValue::from(b64::from_buffer(&ad))),
                ("signature", JValue::from(b64::from_buffer(&sig))),
            ]),
        ),
    ])
}

#[test]
fn es256_round_trip() {
    let origins = vec![ORIGIN.to_string()];
    let ex = |c: &'static str| Expected {
        challenge: c,
        origins: &origins,
        rp_id: RP,
    };
    let (sk, key) = p256_key();
    let cred = verify_registration(&registration(&key, "reg", 0x45), ex("reg")).unwrap();
    assert_eq!(cred.public_key, key);
    assert_eq!(cred.id, "BwcHBwcHBwcHBwcHBwcHBw");
    assert_eq!((cred.device_type, cred.backed_up), ("singleDevice", false));
    assert!(verify_registration(&registration(&key, "reg", 0x41), ex("reg")).is_err());
    assert!(verify_registration(&registration(&key, "reg", 0x45), ex("other")).is_err());

    let sign = |d: &[u8]| -> Vec<u8> {
        use p256::ecdsa::signature::Signer;
        let s: p256::ecdsa::Signature = sk.sign(d);
        s.to_der().as_bytes().to_vec()
    };
    let stored = Stored {
        public_key: &cred.public_key,
        counter: 0.0,
    };
    assert_eq!(
        verify_authentication(&assertion(sign, "a", 0x05, 3), ex("a"), stored).unwrap(),
        3
    );
    assert!(verify_authentication(&assertion(sign, "a", 0x01, 3), ex("a"), stored).is_err());
    assert!(verify_authentication(&assertion(sign, "a", 0x05, 3), ex("b"), stored).is_err());
    let used = Stored {
        public_key: &cred.public_key,
        counter: 3.0,
    };
    assert!(verify_authentication(&assertion(sign, "a", 0x05, 3), ex("a"), used).is_err());
    let forged = |d: &[u8]| {
        let mut s = sign(d);
        let n = s.len() - 2;
        s[n] ^= 1;
        s
    };
    assert!(verify_authentication(&assertion(forged, "a", 0x05, 9), ex("a"), stored).is_err());
}

#[test]
fn eddsa_round_trip() {
    use ed25519_dalek::Signer;
    let origins = vec![ORIGIN.to_string()];
    let sk = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
    let key = encode(&Cbor::Map(vec![
        (Cbor::Num(1.0), Cbor::Num(1.0)),
        (Cbor::Num(3.0), Cbor::Num(-8.0)),
        (Cbor::Num(-1.0), Cbor::Num(6.0)),
        (
            Cbor::Num(-2.0),
            Cbor::Bytes(sk.verifying_key().to_bytes().to_vec()),
        ),
    ]));
    let ex = Expected {
        challenge: "c",
        origins: &origins,
        rp_id: RP,
    };
    let cred = verify_registration(&registration(&key, "c", 0x5d), ex).unwrap();
    assert_eq!((cred.device_type, cred.backed_up), ("multiDevice", true));
    let sign = |d: &[u8]| sk.sign(d).to_bytes().to_vec();
    let stored = Stored {
        public_key: &cred.public_key,
        counter: 0.0,
    };
    assert_eq!(
        verify_authentication(&assertion(sign, "c", 0x1d, 0), ex, stored).unwrap(),
        0
    );
    // Backed up without backup eligibility is refused.
    assert!(verify_authentication(&assertion(sign, "c", 0x15, 0), ex, stored).is_err());
}
