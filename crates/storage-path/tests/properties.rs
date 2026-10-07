//! Property tests and caps for storage-path (noevia#978).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use proptest::prelude::*;
use storage_path::{
    clean_root, is_plain_filename, join_root, safe_relative_path, Error, MAX_INPUT_BYTES,
};

fn path_piece() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(".".to_owned()),
        Just("..".to_owned()),
        Just("/".to_owned()),
        Just("\\".to_owned()),
        Just("\0".to_owned()),
        Just(" ".to_owned()),
        Just("%2e".to_owned()),
        Just("\u{2024}".to_owned()),
        Just("C:".to_owned()),
        "[a-z]{1,3}",
        any::<char>().prop_map(|c| c.to_string()),
    ]
}

fn pathish() -> impl Strategy<Value = String> {
    proptest::collection::vec(path_piece(), 0..24).prop_map(|v| v.concat())
}

fn assert_safe(out: &str) -> Result<(), TestCaseError> {
    prop_assert!(!out.starts_with('/'));
    prop_assert!(!out.contains('\0'));
    prop_assert!(!out.contains('\\'));
    prop_assert!(out.split('/').all(|s| s != ".." && s != "."));
    if !out.is_empty() {
        prop_assert!(out.split('/').all(|s| !s.is_empty()));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    // noevia#978: the output never contains a `..` segment, never starts with `/`, never has NUL.
    #[test]
    fn safe_relative_path_output_is_safe(raw in pathish()) {
        assert_safe(&safe_relative_path(&raw).unwrap())?;
    }

    #[test]
    fn safe_relative_path_output_is_safe_on_any_string(raw in any::<String>()) {
        assert_safe(&safe_relative_path(&raw).unwrap())?;
    }

    // Ordinary names (no edge whitespace, no separators, not dot segments) pass through unchanged.
    #[test]
    fn plain_paths_round_trip(segs in proptest::collection::vec("[a-zA-Z0-9_.é😀-]{1,12}", 1..8)) {
        prop_assume!(segs.iter().all(|s| s != "." && s != ".."));
        let p = segs.join("/");
        prop_assume!(p.encode_utf16().count() <= 500);
        prop_assert_eq!(safe_relative_path(&p).unwrap(), p);
    }

    // A path with a dot-dot segment anywhere is always refused, whatever surrounds it.
    #[test]
    fn dot_dot_segments_are_refused(a in pathish(), b in pathish(), sep in prop_oneof![Just("/"), Just("\\")]) {
        prop_assert_eq!(safe_relative_path(&format!("{a}{sep}..{sep}{b}")).unwrap(), "");
        prop_assert_eq!(safe_relative_path(&format!("..{sep}{b}")).unwrap(), "");
    }

    #[test]
    fn clean_root_has_no_edge_slashes(root in pathish()) {
        let r = clean_root(&root).unwrap();
        prop_assert!(!r.starts_with('/') && !r.ends_with('/'));
    }

    // Joining a safe relative path under a clean root never escapes the root textually.
    #[test]
    fn join_root_keeps_the_root_prefix(root in "[a-z/]{0,12}", raw in pathish()) {
        let rel = safe_relative_path(&raw).unwrap();
        let joined = join_root(&root, &rel).unwrap();
        let clean = clean_root(&root).unwrap();
        let root_dir = format!("{clean}/");
        prop_assert!(clean.is_empty() || joined == clean || joined.starts_with(&root_dir));
        prop_assert!(!joined.starts_with('/'));
        prop_assert!(joined.split('/').all(|s| s != ".."));
    }

    #[test]
    fn plain_filenames_have_no_separators_or_controls(name in any::<String>()) {
        if is_plain_filename(&name).unwrap() {
            prop_assert!(!name.is_empty() && name != "." && name != "..");
            prop_assert!(!name.contains('/') && !name.contains('\\'));
            prop_assert!(!name.chars().any(|c| c.is_ascii_control() && c != '\x7f'));
            prop_assert!(name.encode_utf16().count() <= 200);
        }
    }
}

#[test]
fn caps_are_typed_errors() {
    let big = "a".repeat(MAX_INPUT_BYTES + 1);
    let err = Error::InputTooLarge {
        len: big.len(),
        max: MAX_INPUT_BYTES,
    };
    assert_eq!(safe_relative_path(&big), Err(err.clone()));
    assert_eq!(clean_root(&big), Err(err.clone()));
    assert_eq!(join_root("", &big), Err(err.clone()));
    assert_eq!(join_root(&big, ""), Err(err.clone()));
    assert_eq!(is_plain_filename(&big), Err(err));
    assert!(safe_relative_path(&"a".repeat(MAX_INPUT_BYTES)).is_ok());
}
