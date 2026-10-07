//! Cross-language grant vectors. The same file is committed in noevia as
//! `apps/web/contracts/egress-grant.v1.vectors.json`, where code-egress.cjs `mintGrant` must
//! produce every `valid` token byte for byte. Here every valid token must mint identically and
//! verify back to its fields, and every invalid one must fail with its named error.
//!
//! Regenerate (only when the contract changes, then copy the file to noevia):
//!   GRANT_VECTORS_WRITE=1 cargo test -p egress --test grant_vectors
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use egress::{
    b64url_decode, b64url_encode, canonical_payload, mint, verify, Act, GrantKey, SignedGrant,
};
use serde_json::{json, Value};

const PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/egress-grant.v1.vectors.json"
);
const KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const NOW: u64 = 1_800_000_000_000;

fn grant_json(g: &SignedGrant) -> Value {
    json!({
        "act": match g.act { Act::Grant => "grant", Act::Revoke => "revoke" },
        "task": g.task, "hosts": g.hosts, "iat": g.iat, "exp": g.exp, "idle": g.idle,
        "nonce": g.nonce,
    })
}

fn grant_from(v: &Value) -> SignedGrant {
    SignedGrant {
        act: if v["act"] == "revoke" {
            Act::Revoke
        } else {
            Act::Grant
        },
        task: v["task"].as_str().unwrap().into(),
        hosts: v["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h.as_str().unwrap().to_owned())
            .collect(),
        iat: v["iat"].as_u64().unwrap(),
        exp: v["exp"].as_u64().unwrap(),
        idle: v["idle"].as_u64().unwrap(),
        nonce: v["nonce"].as_str().unwrap().into(),
    }
}

fn sign_raw(payload: &str) -> String {
    // mint() refuses non-canonical payloads, so these are signed by hand with the vector key;
    // the valid set proves this HMAC is the one mint() uses.
    use hmac::{Hmac, Mac};
    let signed = format!("ngr1.{}", b64url_encode(payload.as_bytes()));
    let k = hex(KEY_HEX);
    let mut m = <Hmac<sha2::Sha256> as Mac>::new_from_slice(&k).unwrap();
    m.update(signed.as_bytes());
    format!("{signed}.{}", b64url_encode(&m.finalize().into_bytes()))
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

fn build() -> Value {
    let key = GrantKey::from_hex(KEY_HEX).unwrap();
    let base = SignedGrant {
        act: Act::Grant,
        task: "3f2b8c1e-0d4a-4c7e-9a51-6b2f0e9d8c7a".into(),
        hosts: vec!["registry.npmjs.example".into()],
        iat: NOW,
        exp: NOW + 12 * 3_600_000,
        idle: 6 * 3_600_000,
        nonce: "AAECAwQFBgcICQoLDA0ODw".into(),
    };
    let mut valid = Vec::new();
    let mut add = |name: &str, g: SignedGrant| {
        let token = mint(&key, &g).unwrap();
        valid.push(json!({ "name": name, "grant": grant_json(&g),
            "payload": canonical_payload(&g), "token": token, "now": g.iat + 1 }));
    };
    add("single host", base.clone());
    add(
        "several hosts, order kept",
        SignedGrant {
            hosts: vec![
                "pypi.example".into(),
                "files.pythonhosted.example".into(),
                "github.example".into(),
            ],
            nonce: "_-_-_-_-_-_-_-_-_-_-_w".into(),
            ..base.clone()
        },
    );
    add(
        "short-lived, idle equals lifetime",
        SignedGrant {
            task: "task:browser.1_x".into(),
            exp: NOW + 1000,
            idle: 1000,
            ..base.clone()
        },
    );
    add(
        "revoke",
        SignedGrant {
            act: Act::Revoke,
            hosts: vec![],
            idle: 1,
            nonce: "zzzzzzzzzzzzzzzzzzzzzw".into(),
            ..base.clone()
        },
    );

    let good = mint(&key, &base).unwrap();
    let (signed, tag) = good.rsplit_once('.').unwrap();
    let canon = canonical_payload(&base);
    let other = mint(
        &key,
        &SignedGrant {
            hosts: vec!["evil.example".into()],
            ..base.clone()
        },
    )
    .unwrap();
    let invalid = vec![
        json!({ "name": "forged: other key", "now": NOW,
            "token": mint(&GrantKey::from_bytes([0xff; 32]), &base).unwrap(), "error": "BadSignature" }),
        json!({ "name": "tampered: hosts swapped under the original tag", "now": NOW,
            "token": format!("{}.{tag}", other.rsplit_once('.').unwrap().0), "error": "BadSignature" }),
        json!({ "name": "tampered: tag truncated", "now": NOW,
            "token": format!("{signed}.{}", &tag[..40]), "error": "BadSignature" }),
        json!({ "name": "wrong version prefix", "now": NOW,
            "token": good.replacen("ngr1.", "ngr2.", 1), "error": "WrongVersion" }),
        json!({ "name": "wrong version field, validly signed", "now": NOW,
            "token": sign_raw(&canon.replacen("\"v\":1", "\"v\":2", 1)), "error": "WrongVersion" }),
        json!({ "name": "non-canonical: whitespace, validly signed", "now": NOW,
            "token": sign_raw(&canon.replacen(",", ", ", 1)), "error": "Malformed" }),
        json!({ "name": "non-canonical: upper-case host, validly signed", "now": NOW,
            "token": sign_raw(&canon.replace("registry", "Registry")), "error": "Malformed" }),
        json!({ "name": "expired", "now": base.exp, "token": good, "error": "Expired" }),
        json!({ "name": "not yet valid", "now": NOW - 60_001, "token": good, "error": "NotYetValid" }),
        json!({ "name": "not a grant", "now": NOW, "token": "tok-plain-0123456789", "error": "NotAGrant" }),
    ];
    json!({
        "contract": "egress-grant",
        "version": 1,
        "keyHex": KEY_HEX,
        "valid": valid,
        "invalid": invalid,
    })
}

#[test]
fn vectors_match_byte_for_byte() {
    let built = build();
    let text = format!("{}\n", serde_json::to_string_pretty(&built).unwrap());
    if std::env::var_os("GRANT_VECTORS_WRITE").is_some() {
        std::fs::write(PATH, &text).unwrap();
    }
    let committed: Value = serde_json::from_str(&std::fs::read_to_string(PATH).unwrap()).unwrap();
    assert_eq!(committed, built, "vectors drifted; regenerate deliberately");

    let key = GrantKey::from_hex(committed["keyHex"].as_str().unwrap()).unwrap();
    let mut checked = 0;
    for v in committed["valid"].as_array().unwrap() {
        let g = grant_from(&v["grant"]);
        let token = v["token"].as_str().unwrap();
        assert_eq!(canonical_payload(&g), v["payload"].as_str().unwrap());
        assert_eq!(mint(&key, &g).unwrap(), token, "{}", v["name"]);
        let body = token.split('.').nth(1).unwrap();
        assert_eq!(
            b64url_decode(body.as_bytes()).unwrap(),
            v["payload"].as_str().unwrap().as_bytes()
        );
        assert_eq!(
            verify(&key, token.as_bytes(), v["now"].as_u64().unwrap()).unwrap(),
            g
        );
        checked += 1;
    }
    for v in committed["invalid"].as_array().unwrap() {
        let err = verify(
            &key,
            v["token"].as_str().unwrap().as_bytes(),
            v["now"].as_u64().unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            format!("{err:?}"),
            v["error"].as_str().unwrap(),
            "{}",
            v["name"]
        );
        checked += 1;
    }
    assert_eq!(checked, 14);
}
