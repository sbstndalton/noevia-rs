//! Property tests and caps for secret-envelope (noevia#979). Synthetic keys only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use proptest::prelude::*;
use secret_envelope::{
    decode_base64_node, encode_base64url, open, seal, version_of, Error, KeyUsed,
    MAX_ENVELOPE_UNITS, MAX_PLAIN_BYTES, MAX_USER_BYTES,
};

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// What seal writes, open reads back with the same key and user, as v1 or v2.
    #[test]
    fn round_trip(key in any::<[u8; 32]>(), nonce in any::<[u8; 12]>(), plain in proptest::collection::vec(any::<u8>(), 0..300), user in proptest::option::of(proptest::collection::vec(any::<u8>(), 0..40))) {
        let env = seal(&key, &nonce, &plain, user.as_deref()).unwrap();
        prop_assert_eq!(version_of(&units(&env)), if user.is_some() { 2 } else { 1 });
        let (used, got) = open(&key, None, &units(&env), user.as_deref()).unwrap();
        prop_assert_eq!(used, KeyUsed::Current);
        prop_assert_eq!(got.as_slice(), plain.as_slice());
    }

    /// Any single-bit change to iv, tag or body is refused.
    #[test]
    fn tamper_is_refused(key in any::<[u8; 32]>(), plain in proptest::collection::vec(any::<u8>(), 0..64), pos in any::<usize>(), bit in 0u8..8) {
        let env = seal(&key, &[7; 12], &plain, Some(b"u")).unwrap();
        let mut raw = decode_base64_node(&units(&env[7..]));
        let i = pos % raw.len();
        raw[i] ^= 1 << bit;
        let bad = format!("enc:v2:{}", encode_base64url(&raw));
        prop_assert_eq!(open(&key, None, &units(&bad), Some(b"u")).unwrap_err(), Error::Unopenable);
    }

    /// A different user, a different key, or a v2 without a user never opens.
    #[test]
    fn wrong_user_or_key(key in any::<[u8; 32]>(), other in any::<[u8; 32]>(), a in "[a-z0-9-]{1,36}", b in "[a-z0-9-]{1,36}") {
        let env = units(&seal(&key, &[1; 12], b"synthetic", Some(a.as_bytes())).unwrap());
        if a != b {
            prop_assert_eq!(open(&key, None, &env, Some(b.as_bytes())).unwrap_err(), Error::Unopenable);
        }
        if key != other {
            prop_assert_eq!(open(&other, None, &env, Some(a.as_bytes())).unwrap_err(), Error::Unopenable);
            prop_assert_eq!(open(&other, Some(&key), &env, Some(a.as_bytes())).unwrap().0, KeyUsed::Previous);
        }
        prop_assert_eq!(open(&key, None, &env, None).unwrap_err(), Error::Bound);
    }

    /// open is total on arbitrary UTF-16 input: never panics, and anything that is not an
    /// envelope comes back as None with no plaintext.
    #[test]
    fn open_is_total(value in proptest::collection::vec(any::<u16>(), 0..200), prefix in prop_oneof![Just(""), Just("enc:v1:"), Just("enc:v2:")]) {
        let mut v = units(prefix);
        v.extend_from_slice(&value);
        match open(&[9; 32], Some(&[8; 32]), &v, Some(b"u")) {
            Ok((KeyUsed::None, p)) => prop_assert!(p.is_empty() && version_of(&v) == 0),
            Ok(_) => prop_assert!(false, "random input opened"),
            Err(e) => prop_assert_eq!(e, Error::Unopenable),
        }
    }

    /// The lenient decoder reads canonical base64url exactly and ignores junk between groups.
    #[test]
    fn base64_canonical_and_junk(bytes in proptest::collection::vec(any::<u8>(), 0..100), junk in prop_oneof![Just('!'), Just(' '), Just('\n'), Just('\u{100}'), Just('.')], at in any::<usize>()) {
        let text = encode_base64url(&bytes);
        prop_assert_eq!(decode_base64_node(&units(&text)), bytes.clone());
        let at = at % (text.len() + 1);
        let mut with = text[..at].to_owned();
        with.push(junk);
        with.push_str(&text[at..]);
        prop_assert_eq!(decode_base64_node(&units(&with)), bytes);
    }
}

#[test]
fn caps() {
    let k = [1u8; 32];
    let max = vec![b'a'; MAX_PLAIN_BYTES];
    let env = seal(&k, &[0; 12], &max, Some(b"u")).unwrap();
    assert!(env.encode_utf16().count() <= MAX_ENVELOPE_UNITS);
    assert_eq!(
        open(&k, None, &units(&env), Some(b"u")).unwrap().1.len(),
        MAX_PLAIN_BYTES
    );
    let over = vec![b'a'; MAX_PLAIN_BYTES + 1];
    assert_eq!(
        seal(&k, &[0; 12], &over, None).unwrap_err(),
        Error::TooLarge
    );
    let long_user = vec![b'u'; MAX_USER_BYTES + 1];
    assert_eq!(
        seal(&k, &[0; 12], b"a", Some(&long_user)).unwrap_err(),
        Error::TooLarge
    );
    assert_eq!(
        open(&k, None, &vec![b'A' as u16; MAX_ENVELOPE_UNITS + 1], None).unwrap_err(),
        Error::TooLarge
    );
    assert_eq!(
        open(&k[..16], None, &units("enc:v1:AAAA"), None).unwrap_err(),
        Error::Input
    );
}

#[test]
fn short_tags_are_refused() {
    // 12 + 15 bytes: Node would check a 15-byte tag; the port requires 16.
    let k = [2u8; 32];
    for n in 0..28 {
        let raw = vec![0u8; n];
        let v = format!("enc:v1:{}", encode_base64url(&raw));
        assert_eq!(
            open(&k, None, &units(&v), None).unwrap_err(),
            Error::Unopenable
        );
    }
}
