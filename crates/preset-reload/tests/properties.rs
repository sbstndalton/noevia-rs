//! Properties (noevia#1012): no input panics, a text compared with itself is never a change,
//! adding a new section never changes a loaded model, and editing a loaded model's own section
//! always does.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use preset_reload::{check, check_json, Verdict};
use proptest::prelude::*;

fn name() -> impl Strategy<Value = String> {
    "[A-Za-z][A-Za-z0-9_.-]{0,12}"
}

fn section() -> impl Strategy<Value = (String, Vec<String>)> {
    (
        name(),
        prop::collection::vec("[a-z][a-z-]{0,9} = [A-Za-z0-9/._-]{0,12}", 0..5),
    )
}

fn render(global: &[String], sections: &[(String, Vec<String>)]) -> String {
    let mut out = String::from("version = 1\n[*]\n");
    for l in global {
        out.push_str(l);
        out.push('\n');
    }
    for (n, body) in sections {
        out.push_str(&format!("[{n}]\n"));
        for l in body {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

fn unique(sections: Vec<(String, Vec<String>)>) -> Vec<(String, Vec<String>)> {
    let mut seen = std::collections::HashSet::new();
    sections
        .into_iter()
        .filter(|(n, _)| n != "default" && seen.insert(n.clone()))
        .collect()
}

proptest! {
    #[test]
    fn never_panics(a in "\\PC{0,400}", b in "\\PC{0,400}", ids in prop::collection::vec("\\PC{1,8}", 0..4)) {
        let _ = check(&a, &b, &ids);
        let _ = check_json(&a);
    }

    #[test]
    fn line_ending_style_never_changes_the_verdict(secs in prop::collection::vec(section(), 1..6), pick in 0usize..6) {
        // llama.cpp preset.cpp:186: newline is "\r\n" / "\n" / "\r", so all three spell the same file (noevia#1043).
        let secs = unique(secs);
        let i = pick % secs.len();
        let lf = render(&[], &secs);
        let mut edited = secs.clone();
        edited[i].1.push("ctx-size = 123457".to_owned());
        let lf_after = render(&[], &edited);
        let loaded: Vec<String> = secs.iter().map(|(n, _)| n.clone()).collect();
        let want = check(&lf, &lf_after, &loaded);
        for eol in ["\r", "\r\n"] {
            prop_assert_eq!(check(&lf.replace('\n', eol), &lf_after.replace('\n', eol), &loaded), want.clone());
            prop_assert_eq!(check(&lf, &lf_after.replace('\n', eol), &loaded), want.clone());
        }
        prop_assert_eq!(check(&lf.replace('\n', "\r"), &lf, &loaded), Verdict::Unchanged);
    }

    #[test]
    fn identical_text_is_unchanged(global in prop::collection::vec("[a-z]{1,6} = [0-9]{1,4}", 0..3), secs in prop::collection::vec(section(), 1..6)) {
        let secs = unique(secs);
        let text = render(&global, &secs);
        let loaded: Vec<String> = secs.iter().map(|(n, _)| n.clone()).collect();
        prop_assert_eq!(check(&text, &text, &loaded), Verdict::Unchanged);
    }

    #[test]
    fn adding_a_section_keeps_loaded_models(secs in prop::collection::vec(section(), 1..6), extra in section()) {
        let secs = unique(secs);
        prop_assume!(extra.0 != "default" && !secs.iter().any(|(n, _)| *n == extra.0));
        let before = render(&[], &secs);
        let mut grown = secs.clone();
        grown.push(extra);
        let after = render(&[], &grown);
        let loaded: Vec<String> = secs.iter().map(|(n, _)| n.clone()).collect();
        prop_assert_eq!(check(&before, &after, &loaded), Verdict::Unchanged);
    }

    #[test]
    fn editing_a_loaded_section_is_reported(secs in prop::collection::vec(section(), 1..6), pick in 0usize..6) {
        let secs = unique(secs);
        let i = pick % secs.len();
        let before = render(&[], &secs);
        let mut edited = secs.clone();
        edited[i].1.push("ctx-size = 123457".to_owned());
        let after = render(&[], &edited);
        let id = secs[i].0.clone();
        prop_assert_eq!(check(&before, &after, std::slice::from_ref(&id)), Verdict::Changed(vec![id]));
    }
}
