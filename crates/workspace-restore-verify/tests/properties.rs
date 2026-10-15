//! Properties: no input panics, and a single flipped bit of a valid export is either refused or
//! verifies to exactly the same listing (a CRC- and checksum-protected archive cannot be bent
//! into a different accepted one by one bit).

mod common;

use proptest::prelude::*;
use workspace_restore_verify::verify;

fn small() -> Vec<u8> {
    let mut entries = common::export_entries(
        &[("a.md", b"alpha alpha alpha"), ("b/c.md", b"gamma")],
        &["b", "e"],
    );
    for e in &mut entries {
        e.deflate = !e.name.ends_with(b"/");
    }
    common::zip(&entries)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn arbitrary_bytes_never_panic(body in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let _ = verify(&body);
    }

    #[test]
    fn one_flipped_bit_never_changes_an_accepted_listing(at in 0usize..4096, bit in 0u8..8) {
        let good = small();
        let want = verify(&good).map(|v| v.listing_sha256());
        prop_assert!(want.is_ok());
        let mut bad = good.clone();
        let at = at % bad.len();
        if let Some(b) = bad.get_mut(at) {
            *b ^= 1 << bit;
        }
        if let Ok(v) = verify(&bad) {
            prop_assert_eq!(Ok(v.listing_sha256()), want);
        }
    }

    #[test]
    fn appended_or_prepended_bytes_are_refused(extra in proptest::collection::vec(any::<u8>(), 1..64), front in any::<bool>()) {
        let good = small();
        let body = if front { [extra.as_slice(), good.as_slice()].concat() } else { [good.as_slice(), extra.as_slice()].concat() };
        prop_assert!(verify(&body).is_err());
    }
}
