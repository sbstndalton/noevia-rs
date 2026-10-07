//! Property tests: hostile input never panics, the caps hold, and the parsers keep the
//! invariants the Python reference has. Agreement with Python itself is differential.rs (and
//! noevia-services' seeded fuzz against the live Python function).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use model_files::{files_from_tree_json, infer_quant, json, py_int_str, shard_key, BigInt, Error};
use proptest::prelude::*;

/// Characters the rules treat specially, plus arbitrary ones.
fn name_char() -> impl Strategy<Value = char> {
    prop_oneof![
        3 => prop::sample::select(vec![
            '-', '.', '_', '/', 'Q', 'q', 'I', 'i', 'F', 'B', 'P', 'K', 'M', 'S', 'o', 'f', '0',
            '1', '2', '4', '6', '8', '\u{130}', '\u{131}', '\u{17f}', '\u{212a}', '\u{664}',
            '\u{ff14}', '\u{b2}', '\u{301}', '\n', '\u{85}',
        ]),
        1 => any::<char>(),
    ]
}

fn name() -> impl Strategy<Value = String> {
    prop::collection::vec(name_char(), 0..40).prop_map(|v| v.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = files_from_tree_json(&bytes);
    }

    #[test]
    fn arbitrary_json_like_text_never_panics(s in "[\\[\\]{}\",:0-9eE.+\\-a-zNIfy\\\\u ]{0,200}") {
        let _ = files_from_tree_json(s.as_bytes());
    }

    #[test]
    fn rules_never_panic(n in name()) {
        let _ = shard_key(&n);
        let _ = infer_quant(&n);
        let _ = model_files::is_support_file(&n);
        let _ = py_int_str(&n);
    }

    #[test]
    fn shard_key_round_trips(stem in name(), ext in "[a-z]{1,6}", i in 0u32..100_000, t in 0u32..100_000) {
        prop_assume!(!ext.contains('.'));
        let path = format!("{stem}-{i:05}-of-{t:05}.{ext}");
        let (base, idx, tot) = shard_key(&path);
        prop_assert_eq!(base, format!("{stem}.{ext}"));
        prop_assert_eq!(idx, Some(i));
        prop_assert_eq!(tot, Some(t));
    }

    #[test]
    fn shard_key_without_a_match_is_identity(n in name()) {
        let (base, idx, tot) = shard_key(&n);
        prop_assert_eq!(idx.is_some(), tot.is_some());
        if idx.is_none() {
            prop_assert_eq!(base, n);
        } else {
            // Exactly the 15-character suffix group was removed.
            prop_assert_eq!(base.chars().count() + 15, n.chars().count());
        }
    }

    #[test]
    fn quant_is_upper_and_from_the_name(n in name()) {
        if let Some(q) = infer_quant(&n) {
            prop_assert!(!q.is_empty());
            prop_assert!(!q.chars().any(|c| c.is_ascii_lowercase()));
            let first = q.chars().next();
            prop_assert!(matches!(first, Some('Q' | 'I' | 'F' | 'B' | '\u{130}')), "{}", q);
        }
    }

    #[test]
    fn known_quants_are_found(prefix in "[a-z]{0,5}[-. ]", q in prop::sample::select(vec![
        "Q4_K_M", "Q8_0", "IQ2_XXS", "Q6_K", "BF16", "F16", "F32", "FP8", "Q5_K_S", "IQ4_NL",
    ])) {
        let lower = format!("{prefix}{}.gguf", q.to_lowercase());
        let got = infer_quant(&lower);
        prop_assert_eq!(got.as_deref(), Some(q));
    }

    #[test]
    fn int_of_ascii_decimal_matches_u128(n in any::<u64>(), neg in any::<bool>(), pad in " {0,3}") {
        let text = format!("{pad}{}{n}{pad}", if neg { "-" } else { "" });
        let want = if neg && n != 0 { format!("-{n}") } else { n.to_string() };
        prop_assert_eq!(py_int_str(&text).map(|b| b.to_string()), Some(want));
    }

    #[test]
    fn int_of_float_truncates(x in -1e18f64..1e18f64) {
        let got = BigInt::from_f64_trunc(x).unwrap().to_string();
        prop_assert_eq!(got, format!("{}", x.trunc() as i64));
    }

    #[test]
    fn big_floats_are_exact(m in 1u64..(1 << 53), e in 0i32..900) {
        let x = (m as f64) * 2f64.powi(e);
        prop_assume!(x.is_finite());
        let digits = BigInt::from_f64_trunc(x).unwrap().to_string();
        // Parsing the exact digits back gives the same double.
        prop_assert_eq!(digits.parse::<f64>().unwrap(), x);
    }
}

#[test]
fn caps_are_enforced() {
    let too_big = vec![b' '; model_files::MAX_INPUT_BYTES + 1];
    assert_eq!(files_from_tree_json(&too_big), Err(Error::InputTooLarge));

    let deep = format!(
        "[{}{}]",
        "[".repeat(json::MAX_DEPTH),
        "]".repeat(json::MAX_DEPTH)
    );
    assert_eq!(files_from_tree_json(deep.as_bytes()), Err(Error::TooDeep));

    let long = format!(
        r#"[{{"type":"file","path":"{}.gguf"}}]"#,
        "a".repeat(json::MAX_STRING_CHARS)
    );
    assert_eq!(files_from_tree_json(long.as_bytes()), Err(Error::TooLong));

    let long_number = format!(
        r#"[{{"type":"file","path":"a.gguf","size":{}}}]"#,
        "9".repeat(json::MAX_NUMBER_LEN + 1)
    );
    assert_eq!(
        files_from_tree_json(long_number.as_bytes()),
        Err(Error::TooLong)
    );

    let many = format!("[{}{{}}]", "{},".repeat(model_files::MAX_ENTRIES));
    assert_eq!(
        files_from_tree_json(many.as_bytes()),
        Err(Error::TooManyEntries)
    );
}

#[test]
fn input_contract_errors_are_typed() {
    assert_eq!(files_from_tree_json(b"\xff"), Err(Error::NotUtf8));
    assert_eq!(files_from_tree_json(b"{}"), Err(Error::NotAList));
    assert_eq!(
        files_from_tree_json(br#"["\ud800"]"#),
        Err(Error::LoneSurrogate)
    );
    assert_eq!(files_from_tree_json(b"[1]"), Err(Error::EntryNotObject(0)));
    assert_eq!(
        files_from_tree_json(br#"[{"type":"file","path":1}]"#),
        Err(Error::PathNotString(0))
    );
    assert_eq!(
        files_from_tree_json(br#"[{"type":"file","path":"a.gguf","lfs":"x"}]"#),
        Err(Error::LfsNotObject(0))
    );
    assert_eq!(
        files_from_tree_json(br#"[{"type":"file","path":"a.gguf","size":"x"}]"#),
        Err(Error::BadSize(0))
    );
    assert!(matches!(files_from_tree_json(b"[1,]"), Err(Error::Json(_))));
}
