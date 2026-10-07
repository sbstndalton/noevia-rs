//! Property tests and caps for upload-sniff (noevia#977).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use proptest::prelude::*;
use upload_sniff::{
    classify, cp1252, decode_reply, decode_text, validate, Encoding, Error, Refusal, CAP,
    MAX_DECODE_BYTES, SNIFF_BYTES,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Only the first SNIFF_BYTES bytes matter to validate.
    #[test]
    fn validate_reads_only_the_head(name in "[a-z.]{1,12}", head in proptest::collection::vec(any::<u8>(), 0..400), tail in proptest::collection::vec(any::<u8>(), 0..64)) {
        let len = (head.len() + tail.len()) as u64;
        let mut full = head.clone();
        full.extend_from_slice(&tail);
        let cut = &full[..full.len().min(SNIFF_BYTES)];
        prop_assert_eq!(validate(&name, len, &full), validate(&name, len, cut));
    }

    /// Size refusals depend on the length alone, after the filename rule.
    #[test]
    fn size_refusals(len in prop_oneof![Just(0u64), Just(CAP), Just(CAP + 1), any::<u64>()]) {
        let got = validate("upload.txt", len, b"text");
        let want = if len == 0 { Some(Refusal::Empty) } else if len > CAP { Some(Refusal::TooBig) } else { None };
        prop_assert_eq!(got, want);
    }

    /// decode never panics; text from it is valid UTF-8 by construction, NUL-free unless a BOM
    /// decoder accepted it, and the wire form is the tag then the same text.
    #[test]
    fn decode_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let r = decode_text(&bytes).unwrap();
        let wire = decode_reply(&bytes).unwrap();
        match r {
            None => {
                prop_assert!(bytes.contains(&0));
                prop_assert_eq!(wire, vec![0]);
            }
            Some((text, enc)) => {
                if matches!(enc, Encoding::Utf8 | Encoding::Windows1252) && !bytes.starts_with(&[0xff, 0xfe]) && !bytes.starts_with(&[0xfe, 0xff]) {
                    prop_assert!(!text.contains('\0'));
                }
                if enc == Encoding::Windows1252 {
                    prop_assert!(std::str::from_utf8(&bytes).is_err());
                    prop_assert_eq!(text.chars().count(), bytes.len());
                }
                prop_assert_eq!(wire[0], enc.tag());
                prop_assert_eq!(&wire[1..], text.as_bytes());
            }
        }
    }

    /// Valid UTF-8 without NUL decodes to itself.
    #[test]
    fn utf8_round_trips(s in "[^\u{0}\u{FEFF}]{0,64}") {
        prop_assert_eq!(decode_text(s.as_bytes()).unwrap(), Some((s.clone(), Encoding::Utf8)));
    }

    /// Well-formed UTF-16 behind its BOM decodes to itself (no leading U+FEFF to strip).
    #[test]
    fn utf16_round_trips(s in "[^\u{FEFF}]{0,48}", le in any::<bool>()) {
        let mut b = if le { vec![0xff, 0xfe] } else { vec![0xfe, 0xff] };
        for u in s.encode_utf16() {
            b.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() });
        }
        let enc = if le { Encoding::Utf16Le } else { Encoding::Utf16Be };
        prop_assert_eq!(decode_text(&b).unwrap(), Some((s.clone(), enc)));
    }

    /// classify is one of the four groups and ignores the directory part.
    #[test]
    fn classify_groups(dir in "[a-z.]{0,8}", name in "[a-zA-Z.]{0,12}") {
        let g = classify(&name);
        prop_assert!(["Documents", "Images", "Text", "Other"].contains(&g));
        if !name.is_empty() {
            prop_assert_eq!(classify(&format!("{dir}/{name}")), g);
        }
    }
}

#[test]
fn cp1252_is_a_bijection_onto_its_range() {
    let mut seen = std::collections::HashSet::new();
    for b in 0..=255u8 {
        assert!(seen.insert(cp1252(b)), "duplicate for {b:#x}");
    }
}

#[test]
fn decode_cap() {
    let ok = vec![b'a'; MAX_DECODE_BYTES];
    assert!(decode_text(&ok).unwrap().is_some());
    let big = vec![b'a'; MAX_DECODE_BYTES + 1];
    assert_eq!(
        decode_text(&big),
        Err(Error::InputTooLarge {
            len: MAX_DECODE_BYTES + 1,
            max: MAX_DECODE_BYTES
        })
    );
    assert_eq!(decode_reply(&big).unwrap_err().code(), "too_large");
}

#[test]
fn huge_name_is_not_plain() {
    let name = "a".repeat(storage_path::MAX_INPUT_BYTES + 1);
    assert_eq!(validate(&name, 1, b"x"), Some(Refusal::Filename));
}
