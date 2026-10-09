//! Properties of the project-file-names port: no input panics, traversal-shaped names never
//! resolve, a resolved file is the one exact or unique suffix match, and large projects stay linear.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use project_file_names::{call, invalid_reason, resolve, Resolved};
use prompt_framing::js::{trim, units};
use proptest::prelude::*;
use std::time::{Duration, Instant};

const SEG: &[&str] = &[
    "a", "b", "notes.md", "..", ".", "", "Text", "%2e", "%2F", "C:", "\\", "é", "日本", "e\u{301}",
    " ", "x\u{0}", "%00",
];

fn path() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(SEG), 1..5).prop_map(|s| s.join("/"))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    #[test]
    fn random_bytes_never_panic(body in prop::collection::vec(any::<u8>(), 0..256)) {
        let mut input = vec![1u8];
        input.extend(body);
        let (status, reply) = call(&input);
        prop_assert!(status <= 1 && !reply.is_empty());
    }

    #[test]
    fn traversal_never_resolves(names in prop::collection::vec(path(), 0..6), raw in path()) {
        let n: Vec<Vec<u16>> = names.iter().map(|s| units(s)).collect();
        let r = resolve(&n, &units(&raw));
        let t = String::from_utf16_lossy(trim(&units(&raw)));
        let shaped = t.split('/').any(|s| s.is_empty() || s == "." || s == "..")
            || t.starts_with('/') || t.contains('\\') || t.to_ascii_lowercase().contains("%2e")
            || t.to_ascii_lowercase().contains("%2f") || t.contains("%00") || t.contains('\u{0}');
        if shaped {
            prop_assert!(matches!(r, Ok(Resolved::Invalid(_))), "{:?} -> {:?}", raw, r);
        }
        match r {
            Ok(Resolved::File(i)) => {
                let w = trim(&units(&raw)).to_vec();
                let name = &n[i];
                let mut suffix = vec![u16::from(b'/')];
                suffix.extend(&w);
                prop_assert!(name == &w || name.ends_with(&suffix));
                prop_assert!(invalid_reason(&w).is_none());
            }
            Ok(Resolved::Ambiguous(c)) => {
                prop_assert!(c.len() > 1 && c.windows(2).all(|p| p[0] < p[1]));
            }
            _ => {}
        }
    }
}

#[test]
fn large_projects_stay_linear() {
    let names: Vec<Vec<u16>> = (0..200_000)
        .map(|i| units(&format!("f/{i}/notes.md")))
        .collect();
    let t = Instant::now();
    let r = resolve(&names, &units("notes.md")).unwrap();
    assert!(matches!(r, Resolved::Ambiguous(ref c) if c.len() == 200_000));
    let r = resolve(&names, &units("199999/notes.md")).unwrap();
    assert_eq!(r, Resolved::File(199_999));
    assert!(t.elapsed() < Duration::from_secs(5));
}
