//! Properties of the tenant assertion check. The oracle below restates Python's acceptance rule
//! (diary/agent/tenant_assertion.py `verify`) directly: Python accepts exactly when the tenant is
//! non-empty, the header equals its own re-signature `v2.<int(ts)>.<nonce>.<sig>` and
//! |now - ts| <= 60 (the nonce cache only refuses more). The crate must never accept outside it.

use hmac::{Hmac, Mac};
use proptest::prelude::*;
use sha2::{Digest, Sha256};
use tenant_assertion::{sign, verify, Reject, Request, MAX_FIELD_BYTES};

fn hexs(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Python's re-signature, written independently of the crate.
fn python_expected(r: &Request<'_>, ts: u64, nonce: &str) -> String {
    let msg = [
        "cowork-diary-tenant-v2".to_string(),
        r.user_id.to_lowercase(),
        ts.to_string(),
        nonce.to_string(),
        r.method.to_uppercase(),
        r.path.to_string(),
        format!("query={}", hexs(&Sha256::digest(r.query))),
        format!("body={}", r.body_hash),
        hexs(&Sha256::digest(r.storage.as_bytes())),
        r.legacy_owner.to_string(),
        r.blocked.to_string(),
    ]
    .join("\n");
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(r.key.as_bytes()) else {
        return String::new();
    };
    mac.update(msg.as_bytes());
    let tag = mac.finalize().into_bytes();
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bits = String::new();
    for b in tag.iter() {
        bits.push_str(&format!("{b:08b}"));
    }
    while !bits.len().is_multiple_of(6) {
        bits.push('0');
    }
    let sig: String = (0..bits.len() / 6)
        .map(|i| {
            let v = u8::from_str_radix(bits.get(i * 6..i * 6 + 6).unwrap_or("0"), 2).unwrap_or(0);
            char::from(*A.get(usize::from(v)).unwrap_or(&b'A'))
        })
        .collect();
    format!("v2.{ts}.{nonce}.{sig}")
}

/// Would Python accept `r` (ignoring the nonce cache, which only refuses more)?
fn python_accepts(r: &Request<'_>) -> bool {
    if r.user_id.is_empty() {
        return false;
    }
    let parts: Vec<&str> = r.assertion.split('.').collect();
    let [v, ts, nonce, _sig] = parts.as_slice() else {
        return false;
    };
    // Python's \d admits Unicode digits but int() + re-signing makes only ASCII ones verifiable.
    if *v != "v2" || ts.is_empty() || ts.len() > 12 || !ts.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Ok(ts) = ts.parse::<u64>() else {
        return false;
    };
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return false;
    }
    python_expected(r, ts, nonce) == r.assertion && (r.now - ts as f64).abs() <= 60.0
}

fn field() -> impl Strategy<Value = String> {
    prop_oneof![Just(String::new()), "[ -~]{0,24}", "\\PC{0,8}"]
}

fn ascii_id() -> impl Strategy<Value = String> {
    prop_oneof!["[0-9a-fA-F-]{1,36}", "[ -~]{1,12}"]
}

