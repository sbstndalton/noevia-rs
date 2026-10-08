//! Properties of the long section (noevia#1079): whatever the base section holds, the reply keeps
//! the input text as its prefix, every existing section reloads unchanged, the new section is the
//! last one and holds none of the dropped keys.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use long_profile::{pairs, section, Row};
use proptest::prelude::*;

fn line() -> impl Strategy<Value = String> {
    prop_oneof![
        "(ctx-size|cache-type-k|ubatch-size|jinja|model|mmproj|alias|load-on-startup|hf-repo) = [a-z0-9/._]{0,12}",
        "[;#][ -~]{0,20}",
        Just(String::new()),
    ]
}

proptest! {
    #[test]
    fn the_long_section_only_adds(
        before in proptest::collection::vec(line(), 0..6),
        body in proptest::collection::vec(line(), 0..10),
        after in proptest::collection::vec(line(), 0..6),
        model in proptest::option::of("/models/[a-z]{1,8}\\.gguf"),
    ) {
        let text = format!(
            "[*]\ncache-ram = 1024\n[first]\n{}\n[base]\n{}\n[last]\n{}\n",
            before.join("\n"),
            body.join("\n"),
            after.join("\n")
        );
        match section(&text, "base", model.as_deref(), None) {
            Ok((id, out)) => {
                prop_assert_eq!(id.as_str(), "base-long");
                prop_assert!(out.starts_with(&text));
                let names: Vec<String> = ["*", "first", "base", "last"].iter().map(|s| (*s).to_owned()).collect();
                prop_assert!(preset_reload::check(&text, &out, &names).safe());
                let tail = out.get(text.len()..).unwrap_or_default();
                prop_assert!(tail.starts_with("\n[base-long]\n"));
                for l in tail.lines().skip(2) {
                    prop_assert!(!l.starts_with("alias") && !l.starts_with("load-on-startup"));
                }
            }
            Err(r) => prop_assert_eq!(r.code(), "no_model"),
        }
    }

    #[test]
    fn a_pair_always_shares_its_file(
        rows in proptest::collection::vec(("(a|b|a-long|b-long|a-long-long)", proptest::option::of("/(f|g)")), 0..6),
    ) {
        let mut unique: Vec<Row> = Vec::new();
        for (id, model) in rows {
            if !unique.iter().any(|r| r.id == id) {
                unique.push(Row { id, model });
            }
        }
        for p in pairs(&unique) {
            let file = |id: &str| unique.iter().find(|r| r.id == id).and_then(|r| r.model.clone());
            prop_assert_eq!(format!("{}-long", p.base), p.long.clone());
            prop_assert!(file(&p.base).is_some());
            prop_assert_eq!(file(&p.base), file(&p.long));
        }
    }
}
