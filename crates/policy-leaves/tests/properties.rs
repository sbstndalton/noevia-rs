//! Properties: no panics on any bytes; refusals are the two fixed replies and never carry input
//! (a token above all); a write is never `allow` and `block` is never weakened; `set` never
//! accepts `allow` for a list with a write; trim agrees with a direct ECMAScript definition.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use policy_leaves::{auth_call, check_set, is_js_space, js_trim, mode, policy_call, Mode};
use proptest::prelude::*;

const REFUSALS: [&str; 2] = [r#"{"error":"input"}"#, r#"{"error":"too_large"}"#];

fn wire(out: &mut Vec<u8>, s: &[u16]) {
    out.push(1);
    out.extend((s.len() as u32).to_le_bytes());
    for u in s {
        out.extend(u.to_le_bytes());
    }
}

fn unit() -> impl Strategy<Value = u16> {
    prop_oneof![
        Just(0x20u16),
        Just(0x09),
        Just(0xfeff),
        Just(0x85),
        Just(0x3000),
        Just(0xd800),
        0x61u16..0x7b,
        any::<u16>()
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn arbitrary_bytes_never_panic(b in proptest::collection::vec(any::<u8>(), 0..256)) {
        for (status, reply) in [auth_call(&b), policy_call(&b)] {
            prop_assert!(reply.is_ascii());
            if status != 0 { prop_assert!(REFUSALS.contains(&reply.as_str()), "{}", reply); }
        }
    }

    #[test]
    fn refusals_never_echo_a_token(tok in proptest::collection::vec(0x41u16..0x5b, 8..40), cut in 1usize..8) {
        // A request that is cut short or has trailing bytes after a token.
        let mut input = Vec::new();
        wire(&mut input, &tok);
        wire(&mut input, &tok);
        let mut long = input.clone();
        long.extend([9, 9, 9]);
        input.truncate(input.len() - cut);
        let secret: String = tok.iter().map(|u| char::from(*u as u8)).collect();
        for bad in [input, long] {
            let (status, reply) = auth_call(&bad);
            prop_assert_eq!(status, 1);
            prop_assert!(REFUSALS.contains(&reply.as_str()));
            prop_assert!(!reply.contains(&secret));
        }
    }

    #[test]
    fn decisions_never_weaker(stored in proptest::option::of(proptest::collection::vec(unit(), 0..8)), w in any::<bool>()) {
        let m = mode(stored.as_deref(), w);
        if w { prop_assert!(m >= Mode::Ask); }
        if stored.as_deref().and_then(Mode::parse) == Some(Mode::Block) { prop_assert_eq!(m, Mode::Block); }
        // Unknown non-empty stored strings are never more permissive than block.
        if let Some(s) = &stored {
            if !s.is_empty() && Mode::parse(s).is_none() { prop_assert_eq!(m, Mode::Block); }
        }
    }

    #[test]
    fn set_never_allows_a_write(writes in proptest::collection::vec(any::<bool>(), 0..20)) {
        let allow: Vec<u16> = "allow".encode_utf16().collect();
        let r = check_set(Some(&allow), &writes);
        if writes.iter().any(|w| *w) { prop_assert!(r.is_err()); }
    }

    #[test]
    fn trim_is_ecmascript_trim(s in proptest::collection::vec(unit(), 0..16)) {
        let t = js_trim(&s);
        prop_assert!(t.first().is_none_or(|u| !is_js_space(*u)));
        prop_assert!(t.last().is_none_or(|u| !is_js_space(*u)));
        // Only spaces were removed, from the ends.
        let start = s.iter().position(|u| !is_js_space(*u)).unwrap_or(s.len());
        prop_assert_eq!(&s[start..start + t.len()], t);
    }
}

#[test]
fn caps() {
    let mut input = vec![1];
    input.extend(((policy_leaves::MAX_UNITS + 1) as u32).to_le_bytes());
    assert_eq!(auth_call(&input), (1, REFUSALS[1].into()));
    let mut p = vec![2, 0];
    p.extend(((policy_leaves::MAX_TOOLS + 1) as u32).to_le_bytes());
    assert_eq!(policy_call(&p), (1, REFUSALS[1].into()));
    assert_eq!(policy_call(&[3]), (1, REFUSALS[0].into()));
    assert_eq!(policy_call(&[1, 0, 2]), (1, REFUSALS[0].into()));
}