fn body_hash() -> impl Strategy<Value = String> {
    prop_oneof![Just("stream".to_string()), "[0-9a-f]{64}"]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn signed_requests_inside_the_window_are_accepted(
        key in "[ -~]{1,80}", user in ascii_id(), method in "[A-Za-z]{1,8}", path in field(),
        query in proptest::collection::vec(any::<u8>(), 0..32), bh in body_hash(), storage in field(),
        legacy in field(), blocked in field(), ts in 0u64..=999_999_999_999, nonce in "[0-9a-f]{32}",
        skew in -60.0f64..=60.0,
    ) {
        let mut r = Request { key: &key, user_id: &user, assertion: "", method: &method, path: &path,
            query: &query, body_hash: &bh, storage: &storage, legacy_owner: &legacy, blocked: &blocked,
            now: ts as f64 + skew };
        let a = sign(&r, ts, &nonce).unwrap_or_default();
        prop_assert_eq!(&a, &python_expected(&r, ts, &nonce));
        r.assertion = &a;
        prop_assert_eq!(verify(&r), Ok(()));
        prop_assert!(python_accepts(&r));
    }

    #[test]
    fn any_single_edit_of_a_valid_assertion_is_refused(
        pos in 0usize..120, ch in "[ -~\\u{e9}\\u{661}]", mode in 0u8..3,
    ) {
        let mut r = Request { key: "synthetic-key", user_id: "11111111-1111-4111-8111-111111111111",
            assertion: "", method: "GET", path: "/p", query: b"", body_hash: "stream", storage: "",
            legacy_owner: "", blocked: "", now: 1000.0 };
        let good = sign(&r, 1000, "0123456789abcdef0123456789abcdef").unwrap_or_default();
        let at = pos % good.len();
        let (head, tail) = good.split_at(at);
        let edited = match mode {
            0 => format!("{head}{ch}{}", tail.get(1..).unwrap_or("")),
            1 => format!("{head}{}", tail.get(1..).unwrap_or("")),
            _ => format!("{head}{ch}{tail}"),
        };
        prop_assume!(edited != good);
        r.assertion = &edited;
        prop_assert!(verify(&r).is_err());
    }

    #[test]
    fn changing_any_signed_field_is_refused(which in 0u8..9, extra in "[ -~]{1,4}") {
        let base = Request { key: "synthetic-key", user_id: "11111111-1111-4111-8111-111111111111",
            assertion: "", method: "POST", path: "/p", query: b"q=1", body_hash: "stream", storage: "s",
            legacy_owner: "l", blocked: "", now: 5000.0 };
        let a = sign(&base, 5000, "0123456789abcdef0123456789abcdef").unwrap_or_default();
        let user = format!("{}{extra}", base.user_id);
        let method = format!("{}{extra}", base.method);
        let path = format!("{}{extra}", base.path);
        let storage = format!("{}{extra}", base.storage);
        let legacy = format!("{}{extra}", base.legacy_owner);
        let blocked = format!("{}{extra}", base.blocked);
        let key = format!("{}{extra}", base.key);
        let query = [base.query, extra.as_bytes()].concat();
        let mut r = Request { assertion: &a, ..base };
        match which {
            0 => r.user_id = &user,
            1 => r.method = &method,
            2 => r.path = &path,
            3 => r.storage = &storage,
            4 => r.legacy_owner = &legacy,
            5 => r.blocked = &blocked,
            6 => r.key = &key,
            7 => r.query = &query,
            _ => r.body_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        }
        let res = verify(&r);
        prop_assert!(res.is_err(), "field {} accepted after change", which);
        prop_assert!(!python_accepts(&r) || res.is_err());
    }

    /// The safety rule: on arbitrary inputs (including near-valid ones) the crate never accepts
    /// a request Python's rule would refuse.
    #[test]
    fn never_accepts_what_python_rejects(
        key in "\\PC{0,20}", user in prop_oneof![Just(String::new()), ascii_id(), "\\PC{1,6}"],
        method in prop_oneof!["[A-Za-z]{0,6}", "\\PC{0,4}"], path in field(),
        bh in prop_oneof![body_hash(), field()], ts in 0u64..=999_999_999_999,
        nonce in prop_oneof!["[0-9a-f]{32}", "[0-9a-fA-F]{30,34}"], skew in -120.0f64..120.0,
        tamper in proptest::option::of("[ -~\\u{661}]{0,3}"), resign in any::<bool>(),
        now_special in prop_oneof![Just(None), Just(Some(f64::NAN)), Just(Some(f64::INFINITY))],
    ) {
        let now = now_special.unwrap_or(ts as f64 + skew);
        let mut r = Request { key: &key, user_id: &user, assertion: "", method: &method, path: &path,
            query: b"", body_hash: &bh, storage: "", legacy_owner: "", blocked: "", now };
        let mut a = if resign { python_expected(&r, ts, &nonce) } else { sign(&r, ts, &nonce).unwrap_or_default() };
        if let Some(t) = tamper { a.push_str(&t); }
        r.assertion = &a;
        if verify(&r).is_ok() {
            prop_assert!(python_accepts(&r), "accepted what Python refuses: {:?}", r.assertion);
        }
    }
}

#[test]
fn stricter_inputs_are_refused_even_when_correctly_signed() {
    let big = "s".repeat(MAX_FIELD_BYTES + 1);
    for (user, method, bh, key, storage, now) in [
        ("caf\u{e9}", "GET", "stream", "k", "", 1.0),
        ("u", "G\u{c9}T", "stream", "k", "", 1.0),
        ("u", "GET", "not-a-hash", "k", "", 1.0),
        ("u", "GET", "stream", "", "", 1.0),
        ("u", "GET", "stream", "k", big.as_str(), 1.0),
        ("u", "GET", "stream", "k", "", f64::NAN),
    ] {
        let mut r = Request {
            key,
            user_id: user,
            assertion: "",
            method,
            path: "/",
            query: b"",
            body_hash: bh,
            storage,
            legacy_owner: "",
            blocked: "",
            now,
        };
        let a = python_expected(&r, 1, "0123456789abcdef0123456789abcdef");
        r.assertion = &a;
        assert_eq!(
            verify(&r),
            Err(Reject::Unsupported),
            "{user:?} {method:?} {bh:?}"
        );
    }
}
